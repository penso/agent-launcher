use std::{ffi::OsString, path::PathBuf, sync::Arc};

use agent_launcher_core::{
    BackendKind, Repository, RunState, RunSummary, WorkspaceRef, WorktreeInspection,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    Backend, BackendCapabilities, BackendDetection, Capability, DispatchRequest, DispatchResult,
    Error, OpenResult, Result, SessionRegistry, StatusResult,
    command::{contains_string, find_string, open_uri, run_json},
    registry::{BackendSession, RunRecord},
    sanitize_branch, sanitize_workspace_name,
};

#[derive(Clone, Debug)]
pub struct SupersetConfig {
    pub executable: PathBuf,
    pub host: Option<String>,
    pub required_agent: Option<String>,
}

impl Default for SupersetConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("superset"),
            host: None,
            required_agent: None,
        }
    }
}

pub struct SupersetBackend {
    config: SupersetConfig,
    registry: Arc<SessionRegistry>,
}

impl SupersetBackend {
    pub fn new(config: SupersetConfig, registry: Arc<SessionRegistry>) -> Self {
        Self { config, registry }
    }

    async fn command(&self, args: Vec<OsString>) -> Result<Value> {
        let args = json_command_args(args);
        tracing::debug!("running Superset CLI");
        run_json(&self.config.executable, &args, None).await
    }

    fn target_args(&self, require_explicit_local: bool) -> Vec<OsString> {
        target_args(&self.config, require_explicit_local)
    }

    async fn projects(&self) -> Result<Value> {
        self.command(projects_list_args()).await
    }

    async fn agents(&self) -> Result<Value> {
        self.command(agents_list_args(&self.config)).await
    }

    async fn find_project(&self, repository: &Repository) -> Result<ProjectMatch> {
        let response = self.projects().await?;
        let projects = response
            .as_array()
            .or_else(|| response.get("projects").and_then(Value::as_array))
            .or_else(|| response.get("data").and_then(Value::as_array))
            .ok_or_else(|| Error::InvalidResponse("projects list was not an array".into()))?;

        let local_root = if self.config.host.is_none() {
            tokio::fs::canonicalize(&repository.root).await.ok()
        } else {
            None
        };
        let expected_remote = repository
            .remote
            .as_ref()
            .map(|remote| normalize_remote(&remote.url));

        for project in projects {
            let Some(id) = find_string(project, &["id", "projectId", "project_id"]) else {
                continue;
            };
            let path = find_string(project, &[
                "path",
                "root",
                "projectPath",
                "repositoryPath",
                "worktreePath",
            ])
            .map(PathBuf::from);
            let remote = find_string(project, &[
                "repoCloneUrl",
                "repo",
                "repositoryUrl",
                "repoUrl",
                "gitUrl",
                "remoteUrl",
            ]);

            let path_matches = match (&local_root, &path) {
                (Some(expected), Some(candidate)) => tokio::fs::canonicalize(candidate)
                    .await
                    .is_ok_and(|candidate| candidate == *expected),
                _ => false,
            };
            let remote_matches = expected_remote.as_ref().is_some_and(|expected| {
                remote
                    .as_ref()
                    .is_some_and(|candidate| normalize_remote(candidate) == *expected)
            });
            if path_matches || remote_matches {
                return Ok(ProjectMatch { id });
            }
        }
        Err(Error::ProjectNotFound(repository.root.clone()))
    }

    async fn require_agent(&self) -> Result<()> {
        let Some(required_agent) = self.config.required_agent.as_deref() else {
            return Ok(());
        };
        let response = self.agents().await?;
        if agent_available(&response, required_agent)? {
            Ok(())
        } else {
            Err(Error::SupersetAgentUnavailable(required_agent.to_string()))
        }
    }

    async fn record(&self, run_id: &str) -> Result<RunRecord> {
        let record = self.registry.get(run_id).await?;
        if !matches!(record.session, BackendSession::Superset { .. }) {
            return Err(Error::RunNotFound(run_id.to_string()));
        }
        Ok(record)
    }
}

#[async_trait]
impl Backend for SupersetBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Superset
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new([
            Capability::Detect,
            Capability::Dispatch,
            Capability::Open,
            Capability::DeleteWorktree,
            Capability::Remote,
        ])
    }

    async fn owns_run(&self, run_id: &str) -> bool {
        self.registry
            .get(run_id)
            .await
            .is_ok_and(|record| matches!(record.session, BackendSession::Superset { .. }))
    }

    async fn detect(&self, repository: &Repository) -> Result<BackendDetection> {
        let result = async {
            self.find_project(repository).await?;
            self.require_agent().await
        }
        .await;
        match result {
            Ok(_) => Ok(BackendDetection {
                backend: self.kind(),
                available: true,
                manager_running: true,
                capabilities: self.capabilities(),
                message: None,
                compute_targets: Vec::new(),
            }),
            Err(error) => Ok(BackendDetection {
                backend: self.kind(),
                available: false,
                manager_running: false,
                capabilities: self.capabilities(),
                message: Some(error.to_string()),
                compute_targets: Vec::new(),
            }),
        }
    }

    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult> {
        request.validate()?;
        crate::private::guard_dispatch(&request, self.kind()).await?;
        if request.model.is_some() {
            return Err(Error::InvalidRequest(
                "Superset does not support model selection; use the harness default".into(),
            ));
        }
        let project = self.find_project(&request.repository).await?;
        let hash = issue_hash(&request.issue.key.canonical());
        let workspace_name = request.workspace_name.clone().unwrap_or_else(|| {
            sanitize_workspace_name(&format!(
                "{}-{}",
                request.issue.identifier, request.issue.title
            ))
        });
        let branch = sanitize_branch(
            request
                .branch
                .as_deref()
                .unwrap_or(&format!("agent/{workspace_name}-{}", &hash[..8])),
        );

        let mut workspace_args = vec![
            "workspaces".into(),
            "create".into(),
            "--project".into(),
            project.id.into(),
            "--name".into(),
            workspace_name.into(),
            "--branch".into(),
            branch.clone().into(),
            "--agent".into(),
            request.agent.clone().into(),
            "--prompt".into(),
            request.prompt.into(),
        ];
        if let Some(base_branch) = &request.base_branch {
            workspace_args.push("--base-branch".into());
            workspace_args.push(base_branch.into());
        }
        workspace_args.extend(self.target_args(true));
        let response = self.command(workspace_args).await?;
        let created = parse_workspace_created(&response)?;
        let workspace_id = created.workspace_id;
        let worktree_path = created.worktree_path;
        let session_id = created.session_id;
        let session_kind = created.session_kind;

        let now = Utc::now();
        let run_id = Uuid::new_v4().to_string();
        let summary = RunSummary {
            confidential: false,
            id: run_id,
            issue_key: request.issue.key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Superset,
                id: workspace_id.clone(),
                host: self.config.host.clone(),
                path: worktree_path,
                branch,
            }),
            agent: request.agent,
            model: None,
            state: RunState::Running,
            message: None,
            session_id: Some(session_id.clone()),
            started_at: now,
            updated_at: now,
        };
        self.registry
            .insert(RunRecord {
                summary: summary.clone(),
                session: BackendSession::Superset {
                    workspace_id,
                    session_id,
                    session_kind: session_kind.clone(),
                    host: self.config.host.clone(),
                },
                deletion: None,
            })
            .await?;

        Ok(DispatchResult {
            run: summary,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        self.record(run_id).await?;
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::Refresh,
        })
    }

    async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        let _ = text;
        self.record(run_id).await?;
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::SendInput,
        })
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        self.record(run_id).await?;
        Err(Error::UnsupportedCapability {
            backend: self.kind(),
            capability: Capability::Stop,
        })
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Superset {
            workspace_id,
            session_id,
            session_kind,
            ..
        } = record.session
        else {
            unreachable!()
        };
        let mut uri = Url::parse(&format!("superset://v2-workspace/{workspace_id}"))
            .map_err(|error| Error::InvalidResponse(error.to_string()))?;
        uri.query_pairs_mut()
            .append_pair(
                if session_kind == "chat" {
                    "chatSessionId"
                } else {
                    "terminalId"
                },
                &session_id,
            )
            .append_pair("focusRequestId", &Uuid::new_v4().to_string());

        open_uri(&uri, self.kind()).await?;
        Ok(OpenResult {
            uri,
            launched: true,
        })
    }

    async fn delete_worktree(
        &self,
        run_id: &str,
        _force: bool,
        _expected: Option<&WorktreeInspection>,
    ) -> Result<()> {
        let record = self.record(run_id).await?;
        let BackendSession::Superset {
            workspace_id, host, ..
        } = record.session
        else {
            unreachable!()
        };
        self.registry.begin_deletion(run_id, true, None).await?;
        if let Err(error) = self
            .command(workspace_delete_args(&workspace_id, host.as_deref()))
            .await
        {
            let removed = self
                .command(json_command_args(workspace_list_args(host.as_deref())))
                .await
                .is_ok_and(|workspaces| !contains_string(&workspaces, &workspace_id));
            if !removed {
                let _ = self.registry.cancel_deletion(run_id).await;
                return Err(error);
            }
        }
        self.registry.complete_deletion(run_id).await?;
        Ok(())
    }

    async fn deletion_pending(&self, run_id: &str) -> bool {
        self.registry.deletion_pending(run_id).await
    }

    async fn finalize_deletion(&self, run_id: &str) -> Result<()> {
        self.registry.finalize_deletion(run_id).await
    }
}

#[derive(Debug)]
struct ProjectMatch {
    id: String,
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceCreated {
    workspace_id: String,
    worktree_path: Option<PathBuf>,
    session_id: String,
    session_kind: String,
}

fn parse_workspace_created(value: &Value) -> Result<WorkspaceCreated> {
    let workspace = value
        .get("workspace")
        .ok_or_else(|| Error::InvalidResponse("workspace response has no workspace".into()))?;
    let workspace_id = workspace
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidResponse("workspace response has no workspace id".into()))?;
    let worktree_path = ["worktreePath", "path"]
        .into_iter()
        .find_map(|key| workspace.get(key).and_then(Value::as_str))
        .map(PathBuf::from);
    let agent = value
        .get("agents")
        .and_then(Value::as_array)
        .and_then(|agents| agents.first())
        .ok_or_else(|| Error::InvalidResponse("workspace response has no agent result".into()))?;
    if agent.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = agent
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("agent launch failed");
        return Err(Error::InvalidResponse(error.into()));
    }
    let session_id = agent
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidResponse("agent result has no session id".into()))?;
    let session_kind = agent
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidResponse("agent result has no kind".into()))?;
    Ok(WorkspaceCreated {
        workspace_id: workspace_id.into(),
        worktree_path,
        session_id: session_id.into(),
        session_kind: session_kind.into(),
    })
}

fn issue_hash(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes())
        .simple()
        .to_string()
}

fn json_command_args(mut args: Vec<OsString>) -> Vec<OsString> {
    args.push("--json".into());
    args
}

fn projects_list_args() -> Vec<OsString> {
    vec!["projects".into(), "list".into()]
}

fn agents_list_args(config: &SupersetConfig) -> Vec<OsString> {
    let mut args = vec!["agents".into(), "list".into()];
    args.extend(target_args(config, true));
    args
}

fn workspace_delete_args(workspace_id: &str, host: Option<&str>) -> Vec<OsString> {
    let mut args = vec!["workspaces".into(), "delete".into(), workspace_id.into()];
    match host {
        Some(host) => args.extend([OsString::from("--host"), host.into()]),
        None => args.push("--local".into()),
    }
    args
}

fn workspace_list_args(host: Option<&str>) -> Vec<OsString> {
    let mut args = vec!["workspaces".into(), "list".into()];
    match host {
        Some(host) => args.extend([OsString::from("--host"), host.into()]),
        None => args.push("--local".into()),
    }
    args
}

fn target_args(config: &SupersetConfig, require_explicit_local: bool) -> Vec<OsString> {
    match &config.host {
        Some(host) => vec!["--host".into(), host.into()],
        None if require_explicit_local => vec!["--local".into()],
        None => Vec::new(),
    }
}

fn agent_available(response: &Value, required_agent: &str) -> Result<bool> {
    let agents = response
        .as_array()
        .or_else(|| response.get("agents").and_then(Value::as_array))
        .or_else(|| response.get("data").and_then(Value::as_array))
        .ok_or_else(|| Error::InvalidResponse("agents list was not an array".into()))?;
    Ok(agents.iter().any(|agent| {
        ["id", "presetId"]
            .into_iter()
            .any(|key| agent.get(key).and_then(Value::as_str) == Some(required_agent))
    }))
}

fn normalize_remote(value: &str) -> String {
    let value = value.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Ok(url) = Url::parse(value) {
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        return format!("{host}/{}", url.path().trim_matches('/')).to_ascii_lowercase();
    }
    let without_user = value.rsplit_once('@').map_or(value, |(_, tail)| tail);
    without_user.replace(':', "/").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_explicit_superset_targets() {
        let local = SupersetConfig::default();
        let remote = SupersetConfig {
            executable: "superset".into(),
            host: Some("host_123".into()),
            required_agent: None,
        };
        assert_eq!(target_args(&local, true), vec![OsString::from("--local")]);
        assert_eq!(target_args(&remote, true), vec![
            OsString::from("--host"),
            OsString::from("host_123")
        ]);
    }

    #[test]
    fn builds_current_list_command_args() {
        let local = SupersetConfig::default();
        let remote = SupersetConfig {
            host: Some("host_123".into()),
            ..SupersetConfig::default()
        };

        assert_eq!(json_command_args(projects_list_args()), [
            OsString::from("projects"),
            OsString::from("list"),
            OsString::from("--json"),
        ]);
        assert_eq!(json_command_args(agents_list_args(&local)), [
            OsString::from("agents"),
            OsString::from("list"),
            OsString::from("--local"),
            OsString::from("--json"),
        ]);
        assert_eq!(json_command_args(agents_list_args(&remote)), [
            OsString::from("agents"),
            OsString::from("list"),
            OsString::from("--host"),
            OsString::from("host_123"),
            OsString::from("--json"),
        ]);
    }

    #[test]
    fn builds_explicit_workspace_delete_targets() {
        assert_eq!(workspace_list_args(None), [
            OsString::from("workspaces"),
            OsString::from("list"),
            OsString::from("--local"),
        ]);
        assert_eq!(workspace_list_args(Some("host_1")), [
            OsString::from("workspaces"),
            OsString::from("list"),
            OsString::from("--host"),
            OsString::from("host_1"),
        ]);
        assert_eq!(workspace_delete_args("workspace_1", None), [
            OsString::from("workspaces"),
            OsString::from("delete"),
            OsString::from("workspace_1"),
            OsString::from("--local"),
        ]);
        assert_eq!(workspace_delete_args("workspace_1", Some("host_1")), [
            OsString::from("workspaces"),
            OsString::from("delete"),
            OsString::from("workspace_1"),
            OsString::from("--host"),
            OsString::from("host_1"),
        ]);
    }

    #[test]
    fn finds_current_agents_by_instance_or_preset_id() {
        let value = serde_json::json!([
            {
                "id": "8e026568-e971-4ec2-99c8-45f3ef0f1626",
                "presetId": "opencode",
                "label": "OpenCode",
                "command": "opencode",
                "args": [],
                "promptTransport": "argv",
                "promptArgs": [],
                "env": {},
                "order": 0
            },
            {
                "id": "superset",
                "presetId": "superset",
                "label": "Superset",
                "command": "(superset runtime)"
            }
        ]);

        assert!(agent_available(&value, "opencode").expect("preset should match"));
        assert!(
            agent_available(&value, "8e026568-e971-4ec2-99c8-45f3ef0f1626")
                .expect("instance id should match")
        );
        assert!(agent_available(&value, "superset").expect("runtime agent should match"));
        assert!(!agent_available(&value, "claude").expect("missing agent should not match"));
    }

    #[test]
    fn parses_current_nested_workspace_and_agent_json() {
        let value = serde_json::json!({
            "workspace": {"id": "ws_1", "worktreePath": "/tmp/ws"},
            "terminals": [],
            "agents": [{"ok": true, "kind": "chat", "sessionId": "ses_1", "label": "Superset"}],
            "alreadyExists": false
        });
        assert_eq!(
            parse_workspace_created(&value).expect("response should parse"),
            WorkspaceCreated {
                workspace_id: "ws_1".into(),
                worktree_path: Some("/tmp/ws".into()),
                session_id: "ses_1".into(),
                session_kind: "chat".into(),
            }
        );

        let failed = serde_json::json!({
            "workspace": {"id": "ws_2"},
            "agents": [{"ok": false, "error": "preset is unavailable"}]
        });
        assert!(
            parse_workspace_created(&failed)
                .expect_err("failed launch must not become a run")
                .to_string()
                .contains("preset is unavailable")
        );
    }

    #[test]
    fn normalizes_common_git_remotes() {
        assert_eq!(
            normalize_remote("git@github.com:Org/Repo.git"),
            normalize_remote("https://github.com/org/repo")
        );
    }
}
