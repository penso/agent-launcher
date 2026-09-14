use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use agent_launcher_core::{
    BackendKind, ComputeTargetStatus, Issue, Repository, RunSummary, WorktreeInspection,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Detect,
    Dispatch,
    Refresh,
    SendInput,
    Stop,
    Open,
    DeleteWorktree,
    Remote,
    Away,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendCapabilities {
    supported: BTreeSet<Capability>,
}

impl BackendCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            supported: capabilities.into_iter().collect(),
        }
    }

    pub fn supports(&self, capability: Capability) -> bool {
        self.supported.contains(&capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.supported.iter().copied()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackendDetection {
    pub backend: BackendKind,
    pub available: bool,
    #[serde(default)]
    pub manager_running: bool,
    pub capabilities: BackendCapabilities,
    pub message: Option<String>,
    #[serde(default)]
    pub compute_targets: Vec<ComputeTargetStatus>,
}

#[derive(Clone, Debug)]
pub struct DispatchRequest {
    pub private_fork: Option<agent_launcher_core::PrivateAdvisoryFork>,
    pub repository: Repository,
    pub issue: Issue,
    pub prompt: String,
    pub agent: String,
    pub branch: Option<String>,
    pub workspace_name: Option<String>,
    pub base_branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub target: Option<String>,
}

impl DispatchRequest {
    pub fn validate(&self) -> Result<()> {
        crate::private::validate_request(self)?;
        if self.prompt.trim().is_empty() {
            return Err(Error::InvalidRequest("prompt cannot be empty".into()));
        }
        for (name, value) in [
            ("agent", Some(self.agent.as_str())),
            ("model", self.model.as_deref()),
        ] {
            if let Some(value) = value
                && (value.trim().is_empty()
                    || value.trim_start().starts_with('-')
                    || value.chars().any(char::is_control))
            {
                return Err(Error::InvalidRequest(format!(
                    "{name} must be nonblank, contain no control characters, and not start with '-'"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DispatchResult {
    pub run: RunSummary,
    pub capabilities: BackendCapabilities,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusResult {
    pub run: RunSummary,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenResult {
    pub uri: Url,
    pub launched: bool,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Away worker was not started: {0}")]
    AwayNotStarted(String),
    #[error("private security checkout safety check failed")]
    PrivateSecurity,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{} ({program} exited with status {status})", if stderr.is_empty() { "command failed without stderr" } else { stderr })]
    CommandFailed {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("{program} timed out after {timeout:?}")]
    CommandTimedOut { program: String, timeout: Duration },
    #[error("required executable is unavailable: {0}")]
    ExecutableNotFound(String),
    #[error("backend {0} is unavailable")]
    BackendUnavailable(BackendKind),
    #[error("compute target is not configured: {0}")]
    ComputeTargetNotFound(String),
    #[error("compute target {target} is full ({active}/{maximum} active runs)")]
    ComputeTargetAtCapacity {
        target: String,
        active: usize,
        maximum: usize,
    },
    #[error("no configured compute target has available capacity")]
    NoComputeTargetCapacity,
    #[error("no configured compute target is currently available")]
    NoComputeTargetAvailable,
    #[error("backend {backend} does not support {capability:?}")]
    UnsupportedCapability {
        backend: BackendKind,
        capability: Capability,
    },
    #[error("no matching Superset project was found for {0}")]
    ProjectNotFound(PathBuf),
    #[error("required Superset agent is unavailable: {0}")]
    SupersetAgentUnavailable(String),
    #[error("no Conductor project remote matches {0}")]
    ConductorProjectNotFound(PathBuf),
    #[error(
        "Conductor bearer token is not configured; set ConductorConfig::bearer_token, CONDUCTOR_API_KEY, or CONDUCTOR_API_TOKEN"
    )]
    ConductorTokenUnavailable,
    #[error(
        "Conductor created workspace {workspace_id} and session {session_id}, but the initial message failed: {message}"
    )]
    ConductorInitialMessage {
        workspace_id: String,
        session_id: String,
        message: String,
    },
    #[error("run not found: {0}")]
    RunNotFound(String),
    #[error("invalid backend response: {0}")]
    InvalidResponse(String),
    #[error("invalid dispatch request: {0}")]
    InvalidRequest(String),
    #[error("run is disconnected: {0}")]
    Disconnected(String),
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("could not determine a user data directory")]
    DataDirectoryUnavailable,
}

impl Error {
    pub(crate) fn for_private(self, confidential: bool) -> Self {
        if confidential {
            Self::PrivateSecurity
        } else {
            self
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> BackendCapabilities;
    async fn owns_run(&self, run_id: &str) -> bool;

    /// Metadata-only restart discovery; never refreshes or starts a harness.
    async fn confidential_runs(&self) -> Vec<RunSummary> {
        Vec::new()
    }

    async fn detect(&self, repository: &Repository) -> Result<BackendDetection>;
    fn verify_private_storage(&self) -> Result<()> {
        Err(Error::PrivateSecurity)
    }
    async fn verify_private_transport(&self) -> Result<()> {
        Err(Error::PrivateSecurity)
    }
    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult>;
    /// Dispatch a finite worker using the caller's persisted reservation UUID.
    async fn dispatch_away(
        &self,
        _request: DispatchRequest,
        _run_id: &str,
    ) -> Result<DispatchResult> {
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::Away,
        })
    }
    /// Refresh using process-exit evidence, not interactive harness idle state.
    async fn refresh_away(&self, _run_id: &str) -> Result<StatusResult> {
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::Away,
        })
    }
    async fn refresh(&self, run_id: &str) -> Result<StatusResult>;
    async fn send_input(&self, run_id: &str, text: &str) -> Result<()>;
    async fn stop(&self, run_id: &str) -> Result<()>;
    async fn open(&self, run_id: &str) -> Result<OpenResult>;
    async fn inspect_worktree(&self, _run_id: &str) -> Result<WorktreeInspection> {
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::DeleteWorktree,
        })
    }
    async fn delete_worktree(
        &self,
        run_id: &str,
        force: bool,
        expected: Option<&WorktreeInspection>,
    ) -> Result<()>;
    async fn deletion_pending(&self, _run_id: &str) -> bool {
        false
    }
    async fn deletion_in_progress(&self, run_id: &str) -> bool {
        self.deletion_pending(run_id).await
    }
    async fn finalize_deletion(&self, _run_id: &str) -> Result<()> {
        Ok(())
    }
}

pub struct Runner {
    backends: Vec<Arc<dyn Backend>>,
}

impl Runner {
    pub fn new(backends: impl IntoIterator<Item = Arc<dyn Backend>>) -> Self {
        Self {
            backends: backends.into_iter().collect(),
        }
    }

    pub fn backend(&self, kind: BackendKind) -> Result<&Arc<dyn Backend>> {
        self.backends
            .iter()
            .find(|backend| backend.kind() == kind)
            .ok_or(Error::BackendUnavailable(kind))
    }

    pub async fn detect(&self, repository: &Repository) -> Vec<BackendDetection> {
        let mut detections = Vec::with_capacity(self.backends.len());
        for backend in &self.backends {
            match backend.detect(repository).await {
                Ok(detection) => detections.push(detection),
                Err(error) => detections.push(BackendDetection {
                    backend: backend.kind(),
                    available: false,
                    manager_running: false,
                    capabilities: backend.capabilities(),
                    message: Some(error.to_string()),
                    compute_targets: Vec::new(),
                }),
            }
        }
        detections
    }

    /// Private summaries belong in runtime memory, never the ordinary SQLite import.
    pub async fn confidential_runs(&self) -> Vec<RunSummary> {
        let mut seen = BTreeSet::new();
        let mut runs = Vec::new();
        for backend in &self.backends {
            if !matches!(backend.kind(), BackendKind::Native | BackendKind::Herdr) {
                continue;
            }
            for mut run in backend.confidential_runs().await {
                if !run.confidential
                    || !run
                        .workspace
                        .as_ref()
                        .is_some_and(|workspace| workspace.backend == backend.kind())
                    || !self
                        .backend_for_run(&run.id)
                        .await
                        .is_ok_and(|owner| Arc::ptr_eq(owner, backend))
                    || !seen.insert(run.id.clone())
                {
                    continue;
                }
                run.message = None;
                runs.push(run);
            }
        }
        runs.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then(left.id.cmp(&right.id))
        });
        runs
    }

    pub async fn dispatch(
        &self,
        backend: BackendKind,
        request: DispatchRequest,
    ) -> Result<DispatchResult> {
        request.validate()?;
        if request.private_fork.is_some() || request.issue.security_advisory.is_some() {
            self.backend(backend)
                .and_then(|backend| backend.verify_private_storage())
                .map_err(|_| Error::PrivateSecurity)?;
        }
        crate::private::guard_dispatch(&request, backend).await?;
        if backend != BackendKind::Native && request.target.is_some() {
            return Err(Error::InvalidRequest(format!(
                "backend {backend} does not support compute target selection"
            )));
        }
        let private = request.private_fork.is_some();
        self.backend(backend)?
            .dispatch(request)
            .await
            .map_err(|error| {
                if private {
                    Error::PrivateSecurity
                } else {
                    error
                }
            })
    }

    pub async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let backend = self.backend_for_run(run_id).await?;
        backend.refresh(run_id).await
    }

    pub async fn dispatch_away(
        &self,
        backend: BackendKind,
        request: DispatchRequest,
        run_id: &str,
    ) -> Result<DispatchResult> {
        request
            .validate()
            .map_err(|error| Error::AwayNotStarted(error.to_string()))?;
        if request.private_fork.is_some() || request.issue.security_advisory.is_some() {
            return Err(Error::AwayNotStarted(Error::PrivateSecurity.to_string()));
        }
        if request.target.is_some() {
            return Err(Error::AwayNotStarted(
                "Away does not support compute targets".into(),
            ));
        }
        self.backend(backend)
            .map_err(|error| Error::AwayNotStarted(error.to_string()))?
            .dispatch_away(request, run_id)
            .await
            .map_err(|error| match error {
                Error::UnsupportedCapability { .. } => Error::AwayNotStarted(error.to_string()),
                _ => error,
            })
    }

    pub async fn refresh_away(&self, run_id: &str) -> Result<StatusResult> {
        self.backend_for_run(run_id)
            .await?
            .refresh_away(run_id)
            .await
    }

    pub async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        let backend = self.backend_for_run(run_id).await?;
        backend.send_input(run_id, text).await
    }

    pub async fn stop(&self, run_id: &str) -> Result<()> {
        let backend = self.backend_for_run(run_id).await?;
        backend.stop(run_id).await
    }

    pub async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let backend = self.backend_for_run(run_id).await?;
        backend.open(run_id).await
    }

    pub async fn inspect_worktree(&self, run_id: &str) -> Result<WorktreeInspection> {
        let backend = self.backend_for_run(run_id).await?;
        backend.inspect_worktree(run_id).await
    }

    pub async fn delete_worktree(
        &self,
        backend: BackendKind,
        run_id: &str,
        force: bool,
        expected: Option<&WorktreeInspection>,
    ) -> Result<()> {
        self.backend(backend)?
            .delete_worktree(run_id, force, expected)
            .await
    }

    pub fn supports(&self, backend: BackendKind, capability: Capability) -> Result<bool> {
        Ok(self.backend(backend)?.capabilities().supports(capability))
    }

    pub async fn deletion_pending(&self, run_id: &str) -> bool {
        for backend in &self.backends {
            if backend.deletion_pending(run_id).await {
                return true;
            }
        }
        false
    }

    pub async fn deletion_in_progress(&self, run_id: &str) -> bool {
        for backend in &self.backends {
            if backend.deletion_in_progress(run_id).await {
                return true;
            }
        }
        false
    }

    pub async fn finalize_deletion(&self, backend: BackendKind, run_id: &str) -> Result<()> {
        self.backend(backend)?.finalize_deletion(run_id).await
    }

    async fn backend_for_run(&self, run_id: &str) -> Result<&Arc<dyn Backend>> {
        for backend in &self.backends {
            if backend.owns_run(run_id).await {
                return Ok(backend);
            }
        }
        Err(Error::RunNotFound(run_id.to_string()))
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::{
        HerdrBackend, HerdrConfig, NativeBackend, NativeConfig, SessionRegistry, SupersetBackend,
        SupersetConfig,
    };

    fn request() -> DispatchRequest {
        DispatchRequest {
            private_fork: None,
            repository: Repository { root: "/nonexistent/selection-test".into(), git_dir: "/nonexistent/selection-test/.git".into(), remote: None, has_beads: false },
            issue: serde_json::from_value(serde_json::json!({
                "key": {"provider": "github", "host": "example.com", "repository": "a/b", "native_id": "1"},
                "identifier": "1", "title": "Test", "state": "open", "labels": [], "blocked_by": []
            })).unwrap(),
            prompt: "review".into(), agent: "opencode".into(), branch: None,
            workspace_name: None, base_branch: None, model: None, effort: None, target: None,
        }
    }

    #[test]
    fn malformed_agent_and_model_arguments_are_rejected() {
        for value in ["", " ", "-bad", "  --bad", "model\n", "model\0", "\tmodel"] {
            let mut request = request();
            request.model = Some(value.into());
            assert!(request.validate().is_err());
            request.model = None;
            request.agent = value.into();
            assert!(request.validate().is_err());
        }
        let mut request = request();
        request.model = Some("provider/id; $(literal)".into());
        assert!(request.validate().is_ok());
    }

    #[tokio::test]
    async fn unsupported_models_fail_before_any_backend_command_or_workspace() {
        let root =
            std::env::temp_dir().join(format!("launcher-selection-{}", uuid::Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(root.join("registry.json")))
            .await
            .unwrap();
        let herdr = HerdrBackend::new(
            HerdrConfig {
                executable: root.join("missing-herdr"),
            },
            registry.clone(),
        );
        let superset = SupersetBackend::new(
            SupersetConfig {
                executable: root.join("missing-superset"),
                ..Default::default()
            },
            registry.clone(),
        );
        let native = NativeBackend::new(
            NativeConfig {
                workspace_root: Some(root.join("workspaces")),
                ..Default::default()
            },
            registry.clone(),
        );
        let mut request = request();
        request.model = Some("sonnet".into());
        assert!(matches!(
            superset.dispatch(request.clone()).await,
            Err(Error::InvalidRequest(_))
        ));
        for kind in ["custom", "codex", "gemini"] {
            request.agent = kind.into();
            assert!(matches!(
                herdr.dispatch(request.clone()).await,
                Err(Error::InvalidRequest(_))
            ));
        }
        request.agent = "opencode".into();
        for model in [
            "sonnet",
            "/model",
            "provider/",
            "provider/  ",
            "provider /model",
        ] {
            request.model = Some(model.into());
            assert!(matches!(
                native.dispatch(request.clone()).await,
                Err(Error::InvalidRequest(_))
            ));
        }
        assert!(registry.summaries().await.is_empty());
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsafe_private_storage_stops_direct_and_runner_dispatch() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("private-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = root.join("herdr");
        std::fs::write(&executable, "#!/bin/sh\ntouch \"$0.called\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = SessionRegistry::load(Some(root.join("registry.json")))
            .await
            .unwrap();
        let backends: Vec<Arc<dyn Backend>> = vec![
            Arc::new(HerdrBackend::new(
                HerdrConfig { executable },
                registry.clone(),
            )),
            Arc::new(NativeBackend::new(
                NativeConfig {
                    workspace_root: Some(root.join("workspaces")),
                    ..Default::default()
                },
                registry.clone(),
            )),
        ];
        let mut request = request();
        request.private_fork = Some(agent_launcher_core::PrivateAdvisoryFork {
            id: 1,
            host: "example.com".into(),
            full_name: "a/private".into(),
            default_branch: "main".into(),
        });
        request.repository.remote = Some(agent_launcher_core::RepositoryRemote {
            name: "origin".into(),
            url: "https://example.com/a/private.git".into(),
            host: "example.com".into(),
            repository: "a/private".into(),
            provider: agent_launcher_core::IssueProvider::Github,
        });
        request.issue.security_advisory = Some(
            serde_json::from_value(serde_json::json!({
                "ghsa_id": "GHSA-test-test-test", "cve_id": null, "severity": null
            }))
            .unwrap(),
        );
        request.prompt = "private prompt must never be delivered".into();
        request.validate().unwrap();
        for backend in &backends {
            assert!(matches!(
                backend.verify_private_storage(),
                Err(Error::PrivateSecurity)
            ));
            assert!(matches!(
                backend.dispatch(request.clone()).await,
                Err(Error::PrivateSecurity)
            ));
        }
        let runner = Runner::new(backends);
        for kind in [BackendKind::Native, BackendKind::Herdr] {
            assert!(matches!(
                runner.dispatch(kind, request.clone()).await,
                Err(Error::PrivateSecurity)
            ));
        }
        assert!(!root.join("herdr.called").exists());
        assert!(!root.join("workspaces").exists());
        assert!(!root.join("registry.json").exists());
        assert!(registry.summaries().await.is_empty());
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
