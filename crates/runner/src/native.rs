use std::{
    cmp::Ordering,
    ffi::OsString,
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use agent_launcher_core::{
    BackendKind, ComputePlacement, ComputeProvider, ComputeTargetAvailability, ComputeTargetStatus,
    Repository, RunState, RunSummary, WakeConfig, WorkspaceRef, WorktreeInspection,
};
use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::join_all;
use reqwest::{Client, RequestBuilder, Response};
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
    pub id: String,
    pub name: String,
    pub destination: String,
    pub workspace_root: PathBuf,
    pub max_active_runs: Option<usize>,
    pub wake: Option<WakeConfig>,
}

#[derive(Clone, Debug)]
pub struct NativeConfig {
    pub git_executable: PathBuf,
    pub opencode_executable: PathBuf,
    pub ssh_executable: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub ssh_targets: Vec<NativeSshConfig>,
    pub placement: ComputePlacement,
    pub startup_timeout: Duration,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            git_executable: "git".into(),
            opencode_executable: "opencode".into(),
            ssh_executable: "ssh".into(),
            workspace_root: None,
            ssh_targets: Vec::new(),
            placement: ComputePlacement::LeastLoaded,
            startup_timeout: Duration::from_secs(15),
        }
    }
}

pub struct NativeBackend {
    config: NativeConfig,
    registry: Arc<SessionRegistry>,
    client: Client,
    ssh_readiness_lock: Mutex<()>,
    reconnect_lock: Mutex<()>,
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
            reconnect_lock: Mutex::new(()),
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

    fn target_by_id(&self, id: &str) -> Result<&NativeSshConfig> {
        self.config
            .ssh_targets
            .iter()
            .find(|target| target.id == id)
            .ok_or_else(|| Error::ComputeTargetNotFound(id.to_owned()))
    }

    fn target_for_record(&self, record: &RunRecord) -> Result<&NativeSshConfig> {
        let BackendSession::Native { target_id, .. } = &record.session else {
            return Err(Error::RunNotFound(record.summary.id.clone()));
        };
        if let Some(target_id) = target_id {
            let target = self.target_by_id(target_id)?;
            self.validate_target_location(record, target)?;
            return Ok(target);
        }
        let destination = record
            .summary
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.host.as_deref())
            .ok_or_else(|| {
                Error::InvalidRequest("the run's SSH compute target is missing".into())
            })?;
        let mut matches = self
            .config
            .ssh_targets
            .iter()
            .filter(|target| target.destination == destination);
        let target = matches.next().ok_or_else(|| {
            Error::InvalidRequest(format!(
                "the run's SSH host {destination} is not configured"
            ))
        })?;
        if matches.next().is_some() {
            return Err(Error::InvalidRequest(format!(
                "the legacy run's SSH host {destination} matches multiple compute targets"
            )));
        }
        self.validate_target_location(record, target)?;
        Ok(target)
    }

    fn validate_target_location(&self, record: &RunRecord, target: &NativeSshConfig) -> Result<()> {
        let workspace = record
            .summary
            .workspace
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("run has no workspace".into()))?;
        if workspace.host.as_deref() != Some(target.destination.as_str()) {
            return Err(Error::InvalidRequest(format!(
                "compute target `{}` now points to {}, but this run belongs to {}; restore the original host until the run is deleted",
                target.id,
                target.destination,
                workspace.host.as_deref().unwrap_or("an unknown host")
            )));
        }
        let expected_parent = target.workspace_root.join("workspaces");
        if !workspace
            .path
            .as_ref()
            .is_some_and(|path| path.starts_with(&expected_parent))
        {
            return Err(Error::InvalidRequest(format!(
                "compute target `{}` workspace_root changed; restore the original root until the run is deleted",
                target.id
            )));
        }
        Ok(())
    }

    async fn active_runs(&self, target: &NativeSshConfig) -> usize {
        self.registry
            .native_target_active_runs(&target.id, &target.destination)
            .await
    }

    async fn ensure_capacity(&self, target: &NativeSshConfig) -> Result<usize> {
        let active = self.active_runs(target).await;
        if let Some(maximum) = target.max_active_runs
            && active >= maximum
        {
            return Err(Error::ComputeTargetAtCapacity {
                target: target.id.clone(),
                active,
                maximum,
            });
        }
        Ok(active)
    }

    async fn select_ready_target(&self, requested: Option<&str>) -> Result<&NativeSshConfig> {
        if let Some(id) = requested {
            let target = self.target_by_id(id)?;
            self.ensure_capacity(target).await?;
            self.ensure_ssh_ready(target).await?;
            return Ok(target);
        }

        let probes = join_all(self.config.ssh_targets.iter().enumerate().map(
            |(index, target)| async move {
                let active = self.active_runs(target).await;
                let reachable = self.ssh_ready(target).await.is_ok();
                let metrics = if reachable {
                    self.remote_metrics(target).await.ok()
                } else {
                    None
                };
                let load = metrics.map_or(f32::INFINITY, |metrics| metrics.cpu.max(metrics.memory));
                (index, target, active, reachable, load)
            },
        ))
        .await;
        let any_capacity = probes.iter().any(|(_, target, active, ..)| {
            target
                .max_active_runs
                .is_none_or(|maximum| *active < maximum)
        });
        let mut candidates = probes
            .into_iter()
            .filter(|(_, target, active, reachable, _)| {
                target
                    .max_active_runs
                    .is_none_or(|maximum| *active < maximum)
                    && (*reachable || target.wake.is_some())
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(if any_capacity {
                Error::NoComputeTargetAvailable
            } else {
                Error::NoComputeTargetCapacity
            });
        }
        match self.config.placement {
            ComputePlacement::LeastLoaded => candidates.sort_by(compare_target_candidates),
            ComputePlacement::Random => {
                let offset = Uuid::new_v4().as_u128() as usize % candidates.len();
                candidates.rotate_left(offset);
            },
        }
        let mut last_error = None;
        for (_, target, ..) in candidates {
            match self.ensure_ssh_ready(target).await {
                Ok(()) => return Ok(target),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or(Error::NoComputeTargetAvailable))
    }

    async fn provision_local(
        &self,
        repository: &Repository,
        workspace_path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<()> {
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

    async fn launch_local_server(&self, workspace_path: &Path) -> Result<(Url, Child)> {
        let local_port = available_port()?;
        let mut command = Command::new(&self.config.opencode_executable);
        command
            .arg("serve")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(local_port.to_string())
            .current_dir(workspace_path);
        // The server outlives this launcher process so persisted sessions can reconnect on restart.
        // OpenCode permissions remain interactive/default; notably, --auto is never passed.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| map_executable_error(&self.config.opencode_executable, error))?;
        let base_url = Url::parse(&format!("http://127.0.0.1:{local_port}/"))
            .map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if let Err(error) = self.wait_for_health(&base_url, &mut child, None).await {
            let _ = child.kill().await;
            return Err(error);
        }
        Ok((base_url, child))
    }

    async fn start_remote_server(
        &self,
        ssh: &NativeSshConfig,
        workspace_path: &str,
        workspace_id: &str,
        remote_port: u16,
    ) -> Result<RemoteServer> {
        let runtime_path = ssh.workspace_root.join("runtime").join(workspace_id);
        let script = remote_server_start_script(&runtime_path, workspace_path, remote_port);
        let output = run_output(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, &script, None),
            None,
        )
        .await?;
        parse_remote_server_start(&output)
    }

    async fn start_remote_server_available(
        &self,
        ssh: &NativeSshConfig,
        workspace_path: &str,
        workspace_id: &str,
        preferred_port: Option<u16>,
    ) -> Result<RemoteServer> {
        let mut last_conflict = None;
        for remote_port in remote_port_candidates(workspace_id, preferred_port) {
            match self
                .start_remote_server(ssh, workspace_path, workspace_id, remote_port)
                .await
            {
                Ok(server) => return Ok(server),
                Err(error) if remote_port_conflict(&error) => last_conflict = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last_conflict.unwrap_or_else(|| {
            Error::InvalidRequest("no remote OpenCode server ports are available".into())
        }))
    }

    async fn launch_remote_tunnel(
        &self,
        ssh: &NativeSshConfig,
        remote_port: u16,
        password: &str,
    ) -> Result<(Url, Child)> {
        let local_port = available_port()?;
        let forwarding = format!("{local_port}:127.0.0.1:{remote_port}");
        let mut command = Command::new(&self.config.ssh_executable);
        command
            .args(ssh_tunnel_args(&ssh.destination, &forwarding))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| map_executable_error(&self.config.ssh_executable, error))?;
        let base_url = Url::parse(&format!("http://127.0.0.1:{local_port}/"))
            .map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if let Err(error) = self
            .wait_for_health(&base_url, &mut child, Some(password))
            .await
        {
            let _ = child.kill().await;
            return Err(error);
        }
        Ok((base_url, child))
    }

    async fn stop_remote_server(
        &self,
        ssh: &NativeSshConfig,
        workspace_id: &str,
        remote_port: u16,
    ) -> Result<()> {
        let runtime_path = ssh.workspace_root.join("runtime").join(workspace_id);
        let script = remote_server_stop_script(&runtime_path, remote_port);
        run_output(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, &script, None),
            None,
        )
        .await?;
        Ok(())
    }

    async fn rollback_dispatch(
        &self,
        ssh: Option<&NativeSshConfig>,
        workspace_id: &str,
        remote_port: Option<u16>,
        server_start: Option<RemoteServerStart>,
        server_password: Option<&str>,
        base_url: &Url,
        session_id: Option<&str>,
        child: &mut Child,
    ) {
        if let Some(session_id) = session_id
            && let Ok(url) = endpoint(base_url, &["session", session_id, "abort"])
        {
            let _ = self
                .authenticated(self.client.post(url), server_password)
                .send()
                .await;
        }
        let _ = child.kill().await;
        if server_start == Some(RemoteServerStart::Started)
            && let (Some(ssh), Some(remote_port)) = (ssh, remote_port)
        {
            let _ = self
                .stop_remote_server(ssh, workspace_id, remote_port)
                .await;
        }
    }

    async fn terminate_managed_process(
        &self,
        run_id: &str,
        process_id: Option<u32>,
        base_url: &Url,
        remote: bool,
        _password: Option<&str>,
    ) -> Result<bool> {
        if let Some(mut managed_child) = self.registry.children.lock().await.remove(run_id) {
            if managed_child.child.try_wait()?.is_none() {
                managed_child.child.kill().await?;
                return Ok(true);
            }
            return Ok(false);
        }
        let Some(process_id) = process_id else {
            return Ok(false);
        };
        let executable = if remote {
            &self.config.ssh_executable
        } else {
            &self.config.opencode_executable
        };
        #[cfg(windows)]
        if !self.health(base_url, _password).await {
            return Ok(false);
        }
        if !managed_process_matches(process_id, executable, base_url, remote).await? {
            return Ok(false);
        }
        terminate_process(process_id).await
    }

    async fn connection_process_matches(
        &self,
        run_id: &str,
        process_id: Option<u32>,
        base_url: &Url,
        remote: bool,
    ) -> Result<bool> {
        let Some(process_id) = process_id else {
            return Ok(false);
        };
        let mut children = self.registry.children.lock().await;
        if let Some(child) = children.get_mut(run_id) {
            if child.child.id() != Some(process_id) {
                return Ok(false);
            }
            if child.child.try_wait()?.is_none() {
                return Ok(true);
            }
            children.remove(run_id);
            return Ok(false);
        }
        drop(children);
        let executable = if remote {
            &self.config.ssh_executable
        } else {
            &self.config.opencode_executable
        };
        managed_process_matches(process_id, executable, base_url, remote).await
    }

    async fn wait_for_health(
        &self,
        base_url: &Url,
        child: &mut Child,
        password: Option<&str>,
    ) -> Result<()> {
        let deadline = Instant::now() + self.config.startup_timeout;
        let health_url = endpoint(base_url, &["global", "health"])?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(Error::Disconnected(format!(
                    "OpenCode server exited during startup with {status}"
                )));
            }
            if self
                .authenticated(self.client.get(health_url.clone()), password)
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

    fn authenticated(&self, request: RequestBuilder, password: Option<&str>) -> RequestBuilder {
        match password {
            Some(password) => request.basic_auth("opencode", Some(password)),
            None => request,
        }
    }

    async fn health(&self, base_url: &Url, password: Option<&str>) -> bool {
        let Ok(url) = endpoint(base_url, &["global", "health"]) else {
            return false;
        };
        self.authenticated(self.client.get(url), password)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    async fn connection_matches(
        &self,
        record: &RunRecord,
        base_url: &Url,
        password: Option<&str>,
    ) -> bool {
        if !self.health(base_url, password).await {
            return false;
        }
        let Some(session_id) = record.summary.session_id.as_deref() else {
            return true;
        };
        let Ok(url) = endpoint(base_url, &["session", session_id]) else {
            return false;
        };
        self.authenticated(self.client.get(url), password)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    async fn complete_initial_prompt(
        &self,
        run_id: &str,
        state: RunState,
        message: Option<String>,
    ) -> Result<RunSummary> {
        let mut record = self.record(run_id).await?;
        let BackendSession::Native { initial_prompt, .. } = &mut record.session else {
            unreachable!()
        };
        *initial_prompt = None;
        record.summary.state = state;
        record.summary.message = message;
        record.summary.updated_at = Utc::now();
        let summary = record.summary.clone();
        self.registry.update(record).await?;
        Ok(summary)
    }

    async fn recover_initial_prompt(
        &self,
        mut record: RunRecord,
        base_url: &Url,
    ) -> Result<RunRecord> {
        if record.summary.state != RunState::Starting {
            return Ok(record);
        }
        let BackendSession::Native {
            initial_prompt,
            server_password,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let Some(body) = initial_prompt.clone() else {
            return Ok(record);
        };
        let message_id = body
            .get("messageID")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::InvalidResponse("persisted initial prompt has no message ID".into())
            })?;
        let session_id = record
            .summary
            .session_id
            .as_deref()
            .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
        let messages_url = endpoint(base_url, &["session", session_id, "message"])?;
        let messages = self
            .request_json(
                self.authenticated(self.client.get(messages_url), server_password.as_deref())
                    .send()
                    .await,
            )
            .await?;
        if !has_message(&messages, message_id) {
            let prompt_url = endpoint(base_url, &["session", session_id, "prompt_async"])?;
            let result = self
                .request_empty(
                    self.authenticated(self.client.post(prompt_url), server_password.as_deref())
                        .json(&body)
                        .send()
                        .await,
                )
                .await;
            if let Err(error) = result {
                if !initial_prompt_definitively_rejected(&error) {
                    return Err(error);
                }
                let BackendSession::Native { initial_prompt, .. } = &mut record.session else {
                    unreachable!()
                };
                *initial_prompt = None;
                record.summary.state = RunState::Failed;
                record.summary.message = Some(format!("Initial prompt failed: {error}"));
                record.summary.updated_at = Utc::now();
                self.registry.update(record.clone()).await?;
                return Ok(record);
            }
        }
        let BackendSession::Native { initial_prompt, .. } = &mut record.session else {
            unreachable!()
        };
        *initial_prompt = None;
        record.summary.state = RunState::Running;
        record.summary.message = None;
        record.summary.updated_at = Utc::now();
        self.registry.update(record.clone()).await?;
        Ok(record)
    }

    async fn pending_permission(
        &self,
        base_url: &Url,
        session_id: &str,
        password: Option<&str>,
    ) -> Option<PendingPermission> {
        let Ok(url) = endpoint(base_url, &["permission"]) else {
            return None;
        };
        let Ok(response) = self
            .authenticated(self.client.get(url), password)
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

    async fn pending_question(
        &self,
        base_url: &Url,
        session_id: &str,
        password: Option<&str>,
    ) -> Option<PendingQuestion> {
        let Ok(url) = endpoint(base_url, &["question"]) else {
            return None;
        };
        let Ok(response) = self
            .authenticated(self.client.get(url), password)
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
        let script = "for tool in git opencode nohup ps xargs od tr readlink awk uname sleep; do command -v \"$tool\" >/dev/null || { printf 'agent-launcher: missing required remote tool %s\\n' \"$tool\" >&2; exit 127; }; done; os=$(uname -s); if [ \"$os\" = Darwin ]; then for tool in top vm_stat sysctl; do command -v \"$tool\" >/dev/null || { printf 'agent-launcher: missing required macOS tool %s\\n' \"$tool\" >&2; exit 127; }; done; fi; ps -p $$ -o lstart= >/dev/null 2>&1 || { printf 'agent-launcher: remote ps does not support -o lstart=\\n' >&2; exit 127; }; printf '' | xargs -0 true >/dev/null 2>&1 || { printf 'agent-launcher: remote xargs does not support -0\\n' >&2; exit 127; }";
        run_output_with_timeout(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, script, None),
            None,
            Duration::from_secs(3),
        )
        .await
        .map(|_| ())
    }

    async fn remote_metrics(&self, ssh: &NativeSshConfig) -> Result<RemoteMetrics> {
        let output = run_output_with_timeout(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, remote_metrics_script(), None),
            None,
            Duration::from_secs(4),
        )
        .await?;
        parse_remote_metrics(&output)
    }

    async fn compute_target_status(&self, target: &NativeSshConfig) -> ComputeTargetStatus {
        let active_runs = self.active_runs(target).await;
        let full = target
            .max_active_runs
            .is_some_and(|maximum| active_runs >= maximum);
        let readiness = self.ssh_ready(target).await;
        let (metrics, metrics_error) = if readiness.is_ok() {
            match self.remote_metrics(target).await {
                Ok(metrics) => (Some(metrics), None),
                Err(error) => (None, Some(format!("load metrics unavailable: {error}"))),
            }
        } else {
            (None, None)
        };
        let availability = if full {
            ComputeTargetAvailability::Full
        } else if readiness.is_ok() {
            ComputeTargetAvailability::Online
        } else if target.wake.is_some()
            && readiness.as_ref().is_err_and(|error| {
                !matches!(error, Error::ExecutableNotFound(_))
                    && !remote_prerequisite_missing(error)
            })
        {
            ComputeTargetAvailability::Wakeable
        } else {
            ComputeTargetAvailability::Offline
        };
        let message = readiness
            .err()
            .map(|error| error.to_string())
            .or(metrics_error);
        ComputeTargetStatus {
            id: target.id.clone(),
            name: target.name.clone(),
            provider: ComputeProvider::Ssh,
            availability,
            active_runs,
            max_active_runs: target.max_active_runs,
            cpu_percent: metrics.as_ref().map(|metrics| metrics.cpu),
            memory_percent: metrics.as_ref().map(|metrics| metrics.memory),
            sampled_at: Utc::now(),
            message,
        }
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

    async fn connected_record(&self, run_id: &str) -> Result<(RunRecord, Url)> {
        let record = self.record(run_id).await?;
        let BackendSession::Native {
            base_url,
            remote,
            server_password,
            process_id,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let base_url =
            Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if self
            .connection_process_matches(run_id, *process_id, &base_url, *remote)
            .await?
            && self
                .connection_matches(&record, &base_url, server_password.as_deref())
                .await
        {
            return Ok((record, base_url));
        }
        if !remote {
            return Err(Error::Disconnected(run_id.into()));
        }

        let _guard = self.reconnect_lock.lock().await;
        let record = self.record(run_id).await?;
        let BackendSession::Native {
            base_url,
            remote_workspace_path,
            remote_port,
            server_password,
            process_id,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let stale_url =
            Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        if self
            .connection_process_matches(run_id, *process_id, &stale_url, true)
            .await?
            && self
                .connection_matches(&record, &stale_url, server_password.as_deref())
                .await
        {
            return Ok((record, stale_url));
        }

        let workspace = record
            .summary
            .workspace
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("run has no workspace".into()))?;
        let ssh = self.target_for_record(&record)?;
        let remote_workspace_path = remote_workspace_path.clone().or_else(|| {
            workspace
                .path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
        });
        let remote_workspace_path = remote_workspace_path
            .ok_or_else(|| Error::InvalidRequest("remote workspace path is missing".into()))?;
        let preferred_port = remote_port.unwrap_or_else(|| remote_port_for(run_id));
        let workspace_id = workspace.id.clone();
        let stale_process_id = *process_id;

        let _ = self
            .terminate_managed_process(
                run_id,
                stale_process_id,
                &stale_url,
                true,
                server_password.as_deref(),
            )
            .await;
        self.ensure_ssh_ready(ssh).await?;
        let server = self
            .start_remote_server_available(
                ssh,
                &remote_workspace_path,
                &workspace_id,
                Some(preferred_port),
            )
            .await?;
        let (base_url, child) = match self
            .launch_remote_tunnel(ssh, server.port, &server.password)
            .await
        {
            Ok(connection) => connection,
            Err(error) => {
                if server.start == RemoteServerStart::Started {
                    let _ = self
                        .stop_remote_server(ssh, &workspace_id, server.port)
                        .await;
                }
                return Err(error);
            },
        };
        if !self
            .connection_matches(&record, &base_url, Some(&server.password))
            .await
        {
            let mut child = child;
            let _ = child.kill().await;
            if server.start == RemoteServerStart::Started {
                let _ = self
                    .stop_remote_server(ssh, &workspace_id, server.port)
                    .await;
            }
            return Err(Error::Disconnected(format!(
                "OpenCode session for run {run_id} is unavailable on the remote server"
            )));
        }

        self.persist_reconnected_tunnel(
            ssh,
            run_id,
            record,
            remote_workspace_path,
            workspace_id,
            server,
            base_url,
            child,
        )
        .await
    }

    async fn persist_reconnected_tunnel(
        &self,
        ssh: &NativeSshConfig,
        run_id: &str,
        mut record: RunRecord,
        remote_workspace_path: String,
        workspace_id: String,
        server: RemoteServer,
        base_url: Url,
        child: Child,
    ) -> Result<(RunRecord, Url)> {
        let process_id = child.id();
        let BackendSession::Native {
            base_url: persisted_url,
            remote_workspace_path: persisted_workspace,
            remote_port: persisted_port,
            server_password: persisted_password,
            process_id: persisted_process_id,
            ..
        } = &mut record.session
        else {
            unreachable!()
        };
        *persisted_url = base_url.to_string();
        *persisted_workspace = Some(remote_workspace_path);
        *persisted_port = Some(server.port);
        *persisted_password = Some(server.password.clone());
        *persisted_process_id = process_id;
        if let Err(error) = self.registry.update(record.clone()).await {
            let mut child = child;
            let _ = child.kill().await;
            if server.start == RemoteServerStart::Started {
                let _ = self
                    .stop_remote_server(ssh, &workspace_id, server.port)
                    .await;
            }
            return Err(error);
        }
        self.registry
            .children
            .lock()
            .await
            .insert(run_id.into(), ManagedChild { child });
        Ok((record, base_url))
    }

    async fn remote_git_output(
        &self,
        ssh: &NativeSshConfig,
        workspace_path: &Path,
        args: &[&str],
    ) -> std::result::Result<String, String> {
        let arguments = args
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "git -C {} {arguments}",
            remote_path_expression(workspace_path)
        );
        run_output(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, &script, None),
            None,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn inspect_remote_worktree(
        &self,
        ssh: &NativeSshConfig,
        workspace_path: &Path,
    ) -> WorktreeInspection {
        if let Err(error) = self.ensure_ssh_ready(ssh).await {
            return WorktreeInspection {
                warning: Some(format!(
                    "Could not inspect changes on remote host {}: {error}",
                    ssh.destination
                )),
                ..WorktreeInspection::default()
            };
        }
        let status = self
            .remote_git_output(ssh, workspace_path, &[
                "status",
                "--porcelain",
                "--untracked-files=normal",
                "--ignored=matching",
            ])
            .await;
        let unpushed = self
            .remote_git_output(ssh, workspace_path, &[
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                "--remotes",
            ])
            .await;
        let fingerprint = self.remote_worktree_fingerprint(ssh, workspace_path).await;
        let has_uncommitted_changes = status
            .as_ref()
            .is_ok_and(|output| output.lines().any(|line| !line.starts_with("!! ")));
        let has_ignored_files = status
            .as_ref()
            .is_ok_and(|output| output.lines().any(|line| line.starts_with("!! ")));
        let unpushed_commits = unpushed
            .as_ref()
            .ok()
            .and_then(|output| output.trim().parse().ok())
            .unwrap_or(0);
        let inspection_fingerprint = fingerprint.as_ref().ok().cloned();
        let errors = [status.err(), unpushed.err(), fingerprint.err()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        WorktreeInspection {
            has_uncommitted_changes,
            has_ignored_files,
            unpushed_commits,
            warning: (!errors.is_empty())
                .then(|| format!("Worktree safety inspection was incomplete: {errors}")),
            fingerprint: inspection_fingerprint,
        }
    }

    async fn remote_worktree_fingerprint(
        &self,
        ssh: &NativeSshConfig,
        workspace_path: &Path,
    ) -> std::result::Result<String, String> {
        let workspace = remote_path_expression(workspace_path);
        let script = format!(
            "workspace={workspace}; cd \"$workspace\" && \
             git status --porcelain=v1 -z && printf '\\n--head--\\n' && git rev-parse HEAD && \
             printf '%s\\n' '--tracked--' && git diff --binary HEAD -- && \
             printf '%s\\n' '--untracked-paths--' && git ls-files --others --exclude-standard -z && \
             printf '%s\\n' '--untracked-hashes--' && \
             git ls-files --others --exclude-standard -z | xargs -0 -n 1 sh -c 'if [ \"$#\" -eq 0 ]; then exit 0; fi; path=$1; if [ -L \"$path\" ]; then printf \"link:\"; readlink \"./$path\"; elif [ -f \"$path\" ]; then git hash-object --no-filters -- \"$path\"; else printf \"agent-launcher: refusing to hash non-regular path %s\\n\" \"$path\" >&2; exit 1; fi' sh && \
             printf '%s\\n' '--ignored-paths--' && git ls-files --others --ignored --exclude-standard -z && \
             printf '%s\\n' '--ignored-hashes--' && \
             git ls-files --others --ignored --exclude-standard -z | xargs -0 -n 1 sh -c 'if [ \"$#\" -eq 0 ]; then exit 0; fi; path=$1; if [ -L \"$path\" ]; then printf \"link:\"; readlink \"./$path\"; elif [ -f \"$path\" ]; then git hash-object --no-filters -- \"$path\"; else printf \"agent-launcher: refusing to hash non-regular path %s\\n\" \"$path\" >&2; exit 1; fi' sh"
        );
        let output = run_output_with_timeout(
            &self.config.ssh_executable,
            &ssh_command_args(&ssh.destination, &script, None),
            None,
            Duration::from_secs(30),
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        Ok(format!("{:016x}", hasher.finish()))
    }
}

struct PendingPermission {
    id: String,
    permission: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteServerStart {
    Started,
    Reused,
}

struct RemoteServer {
    port: u16,
    start: RemoteServerStart,
    password: String,
}

#[derive(Clone, Copy, Debug)]
struct RemoteMetrics {
    cpu: f32,
    memory: f32,
}

type TargetCandidate<'a> = (usize, &'a NativeSshConfig, usize, bool, f32);

fn compare_target_candidates(left: &TargetCandidate<'_>, right: &TargetCandidate<'_>) -> Ordering {
    right
        .3
        .cmp(&left.3)
        .then_with(|| left.2.cmp(&right.2))
        .then_with(|| left.4.total_cmp(&right.4))
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| left.1.id.cmp(&right.1.id))
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
            Capability::DeleteWorktree,
        ];
        if !self.config.ssh_targets.is_empty() {
            capabilities.push(Capability::Remote);
        } else {
            capabilities.push(Capability::Open);
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
        if !self.config.ssh_targets.is_empty() {
            let compute_targets = join_all(
                self.config
                    .ssh_targets
                    .iter()
                    .map(|target| self.compute_target_status(target)),
            )
            .await;
            let available = compute_targets
                .iter()
                .any(ComputeTargetStatus::is_dispatchable);
            let message = (!available).then(|| {
                if compute_targets.iter().all(ComputeTargetStatus::is_full) {
                    "all compute targets are at capacity".to_owned()
                } else {
                    "no SSH compute target is currently available".to_owned()
                }
            });
            return Ok(BackendDetection {
                backend: self.kind(),
                available,
                manager_running: false,
                capabilities: self.capabilities(),
                message,
                compute_targets,
            });
        }
        let result = {
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
            message: result.as_ref().err().map(ToString::to_string),
            compute_targets: Vec::new(),
        })
    }

    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult> {
        request.validate()?;
        let _provision_guard = self.registry.provision_lock.lock().await;
        let ssh = if self.config.ssh_targets.is_empty() {
            if let Some(target) = &request.target {
                return Err(Error::ComputeTargetNotFound(target.clone()));
            }
            None
        } else {
            Some(self.select_ready_target(request.target.as_deref()).await?)
        };
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
        let run_id = Uuid::new_v4().to_string();

        let (workspace_path, remote_path, remote_port, remote_server_start, server_password) =
            if let Some(ssh) = ssh {
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
                let server = self
                    .start_remote_server_available(ssh, &path_string, &issue_hash, None)
                    .await?;
                (
                    path,
                    Some(path_string),
                    Some(server.port),
                    Some(server.start),
                    Some(server.password),
                )
            } else {
                let path = self
                    .local_workspace_root()?
                    .join(repository_segment)
                    .join(&leaf);
                self.provision_local(&request.repository, &path, &branch, base)
                    .await?;
                (path, None, None, None, None)
            };

        let (base_url, mut child) = if let (Some(ssh), Some(remote_port)) = (ssh, remote_port) {
            match self
                .launch_remote_tunnel(
                    ssh,
                    remote_port,
                    server_password
                        .as_deref()
                        .expect("remote servers have a password"),
                )
                .await
            {
                Ok(connection) => connection,
                Err(error) => {
                    if remote_server_start == Some(RemoteServerStart::Started) {
                        let _ = self.stop_remote_server(ssh, &issue_hash, remote_port).await;
                    }
                    return Err(error);
                },
            }
        } else {
            self.launch_local_server(&workspace_path).await?
        };
        let process_id = child.id();

        let session_url = endpoint(&base_url, &["session"])?;
        let session = match self
            .request_json(
                self.authenticated(
                    self.client.post(session_url),
                    server_password.as_deref(),
                )
                    .json(&json!({"title": format!("{}: {}", request.issue.identifier, request.issue.title)}))
                    .send()
                    .await,
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                self.rollback_dispatch(
                    ssh,
                    &issue_hash,
                    remote_port,
                    remote_server_start,
                    server_password.as_deref(),
                    &base_url,
                    None,
                    &mut child,
                )
                .await;
                return Err(error);
            }
        };
        let session_id = match find_string(&session, &["id", "sessionID", "sessionId"]) {
            Some(session_id) => session_id,
            None => {
                self.rollback_dispatch(
                    ssh,
                    &issue_hash,
                    remote_port,
                    remote_server_start,
                    server_password.as_deref(),
                    &base_url,
                    None,
                    &mut child,
                )
                .await;
                return Err(Error::InvalidResponse("session response has no id".into()));
            },
        };
        let prompt_url = endpoint(&base_url, &["session", &session_id, "prompt_async"])?;
        let message_id = format!("msg_{}", run_id.replace('-', ""));
        let body = prompt_body(
            &request.agent,
            &request.prompt,
            request.model.as_deref(),
            Some(&message_id),
        );
        let now = Utc::now();
        let starting = RunSummary {
            id: run_id.clone(),
            issue_key: request.issue.key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Native,
                id: issue_hash.clone(),
                host: ssh.map(|ssh| ssh.destination.clone()),
                path: Some(workspace_path),
                branch,
            }),
            agent: request.agent,
            state: RunState::Starting,
            message: None,
            session_id: Some(session_id.clone()),
            started_at: now,
            updated_at: now,
        };
        if let Err(error) = self
            .registry
            .insert(RunRecord {
                summary: starting.clone(),
                session: BackendSession::Native {
                    base_url: base_url.to_string(),
                    remote: ssh.is_some(),
                    target_id: ssh.map(|ssh| ssh.id.clone()),
                    remote_workspace_path: remote_path,
                    remote_port,
                    server_password: server_password.clone(),
                    process_id,
                    initial_prompt: Some(body.clone()),
                    pending_permission_id: None,
                    pending_question_id: None,
                    pending_question_count: 0,
                    pending_question_prompt: None,
                    last_message_id: None,
                },
                deletion: None,
            })
            .await
        {
            self.rollback_dispatch(
                ssh,
                &issue_hash,
                remote_port,
                remote_server_start,
                server_password.as_deref(),
                &base_url,
                Some(&session_id),
                &mut child,
            )
            .await;
            return Err(error);
        }
        self.registry
            .children
            .lock()
            .await
            .insert(run_id.clone(), ManagedChild { child });

        let summary = match self
            .request_empty(
                self.authenticated(self.client.post(prompt_url), server_password.as_deref())
                    .json(&body)
                    .send()
                    .await,
            )
            .await
        {
            Ok(()) => self
                .complete_initial_prompt(&run_id, RunState::Running, None)
                .await
                .unwrap_or_else(|_| {
                    let mut summary = starting.clone();
                    summary.message = Some(
                        "Initial prompt was accepted; status persistence will retry on refresh"
                            .into(),
                    );
                    summary
                }),
            Err(error) if initial_prompt_definitively_rejected(&error) => self
                .complete_initial_prompt(
                    &run_id,
                    RunState::Failed,
                    Some(format!("Initial prompt failed: {error}")),
                )
                .await
                .unwrap_or_else(|_| {
                    let mut summary = starting;
                    summary.state = RunState::Failed;
                    summary.message = Some(format!("Initial prompt failed: {error}"));
                    summary
                }),
            Err(error) => {
                let mut summary = starting;
                summary.message = Some(format!(
                    "Initial prompt delivery is unconfirmed and will be reconciled on refresh: {error}"
                ));
                summary
            },
        };
        Ok(DispatchResult {
            run: summary,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let existing = self.record(run_id).await?;
        if existing.summary.state == RunState::Cancelled {
            return Ok(StatusResult {
                run: existing.summary,
                output: None,
            });
        }
        let (record, base_url) = match self.connected_record(run_id).await {
            Ok(connected) => connected,
            Err(error) => {
                let summary = self
                    .registry
                    .set_state(
                        run_id,
                        RunState::Disconnected,
                        Some(format!(
                            "OpenCode server or SSH tunnel is unavailable: {error}"
                        )),
                    )
                    .await?;
                return Ok(StatusResult {
                    run: summary,
                    output: None,
                });
            },
        };
        let record = self.recover_initial_prompt(record, &base_url).await?;
        if record.summary.state == RunState::Failed {
            return Ok(StatusResult {
                run: record.summary,
                output: None,
            });
        }

        let session_id = record
            .summary
            .session_id
            .as_deref()
            .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
        let password = native_server_password(&record.session);
        let status_url = endpoint(&base_url, &["session", "status"])?;
        let statuses = self
            .request_json(
                self.authenticated(self.client.get(status_url), password)
                    .send()
                    .await,
            )
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
        let pending_permission = self
            .pending_permission(&base_url, session_id, password)
            .await;
        let pending_question = self.pending_question(&base_url, session_id, password).await;
        if pending_permission.is_some() || pending_question.is_some() {
            state = RunState::NeedsInput;
        }

        let messages_url = endpoint(&base_url, &["session", session_id, "message"])?;
        let latest_message = match self
            .authenticated(self.client.get(messages_url), password)
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
        if self.record(run_id).await?.summary.state == RunState::Cancelled {
            return Err(Error::InvalidRequest(format!(
                "run {run_id} is cancelled and cannot accept input"
            )));
        }
        let (record, base_url) = self.connected_record(run_id).await?;
        let BackendSession::Native {
            pending_permission_id,
            pending_question_id,
            pending_question_count,
            server_password,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let password = server_password.as_deref();
        if let Some(permission_id) = pending_permission_id {
            let reply = permission_reply(text)?;
            let url = endpoint(&base_url, &["permission", permission_id, "reply"])?;
            self.request_json(
                self.authenticated(self.client.post(url), password)
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
            let request = self.authenticated(self.client.post(url), password);
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
            self.authenticated(self.client.post(url), password)
                .json(&prompt_body(&record.summary.agent, text, None, None))
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
        let initial = self.record(run_id).await?;
        let remote = matches!(&initial.session, BackendSession::Native {
            remote: true,
            ..
        });
        let (record, base_url) = if remote {
            self.connected_record(run_id).await?
        } else {
            let BackendSession::Native { base_url, .. } = &initial.session else {
                unreachable!()
            };
            let base_url =
                Url::parse(base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
            (initial, base_url)
        };
        let BackendSession::Native {
            process_id,
            server_password,
            remote_port,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let process_id = *process_id;
        let password = server_password.as_deref();
        let abort_error = if self.health(&base_url, password).await {
            let session_id = record
                .summary
                .session_id
                .as_deref()
                .ok_or_else(|| Error::InvalidResponse("run has no session id".into()))?;
            let url = endpoint(&base_url, &["session", session_id, "abort"])?;
            self.request_empty(
                self.authenticated(self.client.post(url), password)
                    .send()
                    .await,
            )
            .await
            .err()
        } else {
            Some(Error::Disconnected(run_id.into()))
        };
        let abort_failed = abort_error.is_some();
        let terminated = self
            .terminate_managed_process(run_id, process_id, &base_url, remote, password)
            .await?;
        if remote {
            let ssh = self.target_for_record(&record)?;
            let workspace_id = record
                .summary
                .workspace
                .as_ref()
                .map(|workspace| workspace.id.as_str())
                .ok_or_else(|| Error::InvalidRequest("run has no workspace".into()))?;
            self.ensure_ssh_ready(ssh).await?;
            self.stop_remote_server(
                ssh,
                workspace_id,
                remote_port.unwrap_or_else(|| remote_port_for(run_id)),
            )
            .await?;
        }
        if !terminated && let Some(error) = abort_error {
            return Err(error);
        }
        let message = if terminated && abort_failed {
            "managed OpenCode process or SSH tunnel terminated; session abort was unavailable"
        } else if terminated {
            if remote {
                "OpenCode session aborted; SSH tunnel and remote server stopped"
            } else {
                "OpenCode session aborted and managed server terminated"
            }
        } else {
            "OpenCode session aborted; persisted local process was already unavailable"
        };
        self.registry
            .set_state(run_id, RunState::Cancelled, Some(message.into()))
            .await?;
        Ok(())
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let (record, uri) = self.connected_record(run_id).await?;
        if native_server_password(&record.session).is_some() {
            return Err(Error::InvalidRequest(
                "browser opening is unavailable for authenticated remote OpenCode sessions; use agent-launcher to inspect and control the run"
                    .into(),
            ));
        }
        open_uri(&uri, self.kind()).await?;
        Ok(OpenResult {
            uri,
            launched: true,
        })
    }

    async fn inspect_worktree(&self, run_id: &str) -> Result<WorktreeInspection> {
        let record = self.record(run_id).await?;
        let workspace = record
            .summary
            .workspace
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("run has no workspace".into()))?;
        let BackendSession::Native { remote, .. } = record.session else {
            unreachable!()
        };
        if !remote {
            return Err(Error::UnsupportedCapability {
                backend: self.kind(),
                capability: Capability::DeleteWorktree,
            });
        }
        let ssh = self.target_for_record(&record)?;
        let path = workspace
            .path
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("workspace has no path".into()))?;
        Ok(self.inspect_remote_worktree(ssh, path).await)
    }

    async fn delete_worktree(
        &self,
        run_id: &str,
        force: bool,
        expected: Option<&WorktreeInspection>,
    ) -> Result<()> {
        let record = self.record(run_id).await?;
        let workspace = record
            .summary
            .workspace
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("run has no workspace".into()))?;
        let path = workspace
            .path
            .as_ref()
            .ok_or_else(|| Error::InvalidRequest("workspace has no path".into()))?;
        let ssh = matches!(&record.session, BackendSession::Native { remote: true, .. })
            .then(|| self.target_for_record(&record))
            .transpose()?;
        let BackendSession::Native {
            base_url,
            remote,
            remote_port,
            server_password,
            process_id,
            ..
        } = record.session
        else {
            unreachable!()
        };

        let base_url =
            Url::parse(&base_url).map_err(|error| Error::InvalidResponse(error.to_string()))?;
        self.registry
            .begin_deletion(run_id, force, expected.cloned())
            .await?;
        let deletion: Result<()> = async {
            if remote {
                let ssh = ssh.expect("remote SSH configuration was validated above");
                self.ensure_ssh_ready(ssh).await?;
                self.stop_remote_server(
                    ssh,
                    &workspace.id,
                    remote_port.unwrap_or_else(|| remote_port_for(run_id)),
                )
                .await?;
                self.terminate_managed_process(
                    run_id,
                    process_id,
                    &base_url,
                    true,
                    server_password.as_deref(),
                )
                .await?;
                let runtime = remote_path_expression(
                    &ssh.workspace_root.join("runtime").join(&workspace.id),
                );
                let workspace = remote_path_expression(path);
                let force = if force {
                    " --force"
                } else {
                    ""
                };
                let script = format!(
                    "workspace={workspace}; runtime={runtime}; if [ -e \"$workspace\" ]; then git_dir=$(git -C \"$workspace\" rev-parse --path-format=absolute --git-common-dir) && git --git-dir \"$git_dir\" worktree remove{force} -- \"$workspace\"; fi && rm -rf \"$runtime\""
                );
                let args = ssh_command_args(&ssh.destination, &script, None);
                run_output(&self.config.ssh_executable, &args, None).await?;
            } else {
                self.terminate_managed_process(run_id, process_id, &base_url, false, None)
                    .await?;
                if !tokio::fs::try_exists(path).await? {
                    return Ok(());
                }
                let common_dir = run_output(
                    &self.config.git_executable,
                    &[
                        "-C".into(),
                        path.as_os_str().to_owned(),
                        "rev-parse".into(),
                        "--path-format=absolute".into(),
                        "--git-common-dir".into(),
                    ],
                    None,
                )
                .await?;
                let mut args = vec![
                    "--git-dir".into(),
                    common_dir.trim().into(),
                    "worktree".into(),
                    "remove".into(),
                ];
                if force {
                    args.push("--force".into());
                }
                args.extend([OsString::from("--"), path.as_os_str().to_owned()]);
                run_output(&self.config.git_executable, &args, None).await?;
            }
            Ok(())
        }
        .await;
        deletion?;
        self.registry.complete_deletion(run_id).await?;
        Ok(())
    }

    async fn deletion_pending(&self, run_id: &str) -> bool {
        self.registry.deletion_pending(run_id).await
    }

    async fn deletion_in_progress(&self, run_id: &str) -> bool {
        self.registry.deletion_in_progress(run_id).await
    }

    async fn finalize_deletion(&self, run_id: &str) -> Result<()> {
        self.registry.finalize_deletion(run_id).await
    }
}

fn stable_hash(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes())
        .simple()
        .to_string()
}

fn remote_port_for(value: &str) -> u16 {
    let port_hash = stable_hash(value);
    30_000 + u16::from_str_radix(&port_hash[..4], 16).unwrap_or(0) % 20_000
}

fn remote_port_candidates(value: &str, preferred: Option<u16>) -> Vec<u16> {
    let mut ports = Vec::with_capacity(16);
    if let Some(preferred) = preferred {
        ports.push(preferred);
    }
    for attempt in 0..16 {
        let key = if attempt == 0 {
            value.to_string()
        } else {
            format!("{value}:{attempt}")
        };
        let port = remote_port_for(&key);
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

fn parse_remote_server_start(output: &str) -> Result<RemoteServer> {
    let output = output
        .lines()
        .next_back()
        .map(str::trim)
        .unwrap_or_default();
    let mut parts = output.splitn(3, ':');
    let state = parts.next().unwrap_or_default();
    let port = parts
        .next()
        .ok_or_else(|| {
            Error::InvalidResponse(
                "remote OpenCode server returned an invalid startup result".into(),
            )
        })?
        .parse::<u16>()
        .map_err(|_| {
            Error::InvalidResponse("remote OpenCode server returned an invalid port".into())
        })?;
    let password = parts
        .next()
        .filter(|password| !password.is_empty())
        .ok_or_else(|| {
            Error::InvalidResponse(
                "remote OpenCode server returned no authentication password".into(),
            )
        })?;
    let start = match state {
        "started" => RemoteServerStart::Started,
        "reused" => RemoteServerStart::Reused,
        _ => {
            return Err(Error::InvalidResponse(
                "remote OpenCode server returned an invalid startup result".into(),
            ));
        },
    };
    Ok(RemoteServer {
        port,
        start,
        password: password.into(),
    })
}

fn native_server_password(session: &BackendSession) -> Option<&str> {
    let BackendSession::Native {
        server_password, ..
    } = session
    else {
        return None;
    };
    server_password.as_deref()
}

fn parse_model(model: &str) -> Option<(&str, &str)> {
    let (provider, model) = model.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

fn prompt_body(agent: &str, text: &str, model: Option<&str>, message_id: Option<&str>) -> Value {
    let mut body = json!({
        "parts": [{"type": "text", "text": text}],
    });
    if agent != "opencode" {
        body["agent"] = Value::String(agent.into());
    }
    if let Some(model) = model.and_then(parse_model) {
        body["model"] = json!({"providerID": model.0, "modelID": model.1});
    }
    if let Some(message_id) = message_id {
        body["messageID"] = Value::String(message_id.into());
    }
    body
}

fn has_message(value: &Value, message_id: &str) -> bool {
    value
        .as_array()
        .or_else(|| value.get("data").and_then(Value::as_array))
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                find_string(message, &["id", "messageID", "messageId"])
                    .or_else(|| {
                        message
                            .get("info")
                            .and_then(|info| find_string(info, &["id", "messageID", "messageId"]))
                    })
                    .is_some_and(|id| id == message_id)
            })
        })
}

fn initial_prompt_definitively_rejected(error: &Error) -> bool {
    matches!(
        error,
        Error::HttpStatus { status, .. }
            if (400..500).contains(status) && !matches!(status, 408 | 409 | 425 | 429)
    )
}

fn available_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    Ok(listener.local_addr()?.port())
}

#[cfg(unix)]
async fn managed_process_matches(
    process_id: u32,
    executable: &Path,
    base_url: &Url,
    remote: bool,
) -> Result<bool> {
    let output = Command::new("ps")
        .args(["-p", &process_id.to_string(), "-o", "args="])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(command_matches_session(
        &String::from_utf8_lossy(&output.stdout),
        executable,
        base_url,
        remote,
    ))
}

#[cfg(windows)]
async fn managed_process_matches(
    process_id: u32,
    executable: &Path,
    _base_url: &Url,
    _remote: bool,
) -> Result<bool> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {process_id}"), "/FO", "CSV", "/NH"])
        .output()
        .await?;
    let expected = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Ok(output.status.success()
        && !expected.is_empty()
        && String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains(&expected))
}

#[cfg(unix)]
fn command_matches_session(command: &str, executable: &Path, base_url: &Url, remote: bool) -> bool {
    let Some(executable) = executable.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(port) = base_url.port_or_known_default() else {
        return false;
    };
    let port_signature = if remote {
        format!("{port}:127.0.0.1:")
    } else {
        format!("--port {port}")
    };
    command.contains(executable) && command.contains(&port_signature)
}

#[cfg(unix)]
async fn process_running(process_id: u32) -> Result<bool> {
    let probe = run_output(
        Path::new("kill"),
        &[OsString::from("-0"), process_id.to_string().into()],
        None,
    )
    .await;
    match probe {
        Ok(_) => Ok(true),
        Err(Error::CommandFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
async fn terminate_process(process_id: u32) -> Result<bool> {
    if !process_running(process_id).await? {
        return Ok(false);
    }
    run_output(
        Path::new("kill"),
        &[OsString::from("-TERM"), process_id.to_string().into()],
        None,
    )
    .await?;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if !process_running(process_id).await? {
            return Ok(true);
        }
    }
    run_output(
        Path::new("kill"),
        &[OsString::from("-KILL"), process_id.to_string().into()],
        None,
    )
    .await?;
    for _ in 0..20 {
        sleep(Duration::from_millis(100)).await;
        if !process_running(process_id).await? {
            return Ok(true);
        }
    }
    Err(Error::Disconnected(format!(
        "managed process {process_id} did not exit"
    )))
}

#[cfg(windows)]
async fn terminate_process(process_id: u32) -> Result<bool> {
    match run_output(
        Path::new("taskkill"),
        &[
            OsString::from("/PID"),
            process_id.to_string().into(),
            OsString::from("/T"),
            OsString::from("/F"),
        ],
        None,
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(Error::CommandFailed { .. }) => Ok(false),
        Err(error) => Err(error),
    }
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
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ConnectionAttempts=1".into(),
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

fn ssh_tunnel_args(destination: &str, forwarding: &str) -> Vec<OsString> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ConnectTimeout=3".into(),
        "-o".into(),
        "ConnectionAttempts=1".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=4".into(),
        "-N".into(),
        "-L".into(),
        forwarding.into(),
        "--".into(),
        destination.into(),
    ]
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

fn remote_server_start_script(
    runtime_path: &Path,
    workspace_path: &str,
    remote_port: u16,
) -> String {
    let runtime = remote_path_expression(runtime_path);
    let workspace = remote_path_expression(Path::new(workspace_path));
    format!(
        "umask 077; runtime={runtime}; workspace={workspace}; mkdir -p \"$runtime\" && \
         pidfile=\"$runtime/opencode.pid\" && startfile=\"$runtime/opencode.start\" && \
         portfile=\"$runtime/opencode.port\" && passwordfile=\"$runtime/opencode.password\" && \
         logfile=\"$runtime/opencode.log\" && \
         if [ -s \"$pidfile\" ] && [ -s \"$startfile\" ] && [ -s \"$portfile\" ] && [ -s \"$passwordfile\" ]; then \
         pid=$(cat \"$pidfile\"); owned_port=$(cat \"$portfile\"); password=$(cat \"$passwordfile\"); \
         case \"$pid\" in *[!0-9]*|'') ;; *) \
         command=$(ps -p \"$pid\" -o args= 2>/dev/null || true); \
         actual_start=$(ps -p \"$pid\" -o lstart= 2>/dev/null || true); expected_start=$(cat \"$startfile\"); \
         case \"$owned_port\" in *[!0-9]*|'') ;; *) case \"$command\" in *opencode*serve*\"--port $owned_port\"*) \
         if [ -n \"$actual_start\" ] && [ \"$actual_start\" = \"$expected_start\" ]; then \
         printf 'reused:%s:%s\\n' \"$owned_port\" \"$password\"; exit 0; fi ;; esac ;; esac ;; esac; fi && \
         rm -f \"$pidfile\" \"$startfile\" \"$portfile\"; cd \"$workspace\" || exit 1; \
         if [ -s \"$passwordfile\" ]; then password=$(cat \"$passwordfile\"); else \
         password=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \\n'); \
         if [ -z \"$password\" ]; then exit 1; fi; \
         printf '%s\\n' \"$password\" >\"$passwordfile.tmp.$$\" && \
         mv \"$passwordfile.tmp.$$\" \"$passwordfile\" || exit 1; fi; \
         OPENCODE_SERVER_USERNAME=opencode OPENCODE_SERVER_PASSWORD=\"$password\" \
         nohup opencode serve --hostname 127.0.0.1 --port {remote_port} \
         >\"$logfile\" 2>&1 </dev/null & pid=$!; \
         sleep 1; if ! kill -0 \"$pid\" 2>/dev/null; then \
         cat \"$logfile\" >&2; exit 1; fi; \
         started_at=$(ps -p \"$pid\" -o lstart= 2>/dev/null || true); \
         if [ -z \"$started_at\" ]; then kill \"$pid\" 2>/dev/null || true; exit 1; fi; \
         if ! {{ printf '%s\\n' \"$pid\" >\"$pidfile.tmp.$$\" && \
         printf '%s\\n' \"$started_at\" >\"$startfile.tmp.$$\" && \
         printf '%s\\n' {remote_port} >\"$portfile.tmp.$$\" && \
         mv \"$pidfile.tmp.$$\" \"$pidfile\" && mv \"$startfile.tmp.$$\" \"$startfile\" && \
         mv \"$portfile.tmp.$$\" \"$portfile\"; }}; then \
         kill \"$pid\" 2>/dev/null || true; \
         rm -f \"$pidfile.tmp.$$\" \"$startfile.tmp.$$\" \"$portfile.tmp.$$\"; exit 1; fi; \
         printf 'started:%s:%s\\n' {remote_port} \"$password\""
    )
}

fn remote_server_stop_script(runtime_path: &Path, remote_port: u16) -> String {
    let runtime = remote_path_expression(runtime_path);
    format!(
        "runtime={runtime}; pidfile=\"$runtime/opencode.pid\"; startfile=\"$runtime/opencode.start\"; \
         portfile=\"$runtime/opencode.port\"; \
         if [ ! -s \"$pidfile\" ] || [ ! -s \"$startfile\" ] || [ ! -s \"$portfile\" ]; then exit 0; fi; \
         pid=$(cat \"$pidfile\"); owned_port=$(cat \"$portfile\"); \
         case \"$pid:$owned_port\" in *[!0-9:]*) exit 0 ;; esac; \
         if [ \"$owned_port\" != {remote_port} ]; then exit 0; fi; \
         command=$(ps -p \"$pid\" -o args= 2>/dev/null || true); \
         actual_start=$(ps -p \"$pid\" -o lstart= 2>/dev/null || true); expected_start=$(cat \"$startfile\"); \
         case \"$command\" in *opencode*serve*\"--port $owned_port\"*) \
         if [ -z \"$actual_start\" ] || [ \"$actual_start\" != \"$expected_start\" ]; then exit 0; fi; \
         kill \"$pid\" 2>/dev/null || true; \
         count=0; while kill -0 \"$pid\" 2>/dev/null && [ \"$count\" -lt 10 ]; do \
         sleep 1; count=$((count + 1)); done; \
         if kill -0 \"$pid\" 2>/dev/null; then kill -9 \"$pid\" 2>/dev/null || true; fi ;; esac; \
         rm -f \"$pidfile\" \"$startfile\" \"$portfile\""
    )
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

fn remote_metrics_script() -> &'static str {
    "os=$(uname -s); \
     if [ \"$os\" = Linux ]; then \
       read _ u1 n1 s1 i1 w1 q1 z1 t1 _ < /proc/stat; total1=$((u1+n1+s1+i1+w1+q1+z1+t1)); idle1=$((i1+w1)); \
       sleep 1; read _ u2 n2 s2 i2 w2 q2 z2 t2 _ < /proc/stat; total2=$((u2+n2+s2+i2+w2+q2+z2+t2)); idle2=$((i2+w2)); \
       mem_total=0; mem_available=0; while read key value _; do case \"$key\" in MemTotal:) mem_total=$value;; MemAvailable:) mem_available=$value;; esac; done < /proc/meminfo; \
       awk -v total=$((total2-total1)) -v idle=$((idle2-idle1)) -v mt=$mem_total -v ma=$mem_available 'BEGIN { if (total <= 0 || mt <= 0) exit 1; printf \"cpu=%.2f\\nmemory=%.2f\\n\", 100*(total-idle)/total, 100*(mt-ma)/mt }'; \
     elif [ \"$os\" = Darwin ]; then \
       cpu=$(LC_ALL=C top -l 2 -n 0 -s 1 | awk '/CPU usage/ { idle=$7 } END { gsub(/%/, \"\", idle); if (idle == \"\") exit 1; printf \"%.2f\", 100-idle }') || exit 1; \
       total=$(sysctl -n hw.memsize) || exit 1; page=$(sysctl -n hw.pagesize) || exit 1; \
       memory=$(vm_stat | awk -v total=\"$total\" -v page=\"$page\" '/Pages active/ { gsub(/\\./, \"\", $3); active=$3 } /Pages wired down/ { gsub(/\\./, \"\", $4); wired=$4 } /Pages occupied by compressor/ { gsub(/\\./, \"\", $5); compressed=$5 } END { if (total <= 0) exit 1; printf \"%.2f\", 100*(active+wired+compressed)*page/total }') || exit 1; \
       printf 'cpu=%s\\nmemory=%s\\n' \"$cpu\" \"$memory\"; \
     else printf 'agent-launcher: unsupported metrics operating system %s\\n' \"$os\" >&2; exit 2; fi"
}

fn parse_remote_metrics(output: &str) -> Result<RemoteMetrics> {
    let value = |key: &str| {
        output.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|value| value.strip_prefix('='))
                .and_then(|value| value.trim().parse::<f32>().ok())
        })
    };
    let cpu = value("cpu")
        .filter(|value| value.is_finite())
        .ok_or_else(|| Error::InvalidResponse("remote metrics response has no CPU value".into()))?;
    let memory = value("memory")
        .filter(|value| value.is_finite())
        .ok_or_else(|| {
            Error::InvalidResponse("remote metrics response has no memory value".into())
        })?;
    Ok(RemoteMetrics {
        cpu: cpu.clamp(0.0, 100.0),
        memory: memory.clamp(0.0, 100.0),
    })
}

fn remote_prerequisite_missing(error: &Error) -> bool {
    matches!(
        error,
        Error::CommandFailed { stderr, .. }
            if stderr.contains("agent-launcher: missing required remote tool")
                || stderr.contains("agent-launcher: remote ps does not support")
                || stderr.contains("agent-launcher: remote xargs does not support")
    )
}

fn remote_port_conflict(error: &Error) -> bool {
    let Error::CommandFailed { stderr, .. } = error else {
        return false;
    };
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("eaddrinuse")
        || stderr.contains("address already in use")
        || (stderr.contains("port") && stderr.contains("already in use"))
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

    fn test_ssh_target(
        id: &str,
        destination: &str,
        max_active_runs: Option<usize>,
    ) -> NativeSshConfig {
        NativeSshConfig {
            id: id.into(),
            name: id.into(),
            destination: destination.into(),
            workspace_root: "/srv/agent-launcher".into(),
            max_active_runs,
            wake: None,
        }
    }

    fn test_remote_record(id: &str, target_id: Option<&str>, destination: &str) -> RunRecord {
        let now = Utc::now();
        RunRecord {
            summary: RunSummary {
                id: id.into(),
                issue_key: format!("issue-{id}"),
                workspace: Some(WorkspaceRef {
                    backend: BackendKind::Native,
                    id: format!("workspace-{id}"),
                    host: Some(destination.into()),
                    path: Some(format!("/srv/workspaces/{id}").into()),
                    branch: format!("agent/{id}"),
                }),
                agent: "opencode".into(),
                state: RunState::Running,
                message: None,
                session_id: Some(format!("session-{id}")),
                started_at: now,
                updated_at: now,
            },
            session: BackendSession::Native {
                base_url: "http://127.0.0.1:31234/".into(),
                remote: true,
                target_id: target_id.map(str::to_owned),
                remote_workspace_path: Some(format!("/srv/workspaces/{id}")),
                remote_port: Some(38123),
                server_password: Some("secret".into()),
                process_id: None,
                initial_prompt: None,
                pending_permission_id: None,
                pending_question_id: None,
                pending_question_count: 0,
                pending_question_prompt: None,
                last_message_id: None,
            },
            deletion: None,
        }
    }

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
        assert_eq!(args[8], OsString::from("--"));
        assert_eq!(args[9], OsString::from("dev@example"));
        assert_eq!(
            args[10],
            OsString::from("sh -lc 'cd '\\''/tmp/a'\\''\\'\\'''\\''b'\\'' && opencode'")
        );
    }

    #[test]
    fn builds_a_forward_only_reconnectable_ssh_tunnel() {
        assert_eq!(
            ssh_tunnel_args("dev@example", "41234:127.0.0.1:38123"),
            os_args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ConnectTimeout=3",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
                "-N",
                "-L",
                "41234:127.0.0.1:38123",
                "--",
                "dev@example",
            ])
        );
    }

    #[test]
    fn remote_server_scripts_own_only_the_expected_process() {
        let start = remote_server_start_script(
            Path::new("~/.local/share/agent launcher/runtime/workspace-1"),
            "~/.local/share/agent launcher/work tree",
            38123,
        );
        assert!(start.contains("nohup opencode serve --hostname 127.0.0.1 --port 38123"));
        assert!(start.contains("OPENCODE_SERVER_PASSWORD=\"$password\""));
        assert!(start.contains("od -An -N32 -tx1 /dev/urandom"));
        assert!(start.contains("</dev/null & pid=$!;"));
        assert!(start.contains("$HOME"));
        assert!(start.contains("'.local/share/agent launcher/work tree'"));
        #[cfg(unix)]
        assert!(
            std::process::Command::new("sh")
                .args(["-n", "-c", &start])
                .status()
                .expect("sh should be available")
                .success()
        );

        let stop = remote_server_stop_script(
            Path::new("~/.local/share/agent launcher/runtime/workspace-1"),
            38123,
        );
        assert!(stop.contains("*opencode*serve*\"--port $owned_port\"*"));
        assert!(stop.contains("kill -9 \"$pid\""));
        #[cfg(unix)]
        assert!(
            std::process::Command::new("sh")
                .args(["-n", "-c", &stop])
                .status()
                .expect("sh should be available")
                .success()
        );
    }

    #[test]
    fn remote_ports_are_stable_per_worktree() {
        assert_eq!(
            remote_port_for("workspace-1"),
            remote_port_for("workspace-1")
        );
        assert!((30_000..50_000).contains(&remote_port_for("workspace-1")));
        let candidates = remote_port_candidates("workspace-1", Some(49_999));
        assert_eq!(candidates.first(), Some(&49_999));
        assert!(
            candidates
                .iter()
                .all(|port| (30_000..50_000).contains(port))
        );
        assert!(candidates.windows(2).all(|ports| ports[0] != ports[1]));
    }

    #[test]
    fn parses_authenticated_remote_server_identity() {
        let started = parse_remote_server_start("login banner\nstarted:38123:secret-value\n")
            .expect("startup identity should parse");
        assert_eq!(started.port, 38123);
        assert_eq!(started.start, RemoteServerStart::Started);
        assert_eq!(started.password, "secret-value");

        let reused = parse_remote_server_start("reused:40123:existing-secret")
            .expect("reused identity should parse");
        assert_eq!(reused.port, 40123);
        assert_eq!(reused.start, RemoteServerStart::Reused);
        assert!(parse_remote_server_start("started:38123:").is_err());
    }

    #[test]
    fn parses_and_clamps_remote_load_metrics() {
        let metrics =
            parse_remote_metrics("cpu=42.25\nmemory=73.5\n").expect("metrics should parse");
        assert_eq!(metrics.cpu, 42.25);
        assert_eq!(metrics.memory, 73.5);

        let metrics = parse_remote_metrics("cpu=102\nmemory=-1\n").expect("metrics should clamp");
        assert_eq!(metrics.cpu, 100.0);
        assert_eq!(metrics.memory, 0.0);
        assert!(parse_remote_metrics("cpu=10\n").is_err());
    }

    #[test]
    fn least_loaded_placement_prefers_reachable_then_active_count_then_load() {
        let a = test_ssh_target("a", "a", None);
        let b = test_ssh_target("b", "b", None);
        let c = test_ssh_target("c", "c", None);
        let sleeping = test_ssh_target("sleeping", "sleeping", None);
        let mut candidates = [
            (0, &a, 2, true, 10.0),
            (1, &b, 1, true, 80.0),
            (2, &c, 1, true, 20.0),
            (3, &sleeping, 0, false, f32::INFINITY),
        ];
        candidates.sort_by(compare_target_candidates);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.1.id.as_str())
                .collect::<Vec<_>>(),
            ["c", "b", "a", "sleeping"]
        );
    }

    #[tokio::test]
    async fn persisted_target_id_cannot_be_repointed_to_another_host() {
        let path = std::env::temp_dir().join(format!("native-target-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        let backend = NativeBackend::new(
            NativeConfig {
                ssh_targets: vec![test_ssh_target("builder", "new-host", None)],
                ..NativeConfig::default()
            },
            registry,
        );
        let record = test_remote_record("run", Some("builder"), "old-host");
        let error = backend
            .target_for_record(&record)
            .expect_err("repointed target must be rejected");
        assert!(error.to_string().contains("restore the original host"));
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn persisted_target_workspace_root_cannot_be_repointed() {
        let path = std::env::temp_dir().join(format!("native-target-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        let backend = NativeBackend::new(
            NativeConfig {
                ssh_targets: vec![NativeSshConfig {
                    workspace_root: "/new/root".into(),
                    ..test_ssh_target("builder", "builder", None)
                }],
                ..NativeConfig::default()
            },
            registry,
        );
        let mut record = test_remote_record("run", Some("builder"), "builder");
        record.summary.workspace.as_mut().unwrap().path =
            Some("/old/root/workspaces/repository/run".into());
        let error = backend
            .target_for_record(&record)
            .expect_err("repointed workspace root must be rejected");
        assert!(error.to_string().contains("workspace_root changed"));
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn explicit_target_capacity_is_hard_for_resumable_runs() {
        let path = std::env::temp_dir().join(format!("native-target-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        registry
            .insert(test_remote_record("run", Some("builder"), "builder"))
            .await
            .unwrap();
        let backend = NativeBackend::new(
            NativeConfig {
                ssh_targets: vec![test_ssh_target("builder", "builder", Some(1))],
                ..NativeConfig::default()
            },
            registry,
        );
        assert!(matches!(
            backend
                .ensure_capacity(&backend.config.ssh_targets[0])
                .await,
            Err(Error::ComputeTargetAtCapacity {
                active: 1,
                maximum: 1,
                ..
            })
        ));
        let _ = tokio::fs::remove_file(path).await;
    }

    #[test]
    fn classifies_only_actionable_remote_startup_failures() {
        let port_conflict = Error::CommandFailed {
            program: "ssh host".into(),
            status: "1".into(),
            stderr: "listen EADDRINUSE: address already in use".into(),
        };
        assert!(remote_port_conflict(&port_conflict));
        assert!(!remote_prerequisite_missing(&port_conflict));

        let missing_tool = Error::CommandFailed {
            program: "ssh host".into(),
            status: "127".into(),
            stderr: "agent-launcher: missing required remote tool nohup".into(),
        };
        assert!(remote_prerequisite_missing(&missing_tool));
        assert!(!remote_port_conflict(&missing_tool));
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

    #[cfg(unix)]
    #[test]
    fn recognizes_only_processes_owned_by_the_persisted_session() {
        let local = Url::parse("http://127.0.0.1:31234/").expect("URL should parse");
        assert!(command_matches_session(
            "/opt/bin/opencode serve --hostname 127.0.0.1 --port 31234",
            Path::new("/opt/bin/opencode"),
            &local,
            false,
        ));
        assert!(!command_matches_session(
            "/usr/bin/python --port 31234",
            Path::new("/opt/bin/opencode"),
            &local,
            false,
        ));

        let remote = Url::parse("http://127.0.0.1:41234/").expect("URL should parse");
        assert!(command_matches_session(
            "ssh -L 41234:127.0.0.1:38123 host opencode serve",
            Path::new("ssh"),
            &remote,
            true,
        ));
        assert!(!command_matches_session(
            "ssh host",
            Path::new("ssh"),
            &remote,
            true,
        ));
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
        assert!(has_message(&value, "msg_1"));
        assert!(has_message(&json!({"data": [{"id": "msg_1"}]}), "msg_1"));
        assert!(!has_message(&value, "msg_unrelated"));
        assert!(!has_message(&json!([]), "msg_1"));
    }

    #[test]
    fn retries_ambiguous_initial_prompt_delivery_errors() {
        assert!(!initial_prompt_definitively_rejected(
            &Error::CommandTimedOut {
                program: "prompt request".into(),
                timeout: Duration::from_secs(30),
            }
        ));
        assert!(!initial_prompt_definitively_rejected(&Error::HttpStatus {
            status: 408,
            body: "timeout".into(),
        }));
        assert!(initial_prompt_definitively_rejected(&Error::HttpStatus {
            status: 422,
            body: "invalid prompt".into(),
        }));
    }

    #[test]
    fn validates_explicit_permission_replies() {
        assert_eq!(permission_reply(" ONCE ").expect("once is valid"), "once");
        assert!(permission_reply("continue").is_err());
    }

    #[test]
    fn omits_only_the_default_opencode_agent_from_prompts() {
        let default = prompt_body("opencode", "fix it", Some("openai/gpt-5"), None);
        assert!(default.get("agent").is_none());
        assert_eq!(default["model"]["providerID"], "openai");

        let custom = prompt_body("reviewer", "check it", None, Some("msg_initial"));
        assert_eq!(custom["agent"], "reviewer");
        assert_eq!(custom["messageID"], "msg_initial");
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
