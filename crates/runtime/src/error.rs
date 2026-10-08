use agent_launcher_core::IssueKey;
use thiserror::Error;

/// Errors produced while starting or controlling the runtime.
#[derive(Debug, Error)]
pub enum Error {
    #[error("private advisory operation rejected: {0}")]
    SecurityRejected(&'static str),
    #[error("private advisory operation failed; verify access and backend configuration")]
    SecurityFailed,
    #[error("private advisory access revoked; cached advisories are unavailable")]
    SecurityAccessRevoked,
    #[error("private launch cancelled or source access revoked")]
    SecurityCancelled,
    #[error(
        "private launch or cleanup outcome is unknown; inspect the selected harness before retrying"
    )]
    SecurityOutcomeUnknown,
    #[error("prompt editor is read-only: no prompt root configured")]
    PromptRootUnavailable,
    #[error(
        "invalid prompt name: use one nonempty path component of at most 128 bytes, without slashes or control characters"
    )]
    InvalidPromptName,
    #[error("prompt changed or already exists; reload before saving")]
    PromptConflict,
    #[error("prompt editor refuses symlinked or non-regular roots, directories, or files")]
    UnsafePromptPath,
    #[error("prompt editor is busy; retry saving")]
    PromptBusy,
    #[error("prompt profile `{0}` is read-only; change its permissions before editing")]
    ReadOnlyPromptProfile(String),
    #[error("prompt storage error: {0}")]
    PromptIo(#[source] std::io::Error),
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

    #[error("dispatch refused: {0}")]
    BackendBlocked(String),

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

impl Error {
    /// Whether the run has nothing left to open: its agent, workspace and
    /// worktree are gone, or the backend no longer knows the run.
    pub fn is_gone(&self) -> bool {
        matches!(
            self,
            Self::RunNotFound(_)
                | Self::WorkspaceUnavailable(_)
                | Self::Runner(
                    agent_launcher_runner::Error::Disconnected(_)
                        | agent_launcher_runner::Error::RunNotFound(_)
                )
        )
    }
}
