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

#[path = "away.rs"]
mod away;

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
        if !matches!(
            record.session,
            BackendSession::Herdr { .. } | BackendSession::HerdrAway { .. }
        ) {
            return Err(Error::RunNotFound(run_id.to_string()));
        }
        Ok(record)
    }

    async fn status(&self) -> Result<HerdrStatus> {
        let status = self.reported_status().await?;
        if let Some(warning) = validate_status(&status)? {
            tracing::warn!("{warning}");
        }
        Ok(status)
    }

    async fn reported_status(&self) -> Result<HerdrStatus> {
        let value = self.command(status_args()).await?;
        Ok(serde_json::from_value(value)?)
    }

    async fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane: &str,
        model: Option<&str>,
    ) -> Result<()> {
        let args = agent_start_args(name, kind, pane, model);
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

    async fn private_prompt(&self, name: &str, text: &str) -> Result<()> {
        let path = private_socket_path()?;
        let response = private_ipc(
            &path,
            "agent.prompt",
            serde_json::json!({
                "target": name, "text": text
            }),
        )
        .await?;
        if response["result"]["type"] != "agent_prompted" {
            return Err(Error::PrivateSecurity);
        }
        // The API acknowledgement is submission, not completion of an agent turn.
        // Keep CLI readiness polling separate, with only a generic target in argv.
        self.command(vec![
            "agent".into(),
            "wait".into(),
            name.into(),
            "--until".into(),
            "working".into(),
            "--until".into(),
            "blocked".into(),
            "--until".into(),
            "done".into(),
            "--until".into(),
            "idle".into(),
            "--timeout".into(),
            INITIAL_PROMPT_TIMEOUT_MS.into(),
        ])
        .await
        .map_err(|_| Error::PrivateSecurity)?;
        Ok(())
    }
}

// Wire contracts pinned to herdrdev/herdr:
// 0.8.2: 9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c
// 0.9.0: b99002ac99b09e00b4ca692436cb15a6b0d676f1
// src/{session.rs,config/io.rs,api/client.rs,api/schema{.rs,/agents.rs,/response.rs}}.
// No --session CLI option is supplied by this adapter, so the socket override wins.
fn private_socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("HERDR_SOCKET_PATH") {
        return Ok(path.into());
    }
    let mut root = if let Ok(root) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(root).join("herdr")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/herdr")
    } else {
        std::env::temp_dir().join("herdr")
    };
    if let Ok(name) = std::env::var("HERDR_SESSION") {
        if name.is_empty()
            || name.len() > 64
            || matches!(name.as_str(), "." | "..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(Error::PrivateSecurity);
        }
        if name != "default" {
            root = root.join("sessions").join(name);
        }
    }
    Ok(root.join("herdr.sock"))
}

#[cfg(unix)]
async fn private_ipc(path: &std::path::Path, method: &str, params: Value) -> Result<Value> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    crate::private::redact_errors(true, async {
        tokio::time::timeout(Duration::from_secs(15), async {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.file_type().is_socket()
                || metadata.mode() & 0o7777 != 0o600
                || metadata.uid() != rustix::process::getuid().as_raw()
            {
                return Err(Error::PrivateSecurity);
            }
            let mut stream = tokio::net::UnixStream::connect(path).await?;
            // Check the connected peer as well as the pathname (which can be replaced).
            if stream.peer_cred()?.uid() != rustix::process::getuid().as_raw() {
                return Err(Error::PrivateSecurity);
            }
            let id = Uuid::new_v4().to_string();
            let mut bytes = serde_json::to_vec(
                &serde_json::json!({"id": id, "method": method, "params": params}),
            )?;
            bytes.push(b'\n');
            stream.write_all(&bytes).await?;
            let mut response = Vec::new();
            BufReader::new(stream.take(1024 * 1024 + 1))
                .read_until(b'\n', &mut response)
                .await?;
            if response.len() > 1024 * 1024 || response.last() != Some(&b'\n') {
                return Err(Error::PrivateSecurity);
            }
            let value: Value = serde_json::from_slice(&response)?;
            if value["id"] != id || value.get("error").is_some() || !value["result"].is_object() {
                return Err(Error::PrivateSecurity);
            }
            Ok(value)
        })
        .await
        .map_err(|_| Error::PrivateSecurity)?
    })
    .await
}

#[cfg(not(unix))]
async fn private_ipc(_path: &std::path::Path, _method: &str, _params: Value) -> Result<Value> {
    Err(Error::PrivateSecurity)
}

/// Call before creating an advisory fork/checkout, using the configured backend.
/// Only audited releases are accepted; debug builds need HERDR_SOCKET_PATH.
pub async fn verify_private_herdr_transport(runner: &crate::Runner) -> Result<()> {
    crate::private::redact_errors(true, async {
        runner
            .backend(BackendKind::Herdr)?
            .verify_private_transport()
            .await
    })
    .await
}

fn validate_private_status(status: &HerdrStatus, path: &std::path::Path) -> Result<()> {
    validate_status(status).map_err(|_| Error::PrivateSecurity)?;
    // Private dispatch stays pinned to one audited release on both ends.
    if status.server.version.as_deref() != Some(status.client.version.as_str()) {
        return Err(Error::PrivateSecurity);
    }
    let protocol = match status.client.version.as_str() {
        "0.8.2" => 20,
        "0.9.0" => 22,
        _ => return Err(Error::PrivateSecurity),
    };
    if status.client.protocol != protocol || status.server.socket.as_deref() != Some(path) {
        return Err(Error::PrivateSecurity);
    }
    Ok(())
}

struct ProvisionalWorkspace {
    executable: PathBuf,
    workspace: Option<String>,
}

impl ProvisionalWorkspace {
    async fn cleanup(&mut self) {
        if let Some(workspace) = &self.workspace {
            let args = vec![
                "workspace".into(),
                "close".into(),
                "--workspace".into(),
                workspace.into(),
            ];
            if run_output(&self.executable, &args, None).await.is_ok() {
                self.workspace = None;
            }
        }
    }
}

impl Drop for ProvisionalWorkspace {
    fn drop(&mut self) {
        if let Some(workspace) = self.workspace.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let executable = self.executable.clone();
            runtime.spawn(async move {
                // Do not capture another Drop guard: shutdown may discard this
                // task before polling it, recursively spawning cleanup forever.
                let args = vec![
                    "workspace".into(),
                    "close".into(),
                    "--workspace".into(),
                    workspace.into(),
                ];
                let _ = run_output(&executable, &args, None).await;
            });
        }
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
            Capability::Away,
        ])
    }

    async fn owns_run(&self, run_id: &str) -> bool {
        self.registry.get(run_id).await.is_ok_and(|record| {
            matches!(
                record.session,
                BackendSession::Herdr { .. } | BackendSession::HerdrAway { .. }
            )
        })
    }

    async fn confidential_runs(&self) -> Vec<RunSummary> {
        let mut runs = Vec::new();
        for mut run in self.registry.summaries().await {
            if run.confidential && self.owns_run(&run.id).await {
                run.message = None;
                runs.push(run);
            }
        }
        runs
    }

    async fn verify_private_transport(&self) -> Result<()> {
        crate::private::redact_errors(true, async {
            let path = private_socket_path()?;
            let status = self.reported_status().await?;
            validate_private_status(&status, &path)?;
            let response = private_ipc(&path, "ping", serde_json::json!({})).await?;
            if response["result"]["type"] != "pong"
                || response["result"]["version"] != status.client.version
                || response["result"]["protocol"].as_u64() != Some(status.client.protocol)
            {
                return Err(Error::PrivateSecurity);
            }
            Ok(())
        })
        .await
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
            message: result.unwrap_or_else(|error| Some(error.to_string())),
            compute_targets: Vec::new(),
        })
    }

    fn verify_private_storage(&self) -> Result<()> {
        self.registry.verify_private_storage()
    }

    async fn dispatch(&self, mut request: DispatchRequest) -> Result<DispatchResult> {
        let private = request.private_fork.is_some() || request.issue.security_advisory.is_some();
        let result = async {
            request.validate()?;
            if private {
                self.verify_private_storage()?;
                self.verify_private_transport().await?;
            }
            crate::private::guard_dispatch(&request, self.kind()).await?;
            if request.model.is_some()
                && !matches!(request.agent.as_str(), "claude" | "opencode" | "pi")
            {
                return Err(Error::InvalidRequest(
                    "Herdr model selection is supported only for claude, opencode, and pi".into(),
                ));
            }
            if !private {
                self.status().await?;
            }
            crate::private::private_branch(&mut request).await?;

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
            let args = if request.private_fork.is_some() {
                private_workspace_args(&request.repository.root)
            } else {
                resolved_worktree_create_args(
                    &request.repository.root,
                    &branch,
                    &workspace_name,
                    request.base_branch.as_deref(),
                )
                .await?
            };
            let response = self.command(args).await?;
            let mut provisional = ProvisionalWorkspace {
                executable: self.config.executable.clone(),
                workspace: if private {
                    response
                        .pointer("/result/workspace/workspace_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                } else {
                    None
                },
            };
            let mut worktree = match parse_worktree(&response) {
                Ok(worktree) => worktree,
                Err(error) => {
                    provisional.cleanup().await;
                    return Err(error);
                },
            };
            if request.private_fork.is_some() {
                worktree.path = Some(request.repository.root.clone());
            }

            let agent_name = format!("launcher-{}", &Uuid::new_v4().simple().to_string()[..23]);
            let launch_result: Result<()> = async {
                if let Some(fork) = &request.private_fork {
                    crate::verify_private_checkout(&request.repository, fork).await?;
                    self.command(agent_start_args(
                        &agent_name,
                        &request.agent,
                        &worktree.pane_id,
                        request.model.as_deref(),
                    ))
                    .await
                    .map_err(|_| Error::PrivateSecurity)?;
                    self.private_prompt(&agent_name, &request.prompt)
                        .await
                        .map_err(|_| Error::PrivateSecurity)?;
                    return Ok(());
                }
                self.start_agent(
                    &agent_name,
                    &request.agent,
                    &worktree.pane_id,
                    request.model.as_deref(),
                )
                .await?;
                self.submit_initial_prompt(&agent_name, &request.prompt)
                    .await?;
                Ok(())
            }
            .await;
            if let Err(error) = launch_result {
                if request.private_fork.is_some() {
                    provisional.cleanup().await;
                    return Err(Error::PrivateSecurity);
                }
                return Err(self.rollback_workspace(&worktree.workspace_id, error).await);
            }

            let now = Utc::now();
            let run_id = Uuid::new_v4().to_string();
            let summary = RunSummary {
                confidential: request.private_fork.is_some(),
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
                model: request.model,
                state: RunState::Running,
                message: None,
                session_id: Some(agent_name.clone()),
                started_at: now,
                updated_at: now,
            };
            let inserted = self
                .registry
                .insert(RunRecord {
                    summary: summary.clone(),
                    session: BackendSession::Herdr {
                        workspace_id: worktree.workspace_id,
                        pane_id: worktree.pane_id,
                        agent_name,
                    },
                    deletion: None,
                })
                .await;
            if let Err(error) = inserted {
                provisional.cleanup().await;
                return Err(error);
            }
            provisional.workspace = None;
            Ok(DispatchResult {
                run: summary,
                capabilities: self.capabilities(),
            })
        }
        .await;
        result.map_err(|error| {
            if private {
                Error::PrivateSecurity
            } else {
                error
            }
        })
    }

    async fn dispatch_away(
        &self,
        request: DispatchRequest,
        run_id: &str,
    ) -> Result<DispatchResult> {
        self.away_dispatch(request, run_id).await
    }

    async fn refresh_away(&self, run_id: &str) -> Result<StatusResult> {
        self.away_refresh(run_id).await
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let record = self.record(run_id).await?;
        if matches!(record.session, BackendSession::HerdrAway { .. }) {
            return self.away_refresh(run_id).await;
        }
        let private = record.summary.confidential;
        crate::private::redact_errors(private, async {
            let BackendSession::Herdr { agent_name, .. } = &record.session else {
                unreachable!()
            };
            let agent = match self
                .command(vec!["agent".into(), "get".into(), agent_name.into()])
                .await
            {
                Ok(value) => parse_agent(&value)?,
                // Runs recorded as completed before `done` meant a finished turn
                // are re-checked once; an agent closed since then stays completed.
                Err(_) if record.summary.state == RunState::Completed => {
                    return Ok(StatusResult {
                        run: record.summary,
                        output: None,
                    });
                },
                Err(error) => {
                    let error = error.for_private(private);
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
            let output = if private {
                None
            } else {
                let output =
                    run_output(&self.config.executable, &agent_read_args(agent_name), None).await?;
                (!output.is_empty()).then_some(output)
            };
            let mut summary = record.summary;
            if summary.state != RunState::Cancelled {
                summary.state = map_agent_status(&agent.status);
                summary.message = match agent.status.as_str() {
                "blocked" => Some(
                    "Herdr reports the agent is blocked; the required interaction is not exposed"
                        .into(),
                ),
                "unknown" => Some("Herdr cannot classify the agent state".into()),
                "done" => Some("Agent finished its turn; review it in Herdr".into()),
                _ => None,
            };
                summary.updated_at = Utc::now();
                self.registry.update_summary(summary.clone()).await?;
            }
            Ok(StatusResult {
                run: summary,
                output,
            })
        })
        .await
    }

    async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Err(Error::InvalidRequest("input cannot be empty".into()));
        }
        let record = self.record(run_id).await?;
        if matches!(record.session, BackendSession::HerdrAway { .. }) {
            return Err(Error::UnsupportedCapability {
                backend: self.kind(),
                capability: Capability::SendInput,
            });
        }
        crate::private::redact_errors(record.summary.confidential, async {
            let BackendSession::Herdr { agent_name, .. } = record.session else {
                unreachable!()
            };
            if record.summary.confidential {
                self.verify_private_transport().await?;
                self.private_prompt(&agent_name, text).await?;
            } else {
                self.command(agent_prompt_args(&agent_name, text)).await?;
            }
            self.registry
                .set_state(run_id, RunState::Running, None)
                .await?;
            Ok(())
        })
        .await
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        let record = self.record(run_id).await?;
        if matches!(record.session, BackendSession::HerdrAway { .. }) {
            return self.away_stop(run_id).await;
        }
        crate::private::redact_errors(record.summary.confidential, async {
            if record.summary.state == RunState::Cancelled {
                return Ok(());
            }
            if record.summary.confidential {
                let BackendSession::Herdr { workspace_id, .. } = &record.session else {
                    unreachable!()
                };
                self.command(vec![
                    "workspace".into(),
                    "close".into(),
                    "--workspace".into(),
                    workspace_id.into(),
                ])
                .await?;
                self.registry
                    .set_state(run_id, RunState::Cancelled, None)
                    .await?;
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
        })
        .await
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let record = self.record(run_id).await?;
        if let BackendSession::HerdrAway { workspace_id, .. } = &record.session {
            let workspace = workspace_id.as_ref().ok_or_else(|| {
                Error::Disconnected("Away workspace creation is unconfirmed".into())
            })?;
            self.command(vec!["workspace".into(), "focus".into(), workspace.into()])
                .await?;
            let mut uri = Url::parse("herdr://workspace/").unwrap();
            uri.path_segments_mut().unwrap().push(workspace);
            return Ok(OpenResult {
                uri,
                launched: true,
            });
        }
        crate::private::redact_errors(record.summary.confidential, async {
            let BackendSession::Herdr {
                agent_name,
                workspace_id,
                ..
            } = &record.session
            else {
                unreachable!()
            };
            // The agent may have exited, or its workspace been closed, since
            // dispatch: fall back to the workspace, then reopen the worktree.
            match self
                .command(vec![
                    "agent".into(),
                    "focus".into(),
                    agent_name.clone().into(),
                ])
                .await
            {
                Ok(_) => return opened(&format!("herdr://agent/{agent_name}")),
                Err(error) if !is_not_found(&error) => return Err(error),
                Err(_) => {},
            }
            match self
                .command(vec![
                    "workspace".into(),
                    "focus".into(),
                    workspace_id.clone().into(),
                ])
                .await
            {
                Ok(_) => return opened(&format!("herdr://workspace/{workspace_id}")),
                Err(error) if !is_not_found(&error) => return Err(error),
                Err(_) => {},
            }
            let worktree = record
                .summary
                .workspace
                .as_ref()
                .filter(|workspace| workspace.host.is_none())
                .and_then(|workspace| workspace.path.as_deref())
                .filter(|path| path.is_dir());
            let Some(path) = worktree else {
                return Err(Error::Disconnected(
                    "its Herdr agent, workspace and worktree are gone".into(),
                ));
            };
            let reopened = parse_worktree(&self.command(worktree_open_args(path)).await?)?;
            opened(&format!("herdr://workspace/{}", reopened.workspace_id))
        })
        .await
    }

    async fn delete_worktree(
        &self,
        run_id: &str,
        force: bool,
        _expected: Option<&WorktreeInspection>,
    ) -> Result<()> {
        let record = self.record(run_id).await?;
        let workspace_id = match record.session {
            BackendSession::Herdr { workspace_id, .. } => workspace_id,
            BackendSession::HerdrAway {
                workspace_id,
                pane_id,
                ..
            } => {
                let status = self.away_refresh(run_id).await?;
                if !away::terminal(status.run.state) {
                    return Err(Error::Disconnected(
                        "Cannot delete Away worktree before confirmed process exit".into(),
                    ));
                }
                let workspace = workspace_id.ok_or_else(|| {
                    Error::Disconnected("Away workspace creation is unconfirmed".into())
                })?;
                let pane = pane_id
                    .ok_or_else(|| Error::Disconnected("Away root pane is unconfirmed".into()))?;
                self.away_verify_idle_workspace(&workspace, &pane).await?;
                workspace
            },
            _ => unreachable!(),
        };
        self.registry.begin_deletion(run_id, force, None).await?;
        if record.summary.confidential {
            // Named private workspaces are not Herdr-managed Git worktrees. Closing
            // the pane deliberately retains the isolated checkout and private fork.
            if self
                .command(vec![
                    "workspace".into(),
                    "close".into(),
                    "--workspace".into(),
                    workspace_id.clone().into(),
                ])
                .await
                .is_err()
            {
                let _ = self.registry.cancel_deletion(run_id).await;
                return Err(Error::PrivateSecurity);
            }
            self.registry.complete_deletion(run_id).await?;
            return Ok(());
        }
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
    #[serde(default)]
    socket: Option<PathBuf>,
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

/// Accepts any client/server pair that Herdr reports as compatible on the same
/// protocol. A differing release (e.g. a CLI upgraded while the server keeps
/// running) is usable, so it is returned as a warning rather than an error.
fn validate_status(status: &HerdrStatus) -> Result<Option<String>> {
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
    if status.client.protocol != server_protocol
        || status.server.compatible != Some(true)
        || status.server.restart_needed
    {
        return Err(Error::InvalidResponse(format!(
            "Herdr client {} protocol {} is incompatible with server {server_version} protocol {server_protocol}; restart or update Herdr",
            status.client.version, status.client.protocol
        )));
    }
    Ok((status.client.version != server_version).then(|| {
        format!(
            "Herdr client {} differs from server {server_version} (both protocol {server_protocol}); restart the Herdr server to match",
            status.client.version
        )
    }))
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

fn opened(uri: &str) -> Result<OpenResult> {
    Ok(OpenResult {
        uri: Url::parse(uri).map_err(|error| Error::InvalidResponse(error.to_string()))?,
        launched: true,
    })
}

/// Herdr reports a missing agent or workspace as `[agent_not_found]` or
/// `[workspace_not_found]` after the message.
fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::CommandFailed { stderr, .. } if stderr.ends_with("_not_found]"))
}

/// Reopens an existing worktree checkout as a focused Herdr workspace.
fn worktree_open_args(path: &std::path::Path) -> Vec<OsString> {
    vec![
        "worktree".into(),
        "open".into(),
        "--cwd".into(),
        path.into(),
        "--path".into(),
        path.into(),
        "--focus".into(),
    ]
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
        // Herdr's `done` is a finished turn whose output is unseen, not the end of
        // the session: the agent can be prompted again, so keep tracking it.
        "idle" | "done" => RunState::Idle,
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

fn private_workspace_args(root: &std::path::Path) -> Vec<OsString> {
    // A named workspace cannot resolve back to a public primary worktree.
    let mut args = vec![
        "workspace".into(),
        "create".into(),
        "--cwd".into(),
        root.as_os_str().to_owned(),
        "--label".into(),
        crate::private::TITLE.into(),
        "--no-focus".into(),
    ];
    for value in [
        "GIT_CONFIG_GLOBAL=/dev/null",
        "GIT_CONFIG_SYSTEM=/dev/null",
        "GIT_CONFIG_NOSYSTEM=1",
        "GIT_CONFIG_COUNT=0",
        "GIT_CONFIG_PARAMETERS=",
        "GIT_TERMINAL_PROMPT=0",
        "GIT_LFS_SKIP_SMUDGE=1",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES=",
    ] {
        args.extend(["--env".into(), value.into()]);
    }
    for (key, path) in [
        ("GIT_DIR", root.join(".git")),
        ("GIT_COMMON_DIR", root.join(".git")),
        ("GIT_WORK_TREE", root.to_owned()),
        ("GIT_OBJECT_DIRECTORY", root.join(".git/objects")),
        ("GIT_INDEX_FILE", root.join(".git/index")),
    ] {
        args.extend(["--env".into(), format!("{key}={}", path.display()).into()]);
    }
    args
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

fn agent_start_args(name: &str, kind: &str, pane: &str, model: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        "agent".into(),
        "start".into(),
        name.into(),
        "--kind".into(),
        kind.into(),
        "--pane".into(),
        pane.into(),
    ];
    if let Some(model) = model {
        args.extend(["--".into(), "--model".into(), model.into()]);
    }
    args
}

#[cfg(test)]
#[test]
fn model_arguments_are_literal_harness_arguments() {
    for (kind, model) in [
        ("claude", "sonnet"),
        ("opencode", "openai/gpt-5.4"),
        ("pi", "openai/gpt-5.4"),
        ("pi", "provider/model; $(touch /tmp/not-executed)"),
    ] {
        assert_eq!(
            agent_start_args("name", kind, "pane", Some(model)),
            [
                "agent", "start", "name", "--kind", kind, "--pane", "pane", "--", "--model", model
            ]
            .map(OsString::from)
        );
    }
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

    #[test]
    fn private_cleanup_discarded_at_runtime_shutdown_does_not_recurse() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        drop(ProvisionalWorkspace {
            executable: "never-run-fixture".into(),
            workspace: Some("w1".into()),
        });
        drop(entered);
        // The cleanup task has never been polled. Dropping it must not construct
        // another guard that schedules yet another task into the closed runtime.
        drop(runtime);
    }

    #[test]
    fn private_preflight_allows_only_audited_version_protocol_and_socket_pairs() {
        let path = Path::new("/fixture/herdr.sock");
        for (version, protocol, accepted) in [
            ("0.8.2", 20, true),
            ("0.9.0", 22, true),
            ("0.8.2", 22, false),
            ("0.9.0", 20, false),
            ("0.8.3", 20, false),
            ("0.9.1", 22, false),
            ("0.9.0-preview.1", 22, false),
            ("1.0.0", 22, false),
        ] {
            let mut status: HerdrStatus = serde_json::from_value(json!({
                "client": {"version": version, "protocol": protocol},
                "server": {"running": true, "version": version, "protocol": protocol,
                    "compatible": true, "restart_needed": false, "socket": path}
            }))
            .unwrap();
            assert_eq!(
                validate_private_status(&status, path).is_ok(),
                accepted,
                "{version}/{protocol}"
            );
            status.server.socket = Some("/different/herdr.sock".into());
            assert!(validate_private_status(&status, path).is_err());
            status.server.socket = Some(path.into());
            status.server.version = Some("unmatched".into());
            assert!(validate_private_status(&status, path).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_preflight_uses_configured_backend_executable_without_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestRepository::new();
        let executable = temp.0.join("configured-cli");
        let marker = temp.0.join("called");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{{\"client\":{{\"version\":\"unsupported\",\"protocol\":22}},\"server\":{{\"running\":false,\"restart_needed\":false}}}}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = SessionRegistry::load(Some(temp.0.join("registry.json")))
            .await
            .unwrap();
        let runner =
            crate::Runner::new([
                Arc::new(HerdrBackend::new(HerdrConfig { executable }, registry))
                    as Arc<dyn Backend>,
            ]);
        assert!(matches!(
            verify_private_herdr_transport(&runner).await,
            Err(Error::PrivateSecurity)
        ));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "status\n--json\n");
        assert!(matches!(
            verify_private_herdr_transport(&crate::Runner::new([])).await,
            Err(Error::PrivateSecurity)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_falls_back_from_a_gone_agent_to_its_workspace_then_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestRepository::new();
        let executable = temp.0.join("cli");
        let calls = temp.0.join("calls");
        let worktree = temp.0.join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        // `workspace` names what is still open: "" (nothing), "w1" or "agent".
        let state = temp.0.join("state");
        std::fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{calls}'
open=$(cat '{state}')
case "$1 $2" in
'agent focus')
  [ "$open" = agent ] && {{ printf '{{}}'; exit 0; }}
  printf '%s' '{{"error":{{"code":"agent_not_found","message":"agent target x not found"}}}}' >&2; exit 1;;
'workspace focus')
  [ "$open" = w1 ] && {{ printf '{{}}'; exit 0; }}
  printf '%s' '{{"error":{{"code":"workspace_not_found","message":"workspace w1 not found"}}}}' >&2; exit 1;;
'worktree open')
  printf '%s' '{{"result":{{"workspace":{{"workspace_id":"w9"}},"root_pane":{{"pane_id":"w9:p1"}}}}}}';;
esac
"#,
                calls = calls.display(),
                state = state.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = SessionRegistry::load(Some(temp.0.join("registry/sessions.json")))
            .await
            .unwrap();
        let now = Utc::now();
        registry
            .insert(RunRecord {
                summary: RunSummary {
                    confidential: false,
                    id: "run".into(),
                    issue_key: "issue".into(),
                    workspace: Some(agent_launcher_core::WorkspaceRef {
                        backend: BackendKind::Herdr,
                        id: "w1".into(),
                        host: None,
                        path: Some(worktree.clone()),
                        branch: "agent/fix".into(),
                    }),
                    agent: "claude".into(),
                    model: None,
                    state: RunState::Disconnected,
                    message: None,
                    session_id: None,
                    started_at: now,
                    updated_at: now,
                },
                session: BackendSession::Herdr {
                    workspace_id: "w1".into(),
                    pane_id: "w1:p1".into(),
                    agent_name: "launcher-fixture".into(),
                },
                deletion: None,
            })
            .await
            .unwrap();
        let backend = HerdrBackend::new(HerdrConfig { executable }, registry);
        let open = |what: &str| {
            std::fs::write(&state, what).unwrap();
            let _ = std::fs::remove_file(&calls);
            async { backend.open("run").await }
        };

        let result = open("agent").await.unwrap();
        assert_eq!(result.uri.as_str(), "herdr://agent/launcher-fixture");
        let result = open("w1").await.unwrap();
        assert_eq!(result.uri.as_str(), "herdr://workspace/w1");
        let result = open("").await.unwrap();
        assert_eq!(result.uri.as_str(), "herdr://workspace/w9");
        let log = std::fs::read_to_string(&calls).unwrap();
        assert!(
            log.ends_with(&format!(
                "worktree open --cwd {path} --path {path} --focus\n",
                path = worktree.display()
            )),
            "{log}"
        );

        std::fs::remove_dir(&worktree).unwrap();
        assert!(matches!(
            open("").await,
            Err(Error::Disconnected(message)) if message.contains("gone")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_refresh_never_reads_transcript_stdout() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestRepository::new();
        let executable = temp.0.join("cli");
        let marker = temp.0.join("calls");
        std::fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1 $2" = 'agent get' ]; then
printf '%s' '{{"result":{{"agent":{{"agent_status":"working"}}}}}}'
else
printf '%s' PRIVATE_TRANSCRIPT_SENTINEL
exit 1
fi
"#,
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = SessionRegistry::load(Some(temp.0.join("registry/sessions.json")))
            .await
            .unwrap();
        let now = Utc::now();
        registry
            .insert(RunRecord {
                summary: RunSummary {
                    confidential: true,
                    id: "private".into(),
                    issue_key: "private".into(),
                    workspace: None,
                    agent: "opencode".into(),
                    model: None,
                    state: RunState::Running,
                    message: None,
                    session_id: Some("launcher-fixture".into()),
                    started_at: now,
                    updated_at: now,
                },
                session: BackendSession::Herdr {
                    workspace_id: "w1".into(),
                    pane_id: "w1:p1".into(),
                    agent_name: "launcher-fixture".into(),
                },
                deletion: None,
            })
            .await
            .unwrap();
        let backend = HerdrBackend::new(HerdrConfig { executable }, registry);
        let status = backend.refresh("private").await.unwrap();
        assert_eq!(status.run.state, RunState::Running);
        assert!(status.output.is_none());
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "agent get launcher-fixture\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provisional_workspace_drop_and_explicit_rollback_close_workspace() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestRepository::new();
        let executable = temp.0.join("cli");
        let marker = temp.0.join("closed");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut guard = ProvisionalWorkspace {
            executable: executable.clone(),
            workspace: Some("w1".into()),
        };
        guard.cleanup().await;
        assert!(guard.workspace.is_none());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "workspace\nclose\n--workspace\nw1\n"
        );
        std::fs::remove_file(&marker).unwrap();
        drop(ProvisionalWorkspace {
            executable,
            workspace: Some("w2".into()),
        });
        for _ in 0..200 {
            if std::fs::read_to_string(&marker).is_ok_and(|text| text.ends_with("w2\n")) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("drop cleanup did not close provisional workspace");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_socket_fixture_transports_sentinel_and_rejects_unsafe_responses() {
        use std::os::unix::fs::PermissionsExt;

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let temp = TestRepository(PathBuf::from("/tmp").join(format!("ipc-{}", Uuid::new_v4())));
        std::fs::create_dir(&temp.0).unwrap();
        let socket = temp.0.join("ipc");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = tokio::spawn(async move {
            for bad in [false, true] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "agent.prompt");
                assert_eq!(request["params"]["target"], "launcher-fixture");
                assert_eq!(
                    request["params"]["text"],
                    "PRIVATE_IPC_SENTINEL\nsecond line"
                );
                let response = if bad {
                    json!({"id": request["id"], "error": {"message": "PRIVATE_IPC_SENTINEL"}})
                } else {
                    json!({"id": request["id"], "result": {"type": "agent_prompted"}})
                };
                reader
                    .get_mut()
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let params =
            json!({"target": "launcher-fixture", "text": "PRIVATE_IPC_SENTINEL\nsecond line"});
        private_ipc(&socket, "agent.prompt", params.clone())
            .await
            .unwrap();
        let error = private_ipc(&socket, "agent.prompt", params.clone())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SENTINEL"));
        server.await.unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            private_ipc(&socket, "agent.prompt", params.clone())
                .await
                .is_err()
        );
        let link = temp.0.join("link");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        assert!(
            private_ipc(&link, "agent.prompt", params.clone())
                .await
                .is_err()
        );
        let regular = temp.0.join("regular");
        std::fs::write(&regular, "").unwrap();
        assert!(private_ipc(&regular, "agent.prompt", params).await.is_err());
        for args in [
            agent_start_args("launcher-fixture", "opencode", "w1:p1", None),
            private_workspace_args(&temp.0),
        ] {
            assert!(!format!("{args:?}").contains("SENTINEL"));
        }
    }

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
    fn private_workspace_uses_exact_clone_without_worktree_resolution() {
        let args = private_workspace_args(Path::new("/private/opaque/clone"));
        assert_eq!(
            &args[..6],
            &[
                "workspace",
                "create",
                "--cwd",
                "/private/opaque/clone",
                "--label",
                crate::private::TITLE
            ]
            .map(OsString::from)
        );
        assert!(!args.iter().any(|arg| arg == "worktree" || arg == "--base"));
        assert!(args.iter().any(|arg| arg == "GIT_CONFIG_GLOBAL=/dev/null"));
        assert!(
            args.iter()
                .any(|arg| arg == "GIT_DIR=/private/opaque/clone/.git")
        );
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
            agent_start_args("launcher-123", "opencode", "w1:p1", None),
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
        assert_eq!(map_agent_status("done"), RunState::Idle);
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
        assert_eq!(
            validate_status(&valid).expect("matching status should validate"),
            None
        );

        let skewed: HerdrStatus = serde_json::from_value(json!({
            "client": {"version": "0.9.1", "protocol": 22},
            "server": {
                "running": true,
                "version": "0.9.0",
                "protocol": 22,
                "compatible": true,
                "restart_needed": false
            }
        }))
        .expect("status should deserialize");
        let warning = validate_status(&skewed)
            .expect("compatible version skew should validate")
            .expect("version skew should warn");
        assert!(warning.contains("0.9.1") && warning.contains("0.9.0"));
        assert!(validate_private_status(&skewed, std::path::Path::new("/x")).is_err());

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
