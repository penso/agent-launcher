use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    BackendKind, EventEnvelope, Issue, IssueKey, Repository, RunSummary, WorktreeDeletePreview,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub herdr_activity: crate::HerdrActivitySnapshot,
    #[serde(default)]
    pub initialized: bool,
    pub repository: Option<Repository>,
    pub issues: Vec<Issue>,
    pub runs: Vec<RunSummary>,
    pub run_events: HashMap<String, Vec<EventEnvelope>>,
    pub sources: Vec<SourceStatus>,
    pub backends: Vec<BackendStatus>,
    #[serde(default)]
    pub compute_targets: Vec<ComputeTargetStatus>,
    pub selected_backend: Option<BackendKind>,
    pub selected_agent: String,
    #[serde(default)]
    pub prompt_profiles: Vec<String>,
    pub refreshing: bool,
    pub last_refreshed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub diagnostic_log_path: Option<std::path::PathBuf>,
    pub diagnostic_log_error: Option<String>,
    pub last_failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComputeProvider {
    Ssh,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComputeTargetAvailability {
    Online,
    Wakeable,
    Offline,
    Full,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComputeTargetStatus {
    pub id: String,
    pub name: String,
    pub provider: ComputeProvider,
    pub availability: ComputeTargetAvailability,
    pub active_runs: usize,
    pub max_active_runs: Option<usize>,
    pub cpu_percent: Option<f32>,
    pub memory_percent: Option<f32>,
    pub sampled_at: DateTime<Utc>,
    pub message: Option<String>,
}

impl ComputeTargetStatus {
    pub fn is_full(&self) -> bool {
        self.max_active_runs
            .is_some_and(|maximum| self.active_runs >= maximum)
    }

    pub fn is_dispatchable(&self) -> bool {
        matches!(
            self.availability,
            ComputeTargetAvailability::Online | ComputeTargetAvailability::Wakeable
        ) && !self.is_full()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceStatus {
    pub name: String,
    pub connected: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackendStatus {
    pub kind: BackendKind,
    pub available: bool,
    #[serde(default)]
    pub manager_running: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    Refresh,
    Dispatch {
        issue: IssueKey,
        profile: Option<String>,
        target: Option<String>,
    },
    Review {
        issue: IssueKey,
        target: Option<String>,
    },
    SendInput {
        run_id: String,
        text: String,
    },
    Stop {
        run_id: String,
    },
    Open {
        run_id: String,
    },
    DeleteWorktree {
        preview: Box<WorktreeDeletePreview>,
    },
    Shutdown,
}
