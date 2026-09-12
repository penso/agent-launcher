use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use agent_launcher_core::{
    BackendKind, Repository, RunState, RunSummary, WorkspaceRef, WorktreeInspection,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    Backend, BackendCapabilities, BackendDetection, Capability, DispatchRequest, DispatchResult,
    Error, OpenResult, Result, SessionRegistry, StatusResult,
    command::{contains_string, find_string, run_json, run_output},
    registry::{BackendSession, RunRecord},
    sanitize_branch, sanitize_workspace_name,
};

const MINIMUM_HERDR_VERSION: &str = "0.8.2";
const AGENT_START_ATTEMPTS: usize = 20;
const AGENT_START_RETRY_DELAY: Duration = Duration::from_millis(100);
const INITIAL_PROMPT_TIMEOUT_MS: &str = "10000";
const INITIAL_PROMPT_ATTEMPTS: usize = 2;
const INITIAL_PROMPT_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub struct HerdrConfig {
    pub executable: PathBuf,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            executable: "herdr".into(),
        }
    }
}

pub struct HerdrBackend {
    config: HerdrConfig,
    registry: Arc<SessionRegistry>,
}

impl HerdrBackend {
    pub fn new(config: HerdrConfig, registry: Arc<SessionRegistry>) -> Self {
        Self { config, registry }
    }

    async fn command(&self, args: Vec<OsString>) -> Result<Value> {
        tracing::debug!("running Herdr CLI");
        run_json(&self.config.executable, &args, None).await
    }

    async fn record(&self, run_id: &str) -> Result<RunRecord> {
        let record = self.registry.get(run_id).await?;
        if !matches!(record.session, BackendSession::Herdr { .. }) {
            return Err(Error::RunNotFound(run_id.to_string()));
        }
        Ok(record)
    }

    async fn status(&self) -> Result<HerdrStatus> {
        let status = self.reported_status().await?;
        validate_status(&status)?;
        Ok(status)
    }

    async fn reported_status(&self) -> Result<HerdrStatus> {
        let value = self.command(status_args()).await?;
        Ok(serde_json::from_value(value)?)
    }

    async fn start_agent(&self, name: &str, kind: &str, pane: &str) -> Result<()> {
        let args = agent_start_args(name, kind, pane);
        let mut last_error = None;
        for attempt in 1..=AGENT_START_ATTEMPTS {
            match self.command(args.clone()).await {
                Ok(_) => return Ok(()),
                Err(error) => {
                    tracing::debug!(attempt, %error, "Herdr agent start failed; retrying");
                    last_error = Some(error);
                    if attempt < AGENT_START_ATTEMPTS {
                        tokio::time::sleep(AGENT_START_RETRY_DELAY).await;
                    }
                },
            }
        }
        Err(last_error
            .unwrap_or_else(|| Error::InvalidResponse("Herdr agent start did not run".to_owned())))
    }

    async fn rollback_workspace(&self, workspace_id: &str, cause: Error) -> Error {
        match self.command(worktree_remove_args(workspace_id, true)).await {
            Ok(_) => cause,
            Err(cleanup) => Error::InvalidResponse(format!(
                "Herdr dispatch failed after creating workspace {workspace_id}: {cause}; workspace rollback failed: {cleanup}"
            )),
        }
    }

    async fn submit_initial_prompt(&self, name: &str, prompt: &str) -> Result<()> {
        let args = initial_agent_prompt_args(name, prompt);
        for attempt in 1..=INITIAL_PROMPT_ATTEMPTS {
            match self.command(args.clone()).await {
                Ok(_) => return Ok(()),
                Err(error)
                    if attempt < INITIAL_PROMPT_ATTEMPTS
                        && retryable_initial_prompt_error(&error) =>
                {
                    tracing::debug!(attempt, %error, "Herdr initial prompt had no effect; retrying");
                    tokio::time::sleep(INITIAL_PROMPT_RETRY_DELAY).await;
                },
                Err(error) => return Err(error),
            }
        }
        Err(Error::InvalidResponse(
            "Herdr initial prompt did not run".to_owned(),
        ))
    }
}

#[async_trait]
impl Backend for HerdrBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Herdr
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new([
            Capability::Detect,
            Capability::Dispatch,
            Capability::Refresh,
            Capability::SendInput,
            Capability::Stop,
            Capability::Open,
            Capability::DeleteWorktree,
        ])
    }

    async fn owns_run(&self, run_id: &str) -> bool {
        self.registry
            .get(run_id)
            .await
            .is_ok_and(|record| matches!(record.session, BackendSession::Herdr { .. }))
    }

    async fn detect(&self, _repository: &Repository) -> Result<BackendDetection> {
        let (manager_running, result) = match self.reported_status().await {
            Ok(status) => (status.server.running, validate_status(&status)),
            Err(error) => (false, Err(error)),
        };
        Ok(BackendDetection {
            backend: self.kind(),
            available: result.is_ok(),
            manager_running,
            capabilities: self.capabilities(),
            message: result.err().map(|error| error.to_string()),
            compute_targets: Vec::new(),
        })
    }

    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult> {
        request.validate()?;
        self.status().await?;

        let issue_hash = stable_hash(&request.issue.key.canonical());
        let workspace_name = request.workspace_name.clone().unwrap_or_else(|| {
            sanitize_workspace_name(&format!(
                "{}-{}",
                request.issue.identifier, request.issue.title
            ))
        });
        let branch = sanitize_branch(
            request
                .branch
                .as_deref()
                .unwrap_or(&format!("agent/{workspace_name}-{}", &issue_hash[..8])),
        );
        let args = resolved_worktree_create_args(
            &request.repository.root,
            &branch,
            &workspace_name,
            request.base_branch.as_deref(),
        )
        .await?;
        let response = self.command(args).await?;
        let worktree = parse_worktree(&response)?;

        let agent_name = format!("launcher-{}", &Uuid::new_v4().simple().to_string()[..23]);
        let launch_result: Result<()> = async {
            self.start_agent(&agent_name, &request.agent, &worktree.pane_id)
                .await?;
            self.submit_initial_prompt(&agent_name, &request.prompt)
                .await?;
            Ok(())
        }
        .await;
        if let Err(error) = launch_result {
            return Err(self.rollback_workspace(&worktree.workspace_id, error).await);
        }

        let now = Utc::now();
        let run_id = Uuid::new_v4().to_string();
        let summary = RunSummary {
            id: run_id,
            issue_key: request.issue.key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Herdr,
                id: worktree.workspace_id.clone(),
                host: None,
                path: worktree.path,
                branch,
            }),
            agent: request.agent,
            state: RunState::Running,
            message: None,
            session_id: Some(agent_name.clone()),
            started_at: now,
            updated_at: now,
        };
        self.registry
            .insert(RunRecord {
                summary: summary.clone(),
                session: BackendSession::Herdr {
                    workspace_id: worktree.workspace_id,
                    pane_id: worktree.pane_id,
                    agent_name,
                },
                deletion: None,
            })
            .await?;
        Ok(DispatchResult {
            run: summary,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Herdr { agent_name, .. } = &record.session else {
            unreachable!()
        };
        let agent = match self
            .command(vec!["agent".into(), "get".into(), agent_name.into()])
            .await
        {
            Ok(value) => parse_agent(&value)?,
            Err(error) => {
                let summary = self
                    .registry
                    .set_state(run_id, RunState::Disconnected, Some(error.to_string()))
                    .await?;
                return Ok(StatusResult {
                    run: summary,
                    output: None,
                });
            },
        };
        let output =
            run_output(&self.config.executable, &agent_read_args(agent_name), None).await?;
        let mut summary = record.summary;
        if summary.state != RunState::Cancelled {
            summary.state = map_agent_status(&agent.status);
            summary.message = match agent.status.as_str() {
                "blocked" => Some(
                    "Herdr reports the agent is blocked; the required interaction is not exposed"
                        .into(),
                ),
                "unknown" => Some("Herdr cannot classify the agent state".into()),
                _ => None,
            };
            summary.updated_at = Utc::now();
            self.registry.update_summary(summary.clone()).await?;
        }
        Ok(StatusResult {
            run: summary,
            output: (!output.is_empty()).then_some(output),
        })
    }

    async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Err(Error::InvalidRequest("input cannot be empty".into()));
        }
        let record = self.record(run_id).await?;
        let BackendSession::Herdr { agent_name, .. } = record.session else {
            unreachable!()
        };
        self.command(agent_prompt_args(&agent_name, text)).await?;
        self.registry
            .set_state(run_id, RunState::Running, None)
            .await?;
        Ok(())
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        let record = self.record(run_id).await?;
        if record.summary.state == RunState::Cancelled {
            return Ok(());
        }
        let BackendSession::Herdr { agent_name, .. } = record.session else {
            unreachable!()
        };
        let agent = self
            .command(vec![
                "agent".into(),
                "get".into(),
                agent_name.clone().into(),
            ])
            .await?;
        if parse_agent(&agent)?.status == "blocked" {
            self.command(agent_send_keys_args(&agent_name, "esc"))
                .await?;
        }
        self.command(agent_send_keys_args(&agent_name, "ctrl+c"))
            .await?;
        self.registry
            .set_state(
                run_id,
                RunState::Cancelled,
                Some("interrupt sent through Herdr".into()),
            )
            .await?;
        Ok(())
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Herdr { agent_name, .. } = record.session else {
            unreachable!()
        };
        self.command(vec![
            "agent".into(),
            "focus".into(),
            agent_name.clone().into(),
        ])
        .await?;
        let uri = Url::parse(&format!("herdr://agent/{agent_name}"))
            .map_err(|error| Error::InvalidResponse(error.to_string()))?;
        Ok(OpenResult {
            uri,
            launched: true,
        })
    }

    async fn delete_worktree(
        &self,
        run_id: &str,
        force: bool,
        _expected: Option<&WorktreeInspection>,
    ) -> Result<()> {
        let record = self.record(run_id).await?;
        let BackendSession::Herdr { workspace_id, .. } = record.session else {
            unreachable!()
        };
        self.registry.begin_deletion(run_id, force, None).await?;
        if let Err(error) = self
            .command(worktree_remove_args(&workspace_id, force))
            .await
        {
            let removed = self
                .command(vec!["worktree".into(), "list".into(), "--json".into()])
                .await
                .is_ok_and(|worktrees| !contains_string(&worktrees, &workspace_id));
            if !removed {
                let _ = self.registry.cancel_deletion(run_id).await;
                return Err(error);
            }
        }
        self.registry.complete_deletion(run_id).await?;
        Ok(())
    }

    async fn deletion_pending(&self, run_id: &str) -> bool {
        self.registry.deletion_pending(run_id).await
    }

    async fn finalize_deletion(&self, run_id: &str) -> Result<()> {
        self.registry.finalize_deletion(run_id).await
    }
}

#[derive(Debug, Deserialize)]
struct HerdrStatus {
    client: HerdrClientStatus,
    server: HerdrServerStatus,
}

#[derive(Debug, Deserialize)]
struct HerdrClientStatus {
    version: String,
    protocol: u64,
}

#[derive(Debug, Deserialize)]
struct HerdrServerStatus {
    running: bool,
    version: Option<String>,
    protocol: Option<u64>,
    compatible: Option<bool>,
    restart_needed: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct WorktreeResult {
    workspace_id: String,
    pane_id: String,
    path: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
struct AgentResult {
    status: String,
}

fn validate_status(status: &HerdrStatus) -> Result<()> {
    if !version_at_least(&status.client.version, MINIMUM_HERDR_VERSION) {
        return Err(Error::InvalidResponse(format!(
            "Herdr {} is too old; version {MINIMUM_HERDR_VERSION} or newer is required",
            status.client.version
        )));
    }
    if !status.server.running {
        return Err(Error::InvalidResponse("Herdr server is not running".into()));
    }
    let server_version = status
        .server
        .version
        .as_deref()
        .ok_or_else(|| Error::InvalidResponse("Herdr status has no server version".into()))?;
    let server_protocol = status
        .server
        .protocol
        .ok_or_else(|| Error::InvalidResponse("Herdr status has no server protocol".into()))?;
    if status.client.version != server_version
        || status.client.protocol != server_protocol
        || status.server.compatible != Some(true)
        || status.server.restart_needed
    {
        return Err(Error::InvalidResponse(format!(
            "Herdr client {} protocol {} is incompatible with server {server_version} protocol {server_protocol}; restart or update Herdr",
            status.client.version, status.client.protocol
        )));
    }
    Ok(())
}

fn version_at_least(actual: &str, required: &str) -> bool {
    fn parts(value: &str) -> Option<Vec<u64>> {
        value
            .split_once('-')
            .map_or(value, |(version, _)| version)
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()
    }
    matches!((parts(actual), parts(required)), (Some(actual), Some(required)) if actual >= required)
}

fn parse_worktree(value: &Value) -> Result<WorktreeResult> {
    let workspace_id = value
        .pointer("/result/workspace/workspace_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::InvalidResponse("Herdr worktree response has no workspace id".into())
        })?;
    let pane_id = value
        .pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::InvalidResponse("Herdr worktree response has no root pane id".into())
        })?;
    let path = value
        .pointer("/result/worktree/path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Ok(WorktreeResult {
        workspace_id: workspace_id.into(),
        pane_id: pane_id.into(),
        path,
    })
}

fn parse_agent(value: &Value) -> Result<AgentResult> {
    let agent = value
        .pointer("/result/agent")
        .ok_or_else(|| Error::InvalidResponse("Herdr agent response has no agent".into()))?;
    let status = find_string(agent, &["agent_status"])
        .ok_or_else(|| Error::InvalidResponse("Herdr agent response has no status".into()))?;
    Ok(AgentResult { status })
}

fn map_agent_status(status: &str) -> RunState {
    match status {
        "working" => RunState::Running,
        "blocked" => RunState::NeedsInput,
        "idle" => RunState::Idle,
        "done" => RunState::Completed,
        _ => RunState::Disconnected,
    }
}

fn status_args() -> Vec<OsString> {
    vec!["status".into(), "--json".into()]
}

async fn resolved_worktree_create_args(
    source: &std::path::Path,
    branch: &str,
    label: &str,
    base: Option<&str>,
) -> Result<Vec<OsString>> {
    let git = std::path::Path::new("git");
    let output = run_output(
        git,
        &["worktree", "list", "--porcelain", "-z"].map(OsString::from),
        Some(source),
    )
    .await?;
    // Git lists the primary worktree first. NUL delimiters preserve whitespace
    // and avoid Git's path quoting.
    let record = output.split("\0\0").next().unwrap_or_default();
    if record.split('\0').any(|field| field == "bare") {
        return Err(Error::InvalidRequest(
            "Herdr requires a non-bare primary checkout; use a repository cloned without --bare"
                .into(),
        ));
    }
    let primary = record
        .split('\0')
        .next()
        .and_then(|field| field.strip_prefix("worktree "))
        .filter(|path| std::path::Path::new(path).is_absolute())
        .ok_or_else(|| Error::InvalidResponse("Git did not report a primary checkout".into()))?;
    let common_dir = run_output(
        git,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"].map(OsString::from),
        Some(source),
    )
    .await?;
    let common_dir = PathBuf::from(common_dir.trim_end_matches('\n'));
    let source_git_dir = run_output(
        git,
        &["rev-parse", "--absolute-git-dir"].map(OsString::from),
        Some(source),
    )
    .await?;
    let missing_primary = || {
        Error::InvalidRequest(
            "Herdr cannot locate the primary checkout; launch from the primary checkout \
             or configure its core.worktree for the separate Git directory"
                .into(),
        )
    };
    let primary = if tokio::fs::canonicalize(source_git_dir.trim_end_matches('\n')).await?
        == tokio::fs::canonicalize(&common_dir).await?
    {
        source.to_path_buf()
    } else {
        // For separate git-dirs, Git's list guesses from the metadata path.
        // Let Git honor the primary's core.worktree (including config.worktree).
        let path = run_output(
            git,
            &[
                OsString::from("--git-dir"),
                common_dir.as_os_str().to_owned(),
                OsString::from("rev-parse"),
                OsString::from("--show-toplevel"),
            ],
            Some(std::path::Path::new(primary)),
        )
        .await
        .map_err(|_| missing_primary())?;
        let path = PathBuf::from(path.trim_end_matches('\n'));
        if path == common_dir || !path.join(".git").exists() {
            return Err(missing_primary());
        }
        path
    };
    let base = match base {
        Some(base) => base.to_owned(),
        // Herdr resolves HEAD relative to --cwd, not the original workspace.
        None => run_output(
            git,
            &["rev-parse", "--verify", "HEAD^{commit}"].map(OsString::from),
            Some(source),
        )
        .await?
        .trim()
        .to_owned(),
    };
    Ok(worktree_create_args(&primary, branch, label, Some(&base)))
}

fn worktree_create_args(
    repository: &std::path::Path,
    branch: &str,
    label: &str,
    base: Option<&str>,
) -> Vec<OsString> {
    let mut args = vec![
        "worktree".into(),
        "create".into(),
        "--cwd".into(),
        repository.as_os_str().to_owned(),
        "--branch".into(),
        branch.into(),
    ];
    if let Some(base) = base {
        args.extend([OsString::from("--base"), base.into()]);
    }
    args.extend([
        OsString::from("--label"),
        label.into(),
        OsString::from("--no-focus"),
    ]);
    args
}

fn worktree_remove_args(workspace_id: &str, force: bool) -> Vec<OsString> {
    let mut args = vec![
        "worktree".into(),
        "remove".into(),
        "--workspace".into(),
        workspace_id.into(),
    ];
    if force {
        args.push("--force".into());
    }
    args
}

fn agent_start_args(name: &str, kind: &str, pane: &str) -> Vec<OsString> {
    vec![
        "agent".into(),
        "start".into(),
        name.into(),
        "--kind".into(),
        kind.into(),
        "--pane".into(),
        pane.into(),
    ]
}

fn agent_prompt_args(name: &str, prompt: &str) -> Vec<OsString> {
    vec!["agent".into(), "prompt".into(), name.into(), prompt.into()]
}

fn initial_agent_prompt_args(name: &str, prompt: &str) -> Vec<OsString> {
    let mut args = agent_prompt_args(name, prompt);
    args.extend([
        OsString::from("--wait"),
        OsString::from("--until"),
        OsString::from("working"),
        OsString::from("--until"),
        OsString::from("blocked"),
        OsString::from("--until"),
        OsString::from("done"),
        OsString::from("--until"),
        OsString::from("idle"),
        OsString::from("--timeout"),
        OsString::from(INITIAL_PROMPT_TIMEOUT_MS),
    ]);
    args
}

fn retryable_initial_prompt_error(error: &Error) -> bool {
    matches!(
        error,
        Error::CommandFailed { stderr, .. }
            if stderr.contains("agent_prompt_stalled") || stderr.contains("agent_not_ready")
    )
}

fn agent_read_args(name: &str) -> Vec<OsString> {
    vec![
        "agent".into(),
        "read".into(),
        name.into(),
        "--source".into(),
        "visible".into(),
        "--lines".into(),
        "240".into(),
    ]
}

fn agent_send_keys_args(name: &str, key: &str) -> Vec<OsString> {
    vec!["agent".into(), "send-keys".into(), name.into(), key.into()]
}

fn stable_hash(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes())
        .simple()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    struct TestRepository(PathBuf);

    impl TestRepository {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("herdr-source-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(cwd)
            .args([
                "-c",
                "user.name=Herdr Test",
                "-c",
                "user.email=herdr@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[tokio::test]
    async fn resolves_primary_checkout_and_preserves_source_base() {
        for separate_git_dir in [false, true] {
            let temp = TestRepository::new();
            let primary = temp.0.join("primary checkout \u{e9}");
            let linked = temp.0.join("linked checkout \u{e9}");
            let git_dir = temp.0.join("separate metadata");
            if separate_git_dir {
                git(&temp.0, &[
                    "init",
                    "--separate-git-dir",
                    git_dir.to_str().unwrap(),
                    primary.to_str().unwrap(),
                ]);
            } else {
                git(&temp.0, &["init", primary.to_str().unwrap()]);
            }
            git(&primary, &["commit", "--allow-empty", "-m", "primary"]);
            let primary_head = git(&primary, &["rev-parse", "HEAD"]);
            assert_eq!(
                resolved_worktree_create_args(&primary, "agent/fix", "fix", None)
                    .await
                    .unwrap(),
                worktree_create_args(&primary, "agent/fix", "fix", Some(&primary_head)),
            );
            git(&primary, &[
                "worktree",
                "add",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ]);
            git(&linked, &["commit", "--allow-empty", "-m", "linked"]);
            let linked_head = git(&linked, &["rev-parse", "HEAD"]);
            assert_ne!(primary_head, linked_head);
            if separate_git_dir {
                let error = resolved_worktree_create_args(&linked, "agent/fix", "fix", None)
                    .await
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("launch from the primary checkout")
                );
                git(&primary, &["config", "extensions.worktreeConfig", "true"]);
                git(&primary, &[
                    "config",
                    "--worktree",
                    "core.worktree",
                    "../primary checkout \u{e9}",
                ]);
            }
            assert_eq!(
                resolved_worktree_create_args(&linked, "agent/fix", "fix", None)
                    .await
                    .unwrap(),
                worktree_create_args(&primary, "agent/fix", "fix", Some(&linked_head)),
            );
            for base in ["HEAD~1", "linked"] {
                assert_eq!(
                    resolved_worktree_create_args(&linked, "agent/fix", "fix", Some(base))
                        .await
                        .unwrap(),
                    worktree_create_args(&primary, "agent/fix", "fix", Some(base)),
                );
            }
            git(&linked, &["checkout", "--detach"]);
            assert_eq!(
                resolved_worktree_create_args(&linked, "agent/fix", "fix", None)
                    .await
                    .unwrap(),
                worktree_create_args(&primary, "agent/fix", "fix", Some(&linked_head)),
            );
        }
    }

    #[tokio::test]
    async fn rejects_bare_primary_with_actionable_error() {
        let temp = TestRepository::new();
        let seed = temp.0.join("seed");
        let bare = temp.0.join("bare.git");
        let linked = temp.0.join("linked");
        git(&temp.0, &["init", seed.to_str().unwrap()]);
        git(&seed, &["commit", "--allow-empty", "-m", "seed"]);
        git(&temp.0, &[
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ]);
        git(&bare, &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ]);
        for source in [&bare, &linked] {
            let error = resolved_worktree_create_args(source, "agent/fix", "fix", None)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::InvalidRequest(_)));
            assert!(
                error
                    .to_string()
                    .contains("use a repository cloned without --bare")
            );
        }
    }

    #[test]
    fn builds_documented_dispatch_commands() {
        assert_eq!(
            worktree_create_args(Path::new("/repo with space"), "agent/fix", "fix", None),
            args(&[
                "worktree",
                "create",
                "--cwd",
                "/repo with space",
                "--branch",
                "agent/fix",
                "--label",
                "fix",
                "--no-focus",
            ])
        );
        assert_eq!(
            agent_start_args("launcher-123", "opencode", "w1:p1"),
            args(&[
                "agent",
                "start",
                "launcher-123",
                "--kind",
                "opencode",
                "--pane",
                "w1:p1",
            ])
        );
        assert_eq!(
            agent_prompt_args("launcher-123", "fix it; safely"),
            args(&["agent", "prompt", "launcher-123", "fix it; safely"])
        );
        assert_eq!(
            initial_agent_prompt_args("launcher-123", "fix it; safely"),
            args(&[
                "agent",
                "prompt",
                "launcher-123",
                "fix it; safely",
                "--wait",
                "--until",
                "working",
                "--until",
                "blocked",
                "--until",
                "done",
                "--until",
                "idle",
                "--timeout",
                "10000",
            ])
        );
        assert_eq!(
            agent_read_args("launcher-123"),
            args(&[
                "agent",
                "read",
                "launcher-123",
                "--source",
                "visible",
                "--lines",
                "240",
            ])
        );
        assert_eq!(
            agent_send_keys_args("launcher-123", "ctrl+c"),
            args(&["agent", "send-keys", "launcher-123", "ctrl+c"])
        );
        assert_eq!(status_args(), args(&["status", "--json"]));
        assert_eq!(
            worktree_remove_args("w2", true),
            args(&["worktree", "remove", "--workspace", "w2", "--force"])
        );
    }

    #[test]
    fn retries_only_initial_prompt_failures_that_sent_no_effective_turn() {
        let error = |stderr: &str| Error::CommandFailed {
            program: "herdr agent prompt".into(),
            status: "1".into(),
            stderr: stderr.into(),
        };
        assert!(retryable_initial_prompt_error(&error(
            r#"{"error":{"code":"agent_prompt_stalled"}}"#
        )));
        assert!(retryable_initial_prompt_error(&error(
            r#"{"error":{"code":"agent_not_ready"}}"#
        )));
        assert!(!retryable_initial_prompt_error(&error(
            r#"{"error":{"code":"agent_blocked"}}"#
        )));
    }

    #[test]
    fn parses_authoritative_worktree_and_agent_shapes() {
        let worktree = json!({
            "id": "req",
            "result": {
                "workspace": {"workspace_id": "w2"},
                "root_pane": {"pane_id": "w2:p1"},
                "worktree": {"path": "/tmp/repo/fix"}
            }
        });
        assert_eq!(
            parse_worktree(&worktree).expect("worktree should parse"),
            WorktreeResult {
                workspace_id: "w2".into(),
                pane_id: "w2:p1".into(),
                path: Some("/tmp/repo/fix".into()),
            }
        );
        let agent = json!({"result": {"agent": {"agent_status": "blocked"}}});
        assert_eq!(
            parse_agent(&agent).expect("agent should parse"),
            AgentResult {
                status: "blocked".into()
            }
        );
        assert_eq!(map_agent_status("blocked"), RunState::NeedsInput);
        assert_eq!(map_agent_status("done"), RunState::Completed);
        assert_eq!(map_agent_status("future"), RunState::Disconnected);
    }

    #[test]
    fn validates_status_protocol_and_version() {
        let valid: HerdrStatus = serde_json::from_value(json!({
            "client": {"version": "0.8.2", "protocol": 20},
            "server": {
                "running": true,
                "version": "0.8.2",
                "protocol": 20,
                "compatible": true,
                "restart_needed": false
            }
        }))
        .expect("status should deserialize");
        validate_status(&valid).expect("matching status should validate");

        let incompatible: HerdrStatus = serde_json::from_value(json!({
            "client": {"version": "0.8.2", "protocol": 20},
            "server": {
                "running": true,
                "version": "0.7.4",
                "protocol": 16,
                "compatible": false,
                "restart_needed": true
            }
        }))
        .expect("status should deserialize");
        assert!(incompatible.server.running);
        assert!(validate_status(&incompatible).is_err());
        assert!(version_at_least("0.8.2-preview.1", "0.7.4"));
    }
}
