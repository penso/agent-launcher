use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_launcher_core::{
    AppConfig, BackendConfig, BackendKind, BackendStatus, EventEnvelope, Issue, IssueKey,
    IssueProvider, OutputStream, Repository, RunEvent, RunState, RunSummary, RuntimeCommand,
    RuntimeSnapshot, SourceStatus,
};
use agent_launcher_issues::{IssueSource, SyncCheckpoint, SyncMode};
use agent_launcher_runner::{BackendDetection, Capability, DispatchRequest, Runner, StatusResult};
use agent_launcher_store::Store;
use chrono::Utc;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior},
};

use crate::{DesktopNotifier, Error, NoopNotifier, NotifyRustNotifier, Result};

const COMMAND_CAPACITY: usize = 64;
const RUN_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_IN_MEMORY_EVENTS: usize = 256;

type RuntimeJoin = JoinHandle<Result<()>>;

struct CommandRequest {
    command: RuntimeCommand,
    acknowledge: oneshot::Sender<Result<()>>,
}

/// Cloneable command and snapshot interface to a running [`RuntimeService`].
#[derive(Clone)]
pub struct RuntimeHandle {
    commands: mpsc::Sender<CommandRequest>,
    snapshots: watch::Receiver<RuntimeSnapshot>,
    task: Arc<Mutex<Option<RuntimeJoin>>>,
}

impl RuntimeHandle {
    /// Returns the most recently published snapshot.
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshots.borrow().clone()
    }

    /// Creates an independent snapshot subscription.
    pub fn subscribe(&self) -> watch::Receiver<RuntimeSnapshot> {
        self.snapshots.clone()
    }

    /// Sends a command to the single-owner runtime loop.
    pub async fn send(&self, command: RuntimeCommand) -> Result<()> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.commands
            .send(CommandRequest {
                command,
                acknowledge,
            })
            .await
            .map_err(|_| Error::CommandChannelClosed)?;
        acknowledged
            .await
            .map_err(|_| Error::CommandAcknowledgmentDropped)?
    }

    pub async fn refresh(&self) -> Result<()> {
        self.send(RuntimeCommand::Refresh).await
    }

    pub async fn dispatch(&self, issue: IssueKey) -> Result<()> {
        self.send(RuntimeCommand::Dispatch { issue }).await
    }

    pub async fn send_input(
        &self,
        run_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<()> {
        self.send(RuntimeCommand::SendInput {
            run_id: run_id.into(),
            text: text.into(),
        })
        .await
    }

    pub async fn stop(&self, run_id: impl Into<String>) -> Result<()> {
        self.send(RuntimeCommand::Stop {
            run_id: run_id.into(),
        })
        .await
    }

    pub async fn open(&self, run_id: impl Into<String>) -> Result<()> {
        self.send(RuntimeCommand::Open {
            run_id: run_id.into(),
        })
        .await
    }

    /// Requests shutdown and waits for all runtime-owned background work to stop.
    pub async fn shutdown(&self) -> Result<()> {
        let command_result = self.send(RuntimeCommand::Shutdown).await;
        let task = self
            .task
            .lock()
            .expect("runtime task mutex poisoned")
            .take();
        let task_result = match task {
            Some(task) => task.await?,
            None => Ok(()),
        };
        command_result.and(task_result)
    }
}

/// Owns all mutable runtime state and serializes state changes through one loop.
pub struct RuntimeService {
    repository: Repository,
    sources: Vec<Arc<dyn IssueSource>>,
    store: Store,
    runner: Arc<Runner>,
    config: AppConfig,
    notifier: Arc<dyn DesktopNotifier>,
    snapshot: RuntimeSnapshot,
    snapshots: watch::Sender<RuntimeSnapshot>,
    commands: mpsc::Receiver<CommandRequest>,
    errors: BTreeMap<String, String>,
    work: JoinSet<WorkResult>,
    notifications: JoinSet<()>,
    source_in_flight: HashSet<String>,
    run_in_flight: HashMap<String, u64>,
    run_refresh_pending: HashSet<String>,
    run_refresh_unsupported: HashSet<String>,
    run_generations: HashMap<String, u64>,
    detection_in_flight: bool,
    refresh_waiters: Vec<oneshot::Sender<Result<()>>>,
    next_sequences: HashMap<String, u64>,
    last_outputs: HashMap<String, String>,
    notified: HashSet<(String, &'static str)>,
}

impl RuntimeService {
    /// Constructs a runtime and its handle without spawning it.
    pub fn new(
        repository: Repository,
        sources: Vec<Box<dyn IssueSource>>,
        store: Store,
        runner: Arc<Runner>,
        config: AppConfig,
    ) -> (Self, RuntimeHandle) {
        let notifier: Arc<dyn DesktopNotifier> = if config.notifications.desktop {
            Arc::new(NotifyRustNotifier)
        } else {
            Arc::new(NoopNotifier)
        };
        Self::new_with_notifier(repository, sources, store, runner, config, notifier)
    }

    /// Constructs a runtime with an injected notifier, primarily for tests.
    pub fn new_with_notifier(
        repository: Repository,
        sources: Vec<Box<dyn IssueSource>>,
        store: Store,
        runner: Arc<Runner>,
        config: AppConfig,
        notifier: Arc<dyn DesktopNotifier>,
    ) -> (Self, RuntimeHandle) {
        let source_statuses = sources
            .iter()
            .map(|source| SourceStatus {
                name: source.source_key().canonical(),
                connected: false,
                message: None,
            })
            .collect();
        let snapshot = RuntimeSnapshot {
            repository: Some(repository.clone()),
            sources: source_statuses,
            selected_agent: config.agent.name.clone(),
            ..RuntimeSnapshot::default()
        };
        let (snapshots, snapshot_rx) = watch::channel(snapshot.clone());
        let (command_tx, commands) = mpsc::channel(COMMAND_CAPACITY);
        let task = Arc::new(Mutex::new(None));
        let handle = RuntimeHandle {
            commands: command_tx,
            snapshots: snapshot_rx,
            task: Arc::clone(&task),
        };
        (
            Self {
                repository,
                sources: sources.into_iter().map(Arc::from).collect(),
                store,
                runner,
                config,
                notifier,
                snapshot,
                snapshots,
                commands,
                errors: BTreeMap::new(),
                work: JoinSet::new(),
                notifications: JoinSet::new(),
                source_in_flight: HashSet::new(),
                run_in_flight: HashMap::new(),
                run_refresh_pending: HashSet::new(),
                run_refresh_unsupported: HashSet::new(),
                run_generations: HashMap::new(),
                detection_in_flight: false,
                refresh_waiters: Vec::new(),
                next_sequences: HashMap::new(),
                last_outputs: HashMap::new(),
                notified: HashSet::new(),
            },
            handle,
        )
    }

    /// Constructs and starts the runtime using desktop notifications from configuration.
    pub fn start(
        repository: Repository,
        sources: Vec<Box<dyn IssueSource>>,
        store: Store,
        runner: Arc<Runner>,
        config: AppConfig,
    ) -> RuntimeHandle {
        let (service, handle) = Self::new(repository, sources, store, runner, config);
        handle.attach(tokio::spawn(service.run()));
        handle
    }

    /// Starts the runtime with an injected notifier.
    pub fn start_with_notifier(
        repository: Repository,
        sources: Vec<Box<dyn IssueSource>>,
        store: Store,
        runner: Arc<Runner>,
        config: AppConfig,
        notifier: Arc<dyn DesktopNotifier>,
    ) -> RuntimeHandle {
        let (service, handle) =
            Self::new_with_notifier(repository, sources, store, runner, config, notifier);
        handle.attach(tokio::spawn(service.run()));
        handle
    }

    /// Runs the service until a shutdown command or all handles are dropped.
    pub async fn run(mut self) -> Result<()> {
        self.initialize().await;
        self.launch_source_refreshes();
        self.launch_run_refreshes();

        let issue_period = Duration::from_secs(self.config.poll_interval_seconds.max(1));
        let mut issue_tick = tokio::time::interval_at(Instant::now() + issue_period, issue_period);
        issue_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut run_tick =
            tokio::time::interval_at(Instant::now() + RUN_POLL_INTERVAL, RUN_POLL_INTERVAL);
        run_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(request) = command else { break };
                    if self.handle_command(request).await {
                        break;
                    }
                }
                _ = issue_tick.tick() => self.launch_source_refreshes(),
                _ = run_tick.tick() => self.launch_run_refreshes(),
                Some(result) = self.work.join_next(), if !self.work.is_empty() => {
                    match result {
                        Ok(result) => self.handle_work_result(result).await,
                        Err(error) if error.is_cancelled() => {},
                        Err(error) => {
                            self.set_error("task", error.to_string());
                            self.complete_refreshes_if_idle();
                        },
                    }
                }
                Some(result) = self.notifications.join_next(), if !self.notifications.is_empty() => {
                    if let Err(error) = result
                        && !error.is_cancelled()
                    {
                        tracing::warn!(%error, "notification task failed");
                    }
                }
            }
        }

        self.work.abort_all();
        while self.work.join_next().await.is_some() {}
        self.notifications.abort_all();
        while self.notifications.join_next().await.is_some() {}
        Ok(())
    }

    async fn initialize(&mut self) {
        let active_sources = self
            .sources
            .iter()
            .map(|source| source.source_key().canonical())
            .collect::<Vec<_>>();
        if let Err(error) = self.store.prune_inactive_sources(&active_sources).await {
            self.set_error_without_publish("store:prune", error.to_string());
        }
        match self.store.load_issues().await {
            Ok(issues) => self.snapshot.issues = visible_issues(issues),
            Err(error) => self.set_error_without_publish("store:issues", error.to_string()),
        }
        match self.store.load_runs().await {
            Ok(runs) => {
                self.snapshot.runs = runs;
                self.initialize_event_cursors().await;
            },
            Err(error) => self.set_error_without_publish("store:runs", error.to_string()),
        }
        self.apply_detections(self.runner.detect(&self.repository).await);
        self.publish();
    }

    async fn initialize_event_cursors(&mut self) {
        for run in self.snapshot.runs.clone() {
            match self
                .store
                .load_recent_events(&run.id, MAX_IN_MEMORY_EVENTS)
                .await
            {
                Ok(events) => {
                    let next = events
                        .last()
                        .map_or(0, |event| event.sequence.saturating_add(1));
                    self.next_sequences.insert(run.id.clone(), next);
                    self.snapshot.run_events.insert(run.id.clone(), events);
                },
                Err(error) => self.set_error_without_publish(
                    format!("store:events:{}", run.id),
                    error.to_string(),
                ),
            }
        }
    }

    async fn handle_command(&mut self, request: CommandRequest) -> bool {
        let CommandRequest {
            command,
            acknowledge,
        } = request;
        if matches!(&command, RuntimeCommand::Refresh) {
            self.refresh_waiters.push(acknowledge);
            self.launch_detection();
            self.launch_source_refreshes();
            self.launch_run_refreshes();
            self.update_refreshing();
            self.complete_refreshes_if_idle();
            self.publish();
            return false;
        }
        let (name, result, shutdown) = match command {
            RuntimeCommand::Refresh => unreachable!("refresh handled above"),
            RuntimeCommand::Dispatch { issue } => {
                let result = self.dispatch_issue(&issue).await;
                ("dispatch", result, false)
            },
            RuntimeCommand::SendInput { run_id, text } => {
                self.invalidate_run_refresh(&run_id);
                let result = self
                    .runner
                    .send_input(&run_id, &text)
                    .await
                    .map_err(Error::from);
                self.schedule_post_mutation_refresh(&run_id);
                ("send-input", result, false)
            },
            RuntimeCommand::Stop { run_id } => {
                self.invalidate_run_refresh(&run_id);
                let result = self.runner.stop(&run_id).await.map_err(Error::from);
                self.schedule_post_mutation_refresh(&run_id);
                ("stop", result, false)
            },
            RuntimeCommand::Open { run_id } => {
                let result = self
                    .runner
                    .open(&run_id)
                    .await
                    .map(|_| ())
                    .map_err(Error::from);
                ("open", result, false)
            },
            RuntimeCommand::Shutdown => ("shutdown", Ok(()), true),
        };
        self.record_command_result(name, &result);
        let _ = acknowledge.send(result);
        shutdown
    }

    fn record_command_result(&mut self, name: &str, result: &Result<()>) {
        let key = format!("command:{name}");
        match result {
            Ok(()) => self.clear_error(&key),
            Err(error) => self.set_error(&key, error.to_string()),
        }
    }

    async fn dispatch_issue(&mut self, key: &IssueKey) -> Result<()> {
        let canonical = key.canonical();
        if self
            .snapshot
            .runs
            .iter()
            .any(|run| run.issue_key == canonical && run.state.is_active())
        {
            return Ok(());
        }
        let issue = self
            .snapshot
            .issues
            .iter()
            .find(|issue| issue.key == *key)
            .cloned()
            .ok_or_else(|| Error::IssueNotFound(key.clone()))?;
        let backend = self.snapshot.selected_backend.ok_or_else(|| {
            Error::BackendUnavailable(backend_config_name(&self.config.backend).to_owned())
        })?;
        let request = DispatchRequest {
            repository: self.repository.clone(),
            prompt: issue_prompt(&issue),
            issue,
            agent: self.config.agent.name.clone(),
            branch: None,
            workspace_name: None,
            base_branch: None,
            model: self.config.agent.model.clone(),
            effort: self.config.agent.effort.clone(),
        };
        let result = self.runner.dispatch(backend, request).await?;
        let mut run = result.run;
        if !result.capabilities.supports(Capability::Refresh) {
            run.state = RunState::Idle;
            run.message = Some("Status tracking is not exposed by this backend; open the workspace to follow progress".to_owned());
            run.updated_at = Utc::now();
            self.run_refresh_unsupported.insert(run.id.clone());
        }
        self.store.insert_run(&run).await?;
        self.next_sequences.insert(run.id.clone(), 0);
        self.upsert_snapshot_run(run);
        self.publish();
        Ok(())
    }

    fn launch_source_refreshes(&mut self) {
        let mut launched = false;
        for source in &self.sources {
            let source_name = source.source_key().canonical();
            if !self.source_in_flight.insert(source_name.clone()) {
                continue;
            }
            let source = Arc::clone(source);
            let store = self.store.clone();
            launched = true;
            self.work.spawn(async move {
                WorkResult::Source {
                    source_name: source_name.clone(),
                    result: sync_source(source_name, source, store).await,
                }
            });
        }
        self.update_refreshing();
        if launched {
            self.publish();
        }
    }

    fn launch_run_refreshes(&mut self) {
        let run_ids = self
            .snapshot
            .runs
            .iter()
            .filter(|run| run.state.is_active())
            .filter(|run| !self.run_refresh_unsupported.contains(&run.id))
            .map(|run| run.id.clone())
            .collect::<Vec<_>>();
        for run_id in run_ids {
            self.launch_run_refresh(&run_id);
        }
    }

    fn launch_run_refresh(&mut self, run_id: &str) {
        if self.run_refresh_unsupported.contains(run_id) {
            return;
        }
        if self.run_in_flight.contains_key(run_id) {
            return;
        }
        let generation = *self.run_generations.entry(run_id.to_owned()).or_default();
        self.run_in_flight.insert(run_id.to_owned(), generation);
        let runner = Arc::clone(&self.runner);
        let run_id = run_id.to_owned();
        self.work.spawn(async move {
            let result = runner.refresh(&run_id).await;
            WorkResult::Run {
                run_id,
                generation,
                result,
            }
        });
    }

    fn invalidate_run_refresh(&mut self, run_id: &str) {
        let generation = self.run_generations.entry(run_id.to_owned()).or_default();
        *generation = generation.saturating_add(1);
    }

    fn schedule_post_mutation_refresh(&mut self, run_id: &str) {
        if self.run_in_flight.contains_key(run_id) {
            self.run_refresh_pending.insert(run_id.to_owned());
        } else {
            self.launch_run_refresh(run_id);
        }
    }

    fn launch_detection(&mut self) {
        if self.detection_in_flight {
            return;
        }
        self.detection_in_flight = true;
        let runner = Arc::clone(&self.runner);
        let repository = self.repository.clone();
        self.work
            .spawn(async move { WorkResult::Detection(runner.detect(&repository).await) });
    }

    async fn handle_work_result(&mut self, result: WorkResult) {
        match result {
            WorkResult::Source {
                source_name,
                result,
            } => {
                self.source_in_flight.remove(&source_name);
                match result {
                    Ok(()) => {
                        self.set_source_status(&source_name, true, None);
                        self.clear_error_without_publish(&format!("source:{source_name}"));
                        match self.store.load_issues().await {
                            Ok(issues) => {
                                self.snapshot.issues = visible_issues(issues);
                                self.clear_error_without_publish("store:issues");
                            },
                            Err(error) => {
                                self.set_error_without_publish("store:issues", error.to_string())
                            },
                        }
                    },
                    Err(error) => {
                        let message = error.to_string();
                        self.set_source_status(&source_name, false, Some(message.clone()));
                        self.set_error_without_publish(format!("source:{source_name}"), message);
                    },
                }
                self.update_refreshing();
                if self.source_in_flight.is_empty() {
                    self.snapshot.last_refreshed_at = Some(Utc::now());
                }
            },
            WorkResult::Run {
                run_id,
                generation,
                result,
            } => {
                self.run_in_flight.remove(&run_id);
                if self
                    .run_generations
                    .get(&run_id)
                    .copied()
                    .unwrap_or_default()
                    == generation
                {
                    match result {
                        Ok(status) => match self.persist_status(status).await {
                            Ok(()) => self.clear_error_without_publish(&format!("run:{run_id}")),
                            Err(error) => self.set_error_without_publish(
                                format!("run:{run_id}"),
                                error.to_string(),
                            ),
                        },
                        Err(error @ agent_launcher_runner::Error::RunNotFound(_)) => {
                            let message = error.to_string();
                            match self.persist_disconnected(&run_id, message.clone()).await {
                                Ok(()) => {},
                                Err(persist_error) => self.set_error_without_publish(
                                    format!("run:{run_id}:persist"),
                                    persist_error.to_string(),
                                ),
                            }
                            self.set_error_without_publish(format!("run:{run_id}"), message);
                        },
                        Err(agent_launcher_runner::Error::UnsupportedCapability {
                            capability: Capability::Refresh,
                            ..
                        }) => {
                            self.run_refresh_unsupported.insert(run_id.clone());
                            if let Some(mut run) = self
                                .snapshot
                                .runs
                                .iter()
                                .find(|run| run.id == run_id)
                                .cloned()
                            {
                                run.state = RunState::Idle;
                                run.message = Some("Status tracking is not exposed by this backend; open the workspace to follow progress".to_owned());
                                run.updated_at = Utc::now();
                                if let Err(error) = self.store.update_run(&run).await {
                                    self.set_error_without_publish(
                                        format!("run:{run_id}:persist"),
                                        error.to_string(),
                                    );
                                } else {
                                    self.upsert_snapshot_run(run);
                                }
                            }
                            self.clear_error_without_publish(&format!("run:{run_id}"));
                        },
                        Err(error) => self
                            .set_error_without_publish(format!("run:{run_id}"), error.to_string()),
                    }
                }
                if self.run_refresh_pending.remove(&run_id) {
                    self.launch_run_refresh(&run_id);
                }
            },
            WorkResult::Detection(detections) => {
                self.detection_in_flight = false;
                self.apply_detections(detections);
            },
        }
        self.complete_refreshes_if_idle();
        self.publish();
    }

    async fn persist_status(&mut self, status: StatusResult) -> Result<()> {
        let previous = self
            .snapshot
            .runs
            .iter()
            .find(|run| run.id == status.run.id)
            .cloned();
        let transitioned = previous
            .as_ref()
            .is_none_or(|run| run.state != status.run.state || run.message != status.run.message);
        let mut sequence = *self
            .next_sequences
            .entry(status.run.id.clone())
            .or_insert(0);
        let mut events = Vec::new();
        if transitioned {
            events.push(EventEnvelope {
                run_id: status.run.id.clone(),
                sequence,
                timestamp: status.run.updated_at,
                payload: RunEvent::StateChanged {
                    state: status.run.state,
                    message: status.run.message.clone(),
                },
            });
            sequence = sequence.saturating_add(1);
        }
        let changed_output = status
            .output
            .filter(|output| self.last_outputs.get(&status.run.id) != Some(output));
        if let Some(output) = changed_output.as_ref() {
            let text = self
                .last_outputs
                .get(&status.run.id)
                .and_then(|previous| output.strip_prefix(previous))
                .filter(|delta| !delta.is_empty())
                .unwrap_or(output)
                .to_owned();
            events.push(EventEnvelope {
                run_id: status.run.id.clone(),
                sequence,
                timestamp: status.run.updated_at,
                payload: RunEvent::Output {
                    stream: OutputStream::Pty,
                    text,
                },
            });
            sequence = sequence.saturating_add(1);
        }

        self.store
            .update_run_with_events(&status.run, &events)
            .await?;
        self.next_sequences.insert(status.run.id.clone(), sequence);
        if let Some(output) = changed_output {
            self.last_outputs.insert(status.run.id.clone(), output);
        }
        let snapshot_events = self
            .snapshot
            .run_events
            .entry(status.run.id.clone())
            .or_default();
        snapshot_events.extend(events);
        if snapshot_events.len() > MAX_IN_MEMORY_EVENTS {
            snapshot_events.drain(..snapshot_events.len() - MAX_IN_MEMORY_EVENTS);
        }

        if previous
            .as_ref()
            .is_some_and(|run| !run.state.needs_attention())
            && status.run.state.needs_attention()
        {
            self.notify_once(&status.run);
        }
        self.upsert_snapshot_run(status.run);
        Ok(())
    }

    async fn persist_disconnected(&mut self, run_id: &str, message: String) -> Result<()> {
        let Some(mut run) = self
            .snapshot
            .runs
            .iter()
            .find(|run| run.id == run_id && run.state.is_active())
            .cloned()
        else {
            return Ok(());
        };
        run.state = RunState::Disconnected;
        run.message = Some(message);
        run.updated_at = Utc::now();
        self.persist_status(StatusResult { run, output: None })
            .await
    }

    fn notify_once(&mut self, run: &RunSummary) {
        let state = match run.state {
            RunState::NeedsInput => "needs_input",
            RunState::Failed => "failed",
            RunState::Disconnected => "disconnected",
            _ => return,
        };
        let key = (run.id.clone(), state);
        if !self.config.notifications.desktop || !self.notified.insert(key) {
            return;
        }
        let notifier = Arc::clone(&self.notifier);
        let run = run.clone();
        self.notifications.spawn_blocking(move || {
            if let Err(error) = notifier.notify(&run) {
                tracing::warn!(run_id = %run.id, %error, "desktop notification failed");
            }
        });
    }

    fn apply_detections(&mut self, detections: Vec<BackendDetection>) {
        let selected = select_backend(&self.config.backend, &detections);
        self.snapshot.selected_backend = selected;
        self.snapshot.backends = detections
            .iter()
            .map(|detection| BackendStatus {
                kind: detection.backend,
                available: detection.available,
                manager_running: detection.manager_running,
                message: detection.message.clone(),
            })
            .collect();

        self.errors.retain(|key, _| !key.starts_with("backend:"));
        if selected.is_none() {
            let requested = backend_config_name(&self.config.backend);
            self.errors.insert(
                "backend:selected".to_owned(),
                Error::BackendUnavailable(requested.to_owned()).to_string(),
            );
        } else {
            self.errors.remove("backend:selected");
        }
        self.update_error_text();
    }

    fn set_source_status(&mut self, name: &str, connected: bool, message: Option<String>) {
        if let Some(status) = self
            .snapshot
            .sources
            .iter_mut()
            .find(|status| status.name == name)
        {
            status.connected = connected;
            status.message = message;
        } else {
            self.snapshot.sources.push(SourceStatus {
                name: name.to_owned(),
                connected,
                message,
            });
            self.snapshot
                .sources
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
    }

    fn upsert_snapshot_run(&mut self, run: RunSummary) {
        if let Some(existing) = self.snapshot.runs.iter_mut().find(|item| item.id == run.id) {
            *existing = run;
        } else {
            self.snapshot.runs.push(run);
        }
        self.snapshot.runs.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then(left.id.cmp(&right.id))
        });
    }

    fn update_refreshing(&mut self) {
        self.snapshot.refreshing =
            !self.source_in_flight.is_empty() || !self.refresh_waiters.is_empty();
    }

    fn complete_refreshes_if_idle(&mut self) {
        if self.refresh_waiters.is_empty()
            || self.detection_in_flight
            || !self.source_in_flight.is_empty()
            || !self.run_in_flight.is_empty()
            || !self.run_refresh_pending.is_empty()
        {
            return;
        }

        let failure = self
            .errors
            .iter()
            .filter(|(key, _)| !key.starts_with("command:"))
            .map(|(key, value)| format!("{key}: {value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if failure.is_empty() {
            self.clear_error_without_publish("command:refresh");
        } else {
            self.set_error_without_publish("command:refresh", failure.clone());
        }
        let waiters = std::mem::take(&mut self.refresh_waiters);
        self.update_refreshing();
        self.publish();
        for waiter in waiters {
            let result = if failure.is_empty() {
                Ok(())
            } else {
                Err(Error::RefreshFailed(failure.clone()))
            };
            let _ = waiter.send(result);
        }
    }

    fn set_error(&mut self, key: impl Into<String>, message: String) {
        self.set_error_without_publish(key, message);
        self.publish();
    }

    fn set_error_without_publish(&mut self, key: impl Into<String>, message: String) {
        self.errors.insert(key.into(), message);
        self.update_error_text();
    }

    fn clear_error(&mut self, key: &str) {
        self.clear_error_without_publish(key);
        self.publish();
    }

    fn clear_error_without_publish(&mut self, key: &str) {
        self.errors.remove(key);
        self.update_error_text();
    }

    fn update_error_text(&mut self) {
        self.snapshot.error = (!self.errors.is_empty()).then(|| {
            self.errors
                .iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join("; ")
        });
    }

    fn publish(&self) {
        self.snapshots.send_replace(self.snapshot.clone());
    }
}

impl RuntimeHandle {
    fn attach(&self, task: RuntimeJoin) {
        *self.task.lock().expect("runtime task mutex poisoned") = Some(task);
    }
}

enum WorkResult {
    Source {
        source_name: String,
        result: Result<()>,
    },
    Run {
        run_id: String,
        generation: u64,
        result: agent_launcher_runner::Result<StatusResult>,
    },
    Detection(Vec<BackendDetection>),
}

async fn sync_source(
    source_name: String,
    source: Arc<dyn IssueSource>,
    store: Store,
) -> Result<()> {
    let checkpoint = store
        .source_checkpoint(&source_name)
        .await?
        .map(|value| {
            serde_json::from_value::<SyncCheckpoint>(value).map_err(|source| Error::Checkpoint {
                source_name: source_name.clone(),
                source,
            })
        })
        .transpose()?;
    let result = source
        .sync(checkpoint.as_ref())
        .await
        .map_err(|source| Error::IssueSource {
            source_name: source_name.clone(),
            source,
        })?;

    match result.mode {
        SyncMode::Full => store.replace_issues(&source_name, &result.issues).await?,
        SyncMode::Delta => store.upsert_issues(&source_name, &result.issues).await?,
        SyncMode::NotModified => {},
    }
    let checkpoint =
        serde_json::to_value(result.checkpoint).map_err(|source| Error::Checkpoint {
            source_name: source_name.clone(),
            source,
        })?;
    store
        .set_source_checkpoint(&source_name, &checkpoint)
        .await?;
    Ok(())
}

fn select_backend(
    configured: &BackendConfig,
    detections: &[BackendDetection],
) -> Option<BackendKind> {
    let available = |kind| {
        detections
            .iter()
            .any(|detection| detection.backend == kind && detection.available)
    };
    match configured {
        BackendConfig::Auto => [BackendKind::Superset, BackendKind::Native]
            .into_iter()
            .find(|kind| available(*kind)),
        BackendConfig::Superset => {
            available(BackendKind::Superset).then_some(BackendKind::Superset)
        },
        BackendConfig::Native => available(BackendKind::Native).then_some(BackendKind::Native),
        BackendConfig::Herdr => available(BackendKind::Herdr).then_some(BackendKind::Herdr),
        BackendConfig::Conductor => {
            available(BackendKind::Conductor).then_some(BackendKind::Conductor)
        },
    }
}

fn backend_config_name(config: &BackendConfig) -> &'static str {
    match config {
        BackendConfig::Auto => "auto (Superset or Native)",
        BackendConfig::Superset => "superset",
        BackendConfig::Native => "native",
        BackendConfig::Herdr => "herdr",
        BackendConfig::Conductor => "conductor",
    }
}

fn visible_issues(issues: Vec<Issue>) -> Vec<Issue> {
    issues
        .into_iter()
        .filter(|issue| match issue.key.provider {
            IssueProvider::Beads => !issue.state.eq_ignore_ascii_case("closed"),
            IssueProvider::Github | IssueProvider::Gitlab => {
                issue.state.eq_ignore_ascii_case("open")
                    || issue.state.eq_ignore_ascii_case("opened")
            },
        })
        .collect()
}

fn issue_prompt(issue: &Issue) -> String {
    format!(
        "Implement this issue.\n\nProvider: {}\nRepository: {}\nIdentifier: {}\nTitle: {}\nDescription: {}\nURL: {}",
        issue.key.provider,
        issue.key.repository,
        issue.identifier,
        issue.title,
        issue.description.as_deref().unwrap_or("(none)"),
        issue.url.as_deref().unwrap_or("(none)"),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use agent_launcher_core::{AgentConfig, NotificationConfig, RepositoryRemote, WorkspaceRef};
    use agent_launcher_issues::{SourceKey, SyncResult};
    use agent_launcher_runner::{
        Backend, BackendCapabilities, Capability, DispatchResult, OpenResult,
    };
    use async_trait::async_trait;
    use tokio::sync::Semaphore;

    use super::*;

    struct MockSource {
        key: SourceKey,
        state: Arc<MockSourceState>,
    }

    struct MockSourceState {
        results: Mutex<VecDeque<std::result::Result<SyncResult, agent_launcher_issues::Error>>>,
        checkpoints: Mutex<Vec<Option<SyncCheckpoint>>>,
        calls: AtomicUsize,
    }

    impl MockSource {
        fn new(
            results: Vec<std::result::Result<SyncResult, agent_launcher_issues::Error>>,
        ) -> (Self, Arc<MockSourceState>) {
            Self::with_key(
                SourceKey {
                    provider: IssueProvider::Github,
                    host: "example.com".to_owned(),
                    repository: "acme/widgets".to_owned(),
                },
                results,
            )
        }

        fn with_key(
            key: SourceKey,
            results: Vec<std::result::Result<SyncResult, agent_launcher_issues::Error>>,
        ) -> (Self, Arc<MockSourceState>) {
            let state = Arc::new(MockSourceState {
                results: Mutex::new(results.into()),
                checkpoints: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            });
            (
                Self {
                    key,
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    #[async_trait]
    impl IssueSource for MockSource {
        fn source_key(&self) -> &SourceKey {
            &self.key
        }

        async fn sync(
            &self,
            checkpoint: Option<&SyncCheckpoint>,
        ) -> std::result::Result<SyncResult, agent_launcher_issues::Error> {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            self.state
                .checkpoints
                .lock()
                .unwrap()
                .push(checkpoint.cloned());
            self.state
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(SyncResult {
                        issues: Vec::new(),
                        checkpoint: checkpoint.cloned().unwrap_or_default(),
                        mode: SyncMode::NotModified,
                    })
                })
        }
    }

    struct MockBackend {
        kind: BackendKind,
        available: bool,
        dispatches: AtomicUsize,
        refreshes: AtomicUsize,
        inputs: AtomicUsize,
        stops: AtomicUsize,
        opens: AtomicUsize,
        requests: Mutex<Vec<DispatchRequest>>,
        runs: Mutex<HashMap<String, RunSummary>>,
        next_status: Mutex<Option<(RunState, Option<String>, Option<String>)>>,
        refresh_gate: Mutex<Option<RefreshGate>>,
    }

    struct RefreshGate {
        started: Arc<Semaphore>,
        release: Arc<Semaphore>,
    }

    impl MockBackend {
        fn new(kind: BackendKind, available: bool) -> Arc<Self> {
            Arc::new(Self {
                kind,
                available,
                dispatches: AtomicUsize::new(0),
                refreshes: AtomicUsize::new(0),
                inputs: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
                opens: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
                runs: Mutex::new(HashMap::new()),
                next_status: Mutex::new(None),
                refresh_gate: Mutex::new(None),
            })
        }

        fn set_status(&self, state: RunState, message: Option<&str>, output: Option<&str>) {
            *self.next_status.lock().unwrap() =
                Some((state, message.map(str::to_owned), output.map(str::to_owned)));
        }

        fn block_next_refresh(&self) -> RefreshGate {
            let gate = RefreshGate {
                started: Arc::new(Semaphore::new(0)),
                release: Arc::new(Semaphore::new(0)),
            };
            *self.refresh_gate.lock().unwrap() = Some(RefreshGate {
                started: Arc::clone(&gate.started),
                release: Arc::clone(&gate.release),
            });
            gate
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new([
                Capability::Detect,
                Capability::Dispatch,
                Capability::Refresh,
                Capability::SendInput,
                Capability::Stop,
                Capability::Open,
            ])
        }

        async fn owns_run(&self, run_id: &str) -> bool {
            self.runs.lock().unwrap().contains_key(run_id)
        }

        async fn detect(
            &self,
            _repository: &Repository,
        ) -> agent_launcher_runner::Result<BackendDetection> {
            Ok(BackendDetection {
                backend: self.kind,
                available: self.available,
                manager_running: self.available
                    && matches!(self.kind, BackendKind::Superset | BackendKind::Herdr),
                capabilities: self.capabilities(),
                message: (!self.available).then(|| "mock unavailable".to_owned()),
            })
        }

        async fn dispatch(
            &self,
            request: DispatchRequest,
        ) -> agent_launcher_runner::Result<DispatchResult> {
            let number = self.dispatches.fetch_add(1, Ordering::SeqCst) + 1;
            let now = Utc::now();
            let run = RunSummary {
                id: format!("run-{number}"),
                issue_key: request.issue.key.canonical(),
                workspace: Some(WorkspaceRef {
                    backend: self.kind,
                    id: format!("workspace-{number}"),
                    host: None,
                    path: None,
                    branch: format!("agent/{number}"),
                }),
                agent: request.agent.clone(),
                state: RunState::Running,
                message: None,
                session_id: Some(format!("session-{number}")),
                started_at: now,
                updated_at: now,
            };
            self.requests.lock().unwrap().push(request);
            self.runs
                .lock()
                .unwrap()
                .insert(run.id.clone(), run.clone());
            Ok(DispatchResult {
                run,
                capabilities: self.capabilities(),
            })
        }

        async fn refresh(&self, run_id: &str) -> agent_launcher_runner::Result<StatusResult> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            let gate = self.refresh_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                let run = self
                    .runs
                    .lock()
                    .unwrap()
                    .get(run_id)
                    .cloned()
                    .ok_or_else(|| agent_launcher_runner::Error::RunNotFound(run_id.to_owned()))?;
                gate.started.add_permits(1);
                gate.release.acquire().await.unwrap().forget();
                return Ok(StatusResult { run, output: None });
            }
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(run_id)
                .ok_or_else(|| agent_launcher_runner::Error::RunNotFound(run_id.to_owned()))?;
            let output =
                if let Some((state, message, output)) = self.next_status.lock().unwrap().take() {
                    run.state = state;
                    run.message = message;
                    run.updated_at = Utc::now();
                    output
                } else {
                    None
                };
            Ok(StatusResult {
                run: run.clone(),
                output,
            })
        }

        async fn send_input(&self, run_id: &str, _text: &str) -> agent_launcher_runner::Result<()> {
            self.inputs.fetch_add(1, Ordering::SeqCst);
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(run_id)
                .ok_or_else(|| agent_launcher_runner::Error::RunNotFound(run_id.to_owned()))?;
            run.state = RunState::Running;
            run.message = None;
            run.updated_at = Utc::now();
            Ok(())
        }

        async fn stop(&self, run_id: &str) -> agent_launcher_runner::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .get_mut(run_id)
                .ok_or_else(|| agent_launcher_runner::Error::RunNotFound(run_id.to_owned()))?;
            run.state = RunState::Cancelled;
            run.updated_at = Utc::now();
            Ok(())
        }

        async fn open(&self, run_id: &str) -> agent_launcher_runner::Result<OpenResult> {
            if !self.runs.lock().unwrap().contains_key(run_id) {
                return Err(agent_launcher_runner::Error::RunNotFound(run_id.to_owned()));
            }
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(OpenResult {
                uri: "https://example.com/run".parse().unwrap(),
                launched: true,
            })
        }
    }

    #[derive(Default)]
    struct MockNotifier {
        calls: AtomicUsize,
    }

    impl DesktopNotifier for MockNotifier {
        fn notify(&self, _run: &RunSummary) -> std::result::Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn repository() -> Repository {
        Repository {
            root: PathBuf::from("/work/widgets"),
            git_dir: PathBuf::from("/work/widgets/.git"),
            remote: Some(RepositoryRemote {
                name: "origin".to_owned(),
                url: "https://example.com/acme/widgets.git".to_owned(),
                host: "example.com".to_owned(),
                repository: "acme/widgets".to_owned(),
                provider: IssueProvider::Github,
            }),
            has_beads: false,
        }
    }

    fn issue(id: &str, title: &str, state: &str) -> Issue {
        Issue {
            key: IssueKey {
                provider: IssueProvider::Github,
                host: "example.com".to_owned(),
                repository: "acme/widgets".to_owned(),
                native_id: id.to_owned(),
            },
            identifier: format!("#{id}"),
            title: title.to_owned(),
            description: Some(format!("Description for {id}")),
            state: state.to_owned(),
            url: Some(format!("https://example.com/acme/widgets/issues/{id}")),
            author: None,
            labels: Vec::new(),
            parent_id: None,
            blocked_by: Vec::new(),
            priority: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn beads_issue(id: &str, state: &str) -> Issue {
        let mut issue = issue(id, "Local work", state);
        issue.key.provider = IssueProvider::Beads;
        issue.key.host = "local".to_owned();
        issue.key.repository = "/work/widgets".to_owned();
        issue.identifier = id.to_owned();
        issue.url = None;
        issue
    }

    fn config() -> AppConfig {
        AppConfig {
            poll_interval_seconds: 3_600,
            backend: BackendConfig::Auto,
            agent: AgentConfig {
                name: "opencode".to_owned(),
                model: Some("provider/model".to_owned()),
                effort: Some("high".to_owned()),
            },
            superset_host: None,
            ssh: None,
            notifications: NotificationConfig { desktop: true },
        }
    }

    fn runner(backends: &[Arc<MockBackend>]) -> Arc<Runner> {
        Arc::new(Runner::new(backends.iter().cloned().map(|backend| {
            let backend: Arc<dyn Backend> = backend;
            backend
        })))
    }

    async fn wait_for(
        snapshots: &mut watch::Receiver<RuntimeSnapshot>,
        predicate: impl Fn(&RuntimeSnapshot) -> bool,
    ) -> RuntimeSnapshot {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if predicate(&snapshot) {
                    return snapshot;
                }
                snapshots.changed().await.expect("runtime still running");
            }
        })
        .await
        .expect("snapshot condition timed out")
    }

    #[tokio::test]
    async fn startup_applies_full_then_delta_and_preserves_cache_on_failure() {
        let store = Store::in_memory().await.unwrap();
        let source_name = "github:example.com:acme/widgets";
        store
            .replace_issues(source_name, &[issue("cached", "Cached", "open")])
            .await
            .unwrap();
        let checkpoint = SyncCheckpoint {
            etag: Some("one".to_owned()),
            ..SyncCheckpoint::default()
        };
        let (source, source_state) = MockSource::new(vec![
            Ok(SyncResult {
                issues: vec![
                    issue("1", "First", "open"),
                    issue("closed", "Closed", "closed"),
                ],
                checkpoint: checkpoint.clone(),
                mode: SyncMode::Full,
            }),
            Ok(SyncResult {
                issues: vec![issue("2", "Second", "open")],
                checkpoint: SyncCheckpoint {
                    etag: Some("two".to_owned()),
                    ..checkpoint.clone()
                },
                mode: SyncMode::Delta,
            }),
            Err(agent_launcher_issues::Error::InvalidRemoteUrl(
                "temporary failure".to_owned(),
            )),
        ]);
        let (beads_source, _beads_state) = MockSource::with_key(
            SourceKey {
                provider: IssueProvider::Beads,
                host: "local".to_owned(),
                repository: "/work/widgets".to_owned(),
            },
            vec![Ok(SyncResult {
                issues: vec![beads_issue("widgets-local", "in_progress")],
                checkpoint: SyncCheckpoint::default(),
                mode: SyncMode::Full,
            })],
        );
        let native = MockBackend::new(BackendKind::Native, true);
        let handle = RuntimeService::start_with_notifier(
            repository(),
            vec![Box::new(source), Box::new(beads_source)],
            store.clone(),
            runner(&[native]),
            config(),
            Arc::new(NoopNotifier),
        );
        let mut snapshots = handle.subscribe();

        let first = wait_for(&mut snapshots, |snapshot| {
            !snapshot.refreshing
                && snapshot
                    .sources
                    .first()
                    .is_some_and(|status| status.connected)
                && snapshot.issues.iter().any(|item| item.key.native_id == "1")
        })
        .await;
        assert_eq!(first.selected_backend, Some(BackendKind::Native));
        assert_eq!(
            first
                .issues
                .iter()
                .map(|item| item.key.native_id.as_str())
                .collect::<Vec<_>>(),
            ["widgets-local", "1"]
        );

        handle.refresh().await.unwrap();
        let delta = wait_for(&mut snapshots, |snapshot| {
            !snapshot.refreshing && snapshot.issues.iter().any(|item| item.key.native_id == "2")
        })
        .await;
        assert_eq!(
            delta
                .issues
                .iter()
                .map(|item| item.key.native_id.as_str())
                .collect::<Vec<_>>(),
            ["widgets-local", "1", "2"]
        );
        assert_eq!(
            source_state.checkpoints.lock().unwrap()[1],
            Some(checkpoint)
        );

        let error = handle.refresh().await.unwrap_err();
        assert!(error.to_string().contains("temporary failure"));
        let failed = wait_for(&mut snapshots, |snapshot| {
            !snapshot.refreshing
                && snapshot
                    .sources
                    .iter()
                    .find(|status| status.name == source_name)
                    .is_some_and(|status| !status.connected)
        })
        .await;
        assert_eq!(failed.issues.len(), 3);
        assert!(
            failed
                .issues
                .iter()
                .any(|item| item.key.provider == IssueProvider::Beads)
        );
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("temporary failure")
        );
        assert_eq!(store.load_issues().await.unwrap().len(), 4);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_refresh_notifications_and_commands_are_serialized() {
        let store = Store::in_memory().await.unwrap();
        let (source, _source_state) = MockSource::new(vec![Ok(SyncResult {
            issues: vec![issue("7", "Repair runtime", "open")],
            checkpoint: SyncCheckpoint::default(),
            mode: SyncMode::Full,
        })]);
        let superset = MockBackend::new(BackendKind::Superset, true);
        let native = MockBackend::new(BackendKind::Native, true);
        let notifier = Arc::new(MockNotifier::default());
        let handle = RuntimeService::start_with_notifier(
            repository(),
            vec![Box::new(source)],
            store.clone(),
            runner(&[Arc::clone(&native), Arc::clone(&superset)]),
            config(),
            notifier.clone(),
        );
        let mut snapshots = handle.subscribe();
        let ready = wait_for(&mut snapshots, |snapshot| {
            !snapshot.refreshing && snapshot.issues.len() == 1
        })
        .await;
        assert_eq!(ready.selected_backend, Some(BackendKind::Superset));
        let issue_key = ready.issues[0].key.clone();

        handle.dispatch(issue_key.clone()).await.unwrap();
        handle.dispatch(issue_key).await.unwrap();
        wait_for(&mut snapshots, |snapshot| snapshot.runs.len() == 1).await;
        assert_eq!(superset.dispatches.load(Ordering::SeqCst), 1);
        assert_eq!(native.dispatches.load(Ordering::SeqCst), 0);
        assert_eq!(store.load_runs().await.unwrap().len(), 1);
        let prompt = superset.requests.lock().unwrap()[0].prompt.clone();
        for expected in [
            "Provider: github",
            "Repository: acme/widgets",
            "Identifier: #7",
            "Title: Repair runtime",
            "Description: Description for 7",
            "URL: https://example.com/acme/widgets/issues/7",
        ] {
            assert!(
                prompt.contains(expected),
                "missing prompt field: {expected}"
            );
        }

        superset.set_status(
            RunState::NeedsInput,
            Some("approval required"),
            Some("terminal output"),
        );
        handle.refresh().await.unwrap();
        assert_eq!(handle.snapshot().runs[0].state, RunState::NeedsInput);
        tokio::time::timeout(Duration::from_secs(1), async {
            while notifier.calls.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let events = store.load_events("run-1").await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 0);
        assert!(matches!(events[0].payload, RunEvent::StateChanged {
            state: RunState::NeedsInput,
            ..
        }));
        assert_eq!(events[1].sequence, 1);
        assert!(matches!(events[1].payload, RunEvent::Output { .. }));

        superset.set_status(
            RunState::NeedsInput,
            Some("approval required"),
            Some("terminal output"),
        );
        let refresh_count = superset.refreshes.load(Ordering::SeqCst);
        handle.refresh().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while superset.refreshes.load(Ordering::SeqCst) == refresh_count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.load_events("run-1").await.unwrap().len(), 2);
        assert_eq!(notifier.calls.load(Ordering::SeqCst), 1);

        superset.set_status(
            RunState::NeedsInput,
            Some("approval required"),
            Some("terminal output extended"),
        );
        handle.refresh().await.unwrap();
        let events = store.load_events("run-1").await.unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[2].payload,
            RunEvent::Output { text, .. } if text == " extended"
        ));

        handle.send_input("run-1", "approved").await.unwrap();
        wait_for(&mut snapshots, |snapshot| {
            snapshot.runs[0].state == RunState::Running
        })
        .await;
        handle.open("run-1").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while superset.opens.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.stop("run-1").await.unwrap();
        wait_for(&mut snapshots, |snapshot| {
            snapshot.runs[0].state == RunState::Cancelled
        })
        .await;
        assert_eq!(superset.inputs.load(Ordering::SeqCst), 1);
        assert_eq!(superset.stops.load(Ordering::SeqCst), 1);
        assert_eq!(superset.opens.load(Ordering::SeqCst), 1);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restored_active_run_missing_from_runner_is_persisted_as_disconnected() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let run = RunSummary {
            id: "restored-run".to_owned(),
            issue_key: issue("9", "Restored", "open").key.canonical(),
            workspace: None,
            agent: "opencode".to_owned(),
            state: RunState::Running,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        };
        store.insert_run(&run).await.unwrap();
        let native = MockBackend::new(BackendKind::Native, true);
        let handle = RuntimeService::start_with_notifier(
            repository(),
            Vec::new(),
            store.clone(),
            runner(&[native]),
            config(),
            Arc::new(NoopNotifier),
        );
        let mut snapshots = handle.subscribe();

        let snapshot = wait_for(&mut snapshots, |snapshot| {
            snapshot
                .runs
                .first()
                .is_some_and(|run| run.state == RunState::Disconnected)
        })
        .await;
        assert!(snapshot.error.as_deref().unwrap().contains("restored-run"));
        let persisted = store.load_run("restored-run").await.unwrap().unwrap();
        assert_eq!(persisted.state, RunState::Disconnected);
        assert!(matches!(
            store.load_events("restored-run").await.unwrap()[0].payload,
            RunEvent::StateChanged {
                state: RunState::Disconnected,
                ..
            }
        ));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restored_snapshot_event_history_is_bounded() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        let run = RunSummary {
            id: "eventful-run".to_owned(),
            issue_key: issue("10", "Eventful", "open").key.canonical(),
            workspace: None,
            agent: "opencode".to_owned(),
            state: RunState::Completed,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        };
        store.insert_run(&run).await.unwrap();
        for sequence in 0..300 {
            store
                .append_event(&EventEnvelope {
                    run_id: run.id.clone(),
                    sequence,
                    timestamp: now,
                    payload: RunEvent::AssistantMessage {
                        text: sequence.to_string(),
                    },
                })
                .await
                .unwrap();
        }
        let native = MockBackend::new(BackendKind::Native, true);
        let handle = RuntimeService::start_with_notifier(
            repository(),
            Vec::new(),
            store,
            runner(&[native]),
            config(),
            Arc::new(NoopNotifier),
        );
        let mut snapshots = handle.subscribe();

        let snapshot = wait_for(&mut snapshots, |snapshot| {
            snapshot
                .run_events
                .get("eventful-run")
                .is_some_and(|events| events.len() == MAX_IN_MEMORY_EVENTS)
        })
        .await;
        let events = &snapshot.run_events["eventful-run"];
        assert_eq!(events.first().unwrap().sequence, 44);
        assert_eq!(events.last().unwrap().sequence, 299);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stale_refresh_cannot_overwrite_stop_and_post_mutation_refresh_is_guaranteed() {
        let store = Store::in_memory().await.unwrap();
        let (source, _) = MockSource::new(vec![Ok(SyncResult {
            issues: vec![issue("11", "Stop safely", "open")],
            checkpoint: SyncCheckpoint::default(),
            mode: SyncMode::Full,
        })]);
        let native = MockBackend::new(BackendKind::Native, true);
        let handle = RuntimeService::start_with_notifier(
            repository(),
            vec![Box::new(source)],
            store.clone(),
            runner(&[Arc::clone(&native)]),
            config(),
            Arc::new(NoopNotifier),
        );
        let mut snapshots = handle.subscribe();
        let ready = wait_for(&mut snapshots, |snapshot| snapshot.issues.len() == 1).await;
        handle.dispatch(ready.issues[0].key.clone()).await.unwrap();

        let gate = native.block_next_refresh();
        let refresh_handle = handle.clone();
        let refresh = tokio::spawn(async move { refresh_handle.refresh().await });
        gate.started.acquire().await.unwrap().forget();
        let refreshing = wait_for(&mut snapshots, |snapshot| snapshot.refreshing).await;
        assert!(refreshing.refreshing);
        assert!(!refresh.is_finished());

        handle.stop("run-1").await.unwrap();
        gate.release.add_permits(1);
        refresh.await.unwrap().unwrap();

        let snapshot = handle.snapshot();
        assert_eq!(snapshot.runs[0].state, RunState::Cancelled);
        assert!(!snapshot.refreshing);
        assert_eq!(
            store.load_run("run-1").await.unwrap().unwrap().state,
            RunState::Cancelled
        );
        assert!(native.refreshes.load(Ordering::SeqCst) >= 2);
        handle.shutdown().await.unwrap();
    }
}
