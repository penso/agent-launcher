//! Read-only, count-only Herdr inventory. No dispatch/backend APIs are used here.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    process::Stdio,
    time::Duration,
};

use agent_launcher_core::{
    ActivityCompleteness, ActivityCounts, ActivityEndpointHealth, ActivityFreshness,
    ActivitySample, ActivityTransportState, HerdrActivityConfig, HerdrActivityEndpointConfig,
    HerdrActivitySnapshot, activity_ssh_target, valid_activity_session,
};
use agent_launcher_store::Store;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, MissedTickBehavior},
};

const POLL: Duration = Duration::from_secs(2);
const DISCOVERY: Duration = Duration::from_secs(60);
const FRESH: Duration = Duration::from_secs(10);
const DEADLINE: Duration = Duration::from_secs(10);
const MAX_REQUESTS: usize = 4;
const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_ENDPOINTS: usize = 1024;
const HISTORY_LIMIT: usize = 451;

type ReadResult<T> = Result<T, &'static str>;
type Route = HerdrActivityEndpointConfig;

#[derive(Clone, Debug)]
enum Read {
    Sessions(Route),
    Machines,
    Agents(Route),
}

fn same_route(a: &Route, b: &Route) -> bool {
    a.target == b.target
        && a.session == b.session
        && a.enabled == b.enabled
        && a.xdg_config_home == b.xdg_config_home
        && a.xdg_state_home == b.xdg_state_home
}

impl Read {
    fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Machines, Self::Machines) => true,
            (Self::Sessions(a), Self::Sessions(b)) | (Self::Agents(a), Self::Agents(b)) => {
                same_route(a, b)
            },
            _ => false,
        }
    }
}

enum Output {
    Inventory(Vec<Route>),
    Counts(ActivityCounts),
}

type RequestResult = (String, u64, ReadResult<Output>, Instant, DateTime<Utc>);

#[derive(Clone)]
struct Job {
    read: Read,
    due: Instant,
    running: bool,
    failures: u32,
    generation: u64,
    active: bool,
}

impl Job {
    fn reconcile(&mut self, desired: Option<&Read>, now: Instant) {
        if self.active == desired.is_some()
            && desired.is_none_or(|read| self.read.same_request(read))
        {
            return;
        }
        // Keep the running slot until its child exits, but never accept its old result.
        self.generation = self.generation.wrapping_add(1);
        self.active = desired.is_some();
        if let Some(read) = desired {
            self.read = read.clone();
        }
        self.failures = 0;
        self.due = now;
    }

    fn finish(&mut self, generation: u64) -> bool {
        self.running = false;
        self.active && self.generation == generation
    }
}

fn sample_due(next: &mut Instant, now: Instant) -> bool {
    if now < *next {
        return false;
    }
    // Preserve the original phase, emitting only one observation after a stall.
    let late = now.duration_since(*next).as_nanos() % POLL.as_nanos();
    *next = now + (POLL - Duration::from_nanos(late as u64));
    true
}

struct Endpoint {
    route: Route,
    health: ActivityEndpointHealth,
    observed: Option<Instant>,
    counts: ActivityCounts,
}

fn local(config: &HerdrActivityConfig) -> Route {
    Route {
        xdg_config_home: config.xdg_config_home.clone(),
        xdg_state_home: config.xdg_state_home.clone(),
        ..Route::default()
    }
}

fn explicit(config: &HerdrActivityConfig, endpoint: &Route) -> Route {
    let mut endpoint = endpoint.clone();
    if endpoint.target.is_none() {
        endpoint.xdg_config_home = endpoint
            .xdg_config_home
            .or_else(|| config.xdg_config_home.clone());
        endpoint.xdg_state_home = endpoint
            .xdg_state_home
            .or_else(|| config.xdg_state_home.clone());
    }
    endpoint
}

fn host_key(config: &HerdrActivityConfig, route: &Route) -> String {
    let target = route
        .target
        .as_deref()
        .map(activity_ssh_target)
        .transpose()
        .expect("validated route");
    let group = config.alias_groups.iter().find(|group| {
        group.targets.iter().any(|member| {
            if member == "local" {
                target.is_none()
            } else {
                activity_ssh_target(member).ok() == target
            }
        })
    });
    let host = match group {
        Some(group) => format!("alias:{}", group.canonical),
        None => serde_json::to_string(&target).expect("string tuple"),
    };
    serde_json::to_string(&(host, &route.xdg_config_home)).expect("string tuple")
}

fn endpoint_key(config: &HerdrActivityConfig, route: &Route) -> String {
    serde_json::to_string(&(host_key(config, route), &route.session)).expect("string tuple")
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command(config: &HerdrActivityConfig, read: &Read) -> ReadResult<Command> {
    let local_route = local(config);
    let session_option = match read {
        Read::Agents(route) => format!("--session={}", route.session),
        _ => String::new(),
    };
    let (route, args) = match read {
        Read::Machines => (&local_route, vec!["machine", "list", "--json"]),
        Read::Sessions(route) => (route, vec!["session", "list", "--json"]),
        Read::Agents(route) => {
            if !valid_activity_session(&route.session) {
                return Err("invalid-session");
            }
            (
                route,
                if route.session.starts_with('-') {
                    vec![session_option.as_str(), "agent", "list"]
                } else {
                    vec!["--session", route.session.as_str(), "agent", "list"]
                },
            )
        },
    };
    let mut command = if let Some(target) = &route.target {
        let (destination, port) = activity_ssh_target(target).map_err(|_| "invalid-target")?;
        let mut command = Command::new(&config.ssh_executable);
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]);
        if let Some(port) = port {
            command.args(["-p", &port.to_string()]);
        }
        command.arg("--").arg(destination);
        let mut remote = vec!["env".to_owned()];
        for name in [
            "HERDR_SOCKET_PATH",
            "HERDR_SESSION",
            "HERDR_CONFIG_PATH",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
        ] {
            remote.extend(["-u".into(), name.into()]);
        }
        for (name, value) in [
            ("XDG_CONFIG_HOME", &route.xdg_config_home),
            ("XDG_STATE_HOME", &route.xdg_state_home),
        ] {
            if let Some(value) = value {
                remote.push(format!("{name}={value}"));
            }
        }
        remote.push("herdr".into());
        remote.extend(args.into_iter().map(str::to_owned));
        command.arg(
            remote
                .iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
        command
    } else {
        let mut command = Command::new(&config.executable);
        command.args(args);
        for (name, value) in [
            ("XDG_CONFIG_HOME", &route.xdg_config_home),
            ("XDG_STATE_HOME", &route.xdg_state_home),
        ] {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        command
    };
    for name in ["HERDR_SOCKET_PATH", "HERDR_SESSION", "HERDR_CONFIG_PATH"] {
        command.env_remove(name);
    }
    // Clear pane namespace overrides without discarding SSH_AUTH_SOCK or HOME.
    for name in ["XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
        if route.target.is_some()
            || match name {
                "XDG_CONFIG_HOME" => route.xdg_config_home.is_none(),
                _ => route.xdg_state_home.is_none(),
            }
        {
            command.env_remove(name);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

fn failure(stderr: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if text.contains("host key verification failed")
        || text.contains("remote host identification has changed")
    {
        "host-key"
    } else if text.contains("permission denied (publickey")
        || text.contains("authentication failed")
    {
        "authentication"
    } else if text.contains("permission denied") {
        "permission"
    } else if text.contains("command not found")
        || text.contains("herdr: not found")
        || text.contains("env: herdr:")
    {
        "missing-executable"
    } else if text.contains("method not found") || text.contains("unsupported method") {
        "unsupported-method"
    } else if text.contains("connection refused")
        || text.contains("no such file")
        || text.contains("not running")
    {
        "unavailable"
    } else {
        "process-failed"
    }
}

async fn execute(mut command: Command) -> ReadResult<Vec<u8>> {
    tokio::time::timeout(DEADLINE, async move {
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "missing-executable"
            } else {
                "spawn-failed"
            }
        })?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or("io-error")?
            .take((MAX_RESPONSE + 1) as u64);
        let mut stderr = child
            .stderr
            .take()
            .ok_or("io-error")?
            .take((MAX_RESPONSE + 1) as u64);
        let read = |bytes: Vec<u8>| {
            if bytes.len() > MAX_RESPONSE {
                Err("response-too-large")
            } else {
                Ok(bytes)
            }
        };
        let out = async {
            let mut bytes = Vec::new();
            stdout
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "io-error")?;
            read(bytes)
        };
        let err = async {
            let mut bytes = Vec::new();
            stderr
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "io-error")?;
            read(bytes)
        };
        // Read both pipes concurrently; either limit immediately cancels and kills the child.
        let (stdout, stderr) = tokio::try_join!(out, err)?;
        let status = child.wait().await.map_err(|_| "io-error")?;
        if !status.success() {
            return Err(failure(&stderr));
        }
        Ok(stdout)
    })
    .await
    .map_err(|_| "timeout")?
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<Session>,
}
#[derive(Deserialize)]
struct Session {
    name: String,
    running: bool,
}
#[derive(Deserialize)]
struct Machine {
    target: String,
    session: String,
    enabled: bool,
}
#[derive(Deserialize)]
struct AgentList {
    result: AgentResult,
}
#[derive(Deserialize)]
struct AgentResult {
    r#type: String,
    agents: Vec<Agent>,
}
#[derive(Deserialize)]
struct Agent {
    terminal_id: String,
    agent_status: String,
}

fn parse(read: &Read, bytes: &[u8]) -> ReadResult<Output> {
    if bytes.len() > MAX_RESPONSE {
        return Err("response-too-large");
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "malformed-json")?;
    if let Some(error) = value.get("error") {
        if error.get("code").and_then(|code| code.as_i64()) == Some(-32601) {
            return Err("unsupported-method");
        }
        return Err(match error.get("code").and_then(|code| code.as_str()) {
            Some("method_not_found" | "unsupported_method") => "unsupported-method",
            Some("permission_denied") => "permission",
            _ => "api-error",
        });
    }
    match read {
        Read::Sessions(route) => {
            let list: SessionList =
                serde_json::from_value(value).map_err(|_| "invalid-session-list")?;
            if list.sessions.len() > MAX_ENDPOINTS {
                return Err("inventory-too-large");
            }
            let mut routes = Vec::new();
            let mut seen = BTreeSet::new();
            for session in list.sessions {
                if !valid_activity_session(&session.name) || !seen.insert(session.name.clone()) {
                    return Err("invalid-session-list");
                }
                if session.running {
                    routes.push(Route {
                        session: session.name,
                        ..route.clone()
                    });
                }
            }
            Ok(Output::Inventory(routes))
        },
        Read::Machines => {
            let list: Vec<Machine> =
                serde_json::from_value(value).map_err(|_| "invalid-machine-list")?;
            if list.len() > 64 {
                return Err("inventory-too-large");
            }
            let mut routes = Vec::new();
            for machine in list {
                activity_ssh_target(&machine.target).map_err(|_| "invalid-target")?;
                if !valid_activity_session(&machine.session) {
                    return Err("invalid-session");
                }
                routes.push(Route {
                    target: Some(machine.target),
                    session: machine.session,
                    enabled: machine.enabled,
                    ..Route::default()
                });
            }
            Ok(Output::Inventory(routes))
        },
        Read::Agents(_) => {
            let list: AgentList =
                serde_json::from_value(value).map_err(|_| "invalid-agent-list")?;
            if list.result.r#type != "agent_list" {
                return Err("unsupported-response");
            }
            let mut seen = BTreeSet::new();
            let mut counts = ActivityCounts::default();
            for agent in list.result.agents {
                if agent.terminal_id.is_empty() || !seen.insert(agent.terminal_id) {
                    return Err("invalid-agent-list");
                }
                match agent.agent_status.as_str() {
                    "working" => counts.working += 1,
                    "blocked" => counts.blocked += 1,
                    "idle" => counts.idle += 1,
                    "done" => counts.unseen_done += 1,
                    _ => counts.unknown += 1,
                }
            }
            Ok(Output::Counts(counts))
        },
    }
}

fn backoff(failures: u32, jitter: u64) -> Duration {
    let seconds = (2u64 << failures.saturating_sub(1).min(5)).min(60);
    // Positive jitter prevents synchronized retries; the total delay stays <=60s.
    Duration::from_millis((seconds * 1000 + jitter % 501).min(60_000))
}

fn aggregate(
    endpoints: &mut BTreeMap<String, Endpoint>,
    complete: bool,
    now: Instant,
    utc: DateTime<Utc>,
) -> ActivitySample {
    let mut sample = ActivitySample {
        sampled_at: utc_bucket(utc),
        counts: None,
        expected_endpoints: 0,
        fresh_endpoints: 0,
        stale_endpoints: 0,
        never_observed_endpoints: 0,
        failed_endpoints: 0,
        excluded_endpoints: 0,
        inventory_complete: complete,
        completeness: ActivityCompleteness::Missing,
    };
    let mut counts = ActivityCounts::default();
    for endpoint in endpoints.values_mut() {
        endpoint.health.freshness = match endpoint.observed {
            None => ActivityFreshness::NeverObserved,
            Some(observed) if now.saturating_duration_since(observed) <= FRESH => {
                ActivityFreshness::Fresh
            },
            Some(_) => ActivityFreshness::Stale,
        };
        if !endpoint.route.enabled {
            sample.excluded_endpoints += 1;
            continue;
        }
        sample.expected_endpoints += 1;
        if endpoint.health.transport == ActivityTransportState::Failed {
            sample.failed_endpoints += 1;
        }
        match endpoint.health.freshness {
            ActivityFreshness::Fresh => {
                sample.fresh_endpoints += 1;
                counts.working += endpoint.counts.working;
                counts.blocked += endpoint.counts.blocked;
                counts.idle += endpoint.counts.idle;
                counts.unseen_done += endpoint.counts.unseen_done;
                counts.unknown += endpoint.counts.unknown;
            },
            ActivityFreshness::Stale => sample.stale_endpoints += 1,
            ActivityFreshness::NeverObserved => sample.never_observed_endpoints += 1,
        }
    }
    if sample.fresh_endpoints > 0 || (complete && sample.expected_endpoints == 0) {
        sample.counts = Some(counts);
        sample.completeness = if complete && sample.fresh_endpoints == sample.expected_endpoints {
            ActivityCompleteness::Complete
        } else {
            ActivityCompleteness::Partial
        };
    }
    sample
}

fn inventory(
    config: &HerdrActivityConfig,
    sources: &BTreeMap<String, Vec<Route>>,
) -> ReadResult<BTreeMap<String, Route>> {
    let mut routes: BTreeMap<String, Route> = BTreeMap::new();
    for route in config
        .endpoints
        .iter()
        .map(|route| explicit(config, route))
        .chain(sources.values().flatten().cloned())
    {
        let key = endpoint_key(config, &route);
        if let Some(existing) = routes.get_mut(&key) {
            existing.enabled &= route.enabled;
        } else {
            routes.insert(key, route);
        }
        if routes.len() > MAX_ENDPOINTS {
            return Err("inventory-too-large");
        }
    }
    for excluded in &config.exclusions {
        let mut route = explicit(config, excluded);
        route.enabled = false;
        let key = endpoint_key(config, &route);
        routes
            .entry(key)
            .and_modify(|r| r.enabled = false)
            .or_insert(route);
        if routes.len() > MAX_ENDPOINTS {
            return Err("inventory-too-large");
        }
    }
    Ok(routes)
}

fn utc_bucket(utc: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(utc.timestamp().div_euclid(2) * 2, 0).expect("UTC bucket")
}

#[derive(Clone, Copy, Debug)]
struct HistoryReset {
    generation: u64,
    cutoff: DateTime<Utc>,
}

struct HistoryClock {
    last_utc: DateTime<Utc>,
    reset: HistoryReset,
    acknowledged: Option<u64>,
}

impl HistoryClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            last_utc: now,
            reset: HistoryReset {
                generation: 0,
                cutoff: utc_bucket(now),
            },
            acknowledged: None,
        }
    }

    fn observe(&mut self, now: DateTime<Utc>) -> bool {
        let rolled_back = now < self.last_utc;
        self.last_utc = now;
        if rolled_back {
            let cutoff = utc_bucket(now);
            // A watch channel can coalesce resets. Keep the earliest unacknowledged
            // cutoff so a later rollback never replaces an outstanding deletion.
            self.reset.cutoff = if self.acknowledged == Some(self.reset.generation) {
                cutoff
            } else {
                cutoff.min(self.reset.cutoff)
            };
            self.reset.generation = self.reset.generation.wrapping_add(1);
        }
        rolled_back
    }
}

#[derive(Debug)]
struct HistoryWrite {
    generation: u64,
    sample: ActivitySample,
}

enum Persistence {
    Reset(u64),
    Loaded(u64, ReadResult<Vec<ActivitySample>>),
    Health(u64, Option<&'static str>),
}

async fn persist(
    store: Store,
    mut samples: mpsc::Receiver<HistoryWrite>,
    events: mpsc::Sender<Persistence>,
    mut resets: watch::Receiver<HistoryReset>,
) {
    let mut applied = None;
    loop {
        let reset = *resets.borrow_and_update();
        if applied != Some(reset.generation) {
            // This single writer finishes any in-flight write before clearing. It
            // admits no new writes or history loads until the deletion succeeds.
            if store
                .clear_activity_samples_from(reset.cutoff)
                .await
                .is_err()
            {
                if events
                    .send(Persistence::Health(
                        reset.generation,
                        Some("history-reset-failed"),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    changed = resets.changed() => if changed.is_err() { return; },
                    _ = tokio::time::sleep(POLL) => {},
                }
                continue;
            }
            applied = Some(reset.generation);
            if events
                .send(Persistence::Reset(reset.generation))
                .await
                .is_err()
            {
                return;
            }
            let loaded = store
                .load_activity_samples(
                    reset.cutoff - chrono::Duration::minutes(15),
                    reset.cutoff,
                    HISTORY_LIMIT,
                )
                .await
                .map_err(|_| "history-load-failed");
            if events
                .send(Persistence::Loaded(reset.generation, loaded))
                .await
                .is_err()
            {
                return;
            }
            continue;
        }
        let write = tokio::select! {
            biased;
            changed = resets.changed() => {
                if changed.is_err() { return; }
                continue;
            },
            write = samples.recv() => match write { Some(write) => write, None => return },
        };
        // Includes old writes queued before startup cleanup or while a reset was
        // blocked. A reset arriving during the next await is handled before any
        // subsequent write, so the just-finished obsolete write is deleted too.
        if Some(write.generation) != applied || resets.borrow().generation != write.generation {
            continue;
        }
        let result = async {
            store
                .upsert_activity_sample(&write.sample)
                .await
                .map_err(|_| "sample-write-failed")?;
            store
                .prune_activity_samples(Utc::now() - chrono::Duration::minutes(30), 256)
                .await
                .map_err(|_| "history-prune-failed")
        }
        .await;
        if events
            .send(Persistence::Health(write.generation, result.err()))
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Closing the update receiver cancels and awaits requests, then flushes queued
/// persistence with a bounded deadline. Aborting remains a last-resort fallback;
/// child kill_on_drop and Tokio's process reaping apply to cancelled requests.
pub(crate) async fn run(
    config: HerdrActivityConfig,
    store: Store,
    updates: watch::Sender<HerdrActivitySnapshot>,
    mut refresh: watch::Receiver<u64>,
) {
    let mut snapshot = HerdrActivitySnapshot {
        enabled: config.enabled,
        discover_remote_sessions: config.discover_remote_sessions,
        ..Default::default()
    };
    if !config.enabled {
        updates.send_replace(snapshot);
        return;
    }
    if config.validate().is_err() {
        snapshot.discovery_error = Some("invalid-configuration".into());
        updates.send_replace(snapshot);
        return;
    }
    let (writes, write_rx) = mpsc::channel(16);
    let (events_tx, mut events) = mpsc::channel(16);
    let mut history_clock = HistoryClock::new(Utc::now());
    let (resets, reset_rx) = watch::channel(history_clock.reset);
    snapshot.persistence_error = Some("history-reset-pending".into());
    let mut persistence = JoinSet::new();
    persistence.spawn(persist(store, write_rx, events_tx, reset_rx));
    let mut requests: JoinSet<RequestResult> = JoinSet::new();
    let mut sources: BTreeMap<String, Vec<Route>> = BTreeMap::new();
    let mut discovery_errors: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut endpoints: BTreeMap<String, Endpoint> = BTreeMap::new();
    let mut jobs: BTreeMap<String, Job> = BTreeMap::new();
    let mut queue = VecDeque::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next_sample = Instant::now();
    let mut refresh_open = true;
    let mut jitter = Utc::now().timestamp_subsec_nanos() as u64 | 1;
    loop {
        tokio::select! {
            _ = updates.closed() => break,
            changed = refresh.changed(), if refresh_open => {
                if changed.is_err() { refresh_open = false; }
                else { for job in jobs.values_mut() { job.due = Instant::now(); } }
            }
            Some(event) = events.recv() => match event {
                Persistence::Reset(generation) if generation == history_clock.reset.generation => {
                    history_clock.acknowledged = Some(generation);
                    snapshot.persistence_error = None;
                },
                Persistence::Loaded(generation, result) if generation == history_clock.reset.generation => match result {
                    Ok(samples) => {
                        snapshot.samples.extend(samples);
                        let now = Utc::now();
                        snapshot.samples.retain(|sample| sample.sampled_at >= now - chrono::Duration::minutes(15) && sample.sampled_at <= now);
                        snapshot.samples.sort_by_key(|sample| sample.sampled_at);
                        snapshot.samples.dedup_by_key(|sample| sample.sampled_at);
                        if snapshot.samples.len() > HISTORY_LIMIT { snapshot.samples.drain(..snapshot.samples.len() - HISTORY_LIMIT); }
                    }
                    Err(error) => snapshot.persistence_error = Some(error.into()),
                },
                Persistence::Health(generation, error) if generation == history_clock.reset.generation => snapshot.persistence_error = error.map(str::to_owned),
                _ => {},
            },
            Some(result) = requests.join_next(), if !requests.is_empty() => {
                if let Ok((key, generation, result, observed, utc)) = result
                    && let Some(job) = jobs.get_mut(&key)
                    && job.finish(generation) {
                        let result = match result {
                            Ok(Output::Inventory(routes)) => {
                                let previous = sources.insert(key.clone(), routes);
                                if let Err(error) = inventory(&config, &sources) {
                                    sources.remove(&key);
                                    if let Some(previous) = previous { sources.insert(key.clone(), previous); }
                                    Err(error)
                                } else { Ok(None) }
                            }
                            Ok(Output::Counts(counts)) => Ok(Some(counts)),
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(counts) => {
                                job.failures = 0;
                                job.due = observed + if matches!(job.read, Read::Agents(_)) { POLL } else { DISCOVERY };
                                if let Some(counts) = counts {
                                    if let Some(endpoint) = endpoints.get_mut(key.strip_prefix("poll:").unwrap_or("")) {
                                        endpoint.counts = counts;
                                        endpoint.observed = Some(observed);
                                        endpoint.health.last_success_at = Some(utc);
                                        endpoint.health.transport = ActivityTransportState::Reachable;
                                        endpoint.health.error_kind = None;
                                    }
                                } else { discovery_errors.remove(&key); }
                            }
                            Err(error) => {
                                job.failures = job.failures.saturating_add(1);
                                jitter ^= jitter << 13; jitter ^= jitter >> 7; jitter ^= jitter << 17;
                                job.due = observed + backoff(job.failures, jitter);
                                if matches!(job.read, Read::Agents(_)) {
                                    if let Some(endpoint) = endpoints.get_mut(key.strip_prefix("poll:").unwrap_or("")) {
                                        endpoint.health.transport = ActivityTransportState::Failed;
                                        endpoint.health.error_kind = Some(error.into());
                                    }
                                } else { discovery_errors.insert(key, error); }
                            }
                        }
                }
            }
            _ = tick.tick() => {}
        }

        let now = Instant::now();
        let utc = Utc::now();
        if history_clock.observe(utc) {
            snapshot
                .samples
                .retain(|sample| sample.sampled_at < history_clock.reset.cutoff);
            // Separate from the bounded sample queue: reset intent cannot be dropped
            // under SQLite backpressure. Old event generations cannot restore history.
            resets.send_replace(history_clock.reset);
            snapshot.persistence_error = Some("history-reset-pending".into());
        }
        let mut desired = BTreeMap::new();
        if config.discover_local_sessions {
            desired.insert("local".into(), Read::Sessions(local(&config)));
        }
        if config.discover_saved_profiles {
            desired.insert("machines".into(), Read::Machines);
        }
        // Remote discovery authorization comes only from explicit/saved profiles, not
        // from its own previous output. Disabled or excluded sessions cannot authorize it.
        let base_sources = sources
            .iter()
            .filter(|(key, _)| !key.starts_with("remote:"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let base = inventory(&config, &base_sources).unwrap_or_default();
        if config.discover_remote_sessions {
            for route in base
                .values()
                .filter(|route| route.enabled && route.target.is_some())
            {
                desired
                    .entry(format!("remote:{}", host_key(&config, route)))
                    .or_insert_with(|| Read::Sessions(route.clone()));
            }
        }
        sources.retain(|key, _| desired.contains_key(key));
        discovery_errors.retain(|key, _| desired.contains_key(key));
        for (key, read) in &desired {
            if jobs
                .get(key)
                .is_some_and(|job| !job.active || !job.read.same_request(read))
            {
                // Discovery rows carry transport routes too. Do not keep polling A's
                // discovered sessions after its canonical host switches to alias B.
                sources.remove(key);
                discovery_errors.remove(key);
            }
        }
        let routes = inventory(&config, &sources).expect("bounded validated inventory");
        endpoints.retain(|key, _| routes.contains_key(key));
        for (key, route) in routes {
            let endpoint = endpoints.entry(key.clone()).or_insert_with(|| Endpoint {
                route: route.clone(),
                health: ActivityEndpointHealth {
                    endpoint_id: key.clone(),
                    transport: ActivityTransportState::Connecting,
                    freshness: ActivityFreshness::NeverObserved,
                    last_success_at: None,
                    error_kind: None,
                },
                observed: None,
                counts: ActivityCounts::default(),
            });
            if !same_route(&endpoint.route, &route) {
                endpoint.observed = None;
                endpoint.health.last_success_at = None;
                endpoint.health.error_kind = None;
                endpoint.health.transport = ActivityTransportState::Connecting;
            }
            endpoint.route = route.clone();
            if route.enabled {
                desired.insert(format!("poll:{key}"), Read::Agents(route));
            } else {
                endpoint.health.transport = ActivityTransportState::Disabled;
            }
        }
        // Invalidate removed/replaced requests before admitting new work, including an
        // A -> B -> A replacement while A's original request is still running.
        for (key, job) in &mut jobs {
            job.reconcile(desired.get(key), now);
        }
        jobs.retain(|key, job| job.running || desired.contains_key(key));
        queue.retain(|key| desired.contains_key(key));
        let queued: BTreeSet<_> = queue.iter().cloned().collect();
        for (key, read) in &desired {
            jobs.entry(key.clone()).or_insert_with(|| Job {
                read: read.clone(),
                due: now,
                running: false,
                failures: 0,
                generation: 0,
                active: true,
            });
            if !queued.contains(key) {
                queue.push_back(key.clone());
            }
        }
        // A shared rotating queue gives discoveries and healthy endpoints equal access.
        for _ in 0..queue.len() {
            if requests.len() >= MAX_REQUESTS {
                break;
            }
            let Some(key) = queue.pop_front() else {
                break;
            };
            queue.push_back(key.clone());
            let job = jobs.get_mut(&key).expect("queued job");
            if job.running || job.due > now {
                continue;
            }
            job.running = true;
            let read = job.read.clone();
            let generation = job.generation;
            let command = command(&config, &read);
            requests.spawn(async move {
                let result = match command {
                    Ok(command) => execute(command)
                        .await
                        .and_then(|bytes| parse(&read, &bytes)),
                    Err(error) => Err(error),
                };
                (key, generation, result, Instant::now(), Utc::now())
            });
        }
        snapshot.discovering = desired.iter().any(|(key, read)| {
            !matches!(read, Read::Agents(_)) && jobs.get(key).is_some_and(|job| job.running)
        });
        let complete = !desired.is_empty()
            && desired
                .iter()
                .filter(|(_, read)| !matches!(read, Read::Agents(_)))
                .all(|(key, _)| sources.contains_key(key) && !discovery_errors.contains_key(key));
        snapshot.discovery_error = discovery_errors
            .values()
            .next()
            .map(|error| (*error).into());
        if sample_due(&mut next_sample, now) {
            let sample = aggregate(&mut endpoints, complete, now, utc);
            snapshot.samples.retain(|old| {
                old.sampled_at >= utc - chrono::Duration::minutes(15)
                    && old.sampled_at <= utc
                    && old.sampled_at != sample.sampled_at
            });
            snapshot.samples.push(sample.clone());
            snapshot.samples.sort_by_key(|sample| sample.sampled_at);
            if snapshot.samples.len() > HISTORY_LIMIT {
                snapshot
                    .samples
                    .drain(..snapshot.samples.len() - HISTORY_LIMIT);
            }
            if writes
                .try_send(HistoryWrite {
                    generation: history_clock.reset.generation,
                    sample,
                })
                .is_err()
            {
                snapshot.persistence_error = Some("persistence-queue-full".into());
            }
        } else {
            // Health stays current independently of graph buckets.
            aggregate(&mut endpoints, complete, now, Utc::now());
        }
        snapshot.endpoints = endpoints
            .values()
            .map(|endpoint| endpoint.health.clone())
            .collect();
        updates.send_replace(snapshot.clone());
    }
    requests.shutdown().await;
    drop(writes);
    // Keep resets alive until the writer finishes: its closure takes priority over
    // queued writes. Consume acknowledgments so the bounded event queue cannot stall it.
    let drained = tokio::time::timeout(Duration::from_secs(1), async {
        while !persistence.is_empty() {
            tokio::select! {
                _ = persistence.join_next() => {},
                Some(_) = events.recv() => {},
            }
        }
    })
    .await;
    if drained.is_err() {
        persistence.shutdown().await;
    }
    drop(resets);
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::HerdrActivityAliasGroup;

    use super::*;

    fn counts(bytes: &[u8]) -> ReadResult<ActivityCounts> {
        match parse(&Read::Agents(Route::default()), bytes)? {
            Output::Counts(counts) => Ok(counts),
            _ => panic!("expected counts"),
        }
    }

    fn routes(read: Read, bytes: &[u8]) -> ReadResult<Vec<Route>> {
        match parse(&read, bytes)? {
            Output::Inventory(routes) => Ok(routes),
            _ => panic!("expected inventory"),
        }
    }

    #[test]
    fn minimal_agent_fields_future_states_and_private_metadata() {
        let result = counts(
            br#"{"id":"fixture","result":{"type":"agent_list","agents":[
            {"terminal_id":"1","agent_status":"working","title":"DO NOT KEEP","cwd":"PRIVATE"},
            {"terminal_id":"2","agent_status":"blocked"},
            {"terminal_id":"3","agent_status":"idle"},
            {"terminal_id":"4","agent_status":"done"},
            {"terminal_id":"5","agent_status":"unknown"},
            {"terminal_id":"6","agent_status":"future-state","revision":1000}
        ]}}"#,
        )
        .unwrap();
        assert_eq!(result, ActivityCounts {
            working: 1,
            blocked: 1,
            idle: 1,
            unseen_done: 1,
            unknown: 2
        });
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains("PRIVATE"));
        assert!(!serialized.contains("terminal_id"));
    }

    #[test]
    fn inventories_are_atomic_not_partial_successes() {
        for bytes in [
            br#"{"result":{"type":"agent_list","agents":[{"terminal_id":"1","agent_status":"working"},{"terminal_id":"2"}]}}"#.as_slice(),
            br#"{"result":{"type":"agent_list","agents":[{"terminal_id":"1","agent_status":"working"},{"terminal_id":"1","agent_status":"idle"}]}}"#,
            br#"{"result":{"type":"snapshot","agents":[]}}"#,
            br#"{"result":{"type":"agent_list","agents":[]},"error":{"code":"bad","message":"SECRET"}}"#,
            br#"{"result":{"type":"agent_list","agents":[]}"#,
        ] { assert!(counts(bytes).is_err()); }
        assert_eq!(
            counts(br#"{"error":{"code":-32601,"message":"SECRET"}}"#),
            Err("unsupported-method")
        );
        assert_eq!(
            counts(&vec![b' '; MAX_RESPONSE + 1]),
            Err("response-too-large")
        );
        assert_eq!(
            counts(br#"{"result":{"type":"agent_list","agents":[]}}"#).unwrap(),
            ActivityCounts::default()
        );
    }

    #[test]
    fn session_shape_includes_running_default_and_named_only() {
        let fixture = br#"{"sessions":[{"name":"default","default":true,"running":true,"socket_path":"private","session_dir":"private"},{"name":"named","running":true},{"name":"stopped","running":false}]}"#;
        let result = routes(Read::Sessions(Route::default()), fixture).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|r| r.session.as_str())
                .collect::<Vec<_>>(),
            ["default", "named"]
        );
        assert!(
            routes(
                Read::Sessions(Route::default()),
                br#"{"sessions":[{"name":"default","running":false}]}"#
            )
            .unwrap()
            .is_empty()
        );
        for fixture in [
            br#"{"sessions":[{"name":"../escape","running":true}]}"#.as_slice(),
            br#"{"sessions":[{"name":"default"}]}"#,
            br#"{"sessions":[{"name":"a","running":true},{"name":"a","running":false}]}"#,
        ] {
            assert!(routes(Read::Sessions(Route::default()), fixture).is_err());
        }
    }

    #[test]
    fn machine_bare_array_duplicates_disabled_and_selected_independent() {
        let fixture = br#"[{"id":"a","label":"private","target":"host","session":"default","enabled":true,"selected":false},{"id":"b","target":"ssh://host","session":"default","enabled":false,"selected":true}]"#;
        let routes = routes(Read::Machines, fixture).unwrap();
        let inventory = inventory(
            &HerdrActivityConfig::default(),
            &BTreeMap::from([("machines".into(), routes)]),
        )
        .unwrap();
        assert_eq!(inventory.len(), 1);
        assert!(!inventory.values().next().unwrap().enabled);
        assert!(parse(&Read::Machines, br#"{"machines":[]}"#).is_err());
        assert!(
            parse(
                &Read::Machines,
                br#"[{"target":"-oProxyCommand=evil","session":"default","enabled":true}]"#
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_aliases_exclusions_accounts_and_namespaces() {
        let mut config = HerdrActivityConfig {
            alias_groups: vec![HerdrActivityAliasGroup {
                canonical: "home".into(),
                targets: vec!["local".into(), "me@loopback".into()],
            }],
            ..Default::default()
        };
        let remote = Route {
            target: Some("me@loopback".into()),
            ..Default::default()
        };
        assert_eq!(
            endpoint_key(&config, &Route::default()),
            endpoint_key(&config, &remote)
        );
        for distinct in [
            Route {
                target: Some("other@loopback".into()),
                ..remote.clone()
            },
            Route {
                session: "named".into(),
                ..remote.clone()
            },
            Route {
                xdg_config_home: Some("/other".into()),
                ..remote.clone()
            },
        ] {
            assert_ne!(
                endpoint_key(&config, &remote),
                endpoint_key(&config, &distinct)
            );
        }
        config.endpoints = vec![Route::default(), remote.clone()];
        config.exclusions = vec![remote];
        let inventory = inventory(&config, &BTreeMap::new()).unwrap();
        assert_eq!(inventory.len(), 1);
        assert!(!inventory.values().next().unwrap().enabled);
    }

    #[test]
    fn local_is_not_an_implicit_ssh_alias_and_hyphen_sessions_are_values() {
        let config = HerdrActivityConfig::default();
        assert_ne!(
            endpoint_key(&config, &Route::default()),
            endpoint_key(&config, &Route {
                target: Some("local".into()),
                ..Default::default()
            })
        );
        let command = command(
            &config,
            &Read::Agents(Route {
                session: "-named".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(
            command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["--session=-named", "agent", "list"]
        );
    }

    #[test]
    fn routing_is_allowlisted_and_environment_is_deliberate() {
        let config = HerdrActivityConfig::default();
        for (read, expected) in [
            (Read::Machines, vec!["machine", "list", "--json"]),
            (Read::Sessions(Route::default()), vec![
                "session", "list", "--json",
            ]),
            (Read::Agents(Route::default()), vec![
                "--session",
                "default",
                "agent",
                "list",
            ]),
        ] {
            let command = command(&config, &read).unwrap();
            let command = command.as_std();
            assert_eq!(
                command
                    .get_args()
                    .map(|s| s.to_str().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            let env: BTreeMap<_, _> = command.get_envs().collect();
            for name in [
                "HERDR_SOCKET_PATH",
                "HERDR_SESSION",
                "HERDR_CONFIG_PATH",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
            ] {
                assert_eq!(env.get(std::ffi::OsStr::new(name)), Some(&None));
            }
            assert!(!env.contains_key(std::ffi::OsStr::new("SSH_AUTH_SOCK")));
        }
        let route = Route {
            xdg_config_home: Some("/explicit".into()),
            ..Default::default()
        };
        let command = command(&config, &Read::Agents(route)).unwrap();
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "XDG_CONFIG_HOME"
                    && value == Some(std::ffi::OsStr::new("/explicit")))
        );
    }

    #[test]
    fn ssh_ports_ipv6_and_remote_shell_quoting() {
        let route = Route {
            target: Some("ssh://me@[::1]:2222".into()),
            xdg_config_home: Some("/a'b/$(not-executed)".into()),
            ..Default::default()
        };
        let command = command(&HerdrActivityConfig::default(), &Read::Agents(route)).unwrap();
        let args = command
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert!(args.windows(2).any(|pair| pair == ["--", "me@[::1]"]));
        assert!(args.contains(&"StrictHostKeyChecking=yes"));
        for option in [
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
        ] {
            assert!(args.windows(2).any(|pair| pair == ["-o", option]));
        }
        assert!(
            !command
                .as_std()
                .get_envs()
                .any(|(key, _)| key == "SSH_AUTH_SOCK")
        );
        let remote = args.last().unwrap();
        assert!(remote.contains("'XDG_CONFIG_HOME=/a'\\''b/$(not-executed)'"));
        assert!(remote.ends_with("'herdr' '--session' 'default' 'agent' 'list'"));
        assert!(!remote.contains("--remote"));
    }

    fn endpoint(
        observed: Option<Instant>,
        transport: ActivityTransportState,
        working: u64,
    ) -> Endpoint {
        Endpoint {
            route: Route::default(),
            health: ActivityEndpointHealth {
                endpoint_id: "fixture".into(),
                transport,
                freshness: ActivityFreshness::NeverObserved,
                last_success_at: None,
                error_kind: None,
            },
            observed,
            counts: ActivityCounts {
                working,
                ..Default::default()
            },
        }
    }

    #[test]
    fn fresh_failed_stale_never_and_excluded_are_separate() {
        let now = Instant::now();
        let mut disabled = endpoint(Some(now), ActivityTransportState::Disabled, 1000);
        disabled.route.enabled = false;
        let mut endpoints = BTreeMap::from([
            (
                "fresh".into(),
                endpoint(Some(now), ActivityTransportState::Reachable, 3),
            ),
            (
                "failed-fresh".into(),
                endpoint(Some(now), ActivityTransportState::Failed, 2),
            ),
            (
                "stale".into(),
                endpoint(
                    Some(now - FRESH - Duration::from_millis(1)),
                    ActivityTransportState::Failed,
                    100,
                ),
            ),
            (
                "never".into(),
                endpoint(None, ActivityTransportState::Connecting, 100),
            ),
            ("disabled".into(), disabled),
        ]);
        let sample = aggregate(&mut endpoints, true, now, Utc::now());
        assert_eq!(sample.counts.unwrap().working, 5);
        assert_eq!(sample.completeness, ActivityCompleteness::Partial);
        assert_eq!(
            (
                sample.expected_endpoints,
                sample.fresh_endpoints,
                sample.stale_endpoints,
                sample.never_observed_endpoints,
                sample.failed_endpoints,
                sample.excluded_endpoints
            ),
            (4, 2, 1, 1, 2, 1)
        );
        let sample = aggregate(&mut endpoints, true, now + FRESH + POLL, Utc::now());
        assert_eq!(sample.counts, None);
        assert_eq!(sample.completeness, ActivityCompleteness::Missing);
    }

    #[test]
    fn verified_empty_is_zero_unknown_inventory_is_missing_and_clock_is_not_freshness() {
        let now = Instant::now();
        let utc = DateTime::from_timestamp(101, 900_000_000).unwrap();
        let mut endpoints = BTreeMap::new();
        assert_eq!(
            aggregate(&mut endpoints, true, now, utc).counts,
            Some(ActivityCounts::default())
        );
        assert_eq!(aggregate(&mut endpoints, false, now, utc).counts, None);
        endpoints.insert(
            "host".into(),
            endpoint(Some(now), ActivityTransportState::Reachable, 3),
        );
        let sample = aggregate(&mut endpoints, true, now, utc);
        assert_eq!(sample.sampled_at.timestamp(), 100);
        assert_eq!(sample.completeness, ActivityCompleteness::Complete);
        assert!(
            aggregate(
                &mut endpoints,
                true,
                now + FRESH + POLL,
                utc - chrono::Duration::hours(1)
            )
            .counts
            .is_none()
        );
        assert_eq!(
            aggregate(&mut endpoints, false, now, utc).completeness,
            ActivityCompleteness::Partial
        );
    }

    #[test]
    fn retry_delays_are_bounded_and_jittered() {
        for (failures, seconds) in [
            (1, 2),
            (2, 4),
            (3, 8),
            (4, 16),
            (5, 32),
            (6, 60),
            (u32::MAX, 60),
        ] {
            assert_eq!(backoff(failures, 0), Duration::from_secs(seconds));
            assert!(backoff(failures, u64::MAX) <= Duration::from_secs(60));
        }
        assert_ne!(backoff(1, 12), backoff(1, 42));
        assert_eq!(MAX_REQUESTS, 4);
    }

    #[test]
    fn replaced_and_reauthorized_jobs_discard_inflight_generations() {
        let now = Instant::now();
        let a = Route {
            target: Some("alias-a".into()),
            ..Default::default()
        };
        let b = Route {
            target: Some("alias-b".into()),
            ..Default::default()
        };
        for (a, b) in [
            (Read::Agents(a.clone()), Read::Agents(b.clone())),
            (Read::Sessions(a), Read::Sessions(b)),
        ] {
            let mut job = Job {
                read: a.clone(),
                due: now + DISCOVERY,
                running: true,
                failures: 5,
                generation: 0,
                active: true,
            };
            job.reconcile(Some(&b), now);
            assert!(job.read.same_request(&b));
            assert!(
                job.running,
                "replacement must not overlap the previous child"
            );
            assert_eq!(job.due, now);
            assert_eq!(job.failures, 0);
            let b_generation = job.generation;
            job.reconcile(Some(&b), now + POLL);
            assert_eq!(
                job.generation, b_generation,
                "stable routes must not invalidate work"
            );
            assert!(
                !job.finish(0),
                "neither old successes nor old failures may be applied"
            );
            assert!(!job.running);
            job.running = true;
            job.reconcile(Some(&a), now);
            assert!(!job.finish(b_generation));
            job.running = true;
            let original_a_generation = job.generation;
            job.reconcile(None, now);
            job.reconcile(Some(&a), now);
            assert!(
                !job.finish(original_a_generation),
                "removal then re-addition also invalidates work"
            );
            job.running = true;
            assert!(job.finish(job.generation));
        }
    }

    #[test]
    fn sampling_preserves_phase_and_skips_missed_ticks_without_backfill() {
        let origin = Instant::now();
        let mut next = origin;
        for n in 0..100 {
            let late = origin + POLL * n + Duration::from_millis(130);
            assert!(sample_due(&mut next, late));
            assert_eq!(next, origin + POLL * (n + 1));
            assert!(!sample_due(&mut next, late));
        }
        let after_stall = origin + Duration::from_millis(237_650);
        assert!(sample_due(&mut next, after_stall));
        assert_eq!(next, origin + Duration::from_secs(238));
        assert!(!sample_due(&mut next, after_stall), "no catch-up samples");
        assert!(sample_due(&mut next, origin + Duration::from_secs(238)));
        assert_eq!(next, origin + Duration::from_secs(240));
    }

    #[test]
    fn discovery_and_combined_inventory_limits_fail_atomically() {
        let sessions: Vec<_> = (0..=MAX_ENDPOINTS)
            .map(|n| serde_json::json!({"name": format!("s{n}"), "running": true}))
            .collect();
        let bytes = serde_json::to_vec(&serde_json::json!({"sessions": sessions})).unwrap();
        assert!(matches!(
            parse(&Read::Sessions(Route::default()), &bytes),
            Err("inventory-too-large")
        ));
        let config = HerdrActivityConfig {
            endpoints: (0..MAX_ENDPOINTS)
                .map(|n| Route {
                    session: format!("s{n}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert!(inventory(&config, &BTreeMap::new()).is_ok());
        assert!(matches!(
            inventory(
                &config,
                &BTreeMap::from([("local".into(), vec![Route::default()])])
            ),
            Err("inventory-too-large")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_process_io_failure_and_cancellation() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "printf '{\"error\":{\"code\":\"bad\",\"message\":\"SECRET\"}}'",
        ]);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        assert_eq!(counts(&execute(cmd).await.unwrap()), Err("api-error"));
        let mut cmd = Command::new("/bin/sh");
        cmd.args([
            "-c",
            "printf 'Permission denied (publickey) SECRET' >&2; exit 1",
        ]);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        assert_eq!(execute(cmd).await, Err("authentication"));
        let mut cmd = Command::new("/usr/bin/yes");
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        assert_eq!(execute(cmd).await, Err("response-too-large"));
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), execute(cmd))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_deadline_is_enforced() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        assert_eq!(execute(cmd).await, Err("timeout"));
    }

    #[tokio::test]
    async fn disabled_collector_never_queries_or_persists() {
        let store = Store::in_memory().await.unwrap();
        let config = HerdrActivityConfig {
            enabled: false,
            executable: "/not-a-real-fixture".into(),
            ssh_executable: "/not-a-real-fixture".into(),
            ..Default::default()
        };
        let (updates, snapshot) = watch::channel(HerdrActivitySnapshot::default());
        let (_refresh, refresh) = watch::channel(0);
        run(config, store.clone(), updates, refresh).await;
        assert!(!snapshot.borrow().enabled);
        assert!(snapshot.borrow().discovery_error.is_none());
        let now = Utc::now();
        assert!(
            store
                .load_activity_samples(now - chrono::Duration::minutes(15), now, HISTORY_LIMIT)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn history_load_preserves_times_and_downtime_gaps() {
        let fixture = Fixture::directory();
        let path = fixture.0.join("history.sqlite3");
        // Cancelling SQLx acquisition may retire a connection. A private :memory:
        // database would disappear with it, unlike the production file-backed store.
        let store = Store::open(&path).await.unwrap();
        let now = Utc::now();
        let original = aggregate(
            &mut BTreeMap::new(),
            true,
            Instant::now(),
            now - chrono::Duration::seconds(30),
        );
        store.upsert_activity_sample(&original).await.unwrap();
        let future = ActivitySample {
            sampled_at: utc_bucket(now) + chrono::Duration::seconds(2),
            ..original.clone()
        };
        store.upsert_activity_sample(&future).await.unwrap();
        let config = HerdrActivityConfig {
            executable: "/not-a-real-fixture".into(),
            ssh_executable: "/not-a-real-fixture".into(),
            discover_saved_profiles: false,
            ..Default::default()
        };
        let (updates, mut snapshots) = watch::channel(HerdrActivitySnapshot::default());
        let (_refresh, refresh) = watch::channel(0);
        let mut tasks = JoinSet::new();
        tasks.spawn(run(config, store.clone(), updates, refresh));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots.borrow().samples.contains(&original) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let snapshot = snapshots.borrow().clone();
        assert!(!snapshot.samples.contains(&future));
        assert!(snapshot.samples.len() <= 2, "must not interpolate downtime");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(store);
        let store = Store::open(&path).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while Utc::now() < future.sampled_at {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let restored = store
            .load_activity_samples(
                now - chrono::Duration::minutes(15),
                future.sampled_at,
                HISTORY_LIMIT,
            )
            .await
            .unwrap();
        assert!(restored.contains(&original));
        assert!(
            !restored.contains(&future),
            "startup must delete future history, not just hide it"
        );
    }

    #[test]
    fn rollback_cutoffs_coalesce_until_the_current_generation_is_acknowledged() {
        let now = utc_bucket(Utc::now());
        let mut clock = HistoryClock::new(now);
        clock.acknowledged = Some(0);
        assert!(clock.observe(now - chrono::Duration::seconds(30)));
        assert_eq!(clock.reset.cutoff, now - chrono::Duration::seconds(30));
        assert!(!clock.observe(now + chrono::Duration::seconds(60)));
        assert!(clock.observe(now + chrono::Duration::seconds(20)));
        assert_eq!(clock.reset.generation, 2);
        assert_eq!(clock.reset.cutoff, now - chrono::Duration::seconds(30));
        clock.acknowledged = Some(2);
        assert!(clock.observe(now + chrono::Duration::seconds(19)));
        assert_eq!(clock.reset.cutoff, now + chrono::Duration::seconds(18));
    }

    #[tokio::test]
    async fn rollback_clears_written_and_queued_history_before_catchup_restart() {
        let fixture = Fixture::directory();
        let path = fixture.0.join("history.sqlite3");
        let store = Store::open(&path).await.unwrap();
        // Keep the injected clock below the real clock: Store also filters future
        // rows using Utc::now(), independently of its caller's requested window.
        let now = utc_bucket(Utc::now()) - chrono::Duration::minutes(5);
        let sample = |seconds: i64, working| ActivitySample {
            counts: Some(ActivityCounts {
                working,
                ..Default::default()
            }),
            ..aggregate(
                &mut BTreeMap::new(),
                true,
                Instant::now(),
                now + chrono::Duration::seconds(seconds),
            )
        };
        let past = sample(-20, 7);
        let old_written = sample(20, 99);
        let old_queued = sample(22, 98);
        let current = sample(20, 5);
        store.upsert_activity_sample(&past).await.unwrap();
        let mut clock = HistoryClock::new(now);
        let (resets, reset_rx) = watch::channel(clock.reset);
        let (writes, write_rx) = mpsc::channel(2);
        // A full event channel stalls the worker after the first old write, giving
        // deterministic backpressure without holding SQLite locks or using live CLI.
        let (events_tx, mut events) = mpsc::channel(1);
        for sample in [&old_written, &old_queued] {
            writes
                .try_send(HistoryWrite {
                    generation: 0,
                    sample: sample.clone(),
                })
                .unwrap();
        }
        let mut tasks = JoinSet::new();
        tasks.spawn(persist(store.clone(), write_rx, events_tx, reset_rx));
        tokio::time::timeout(Duration::from_secs(3), async {
            assert!(matches!(events.recv().await, Some(Persistence::Reset(0))));
            loop {
                if store
                    .load_activity_samples(now, now + chrono::Duration::seconds(60), HISTORY_LIMIT)
                    .await
                    .unwrap()
                    .contains(&old_written)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            clock.acknowledged = Some(0);
            assert!(clock.observe(now - chrono::Duration::seconds(30)));
            resets.send_replace(clock.reset);
            clock.observe(now + chrono::Duration::seconds(60));
            assert!(clock.observe(now + chrono::Duration::seconds(20)));
            resets.send_replace(clock.reset);
            assert_eq!(clock.reset.cutoff, now - chrono::Duration::seconds(30));
            writes
                .try_send(HistoryWrite {
                    generation: 2,
                    sample: current.clone(),
                })
                .unwrap();
            assert!(
                writes
                    .try_send(HistoryWrite {
                        generation: 2,
                        sample: current.clone()
                    })
                    .is_err(),
                "the data queue is full but both resets were still delivered"
            );
            let mut reset_seen = false;
            let mut stale_load_seen = false;
            loop {
                match events.recv().await.unwrap() {
                    Persistence::Loaded(0, Ok(samples)) => {
                        assert_eq!(samples, vec![past.clone()]);
                        stale_load_seen = true;
                    },
                    Persistence::Reset(2) => reset_seen = true,
                    Persistence::Loaded(2, Ok(samples)) => assert!(samples.is_empty()),
                    Persistence::Health(2, None) => {
                        assert!(reset_seen && stale_load_seen);
                        break;
                    },
                    Persistence::Health(_, Some(error)) => {
                        panic!("unexpected persistence error: {error}")
                    },
                    _ => {},
                }
            }
        })
        .await
        .unwrap();
        let stored = store
            .load_activity_samples(
                now - chrono::Duration::minutes(15),
                now + chrono::Duration::minutes(2),
                HISTORY_LIMIT,
            )
            .await
            .unwrap();
        assert_eq!(stored, vec![current.clone()]);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}

        // Restart after downtime and clock catch-up: neither the deleted observation
        // nor the queued old-generation observation can reappear, and no gap is filled.
        drop(store);
        let store = Store::open(&path).await.unwrap();
        let restart = HistoryClock::new(now + chrono::Duration::seconds(90));
        let (_resets, reset_rx) = watch::channel(restart.reset);
        let (_writes, write_rx) = mpsc::channel(2);
        let (events_tx, mut events) = mpsc::channel(2);
        tasks.spawn(persist(store.clone(), write_rx, events_tx, reset_rx));
        tokio::time::timeout(Duration::from_secs(3), async {
            assert!(matches!(events.recv().await, Some(Persistence::Reset(0))));
            match events.recv().await.unwrap() {
                Persistence::Loaded(0, Ok(samples)) => assert_eq!(samples, vec![current]),
                _ => panic!("expected restart history"),
            }
        })
        .await
        .unwrap();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn directory() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "launcher-activity-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        #[cfg(unix)]
        fn new(script: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let fixture = Self::directory();
            let path = fixture.0.join("fixture");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            fixture
        }

        #[cfg(unix)]
        fn executable(&self) -> String {
            self.0.join("fixture").to_str().unwrap().to_owned()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixture_collector_discovers_polls_and_persists_without_live_installation() {
        let fixture = Fixture::new(
            r##"#!/bin/sh
if [ -n "${HERDR_SOCKET_PATH+x}${HERDR_SESSION+x}${HERDR_CONFIG_PATH+x}${XDG_CONFIG_HOME+x}${XDG_STATE_HOME+x}" ]; then exit 91; fi
case "$*" in
  'session list --json') printf '%s' '{"sessions":[{"name":"default","running":true},{"name":"named","running":true},{"name":"stopped","running":false}]}' ;;
  'machine list --json') printf '%s' '[{"target":"fixture-host","session":"default","enabled":true},{"target":"never-contact","session":"default","enabled":false}]' ;;
  '--session default agent list'|'--session named agent list') printf '%s' '{"result":{"type":"agent_list","agents":[{"terminal_id":"same-id","agent_status":"working"}]}}' ;;
  -T*)
    for arg do remote="$arg"; done
    case "$*" in *never-contact*) exit 92;; esac
    case "$remote" in
      *"'session' 'list' '--json'") printf '%s' '{"sessions":[{"name":"default","running":true},{"name":"extra","running":true}]}' ;;
      *"'--session' 'default' 'agent' 'list'"|*"'--session' 'extra' 'agent' 'list'") printf '%s' '{"result":{"type":"agent_list","agents":[{"terminal_id":"same-id","agent_status":"working"}]}}' ;;
      *) exit 93;;
    esac ;;
  *) exit 94;;
esac
"##,
        );
        for discover_remote_sessions in [false, true] {
            let config = HerdrActivityConfig {
                executable: fixture.executable(),
                ssh_executable: fixture.executable(),
                discover_remote_sessions,
                ..Default::default()
            };
            let store = Store::in_memory().await.unwrap();
            let (updates, mut snapshot) = watch::channel(HerdrActivitySnapshot::default());
            let (_refresh, refresh) = watch::channel(0);
            let mut tasks = JoinSet::new();
            tasks.spawn(run(config, store.clone(), updates, refresh));
            tokio::time::timeout(Duration::from_secs(6), async {
                loop {
                    snapshot.changed().await.unwrap();
                    let done = snapshot.borrow().samples.last().is_some_and(|sample| {
                        sample.completeness == ActivityCompleteness::Complete
                            && sample.counts.as_ref().unwrap().working
                                == if discover_remote_sessions {
                                    4
                                } else {
                                    3
                                }
                    });
                    if done {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert!(snapshot.borrow().discovery_error.is_none());
            assert!(
                snapshot
                    .borrow()
                    .endpoints
                    .iter()
                    .any(|endpoint| endpoint.transport == ActivityTransportState::Disabled)
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let now = Utc::now();
                    let samples = store
                        .load_activity_samples(
                            now - chrono::Duration::minutes(15),
                            now,
                            HISTORY_LIMIT,
                        )
                        .await
                        .unwrap();
                    if samples
                        .iter()
                        .any(|sample| sample.completeness == ActivityCompleteness::Complete)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixture_alias_replacement_rebinds_polls_and_discovery_ignoring_old_results() {
        let fixture = Fixture::new(
            r##"#!/bin/sh
dir=${0%/*}
if [ "$*" = 'machine list --json' ]; then
  target=alias-a
  if [ -e "$dir/use-b" ]; then target=alias-b; fi
  printf '[{"target":"%s","session":"default","enabled":true}]' "$target"
  exit
fi
case " $* " in
  *' alias-a '*) target=a;;
  *' alias-b '*) target=b;;
  *) exit 91;;
esac
for arg do remote="$arg"; done
case "$remote" in
  *"'session' 'list' '--json'") kind=discovery;;
  *"'--session' 'default' 'agent' 'list'") kind=default;;
  *"'--session' 'old' 'agent' 'list'") kind=old;;
  *"'--session' 'new' 'agent' 'list'") kind=new;;
  *) exit 92;;
esac
printf 'S %s %s\n' "$target" "$kind" >> "$dir/events"
held=no
if [ "$target" = a ] && [ -e "$dir/hold-a" ]; then
  held=yes
  : > "$dir/started-$kind"
  while [ -e "$dir/hold-a" ]; do /bin/sleep 0.02; done
fi
if [ "$kind" = discovery ]; then
  session=old
  if [ "$target" = b ]; then session=new; fi
  if [ "$held" = yes ]; then session=stale; fi
  printf '{"sessions":[{"name":"default","running":true},{"name":"%s","running":true}]}' "$session"
else
  status=working
  if [ "$target" = b ]; then status=idle; fi
  printf '{"result":{"type":"agent_list","agents":[{"terminal_id":"fixture","agent_status":"%s"}]}}' "$status"
fi
"##,
        );
        let config = HerdrActivityConfig {
            executable: fixture.executable(),
            ssh_executable: fixture.executable(),
            discover_local_sessions: false,
            discover_remote_sessions: true,
            alias_groups: vec![HerdrActivityAliasGroup {
                canonical: "host".into(),
                targets: vec!["alias-a".into(), "alias-b".into()],
            }],
            ..Default::default()
        };
        let (updates, mut snapshots) = watch::channel(HerdrActivitySnapshot::default());
        let (refresh, refresh_rx) = watch::channel(0);
        let mut tasks = JoinSet::new();
        tasks.spawn(run(
            config,
            Store::in_memory().await.unwrap(),
            updates,
            refresh_rx,
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots
                    .borrow()
                    .endpoints
                    .iter()
                    .filter(|e| e.freshness == ActivityFreshness::Fresh)
                    .count()
                    == 2
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        std::fs::write(fixture.0.join("hold-a"), "").unwrap();
        refresh.send(1).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !["discovery", "default", "old"]
                .iter()
                .all(|kind| fixture.0.join(format!("started-{kind}")).exists())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(fixture.0.join("use-b"), "").unwrap();
        refresh.send(2).unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                snapshots.changed().await.unwrap();
                let snapshot = snapshots.borrow();
                if snapshot.endpoints.len() == 1
                    && snapshot.endpoints[0].freshness == ActivityFreshness::NeverObserved
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let before_release = snapshots.borrow().samples.last().unwrap().sampled_at;
        let starts_before_release = std::fs::read_to_string(fixture.0.join("events"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("S a "))
            .count();
        std::fs::remove_file(fixture.0.join("hold-a")).unwrap();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                snapshots.changed().await.unwrap();
                let snapshot = snapshots.borrow();
                assert!(snapshot.endpoints.len() <= 2);
                assert!(
                    !snapshot
                        .endpoints
                        .iter()
                        .any(|endpoint| endpoint.endpoint_id.contains("stale")
                            || endpoint.endpoint_id.contains("old"))
                );
                let sample = snapshot.samples.last().unwrap();
                if sample.sampled_at > before_release
                    && let Some(counts) = &sample.counts
                {
                    assert_eq!(counts.working, 0);
                    if sample.completeness == ActivityCompleteness::Complete && counts.idle == 2 {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("events"))
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("S a "))
                .count(),
            starts_before_release
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixture_scheduler_bounds_overlap_fairness_refresh_and_abort() {
        let fixture = Fixture::new(
            r##"#!/bin/sh
dir=${0%/*}
if [ "$1" != --session ] || [ "$3 $4" != 'agent list' ]; then exit 90; fi
printf 'S %s %s\n' "$2" "$$" >> "$dir/events"
case "$2" in
  a-hung) exec /bin/sleep 30 ;;
  *) /bin/sleep 0.1 ;;
esac
printf '%s' '{"result":{"type":"agent_list","agents":[]}}'
printf 'E %s %s\n' "$2" "$$" >> "$dir/events"
"##,
        );
        let config = HerdrActivityConfig {
            executable: fixture.executable(),
            ssh_executable: fixture.executable(),
            discover_local_sessions: false,
            discover_saved_profiles: false,
            endpoints: ["a-hung", "b", "c", "d", "e", "f", "g", "h"]
                .into_iter()
                .map(|session| Route {
                    session: session.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let (updates, mut snapshots) = watch::channel(HerdrActivitySnapshot::default());
        let (refresh, refresh_rx) = watch::channel(0);
        let mut tasks = JoinSet::new();
        tasks.spawn(run(
            config,
            Store::in_memory().await.unwrap(),
            updates,
            refresh_rx,
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                snapshots.changed().await.unwrap();
                refresh.send_modify(|n| *n += 1);
                if snapshots
                    .borrow()
                    .endpoints
                    .iter()
                    .filter(|e| e.freshness == ActivityFreshness::Fresh)
                    .count()
                    == 7
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        let log = std::fs::read_to_string(fixture.0.join("events")).unwrap();
        let mut active = BTreeSet::new();
        let mut hung_pid = None;
        for line in log.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields[0] == "S" {
                assert!(active.insert(fields[1]), "overlap: {log}");
                assert!(active.len() <= MAX_REQUESTS, "concurrency exceeded: {log}");
                if fields[1] == "a-hung" {
                    assert!(hung_pid.replace(fields[2]).is_none());
                }
            } else {
                assert!(active.remove(fields[1]));
            }
        }
        let pid = hung_pid.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let alive = Command::new("/bin/kill")
                    .args(["-0", pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap()
                    .success();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixture_discovery_failure_retains_inventory_and_sanitizes_errors() {
        let fixture = Fixture::new(
            r##"#!/bin/sh
dir=${0%/*}
case "$*" in
  'session list --json')
    if [ -e "$dir/fail" ]; then printf 'SECRET' >&2; exit 1; fi
    printf '%s' '{"sessions":[{"name":"default","running":true}]}' ;;
  '--session default agent list') printf '%s' '{"result":{"type":"agent_list","agents":[]}}' ;;
  *) exit 95;;
esac
"##,
        );
        let config = HerdrActivityConfig {
            executable: fixture.executable(),
            ssh_executable: fixture.executable(),
            discover_saved_profiles: false,
            ..Default::default()
        };
        let (updates, mut snapshots) = watch::channel(HerdrActivitySnapshot::default());
        let (refresh, refresh_rx) = watch::channel(0);
        let mut tasks = JoinSet::new();
        tasks.spawn(run(
            config,
            Store::in_memory().await.unwrap(),
            updates,
            refresh_rx,
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots
                    .borrow()
                    .endpoints
                    .iter()
                    .any(|e| e.freshness == ActivityFreshness::Fresh)
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        std::fs::write(fixture.0.join("fail"), "").unwrap();
        refresh.send(1).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                snapshots.changed().await.unwrap();
                if snapshots
                    .borrow()
                    .samples
                    .last()
                    .is_some_and(|s| s.completeness == ActivityCompleteness::Partial)
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            snapshots.borrow().discovery_error.as_deref(),
            Some("process-failed")
        );
        assert_eq!(snapshots.borrow().endpoints.len(), 1);
        assert!(
            !serde_json::to_string(&*snapshots.borrow())
                .unwrap()
                .contains("SECRET")
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}
