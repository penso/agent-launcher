use std::{collections::HashMap, path::PathBuf};

use agent_launcher_core::{Issue, IssueKey, IssueProvider, Repository};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

use crate::{Error, IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

pub struct BeadsSource {
    key: SourceKey,
    root: PathBuf,
}

impl BeadsSource {
    pub fn new(repository: &Repository) -> Self {
        Self {
            key: SourceKey {
                provider: IssueProvider::Beads,
                host: "local".to_owned(),
                repository: repository.root.to_string_lossy().into_owned(),
            },
            root: repository.root.clone(),
        }
    }
}

#[async_trait]
impl IssueSource for BeadsSource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    async fn sync(&self, checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error> {
        let safety_flags = bd_safety_flags(&self.root).await?;
        let list_output = run_bd(&self.root, &safety_flags, &[
            "list", "--json", "--all", "--limit", "0",
        ])
        .await?;
        let blocked_output = run_bd(&self.root, &safety_flags, &["blocked", "--json"]).await?;

        let records: BeadsOutput =
            serde_json::from_slice(&list_output).map_err(|error| Error::Json {
                source: "Beads",
                error,
            })?;
        let blockers: BeadsBlockedOutput =
            serde_json::from_slice(&blocked_output).map_err(|error| Error::Json {
                source: "Beads blockers",
                error,
            })?;
        let mut blockers: HashMap<String, Vec<String>> = blockers
            .into_records()
            .into_iter()
            .map(|record| {
                (
                    record.id,
                    record
                        .blocked_by
                        .into_iter()
                        .map(BeadsReference::into_id)
                        .collect(),
                )
            })
            .collect();
        let mut updated_at = checkpoint.and_then(|value| value.updated_at);
        let issues = records
            .into_records()
            .into_iter()
            .map(|record| {
                updated_at = newest(updated_at, record.updated_at);
                let blocked_by = blockers.remove(&record.id).unwrap_or_default();
                record.into_issue(&self.key, blocked_by)
            })
            .collect();

        Ok(SyncResult {
            issues,
            checkpoint: SyncCheckpoint {
                updated_at,
                etag: None,
                last_full_at: Some(Utc::now()),
                ..SyncCheckpoint::default()
            },
            mode: SyncMode::Full,
        })
    }
}

async fn bd_safety_flags(root: &PathBuf) -> Result<Vec<&'static str>, Error> {
    let output = Command::new("bd")
        .arg("--help")
        .current_dir(root)
        .output()
        .await
        .map_err(|source| Error::CommandIo {
            program: "bd",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "bd",
            cwd: root.clone(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(safety_flags_from_help(&help))
}

fn safety_flags_from_help(help: &str) -> Vec<&'static str> {
    let words: Vec<_> = help.split_whitespace().collect();
    ["--readonly", "--sandbox"]
        .into_iter()
        .filter(|flag| words.contains(flag))
        .collect()
}

async fn run_bd(root: &PathBuf, safety_flags: &[&str], args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = Command::new("bd")
        .args(safety_flags)
        .args(args)
        .current_dir(root)
        .output()
        .await
        .map_err(|source| Error::CommandIo {
            program: "bd",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "bd",
            cwd: root.clone(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output.stdout)
}

fn newest(
    current: Option<DateTime<Utc>>,
    candidate: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (current, candidate) => current.or(candidate),
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BeadsOutput {
    List(Vec<BeadsIssue>),
    Wrapped { issues: Vec<BeadsIssue> },
}

impl BeadsOutput {
    fn into_records(self) -> Vec<BeadsIssue> {
        match self {
            Self::List(issues) | Self::Wrapped { issues } => issues,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BeadsBlockedOutput {
    List(Vec<BeadsBlockedIssue>),
    Wrapped { issues: Vec<BeadsBlockedIssue> },
}

impl BeadsBlockedOutput {
    fn into_records(self) -> Vec<BeadsBlockedIssue> {
        match self {
            Self::List(issues) | Self::Wrapped { issues } => issues,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BeadsBlockedIssue {
    id: String,
    #[serde(default)]
    blocked_by: Vec<BeadsReference>,
}

#[derive(Debug, Deserialize)]
struct BeadsIssue {
    id: String,
    title: String,
    description: Option<String>,
    status: String,
    priority: Option<i64>,
    created_by: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(alias = "parent")]
    parent_id: Option<String>,
    #[serde(default)]
    dependencies: Vec<BeadsDependency>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl BeadsIssue {
    fn into_issue(self, source: &SourceKey, mut blocked_by: Vec<String>) -> Issue {
        let mut parent_id = self.parent_id;
        for dependency in self.dependencies {
            match dependency.kind.as_deref() {
                Some("parent") | Some("parent-child") => {
                    parent_id.get_or_insert(dependency.depends_on_id);
                },
                _ => {},
            }
        }
        blocked_by.sort();
        blocked_by.dedup();

        Issue {
            pull_request: None,
            key: IssueKey {
                provider: source.provider,
                host: source.host.clone(),
                repository: source.repository.clone(),
                native_id: self.id.clone(),
            },
            identifier: self.id,
            title: self.title,
            description: self.description,
            state: self.status,
            url: None,
            author: self.created_by,
            labels: self.labels,
            parent_id,
            blocked_by,
            priority: self.priority,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BeadsReference {
    Id(String),
    Object {
        #[serde(alias = "depends_on_id", alias = "issue_id")]
        id: String,
    },
}

impl BeadsReference {
    fn into_id(self) -> String {
        match self {
            Self::Id(id) | Self::Object { id } => id,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BeadsDependency {
    depends_on_id: String,
    #[serde(rename = "type", alias = "dependency_type")]
    kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;

    use super::{BeadsBlockedOutput, BeadsOutput, safety_flags_from_help};
    use crate::SourceKey;

    #[test]
    fn converts_beads_issue_and_relationships() {
        let output: BeadsOutput = serde_json::from_value(serde_json::json!([{
            "id": "app-12",
            "title": "Implement sources",
            "description": "Wire all providers",
            "status": "in_progress",
            "priority": 1,
            "assignee": "dev",
            "created_by": "creator",
            "labels": ["backend"],
            "dependencies": [
                { "depends_on_id": "app-10", "type": "blocks" },
                { "depends_on_id": "app-1", "dependency_type": "parent-child" }
            ],
            "created_at": "2026-03-01T10:00:00Z",
            "updated_at": "2026-03-02T11:00:00Z"
        }]))
        .expect("valid Beads output");
        let blockers: BeadsBlockedOutput = serde_json::from_value(serde_json::json!([{
            "id": "app-12",
            "blocked_by": ["app-10", { "id": "app-11" }]
        }]))
        .expect("valid Beads blocked output");
        let record = output.into_records().pop().expect("one Beads issue");
        let blocked_by = blockers
            .into_records()
            .pop()
            .expect("one blocked issue")
            .blocked_by
            .into_iter()
            .map(super::BeadsReference::into_id)
            .collect();
        let issue = record.into_issue(
            &SourceKey {
                provider: IssueProvider::Beads,
                host: "local".to_owned(),
                repository: "/work/app".to_owned(),
            },
            blocked_by,
        );

        assert_eq!(issue.identifier, "app-12");
        assert_eq!(issue.parent_id.as_deref(), Some("app-1"));
        assert_eq!(issue.blocked_by, ["app-10", "app-11"]);
        assert_eq!(issue.priority, Some(1));
        assert_eq!(issue.author.as_deref(), Some("creator"));
    }

    #[test]
    fn accepts_wrapped_beads_output() {
        let output: BeadsOutput = serde_json::from_value(serde_json::json!({ "issues": [] }))
            .expect("valid wrapped output");
        assert!(output.into_records().is_empty());
    }

    #[test]
    fn only_uses_safety_flags_advertised_by_help() {
        assert_eq!(
            safety_flags_from_help("  --readonly read only\n  --sandbox no sync"),
            ["--readonly", "--sandbox"]
        );
        assert_eq!(
            safety_flags_from_help("mentions --readonly=maybe but has no exact flag"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn resolved_dependencies_are_not_reported_as_active_blockers() {
        let output: BeadsOutput = serde_json::from_value(serde_json::json!([{
            "id": "app-12",
            "title": "Implement sources",
            "status": "open",
            "dependencies": [
                { "depends_on_id": "app-closed", "type": "blocks" }
            ]
        }]))
        .expect("valid Beads output");
        let record = output.into_records().pop().expect("one Beads issue");
        let issue = record.into_issue(
            &SourceKey {
                provider: IssueProvider::Beads,
                host: "local".to_owned(),
                repository: "/work/app".to_owned(),
            },
            Vec::new(),
        );

        assert!(issue.blocked_by.is_empty());
    }
}
