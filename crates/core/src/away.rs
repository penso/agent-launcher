use serde::{Deserialize, Serialize};

use crate::{IssueKey, RunState, RunSummary};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppMode {
    #[default]
    Manual,
    Away,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AwayPhase {
    #[default]
    Inactive,
    Refreshing,
    Prioritizing,
    Running,
    Paused,
    Draining,
    QueueEmpty,
    Attention,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AwayRanking {
    #[default]
    Agent,
    SourcePriority,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AwayEntryState {
    Queued,
    Launching,
    Running,
    Finished,
    Attention,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AwayEntry {
    pub issue: IssueKey,
    pub identifier: String,
    pub title: String,
    pub reason: String,
    pub state: AwayEntryState,
    pub run_id: Option<String>,
    pub error: Option<String>,
}

/// Durable operator intent and attempt ledger, independent of inbox sorting/filtering.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AwayState {
    /// Transient controller process also consumes one concurrency slot.
    #[serde(skip)]
    pub prioritizing: bool,
    pub mode: AppMode,
    pub phase: AwayPhase,
    pub max_agents: usize,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub ranking: AwayRanking,
    pub entries: Vec<AwayEntry>,
    pub error: Option<String>,
}

impl Default for AwayState {
    fn default() -> Self {
        Self {
            prioritizing: false,
            mode: AppMode::Manual,
            phase: AwayPhase::Inactive,
            max_agents: 5,
            profile: None,
            model: None,
            effort: None,
            ranking: AwayRanking::Agent,
            entries: Vec::new(),
            error: None,
        }
    }
}

impl AwayState {
    pub fn owns_run(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.run_id.as_deref() == Some(id))
    }

    /// Reservations count before a backend can create a process. Unknown outcomes
    /// retain their slot until the backend proves that the process has exited.
    pub fn occupied_slots(&self, runs: &[RunSummary]) -> usize {
        let reservations = self
            .entries
            .iter()
            .filter(|entry| {
                entry.run_id.as_ref().is_some_and(|id| {
                    !runs.iter().any(|run| &run.id == id)
                        && matches!(
                            entry.state,
                            AwayEntryState::Launching
                                | AwayEntryState::Running
                                | AwayEntryState::Attention
                        )
                })
            })
            .count();
        usize::from(self.prioritizing)
            + reservations
            + runs
                .iter()
                .filter(|run| {
                    run.state.is_active()
                        || run.state == RunState::Disconnected
                        || (!self.owns_run(&run.id)
                            && run.workspace.as_ref().is_some_and(|workspace| {
                                workspace.backend == crate::BackendKind::Herdr
                            })
                            && run.state == RunState::Completed)
                })
                .count()
    }
}
