//! Scheduler integration tests. All backend and source effects stay in this fixture.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use agent_launcher_core::WorktreeInspection;
use agent_launcher_issues::SourceKey;
use agent_launcher_runner::{Backend, BackendCapabilities, DispatchResult, OpenResult};
use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::*;

struct LocalSource {
    key: SourceKey,
    issues: Vec<Issue>,
    state: Arc<SourceControl>,
}

#[derive(Default)]
struct SourceControl {
    fail: AtomicBool,
    throttle: AtomicBool,
    retry_at: Mutex<Option<chrono::DateTime<Utc>>>,
    calls: AtomicUsize,
    deletes: AtomicUsize,
}

#[async_trait]
impl IssueSource for LocalSource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    fn supports_delete(&self) -> bool {
        self.key.provider == IssueProvider::Beads
    }

    async fn delete_issue(
        &self,
        _: &IssueKey,
    ) -> std::result::Result<(), agent_launcher_issues::Error> {
        self.state.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn retry_at(&self) -> Option<chrono::DateTime<Utc>> {
        *self.state.retry_at.lock().unwrap()
    }

    async fn sync(
        &self,
        _: Option<&SyncCheckpoint>,
    ) -> std::result::Result<SyncResult, agent_launcher_issues::Error> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        if self.state.throttle.load(Ordering::SeqCst) {
            *self.state.retry_at.lock().unwrap() = Some(Utc::now() + chrono::Duration::hours(1));
        }
        if let Some(retry_at) = self.retry_at() {
            return Err(agent_launcher_issues::Error::Throttled { retry_at });
        }
        if self.state.fail.load(Ordering::SeqCst) {
            return Err(agent_launcher_issues::Error::CommandTimeout);
        }
        Ok(SyncResult {
            issues: self.issues.clone(),
            checkpoint: SyncCheckpoint::default(),
            mode: SyncMode::Full,
        })
    }
}

struct LocalBackend {
    started: Semaphore,
    release: Semaphore,
    requests: Mutex<Vec<(String, DispatchRequest)>>,
    runs: Mutex<HashMap<String, RunSummary>>,
    away_refreshes: Mutex<Vec<String>>,
    manual_dispatches: AtomicUsize,
    stops: AtomicUsize,
    fail_launch: AtomicBool,
    unstarted_launch: AtomicBool,
}

impl LocalBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            requests: Mutex::new(Vec::new()),
            runs: Mutex::new(HashMap::new()),
            away_refreshes: Mutex::new(Vec::new()),
            manual_dispatches: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            fail_launch: AtomicBool::new(false),
            unstarted_launch: AtomicBool::new(false),
        })
    }

    fn run(id: &str, issue: &Issue) -> RunSummary {
        RunSummary {
            id: id.into(),
            issue_key: issue.key.canonical(),
            confidential: false,
            workspace: None,
            agent: "opencode".into(),
            model: None,
            state: RunState::Running,
            message: None,
            session_id: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn launches(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    async fn wait_started(&self, count: u32) {
        tokio::time::timeout(Duration::from_secs(5), self.started.acquire_many(count))
            .await
            .expect("scheduler did not start expected launches")
            .unwrap()
            .forget();
    }
}

#[async_trait]
impl Backend for LocalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Herdr
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new([
            Capability::Detect,
            Capability::Dispatch,
            Capability::Refresh,
            Capability::Away,
            Capability::Stop,
        ])
    }

    async fn owns_run(&self, id: &str) -> bool {
        self.runs.lock().unwrap().contains_key(id)
    }

    async fn detect(&self, _: &Repository) -> agent_launcher_runner::Result<BackendDetection> {
        Ok(BackendDetection {
            backend: self.kind(),
            available: true,
            manager_running: true,
            capabilities: self.capabilities(),
            message: None,
            compute_targets: Vec::new(),
        })
    }

    async fn dispatch(
        &self,
        request: DispatchRequest,
    ) -> agent_launcher_runner::Result<DispatchResult> {
        let number = self.manual_dispatches.fetch_add(1, Ordering::SeqCst);
        let run = Self::run(&format!("manual-{number}"), &request.issue);
        self.runs
            .lock()
            .unwrap()
            .insert(run.id.clone(), run.clone());
        Ok(DispatchResult {
            run,
            capabilities: self.capabilities(),
        })
    }

    async fn dispatch_away(
        &self,
        request: DispatchRequest,
        id: &str,
    ) -> agent_launcher_runner::Result<DispatchResult> {
        let run = Self::run(id, &request.issue);
        self.requests.lock().unwrap().push((id.into(), request));
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        if self.unstarted_launch.swap(false, Ordering::SeqCst) {
            return Err(RunnerError::AwayNotStarted(
                "local preflight rejected launch".into(),
            ));
        }
        if self.fail_launch.load(Ordering::SeqCst) {
            return Err(RunnerError::InvalidResponse(
                "local launch outcome unknown".into(),
            ));
        }
        self.runs.lock().unwrap().insert(id.into(), run.clone());
        Ok(DispatchResult {
            run,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh_away(&self, id: &str) -> agent_launcher_runner::Result<StatusResult> {
        self.away_refreshes.lock().unwrap().push(id.into());
        let run = self
            .runs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| RunnerError::RunNotFound(id.into()))?;
        Ok(StatusResult { run, output: None })
    }

    async fn refresh(&self, _: &str) -> agent_launcher_runner::Result<StatusResult> {
        panic!("Away workers must use refresh_away, not interactive refresh")
    }

    async fn send_input(&self, _: &str, _: &str) -> agent_launcher_runner::Result<()> {
        panic!("unexpected input")
    }

    async fn stop(&self, _: &str) -> agent_launcher_runner::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn open(&self, _: &str) -> agent_launcher_runner::Result<OpenResult> {
        panic!("unexpected open")
    }

    async fn delete_worktree(
        &self,
        _: &str,
        _: bool,
        _: Option<&WorktreeInspection>,
    ) -> agent_launcher_runner::Result<()> {
        panic!("unexpected deletion")
    }
}

struct Fixture {
    service: RuntimeService,
    backend: Arc<LocalBackend>,
    source: Arc<SourceControl>,
    // Keep the command/watch channels alive without spawning a runtime loop.
    _handle: RuntimeHandle,
}

fn issue(number: usize) -> Issue {
    Issue {
        key: IssueKey {
            provider: IssueProvider::Github,
            host: "example.invalid".into(),
            repository: "tests/scheduler".into(),
            native_id: number.to_string(),
        },
        identifier: format!("#{number}"),
        title: format!("Implement task {number}"),
        description: Some("Deterministic local test issue".into()),
        state: "open".into(),
        priority: Some(number as i64),
        security_advisory: None,
        pull_request: None,
        activity: None,
        url: None,
        author: None,
        labels: Vec::new(),
        parent_id: None,
        blocked_by: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

impl Fixture {
    async fn new(count: usize) -> Self {
        Self::with_store(
            count,
            Store::in_memory().await.unwrap(),
            LocalBackend::new(),
        )
        .await
    }

    async fn with_store(count: usize, store: Store, backend: Arc<LocalBackend>) -> Self {
        let source = Arc::new(SourceControl::default());
        let local = LocalSource {
            key: SourceKey {
                provider: IssueProvider::Github,
                host: "example.invalid".into(),
                repository: "tests/scheduler".into(),
            },
            issues: (1..=count).map(issue).collect(),
            state: source.clone(),
        };
        let root = store
            .path()
            .and_then(|path| path.parent())
            .unwrap_or_else(|| std::path::Path::new("/unused-away-test"));
        let repository = Repository {
            root: root.into(),
            git_dir: root.join(".git"),
            remote: None,
            has_beads: false,
        };
        let mut config = AppConfig::default();
        config.agent.name = "opencode".into();
        config.backend = BackendConfig::Herdr;
        config.notifications.desktop = false;
        let runner = Arc::new(Runner::new([backend.clone() as Arc<dyn Backend>]));
        let (mut service, handle) = RuntimeService::new_with_notifier(
            repository,
            vec![Box::new(local)],
            store,
            runner,
            config,
            Arc::new(NoopNotifier),
        );
        service.initialize().await;
        Self {
            service,
            backend,
            source,
            _handle: handle,
        }
    }

    async fn start(&mut self, max_agents: usize) {
        self.service
            .away_command(RuntimeCommand::StartAway {
                max_agents,
                profile: None,
                ranking: AwayRanking::SourcePriority,
            })
            .await
            .unwrap();
        self.drain_work().await;
        self.service.reconcile_away().await;
    }

    async fn drain_work(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(result) = self.service.work.join_next().await {
                self.service.handle_work_result(result.unwrap()).await;
            }
        })
        .await
        .expect("local refresh work stalled");
    }

    async fn complete_launches(&mut self, count: usize) {
        self.backend.release.add_permits(count);
        for _ in 0..count {
            let result =
                tokio::time::timeout(Duration::from_secs(5), self.service.away.work.join_next())
                    .await
                    .expect("local launch stalled")
                    .expect("missing launch work");
            self.service.handle_away_result(result).await;
            self.service.reconcile_away().await;
        }
        self.drain_work().await;
        self.service.reconcile_away().await;
    }

    async fn status(&mut self, id: &str, state: RunState) {
        self.backend.runs.lock().unwrap().get_mut(id).unwrap().state = state;
        self.service.launch_run_refresh(id);
        self.drain_work().await;
        self.service.reconcile_away().await;
    }

    async fn command(&mut self, command: RuntimeCommand) -> Result<()> {
        let (acknowledge, response) = oneshot::channel();
        self.service
            .handle_command(CommandRequest::Command {
                command,
                acknowledge,
            })
            .await;
        response.await.unwrap()
    }

    fn reserved(&self) -> usize {
        self.service
            .snapshot
            .away
            .entries
            .iter()
            .filter(|entry| entry.run_id.is_some())
            .count()
    }
}

#[tokio::test]
async fn away_pending_launches_reserve_five_durably_and_never_admit_six() {
    let mut f = Fixture::new(9).await;
    f.start(5).await;
    f.backend.wait_started(5).await;
    for _ in 0..8 {
        f.service.reconcile_away().await;
        f.service.launch_run_refreshes();
    }
    assert_eq!(f.backend.launches(), 5);
    assert_eq!(f.reserved(), 5);
    assert_eq!(f.service.occupied_away_slots(), 5);
    assert_eq!(f.service.away.launching.len(), 5);
    assert!(f.service.snapshot.runs.is_empty());
    assert!(
        f.service.work.is_empty(),
        "pending launches must not be polled as missing workers"
    );
    let saved = f.service.store.load_away().await.unwrap().unwrap();
    assert_eq!(saved, f.service.snapshot.away);
    assert_eq!(
        saved
            .entries
            .iter()
            .filter(|e| e.state == AwayEntryState::Launching)
            .count(),
        5
    );
    f.complete_launches(5).await;
    assert_eq!(f.backend.launches(), 5);
    assert_eq!(f.service.snapshot.runs.len(), 5);
    assert_eq!(f.service.occupied_away_slots(), 5);
}

#[tokio::test]
async fn away_one_exit_starts_exactly_one_replacement() {
    let mut f = Fixture::new(9).await;
    f.start(5).await;
    f.complete_launches(5).await;
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    // Consume the five initial start notifications plus the replacement.
    f.backend.wait_started(6).await;
    for _ in 0..4 {
        f.service.reconcile_away().await;
    }
    assert_eq!(f.backend.launches(), 6);
    assert_eq!(f.service.occupied_away_slots(), 5);
    assert_eq!(f.service.away.launching.len(), 1);
    assert_eq!(
        f.service
            .snapshot
            .away
            .entries
            .iter()
            .filter(|e| e.state == AwayEntryState::Finished)
            .count(),
        1
    );
}

#[tokio::test]
async fn away_finished_open_issue_is_not_relaunched_after_refresh_or_reprioritize() {
    let mut f = Fixture::new(1).await;
    f.start(1).await;
    f.complete_launches(1).await;
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    f.service
        .away_command(RuntimeCommand::ReprioritizeAway {
            ranking: AwayRanking::SourcePriority,
        })
        .await
        .unwrap();
    f.drain_work().await;
    f.service.reconcile_away().await;
    assert_eq!(f.service.snapshot.issues[0].state, "open");
    assert_eq!(f.backend.launches(), 1);
    assert_eq!(f.reserved(), 1);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::QueueEmpty);
    assert_eq!(
        f.service.snapshot.away.entries[0].state,
        AwayEntryState::Finished
    );
}

#[tokio::test]
async fn away_source_error_blocks_cached_queue_until_successful_refresh() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    f.complete_launches(1).await;
    f.source.fail.store(true, Ordering::SeqCst);
    f.service.launch_source_refreshes();
    f.drain_work().await;
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    assert_eq!(f.service.snapshot.issues.len(), 3);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Refreshing);
    assert_eq!(f.backend.launches(), 1);
    assert!(f.service.away.work.is_empty());
    f.source.fail.store(false, Ordering::SeqCst);
    f.service.launch_source_refreshes();
    f.drain_work().await;
    f.service.reconcile_away().await;
    f.backend.wait_started(2).await;
    assert_eq!(f.backend.launches(), 2);
}

#[tokio::test]
async fn away_throttled_connected_source_blocks_refill() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    f.complete_launches(1).await;
    f.source.throttle.store(true, Ordering::SeqCst);
    f.service.launch_source_refreshes();
    f.drain_work().await;
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    assert!(
        f.service.snapshot.sources[0].connected,
        "public throttled cache stays connected"
    );
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Refreshing);
    assert_eq!(f.backend.launches(), 1);
    assert!(f.service.away.work.is_empty());
    let calls = f.source.calls.load(Ordering::SeqCst);
    f.service.launch_due_sources(true, false);
    f.drain_work().await;
    f.service.reconcile_away().await;
    assert_eq!(f.source.calls.load(Ordering::SeqCst), calls);
    assert_eq!(f.backend.launches(), 1);
}

#[tokio::test]
async fn away_source_refresh_in_flight_blocks_new_reservations() {
    let mut f = Fixture::new(3).await;
    f.service
        .away_command(RuntimeCommand::StartAway {
            max_agents: 2,
            profile: None,
            ranking: AwayRanking::SourcePriority,
        })
        .await
        .unwrap();
    assert!(!f.service.source_in_flight.is_empty());
    f.service.reconcile_away().await;
    assert_eq!(f.reserved(), 0);
    assert!(f.service.away.work.is_empty());
    f.drain_work().await;
    f.service.reconcile_away().await;
    f.backend.wait_started(2).await;
    assert_eq!(f.reserved(), 2);
}

#[tokio::test]
async fn away_pause_during_pending_launch_records_result_without_refill() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    f.backend.wait_started(1).await;
    f.command(RuntimeCommand::PauseAway).await.unwrap();
    f.complete_launches(1).await;
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Paused);
    assert_eq!(f.backend.launches(), 1);
    assert!(f.service.away.launching.is_empty());
    assert_eq!(
        f.service.store.load_runs().await.unwrap()[0].state,
        RunState::Completed
    );
    assert_eq!(
        f.service.store.load_away().await.unwrap().unwrap().entries[0].state,
        AwayEntryState::Finished
    );
}

#[tokio::test]
async fn away_manual_during_pending_launch_drains_and_records_result() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    f.backend.wait_started(1).await;
    f.command(RuntimeCommand::SetManual).await.unwrap();
    f.complete_launches(1).await;
    assert_eq!(f.service.snapshot.away.mode, AppMode::Manual);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Draining);
    let id = f.service.snapshot.runs[0].id.clone();
    f.status(&id, RunState::Completed).await;
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Inactive);
    assert_eq!(f.backend.launches(), 1);
    assert_eq!(
        f.service.store.load_runs().await.unwrap()[0].state,
        RunState::Completed
    );
}

#[tokio::test]
async fn away_restart_recovers_missing_reserved_run_via_refresh_without_dispatch() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    f.backend.wait_started(1).await;
    let id = f.service.snapshot.away.entries[0].run_id.clone().unwrap();
    let key = f.service.snapshot.away.entries[0].issue.clone();
    let issue = f
        .service
        .snapshot
        .issues
        .iter()
        .find(|issue| issue.key == key)
        .unwrap();
    // Simulate a backend launch surviving a crash before its result reached SQLite.
    f.backend
        .runs
        .lock()
        .unwrap()
        .insert(id.clone(), LocalBackend::run(&id, issue));
    let store = f.service.store.clone();
    let backend = f.backend.clone();
    drop(f);
    let mut restarted = Fixture::with_store(3, store, backend).await;
    assert_eq!(restarted.service.snapshot.away.phase, AwayPhase::Paused);
    assert!(restarted.service.snapshot.runs.is_empty());
    restarted.service.launch_run_refreshes();
    restarted.drain_work().await;
    restarted.service.reconcile_away().await;
    assert_eq!(*restarted.backend.away_refreshes.lock().unwrap(), vec![
        id.clone()
    ]);
    assert_eq!(restarted.backend.launches(), 1);
    assert_eq!(restarted.service.snapshot.runs[0].id, id);
    assert_eq!(
        restarted.service.snapshot.away.entries[0].state,
        AwayEntryState::Running
    );
    assert_eq!(restarted.service.snapshot.away.phase, AwayPhase::Paused);
    assert_eq!(restarted.service.store.load_runs().await.unwrap().len(), 1);
    assert!(restarted.service.away.work.is_empty());
}

#[tokio::test]
async fn away_manual_dispatch_guards_pending_capacity_and_duplicate_in_manual_mode() {
    let mut f = Fixture::new(3).await;
    f.start(1).await;
    let reserved = f.service.snapshot.away.entries[0].issue.clone();
    let other = f.service.snapshot.away.entries[1].issue.clone();
    let dispatch = |issue| RuntimeCommand::Dispatch {
        issue,
        profile: None,
        target: None,
        options: Default::default(),
    };
    let capacity = f.command(dispatch(other.clone())).await.unwrap_err();
    assert!(capacity.to_string().contains("concurrency"), "{capacity}");
    f.command(RuntimeCommand::SetManual).await.unwrap();
    let duplicate = f.command(dispatch(reserved)).await.unwrap_err();
    assert!(duplicate.to_string().contains("reserved"), "{duplicate}");
    assert_eq!(f.backend.manual_dispatches.load(Ordering::SeqCst), 0);
    f.command(dispatch(other)).await.unwrap();
    assert_eq!(f.backend.manual_dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn away_limit_changes_count_pending_and_do_not_cancel_existing_workers() {
    let mut f = Fixture::new(10).await;
    f.start(5).await;
    f.backend.wait_started(5).await;
    f.command(RuntimeCommand::SetAwayConcurrency { max_agents: 2 })
        .await
        .unwrap();
    f.service.reconcile_away().await;
    assert_eq!(f.service.occupied_away_slots(), 5);
    assert_eq!(f.backend.launches(), 5);
    f.command(RuntimeCommand::SetAwayConcurrency { max_agents: 6 })
        .await
        .unwrap();
    f.service.reconcile_away().await;
    f.backend.wait_started(1).await;
    assert_eq!(f.backend.launches(), 6);
    assert_eq!(f.service.occupied_away_slots(), 6);
    assert_eq!(f.backend.stops.load(Ordering::SeqCst), 0);
    for invalid in [0, 65, usize::MAX] {
        assert!(
            f.command(RuntimeCommand::SetAwayConcurrency {
                max_agents: invalid
            })
            .await
            .is_err()
        );
        assert_eq!(f.service.snapshot.away.max_agents, 6);
    }
    assert_eq!(
        f.service
            .store
            .load_away()
            .await
            .unwrap()
            .unwrap()
            .max_agents,
        6
    );
}

#[tokio::test]
async fn away_malformed_ranking_requires_explicit_recovery() {
    let mut f = Fixture::new(2).await;
    f.service.snapshot.away.mode = AppMode::Away;
    f.service.snapshot.away.ranking = AwayRanking::SourcePriority;
    f.service.snapshot.away.prioritizing = true;
    let generation = f.service.away.generation;
    f.service.away.work.spawn(async move {
        AwayWork::Ranked {
            generation,
            inventory: vec![issue(1), issue(2)],
            result: Err(RunnerError::InvalidResponse("malformed ranking JSON".into()).into()),
        }
    });
    let result = f.service.away.work.join_next().await.unwrap();
    f.service.handle_away_result(result).await;
    f.service.reconcile_away().await;
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    assert!(!f.service.snapshot.away.prioritizing);
    assert!(
        f.service
            .snapshot
            .away
            .error
            .as_deref()
            .unwrap()
            .contains("malformed ranking")
    );
    assert_eq!(f.backend.launches(), 0);
    assert_eq!(f.reserved(), 0);
}

#[tokio::test]
async fn away_stale_ranking_success_or_error_cannot_unpause_or_leave_manual() {
    for manual in [false, true] {
        for malformed in [false, true] {
            let mut f = Fixture::new(2).await;
            f.service.snapshot.away.mode = AppMode::Away;
            f.service.snapshot.away.prioritizing = true;
            let generation = f.service.away.generation;
            f.command(if manual {
                RuntimeCommand::SetManual
            } else {
                RuntimeCommand::PauseAway
            })
            .await
            .unwrap();
            f.service.away.work.spawn(async move {
                AwayWork::Ranked {
                    generation,
                    inventory: vec![issue(1), issue(2)],
                    result: if malformed {
                        Err(RunnerError::InvalidResponse("stale malformed ranking".into()).into())
                    } else {
                        Ok(crate::prioritize::source_priority(vec![issue(1), issue(2)]))
                    },
                }
            });
            let result = f.service.away.work.join_next().await.unwrap();
            f.service.handle_away_result(result).await;
            f.service.reconcile_away().await;
            assert_eq!(
                f.service.snapshot.away.phase,
                if manual {
                    AwayPhase::Inactive
                } else {
                    AwayPhase::Paused
                }
            );
            assert!(!f.service.snapshot.away.prioritizing);
            assert!(f.service.snapshot.away.entries.is_empty());
            assert_eq!(f.backend.launches(), 0);
        }
    }
}

#[tokio::test]
async fn away_three_distinct_worker_failures_trip_circuit_breaker_once() {
    let mut f = Fixture::new(8).await;
    f.start(3).await;
    f.complete_launches(3).await;
    let ids: Vec<_> = f
        .service
        .snapshot
        .runs
        .iter()
        .map(|run| run.id.clone())
        .collect();
    for (index, id) in ids.iter().enumerate() {
        f.status(id, RunState::Failed).await;
        for _ in 0..3 {
            f.service.reconcile_away().await;
        }
        assert_eq!(
            f.service.away.failures,
            index + 1,
            "repeated polls must not count one failure twice"
        );
    }
    f.backend.wait_started(5).await;
    assert_eq!(
        f.backend.launches(),
        5,
        "third failure must not admit a replacement"
    );
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    assert!(
        f.service
            .snapshot
            .away
            .error
            .as_deref()
            .unwrap()
            .contains("Three workers failed")
    );
    f.complete_launches(2).await;
    assert_eq!(f.backend.launches(), 5);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
}

#[tokio::test]
async fn away_uncertain_launch_keeps_reservation_and_blocks_replacement() {
    let mut f = Fixture::new(3).await;
    f.backend.fail_launch.store(true, Ordering::SeqCst);
    f.start(1).await;
    f.complete_launches(1).await;
    for _ in 0..4 {
        f.service.reconcile_away().await;
    }
    assert_eq!(f.backend.launches(), 1);
    assert_eq!(f.reserved(), 1);
    assert_eq!(f.service.occupied_away_slots(), 1);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    assert_eq!(
        f.service.snapshot.away.entries[0].state,
        AwayEntryState::Attention
    );
    assert!(
        f.service.snapshot.away.entries[0]
            .error
            .as_deref()
            .unwrap()
            .contains("uncertain")
    );
    assert_eq!(
        f.service.store.load_away().await.unwrap().unwrap(),
        f.service.snapshot.away
    );
}

#[tokio::test]
async fn away_persistent_owner_denies_second_runtime_admission_and_mode_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let first =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), LocalBackend::new()).await;
    assert!(first.service.away.owner_error.is_none());
    let mut second =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), LocalBackend::new()).await;
    assert!(second.service.away.owner_error.is_some());
    for command in [
        RuntimeCommand::StartAway {
            max_agents: 5,
            profile: None,
            ranking: AwayRanking::SourcePriority,
        },
        RuntimeCommand::ResumeAway,
        RuntimeCommand::PauseAway,
        RuntimeCommand::SetManual,
        RuntimeCommand::SetAwayConcurrency { max_agents: 2 },
        RuntimeCommand::ReprioritizeAway {
            ranking: AwayRanking::SourcePriority,
        },
        RuntimeCommand::Dispatch {
            issue: issue(1).key,
            profile: None,
            target: None,
            options: Default::default(),
        },
        RuntimeCommand::Review {
            issue: issue(1).key,
            profile: None,
            target: None,
            options: Default::default(),
        },
        RuntimeCommand::DispatchSecurity {
            issue: issue(1).key,
            profile: None,
            options: Default::default(),
            consent: true,
        },
    ] {
        let error = second.command(command).await.unwrap_err();
        assert!(error.to_string().contains("ownership"), "{error}");
    }
    second.service.reconcile_away().await;
    second.service.launch_run_refreshes();
    assert_eq!(second.backend.launches(), 0);
    assert_eq!(second.backend.manual_dispatches.load(Ordering::SeqCst), 0);
    assert!(second.service.store.load_away().await.unwrap().is_none());
    drop(first);
    drop(second);
    let third =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), LocalBackend::new()).await;
    assert!(
        third.service.away.owner_error.is_none(),
        "owner lock must release when runtime is dropped"
    );
}

#[tokio::test]
async fn away_persistent_nonowner_cannot_stop_owners_worker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let backend = LocalBackend::new();
    let mut first =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), backend.clone()).await;
    first.start(1).await;
    first.complete_launches(1).await;
    let id = first.service.snapshot.runs[0].id.clone();
    let mut second =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), backend.clone()).await;
    assert!(second.service.away.owner_error.is_some());
    let result = second.command(RuntimeCommand::Stop { run_id: id }).await;
    assert!(
        result.is_err(),
        "nonowner runtime must reject worker mutations"
    );
    assert_eq!(backend.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn away_definitely_unstarted_launch_persists_failure_and_refills_with_different_issue() {
    let mut f = Fixture::new(2).await;
    f.backend.unstarted_launch.store(true, Ordering::SeqCst);
    f.start(1).await;
    f.backend.wait_started(1).await;
    let failed_id = f.service.snapshot.away.entries[0].run_id.clone().unwrap();
    let failed_key = f.service.snapshot.away.entries[0].issue.clone();
    f.backend.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), f.service.away.work.join_next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        &result,
        Ok(AwayWork::Launched {
            result: Err(RunnerError::AwayNotStarted(_)),
            ..
        })
    ));
    f.service.handle_away_result(result).await;
    assert_eq!(f.service.occupied_away_slots(), 0);
    assert!(f.service.away.launching.is_empty());
    let saved = f.service.store.load_runs().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, failed_id);
    assert_eq!(saved[0].state, RunState::Failed);
    assert!(
        saved[0]
            .message
            .as_deref()
            .unwrap()
            .contains("local preflight")
    );
    assert!(f.backend.runs.lock().unwrap().is_empty());
    f.service.reconcile_away().await;
    f.backend.wait_started(1).await;
    assert_eq!(f.service.away.failures, 1);
    assert_eq!(f.service.occupied_away_slots(), 1);
    assert_eq!(f.backend.launches(), 2);
    assert_ne!(
        f.backend.requests.lock().unwrap()[1].1.issue.key,
        failed_key
    );
    f.complete_launches(1).await;
    let next_id = f.service.snapshot.away.entries[1].run_id.clone().unwrap();
    f.status(&next_id, RunState::Completed).await;
    f.command(RuntimeCommand::ReprioritizeAway {
        ranking: AwayRanking::SourcePriority,
    })
    .await
    .unwrap();
    f.drain_work().await;
    f.service.reconcile_away().await;
    assert_eq!(f.backend.launches(), 2);
    assert_eq!(f.service.occupied_away_slots(), 0);
    assert!(f.service.away.work.is_empty());
    assert_eq!(
        f.service.snapshot.away.entries[0].run_id.as_deref(),
        Some(failed_id.as_str())
    );
    assert_eq!(
        f.service.snapshot.away.entries[0].state,
        AwayEntryState::Attention
    );
    assert_eq!(
        f.service.store.load_away().await.unwrap().unwrap(),
        f.service.snapshot.away
    );
}

#[tokio::test]
async fn away_recovered_launching_intent_without_registry_record_becomes_failed() {
    let mut f = Fixture::new(1).await;
    f.start(1).await;
    f.backend.wait_started(1).await;
    let id = f.service.snapshot.away.entries[0].run_id.clone().unwrap();
    assert!(f.backend.runs.lock().unwrap().is_empty());
    assert!(f.service.store.load_runs().await.unwrap().is_empty());
    let store = f.service.store.clone();
    let backend = f.backend.clone();
    drop(f);
    let mut f = Fixture::with_store(1, store, backend).await;
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Paused);
    assert_eq!(
        f.service.snapshot.away.entries[0].state,
        AwayEntryState::Launching
    );
    assert_eq!(f.service.occupied_away_slots(), 1);
    f.service.launch_run_refreshes();
    let result = tokio::time::timeout(Duration::from_secs(5), f.service.work.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(&result, WorkResult::Run { run_id, result: Err(RunnerError::RunNotFound(_)), .. } if run_id == &id)
    );
    f.service.handle_work_result(result).await;
    f.service.reconcile_away().await;
    assert_eq!(f.service.snapshot.runs[0].state, RunState::Failed);
    assert_eq!(f.service.store.load_runs().await.unwrap()[0].id, id);
    assert_eq!(
        f.service.store.load_runs().await.unwrap()[0].state,
        RunState::Failed
    );
    assert_eq!(
        f.service.snapshot.away.entries[0].state,
        AwayEntryState::Attention
    );
    assert_eq!(f.service.occupied_away_slots(), 0);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Paused);
    for _ in 0..3 {
        f.service.launch_run_refreshes();
        f.service.reconcile_away().await;
        assert!(
            f.service.work.is_empty(),
            "terminal recovery must not poll forever"
        );
    }
    f.command(RuntimeCommand::ResumeAway).await.unwrap();
    f.drain_work().await;
    f.service.reconcile_away().await;
    assert_eq!(
        f.backend.launches(),
        1,
        "the original issue must not be retried"
    );
    assert!(f.service.away.work.is_empty());
}

#[tokio::test]
async fn away_deleted_failed_and_finished_attempts_remain_durable_unoccupied_tombstones() {
    for state in [RunState::Failed, RunState::Completed] {
        let mut f = Fixture::new(1).await;
        f.start(1).await;
        f.complete_launches(1).await;
        let id = f.service.snapshot.runs[0].id.clone();
        f.status(&id, state).await;
        assert_eq!(
            f.service.snapshot.away.entries[0].state,
            if state == RunState::Failed {
                AwayEntryState::Attention
            } else {
                AwayEntryState::Finished
            }
        );
        f.service.resolve_deleted_away_run(&id).await.unwrap();
        // Reconciliation must preserve the tombstone even before run deletion commits.
        f.service.reconcile_away().await;
        assert_eq!(
            f.service.snapshot.away.entries[0].state,
            AwayEntryState::Skipped
        );
        f.service.store.delete_run(&id).await.unwrap();
        f.service.snapshot.runs.retain(|run| run.id != id);
        f.backend.runs.lock().unwrap().remove(&id);
        let refreshes = f.backend.away_refreshes.lock().unwrap().len();
        for _ in 0..3 {
            f.service.reconcile_away().await;
            f.service.launch_run_refreshes();
            assert!(f.service.work.is_empty());
            assert_eq!(f.service.occupied_away_slots(), 0);
        }
        let saved = f.service.store.load_away().await.unwrap().unwrap();
        assert_eq!(saved.entries[0].state, AwayEntryState::Skipped);
        assert_eq!(saved.entries[0].run_id.as_deref(), Some(id.as_str()));
        assert!(f.service.store.load_runs().await.unwrap().is_empty());
        let store = f.service.store.clone();
        let backend = f.backend.clone();
        drop(f);
        let mut f = Fixture::with_store(1, store, backend).await;
        f.service.launch_run_refreshes();
        assert!(
            f.service.work.is_empty(),
            "restart must not recover deleted attempts"
        );
        f.command(RuntimeCommand::ResumeAway).await.unwrap();
        f.drain_work().await;
        f.service.reconcile_away().await;
        assert_eq!(
            f.service.snapshot.away.entries[0].state,
            AwayEntryState::Skipped
        );
        assert_eq!(f.backend.away_refreshes.lock().unwrap().len(), refreshes);
        assert_eq!(f.backend.launches(), 1);
        assert_eq!(f.service.occupied_away_slots(), 0);
    }
}

#[tokio::test]
async fn away_pending_reservation_blocks_beads_deletion_without_a_stored_run() {
    let mut f = Fixture::new(0).await;
    let mut beads = issue(1);
    beads.key.provider = IssueProvider::Beads;
    beads.key.host = "local".into();
    beads.key.repository = f.service.repository.root.to_string_lossy().into_owned();
    let source = LocalSource {
        key: SourceKey {
            provider: beads.key.provider,
            host: beads.key.host.clone(),
            repository: beads.key.repository.clone(),
        },
        issues: vec![beads.clone()],
        state: f.source.clone(),
    };
    let name = source.cache_key();
    f.service.snapshot.sources = vec![SourceStatus {
        name: name.clone(),
        supports_delete: true,
        connected: false,
        message: None,
    }];
    f.service.sources = vec![Arc::new(source)];
    f.start(1).await;
    f.backend.wait_started(1).await;
    assert!(f.service.store.load_runs().await.unwrap().is_empty());
    assert!(f.backend.runs.lock().unwrap().is_empty());
    assert_eq!(
        f.service.store.load_source_issues(&name).await.unwrap(),
        vec![beads.clone()]
    );
    // Manual mode must not revoke the still-pending reservation's deletion guard.
    for manual in [false, true] {
        if manual {
            f.command(RuntimeCommand::SetManual).await.unwrap();
        }
        let error = f
            .command(RuntimeCommand::DeleteIssue {
                issue: beads.key.clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::DeleteIssueRejected(_)), "{error}");
        assert!(error.to_string().contains("Away attempt"), "{error}");
        assert_eq!(f.source.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.service.store.load_source_issues(&name).await.unwrap(),
            vec![beads.clone()]
        );
    }
    f.complete_launches(1).await;
    assert_eq!(f.service.snapshot.runs.len(), 1);
    assert_eq!(f.source.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn away_failure_after_disconnected_attention_is_counted_exactly_once() {
    let mut f = Fixture::new(3).await;
    f.start(3).await;
    f.complete_launches(3).await;
    let ids: Vec<_> = f
        .service
        .snapshot
        .runs
        .iter()
        .map(|run| run.id.clone())
        .collect();
    for id in &ids {
        f.status(id, RunState::Disconnected).await;
        assert_eq!(f.service.away.failures, 0);
        assert_eq!(
            f.service
                .snapshot
                .away
                .entries
                .iter()
                .find(|e| e.run_id.as_deref() == Some(id))
                .unwrap()
                .state,
            AwayEntryState::Attention
        );
    }
    for (index, id) in ids.iter().enumerate() {
        f.status(id, RunState::Failed).await;
        assert_eq!(f.service.away.failures, index + 1);
        f.status(id, RunState::Disconnected).await;
        f.status(id, RunState::Failed).await;
        for _ in 0..3 {
            f.service.reconcile_away().await;
        }
        assert_eq!(
            f.service.away.failures,
            index + 1,
            "status flapping must not double count a failure"
        );
    }
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    assert!(
        f.service
            .snapshot
            .away
            .error
            .as_deref()
            .unwrap()
            .contains("Three workers failed")
    );
    assert_eq!(f.backend.launches(), 3);
    assert_eq!(f.service.occupied_away_slots(), 0);
}

#[tokio::test]
async fn away_same_generation_rank_success_cannot_clear_launch_attention() {
    let mut f = Fixture::new(2).await;
    f.start(1).await;
    f.backend.fail_launch.store(true, Ordering::SeqCst);
    f.service.snapshot.away.prioritizing = true;
    let generation = f.service.away.generation;
    f.complete_launches(1).await;
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    let entries = f.service.snapshot.away.entries.clone();
    let error = f.service.snapshot.away.error.clone();
    f.service.away.work.spawn(async move {
        AwayWork::Ranked {
            generation,
            inventory: vec![issue(1), issue(2)],
            result: Ok(crate::prioritize::source_priority(vec![issue(1), issue(2)])),
        }
    });
    let result = f.service.away.work.join_next().await.unwrap();
    f.service.handle_away_result(result).await;
    f.service.reconcile_away().await;
    assert_eq!(f.service.away.generation, generation);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Attention);
    assert_eq!(f.service.snapshot.away.error, error);
    assert_eq!(f.service.snapshot.away.entries, entries);
    assert!(!f.service.snapshot.away.prioritizing);
    assert_eq!(f.backend.launches(), 1);
}

#[tokio::test]
async fn away_same_generation_rank_success_cannot_clear_persistence_pause() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let mut f =
        Fixture::with_store(2, Store::open(&path).await.unwrap(), LocalBackend::new()).await;
    f.service.launch_source_refreshes();
    f.drain_work().await;
    f.service.snapshot.away.mode = AppMode::Away;
    f.service.snapshot.away.ranking = AwayRanking::SourcePriority;
    f.service.snapshot.away.phase = AwayPhase::Prioritizing;
    f.service.snapshot.away.prioritizing = true;
    f.service.save_away().await.unwrap();
    let generation = f.service.away.generation;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_away_save BEFORE INSERT ON away_state BEGIN SELECT RAISE(FAIL, 'fixture persistence failure'); END")
        .execute(&pool).await.unwrap();
    assert!(f.service.save_away().await.is_err());
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Paused);
    let error = f.service.snapshot.away.error.clone();
    assert!(
        error
            .as_deref()
            .unwrap()
            .contains("fixture persistence failure")
    );
    // Make persistence healthy again: the delayed result still must not resume work.
    sqlx::query("DROP TRIGGER reject_away_save")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    f.service.away.work.spawn(async move {
        AwayWork::Ranked {
            generation,
            inventory: vec![issue(1), issue(2)],
            result: Ok(crate::prioritize::source_priority(vec![issue(1), issue(2)])),
        }
    });
    let result = f.service.away.work.join_next().await.unwrap();
    f.service.handle_away_result(result).await;
    f.service.reconcile_away().await;
    assert_eq!(f.service.away.generation, generation);
    assert_eq!(f.service.snapshot.away.phase, AwayPhase::Paused);
    assert_eq!(f.service.snapshot.away.error, error);
    assert!(f.service.snapshot.away.entries.is_empty());
    assert!(!f.service.snapshot.away.prioritizing);
    assert_eq!(f.backend.launches(), 0);
    assert!(f.service.away.work.is_empty());
}

#[tokio::test]
async fn away_start_owned_rejects_guard_for_wrong_store_scope() {
    let dir = tempfile::tempdir().unwrap();
    let wrong = tempfile::tempdir().unwrap();
    let f = Fixture::with_store(
        0,
        Store::open(dir.path().join("runtime.sqlite"))
            .await
            .unwrap(),
        LocalBackend::new(),
    )
    .await;
    let repository = f.service.repository.clone();
    let store = f.service.store.clone();
    let runner = f.service.runner.clone();
    let mut config = f.service.config.clone();
    config.herdr_activity.enabled = false;
    drop(f);
    let ownership = crate::RuntimeOwnership::acquire(wrong.path()).unwrap();
    let handle =
        RuntimeService::start_owned(repository, vec![], store.clone(), runner, config, ownership);
    let mut snapshots = handle.subscribe();
    tokio::time::timeout(
        Duration::from_secs(5),
        snapshots.wait_for(|snapshot| snapshot.initialized),
    )
    .await
    .unwrap()
    .unwrap();
    let snapshot = handle.snapshot();
    let result = handle
        .send(RuntimeCommand::StartAway {
            max_agents: 1,
            profile: None,
            ranking: AwayRanking::SourcePriority,
        })
        .await;
    handle.shutdown().await.unwrap();
    assert_eq!(snapshot.away.phase, AwayPhase::Attention);
    assert!(
        snapshot
            .away
            .error
            .as_deref()
            .unwrap()
            .contains("does not match")
    );
    assert!(result.unwrap_err().to_string().contains("does not match"));
    assert!(store.load_away().await.unwrap().is_none());
    assert!(crate::RuntimeOwnership::acquire(wrong.path()).is_ok());
}

#[tokio::test]
async fn away_start_owned_transfers_guard_until_runtime_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::with_store(
        0,
        Store::open(dir.path().join("runtime.sqlite"))
            .await
            .unwrap(),
        LocalBackend::new(),
    )
    .await;
    let ownership = f.service.away.lock.take().unwrap();
    let repository = f.service.repository.clone();
    let store = f.service.store.clone();
    let runner = f.service.runner.clone();
    let mut config = f.service.config.clone();
    config.herdr_activity.enabled = false;
    drop(f);
    assert!(crate::RuntimeOwnership::acquire(dir.path()).is_err());
    let handle =
        RuntimeService::start_owned(repository, vec![], store.clone(), runner, config, ownership);
    let mut snapshots = handle.subscribe();
    tokio::time::timeout(
        Duration::from_secs(5),
        snapshots.wait_for(|snapshot| snapshot.initialized),
    )
    .await
    .unwrap()
    .unwrap();
    let result = handle
        .send(RuntimeCommand::StartAway {
            max_agents: 1,
            profile: None,
            ranking: AwayRanking::SourcePriority,
        })
        .await;
    let denied_while_running = crate::RuntimeOwnership::acquire(dir.path()).is_err();
    handle.shutdown().await.unwrap();
    result.unwrap();
    assert!(
        denied_while_running,
        "transferred guard must remain held by the runtime"
    );
    assert_eq!(
        store.load_away().await.unwrap().unwrap().phase,
        AwayPhase::Paused
    );
    assert!(
        crate::RuntimeOwnership::acquire(dir.path()).is_ok(),
        "shutdown must release ownership even while handles still exist"
    );
}
