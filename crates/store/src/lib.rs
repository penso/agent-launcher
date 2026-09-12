//! SQLite-backed persistent application state.

use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use agent_launcher_core::{
    ActivitySample, EventEnvelope, Issue, IssueKey, IssueProvider, RunEvent, RunSummary,
    WorkspaceRef,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{
    Row, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};
use thiserror::Error;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS issues (
    canonical_key TEXT PRIMARY KEY NOT NULL,
    source TEXT NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repository TEXT NOT NULL,
    native_id TEXT NOT NULL,
    identifier TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    state TEXT NOT NULL,
    url TEXT,
    author TEXT,
    labels_json TEXT NOT NULL,
    parent_id TEXT,
    blocked_by_json TEXT NOT NULL,
    priority INTEGER,
    created_at TEXT,
    updated_at TEXT,
    pull_request_json TEXT,
    activity_json TEXT
);

CREATE INDEX IF NOT EXISTS issues_source_idx ON issues(source);

CREATE TABLE IF NOT EXISTS source_checkpoints (
    source TEXT PRIMARY KEY NOT NULL,
    checkpoint_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY NOT NULL,
    issue_key TEXT NOT NULL,
    workspace_json TEXT,
    agent TEXT NOT NULL,
    state_json TEXT NOT NULL,
    message TEXT,
    session_id TEXT,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS runs_updated_idx ON runs(updated_at, id);

CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence >= 0),
    timestamp TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    PRIMARY KEY (run_id, sequence)
);

CREATE TABLE IF NOT EXISTS herdr_activity_samples (
    sampled_at_unix_ms INTEGER PRIMARY KEY NOT NULL,
    counts_json TEXT,
    expected_endpoints INTEGER NOT NULL CHECK (expected_endpoints >= 0),
    fresh_endpoints INTEGER NOT NULL CHECK (fresh_endpoints >= 0),
    stale_endpoints INTEGER NOT NULL CHECK (stale_endpoints >= 0),
    never_observed_endpoints INTEGER NOT NULL CHECK (never_observed_endpoints >= 0),
    failed_endpoints INTEGER NOT NULL CHECK (failed_endpoints >= 0),
    excluded_endpoints INTEGER NOT NULL CHECK (excluded_endpoints >= 0),
    inventory_complete INTEGER NOT NULL,
    completeness_json TEXT NOT NULL
);
"#;

const MAX_ACTIVITY_SAMPLE_BATCH: usize = 10_000;

/// Errors returned by [`Store`].
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid issue provider `{0}` in the database")]
    InvalidIssueProvider(String),
    #[error("event sequence {0} cannot be represented by SQLite")]
    SequenceOutOfRange(u64),
    #[error("negative event sequence {0} in the database")]
    InvalidStoredSequence(i64),
    #[error("event limit {0} cannot be represented by SQLite")]
    EventLimitOutOfRange(usize),
    #[error("run `{0}` was not found")]
    RunNotFound(String),
    #[error("activity endpoint count cannot be represented by SQLite or this platform")]
    ActivityCountOutOfRange,
    #[error("invalid activity sample timestamp {0} in the database")]
    InvalidActivityTimestamp(i64),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A cloneable handle to the application SQLite database.
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
    path: Option<std::path::PathBuf>,
}

/// Descriptive alias for [`Store`].
pub type SqliteStore = Store;

impl Store {
    /// Opens or creates a database at `path` and initializes its schema.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        let mut store = Self::connect(options, 5).await?;
        store.path = Some(path);
        Ok(store)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Opens a private in-memory database, primarily for tests.
    pub async fn in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        // Each SQLite in-memory connection has its own database.
        Self::connect(options, 1).await
    }

    /// Alias for [`Store::in_memory`].
    pub async fn open_in_memory() -> Result<Self> {
        Self::in_memory().await
    }

    async fn connect(options: SqliteConnectOptions, max_connections: u32) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::raw_sql(SCHEMA).execute(&mut *transaction).await?;
        let columns = sqlx::query("PRAGMA table_info(issues)")
            .fetch_all(&mut *transaction)
            .await?;
        if !columns
            .iter()
            .any(|column| column.get::<String, _>("name") == "pull_request_json")
        {
            sqlx::query("ALTER TABLE issues ADD COLUMN pull_request_json TEXT")
                .execute(&mut *transaction)
                .await?;
            // Legacy GitHub checkpoints cover issues only; force a full PR bootstrap.
            sqlx::query("DELETE FROM source_checkpoints WHERE source GLOB 'github:*'")
                .execute(&mut *transaction)
                .await?;
        }
        if !columns
            .iter()
            .any(|column| column.get::<String, _>("name") == "activity_json")
        {
            sqlx::query("ALTER TABLE issues ADD COLUMN activity_json TEXT")
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(Self { pool, path: None })
    }

    /// Atomically replaces the issues belonging to `source`.
    ///
    /// Existing issues from other sources are left untouched. Supplied issues
    /// are upserted by [`IssueKey::canonical`].
    /// For combined sources, callers must supply both issues and pull requests.
    pub async fn replace_issues(&self, source: &str, issues: &[Issue]) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM issues WHERE source = ?")
            .bind(source)
            .execute(&mut *transaction)
            .await?;

        for issue in issues {
            upsert_issue(&mut transaction, source, issue).await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    /// Upserts changed issues without removing other records from the source.
    pub async fn upsert_issues(&self, source: &str, issues: &[Issue]) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        for issue in issues {
            upsert_issue(&mut transaction, source, issue).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Loads all current issues in canonical-key order.
    pub async fn load_issues(&self) -> Result<Vec<Issue>> {
        let rows = sqlx::query(
            "SELECT provider, host, repository, native_id, identifier, title, description, \
             state, url, author, labels_json, parent_id, blocked_by_json, priority, \
             created_at, updated_at, pull_request_json, activity_json FROM issues ORDER BY canonical_key ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(issue_from_row).collect()
    }

    /// Loads only records belonging to this source's persistent cache.
    pub async fn load_source_issues(&self, source: &str) -> Result<Vec<Issue>> {
        let rows = sqlx::query(
            "SELECT provider, host, repository, native_id, identifier, title, description, \
             state, url, author, labels_json, parent_id, blocked_by_json, priority, \
             created_at, updated_at, pull_request_json, activity_json FROM issues WHERE source = ? ORDER BY canonical_key ASC",
        )
        .bind(source)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(issue_from_row).collect()
    }

    /// Alias for [`Store::load_issues`].
    pub async fn load_current_issues(&self) -> Result<Vec<Issue>> {
        self.load_issues().await
    }

    /// Gets the opaque JSON checkpoint for `source`.
    pub async fn source_checkpoint(&self, source: &str) -> Result<Option<Value>> {
        let checkpoint = sqlx::query_scalar::<_, String>(
            "SELECT checkpoint_json FROM source_checkpoints WHERE source = ?",
        )
        .bind(source)
        .fetch_optional(&self.pool)
        .await?;

        checkpoint
            .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    /// Alias for [`Store::source_checkpoint`].
    pub async fn get_source_checkpoint(&self, source: &str) -> Result<Option<Value>> {
        self.source_checkpoint(source).await
    }

    /// Sets or replaces the opaque JSON checkpoint for `source`.
    pub async fn set_source_checkpoint(&self, source: &str, checkpoint: &Value) -> Result<()> {
        let checkpoint = serde_json::to_string(checkpoint)?;
        sqlx::query(
            "INSERT INTO source_checkpoints (source, checkpoint_json) VALUES (?, ?) \
             ON CONFLICT(source) DO UPDATE SET checkpoint_json = excluded.checkpoint_json",
        )
        .bind(source)
        .bind(checkpoint)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes issues and checkpoints belonging to sources that are no longer active.
    pub async fn prune_inactive_sources(&self, active_sources: &[String]) -> Result<()> {
        let active_sources: HashSet<&str> = active_sources.iter().map(String::as_str).collect();
        let mut transaction = self.pool.begin().await?;
        let stored_sources = sqlx::query_scalar::<_, String>(
            "SELECT source FROM issues UNION SELECT source FROM source_checkpoints",
        )
        .fetch_all(&mut *transaction)
        .await?;

        for source in stored_sources {
            if active_sources.contains(source.as_str()) {
                continue;
            }
            sqlx::query("DELETE FROM issues WHERE source = ?")
                .bind(&source)
                .execute(&mut *transaction)
                .await?;
            sqlx::query("DELETE FROM source_checkpoints WHERE source = ?")
                .bind(&source)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        Ok(())
    }

    /// Persists aggregate counts and coverage only, independently of launcher runs.
    ///
    /// Timestamps are truncated to Unix milliseconds. The last successful write
    /// replaces every field of an existing millisecond bucket, including null counts.
    pub async fn upsert_activity_sample(&self, sample: &ActivitySample) -> Result<()> {
        let mut query = sqlx::query(
            "INSERT INTO herdr_activity_samples (sampled_at_unix_ms, counts_json, \
             expected_endpoints, fresh_endpoints, stale_endpoints, never_observed_endpoints, \
             failed_endpoints, excluded_endpoints, inventory_complete, completeness_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(sampled_at_unix_ms) DO UPDATE SET counts_json = excluded.counts_json, \
             expected_endpoints = excluded.expected_endpoints, fresh_endpoints = excluded.fresh_endpoints, \
             stale_endpoints = excluded.stale_endpoints, never_observed_endpoints = excluded.never_observed_endpoints, \
             failed_endpoints = excluded.failed_endpoints, excluded_endpoints = excluded.excluded_endpoints, \
             inventory_complete = excluded.inventory_complete, completeness_json = excluded.completeness_json",
        )
        .bind(sample.sampled_at.timestamp_millis())
        .bind(serialize_optional(sample.counts.as_ref())?);
        for count in [
            sample.expected_endpoints,
            sample.fresh_endpoints,
            sample.stale_endpoints,
            sample.never_observed_endpoints,
            sample.failed_endpoints,
            sample.excluded_endpoints,
        ] {
            query =
                query.bind(i64::try_from(count).map_err(|_| StoreError::ActivityCountOutOfRange)?);
        }
        query
            .bind(sample.inventory_complete)
            .bind(serde_json::to_string(&sample.completeness)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Loads the newest samples in the inclusive UTC window, ordered oldest first.
    ///
    /// Future rows (relative to both `until` and the current clock) are excluded.
    /// At most `min(limit, 10_000)` rows are returned; no gaps are filled.
    pub async fn load_activity_samples(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ActivitySample>> {
        // Round the lower bound up so sub-millisecond windows stay inclusive
        // without admitting a persisted timestamp earlier than `since`.
        let since_ms = since.timestamp_millis()
            + i64::from(!since.timestamp_subsec_nanos().is_multiple_of(1_000_000));
        let rows = sqlx::query(
            "SELECT * FROM (SELECT * FROM herdr_activity_samples \
             WHERE sampled_at_unix_ms >= ? AND sampled_at_unix_ms <= ? \
             ORDER BY sampled_at_unix_ms DESC LIMIT ?) ORDER BY sampled_at_unix_ms ASC",
        )
        .bind(since_ms)
        .bind(until.min(Utc::now()).timestamp_millis())
        .bind(limit.min(MAX_ACTIVITY_SAMPLE_BATCH) as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(activity_sample_from_row).collect()
    }

    /// Deletes at most `min(limit, 10_000)` samples strictly before `before`,
    /// oldest first. Launcher runs and events are never touched.
    pub async fn prune_activity_samples(&self, before: DateTime<Utc>, limit: usize) -> Result<()> {
        let before_ms = before.timestamp_millis()
            + i64::from(!before.timestamp_subsec_nanos().is_multiple_of(1_000_000));
        sqlx::query(
            "DELETE FROM herdr_activity_samples WHERE sampled_at_unix_ms IN (\
             SELECT sampled_at_unix_ms FROM herdr_activity_samples \
             WHERE sampled_at_unix_ms < ? ORDER BY sampled_at_unix_ms ASC LIMIT ?)",
        )
        .bind(before_ms)
        .bind(limit.min(MAX_ACTIVITY_SAMPLE_BATCH) as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically deletes all samples at or after `cutoff`, without touching runs
    /// or events. Use after startup or clock rollback to permanently invalidate
    /// future history rather than merely hiding it until the clock catches up.
    pub async fn clear_activity_samples_from(&self, cutoff: DateTime<Utc>) -> Result<()> {
        let cutoff_ms = cutoff.timestamp_millis()
            + i64::from(!cutoff.timestamp_subsec_nanos().is_multiple_of(1_000_000));
        sqlx::query("DELETE FROM herdr_activity_samples WHERE sampled_at_unix_ms >= ?")
            .bind(cutoff_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Inserts a new run. A duplicate run ID is an error.
    pub async fn insert_run(&self, run: &RunSummary) -> Result<()> {
        write_run(&self.pool, run, false).await
    }

    /// Updates an existing run.
    pub async fn update_run(&self, run: &RunSummary) -> Result<()> {
        let workspace = serialize_optional(run.workspace.as_ref())?;
        let state = serde_json::to_string(&run.state)?;
        let result = sqlx::query(
            "UPDATE runs SET issue_key = ?, workspace_json = ?, agent = ?, state_json = ?, \
             message = ?, session_id = ?, started_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&run.issue_key)
        .bind(workspace)
        .bind(&run.agent)
        .bind(state)
        .bind(&run.message)
        .bind(&run.session_id)
        .bind(run.started_at)
        .bind(run.updated_at)
        .bind(&run.id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(StoreError::RunNotFound(run.id.clone()));
        }
        Ok(())
    }

    /// Atomically updates a run and appends its resulting events.
    pub async fn update_run_with_events(
        &self,
        run: &RunSummary,
        events: &[EventEnvelope],
    ) -> Result<()> {
        let workspace = serialize_optional(run.workspace.as_ref())?;
        let state = serde_json::to_string(&run.state)?;
        let mut prepared_events = Vec::with_capacity(events.len());
        for event in events {
            let sequence = i64::try_from(event.sequence)
                .map_err(|_| StoreError::SequenceOutOfRange(event.sequence))?;
            prepared_events.push((event, sequence, serde_json::to_string(&event.payload)?));
        }

        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE runs SET issue_key = ?, workspace_json = ?, agent = ?, state_json = ?, \
             message = ?, session_id = ?, started_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&run.issue_key)
        .bind(workspace)
        .bind(&run.agent)
        .bind(state)
        .bind(&run.message)
        .bind(&run.session_id)
        .bind(run.started_at)
        .bind(run.updated_at)
        .bind(&run.id)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::RunNotFound(run.id.clone()));
        }

        for (event, sequence, payload) in prepared_events {
            sqlx::query(
                "INSERT INTO events (run_id, sequence, timestamp, payload_json) VALUES (?, ?, ?, ?)",
            )
            .bind(&event.run_id)
            .bind(sequence)
            .bind(event.timestamp)
            .bind(payload)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Inserts or replaces a run by ID.
    pub async fn upsert_run(&self, run: &RunSummary) -> Result<()> {
        write_run(&self.pool, run, true).await
    }

    /// Loads one run by ID.
    pub async fn load_run(&self, run_id: &str) -> Result<Option<RunSummary>> {
        let row = sqlx::query(
            "SELECT id, issue_key, workspace_json, agent, state_json, message, session_id, \
             started_at, updated_at FROM runs WHERE id = ?",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads all runs, newest update first with ID as a stable tie-breaker.
    pub async fn load_runs(&self) -> Result<Vec<RunSummary>> {
        let rows = sqlx::query(
            "SELECT id, issue_key, workspace_json, agent, state_json, message, session_id, \
             started_at, updated_at FROM runs ORDER BY updated_at DESC, id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Atomically removes a run and all of its persisted events.
    pub async fn delete_run(&self, run_id: &str) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(&mut *transaction)
            .await?
            > 0;
        if !exists {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        }
        sqlx::query("DELETE FROM events WHERE run_id = ?")
            .bind(run_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM runs WHERE id = ?")
            .bind(run_id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Appends an event. A duplicate `(run_id, sequence)` is an error.
    pub async fn append_event(&self, event: &EventEnvelope) -> Result<()> {
        let sequence = i64::try_from(event.sequence)
            .map_err(|_| StoreError::SequenceOutOfRange(event.sequence))?;
        let payload = serde_json::to_string(&event.payload)?;
        sqlx::query(
            "INSERT INTO events (run_id, sequence, timestamp, payload_json) VALUES (?, ?, ?, ?)",
        )
        .bind(&event.run_id)
        .bind(sequence)
        .bind(event.timestamp)
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads a run's events in ascending sequence order.
    pub async fn load_events(&self, run_id: &str) -> Result<Vec<EventEnvelope>> {
        let rows = sqlx::query(
            "SELECT run_id, sequence, timestamp, payload_json FROM events \
             WHERE run_id = ? ORDER BY sequence ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(event_from_row).collect()
    }

    /// Loads the newest `limit` events for a run, ordered by ascending sequence.
    pub async fn load_recent_events(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>> {
        let limit = i64::try_from(limit).map_err(|_| StoreError::EventLimitOutOfRange(limit))?;
        let rows = sqlx::query(
            "SELECT run_id, sequence, timestamp, payload_json FROM (\
             SELECT run_id, sequence, timestamp, payload_json FROM events \
             WHERE run_id = ? ORDER BY sequence DESC LIMIT ?\
             ) ORDER BY sequence ASC",
        )
        .bind(run_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(event_from_row).collect()
    }
}

async fn upsert_issue(
    transaction: &mut Transaction<'_, Sqlite>,
    source: &str,
    issue: &Issue,
) -> Result<()> {
    let labels = serde_json::to_string(&issue.labels)?;
    let blocked_by = serde_json::to_string(&issue.blocked_by)?;
    sqlx::query(
        "INSERT INTO issues (canonical_key, source, provider, host, repository, native_id, \
         identifier, title, description, state, url, author, labels_json, parent_id, \
         blocked_by_json, priority, created_at, updated_at, pull_request_json, activity_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(canonical_key) DO UPDATE SET source = excluded.source, \
         provider = excluded.provider, host = excluded.host, repository = excluded.repository, \
         native_id = excluded.native_id, identifier = excluded.identifier, title = excluded.title, \
         description = excluded.description, state = excluded.state, url = excluded.url, \
         author = excluded.author, labels_json = excluded.labels_json, \
         parent_id = excluded.parent_id, blocked_by_json = excluded.blocked_by_json, \
         priority = excluded.priority, created_at = excluded.created_at, \
         updated_at = excluded.updated_at, pull_request_json = excluded.pull_request_json, activity_json = excluded.activity_json",
    )
    .bind(issue.key.canonical())
    .bind(source)
    .bind(issue.key.provider.as_str())
    .bind(&issue.key.host)
    .bind(&issue.key.repository)
    .bind(&issue.key.native_id)
    .bind(&issue.identifier)
    .bind(&issue.title)
    .bind(&issue.description)
    .bind(&issue.state)
    .bind(&issue.url)
    .bind(&issue.author)
    .bind(labels)
    .bind(&issue.parent_id)
    .bind(blocked_by)
    .bind(issue.priority)
    .bind(issue.created_at)
    .bind(issue.updated_at)
    .bind(serialize_optional(issue.pull_request.as_ref())?)
    .bind(serialize_optional(issue.activity.as_ref())?)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn activity_sample_from_row(row: &SqliteRow) -> Result<ActivitySample> {
    let sampled_at_ms = row.try_get("sampled_at_unix_ms")?;
    let count = |column| -> Result<usize> {
        usize::try_from(row.try_get::<i64, _>(column)?)
            .map_err(|_| StoreError::ActivityCountOutOfRange)
    };
    Ok(ActivitySample {
        sampled_at: DateTime::from_timestamp_millis(sampled_at_ms)
            .ok_or(StoreError::InvalidActivityTimestamp(sampled_at_ms))?,
        counts: row
            .try_get::<Option<String>, _>("counts_json")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        expected_endpoints: count("expected_endpoints")?,
        fresh_endpoints: count("fresh_endpoints")?,
        stale_endpoints: count("stale_endpoints")?,
        never_observed_endpoints: count("never_observed_endpoints")?,
        failed_endpoints: count("failed_endpoints")?,
        excluded_endpoints: count("excluded_endpoints")?,
        inventory_complete: row.try_get("inventory_complete")?,
        completeness: deserialize_json(row, "completeness_json")?,
    })
}

fn issue_from_row(row: &SqliteRow) -> Result<Issue> {
    let provider: String = row.try_get("provider")?;
    let provider = IssueProvider::from_str(&provider)
        .map_err(|_| StoreError::InvalidIssueProvider(provider))?;
    Ok(Issue {
        activity: row
            .try_get::<Option<String>, _>("activity_json")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        key: IssueKey {
            provider,
            host: row.try_get("host")?,
            repository: row.try_get("repository")?,
            native_id: row.try_get("native_id")?,
        },
        identifier: row.try_get("identifier")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        state: row.try_get("state")?,
        url: row.try_get("url")?,
        author: row.try_get("author")?,
        labels: deserialize_json(row, "labels_json")?,
        parent_id: row.try_get("parent_id")?,
        blocked_by: deserialize_json(row, "blocked_by_json")?,
        priority: row.try_get("priority")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        pull_request: row
            .try_get::<Option<String>, _>("pull_request_json")?
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
    })
}

async fn write_run(pool: &SqlitePool, run: &RunSummary, upsert: bool) -> Result<()> {
    let workspace = serialize_optional(run.workspace.as_ref())?;
    let state = serde_json::to_string(&run.state)?;
    let conflict = if upsert {
        " ON CONFLICT(id) DO UPDATE SET issue_key = excluded.issue_key, \
         workspace_json = excluded.workspace_json, agent = excluded.agent, \
         state_json = excluded.state_json, message = excluded.message, \
         session_id = excluded.session_id, started_at = excluded.started_at, \
         updated_at = excluded.updated_at"
    } else {
        ""
    };
    let query = format!(
        "INSERT INTO runs (id, issue_key, workspace_json, agent, state_json, message, session_id, \
         started_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?){conflict}"
    );
    sqlx::query(&query)
        .bind(&run.id)
        .bind(&run.issue_key)
        .bind(workspace)
        .bind(&run.agent)
        .bind(state)
        .bind(&run.message)
        .bind(&run.session_id)
        .bind(run.started_at)
        .bind(run.updated_at)
        .execute(pool)
        .await?;
    Ok(())
}

fn run_from_row(row: &SqliteRow) -> Result<RunSummary> {
    let workspace = row
        .try_get::<Option<String>, _>("workspace_json")?
        .map(|value| serde_json::from_str::<WorkspaceRef>(&value))
        .transpose()?;
    let state = deserialize_json(row, "state_json")?;
    Ok(RunSummary {
        id: row.try_get("id")?,
        issue_key: row.try_get("issue_key")?,
        workspace,
        agent: row.try_get("agent")?,
        state,
        message: row.try_get("message")?,
        session_id: row.try_get("session_id")?,
        started_at: row.try_get("started_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn event_from_row(row: &SqliteRow) -> Result<EventEnvelope> {
    let sequence = row.try_get::<i64, _>("sequence")?;
    let sequence =
        u64::try_from(sequence).map_err(|_| StoreError::InvalidStoredSequence(sequence))?;
    Ok(EventEnvelope {
        run_id: row.try_get("run_id")?,
        sequence,
        timestamp: row.try_get("timestamp")?,
        payload: deserialize_json::<RunEvent>(row, "payload_json")?,
    })
}

fn serialize_optional<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn deserialize_json<T: serde::de::DeserializeOwned>(row: &SqliteRow, column: &str) -> Result<T> {
    let value: String = row.try_get(column)?;
    Ok(serde_json::from_str(&value)?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_launcher_core::{
        ActivityCompleteness, ActivityCounts, BackendKind, OutputStream, RunState,
    };
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde_json::json;

    use super::*;

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().unwrap()
    }

    fn activity_sample(seconds: i64) -> ActivitySample {
        ActivitySample {
            sampled_at: timestamp(seconds),
            counts: Some(ActivityCounts {
                working: 1,
                blocked: 2,
                idle: 3,
                unseen_done: 4,
                unknown: u64::MAX,
            }),
            expected_endpoints: 10,
            fresh_endpoints: 2,
            stale_endpoints: 3,
            never_observed_endpoints: 5,
            failed_endpoints: 7,
            excluded_endpoints: 11,
            inventory_complete: false,
            completeness: ActivityCompleteness::Partial,
        }
    }

    #[tokio::test]
    async fn activity_samples_round_trip_missing_zero_and_all_coverage() {
        let store = Store::in_memory().await.unwrap();
        let partial = activity_sample(100);
        let mut missing = activity_sample(101);
        missing.counts = None;
        missing.completeness = ActivityCompleteness::Missing;
        let mut zero = activity_sample(102);
        zero.counts = Some(ActivityCounts::default());
        zero.inventory_complete = true;
        zero.completeness = ActivityCompleteness::Complete;
        let expected = vec![partial, missing, zero];
        for sample in &expected {
            store.upsert_activity_sample(sample).await.unwrap();
        }
        assert_eq!(
            store
                .load_activity_samples(timestamp(100), timestamp(102), 10)
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn activity_samples_duplicate_millisecond_overwrites_every_field() {
        let store = Store::in_memory().await.unwrap();
        let original = activity_sample(100);
        store.upsert_activity_sample(&original).await.unwrap();
        let mut replacement = ActivitySample {
            sampled_at: original.sampled_at + Duration::nanoseconds(999_999),
            counts: None,
            expected_endpoints: 20,
            fresh_endpoints: 0,
            stale_endpoints: 12,
            never_observed_endpoints: 8,
            failed_endpoints: 6,
            excluded_endpoints: 9,
            inventory_complete: true,
            completeness: ActivityCompleteness::Missing,
        };
        store.upsert_activity_sample(&replacement).await.unwrap();
        replacement.sampled_at = original.sampled_at;
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            vec![replacement]
        );
        store.upsert_activity_sample(&original).await.unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            vec![original]
        );
    }

    #[tokio::test]
    async fn activity_samples_load_bounded_newest_window_without_future_rows() {
        let store = Store::in_memory().await.unwrap();
        for seconds in [105, 99, 103, 100, 102, 101, 104] {
            store
                .upsert_activity_sample(&activity_sample(seconds))
                .await
                .unwrap();
        }
        let future = Utc::now() + Duration::days(1);
        let mut sample = activity_sample(0);
        sample.sampled_at = future;
        store.upsert_activity_sample(&sample).await.unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(100), timestamp(104), 3)
                .await
                .unwrap(),
            vec![
                activity_sample(102),
                activity_sample(103),
                activity_sample(104)
            ]
        );
        assert_eq!(
            store
                .load_activity_samples(timestamp(100), future, usize::MAX)
                .await
                .unwrap()
                .len(),
            6
        );
        for (since, until, limit) in [
            (timestamp(100), timestamp(104), 0),
            (timestamp(104), timestamp(100), 10),
            (
                timestamp(100) + Duration::nanoseconds(1),
                timestamp(100),
                10,
            ),
            (future, future, 10),
        ] {
            assert!(
                store
                    .load_activity_samples(since, until, limit)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn activity_samples_prune_is_bounded_and_isolated_from_runs() {
        let store = Store::in_memory().await.unwrap();
        let run = run("unrelated", timestamp(100));
        let event = EventEnvelope {
            run_id: run.id.clone(),
            sequence: 0,
            timestamp: run.updated_at,
            payload: RunEvent::Completed {
                success: true,
                message: None,
            },
        };
        store.insert_run(&run).await.unwrap();
        store.append_event(&event).await.unwrap();
        for seconds in [103, 100, 102, 101] {
            store
                .upsert_activity_sample(&activity_sample(seconds))
                .await
                .unwrap();
        }
        store
            .prune_activity_samples(timestamp(103), 0)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap()
                .len(),
            4
        );
        store
            .prune_activity_samples(timestamp(103), 2)
            .await
            .unwrap();
        let remaining = vec![activity_sample(102), activity_sample(103)];
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            remaining
        );
        assert_eq!(store.load_run(&run.id).await.unwrap(), Some(run.clone()));
        assert_eq!(store.load_events(&run.id).await.unwrap(), vec![event]);
        store.delete_run(&run.id).await.unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            remaining
        );
        store
            .prune_activity_samples(timestamp(103), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            vec![activity_sample(103)]
        );
        store
            .prune_activity_samples(timestamp(103) + Duration::nanoseconds(1), 1)
            .await
            .unwrap();
        assert!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn activity_samples_enforce_hard_load_and_prune_bounds() {
        let store = Store::in_memory().await.unwrap();
        store
            .upsert_activity_sample(&activity_sample(0))
            .await
            .unwrap();
        sqlx::query(
            "WITH RECURSIVE buckets(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM buckets WHERE n < ?) \
             INSERT INTO herdr_activity_samples SELECT n, counts_json, expected_endpoints, \
             fresh_endpoints, stale_endpoints, never_observed_endpoints, failed_endpoints, \
             excluded_endpoints, inventory_complete, completeness_json \
             FROM buckets CROSS JOIN herdr_activity_samples WHERE sampled_at_unix_ms = 0",
        )
        .bind(MAX_ACTIVITY_SAMPLE_BATCH as i64 + 1)
        .execute(&store.pool).await.unwrap();
        let loaded = store
            .load_activity_samples(timestamp(0), timestamp(100), usize::MAX)
            .await
            .unwrap();
        assert_eq!(loaded.len(), MAX_ACTIVITY_SAMPLE_BATCH);
        assert_eq!(loaded[0].sampled_at.timestamp_millis(), 2);
        assert_eq!(
            loaded.last().unwrap().sampled_at.timestamp_millis(),
            MAX_ACTIVITY_SAMPLE_BATCH as i64 + 1
        );
        store
            .prune_activity_samples(timestamp(100), usize::MAX)
            .await
            .unwrap();
        let loaded = store
            .load_activity_samples(timestamp(0), timestamp(100), usize::MAX)
            .await
            .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded[0].sampled_at.timestamp_millis(),
            MAX_ACTIVITY_SAMPLE_BATCH as i64
        );
    }

    #[tokio::test]
    async fn activity_samples_migrate_old_schema_and_survive_reopen() {
        let path = std::env::temp_dir().join(format!(
            "agent-launcher-activity-migration-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        let old_schema = SCHEMA
            .split("CREATE TABLE IF NOT EXISTS herdr_activity_samples")
            .next()
            .unwrap();
        sqlx::raw_sql(old_schema).execute(&pool).await.unwrap();
        let original = run("legacy-run", timestamp(100));
        write_run(&pool, &original, false).await.unwrap();
        pool.close().await;
        for reopen in 0..2 {
            let store = Store::open(&path).await.unwrap();
            assert_eq!(
                store.load_run(&original.id).await.unwrap(),
                Some(original.clone())
            );
            let loaded = store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap();
            assert_eq!(
                loaded,
                if reopen == 0 {
                    vec![]
                } else {
                    vec![activity_sample(100)]
                }
            );
            store
                .upsert_activity_sample(&activity_sample(100))
                .await
                .unwrap();
            store.pool.close().await;
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn activity_samples_future_invalidation_survives_reopen_and_preserves_history() {
        let path = std::env::temp_dir().join(format!(
            "agent-launcher-activity-invalidation-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&path).await.unwrap();
        let original = run("retained-run", timestamp(100));
        let event = EventEnvelope {
            run_id: original.id.clone(),
            sequence: 0,
            timestamp: original.updated_at,
            payload: RunEvent::Completed {
                success: true,
                message: None,
            },
        };
        store.insert_run(&original).await.unwrap();
        store.append_event(&event).await.unwrap();
        for seconds in [99, 100, 101] {
            store
                .upsert_activity_sample(&activity_sample(seconds))
                .await
                .unwrap();
        }
        let mut future = activity_sample(0);
        future.sampled_at = Utc::now() + Duration::days(1);
        store.upsert_activity_sample(&future).await.unwrap();

        // A fractional cutoff must retain the millisecond immediately before it.
        store
            .clear_activity_samples_from(timestamp(100) + Duration::nanoseconds(1))
            .await
            .unwrap();
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), timestamp(200), 10)
                .await
                .unwrap(),
            vec![activity_sample(99), activity_sample(100)]
        );
        store
            .clear_activity_samples_from(timestamp(100))
            .await
            .unwrap();
        store.pool.close().await;

        let store = Store::open(&path).await.unwrap();
        // Loading beyond the invalidated window simulates catchup. Inspecting all
        // stored keys also proves real future rows were deleted, not just hidden.
        assert_eq!(
            store
                .load_activity_samples(timestamp(0), future.sampled_at, 10)
                .await
                .unwrap(),
            vec![activity_sample(99)]
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT sampled_at_unix_ms FROM herdr_activity_samples ORDER BY sampled_at_unix_ms"
            )
            .fetch_all(&store.pool)
            .await
            .unwrap(),
            vec![99_000]
        );
        assert_eq!(
            store.load_run(&original.id).await.unwrap(),
            Some(original.clone())
        );
        assert_eq!(store.load_events(&original.id).await.unwrap(), vec![event]);
        store
            .clear_activity_samples_from(timestamp(100))
            .await
            .unwrap();
        store.pool.close().await;
        std::fs::remove_file(path).unwrap();
    }

    fn issue(provider: IssueProvider, native_id: &str, title: &str) -> Issue {
        Issue {
            pull_request: None,
            activity: None,
            key: IssueKey {
                provider,
                host: "example.com".to_string(),
                repository: "acme/widgets".to_string(),
                native_id: native_id.to_string(),
            },
            identifier: format!("W-{native_id}"),
            title: title.to_string(),
            description: Some("A detailed description".to_string()),
            state: "open".to_string(),
            url: Some(format!("https://example.com/issues/{native_id}")),
            author: Some("octocat".to_string()),
            labels: vec!["bug".to_string(), "urgent".to_string()],
            parent_id: Some("epic-1".to_string()),
            blocked_by: vec!["W-9".to_string()],
            priority: Some(1),
            created_at: Some(timestamp(1_700_000_000)),
            updated_at: Some(timestamp(1_700_000_100)),
        }
    }

    fn run(id: &str, updated_at: DateTime<Utc>) -> RunSummary {
        RunSummary {
            id: id.to_string(),
            issue_key: "github:example.com:acme/widgets:1".to_string(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Native,
                id: "workspace-1".to_string(),
                host: Some("builder".to_string()),
                path: Some(PathBuf::from("/tmp/widgets")),
                branch: "agent/issue-1".to_string(),
            }),
            agent: "opencode".to_string(),
            state: RunState::Running,
            message: Some("working".to_string()),
            session_id: Some("session-1".to_string()),
            started_at: timestamp(1_700_000_000),
            updated_at,
        }
    }

    #[tokio::test]
    async fn issues_round_trip_and_replacement_is_source_isolated() {
        let store = Store::in_memory().await.unwrap();
        let github_one = issue(IssueProvider::Github, "1", "First");
        let github_two = issue(IssueProvider::Github, "2", "Second");
        let beads = issue(IssueProvider::Beads, "local-1", "Local");

        store
            .replace_issues("github", &[github_two.clone(), github_one.clone()])
            .await
            .unwrap();
        store
            .replace_issues("beads", std::slice::from_ref(&beads))
            .await
            .unwrap();

        let mut updated = github_two.clone();
        updated.title = "Second, updated".to_string();
        store
            .replace_issues("github", std::slice::from_ref(&updated))
            .await
            .unwrap();

        let loaded = store.load_current_issues().await.unwrap();
        let mut expected = vec![updated, beads];
        expected.sort_by_key(|value| value.key.canonical());
        assert_eq!(loaded, expected);
        assert!(!loaded.contains(&github_one));
    }

    #[tokio::test]
    async fn pr_metadata_and_identity_survive_cache_updates() {
        let store = Store::in_memory().await.unwrap();
        let mut ordinary = issue(IssueProvider::Github, "123", "Issue");
        ordinary.activity = Some(agent_launcher_core::ItemActivity {
            comments: Some(0),
            review_comments: None,
            commits: Some(u64::MAX),
        });
        ordinary.identifier = "#123".into();
        let mut pr = ordinary.clone();
        pr.key.native_id = "pr/123".into();
        pr.state = "draft".into();
        pr.pull_request = Some(agent_launcher_core::PullRequestMetadata {
            number: 123,
            additions: None,
            deletions: Some(0),
            base_ref: "main".into(),
            head_ref: "feature".into(),
            base_sha: "base-sha".into(),
            head_sha: "head-sha".into(),
            head_repository: Some("contributor/widgets".into()),
        });
        assert_ne!(ordinary.key.canonical(), pr.key.canonical());
        store
            .replace_issues("github", &[ordinary.clone(), pr.clone()])
            .await
            .unwrap();
        assert_eq!(store.load_issues().await.unwrap(), vec![
            ordinary.clone(),
            pr.clone()
        ]);

        let mut updated = ordinary.clone();
        updated.title = "Updated issue".into();
        store
            .upsert_issues("github", &[updated.clone()])
            .await
            .unwrap();
        assert_eq!(store.load_issues().await.unwrap(), vec![
            updated,
            pr.clone()
        ]);

        pr.state = "merged".into();
        pr.pull_request.as_mut().unwrap().additions = Some(20);
        pr.pull_request.as_mut().unwrap().head_repository = None;
        store
            .replace_issues("github", &[ordinary.clone(), pr.clone()])
            .await
            .unwrap();
        assert_eq!(store.load_issues().await.unwrap(), vec![ordinary, pr]);
    }

    #[test]
    fn legacy_serialized_issues_default_to_no_pr_metadata() {
        let expected = issue(IssueProvider::Github, "123", "Existing issue");
        let mut value = serde_json::to_value(&expected).unwrap();
        value.as_object_mut().unwrap().remove("pull_request");
        value.as_object_mut().unwrap().remove("activity");
        assert_eq!(serde_json::from_value::<Issue>(value).unwrap(), expected);
    }

    #[tokio::test]
    async fn only_legacy_metadata_migration_invalidates_github_checkpoints() {
        for legacy in [true, false] {
            let path = std::env::temp_dir().join(format!(
                "agent-launcher-pr-migration-{}-{}.sqlite",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let options = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .unwrap();
            let schema = if legacy {
                SCHEMA
                    .replace(",\n    pull_request_json TEXT", "")
                    .replace(",\n    activity_json TEXT", "")
            } else {
                SCHEMA.replace(",\n    activity_json TEXT", "")
            };
            sqlx::raw_sql(&schema).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO issues (canonical_key, source, provider, host, repository, native_id, identifier, title, state, labels_json, blocked_by_json) VALUES ('github:github.com:acme/app:1', 'github', 'github', 'github.com', 'acme/app', '1', '#1', 'Legacy', 'open', '[]', '[]')")
            .execute(&pool).await.unwrap();
            let checkpoint =
                json!({"updated_at": Utc::now(), "last_full_at": Utc::now(), "etag": null});
            let sources = [
                "github:github.com:acme/app",
                "github:git.example.com:acme/empty",
                "gitlab:gitlab.com:acme/app",
                "beads:localhost:repo",
            ];
            for source in sources {
                sqlx::query("INSERT INTO source_checkpoints VALUES (?, ?)")
                    .bind(source)
                    .bind(checkpoint.to_string())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            pool.close().await;

            // Reopening twice also exercises migration idempotence.
            for reopen in 0..2 {
                let store = Store::open(&path).await.unwrap();
                let issues = store.load_issues().await.unwrap();
                assert_eq!(issues.len(), 1);
                assert_eq!(issues[0].title, "Legacy");
                assert!(issues[0].pull_request.is_none());
                assert_eq!(
                    issues[0].activity,
                    if reopen == 0 {
                        None
                    } else {
                        Some(agent_launcher_core::ItemActivity {
                            comments: Some(0),
                            review_comments: Some(u64::MAX),
                            commits: None,
                        })
                    }
                );
                let mut updated = issues[0].clone();
                updated.activity = Some(agent_launcher_core::ItemActivity {
                    comments: Some(0),
                    review_comments: Some(u64::MAX),
                    commits: None,
                });
                store
                    .upsert_issues("github", &[updated.clone()])
                    .await
                    .unwrap();
                assert_eq!(store.load_source_issues("github").await.unwrap(), vec![
                    updated
                ]);
                for source in sources {
                    let invalidated = legacy && reopen == 0 && source.starts_with("github:");
                    assert_eq!(
                        store.source_checkpoint(source).await.unwrap(),
                        if invalidated {
                            None
                        } else {
                            Some(checkpoint.clone())
                        },
                        "legacy={legacy}, reopen={reopen}, source={source}"
                    );
                    store
                        .set_source_checkpoint(source, &checkpoint)
                        .await
                        .unwrap();
                }
                store.pool.close().await;
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn checkpoints_round_trip_update_and_remain_isolated() {
        let store = Store::in_memory().await.unwrap();
        assert_eq!(store.get_source_checkpoint("github").await.unwrap(), None);

        store
            .set_source_checkpoint("github", &json!({"cursor": "one", "page": 2}))
            .await
            .unwrap();
        store
            .set_source_checkpoint("gitlab", &json!(["opaque", 7]))
            .await
            .unwrap();
        store
            .set_source_checkpoint("github", &json!({"cursor": "two"}))
            .await
            .unwrap();

        assert_eq!(
            store.get_source_checkpoint("github").await.unwrap(),
            Some(json!({"cursor": "two"}))
        );
        assert_eq!(
            store.get_source_checkpoint("gitlab").await.unwrap(),
            Some(json!(["opaque", 7]))
        );
    }

    #[tokio::test]
    async fn inactive_sources_prune_issues_and_checkpoints_atomically() {
        let store = Store::in_memory().await.unwrap();
        let active_issue = issue(IssueProvider::Github, "1", "Active");
        let inactive_issue = issue(IssueProvider::Gitlab, "2", "Inactive");
        store
            .replace_issues("active", std::slice::from_ref(&active_issue))
            .await
            .unwrap();
        store
            .replace_issues("inactive", std::slice::from_ref(&inactive_issue))
            .await
            .unwrap();
        store
            .set_source_checkpoint("active", &json!({"cursor": "keep"}))
            .await
            .unwrap();
        store
            .set_source_checkpoint("inactive", &json!({"cursor": "remove"}))
            .await
            .unwrap();
        store
            .set_source_checkpoint("checkpoint-only", &json!({"cursor": "remove"}))
            .await
            .unwrap();

        store
            .prune_inactive_sources(&["active".to_owned()])
            .await
            .unwrap();

        assert_eq!(store.load_issues().await.unwrap(), vec![active_issue]);
        assert_eq!(
            store.source_checkpoint("active").await.unwrap(),
            Some(json!({"cursor": "keep"}))
        );
        assert_eq!(store.source_checkpoint("inactive").await.unwrap(), None);
        assert_eq!(
            store.source_checkpoint("checkpoint-only").await.unwrap(),
            None
        );

        store.prune_inactive_sources(&[]).await.unwrap();
        assert!(store.load_issues().await.unwrap().is_empty());
        assert_eq!(store.source_checkpoint("active").await.unwrap(), None);
    }

    #[tokio::test]
    async fn runs_insert_update_and_load_deterministically() {
        let store = Store::in_memory().await.unwrap();
        let older = run("run-b", timestamp(1_700_000_100));
        let newer = run("run-a", timestamp(1_700_000_200));
        store.insert_run(&older).await.unwrap();
        store.insert_run(&newer).await.unwrap();

        let mut updated = older.clone();
        updated.state = RunState::NeedsInput;
        updated.message = Some("Approval required".to_string());
        updated.workspace = None;
        updated.updated_at = newer.updated_at + Duration::seconds(1);
        store.update_run(&updated).await.unwrap();

        assert_eq!(
            store.load_run("run-b").await.unwrap(),
            Some(updated.clone())
        );
        assert_eq!(store.load_runs().await.unwrap(), vec![updated, newer]);

        let missing = run("missing", timestamp(1_700_000_300));
        assert!(matches!(
            store.update_run(&missing).await,
            Err(StoreError::RunNotFound(id)) if id == "missing"
        ));
    }

    #[tokio::test]
    async fn deleting_a_run_also_deletes_its_events() {
        let store = Store::in_memory().await.unwrap();
        let run = run("run-delete", timestamp(1_700_000_100));
        store.insert_run(&run).await.unwrap();
        store
            .append_event(&EventEnvelope {
                run_id: run.id.clone(),
                sequence: 0,
                timestamp: run.updated_at,
                payload: RunEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "temporary output".to_owned(),
                },
            })
            .await
            .unwrap();

        store.delete_run(&run.id).await.unwrap();

        assert_eq!(store.load_run(&run.id).await.unwrap(), None);
        assert!(store.load_events(&run.id).await.unwrap().is_empty());
        assert!(matches!(
            store.delete_run(&run.id).await,
            Err(StoreError::RunNotFound(id)) if id == "run-delete"
        ));
    }

    #[tokio::test]
    async fn run_transition_and_events_commit_together() {
        let store = Store::in_memory().await.unwrap();
        let mut updated = run("run-atomic", timestamp(1_700_000_100));
        store.insert_run(&updated).await.unwrap();
        updated.state = RunState::NeedsInput;
        updated.message = Some("Choose an option".to_owned());
        updated.updated_at = timestamp(1_700_000_200);
        let event = EventEnvelope {
            run_id: updated.id.clone(),
            sequence: 0,
            timestamp: updated.updated_at,
            payload: RunEvent::StateChanged {
                state: updated.state,
                message: updated.message.clone(),
            },
        };

        store
            .update_run_with_events(&updated, std::slice::from_ref(&event))
            .await
            .unwrap();

        assert_eq!(store.load_run(&updated.id).await.unwrap(), Some(updated));
        assert_eq!(store.load_events(&event.run_id).await.unwrap(), vec![event]);
    }

    #[tokio::test]
    async fn events_are_unique_and_loaded_in_sequence_order() {
        let store = Store::in_memory().await.unwrap();
        let events = [
            EventEnvelope {
                run_id: "run-1".to_string(),
                sequence: 2,
                timestamp: timestamp(1_700_000_002),
                payload: RunEvent::Completed {
                    success: true,
                    message: None,
                },
            },
            EventEnvelope {
                run_id: "run-1".to_string(),
                sequence: 0,
                timestamp: timestamp(1_700_000_000),
                payload: RunEvent::StateChanged {
                    state: RunState::Running,
                    message: Some("started".to_string()),
                },
            },
            EventEnvelope {
                run_id: "run-1".to_string(),
                sequence: 1,
                timestamp: timestamp(1_700_000_001),
                payload: RunEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "hello\n".to_string(),
                },
            },
        ];
        for event in &events {
            store.append_event(event).await.unwrap();
        }

        let loaded = store.load_events("run-1").await.unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(loaded, vec![
            events[1].clone(),
            events[2].clone(),
            events[0].clone()
        ]);
        assert!(matches!(
            store.append_event(&events[0]).await,
            Err(StoreError::Database(_))
        ));
    }

    #[tokio::test]
    async fn recent_events_retain_the_newest_window_in_ascending_order() {
        let store = Store::in_memory().await.unwrap();
        for sequence in [3, 0, 4, 1, 2] {
            store
                .append_event(&EventEnvelope {
                    run_id: "run-1".to_string(),
                    sequence,
                    timestamp: timestamp(1_700_000_000 + sequence as i64),
                    payload: RunEvent::Output {
                        stream: OutputStream::Stdout,
                        text: sequence.to_string(),
                    },
                })
                .await
                .unwrap();
        }
        store
            .append_event(&EventEnvelope {
                run_id: "run-2".to_string(),
                sequence: 99,
                timestamp: timestamp(1_700_000_099),
                payload: RunEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "other run".to_string(),
                },
            })
            .await
            .unwrap();

        let recent = store.load_recent_events("run-1", 3).await.unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(
            store
                .load_recent_events("run-1", 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .load_recent_events("missing", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
