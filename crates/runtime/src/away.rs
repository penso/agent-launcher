//! Runtime-owned admission and durable attempts; the TUI never schedules work.
use agent_launcher_core::{AppMode, AwayEntry, AwayEntryState, AwayPhase, AwayRanking};

use super::*;

#[derive(Default)]
pub(super) struct Scheduler {
    pub(super) work: JoinSet<AwayWork>,
    pub(super) launching: HashSet<String>,
    pub(super) owner_error: Option<String>,
    pub(super) lock: Option<crate::RuntimeOwnership>,
    generation: u64,
    inventory: Option<Vec<Issue>>,
    last_ranked: Option<Instant>,
    failures: usize,
    counted_failures: HashSet<String>,
}

pub(super) enum AwayWork {
    Ranked {
        generation: u64,
        inventory: Vec<Issue>,
        result: Result<Vec<AwayEntry>>,
    },
    Launched {
        run_id: String,
        result: agent_launcher_runner::Result<Box<agent_launcher_runner::DispatchResult>>,
    },
}

impl Scheduler {
    pub(super) fn record_existing_failures(&mut self, runs: &[RunSummary]) {
        self.counted_failures = runs
            .iter()
            .filter(|run| run.state == RunState::Failed)
            .map(|run| run.id.clone())
            .collect();
    }
}

fn rejected(message: impl Into<String>) -> Error {
    RunnerError::InvalidRequest(message.into()).into()
}

fn eligible(issue: &Issue) -> bool {
    issue.security_advisory.is_none()
        && !issue
            .key
            .native_id
            .to_ascii_lowercase()
            .starts_with("advisory/")
        && issue.pull_request.is_none()
        && !issue.is_blocked()
        && !issue.title.trim().is_empty()
        && !matches!(
            issue.state.to_ascii_lowercase().as_str(),
            "closed" | "done" | "completed" | "cancelled" | "canceled" | "draft" | "triage"
        )
}

fn limit(max_agents: usize) -> Result<()> {
    if !(1..=64).contains(&max_agents) {
        return Err(rejected("Away concurrency must be between 1 and 64"));
    }
    Ok(())
}

impl RuntimeService {
    pub(super) async fn initialize_away(&mut self) {
        // Hold ownership for the entire runtime, including Manual/draining, so
        // a second TUI cannot mutate a stale backend registry while Away runs.
        if let Some(parent) = self.store.path().and_then(|path| path.parent()) {
            if let Some(ownership) = &self.away.lock {
                if std::fs::canonicalize(parent).ok().as_ref() != Some(&ownership.directory) {
                    self.away.owner_error =
                        Some("Runtime ownership does not match the store directory".into());
                }
            } else {
                match crate::RuntimeOwnership::acquire(parent) {
                    Ok(lock) => self.away.lock = Some(lock),
                    Err(error) => self.away.owner_error = Some(error.to_string()),
                }
            }
        }
        match self.store.load_away().await {
            Ok(Some(mut state)) => {
                if state.mode == AppMode::Away {
                    state.phase = AwayPhase::Paused;
                    state.error =
                        Some("Restored paused; reconcile existing workers before resuming.".into());
                }
                self.snapshot.away = state;
            },
            Ok(None) => {},
            Err(error) => {
                self.away.owner_error = Some(format!("Cannot load Away state safely: {error}"));
            },
        }
        if let Some(error) = self.away.owner_error.clone() {
            self.snapshot.away.phase = AwayPhase::Attention;
            self.snapshot.away.error = Some(error);
        }
    }

    pub(super) async fn resolve_deleted_away_run(&mut self, run_id: &str) -> Result<()> {
        if let Some(entry) = self
            .snapshot
            .away
            .entries
            .iter_mut()
            .find(|entry| entry.run_id.as_deref() == Some(run_id))
        {
            entry.state = AwayEntryState::Skipped;
            entry.error = Some("Worktree removed; attempt retained in history.".into());
            self.save_away().await?;
        }
        Ok(())
    }

    pub(super) async fn record_unstarted_away(
        &mut self,
        run_id: &str,
        message: &str,
    ) -> Result<()> {
        let Some(entry) = self
            .snapshot
            .away
            .entries
            .iter()
            .find(|entry| entry.run_id.as_deref() == Some(run_id))
        else {
            return Ok(());
        };
        let now = Utc::now();
        let run = RunSummary {
            confidential: false,
            id: run_id.into(),
            issue_key: entry.issue.canonical(),
            workspace: None,
            agent: "opencode".into(),
            model: self.snapshot.away.model.clone(),
            state: RunState::Failed,
            message: Some(format!("Worker was not started: {message}")),
            session_id: None,
            started_at: now,
            updated_at: now,
        };
        self.persist_status(StatusResult { run, output: None })
            .await
    }

    pub(super) fn check_away_admission(&self, issue: Option<&IssueKey>) -> Result<()> {
        if let Some(error) = &self.away.owner_error {
            return Err(rejected(error.clone()));
        }
        if let Some(issue) = issue
            && let Some(entry) = self.snapshot.away.entries.iter().find(|entry| {
                entry.issue == *issue
                    && entry.run_id.is_some()
                    && !matches!(
                        entry.state,
                        AwayEntryState::Finished | AwayEntryState::Skipped
                    )
                    && entry.run_id.as_ref().is_none_or(|id| {
                        !self.snapshot.runs.iter().any(|run| {
                            &run.id == id
                                && matches!(
                                    run.state,
                                    RunState::Completed | RunState::Failed | RunState::Cancelled
                                )
                        })
                    })
            })
        {
            return Err(rejected(format!(
                "Issue is reserved by Away worker {}; inspect the existing attempt",
                entry.run_id.as_deref().unwrap_or("unknown")
            )));
        }
        if self.snapshot.away.mode == AppMode::Away
            && self.occupied_away_slots() >= self.snapshot.away.max_agents
        {
            return Err(rejected(
                "Away concurrency limit reached; wait for a worker to exit or increase the limit",
            ));
        }
        Ok(())
    }

    fn occupied_away_slots(&self) -> usize {
        self.snapshot.away.occupied_slots(&self.snapshot.runs) + self.security_in_flight.len()
    }

    fn validate_away_backend(&self) -> Result<()> {
        if let Some(error) = &self.away.owner_error {
            return Err(rejected(error.clone()));
        }
        if !self.snapshot.initialized {
            return Err(rejected(
                "Wait for launcher initialization before starting Away",
            ));
        }
        if self.snapshot.selected_backend != Some(BackendKind::Herdr)
            || self.snapshot.selected_agent != "opencode"
            || !self.runner.supports(BackendKind::Herdr, Capability::Away)?
        {
            return Err(rejected(
                "Away currently requires the selected Herdr backend and OpenCode harness",
            ));
        }
        if !self
            .snapshot
            .backends
            .iter()
            .any(|backend| backend.kind == BackendKind::Herdr && backend.available)
        {
            return Err(rejected(
                "Herdr is unavailable; start it before enabling Away",
            ));
        }
        Ok(())
    }

    async fn save_away(&mut self) -> Result<()> {
        if let Err(error) = self.store.save_away(&self.snapshot.away).await {
            self.snapshot.away.phase = AwayPhase::Paused;
            self.snapshot.away.error =
                Some(format!("Away paused: state could not be saved: {error}"));
            self.publish();
            return Err(error.into());
        }
        self.publish();
        Ok(())
    }

    pub(super) async fn away_command(&mut self, command: RuntimeCommand) -> Result<()> {
        if let Some(error) = &self.away.owner_error {
            return Err(rejected(error.clone()));
        }
        match command {
            RuntimeCommand::StartAway {
                max_agents,
                profile,
                ranking,
            } => {
                limit(max_agents)?;
                self.validate_away_backend()?;
                if self.snapshot.away.mode == AppMode::Away {
                    return Err(rejected(
                        "Away is already enabled; use Resume or change its limit",
                    ));
                }
                if let Some(name) = &profile {
                    self.load_prompt(name).await?;
                }
                self.snapshot.away.mode = AppMode::Away;
                self.snapshot.away.phase = AwayPhase::Refreshing;
                self.snapshot.away.max_agents = max_agents;
                self.snapshot.away.profile = profile;
                self.snapshot.away.model = self.config.agent.model.clone();
                self.snapshot.away.effort = self.config.agent.effort.clone();
                self.snapshot.away.ranking = ranking;
                self.snapshot.away.error = None;
                self.away.inventory = None;
                self.away.last_ranked = None;
                self.away.generation = self.away.generation.wrapping_add(1);
                self.away.failures = 0;
                self.save_away().await?;
                self.launch_source_refreshes();
                return Ok(());
            },
            RuntimeCommand::PauseAway => {
                if self.snapshot.away.mode != AppMode::Away {
                    return Err(rejected("Away is not enabled"));
                }
                self.snapshot.away.phase = AwayPhase::Paused;
                self.away.generation = self.away.generation.wrapping_add(1);
            },
            RuntimeCommand::ResumeAway => {
                self.validate_away_backend()?;
                if self.snapshot.away.mode != AppMode::Away {
                    return Err(rejected("Start Away before resuming"));
                }
                self.snapshot.away.phase = AwayPhase::Refreshing;
                self.snapshot.away.error = None;
                self.away.failures = 0;
                self.save_away().await?;
                self.launch_source_refreshes();
                self.launch_run_refreshes();
                return Ok(());
            },
            RuntimeCommand::SetManual => {
                self.snapshot.away.mode = AppMode::Manual;
                self.snapshot.away.phase = AwayPhase::Draining;
                self.snapshot.away.error = None;
                self.away.generation = self.away.generation.wrapping_add(1);
            },
            RuntimeCommand::SetAwayConcurrency { max_agents } => {
                limit(max_agents)?;
                self.snapshot.away.max_agents = max_agents;
            },
            RuntimeCommand::ReprioritizeAway { ranking } => {
                self.validate_away_backend()?;
                if self.snapshot.away.mode != AppMode::Away {
                    return Err(rejected("Start Away before reprioritizing"));
                }
                self.snapshot.away.ranking = ranking;
                self.snapshot.away.phase = AwayPhase::Refreshing;
                self.snapshot.away.error = None;
                self.away.inventory = None;
                self.away.last_ranked = None;
                self.away.generation = self.away.generation.wrapping_add(1);
                self.save_away().await?;
                self.launch_source_refreshes();
                return Ok(());
            },
            _ => unreachable!("not a mode command"),
        }
        self.save_away().await
    }

    fn away_inventory(&self) -> Vec<Issue> {
        let mut inventory: Vec<_> = self
            .snapshot
            .issues
            .iter()
            .filter(|issue| eligible(issue))
            .cloned()
            .collect();
        // Provider order and engagement-only refreshes should not trigger paid reranking.
        for issue in &mut inventory {
            issue.activity = None;
        }
        inventory.sort_by_key(|issue| issue.key.canonical());
        inventory.dedup_by(|a, b| a.key == b.key);
        inventory
    }

    fn away_candidates(&self, inventory: &[Issue]) -> Vec<Issue> {
        inventory
            .iter()
            .filter(|issue| {
                !self
                    .snapshot
                    .away
                    .entries
                    .iter()
                    .any(|entry| entry.issue == issue.key && entry.run_id.is_some())
                    && !self.snapshot.runs.iter().any(|run| {
                        run.issue_key == issue.key.canonical()
                            && (run.state.is_active()
                                || run.state == RunState::Disconnected
                                || run.state == RunState::Completed)
                    })
            })
            .cloned()
            .collect()
    }

    fn accept_ranking(&mut self, entries: Vec<AwayEntry>, inventory: Vec<Issue>) {
        self.snapshot
            .away
            .entries
            .retain(|entry| entry.run_id.is_some());
        self.snapshot.away.entries.extend(entries);
        self.away.inventory = Some(inventory);
        self.away.last_ranked = Some(Instant::now());
        self.snapshot.away.phase = AwayPhase::Running;
        self.snapshot.away.error = None;
    }

    pub(super) async fn reconcile_away(&mut self) {
        if self.away.owner_error.is_some() {
            return;
        }
        let previous = self.snapshot.away.clone();
        for entry in &mut self.snapshot.away.entries {
            if entry.state == AwayEntryState::Skipped {
                continue;
            }
            let Some(run) = entry
                .run_id
                .as_ref()
                .and_then(|id| self.snapshot.runs.iter().find(|run| &run.id == id))
            else {
                continue;
            };
            let state = match run.state {
                RunState::Completed => AwayEntryState::Finished,
                RunState::Failed
                | RunState::Cancelled
                | RunState::NeedsInput
                | RunState::Disconnected => AwayEntryState::Attention,
                _ => AwayEntryState::Running,
            };
            if run.state == RunState::Failed && self.away.counted_failures.insert(run.id.clone()) {
                self.away.failures += 1;
            }
            entry.state = state;
            entry.error = (state == AwayEntryState::Attention).then(|| {
                run.message
                    .clone()
                    .unwrap_or_else(|| "Worker needs attention".into())
            });
        }
        if self.snapshot.away.mode == AppMode::Manual {
            let pending = self.snapshot.away.entries.iter().any(|entry| {
                matches!(
                    entry.state,
                    AwayEntryState::Launching | AwayEntryState::Running
                ) || entry.run_id.as_ref().is_some_and(|id| {
                    self.snapshot.runs.iter().any(|run| {
                        &run.id == id
                            && (run.state.is_active() || run.state == RunState::Disconnected)
                    })
                })
            });
            self.snapshot.away.phase = if pending || self.snapshot.away.prioritizing {
                AwayPhase::Draining
            } else {
                AwayPhase::Inactive
            };
            if self.snapshot.away != previous {
                let _ = self.save_away().await;
            }
            return;
        }
        if self.away.failures >= 3
            && !matches!(
                self.snapshot.away.phase,
                AwayPhase::Paused | AwayPhase::Attention
            )
        {
            self.snapshot.away.phase = AwayPhase::Attention;
            self.snapshot.away.error =
                Some("Three workers failed; inspect their results before resuming.".into());
        }
        if matches!(
            self.snapshot.away.phase,
            AwayPhase::Paused | AwayPhase::Attention
        ) {
            if self.snapshot.away != previous {
                let _ = self.save_away().await;
            }
            return;
        }
        let public_sources: Vec<_> = self
            .sources
            .iter()
            .filter(|source| !source.is_confidential())
            .map(|source| source.cache_key())
            .collect();
        let fresh = !public_sources.is_empty()
            && public_sources.iter().all(|name| {
                !self.source_in_flight.contains(name)
                    && self
                        .snapshot
                        .sources
                        .iter()
                        .any(|source| &source.name == name && source.connected)
                    && !self.errors.contains_key(&format!("source:{name}"))
                    && !self.errors.contains_key("store:issues")
            });
        if !fresh {
            self.snapshot.away.phase = AwayPhase::Refreshing;
            self.snapshot.away.error = Some("Waiting for successful refresh of all public issue sources; cached or throttled data will not launch work.".into());
            if self.snapshot.away != previous {
                let _ = self.save_away().await;
            }
            return;
        }
        if let Err(error) = self.validate_away_backend() {
            self.snapshot.away.phase = AwayPhase::Attention;
            self.snapshot.away.error = Some(error.to_string());
            let _ = self.save_away().await;
            return;
        }
        if self.snapshot.away.prioritizing {
            if self.snapshot.away != previous {
                let _ = self.save_away().await;
            }
            return;
        }
        let inventory = self.away_inventory();
        if self.away.inventory.as_ref() != Some(&inventory) {
            let candidates = self.away_candidates(&inventory);
            if candidates.is_empty() || self.snapshot.away.ranking == AwayRanking::SourcePriority {
                self.accept_ranking(crate::prioritize::source_priority(candidates), inventory);
            } else {
                self.snapshot.away.phase = AwayPhase::Prioritizing;
                self.snapshot.away.error = None;
                if self.occupied_away_slots() < self.snapshot.away.max_agents
                    && self
                        .away
                        .last_ranked
                        .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
                {
                    self.snapshot.away.prioritizing = true;
                    if self.save_away().await.is_err() {
                        self.snapshot.away.prioritizing = false;
                        return;
                    }
                    let generation = self.away.generation;
                    let model = self.snapshot.away.model.clone();
                    self.away.work.spawn(async move {
                        AwayWork::Ranked {
                            generation,
                            inventory,
                            result: crate::prioritize::prioritize(candidates, model).await,
                        }
                    });
                } else if self.snapshot.away != previous {
                    let _ = self.save_away().await;
                }
                return;
            }
        }
        self.snapshot.away.error = None;
        // Slot reservations are persisted before spawning. Only this owner makes
        // admission decisions, including manual commands and simultaneous exits.
        while self.occupied_away_slots() < self.snapshot.away.max_agents {
            let Some(index) = self
                .snapshot
                .away
                .entries
                .iter()
                .position(|entry| entry.state == AwayEntryState::Queued)
            else {
                break;
            };
            let key = self.snapshot.away.entries[index].issue.clone();
            let issue = self
                .snapshot
                .issues
                .iter()
                .find(|issue| issue.key == key && eligible(issue))
                .cloned();
            if issue.is_none()
                || self.check_away_admission(Some(&key)).is_err()
                || self.snapshot.runs.iter().any(|run| {
                    run.issue_key == key.canonical()
                        && (run.state.is_active()
                            || matches!(run.state, RunState::Disconnected | RunState::Completed))
                })
            {
                let entry = &mut self.snapshot.away.entries[index];
                entry.state = AwayEntryState::Skipped;
                entry.error = Some("No longer eligible or already has a worker/result.".into());
                continue;
            }
            let issue = issue.expect("checked issue");
            let prompt = match self.snapshot.away.profile.as_deref() {
                Some(name) => self.preview_prompt(&key, name, None).await,
                None => Ok(issue_prompt(&issue)),
            };
            let prompt = match prompt {
                Ok(prompt) => prompt,
                Err(error) => {
                    self.snapshot.away.phase = AwayPhase::Attention;
                    self.snapshot.away.error = Some(error.to_string());
                    let _ = self.save_away().await;
                    return;
                },
            };
            let run_id = uuid::Uuid::new_v4().to_string();
            let request = DispatchRequest {
                private_fork: None,
                repository: self.repository.clone(),
                issue,
                prompt,
                agent: "opencode".into(),
                model: self.snapshot.away.model.clone(),
                effort: self.snapshot.away.effort.clone(),
                branch: None,
                workspace_name: None,
                base_branch: None,
                target: None,
            };
            let entry = &mut self.snapshot.away.entries[index];
            entry.run_id = Some(run_id.clone());
            entry.state = AwayEntryState::Launching;
            self.snapshot.away.phase = AwayPhase::Running;
            if self.save_away().await.is_err() {
                return;
            }
            self.away.launching.insert(run_id.clone());
            let runner = self.runner.clone();
            self.away.work.spawn(async move {
                let result = runner
                    .dispatch_away(BackendKind::Herdr, request, &run_id)
                    .await
                    .map(Box::new);
                AwayWork::Launched { run_id, result }
            });
        }
        self.snapshot.away.phase = if self
            .snapshot
            .away
            .entries
            .iter()
            .any(|entry| entry.state == AwayEntryState::Queued)
            || self.occupied_away_slots() > 0
        {
            AwayPhase::Running
        } else {
            AwayPhase::QueueEmpty
        };
        if self.snapshot.away != previous {
            let _ = self.save_away().await;
        }
    }

    pub(super) async fn handle_away_result(
        &mut self,
        result: std::result::Result<AwayWork, tokio::task::JoinError>,
    ) {
        match result {
            Ok(AwayWork::Ranked {
                generation,
                inventory,
                result,
            }) => {
                self.snapshot.away.prioritizing = false;
                if generation != self.away.generation
                    || self.snapshot.away.mode != AppMode::Away
                    || matches!(
                        self.snapshot.away.phase,
                        AwayPhase::Paused | AwayPhase::Attention
                    )
                {
                    self.publish();
                    return;
                }
                match result {
                    Ok(entries) => self.accept_ranking(entries, inventory),
                    Err(error) => {
                        self.snapshot.away.phase = AwayPhase::Attention;
                        self.snapshot.away.error = Some(format!(
                            "Prioritization failed: {error}. Retry or explicitly select Source priority."
                        ));
                    },
                }
            },
            Ok(AwayWork::Launched { run_id, result }) => {
                self.away.launching.remove(&run_id);
                match result {
                    Ok(result) => {
                        if let Err(error) = self
                            .persist_status(StatusResult {
                                run: result.run,
                                output: None,
                            })
                            .await
                        {
                            self.snapshot.away.phase = AwayPhase::Attention;
                            self.snapshot.away.error = Some(format!(
                                "Worker launched but persistence failed; reserved ID {run_id}: {error}"
                            ));
                        }
                        self.launch_run_refresh(&run_id);
                    },
                    Err(RunnerError::AwayNotStarted(message)) => {
                        if let Err(error) = self.record_unstarted_away(&run_id, &message).await {
                            self.snapshot.away.phase = AwayPhase::Attention;
                            self.snapshot.away.error =
                                Some(format!("Could not persist failed launch: {error}"));
                        }
                    },
                    Err(error) => {
                        if let Some(entry) = self
                            .snapshot
                            .away
                            .entries
                            .iter_mut()
                            .find(|entry| entry.run_id.as_deref() == Some(&run_id))
                        {
                            entry.state = AwayEntryState::Attention;
                            entry.error = Some(format!(
                                "Launch outcome uncertain; do not retry until inspected: {error}"
                            ));
                        }
                        self.snapshot.away.phase = AwayPhase::Attention;
                        self.snapshot.away.error = Some(format!(
                            "Worker launch failed; reserved ID {run_id}: {error}"
                        ));
                        self.launch_run_refresh(&run_id);
                    },
                }
            },
            Err(_) => {
                // Reservations survive task panics. A new process will reconcile
                // their IDs; never interpret a dropped future as process exit.
                self.snapshot.away.phase = AwayPhase::Attention;
                self.snapshot.away.error = Some(
                    "Away task failed; restart to reconcile reserved workers before resuming."
                        .into(),
                );
            },
        }
        let _ = self.save_away().await;
    }

    pub(super) async fn shutdown_away(&mut self) {
        if self.away.owner_error.is_some() {
            return;
        }
        if self.snapshot.away.mode == AppMode::Away {
            self.snapshot.away.phase = AwayPhase::Paused;
            self.snapshot.away.error =
                Some("Launcher closed; scheduling paused. Existing workers may continue.".into());
            let _ = self.save_away().await;
        }
        self.away.generation = self.away.generation.wrapping_add(1);
        // Leave reservations durable even if a bounded launch has an ambiguous
        // outcome. No replacement is admitted while shutting down.
        if tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(result) = self.away.work.join_next().await {
                self.handle_away_result(result).await;
            }
        })
        .await
        .is_err()
        {
            self.away.work.shutdown().await;
        }
        self.snapshot.away.prioritizing = false;
        if self.snapshot.away.mode == AppMode::Away {
            self.snapshot.away.phase = AwayPhase::Paused;
        }
        let _ = self.save_away().await;
    }
}

#[cfg(test)]
#[path = "away_tests.rs"]
mod tests;
