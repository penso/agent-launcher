use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    BackendKind, EventEnvelope, Issue, IssueKey, Repository, RunSummary, WorktreeDeletePreview,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub initialized: bool,
    pub repository: Option<Repository>,
    pub issues: Vec<Issue>,
    pub runs: Vec<RunSummary>,
    pub run_events: HashMap<String, Vec<EventEnvelope>>,
    pub sources: Vec<SourceStatus>,
    pub backends: Vec<BackendStatus>,
    pub selected_backend: Option<BackendKind>,
    pub selected_agent: String,
    #[serde(default)]
    pub prompt_profiles: Vec<String>,
    pub refreshing: bool,
    pub last_refreshed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
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
