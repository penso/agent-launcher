use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueProvider {
    Github,
    Gitlab,
    Beads,
}

impl IssueProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Beads => "beads",
        }
    }
}

impl fmt::Display for IssueProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for IssueProvider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            "beads" => Ok(Self::Beads),
            _ => Err(format!("unknown issue provider: {value}")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct IssueKey {
    pub provider: IssueProvider,
    pub host: String,
    pub repository: String,
    pub native_id: String,
}

impl IssueKey {
    pub fn canonical(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.provider,
            canonical_component(&self.host),
            canonical_component(&self.repository),
            canonical_component(&self.native_id)
        )
    }
}

fn canonical_component(value: &str) -> String {
    value.replace('%', "%25").replace(':', "%3A")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequestMetadata {
    pub number: u64,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub base_ref: String,
    pub head_ref: String,
    pub base_sha: String,
    pub head_sha: String,
    /// Repository containing the head branch, in `owner/name` form when available.
    pub head_repository: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Issue {
    pub key: IssueKey,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub state: String,
    #[serde(default)]
    pub pull_request: Option<PullRequestMetadata>,
    pub url: Option<String>,
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub parent_id: Option<String>,
    pub blocked_by: Vec<String>,
    pub priority: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    pub fn is_blocked(&self) -> bool {
        !self.blocked_by.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{IssueKey, IssueProvider};

    #[test]
    fn canonical_issue_keys_are_injective_across_components() {
        let left = IssueKey {
            provider: IssueProvider::Github,
            host: "github.com".to_owned(),
            repository: "acme:widgets".to_owned(),
            native_id: "1".to_owned(),
        };
        let right = IssueKey {
            provider: IssueProvider::Github,
            host: "github.com".to_owned(),
            repository: "acme".to_owned(),
            native_id: "widgets:1".to_owned(),
        };

        assert_ne!(left.canonical(), right.canonical());
        assert_eq!(left.canonical(), "github:github.com:acme%3Awidgets:1");
    }

    #[test]
    fn canonical_issue_keys_escape_the_escape_marker() {
        let escaped = IssueKey {
            provider: IssueProvider::Gitlab,
            host: "gitlab.com".to_owned(),
            repository: "acme%3Awidgets".to_owned(),
            native_id: "1".to_owned(),
        };
        let colon = IssueKey {
            repository: "acme:widgets".to_owned(),
            ..escaped.clone()
        };

        assert_ne!(escaped.canonical(), colon.canonical());
        assert!(escaped.canonical().contains("acme%253Awidgets"));
    }
}
