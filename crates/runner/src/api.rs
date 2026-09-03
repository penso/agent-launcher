use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use agent_launcher_core::{BackendKind, Issue, Repository, RunSummary};
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
    Remote,
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
}

#[derive(Clone, Debug)]
pub struct DispatchRequest {
    pub repository: Repository,
    pub issue: Issue,
    pub prompt: String,
    pub agent: String,
    pub branch: Option<String>,
    pub workspace_name: Option<String>,
    pub base_branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl DispatchRequest {
    pub fn validate(&self) -> Result<()> {
        if self.prompt.trim().is_empty() {
            return Err(Error::InvalidRequest("prompt cannot be empty".into()));
        }
        if self.agent.trim().is_empty() {
            return Err(Error::InvalidRequest("agent cannot be empty".into()));
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
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{program} exited with status {status}: {stderr}")]
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

pub type Result<T> = std::result::Result<T, Error>;

#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> BackendCapabilities;
    async fn owns_run(&self, run_id: &str) -> bool;

    async fn detect(&self, repository: &Repository) -> Result<BackendDetection>;
    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult>;
    async fn refresh(&self, run_id: &str) -> Result<StatusResult>;
    async fn send_input(&self, run_id: &str, text: &str) -> Result<()>;
    async fn stop(&self, run_id: &str) -> Result<()>;
    async fn open(&self, run_id: &str) -> Result<OpenResult>;
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
                }),
            }
        }
        detections
    }

    pub async fn dispatch(
        &self,
        backend: BackendKind,
        request: DispatchRequest,
    ) -> Result<DispatchResult> {
        self.backend(backend)?.dispatch(request).await
    }

    pub async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let backend = self.backend_for_run(run_id).await?;
        backend.refresh(run_id).await
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

    async fn backend_for_run(&self, run_id: &str) -> Result<&Arc<dyn Backend>> {
        for backend in &self.backends {
            if backend.owns_run(run_id).await {
                return Ok(backend);
            }
        }
        Err(Error::RunNotFound(run_id.to_string()))
    }
}
