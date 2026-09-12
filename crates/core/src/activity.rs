use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivityCounts {
    pub working: u64,
    pub blocked: u64,
    pub idle: u64,
    pub unseen_done: u64,
    pub unknown: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityCompleteness {
    Complete,
    Partial,
    #[default]
    Missing,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivitySample {
    pub sampled_at: DateTime<Utc>,
    pub counts: Option<ActivityCounts>,
    pub expected_endpoints: usize,
    pub fresh_endpoints: usize,
    pub stale_endpoints: usize,
    pub never_observed_endpoints: usize,
    pub failed_endpoints: usize,
    pub excluded_endpoints: usize,
    pub inventory_complete: bool,
    pub completeness: ActivityCompleteness,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityTransportState {
    #[default]
    Connecting,
    Reachable,
    Failed,
    Disabled,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityFreshness {
    #[default]
    NeverObserved,
    Fresh,
    Stale,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivityEndpointHealth {
    pub endpoint_id: String,
    pub transport: ActivityTransportState,
    pub freshness: ActivityFreshness,
    pub last_success_at: Option<DateTime<Utc>>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HerdrActivitySnapshot {
    pub enabled: bool,
    pub discovering: bool,
    pub discover_remote_sessions: bool,
    pub samples: Vec<ActivitySample>,
    pub endpoints: Vec<ActivityEndpointHealth>,
    pub discovery_error: Option<String>,
    pub persistence_error: Option<String>,
}
