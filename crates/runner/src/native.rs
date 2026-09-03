use std::{
    ffi::OsString,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use agent_launcher_core::{
    BackendKind, Repository, RunState, RunSummary, WakeConfig, WorkspaceRef,
};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::{Instant, sleep},
};
use url::Url;
use uuid::Uuid;

use crate::{
    Backend, BackendCapabilities, BackendDetection, Capability, DispatchRequest, DispatchResult,
    Error, OpenResult, Result, SessionRegistry, StatusResult,
    command::{find_string, open_uri, run_output, run_output_with_timeout, shell_quote},
    compute::wake,
    registry::{BackendSession, ManagedChild, RunRecord},
    sanitize_branch, sanitize_workspace_name,
};

#[derive(Clone, Debug)]
pub struct NativeSshConfig {
    pub destination: String,
    pub workspace_root: PathBuf,
    pub wake: Option<WakeConfig>,
}

#[derive(Clone, Debug)]
pub struct NativeConfig {
    pub git_executable: PathBuf,
    pub opencode_executable: PathBuf,
    pub ssh_executable: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub ssh: Option<NativeSshConfig>,
    pub startup_timeout: Duration,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            git_executable: "git".into(),
            opencode_executable: "opencode".into(),
            ssh_executable: "ssh".into(),
            workspace_root: None,
            ssh: None,
            startup_timeout: Duration::from_secs(15),
        }
    }
}

pub struct NativeBackend {
    config: NativeConfig,
    registry: Arc<SessionRegistry>,
    client: Client,
    ssh_readiness_lock: Mutex<()>,
}

impl NativeBackend {
    pub fn new(config: NativeConfig, registry: Arc<SessionRegistry>) -> Self {
        Self {
            config,
            registry,
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("static OpenCode HTTP client configuration is valid"),
            ssh_readiness_lock: Mutex::new(()),
        }
    }

    fn local_workspace_root(&self) -> Result<PathBuf> {
        self.config.workspace_root.clone().map_or_else(
            || {
                dirs::data_local_dir()
                    .ok_or(Error::DataDirectoryUnavailable)
                    .map(|root| root.join("agent-launcher").join("workspaces"))
            },
            Ok,
        )
    }

    async fn record(&self, run_id: &str) -> Result<RunRecord> {
        let record = self.registry.get(run_id).await?;
        if !matches!(record.session, BackendSession::Native { .. }) {
            return Err(Error::RunNotFound(run_id.to_string()));
        }
        Ok(record)
    }

    async fn provision_local(
        &self,
        repository: &Repository,
        workspace_path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<()> {
        let _guard = self.registry.provision_lock.lock().await;
        let list_args = os_args([
            "-C",
            &repository.root.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ]);
        let worktrees = run_output(&self.config.git_executable, &list_args, None).await?;
        if let Some(existing_branch) = worktree_branch(&worktrees, workspace_path) {
            let expected = format!("refs/heads/{branch}");
            if existing_branch == expected {
                return Ok(());
            }
            return Err(Error::InvalidRequest(format!(
                "existing worktree {} is on {existing_branch}, expected {expected}",
                workspace_path.display()
            )));
        }
        if tokio::fs::try_exists(workspace_path).await? {
            return Err(Error::InvalidRequest(format!(
                "workspace path exists but is not a registered worktree: {}",
                workspace_path.display()
            )));
        }
        let parent = workspace_path
            .parent()
            .ok_or_else(|| Error::InvalidRequest("workspace path has no parent".into()))?;
        tokio::fs::create_dir_all(parent).await?;

        let verify_args = os_args([
            "-C",
            &repository.root.to_string_lossy(),
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ]);
        let mut verify = Command::new(&self.config.git_executable);
        verify.args(&verify_args).kill_on_drop(true);
        let branch_exists = tokio::time::timeout(Duration::from_secs(30), verify.status())
            .await
            .map_err(|_| Error::CommandTimedOut {
                program: "git show-ref".into(),
                timeout: Duration::from_secs(30),
            })?
            .map_err(|error| map_executable_error(&self.config.git_executable, error))?
            .success();
        let args = worktree_add_args(
            &repository.root,
            workspace_path,
            branch,
            base,
            branch_exists,
        );
        run_output(&self.config.git_executable, &args, None).await?;
        Ok(())
    }

    async fn provision_remote(
        &self,
        ssh: &NativeSshConfig,
        repository: &Repository,
        workspace_path: &str,
        branch: &str,
        base: &str,
        repository_hash: &str,
    ) -> Result<()> {
        let remote = repository.remote.as_ref().ok_or_else(|| {
            Error::InvalidRequest("SSH dispatch requires a repository remote URL".into())
        })?;
        let root = remote_path_expression(&ssh.workspace_root);
        let repository_parent = format!("{root}/{}", shell_quote("repositories"));
        let workspace_parent = Path::new(workspace_path)
            .parent()
            .ok_or_else(|| Error::InvalidRequest("remote workspace path has no parent".into()))?;
        let workspace_parent = remote_path_expression(workspace_parent);
        let bare = format!(
            "{repository_parent}/{}",
            shell_quote(&format!("{repository_hash}.git"))
        );
        let workspace = remote_path_expression(Path::new(workspace_path));
        let branch_ref = format!("refs/heads/{branch}");
        let fresh_base = if base == "HEAD" {
            "refs/remotes/origin/HEAD".to_string()
        } else if base.starts_with("refs/") {
            base.to_string()
        } else {
            format!(
                "refs/remotes/origin/{}",
                base.strip_prefix("origin/").unwrap_or(base)
            )
        };
        let script = format!(
            "mkdir -p {repository_parent} {workspace_parent} && \
             if [ ! -d {bare} ]; then git clone --bare -- {remote} {bare}; fi && \
             git --git-dir {bare} fetch --prune origin \
             '+refs/heads/*:refs/remotes/origin/*' '+HEAD:refs/remotes/origin/HEAD' && \
             base_ref={base}; \
             if git --git-dir {bare} show-ref --verify --quiet {fresh_base}; \
             then base_ref={fresh_base}; fi && \
             if [ -e {workspace}/.git ]; then \
             test \"$(git -C {workspace} symbolic-ref --short HEAD)\" = {branch}; \
             else \
             if git --git-dir {bare} show-ref --verify --quiet {branch_ref}; then \
             git --git-dir {bare} worktree add -- {workspace} {branch}; \
             else git --git-dir {bare} worktree add -b {branch} -- {workspace} \"$base_ref\"; fi; fi",
            remote = shell_quote(&remote.url),
            branch_ref = shell_quote(&branch_ref),
            branch = shell_quote(branch),
            base = shell_quote(base),
            fresh_base = shell_quote(&fresh_base),
        );
        let args = ssh_command_args(&ssh.destination, &script, None);
        run_output(&self.config.ssh_executable, &args, None)
            .await
            .map_err(|error| redact_remote_provision_error(error, &remote.url))?;
        Ok(())
    }

    async fn launch_server(
        &self,
        workspace_path: &Path,
        remote_workspace_path: Option<&str>,
        remote_port: u16,
    ) -> Result<(Url, Child)> {
        let local_port = available_port()?;
        let mut command = if let Some(ssh) = &self.config.ssh {
            let remote_workspace = remote_workspace_path
                .ok_or_else(|| Error::InvalidRequest("remote workspace path is missing".into()))?;
            let script = format!(
                "cd {} && exec opencode serve --hostname 127.0.0.1 --port {}",
                remote_path_expression(Path::new(remote_workspace)),
                remote_port
            );
            let forwarding = format!("{local_port}:127.0.0.1:{remote_port}");
            let mut command = Command::new(&self.config.ssh_executable);
            command.args(ssh_command_args(
                &ssh.destination,
                &script,
                Some(&forwarding),
            ));
            command
        } else {
            let mut command = Command::new(&self.config.opencode_executable);
            command
                .arg("serve")
                .arg("--hostname")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(local_port.to_string())
                .current_dir(workspace_path);
            command
        };
        // OpenCode permissions remain interactive/default; notably, --auto is never passed.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            let executable = if self.config.ssh.is_some() {
                &self.config.ssh_executable
            } else {
                &self.config.opencode_executable
            };
            map_executable_error(executable, error)
        })?;
        let base_url = Url::parse(&format!("http://127.0.0.1:{local_port}/"))
            .map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if let Err(error) = self.wait_for_health(&base_url, &mut child).await {
            let _ = child.kill().await;
            return Err(error);
        }
        Ok((base_url, child))
    }

    async fn wait_for_health(&self, base_url: &Url, child: &mut Child) -> Result<()> {
        let deadline = Instant::now() + self.config.startup_timeout;
        let health_url = endpoint(base_url, &["global", "health"])?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Disconnected(format!(
                    "OpenCode server exited during startup with {status}"
                )));
            }
            if self
                .client
                .get(health_url.clone())
                .timeout(Duration::from_secs(1))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::Disconnected(
                    "timed out waiting for the OpenCode server".into(),
                ));
            }
            sleep(Duration::from_millis(150)).await;
        }
    }

    async fn request_json(&self, response: reqwest::Result<Response>) -> Result<Value> {
        let response = response?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        Ok(response.json().await?)
    }

    async fn request_empty(&self, response: reqwest::Result<Response>) -> Result<()> {
        let response = response?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        Ok(())
    }

    async fn health(&self, base_url: &Url) -> bool {
        let Ok(url) = endpoint(base_url, &["global", "health"]) else {
            return false;
        };
        self.client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    async fn pending_permission(
        &self,
        base_url: &Url,
        session_id: &str,
    ) -> Option<PendingPermission> {
        let Ok(url) = endpoint(base_url, &["permission"]) else {
            return None;
        };
        let Ok(response) = self
            .client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        else {
            return None;
        };
        if !response.status().is_success() {
            return None;
        }
        let Ok(value) = response.json::<Value>().await else {
            return None;
        };
        let permissions = value
            .as_array()
            .map(|values| values.iter().collect::<Vec<_>>())
            .or_else(|| {
                value
                    .as_object()
                    .map(|values| values.values().collect::<Vec<_>>())
            });
        permissions.and_then(|permissions| {
            permissions.into_iter().find_map(|permission| {
                (find_string(permission, &["sessionID", "sessionId"]).as_deref()
                    == Some(session_id))
                .then(|| {
                    find_string(permission, &["id", "requestID", "requestId"]).map(|id| {
                        PendingPermission {
                            id,
                            permission: find_string(permission, &["permission"]),
                        }
                    })
                })
                .flatten()
            })
        })
    }

    async fn pending_question(&self, base_url: &Url, session_id: &str) -> Option<PendingQuestion> {
        let Ok(url) = endpoint(base_url, &["question"]) else {
            return None;
        };
        let Ok(response) = self
            .client
            .get(url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        else {
            return None;
        };
        if !response.status().is_success() {
            return None;
        }
        let Ok(value) = response.json::<Value>().await else {
            return None;
        };
        pending_question(&value, session_id)
    }

    async fn ssh_ready(&self, ssh: &NativeSshConfig) -> Result<()> {
        let script = "command -v git >/dev/null && command -v opencode >/dev/null";
        run_output_with_timeout(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, script, None),
            None,
            Duration::from_secs(3),
        )
        .await
        .map(|_| ())
    }

    async fn ensure_ssh_ready(&self, ssh: &NativeSshConfig) -> Result<()> {
        let _guard = self.ssh_readiness_lock.lock().await;
        let initial_error = match self.ssh_ready(ssh).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(wake_config) = &ssh.wake else {
            return Err(initial_error);
        };
        wake(wake_config).await?;
        let deadline = Instant::now() + self.config.startup_timeout;
        loop {
            match self.ssh_ready(ssh).await {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

struct PendingPermission {
    id: String,
    permission: Option<String>,
}

struct PendingQuestion {
    id: String,
    count: usize,
    prompt: String,
}

#[async_trait]
impl Backend for NativeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Native
    }

    fn capabilities(&self) -> BackendCapabilities {
        let mut capabilities = vec![
            Capability::Detect,
            Capability::Dispatch,
            Capability::Refresh,
            Capability::SendInput,
            Capability::Stop,
            Capability::Open,
        ];
        if self.config.ssh.is_some() {
            capabilities.push(Capability::Remote);
        }
        BackendCapabilities::new(capabilities)
    }

    async fn owns_run(&self, run_id: &str) -> bool {
        self.registry
            .get(run_id)
            .await
            .is_ok_and(|record| matches!(record.session, BackendSession::Native { .. }))
    }

    async fn detect(&self, _repository: &Repository) -> Result<BackendDetection> {
        let result = if let Some(ssh) = &self.config.ssh {
            self.ensure_ssh_ready(ssh).await
        } else {
            match run_output(&self.config.git_executable, &os_args(["--version"]), None).await {
                Ok(_) => run_output(
                    &self.config.opencode_executable,
                    &os_args(["--version"]),
                    None,
                )
                .await
                .map(|_| ()),
                Err(error) => Err(error),
            }
        };
        Ok(BackendDetection {
            backend: self.kind(),
            available: result.is_ok(),
            manager_running: false,
            capabilities: self.capabilities(),
            message: result.err().map(|error| error.to_string()),
        })
    }

    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult> {
        request.validate()?;
        let issue_hash = stable_hash(&request.issue.key.canonical());
        let repository_identity = request.repository.remote.as_ref().map_or_else(
            || request.repository.root.to_string_lossy().into_owned(),
            |remote| remote.url.clone(),
        );
        let repository_hash = stable_hash(&repository_identity);
        let name = request.workspace_name.clone().unwrap_or_else(|| {
            sanitize_workspace_name(&format!(
                "{}-{}",
                request.issue.identifier, request.issue.title
            ))
        });
        let branch = sanitize_branch(
            request
                .branch
                .as_deref()
                .unwrap_or(&format!("agent/{name}-{}", &issue_hash[..8])),
        );
        let base = request.base_branch.as_deref().unwrap_or("HEAD");
        let repository_segment = format!(
            "{}-{}",
            sanitize_workspace_name(&request.repository.display_name()),
            &repository_hash[..8]
        );
        let issue_segment = sanitize_workspace_name(&request.issue.identifier);
        let leaf = format!("{issue_segment}-{}", &issue_hash[..16]);

        let (workspace_path, remote_path) = if let Some(ssh) = &self.config.ssh {
            self.ensure_ssh_ready(ssh).await?;
            let path = ssh
                .workspace_root
                .join("workspaces")
                .join(&repository_segment)
                .join(&leaf);
            let path_string = path.to_string_lossy().into_owned();
            self.provision_remote(
                ssh,
                &request.repository,
                &path_string,
                &branch,
                base,
                &repository_hash[..16],
            )
            .await?;
            (path, Some(path_string))
        } else {
            let path = self
                .local_workspace_root()?
                .join(repository_segment)
                .join(&leaf);
            self.provision_local(&request.repository, &path, &branch, base)
                .await?;
            (path, None)
        };

        let run_id = Uuid::new_v4().to_string();
        let port_hash = stable_hash(&run_id);
        let remote_port = 30_000 + u16::from_str_radix(&port_hash[..4], 16).unwrap_or(0) % 20_000;
        let (base_url, mut child) = self
            .launch_server(&workspace_path, remote_path.as_deref(), remote_port)
            .await?;

        let session_url = endpoint(&base_url, &["session"])?;
        let session = match self
            .request_json(
                self.client
                    .post(session_url)
                    .json(&json!({"title": format!("{}: {}", request.issue.identifier, request.issue.title)}))
                    .send()
                    .await,
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                let _ = child.kill().await;
                return Err(error);
            }
        };
        let session_id = match find_string(&session, &["id", "sessionID", "sessionId"]) {
            Some(session_id) => session_id,
            None => {
                let _ = child.kill().await;
                return Err(Error::InvalidResponse("session response has no id".into()));
            },
        };
        let prompt_url = endpoint(&base_url, &["session", &session_id, "prompt_async"])?;
        let body = prompt_body(&request.agent, &request.prompt, request.model.as_deref());
        if let Err(error) = self
            .request_empty(self.client.post(prompt_url).json(&body).send().await)
            .await
        {
            let _ = child.kill().await;
            return Err(error);
        }

        let now = Utc::now();
        let summary = RunSummary {
            id: run_id.clone(),
            issue_key: request.issue.key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Native,
                id: issue_hash.clone(),
                host: self.config.ssh.as_ref().map(|ssh| ssh.destination.clone()),
                path: Some(workspace_path),
                branch,
            }),
            agent: request.agent,
            state: RunState::Running,
            message: None,
            session_id: Some(session_id),
            started_at: now,
            updated_at: now,
        };
        if let Err(error) = self
            .registry
            .insert(RunRecord {
                summary: summary.clone(),
                session: BackendSession::Native {
                    base_url: base_url.to_string(),
                    remote: self.config.ssh.is_some(),
                    pending_permission_id: None,
                    pending_question_id: None,
                    pending_question_count: 0,
                    pending_question_prompt: None,
                    last_message_id: None,
                },
            })
            .await
        {
            let _ = child.kill().await;
            return Err(error);
        }
        self.registry
            .children
            .lock()
            .await
            .insert(run_id, ManagedChild { child });
        Ok(DispatchResult {
            run: summary,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Native { base_url, .. } = &record.session else {
            unreachable!()
        };
        let base_url =
            Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;

        let child_exited = {
            let mut children = self.registry.children.lock().await;
            let exited = if let Some(child) = children.get_mut(run_id) {
                child.child.try_wait()?.is_some()
            } else {
                false
            };
            if exited {
                children.remove(run_id);
            }
            exited
        };
        if child_exited || !self.health(&base_url).await {
            let summary = self
                .registry
                .set_state(
                    run_id,
                    RunState::Disconnected,
                    Some("OpenCode server or SSH tunnel is unavailable".into()),
                )
                .await?;
            return Ok(StatusResult {
                run: summary,
                output: None,
            });
        }

        let session_id = record
            .summary
            .session_id
            .as_deref()
            .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
        let status_url = endpoint(&base_url, &["session", "status"])?;
        let statuses = self
            .request_json(self.client.get(status_url).send().await)
            .await?;
        let status = statuses
            .get(session_id)
            .or_else(|| statuses.get("data").and_then(|data| data.get(session_id)));
        let status_type = status
            .and_then(|status| find_string(status, &["type", "status"]))
            .unwrap_or_else(|| "idle".into());
        let mut state = match status_type.as_str() {
            "busy" | "running" | "retry" => RunState::Running,
            "idle" => RunState::Idle,
            "error" | "failed" => RunState::Failed,
            _ => RunState::Running,
        };
        let pending_permission = self.pending_permission(&base_url, session_id).await;
        let pending_question = self.pending_question(&base_url, session_id).await;
        if pending_permission.is_some() || pending_question.is_some() {
            state = RunState::NeedsInput;
        }

        let messages_url = endpoint(&base_url, &["session", session_id, "message"])?;
        let latest_message = match self
            .client
            .get(messages_url)
            .query(&[("limit", "1")])
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| latest_message(&value)),
            _ => None,
        };
        let mut record = record;
        let BackendSession::Native {
            pending_permission_id,
            pending_question_id,
            pending_question_count,
            pending_question_prompt,
            last_message_id,
            ..
        } = &mut record.session
        else {
            unreachable!()
        };
        *pending_permission_id = pending_permission
            .as_ref()
            .map(|permission| permission.id.clone());
        *pending_question_id = pending_question
            .as_ref()
            .map(|question| question.id.clone());
        *pending_question_count = pending_question
            .as_ref()
            .map_or(0, |question| question.count);
        *pending_question_prompt = pending_question
            .as_ref()
            .map(|question| question.prompt.clone());
        let output = latest_message.and_then(|(id, text)| {
            if last_message_id.as_deref() == Some(id.as_str()) {
                None
            } else {
                *last_message_id = Some(id);
                Some(text)
            }
        });
        let summary = &mut record.summary;
        if summary.state != RunState::Cancelled {
            summary.state = state;
        }
        summary.message = pending_permission.map(|pending| {
            let permission = pending.permission.unwrap_or_else(|| "unknown".into());
            format!(
                "OpenCode permission {} is pending ({permission}); reply with once, always, or reject",
                pending.id
            )
        });
        if summary.message.is_none() {
            summary.message = pending_question.map(|pending| pending.prompt);
        }
        if summary.message.is_none() {
            summary.message = status.and_then(|status| find_string(status, &["message"]));
        }
        summary.updated_at = Utc::now();
        let summary = summary.clone();
        self.registry.update(record).await?;
        Ok(StatusResult {
            run: summary,
            output,
        })
    }

    async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Err(Error::InvalidRequest("input cannot be empty".into()));
        }
        let record = self.record(run_id).await?;
        let BackendSession::Native {
            base_url,
            pending_permission_id,
            pending_question_id,
            pending_question_count,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let base_url =
            Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if !self.health(&base_url).await {
            return Err(Error::Disconnected(run_id.into()));
        }
        if let Some(permission_id) = pending_permission_id {
            let reply = permission_reply(text)?;
            let url = endpoint(&base_url, &["permission", permission_id, "reply"])?;
            self.request_json(
                self.client
                    .post(url)
                    .json(&json!({"reply": reply}))
                    .send()
                    .await,
            )
            .await?;
            let mut record = record;
            let BackendSession::Native {
                pending_permission_id,
                pending_question_id,
                pending_question_prompt,
                ..
            } = &mut record.session
            else {
                unreachable!()
            };
            *pending_permission_id = None;
            record.summary.state = if pending_question_id.is_some() {
                RunState::NeedsInput
            } else {
                RunState::Running
            };
            record.summary.message = pending_question_prompt.clone();
            record.summary.updated_at = Utc::now();
            self.registry.update(record).await?;
            return Ok(());
        }
        if let Some(question_id) = pending_question_id {
            let action = question_action(text, *pending_question_count);
            let url = match &action {
                QuestionAction::Reply(_) => {
                    endpoint(&base_url, &["question", question_id, "reply"])?
                },
                QuestionAction::Reject => {
                    endpoint(&base_url, &["question", question_id, "reject"])?
                },
            };
            let request = self.client.post(url);
            let response = match action {
                QuestionAction::Reply(answers) => {
                    request.json(&json!({"answers": answers})).send().await
                },
                QuestionAction::Reject => request.send().await,
            };
            self.request_json(response).await?;
            let mut record = record;
            let BackendSession::Native {
                pending_question_id,
                pending_question_count,
                pending_question_prompt,
                ..
            } = &mut record.session
            else {
                unreachable!()
            };
            *pending_question_id = None;
            *pending_question_count = 0;
            *pending_question_prompt = None;
            record.summary.state = RunState::Running;
            record.summary.message = None;
            record.summary.updated_at = Utc::now();
            self.registry.update(record).await?;
            return Ok(());
        }
        let session_id = record
            .summary
            .session_id
            .as_deref()
            .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
        let url = endpoint(&base_url, &["session", session_id, "prompt_async"])?;
        self.request_empty(
            self.client
                .post(url)
                .json(&prompt_body(&record.summary.agent, text, None))
                .send()
                .await,
        )
        .await?;
        self.registry
            .set_state(run_id, RunState::Running, None)
            .await?;
        Ok(())
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        let record = self.record(run_id).await?;
        let BackendSession::Native { base_url, .. } = &record.session else {
            unreachable!()
        };
        let base_url =
            Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        let abort_error = if self.health(&base_url).await {
            let session_id = record
                .summary
                .session_id
                .as_deref()
                .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
            let url = endpoint(&base_url, &["session", session_id, "abort"])?;
            self.request_empty(self.client.post(url).send().await)
                .await
                .err()
        } else {
            Some(Error::Disconnected(run_id.into()))
        };
        let abort_failed = abort_error.is_some();
        let managed_child = self.registry.children.lock().await.remove(run_id);
        if let Some(mut managed_child) = managed_child {
            managed_child.child.kill().await?;
        } else if let Some(error) = abort_error {
            return Err(error);
        }
        let message = if abort_failed {
            "managed OpenCode server or SSH process terminated; session abort was unavailable"
        } else {
            "OpenCode session aborted and managed server terminated"
        };
        self.registry
            .set_state(run_id, RunState::Cancelled, Some(message.into()))
            .await?;
        Ok(())
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Native { base_url, .. } = record.session else {
            unreachable!()
        };
        let uri =
            Url::parse(&base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if !self.health(&uri).await {
            return Err(Error::Disconnected(run_id.into()));
        }
        open_uri(&uri, self.kind()).await?;
        Ok(OpenResult {
            uri,
            launched: true,
        })
    }
}

fn stable_hash(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes())
        .simple()
        .to_string()
}

fn parse_model(model: &str) -> Option<(&str, &str)> {
    let (provider, model) = model.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

fn prompt_body(agent: &str, text: &str, model: Option<&str>) -> Value {
    let mut body = json!({
        "parts": [{"type": "text", "text": text}],
    });
    if agent != "opencode" {
        body["agent"] = Value::String(agent.into());
    }
    if let Some(model) = model.and_then(parse_model) {
        body["model"] = json!({"providerID": model.0, "modelID": model.1});
    }
    body
}

fn available_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    Ok(listener.local_addr()?.port())
}

fn endpoint(base: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| Error::InvalidResponse("base URL cannot contain path segments".into()))?;
        path.clear();
        path.extend(segments);
    }
    Ok(url)
}

fn worktree_branch(output: &str, expected: &Path) -> Option<String> {
    output.split("\n\n").find_map(|entry| {
        let mut path_matches = false;
        let mut branch = None;
        for line in entry.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                path_matches = Path::new(path) == expected;
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_string());
            }
        }
        path_matches.then_some(branch).flatten()
    })
}

fn worktree_add_args(
    repository: &Path,
    workspace: &Path,
    branch: &str,
    base: &str,
    branch_exists: bool,
) -> Vec<OsString> {
    let mut args = vec![
        "-C".into(),
        repository.as_os_str().to_owned(),
        "worktree".into(),
        "add".into(),
    ];
    if !branch_exists {
        args.push("-b".into());
        args.push(branch.into());
    }
    args.push("--".into());
    args.push(workspace.as_os_str().to_owned());
    args.push(if branch_exists {
        branch.into()
    } else {
        base.into()
    });
    args
}

fn ssh_command_args(destination: &str, script: &str, forwarding: Option<&str>) -> Vec<OsString> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
    ];
    if let Some(forwarding) = forwarding {
        args.push("-L".into());
        args.push(forwarding.into());
    }
    args.push("--".into());
    args.push(destination.into());
    args.push(format!("sh -lc {}", shell_quote(script)).into());
    args
}

fn remote_path_expression(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path == "~" {
        return "\"$HOME\"".into();
    }
    if let Some(relative) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", shell_quote(relative));
    }
    shell_quote(&path)
}

fn latest_message(value: &Value) -> Option<(String, String)> {
    let messages = value
        .as_array()
        .or_else(|| value.get("data").and_then(Value::as_array))?;
    let message = messages.last()?;
    let id = message
        .get("info")
        .and_then(|info| find_string(info, &["id"]))?;
    let parts = message.get("parts")?.as_array()?;
    let text = parts
        .iter()
        .rev()
        .find_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))?;
    Some((id, text))
}

fn pending_question(value: &Value, session_id: &str) -> Option<PendingQuestion> {
    let requests = value
        .as_array()
        .or_else(|| value.get("data").and_then(Value::as_array))?;
    requests.iter().find_map(|request| {
        let request_session = request
            .get("sessionID")
            .or_else(|| request.get("sessionId"))
            .and_then(Value::as_str)?;
        if request_session != session_id {
            return None;
        }
        let id = request
            .get("id")
            .or_else(|| request.get("requestID"))
            .or_else(|| request.get("requestId"))
            .and_then(Value::as_str)?;
        let questions = request.get("questions")?.as_array()?;
        Some(PendingQuestion {
            id: id.into(),
            count: questions.len(),
            prompt: readable_question_prompt(id, questions),
        })
    })
}

fn readable_question_prompt(id: &str, questions: &[Value]) -> String {
    let mut lines = vec![format!(
        "OpenCode question {id} has {} prompt{}:",
        questions.len(),
        if questions.len() == 1 {
            ""
        } else {
            "s"
        }
    )];
    for (index, question) in questions.iter().enumerate() {
        let header = question
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("Question");
        let text = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("No question text provided");
        lines.push(format!("{}. {header}: {text}", index + 1));
        if let Some(options) = question.get("options").and_then(Value::as_array) {
            let options = options
                .iter()
                .filter_map(|option| {
                    let label = option.get("label")?.as_str()?;
                    let description = option.get("description").and_then(Value::as_str);
                    Some(description.map_or_else(
                        || label.to_string(),
                        |description| format!("{label} - {description}"),
                    ))
                })
                .collect::<Vec<_>>();
            if !options.is_empty() {
                lines.push(format!("Options: {}", options.join("; ")));
            }
        }
    }
    lines.push("Reply with an answer, or reject.".into());
    lines.join("\n")
}

enum QuestionAction<'a> {
    Reply(Vec<Vec<&'a str>>),
    Reject,
}

fn question_action(text: &str, count: usize) -> QuestionAction<'_> {
    if text.trim().eq_ignore_ascii_case("reject") {
        QuestionAction::Reject
    } else {
        QuestionAction::Reply(vec![vec![text]; count])
    }
}

fn permission_reply(value: &str) -> Result<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "once" => Ok("once"),
        "always" => Ok("always"),
        "reject" => Ok("reject"),
        _ => Err(Error::InvalidRequest(
            "a pending OpenCode permission must be answered with once, always, or reject".into(),
        )),
    }
}

fn redact_remote_provision_error(error: Error, remote_url: &str) -> Error {
    const LABEL: &str = "remote Git provisioning over SSH";
    match error {
        Error::CommandFailed { status, stderr, .. } => Error::CommandFailed {
            program: LABEL.into(),
            status,
            stderr: redact_remote_credentials(&stderr, remote_url),
        },
        Error::CommandTimedOut { timeout, .. } => Error::CommandTimedOut {
            program: LABEL.into(),
            timeout,
        },
        error => error,
    }
}

fn redact_remote_credentials(stderr: &str, remote_url: &str) -> String {
    let mut redacted = stderr.replace(remote_url, "[redacted remote URL]");
    if let Ok(url) = Url::parse(remote_url) {
        let username = url.username();
        let password = url.password();
        if !username.is_empty() {
            redacted = redacted.replace(username, "[redacted]");
        }
        if let Some(password) = password.filter(|password| !password.is_empty()) {
            redacted = redacted.replace(password, "[redacted]");
        }
    }
    redacted
}

fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

fn map_executable_error(executable: &Path, error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::ExecutableNotFound(executable.display().to_string())
    } else {
        Error::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_new_and_existing_branch_worktree_commands() {
        let new_branch = worktree_add_args(
            Path::new("/repo"),
            Path::new("/data/work tree"),
            "agent/fix",
            "main",
            false,
        );
        assert_eq!(
            new_branch,
            os_args([
                "-C",
                "/repo",
                "worktree",
                "add",
                "-b",
                "agent/fix",
                "--",
                "/data/work tree",
                "main",
            ])
        );
        let existing = worktree_add_args(
            Path::new("/repo"),
            Path::new("/workspace"),
            "agent/fix",
            "main",
            true,
        );
        assert!(!existing.contains(&OsString::from("-b")));
        assert_eq!(existing.last(), Some(&OsString::from("agent/fix")));
    }

    #[test]
    fn builds_safely_quoted_ssh_command() {
        let args = ssh_command_args("dev@example", "cd '/tmp/a'\\''b' && opencode", None);
        assert_eq!(args[4], OsString::from("--"));
        assert_eq!(args[5], OsString::from("dev@example"));
        assert_eq!(
            args[6],
            OsString::from("sh -lc 'cd '\\''/tmp/a'\\''\\'\\'''\\''b'\\'' && opencode'")
        );
    }

    #[test]
    fn reads_branch_from_porcelain_worktree_output() {
        let output = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /tmp/work tree\nHEAD def\nbranch refs/heads/agent/fix\n";
        assert_eq!(
            worktree_branch(output, Path::new("/tmp/work tree")).as_deref(),
            Some("refs/heads/agent/fix")
        );
    }

    #[test]
    fn expands_only_the_remote_home_prefix() {
        assert_eq!(
            remote_path_expression(Path::new("~/.local/share/agent launcher")),
            "\"$HOME\"/'.local/share/agent launcher'"
        );
        assert_eq!(remote_path_expression(Path::new("/srv/a b")), "'/srv/a b'");
    }

    #[test]
    fn parses_representative_opencode_messages() {
        let value = json!([{
            "info": {"id": "msg_1"},
            "parts": [{"type": "text", "text": "Implemented the fix"}]
        }]);
        assert_eq!(
            latest_message(&value),
            Some(("msg_1".into(), "Implemented the fix".into()))
        );
    }

    #[test]
    fn validates_explicit_permission_replies() {
        assert_eq!(permission_reply(" ONCE ").expect("once is valid"), "once");
        assert!(permission_reply("continue").is_err());
    }

    #[test]
    fn omits_only_the_default_opencode_agent_from_prompts() {
        let default = prompt_body("opencode", "fix it", Some("openai/gpt-5"));
        assert!(default.get("agent").is_none());
        assert_eq!(default["model"]["providerID"], "openai");

        let custom = prompt_body("reviewer", "check it", None);
        assert_eq!(custom["agent"], "reviewer");
    }

    #[test]
    fn parses_and_formats_pending_questions_for_the_session() {
        let value = json!([
            {
                "id": "que_other",
                "sessionID": "ses_other",
                "questions": [{"header": "Ignored", "question": "Ignore?", "options": []}]
            },
            {
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [
                    {
                        "header": "Target",
                        "question": "Where should this deploy?",
                        "options": [{"label": "Staging", "description": "Deploy to staging"}]
                    },
                    {"header": "Timing", "question": "Deploy now?", "options": []}
                ]
            }
        ]);
        let pending = pending_question(&value, "ses_1").expect("question should match");
        assert_eq!(pending.id, "que_1");
        assert_eq!(pending.count, 2);
        assert!(pending.prompt.contains("Target: Where should this deploy?"));
        assert!(pending.prompt.contains("Staging - Deploy to staging"));
    }

    #[test]
    fn creates_ordered_question_answers_and_explicit_rejection() {
        let QuestionAction::Reply(answers) = question_action("Staging", 2) else {
            panic!("answer should produce a reply");
        };
        assert_eq!(answers, vec![vec!["Staging"], vec!["Staging"]]);
        assert!(matches!(
            question_action(" REJECT ", 1),
            QuestionAction::Reject
        ));
    }

    #[test]
    fn redacts_remote_credentials_from_provisioning_failures() {
        let error = Error::CommandFailed {
            program: "ssh host sh -lc git clone https://alice:secret@example.com/repo.git".into(),
            status: "128".into(),
            stderr: "fatal: unable to access 'https://alice:secret@example.com/repo.git': secret for alice was rejected".into(),
        };
        let redacted =
            redact_remote_provision_error(error, "https://alice:secret@example.com/repo.git")
                .to_string();
        assert!(redacted.contains("remote Git provisioning over SSH"));
        assert!(!redacted.contains("https://alice:secret"));
        assert!(!redacted.contains("alice"));
        assert!(!redacted.contains("secret"));
    }
}
