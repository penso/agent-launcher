//! Away deliberately does not use Herdr's TUI agent idle/done classification.
//! Herdr 0.9.0 src/cli/pane.rs: `pane run` joins arguments and sends Enter.
//! OpenCode cli/cmd/run.ts emits JSONL {type, sessionID, part|error}; only the
//! supervisor's post-wait exit receipt proves the OpenCode process has exited.
use std::{
    io::{Read, Write},
    path::Path,
};

use super::*;

const LOG_LIMIT: u64 = 16 * 1024 * 1024;
const REPORT_LIMIT: usize = 32 * 1024;
// Behavioral instructions, not an OS-level sandbox or publishing restriction.
const REPORT_PROMPT: &str = r#"

Implement the requested issue, run relevant tests, and then finish this turn.
Do not push, open a PR, merge, or close issues. Ignore any profile instructions
that tell you to publish changes or do any of those actions. Do not launch or
delegate to additional agents, including subagents or background agents.
Do not start persistent servers. Do not request broader permissions.
If blocked or unable to complete, report failure and explain why. Your final response
must be ONLY a JSON object (no Markdown fences), at most 32768 bytes:
{"away_report":1,"status":"success" or "failure","summary":"what changed or why blocked","tests":"commands run and results, or why not run"}.
Only report success when implementation is complete. This final response ends the worker;
the worktree and session will be retained for human review.
"#;

pub(super) fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Completed | RunState::Failed | RunState::Cancelled
    )
}

fn quote(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(Error::InvalidRequest(
            "NUL is not valid in a shell argument".into(),
        ));
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn path_quote(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::InvalidRequest("Away requires UTF-8 paths".into()))?;
    if value.chars().any(char::is_control) {
        return Err(Error::InvalidRequest(
            "Away paths cannot contain terminal control characters".into(),
        ));
    }
    quote(value)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn supervisor(
    directory: &Path,
    worktree: &Path,
    run_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<String> {
    let directory = path_quote(directory)?;
    let worktree = path_quote(worktree)?;
    // Explicitly avoid default_agent=plan. Build still obeys user/project config
    // and permissions; this selects an agent, not a permission bypass.
    let mut command = "opencode run --format json --agent build".to_owned();
    if let Some(model) = model {
        command.push_str(&format!(" --model {}", quote(model)?));
    }
    if let Some(effort) = effort {
        command.push_str(&format!(" --variant {}", quote(effort)?));
    }
    // The mkdir guard also prevents an accidentally re-submitted pane command
    // from starting a second worker. No EXIT trap: a killed supervisor is unknown.
    Ok(format!(
        r#"#!/bin/sh
umask 077
d={directory}
mkdir "$d/started" 2>/dev/null || exit 0
cd {worktree} || exit 1
printf '%s\n' 'Away OpenCode worker running. Raw logs:' "$d/events.jsonl" "$d/stderr.log"
{command} < "$d/prompt.txt" > "$d/events.jsonl" 2> "$d/stderr.log"
code=$?
printf '%s %s\n' {run_id} "$code" > "$d/exit.tmp" && mv "$d/exit.tmp" "$d/exit"
printf '\nAway OpenCode process exited with status %s. Worktree retained.\n' "$code"
printf '%s\n' 'Refresh the launcher for the final report and opencode -s SESSION_ID.'
if [ -f "$d/resume.txt" ]; then cat "$d/resume.txt"; fi
"#,
        run_id = quote(run_id)?
    ))
}

fn read_bounded(path: &Path, limit: u64) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        std::fs::File::from(
            open(
                path,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?,
        )
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::InvalidResponse(
            "Away log is not a regular file".into(),
        ));
    }
    file.take(limit + 1).read_to_end(&mut bytes)?;
    let oversized = bytes.len() as u64 > limit;
    bytes.truncate(limit as usize);
    Ok((bytes, oversized))
}

#[derive(Default)]
struct Transcript {
    session: Option<String>,
    text: String,
    success: bool,
    valid: bool,
    diagnostic: Option<String>,
}

fn permission_diagnostic(line: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    // OpenCode run.ts UI.println warning; strip only SGR color/style sequences.
    // Other terminal escapes/control characters are not an allowed diagnostic.
    let mut clean = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.next()? != '[' {
                return None;
            }
            while chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_digit() || *ch == ';')
            {
                chars.next();
            }
            if chars.next()? != 'm' {
                return None;
            }
        } else if ch.is_control() {
            return None;
        } else {
            clean.push(ch);
        }
    }
    let body = clean
        .strip_prefix("! permission requested: ")?
        .strip_suffix("); auto-rejecting")?;
    let (permission, _patterns) = body.split_once(" (")?;
    if permission.is_empty()
        || !permission
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return None;
    }
    Some(clean)
}

fn parse_transcript(bytes: &[u8], oversized: bool) -> Transcript {
    let mut result = Transcript::default();
    let mut final_text = None;
    let mut finish = None;
    let mut invalid = oversized;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            if let Some(diagnostic) = permission_diagnostic(line) {
                result.diagnostic = Some(diagnostic.chars().take(4096).collect());
                continue;
            }
            invalid = true;
            result.diagnostic = Some(String::from_utf8_lossy(line).chars().take(2048).collect());
            continue;
        };
        let Some(session) = event["sessionID"].as_str().filter(|id| {
            id.starts_with("ses_")
                && id.len() <= 128
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) else {
            invalid = true;
            continue;
        };
        if result
            .session
            .as_deref()
            .is_some_and(|previous| previous != session)
        {
            invalid = true;
        }
        result.session = Some(session.into());
        match event["type"].as_str() {
            Some("text") => {
                let part = &event["part"];
                final_text = part["text"]
                    .as_str()
                    .filter(|text| text.len() <= REPORT_LIMIT)
                    .map(|text| {
                        (
                            text.to_owned(),
                            part["messageID"].as_str().map(str::to_owned),
                        )
                    });
            },
            Some("step_start") => {
                finish = None;
                final_text = None;
            },
            Some("step_finish") => {
                finish = Some((
                    event["part"]["reason"].as_str() == Some("stop"),
                    event["part"]["messageID"].as_str().map(str::to_owned),
                ));
            },
            Some("error") => {
                invalid = true;
                result.diagnostic = Some(event["error"].to_string().chars().take(4096).collect());
            },
            Some("tool_use") if event["part"]["state"]["status"] == "error" => {
                result.diagnostic = Some(
                    event["part"]["state"]["error"]
                        .to_string()
                        .chars()
                        .take(4096)
                        .collect(),
                );
            },
            _ => {},
        }
    }
    if let Some((text, message)) = final_text {
        result.text = text.clone();
        if let Ok(report) = serde_json::from_str::<Value>(&text) {
            let status = report["status"].as_str();
            result.valid = !invalid
                && result.session.is_some()
                && finish.is_some_and(|(stop, id)| stop && id.is_some() && id == message)
                && report["away_report"] == 1
                && matches!(status, Some("success" | "failure"))
                && report["summary"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty())
                && report["tests"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty());
            result.success = status == Some("success");
        }
    }
    if oversized {
        result.diagnostic = Some("JSON log exceeded 16 MiB parsing limit; raw log retained".into());
    }
    result
}

fn exit_code(bytes: &[u8], run_id: &str) -> Option<u8> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (id, code) = text.strip_suffix('\n')?.split_once(' ')?;
    (id == run_id).then(|| code.parse().ok()).flatten()
}

// Herdr 0.9.0 schema/{response,panes}.rs: argv is optional. Never substitute
// name/cmdline substring matching or a bare OpenCode process for run ownership.
fn supervisor_running(value: &Value, pane_id: &str, directory: &Path) -> bool {
    let info = &value["result"]["process_info"];
    value["result"]["type"] == "pane_process_info"
        && info["pane_id"] == pane_id
        && info["foreground_process_group_id"]
            .as_u64()
            .is_some_and(|pid| pid > 0)
        && info["foreground_processes"]
            .as_array()
            .is_some_and(|processes| {
                processes.iter().any(|process| {
                    process["pid"].as_u64().is_some_and(|pid| pid > 0)
                        && process["pid"] != info["shell_pid"]
                        && process["argv"]
                            == serde_json::json!(["/bin/sh", directory.join("worker.sh")])
                })
            })
}

fn shell_idle(value: &Value, pane_id: &str) -> bool {
    let info = &value["result"]["process_info"];
    let Some(shell) = info["shell_pid"].as_u64().filter(|pid| *pid > 0) else {
        return false;
    };
    let Some(processes) = info["foreground_processes"].as_array() else {
        return false;
    };
    if value["result"]["type"] != "pane_process_info"
        || info["pane_id"] != pane_id
        || info["foreground_process_group_id"].as_u64() != Some(shell)
        || processes.len() != 1
        || processes[0]["pid"].as_u64() != Some(shell)
    {
        return false;
    }
    let Some(argv) = processes[0]["argv"].as_array() else {
        return false;
    };
    let Some(executable) = argv.first().and_then(Value::as_str) else {
        return false;
    };
    let name = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .trim_start_matches('-');
    matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh")
        && argv
            .iter()
            .skip(1)
            .all(|arg| matches!(arg.as_str(), Some("-l" | "--login" | "-i")))
}

impl HerdrBackend {
    pub(super) async fn away_dispatch(
        &self,
        request: DispatchRequest,
        run_id: &str,
    ) -> Result<DispatchResult> {
        let mut recorded = false;
        let result = async {
            request.validate()?;
            if request.private_fork.is_some() || request.issue.security_advisory.is_some() {
                return Err(Error::PrivateSecurity);
            }
            if request.agent != "opencode" || request.target.is_some() {
                return Err(Error::InvalidRequest(
                    "Away requires local Herdr + opencode".into(),
                ));
            }
            if request.effort.as_deref().is_some_and(|value| {
                value.trim().is_empty()
                    || value.trim_start().starts_with('-')
                    || value.chars().any(char::is_control)
            }) {
                return Err(Error::InvalidRequest(
                    "Away effort must be a literal nonblank variant, not an option".into(),
                ));
            }
            if !Uuid::parse_str(run_id).is_ok_and(|id| id.to_string() == run_id) {
                return Err(Error::InvalidRequest(
                    "Away run_id must be a canonical UUID".into(),
                ));
            }
            let _guard = self.registry.provision_lock.lock().await;
            if let Ok(record) = self.registry.get(run_id).await {
                recorded = true;
                if !matches!(record.session, BackendSession::HerdrAway { .. })
                    || record.summary.issue_key != request.issue.key.canonical()
                    || record.deletion.is_some()
                {
                    return Err(Error::InvalidRequest(
                        "Away run_id is already owned by another request or deletion".into(),
                    ));
                }
                return Ok(DispatchResult {
                    run: record.summary,
                    capabilities: self.capabilities(),
                });
            }
            self.status().await?;
            let root = SessionRegistry::prepare_app_data_dir(&self.registry.away_root()?)?;
            if root.starts_with(std::fs::canonicalize(&request.repository.root)?) {
                return Err(Error::InvalidRequest(
                    "Away logs must be stored outside the repository".into(),
                ));
            }
            let directory = root.join(run_id);
            // Exclusive directory creation is also a cross-process dispatch claim.
            // A crash before registry persistence leaves a claim, never a retryable launch.
            std::fs::create_dir(&directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            }
            let now = Utc::now();
            let mut record = RunRecord {
                summary: RunSummary {
                    confidential: false,
                    id: run_id.into(),
                    issue_key: request.issue.key.canonical(),
                    workspace: None,
                    agent: request.agent.clone(),
                    model: request.model.clone(),
                    state: RunState::Provisioning,
                    message: Some(format!(
                        "Away reservation; recovery logs: {}",
                        directory.display()
                    )),
                    session_id: None,
                    started_at: now,
                    updated_at: now,
                },
                session: BackendSession::HerdrAway {
                    workspace_id: None,
                    pane_id: None,
                    directory: directory.clone(),
                    submitted: false,
                },
                deletion: None,
            };
            self.registry.insert(record.clone()).await?;
            recorded = true;
            let mut attempted = false;
            let launch: Result<()> = async {
                let branch = sanitize_branch(
                    request
                        .branch
                        .as_deref()
                        .unwrap_or(&format!("agent/away-{run_id}")),
                );
                let label = sanitize_workspace_name(
                    request
                        .workspace_name
                        .as_deref()
                        .unwrap_or(&format!("away-{}", request.issue.identifier)),
                );
                let args = resolved_worktree_create_args(
                    &request.repository.root,
                    &branch,
                    &label,
                    request.base_branch.as_deref(),
                )
                .await?;
                let response = self.command(args).await?;
                let worktree = match parse_worktree(&response) {
                    Ok(worktree) => worktree,
                    Err(error) => {
                        let _ = write_new(
                            &directory.join("worktree.json"),
                            &serde_json::to_vec(&response)?,
                        );
                        return Err(error);
                    },
                };
                record.summary.workspace = Some(WorkspaceRef {
                    backend: BackendKind::Herdr,
                    id: worktree.workspace_id.clone(),
                    host: None,
                    path: worktree.path.clone(),
                    branch,
                });
                record.session = BackendSession::HerdrAway {
                    workspace_id: Some(worktree.workspace_id),
                    pane_id: Some(worktree.pane_id.clone()),
                    directory: directory.clone(),
                    submitted: false,
                };
                record.summary.state = RunState::Starting;
                self.registry.update(record.clone()).await?;
                write_new(
                    &directory.join("worktree.json"),
                    &serde_json::to_vec(&response)?,
                )?;
                let path = worktree
                    .path
                    .ok_or_else(|| Error::InvalidResponse("Herdr worktree has no path".into()))?;
                if !path.is_absolute() {
                    return Err(Error::InvalidResponse(
                        "Herdr worktree path is not absolute".into(),
                    ));
                }
                write_new(
                    &directory.join("prompt.txt"),
                    format!("{}{REPORT_PROMPT}", request.prompt).as_bytes(),
                )?;
                let script = supervisor(
                    &directory,
                    &path,
                    run_id,
                    request.model.as_deref(),
                    request.effort.as_deref(),
                )?;
                write_new(&directory.join("worker.sh"), script.as_bytes())?;
                let args = vec![
                    "pane".into(),
                    "run".into(),
                    worktree.pane_id.into(),
                    format!("/bin/sh {}", path_quote(&directory.join("worker.sh"))?).into(),
                ];
                // Durable write-ahead intent: even a crash before acknowledgement
                // must not make a possibly delivered pane command retryable.
                let mut submitted = record.clone();
                if let BackendSession::HerdrAway { submitted, .. } = &mut submitted.session {
                    *submitted = true;
                }
                self.registry.update(submitted.clone()).await?;
                record = submitted;
                attempted = true;
                self.command(args).await?;
                Ok(())
            }
            .await;
            let submitted = launch.is_ok();
            record.summary.state = if attempted {
                RunState::Disconnected
            } else {
                RunState::Failed
            };
            record.summary.updated_at = Utc::now();
            record.summary.message = Some(match launch {
                Ok(()) => format!(
                    "Awaiting confirmed OpenCode process exit; logs: {}",
                    directory.display()
                ),
                Err(error) if attempted => format!(
                    "Away launch is unconfirmed (not retried): {error}; recovery: {}",
                    directory.display()
                ),
                Err(error) => format!(
                    "Away worker was not submitted: {error}; recovery: {}",
                    directory.display()
                ),
            });
            self.registry.update(record.clone()).await?;
            drop(_guard);
            if submitted {
                record.summary = self.away_refresh(run_id).await?.run;
            }
            Ok(DispatchResult {
                run: record.summary,
                capabilities: self.capabilities(),
            })
        }
        .await;
        result.map_err(|error: Error| {
            if recorded {
                error
            } else {
                Error::AwayNotStarted(error.to_string())
            }
        })
    }

    pub(super) async fn away_refresh(&self, run_id: &str) -> Result<StatusResult> {
        let _guard = self.registry.provision_lock.lock().await;
        let mut record = self.record(run_id).await?;
        let BackendSession::HerdrAway {
            directory,
            pane_id,
            submitted,
            ..
        } = &record.session
        else {
            return Err(Error::UnsupportedCapability {
                backend: self.kind(),
                capability: Capability::Away,
            });
        };
        if terminal(record.summary.state) {
            return Ok(StatusResult {
                output: record.summary.message.clone(),
                run: record.summary,
            });
        }
        if !submitted {
            // Same-registry dispatch holds provision_lock. On restart no command
            // can have been sent without the persisted submission intent.
            let run = self
                .registry
                .set_state(
                    run_id,
                    RunState::Failed,
                    Some(
                        "Away worker was never submitted; interrupted preparation is not retried"
                            .into(),
                    ),
                )
                .await?;
            return Ok(StatusResult {
                output: run.message.clone(),
                run,
            });
        }
        // Read receipt first: logs cannot still be growing after a valid receipt.
        let code =
            read_bounded(&directory.join("exit"), 256)
                .ok()
                .and_then(|(bytes, oversized)| {
                    if oversized {
                        None
                    } else {
                        exit_code(&bytes, run_id)
                    }
                });
        let running = if code.is_none() {
            if let Some(pane) = pane_id {
                self.command(vec![
                    "pane".into(),
                    "process-info".into(),
                    "--pane".into(),
                    pane.into(),
                ])
                .await
                .is_ok_and(|value| supervisor_running(&value, pane, directory))
            } else {
                false
            }
        } else {
            false
        };
        let transcript = read_bounded(&directory.join("events.jsonl"), LOG_LIMIT)
            .map(|(bytes, oversized)| parse_transcript(&bytes, oversized))
            .unwrap_or_default();
        if let Some(session) = transcript.session {
            // Best effort: a refresh during execution lets the retained shell
            // print the real resume command when the supervisor exits.
            let _ = write_new(
                &directory.join("resume.txt"),
                format!("opencode -s {session}\n").as_bytes(),
            );
            record.summary.session_id = Some(session);
        }
        let stderr = read_bounded(&directory.join("stderr.log"), 4096)
            .ok()
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        record.summary.state = match code {
            Some(0) if transcript.valid && transcript.success => RunState::Completed,
            Some(_) => RunState::Failed,
            None if running => RunState::Running,
            None => RunState::Disconnected,
        };
        let mut message = match code {
            Some(code) => format!(
                "OpenCode exited ({code}); {}",
                if transcript.valid {
                    "final report:"
                } else {
                    "missing or invalid final report:"
                }
            ),
            None if running => {
                "Away supervisor is running in its Herdr pane; awaiting OpenCode process exit."
                    .into()
            },
            None => "Away process lifecycle is uncertain; capacity must remain reserved.".into(),
        };
        if !transcript.text.is_empty() {
            message.push_str(&format!("\n{}", transcript.text));
        }
        if let Some(error) = transcript.diagnostic {
            message.push_str(&format!("\n{error}"));
        }
        if !stderr.is_empty() {
            message.push_str(&format!("\n{stderr}"));
        }
        message.push_str(&format!("\nRaw logs: {}", directory.display()));
        if let Some(session) = &record.summary.session_id {
            message.push_str(&format!(
                "\nResume in retained worktree: opencode -s {session}"
            ));
        }
        record.summary.message = Some(message.clone());
        record.summary.updated_at = Utc::now();
        self.registry.update(record.clone()).await?;
        Ok(StatusResult {
            run: record.summary,
            output: Some(message),
        })
    }

    pub(super) async fn away_stop(&self, run_id: &str) -> Result<()> {
        let status = self.away_refresh(run_id).await?;
        if terminal(status.run.state) {
            return Ok(());
        }
        // A pane may now contain a user's resumed session. Do not blindly send
        // Ctrl-C to it or signal a persisted PID which might have been reused.
        Err(Error::Disconnected("Away process ownership cannot be confirmed; interrupt it in the retained Herdr pane. No terminal state has been inferred.".into()))
    }

    pub(super) async fn away_verify_idle_workspace(
        &self,
        workspace: &str,
        root_pane: &str,
    ) -> Result<()> {
        let uncertain = || {
            Error::Disconnected(
                "Cannot delete Away worktree: current workspace panes are active or unconfirmed"
                    .into(),
            )
        };
        let value = self
            .command(vec![
                "pane".into(),
                "list".into(),
                "--workspace".into(),
                workspace.into(),
            ])
            .await
            .map_err(|_| uncertain())?;
        if value["result"]["type"] != "pane_list" {
            return Err(uncertain());
        }
        let panes = value["result"]["panes"].as_array().ok_or_else(uncertain)?;
        let mut seen = std::collections::BTreeSet::new();
        for pane in panes {
            let id = pane["pane_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(uncertain)?;
            if pane["workspace_id"] != workspace || !seen.insert(id) {
                return Err(uncertain());
            }
            let info = self
                .command(vec![
                    "pane".into(),
                    "process-info".into(),
                    "--pane".into(),
                    id.into(),
                ])
                .await
                .map_err(|_| uncertain())?;
            if !shell_idle(&info, id) {
                return Err(uncertain());
            }
        }
        if !seen.contains(root_pane) {
            return Err(uncertain());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("away-test-{}", Uuid::new_v4()));
            Self(SessionRegistry::prepare_app_data_dir(&root).unwrap())
        }

        async fn backend(&self) -> HerdrBackend {
            HerdrBackend::new(
                HerdrConfig {
                    executable: self.0.join("never-run-herdr"),
                },
                SessionRegistry::load(Some(self.0.join("registry.json")))
                    .await
                    .unwrap(),
            )
        }

        async fn record(&self, backend: &HerdrBackend, id: &str) {
            let now = Utc::now();
            backend
                .registry
                .insert(RunRecord {
                    summary: RunSummary {
                        confidential: false,
                        id: id.into(),
                        issue_key: "issue".into(),
                        workspace: None,
                        agent: "opencode".into(),
                        model: None,
                        state: RunState::Starting,
                        message: None,
                        session_id: None,
                        started_at: now,
                        updated_at: now,
                    },
                    session: BackendSession::HerdrAway {
                        workspace_id: Some("w1".into()),
                        pane_id: Some("w1:p1".into()),
                        directory: self.0.clone(),
                        submitted: true,
                    },
                    deletion: None,
                })
                .await
                .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn events(status: &str, reason: &str) -> Vec<u8> {
        let report = json!({"away_report":1,"status":status,"summary":"Implemented the issue","tests":"cargo test: passed"}).to_string();
        format!("{}\n{}\n{}\n",
            json!({"type":"step_start","timestamp":1789400000000u64,"sessionID":"ses_real123","part":{"id":"prt_start","type":"step-start","sessionID":"ses_real123","messageID":"msg_final"}}),
            json!({"type":"text","timestamp":1789400000100u64,"sessionID":"ses_real123","part":{"id":"prt_text","type":"text","sessionID":"ses_real123","messageID":"msg_final","text":report,"time":{"start":1789400000000u64,"end":1789400000100u64}}}),
            json!({"type":"step_finish","timestamp":1789400000101u64,"sessionID":"ses_real123","part":{"id":"prt_finish","type":"step-finish","sessionID":"ses_real123","messageID":"msg_final","reason":reason,"cost":0,"tokens":{"input":100,"output":50,"reasoning":0,"cache":{"read":0,"write":0}}}})).into_bytes()
    }

    fn process_info(directory: &Path, pid: u32) -> Value {
        json!({"id":"cli:pane:process-info","result":{"type":"pane_process_info","process_info":{
            "pane_id":"w1:p1","shell_pid":1,"foreground_process_group_id":pid,"tty":"/dev/ttys001",
            "foreground_processes":[{"pid":pid,"name":"sh","argv0":"/bin/sh","argv":["/bin/sh",directory.join("worker.sh")]}]
        }}})
    }

    fn idle_info(pane: &str) -> Value {
        // Captured read-only from installed Herdr 0.9.0; only IDs/cwd normalized.
        json!({"id":"cli:pane:process_info","result":{"process_info":{
            "foreground_process_group_id":42831,"foreground_processes":[{
                "argv":["-zsh"],"argv0":"zsh","cmdline":"-zsh","cwd":"/fixture/tree","name":"zsh","pid":42831
            }],"pane_id":pane,"shell_pid":42831},"type":"pane_process_info"}})
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn historical_completion_does_not_authorize_deleting_resumed_or_unknown_panes() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let backend = fixture.backend().await;
        fixture.record(&backend, "run").await;
        let historical = backend
            .registry
            .set_state(
                "run",
                RunState::Completed,
                Some("Retained historical report".into()),
            )
            .await
            .unwrap();
        let list_path = fixture.0.join("panes.json");
        let info_path = fixture.0.join("info.json");
        let second_path = fixture.0.join("second.json");
        let removed = fixture.0.join("removed");
        let panes = json!({"result":{"type":"pane_list","panes":[{"pane_id":"w1:p1","workspace_id":"w1"}]}});
        std::fs::write(&list_path, panes.to_string()).unwrap();
        std::fs::write(&second_path, idle_info("w1:p2").to_string()).unwrap();
        std::fs::write(&backend.config.executable, format!(r#"#!/bin/sh
case "$*" in
 'pane list --workspace w1') cat {list} ;;
 'pane process-info --pane w1:p1') cat {info} ;;
 'pane process-info --pane w1:p2') cat {second} ;;
 'worktree remove --workspace w1 --force') touch {removed}; printf '%s\n' '{{"result":{{"type":"worktree_removed"}}}}' ;;
 *) exit 99 ;;
esac
"#, list=path_quote(&list_path).unwrap(), info=path_quote(&info_path).unwrap(), second=path_quote(&second_path).unwrap(), removed=path_quote(&removed).unwrap())).unwrap();
        std::fs::set_permissions(
            &backend.config.executable,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let mut resumed = process_info(&fixture.0, 11560);
        resumed["result"]["process_info"]["foreground_processes"][0]["argv"] =
            json!(["opencode", "-s", "ses_resumed"]);
        for info in [
            resumed.clone(),
            json!({}),
            json!({"error":{"code":"pane_not_found"}}),
        ] {
            std::fs::write(&info_path, info.to_string()).unwrap();
            assert!(backend.delete_worktree("run", true, None).await.is_err());
            assert!(!removed.exists());
            let after = backend.registry.get("run").await.unwrap();
            assert_eq!(after.summary.state, historical.state);
            assert_eq!(after.summary.message, historical.message);
            assert_eq!(after.summary.updated_at, historical.updated_at);
            assert!(after.deletion.is_none());
        }
        let idle = idle_info("w1:p1");
        assert!(shell_idle(&idle, "w1:p1"));
        for argv in [
            json!(["opencode", "-s", "ses_resumed"]),
            json!(["/bin/sh", "-c", "opencode -s ses_resumed"]),
            Value::Null,
        ] {
            let mut busy = idle.clone();
            busy["result"]["process_info"]["foreground_processes"][0]["argv"] = argv;
            assert!(!shell_idle(&busy, "w1:p1"));
        }
        std::fs::write(&info_path, idle.to_string()).unwrap();
        let mut split = panes.clone();
        split["result"]["panes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pane_id":"w1:p2","workspace_id":"w1"}));
        std::fs::write(&list_path, split.to_string()).unwrap();
        resumed["result"]["process_info"]["pane_id"] = json!("w1:p2");
        std::fs::write(&second_path, resumed.to_string()).unwrap();
        assert!(backend.delete_worktree("run", true, None).await.is_err());
        assert!(!removed.exists());
        std::fs::write(&second_path, idle_info("w1:p2").to_string()).unwrap();
        backend.delete_worktree("run", true, None).await.unwrap();
        assert!(removed.exists());
        assert_eq!(
            backend.registry.get("run").await.unwrap().summary.state,
            RunState::Completed
        );
    }

    #[tokio::test]
    async fn restart_fails_only_preparation_without_submission_intent() {
        for submitted in [false, true] {
            for state in [RunState::Provisioning, RunState::Starting] {
                let fixture = Fixture::new();
                let backend = fixture.backend().await;
                fixture.record(&backend, "run").await;
                let mut record = backend.registry.get("run").await.unwrap();
                record.summary.state = state;
                if let BackendSession::HerdrAway {
                    submitted: stage, ..
                } = &mut record.session
                {
                    *stage = submitted;
                }
                backend.registry.update(record).await.unwrap();
                let restarted = fixture.backend().await;
                let result = restarted.refresh_away("run").await.unwrap();
                assert_eq!(
                    result.run.state,
                    if submitted {
                        RunState::Disconnected
                    } else {
                        RunState::Failed
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn recognized_permission_rejection_does_not_invalidate_a_successful_report() {
        let warning = b"\x1b[93m\x1b[1m! \x1b[0mpermission requested: bash (git status, git diff); auto-rejecting\r\n";
        for denied_tool in [false, true] {
            for status in ["success", "failure"] {
                let fixture = Fixture::new();
                let backend = fixture.backend().await;
                fixture.record(&backend, "run").await;
                let mut bytes = warning.to_vec();
                if denied_tool {
                    bytes.extend_from_slice(format!("{}\n", json!({"type":"tool_use","sessionID":"ses_real123","part":{"type":"tool","tool":"bash","messageID":"msg_before","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}})).as_bytes());
                }
                bytes.extend(events(status, "stop"));
                std::fs::write(fixture.0.join("events.jsonl"), bytes).unwrap();
                std::fs::write(fixture.0.join("exit"), "run 0\n").unwrap();
                assert_eq!(
                    backend.refresh_away("run").await.unwrap().run.state,
                    if status == "success" {
                        RunState::Completed
                    } else {
                        RunState::Failed
                    }
                );
            }
        }
        assert!(
            permission_diagnostic(b"! permission requested: bash (*); auto-rejecting").is_some()
        );
        for line in [
            "garbage",
            "{bad json}",
            "! permission requested: bash (*); approved",
            "! permission requested: bash (*); auto-rejecting EXTRA",
            "\x1b[2J! permission requested: bash (*); auto-rejecting",
        ] {
            let mut bytes = format!("{line}\n").into_bytes();
            bytes.extend(events("success", "stop"));
            assert!(!parse_transcript(&bytes, false).valid, "{line}");
        }
    }

    #[test]
    fn running_requires_exact_foreground_supervisor_identity() {
        let directory = Path::new("/fixture/away/run");
        let value = process_info(directory, 42);
        assert!(supervisor_running(&value, "w1:p1", directory));
        assert!(!supervisor_running(&value, "w2:p1", directory));
        assert!(!supervisor_running(
            &value,
            "w1:p1",
            Path::new("/fixture/away/other")
        ));
        for (pointer, replacement) in [
            ("/result/type", json!("pane_info")),
            (
                "/result/process_info/foreground_process_group_id",
                Value::Null,
            ),
            ("/result/process_info/foreground_process_group_id", json!(0)),
            ("/result/process_info/foreground_processes/0/pid", json!(1)),
            ("/result/process_info/foreground_processes/0/pid", json!(0)),
            (
                "/result/process_info/foreground_processes/0/argv",
                Value::Null,
            ),
            (
                "/result/process_info/foreground_processes/0/argv",
                json!(["opencode", "run", "--format", "json"]),
            ),
            (
                "/result/process_info/foreground_processes/0/argv",
                json!(["opencode", "-s", "ses_resumed"]),
            ),
            (
                "/result/process_info/foreground_processes/0/argv",
                json!(["/bin/sh", "-c", "echo /fixture/away/run/worker.sh"]),
            ),
        ] {
            let mut uncertain = value.clone();
            *uncertain.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                !supervisor_running(&uncertain, "w1:p1", directory),
                "{uncertain}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_supervisor_transitions_running_uncertain_and_exited_with_full_prompt() {
        use std::{os::unix::fs::PermissionsExt, process::Stdio};
        let fixture = Fixture::new();
        let backend = fixture.backend().await;
        fixture.record(&backend, "run").await;
        let tree = fixture.0.join("retained tree");
        let bin = fixture.0.join("bin");
        std::fs::create_dir(&tree).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let mut record = backend.registry.get("run").await.unwrap();
        record.summary.workspace = Some(WorkspaceRef {
            backend: BackendKind::Herdr,
            id: "w1".into(),
            host: None,
            path: Some(tree.clone()),
            branch: "fixture".into(),
        });
        backend.registry.update(record).await.unwrap();
        let prompt = format!(
            "Implement the issue.\nProfile says: push and open a PR.\nLiteral ' $(false)\n{REPORT_PROMPT}"
        );
        write_new(&fixture.0.join("prompt.txt"), prompt.as_bytes()).unwrap();
        write_new(&fixture.0.join("fixture.jsonl"), &events("success", "stop")).unwrap();
        let opencode = bin.join("opencode");
        write_new(
            &opencode,
            br#"#!/bin/sh
cat > "$FIXTURE/received-prompt"
pwd > "$FIXTURE/agent-cwd"
cat "$FIXTURE/fixture.jsonl"
i=0
while [ ! -f "$FIXTURE/release" ] && [ "$i" -lt 500 ]; do
    sleep 0.01
    i=$((i + 1))
done
[ -f "$FIXTURE/release" ] || exit 91
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(opencode, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = fixture.0.join("worker.sh");
        write_new(
            &script,
            supervisor(&fixture.0, &tree, "run", None, None)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg(&script)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE", &fixture.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let identity = process_info(&fixture.0, child.id().unwrap());
        let info_path = fixture.0.join("process-info.json");
        std::fs::write(&info_path, identity.to_string()).unwrap();
        write_new(
            &backend.config.executable,
            format!(
                "#!/bin/sh\n[ \"$*\" = 'pane process-info --pane w1:p1' ] || exit 99\ncat {}\n",
                path_quote(&info_path).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();
        std::fs::set_permissions(
            &backend.config.executable,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if read_bounded(&fixture.0.join("events.jsonl"), LOG_LIMIT)
                    .is_ok_and(|(bytes, oversized)| parse_transcript(&bytes, oversized).valid)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(child.try_wait().unwrap().is_none());
        assert!(!fixture.0.join("exit").exists());
        let running = backend.refresh_away("run").await.unwrap();
        assert_eq!(running.run.state, RunState::Running);
        assert_eq!(running.run.session_id.as_deref(), Some("ses_real123"));
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("received-prompt")).unwrap(),
            prompt
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("agent-cwd"))
                .unwrap()
                .trim(),
            tree.to_str().unwrap()
        );
        for instruction in [
            "Do not push, open a PR, merge, or close issues",
            "Ignore any profile instructions",
            "additional agents",
            "subagents",
        ] {
            assert!(prompt.contains(instruction));
        }
        // A stale log/final report is not liveness, and a lost response is not exit.
        std::fs::write(&info_path, "{}").unwrap();
        assert_eq!(
            backend.refresh_away("run").await.unwrap().run.state,
            RunState::Disconnected
        );
        std::fs::write(&info_path, identity.to_string()).unwrap();
        assert_eq!(
            backend.refresh_away("run").await.unwrap().run.state,
            RunState::Running
        );
        write_new(&fixture.0.join("release"), b"").unwrap();
        let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("opencode -s ses_real123")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("exit")).unwrap(),
            "run 0\n"
        );
        // Even a stale process-info response must not override the exit receipt.
        let completed = backend.refresh_away("run").await.unwrap();
        assert_eq!(completed.run.state, RunState::Completed);
        assert_eq!(completed.run.session_id.as_deref(), Some("ses_real123"));
        assert!(
            completed
                .run
                .message
                .unwrap()
                .contains("cargo test: passed")
        );
        assert!(tree.is_dir());
        assert!(fixture.0.join("events.jsonl").is_file());
        let reloaded = fixture.backend().await;
        assert_eq!(
            reloaded.refresh_away("run").await.unwrap().run.state,
            RunState::Completed
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("resume.txt")).unwrap(),
            "opencode -s ses_real123\n"
        );
    }

    #[test]
    fn parses_only_bounded_final_reports_for_the_finished_message() {
        let success = parse_transcript(&events("success", "stop"), false);
        assert!(success.valid && success.success);
        assert_eq!(success.session.as_deref(), Some("ses_real123"));
        assert!(success.text.contains("cargo test: passed"));
        let failure = parse_transcript(&events("failure", "stop"), false);
        assert!(failure.valid && !failure.success);
        assert!(!parse_transcript(&events("success", "tool-calls"), false).valid);
        assert!(!parse_transcript(&events("success", "stop"), true).valid);
        assert!(!parse_transcript(&events("maybe", "stop"), false).valid);
        let mut unfinished = events("success", "stop");
        unfinished.extend_from_slice(b"{\"type\":\"step_start\",\"sessionID\":\"ses_real123\"}\n");
        assert!(!parse_transcript(&unfinished, false).valid);
        let mut error = events("success", "stop");
        error.extend_from_slice(b"{\"type\":\"error\",\"sessionID\":\"ses_real123\",\"error\":{\"message\":\"permission denied\"}}\n");
        let parsed = parse_transcript(&error, false);
        assert!(!parsed.valid);
        assert!(parsed.diagnostic.unwrap().contains("permission denied"));
        let mut mixed = events("success", "stop");
        mixed.extend_from_slice(b"{\"type\":\"step_finish\",\"sessionID\":\"ses_other\",\"part\":{\"messageID\":\"msg_final\",\"reason\":\"stop\"}}\n");
        assert!(!parse_transcript(&mixed, false).valid);
        let mismatched = String::from_utf8(events("success", "stop"))
            .unwrap()
            .replacen("msg_final", "msg_other", 2);
        assert!(!parse_transcript(mismatched.as_bytes(), false).valid);
    }

    #[test]
    fn rejects_missing_tests_unstructured_text_and_oversized_reports() {
        for text in [
            "done".to_owned(),
            json!({"away_report":1,"status":"success","summary":"done"}).to_string(),
            "x".repeat(REPORT_LIMIT + 1),
        ] {
            let data = format!(
                "{}\n{}\n",
                json!({"type":"text","sessionID":"ses_real","part":{"text":text,"messageID":"msg_1"}}),
                json!({"type":"step_finish","sessionID":"ses_real","part":{"reason":"stop","messageID":"msg_1"}})
            );
            assert!(!parse_transcript(data.as_bytes(), false).valid);
        }
    }

    #[test]
    fn exit_receipt_is_exact_and_bound_to_reservation() {
        assert_eq!(exit_code(b"id 0\n", "id"), Some(0));
        assert_eq!(exit_code(b"id 137\n", "id"), Some(137));
        for value in [
            "id 0",
            "other 0\n",
            "id -1\n",
            "id 256\n",
            "id 0\nextra",
            "id unknown\n",
        ] {
            assert_eq!(exit_code(value.as_bytes(), "id"), None, "{value}");
        }
    }

    #[tokio::test]
    async fn refresh_requires_exit_and_terminal_results_survive_restart() {
        let fixture = Fixture::new();
        let backend = fixture.backend().await;
        fixture.record(&backend, "run").await;
        std::fs::write(fixture.0.join("events.jsonl"), events("success", "stop")).unwrap();
        let status = backend.refresh_away("run").await.unwrap();
        assert_eq!(status.run.state, RunState::Disconnected);
        assert_eq!(status.run.session_id.as_deref(), Some("ses_real123"));
        assert!(backend.stop("run").await.is_err());
        assert!(backend.delete_worktree("run", true, None).await.is_err());
        std::fs::write(fixture.0.join("exit"), "run 0\n").unwrap();
        let completed = backend.refresh("run").await.unwrap();
        assert_eq!(completed.run.state, RunState::Completed);
        assert!(
            completed
                .run
                .message
                .as_ref()
                .unwrap()
                .contains("opencode -s ses_real123")
        );
        std::fs::remove_file(fixture.0.join("events.jsonl")).unwrap();
        std::fs::write(fixture.0.join("exit"), "run 1\n").unwrap();
        let reloaded = fixture.backend().await;
        assert!(reloaded.owns_run("run").await);
        let restored = reloaded.refresh_away("run").await.unwrap();
        assert_eq!(restored.run.state, RunState::Completed);
        assert_eq!(restored.run.message, completed.run.message);
        assert_eq!(restored.run.updated_at, completed.run.updated_at);
        reloaded.stop("run").await.unwrap();
    }

    #[tokio::test]
    async fn exited_failure_and_unknown_receipts_never_complete() {
        for (receipt, data, expected) in [
            ("run 1\n", events("success", "stop"), RunState::Failed),
            ("run 0\n", events("failure", "stop"), RunState::Failed),
            ("run 0\n", Vec::new(), RunState::Failed),
            (
                "run unknown\n",
                events("success", "stop"),
                RunState::Disconnected,
            ),
            (
                "other 0\n",
                events("success", "stop"),
                RunState::Disconnected,
            ),
        ] {
            let fixture = Fixture::new();
            let backend = fixture.backend().await;
            fixture.record(&backend, "run").await;
            std::fs::write(fixture.0.join("events.jsonl"), data).unwrap();
            std::fs::write(fixture.0.join("exit"), receipt).unwrap();
            std::fs::write(fixture.0.join("stderr.log"), "blocked: permission denied").unwrap();
            let status = backend.refresh_away("run").await.unwrap();
            assert_eq!(status.run.state, expected);
            assert!(status.output.unwrap().contains("permission denied"));
            if expected == RunState::Failed {
                std::fs::write(fixture.0.join("exit"), "run 0\n").unwrap();
                std::fs::write(fixture.0.join("events.jsonl"), events("success", "stop")).unwrap();
                assert_eq!(
                    backend.refresh_away("run").await.unwrap().run.state,
                    RunState::Failed
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_preserves_literal_arguments_prompt_and_real_exit_status() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let bin = fixture.0.join("bin");
        std::fs::create_dir(&bin).unwrap();
        // The only executable named opencode in this test is this local fixture.
        let executable = bin.join("opencode");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FIXTURE/args\"\ncat > \"$FIXTURE/prompt\"\nprintf '%s\\n' fixture-output\nexit 23\n").unwrap();
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = fixture.0.join("logs ' $(false)");
        let worktree = fixture.0.join("tree ' $(false)");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        let prompt = "implement ' $(touch SHOULD_NOT_EXIST)\n--auto";
        write_new(&directory.join("prompt.txt"), prompt.as_bytes()).unwrap();
        let model = "provider/model' $(touch SHOULD_NOT_EXIST)";
        let script =
            supervisor(&directory, &worktree, "fixture", Some(model), Some("high")).unwrap();
        assert!(!script.contains("--auto"));
        write_new(&directory.join("worker.sh"), script.as_bytes()).unwrap();
        for _ in 0..2 {
            let output = tokio::process::Command::new("/bin/sh")
                .arg(directory.join("worker.sh"))
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("FIXTURE", &fixture.0)
                .output()
                .await
                .unwrap();
            assert!(output.status.success());
        }
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("args")).unwrap(),
            format!("run\n--format\njson\n--agent\nbuild\n--model\n{model}\n--variant\nhigh\n")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("prompt")).unwrap(),
            prompt
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("exit")).unwrap(),
            "fixture 23\n"
        );
        assert!(!worktree.join("SHOULD_NOT_EXIST").exists());
        assert!(quote("nul\0").is_err());
        let (bytes, oversized) = read_bounded(&directory.join("events.jsonl"), 4).unwrap();
        assert!(oversized);
        assert_eq!(bytes, b"fixt");
    }

    #[tokio::test]
    async fn existing_reservation_is_idempotent_without_any_external_commands() {
        let fixture = Fixture::new();
        let backend = fixture.backend().await;
        let id = Uuid::new_v4().to_string();
        fixture.record(&backend, &id).await;
        let issue = serde_json::from_value(json!({"key":{"provider":"github","host":"example.com","repository":"a/b","native_id":"1"},"identifier":"1","title":"Test","state":"open","labels":[],"blocked_by":[]})).unwrap();
        let request = DispatchRequest {
            private_fork: None,
            repository: Repository {
                root: "/never-read".into(),
                git_dir: "/never-read/.git".into(),
                remote: None,
                has_beads: false,
            },
            issue,
            prompt: "implement".into(),
            agent: "opencode".into(),
            branch: None,
            workspace_name: None,
            base_branch: None,
            model: None,
            effort: None,
            target: None,
        };
        let mut record = backend.registry.get(&id).await.unwrap();
        record.summary.issue_key = request.issue.key.canonical();
        backend.registry.update(record).await.unwrap();
        let (first, second) = tokio::join!(
            backend.dispatch_away(request.clone(), &id),
            backend.dispatch_away(request.clone(), &id)
        );
        assert_eq!(first.unwrap().run.id, id);
        assert_eq!(second.unwrap().run.id, id);
        assert_eq!(
            fixture
                .backend()
                .await
                .dispatch_away(request.clone(), &id)
                .await
                .unwrap()
                .run
                .id,
            id
        );
        assert!(matches!(
            backend.dispatch_away(request.clone(), "../../bad").await,
            Err(Error::AwayNotStarted(_))
        ));
        assert!(matches!(
            backend
                .dispatch_away(request.clone(), &Uuid::new_v4().to_string())
                .await,
            Err(Error::AwayNotStarted(_))
        ));
        let runner = crate::Runner::new([]);
        assert!(matches!(
            runner
                .dispatch_away(BackendKind::Herdr, request.clone(), &id)
                .await,
            Err(Error::AwayNotStarted(_))
        ));
        let mut invalid = request.clone();
        invalid.prompt.clear();
        assert!(matches!(
            backend.dispatch_away(invalid.clone(), &id).await,
            Err(Error::AwayNotStarted(_))
        ));
        assert!(matches!(
            runner.dispatch_away(BackendKind::Herdr, invalid, &id).await,
            Err(Error::AwayNotStarted(_))
        ));
        assert_eq!(backend.registry.summaries().await.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fresh = Fixture::new();
            let backend = fresh.backend().await;
            let repo = fresh.0.join("repo");
            std::fs::create_dir(&repo).unwrap();
            // Make the registry destination unwritable as a file after loading.
            std::fs::create_dir(fresh.0.join("registry.json")).unwrap();
            let status = json!({"client":{"version":"0.9.0","protocol":22},"server":{"running":true,"version":"0.9.0","protocol":22,"compatible":true,"restart_needed":false}});
            std::fs::write(
                &backend.config.executable,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' {}\n",
                    quote(&status.to_string()).unwrap()
                ),
            )
            .unwrap();
            std::fs::set_permissions(
                &backend.config.executable,
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let mut request = request;
            request.repository.root = repo;
            assert!(matches!(
                backend
                    .dispatch_away(request, &Uuid::new_v4().to_string())
                    .await,
                Err(Error::AwayNotStarted(_))
            ));
            assert!(backend.registry.summaries().await.is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_persists_before_fixture_agent_and_keeps_worktree_and_logs() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let tree = fixture.0.join("retained-tree");
        let bin = fixture.0.join("bin");
        for path in [&repo, &tree, &bin] {
            std::fs::create_dir(path).unwrap();
        }
        for args in [vec!["init"], vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let path = fixture.0.join("registry.json");
        let snapshot = fixture.0.join("before-agent.json");
        let launched = fixture.0.join("launched");
        let received_prompt = fixture.0.join("received-prompt");
        let data = fixture.0.join("fixture.jsonl");
        std::fs::write(&data, events("success", "stop")).unwrap();
        let opencode = bin.join("opencode");
        std::fs::write(
            &opencode,
            format!(
                "#!/bin/sh\ncp {} {} || exit 1\nprintf x >> {}\ncat > {}\ncat {}\n",
                path_quote(&path).unwrap(),
                path_quote(&snapshot).unwrap(),
                path_quote(&launched).unwrap(),
                path_quote(&received_prompt).unwrap(),
                path_quote(&data).unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&opencode, std::fs::Permissions::from_mode(0o700)).unwrap();
        let herdr = bin.join("herdr");
        let status = json!({"client":{"version":"0.9.0","protocol":22},"server":{"running":true,"version":"0.9.0","protocol":22,"compatible":true,"restart_needed":false}});
        let worktree = json!({"result":{"workspace":{"workspace_id":"w1"},"root_pane":{"pane_id":"w1:p1"},"worktree":{"path":tree}}});
        let worktree_response = fixture.0.join("worktree-response.json");
        let reject_submission = fixture.0.join("reject-submission");
        std::fs::write(&worktree_response, worktree.to_string()).unwrap();
        std::fs::write(
            &herdr,
            format!(
                r#"#!/bin/sh
export PATH={bin}:/usr/bin:/bin
case "$1 $2" in
 'status --json') printf '%s\n' {status} ;;
 'worktree create') cat {worktree_response} ;;
 'pane run') cp {registry} {snapshot}; [ ! -f {reject_submission} ] || exit 1; /bin/sh -c "$4" >/dev/null; printf '%s\n' '{{"result":{{"type":"ok"}}}}' ;;
 *) exit 99 ;;
esac
"#,
                bin = path_quote(&bin).unwrap(),
                status = quote(&status.to_string()).unwrap(),
                worktree_response = path_quote(&worktree_response).unwrap(),
                registry = path_quote(&path).unwrap(),
                snapshot = path_quote(&snapshot).unwrap(),
                reject_submission = path_quote(&reject_submission).unwrap()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Arc::new(HerdrBackend::new(
            HerdrConfig { executable: herdr },
            SessionRegistry::load(Some(path.clone())).await.unwrap(),
        ));
        let runner = crate::Runner::new([backend.clone() as Arc<dyn Backend>]);
        let request = DispatchRequest { private_fork: None, repository: Repository { root: repo.clone(), git_dir: repo.join(".git"), remote: None, has_beads: false }, issue: serde_json::from_value(json!({"key":{"provider":"github","host":"example.com","repository":"a/b","native_id":"1"},"identifier":"1","title":"Test","state":"open","labels":[],"blocked_by":[]})).unwrap(), prompt: "implement the fixture".into(), agent: "opencode".into(), branch: None, workspace_name: None, base_branch: None, model: None, effort: None, target: None };
        let id = Uuid::new_v4().to_string();
        runner
            .dispatch_away(BackendKind::Herdr, request.clone(), &id)
            .await
            .unwrap();
        let before: Vec<RunRecord> =
            serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
        assert_eq!(before[0].summary.id, id);
        assert_eq!(before[0].summary.state, RunState::Starting);
        assert!(before[0].summary.session_id.is_none());
        assert!(
            matches!(&before[0].session, BackendSession::HerdrAway { pane_id: Some(pane), submitted: true, .. } if pane == "w1:p1")
        );
        let completed = runner.refresh_away(&id).await.unwrap();
        assert_eq!(completed.run.state, RunState::Completed);
        assert_eq!(completed.run.session_id.as_deref(), Some("ses_real123"));
        assert_eq!(
            std::fs::read_to_string(received_prompt).unwrap(),
            format!("{}{REPORT_PROMPT}", request.prompt)
        );
        assert_eq!(
            completed.run.workspace.as_ref().unwrap().path.as_ref(),
            Some(&tree)
        );
        runner
            .dispatch_away(BackendKind::Herdr, request.clone(), &id)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&launched).unwrap(), "x");
        assert!(tree.exists());
        assert!(
            fixture
                .0
                .join("away")
                .join(id)
                .join("events.jsonl")
                .exists()
        );
        for attempted in [false, true] {
            let mut response = worktree.clone();
            if !attempted {
                response["result"]["worktree"]["path"] = Value::Null;
            }
            std::fs::write(&worktree_response, response.to_string()).unwrap();
            std::fs::write(&reject_submission, "").unwrap();
            let id = Uuid::new_v4().to_string();
            let result = runner
                .dispatch_away(BackendKind::Herdr, request.clone(), &id)
                .await
                .unwrap();
            let expected = if attempted {
                RunState::Disconnected
            } else {
                RunState::Failed
            };
            assert_eq!(result.run.state, expected);
            assert_eq!(result.run.workspace.as_ref().unwrap().id, "w1");
            assert!(result.run.session_id.is_none());
            let saved = backend.registry.get(&id).await.unwrap();
            assert!(
                matches!(saved.session, BackendSession::HerdrAway { submitted, .. } if submitted == attempted)
            );
            if attempted {
                let snapshot: Vec<RunRecord> =
                    serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
                assert!(matches!(
                    snapshot
                        .iter()
                        .find(|record| record.summary.id == id)
                        .unwrap()
                        .session,
                    BackendSession::HerdrAway {
                        submitted: true,
                        ..
                    }
                ));
            }
            let restarted = HerdrBackend::new(
                backend.config.clone(),
                SessionRegistry::load(Some(path.clone())).await.unwrap(),
            );
            assert_eq!(
                restarted.refresh_away(&id).await.unwrap().run.state,
                expected
            );
            assert_eq!(std::fs::read_to_string(&launched).unwrap(), "x");
        }
    }
}
