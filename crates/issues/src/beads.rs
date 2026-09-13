use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_launcher_core::{Issue, IssueKey, IssueProvider, Repository};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::process::Command;

use crate::{Error, IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

pub struct BeadsSource {
    key: SourceKey,
    root: PathBuf,
    program: PathBuf,
    command_timeout: Duration,
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
            program: PathBuf::from("bd"),
            command_timeout: Duration::from_secs(15),
        }
    }
}

#[async_trait]
impl IssueSource for BeadsSource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    fn supports_delete(&self) -> bool {
        true
    }

    async fn delete_issue(&self, issue: &IssueKey) -> Result<(), Error> {
        if issue.provider != self.key.provider
            || issue.host != self.key.host
            || issue.repository != self.key.repository
            || issue.native_id.is_empty()
            || issue.native_id.len() > 255
            || !issue.native_id.as_bytes()[0].is_ascii_alphanumeric()
            || !issue
                .native_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::InvalidDeleteTarget);
        }
        let mut flags = bd_safety_flags(&self.program, &self.root, self.command_timeout).await?;
        flags.retain(|flag| *flag != "--readonly");
        run_bd(&self.program, &self.root, self.command_timeout, &flags, &[
            "delete",
            "--force",
            "--",
            &issue.native_id,
        ])
        .await?;
        Ok(())
    }

    async fn sync(&self, checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error> {
        let safety_flags = bd_safety_flags(&self.program, &self.root, self.command_timeout).await?;
        let list_output = run_bd(
            &self.program,
            &self.root,
            self.command_timeout,
            &safety_flags,
            &["list", "--json", "--all", "--limit", "0"],
        )
        .await?;
        let blocked_output = run_bd(
            &self.program,
            &self.root,
            self.command_timeout,
            &safety_flags,
            &["blocked", "--json"],
        )
        .await?;

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

async fn bd_safety_flags(
    program: &Path,
    root: &PathBuf,
    deadline: Duration,
) -> Result<Vec<&'static str>, Error> {
    let output = tokio::time::timeout(
        deadline,
        Command::new(program)
            .arg("--help")
            .current_dir(root)
            .env("BEADS_DIR", root.join(".beads"))
            .env_remove("BEADS_DB")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| Error::CommandTimeout)?
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

async fn run_bd(
    program: &Path,
    root: &PathBuf,
    deadline: Duration,
    safety_flags: &[&str],
    args: &[&str],
) -> Result<Vec<u8>, Error> {
    let output = tokio::time::timeout(
        deadline,
        Command::new(program)
            .args(safety_flags)
            .args(args)
            .current_dir(root)
            // Pin routing without bypassing repository-managed .beads/redirect files.
            .env("BEADS_DIR", root.join(".beads"))
            .env_remove("BEADS_DB")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| Error::CommandTimeout)?
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
            activity: None,
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
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use agent_launcher_core::IssueProvider;

    use super::*;
    use crate::SourceKey;

    struct FakeBd(BeadsSource);

    impl FakeBd {
        fn new(body: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "launcher-bd-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let program = root.join("fake-bd");
            fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
            Self(BeadsSource {
                key: SourceKey {
                    provider: IssueProvider::Beads,
                    host: "local".into(),
                    repository: root.to_string_lossy().into_owned(),
                },
                root,
                program,
                command_timeout: Duration::from_secs(2),
            })
        }

        fn key(&self, id: &str) -> IssueKey {
            IssueKey {
                provider: self.0.key.provider,
                host: self.0.key.host.clone(),
                repository: self.0.key.repository.clone(),
                native_id: id.into(),
            }
        }
    }

    impl Drop for FakeBd {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0.root).unwrap();
        }
    }

    #[tokio::test]
    async fn sync_and_delete_override_inherited_store_routing() {
        const CHILD: &str = "AGENT_LAUNCHER_BEADS_ROUTING_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Poison only an isolated test process, never the parallel test runner.
            let output = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "beads::tests::sync_and_delete_override_inherited_store_routing",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("BEADS_DIR", "/other-repository/.beads")
                    .env("BEADS_DB", "/other-repository/.beads/beads.db")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        assert_eq!(
            std::env::var("BEADS_DIR").unwrap(),
            "/other-repository/.beads"
        );
        assert_eq!(
            std::env::var("BEADS_DB").unwrap(),
            "/other-repository/.beads/beads.db"
        );
        let fake = FakeBd::new(
            r#"
printf '%s\n' "$BEADS_DIR" "${BEADS_DB+present}" "$*" >> routing
pwd >> directories
case "$*" in
    --help) printf '%s\n' '--readonly --sandbox' ;;
    '--readonly --sandbox list --json --all --limit 0') printf '%s\n' '[{"id":"app-1","title":"Scoped issue","status":"open"}]' ;;
    '--readonly --sandbox blocked --json') printf '%s\n' '[]' ;;
    '--sandbox delete --force -- app-1') ;;
    *) exit 9 ;;
esac
"#,
        );
        let result = fake.0.sync(None).await.unwrap();
        assert_eq!(result.issues.len(), 1);
        assert_eq!(result.issues[0].key, fake.key("app-1"));
        fake.0.delete_issue(&result.issues[0].key).await.unwrap();
        let beads_dir = fake.0.root.join(".beads");
        let expected = [
            "--help",
            "--readonly --sandbox list --json --all --limit 0",
            "--readonly --sandbox blocked --json",
            "--help",
            "--sandbox delete --force -- app-1",
        ]
        .map(|args| format!("{}\n\n{args}\n", beads_dir.display()))
        .concat();
        assert_eq!(
            fs::read_to_string(fake.0.root.join("routing")).unwrap(),
            expected
        );
        let directories = fs::read_to_string(fake.0.root.join("directories")).unwrap();
        assert_eq!(directories.lines().count(), 5);
        for directory in directories.lines() {
            assert_eq!(
                fs::canonicalize(directory).unwrap(),
                fs::canonicalize(&fake.0.root).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn delete_uses_exact_safe_arguments_and_repository_cwd() {
        for help in ["--readonly --sandbox", "--readonly", "no optional flags"] {
            let fake = FakeBd::new(&format!(
                r#"
if [ "$1" = --help ]; then printf '%s\n' '{help}'; exit 0; fi
pwd > cwd
printf '%s\n' "$@" > args
"#
            ));
            assert!(fake.0.supports_delete());
            fake.0.delete_issue(&fake.key("app-12.3")).await.unwrap();
            let prefix = if help.contains("--sandbox") {
                "--sandbox\n"
            } else {
                ""
            };
            assert_eq!(
                fs::read_to_string(fake.0.root.join("args")).unwrap(),
                format!("{prefix}delete\n--force\n--\napp-12.3\n")
            );
            assert_eq!(
                fs::canonicalize(fs::read_to_string(fake.0.root.join("cwd")).unwrap().trim())
                    .unwrap(),
                fs::canonicalize(&fake.0.root).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn delete_rejects_invalid_ids_and_scope_before_invoking_cli() {
        let fake = FakeBd::new("touch invoked; exit 1");
        for id in [
            "",
            "--all",
            "-x",
            "a b",
            "a\nb",
            "a/b",
            "a,b",
            "a;id",
            "a\0b",
            "@file",
            ".",
            "..",
            &"a".repeat(256),
        ] {
            assert!(matches!(
                fake.0.delete_issue(&fake.key(id)).await,
                Err(Error::InvalidDeleteTarget)
            ));
        }
        for field in 0..3 {
            let mut key = fake.key("app-1");
            match field {
                0 => key.provider = IssueProvider::Github,
                1 => key.host = "other".into(),
                _ => key.repository = "other".into(),
            }
            assert!(matches!(
                fake.0.delete_issue(&key).await,
                Err(Error::InvalidDeleteTarget)
            ));
        }
        assert!(!fake.0.root.join("invoked").exists());
    }

    #[tokio::test]
    async fn delete_reports_help_mutation_failures_and_deadlines() {
        for body in ["exit 7", "if [ \"$1\" = --help ]; then exit 0; fi; exit 8"] {
            let fake = FakeBd::new(body);
            assert!(matches!(
                fake.0.delete_issue(&fake.key("app-1")).await,
                Err(Error::CommandFailed { .. })
            ));
        }
        for body in [
            "exec sleep 10",
            "if [ \"$1\" = --help ]; then exit 0; fi; exec sleep 10",
        ] {
            let mut fake = FakeBd::new(body);
            fake.0.command_timeout = Duration::from_millis(50);
            assert!(matches!(
                fake.0.delete_issue(&fake.key("app-1")).await,
                Err(Error::CommandTimeout)
            ));
        }
    }

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
        assert_eq!(issue.activity, None);
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
