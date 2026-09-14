use agent_launcher_core::{Issue, IssueKey, IssueProvider, SecurityPreparation};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SourceKey {
    pub provider: IssueProvider,
    pub host: String,
    pub repository: String,
}

impl SourceKey {
    pub fn canonical(&self) -> String {
        format!(
            "{}:{}:{}",
            self.provider,
            canonical_component(&self.host),
            canonical_component(&self.repository)
        )
    }
}

fn canonical_component(value: &str) -> String {
    value.replace('%', "%25").replace(':', "%3A")
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncCheckpoint {
    pub updated_at: Option<DateTime<Utc>>,
    /// Source-owned opaque validators. Security inventories encode a versioned page manifest.
    pub etag: Option<String>,
    /// For security sources, UTC completion of the last fully validated live inventory.
    /// A local freshness-cache hit preserves this timestamp unchanged.
    pub last_full_at: Option<DateTime<Utc>>,
    /// GitHub detail revision markers; records themselves remain in the issue cache.
    #[serde(default)]
    pub pr_details: std::collections::HashMap<String, String>,
    /// Last attempted PR number, including failed optional requests.
    #[serde(default)]
    pub pr_cursor: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncResult {
    pub issues: Vec<Issue>,
    pub checkpoint: SyncCheckpoint,
    pub mode: SyncMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Full,
    Delta,
    NotModified,
}

#[async_trait]
pub trait IssueSource: Send + Sync {
    fn source_key(&self) -> &SourceKey;

    fn is_confidential(&self) -> bool {
        false
    }

    fn cache_key(&self) -> String {
        self.source_key().canonical()
    }

    /// Refetch and verify a private dispatch target, only after explicit confirmation.
    async fn prepare_security(
        &self,
        _key: &IssueKey,
        _create_fork: bool,
    ) -> Result<SecurityPreparation, Error> {
        Err(Error::SecurityUnsupported)
    }

    fn supports_delete(&self) -> bool {
        false
    }

    /// Permanently deletes one issue. Call only after explicit user confirmation.
    async fn delete_issue(&self, _issue: &IssueKey) -> Result<(), Error> {
        Err(Error::DeleteUnsupported)
    }

    async fn sync(&self, checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error>;

    /// The runtime supplies this source's persisted records, including closed items.
    /// Confidential sources require a private cache, atomically paired with its checkpoint.
    /// Security sources return Full even on local reuse; unchanged last_full_at means
    /// no live observation. Local reuse may defer even manual refresh for five minutes.
    /// Call sync(None) to force live validation, but never bypass retry_at().
    async fn sync_with_cache(
        &self,
        checkpoint: Option<&SyncCheckpoint>,
        _cached: &[Issue],
    ) -> Result<SyncResult, Error> {
        self.sync(checkpoint).await
    }

    /// An active source-wide deadline. Manual refreshes must not bypass it.
    fn retry_at(&self) -> Option<DateTime<Utc>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;

    use super::SourceKey;

    #[test]
    fn canonical_source_keys_are_injective_across_components() {
        let left = SourceKey {
            provider: IssueProvider::Gitlab,
            host: "gitlab.example.com:8443".to_owned(),
            repository: "group/app".to_owned(),
        };
        let right = SourceKey {
            provider: IssueProvider::Gitlab,
            host: "gitlab.example.com".to_owned(),
            repository: "8443:group/app".to_owned(),
        };

        assert_ne!(left.canonical(), right.canonical());
        assert_eq!(
            left.canonical(),
            "gitlab:gitlab.example.com%3A8443:group/app"
        );
    }
}
