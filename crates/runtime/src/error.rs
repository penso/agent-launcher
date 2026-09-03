use agent_launcher_core::IssueKey;
use thiserror::Error;

/// Errors produced while starting or controlling the runtime.
#[derive(Debug, Error)]
pub enum Error {
    #[error("store error: {0}")]
    Store(#[from] agent_launcher_store::StoreError),

    #[error("runner error: {0}")]
    Runner(#[from] agent_launcher_runner::Error),

    #[error("issue source `{source_name}` failed: {source}")]
    IssueSource {
        source_name: String,
        #[source]
        source: agent_launcher_issues::Error,
    },

    #[error("invalid checkpoint for issue source `{source_name}`: {source}")]
    Checkpoint {
        source_name: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("issue was not found: {0:?}")]
    IssueNotFound(IssueKey),

    #[error("no available backend matches {0}")]
    BackendUnavailable(String),

    #[error("runtime command channel is closed")]
    CommandChannelClosed,

    #[error("runtime command acknowledgment was dropped")]
    CommandAcknowledgmentDropped,

    #[error("refresh failed: {0}")]
    RefreshFailed(String),

    #[error("runtime task failed: {0}")]
    RuntimeTask(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;
