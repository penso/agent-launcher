use serde::{Deserialize, Serialize};

/// Minimal repository-advisory metadata; unused collaborator and token fields are discarded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SecurityAdvisoryMetadata {
    pub ghsa_id: String,
    pub cve_id: Option<String>,
    pub severity: Option<String>,
}

/// An advisory-linked fork verified against GitHub immediately before preparation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivateAdvisoryFork {
    pub id: u64,
    pub host: String,
    pub full_name: String,
    pub default_branch: String,
}

#[derive(Clone, Debug)]
pub struct SecurityPreparation {
    pub issue: crate::Issue,
    pub fork: PrivateAdvisoryFork,
}
