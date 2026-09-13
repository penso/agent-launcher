use agent_launcher_core::IssueKey;
use thiserror::Error;

/// Errors produced while starting or controlling the runtime.
#[derive(Debug, Error)]
pub enum Error {
    #[error("issue deletion rejected: {0}")]
    DeleteIssueRejected(&'static str),
    #[error(
        "issue was deleted at the source, but cache removal failed; reconciliation scheduled: {0}"
    )]
    DeleteIssueCache(#[source] agent_launcher_store::StoreError),
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

    #[error("pull requests must use Review, not Dispatch: {0:?}")]
    DispatchRequiresIssue(IssueKey),

    #[error("Review requires a pull request: {0:?}")]
    ReviewRequiresPullRequest(IssueKey),

    #[error("prompt profile was not found: {0}")]
    PromptProfileNotFound(String),

    #[error("could not read prompt profile `{profile}` at {path}: {source}")]
    ReadPromptProfile {
        profile: String,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("prompt profile `{0}` is empty")]
    EmptyPromptProfile(String),

    #[error("prompt profile `{0}` rendered an empty prompt for this issue")]
    EmptyRenderedPrompt(String),

    #[error("could not render prompt profile `{profile}`: {source}")]
    RenderPromptProfile {
        profile: String,
        #[source]
        source: minijinja::Error,
    },

    #[error("run was not found: {0}")]
    RunNotFound(String),

    #[error("run `{0}` is already active; open the existing workspace instead")]
    RunAlreadyActive(String),

    #[error("existing native run `{0}` must be resumed or deleted before launching again")]
    NativeRunRequiresRecovery(String),

    #[error("run `{run_id}` failed to start: {message}")]
    LaunchFailed { run_id: String, message: String },

    #[error("run `{0}` has no workspace")]
    WorkspaceUnavailable(String),

    #[error("worktree for run `{run_id}` is still used by active run `{active_run_id}`")]
    WorkspaceInUse {
        run_id: String,
        active_run_id: String,
    },

    #[error(
        "worktree `{0}` changed after it was inspected; review the updated warnings and confirm again"
    )]
    WorktreeChanged(String),

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
