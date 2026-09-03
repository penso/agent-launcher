use std::{fmt, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Superset,
    Native,
    Herdr,
    Conductor,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Superset => "superset",
            Self::Native => "native",
            Self::Herdr => "herdr",
            Self::Conductor => "conductor",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceRef {
    pub backend: BackendKind,
    pub id: String,
    pub host: Option<String>,
    pub path: Option<PathBuf>,
    pub branch: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Provisioning,
    Starting,
    Running,
    NeedsInput,
    Idle,
    Completed,
    Failed,
    Cancelled,
    Disconnected,
}

impl RunState {
    pub const fn needs_attention(self) -> bool {
        matches!(self, Self::NeedsInput | Self::Failed | Self::Disconnected)
    }

    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Provisioning | Self::Starting | Self::Running | Self::NeedsInput | Self::Idle
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub issue_key: String,
    pub workspace: Option<WorkspaceRef>,
    pub agent: String,
    pub state: RunState,
    pub message: Option<String>,
    pub session_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
