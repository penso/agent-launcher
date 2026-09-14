use std::{sync::Arc, time::Duration};

use agent_launcher_core::{
    BackendKind, Repository, RunState, RunSummary, WorkspaceRef, WorktreeInspection,
};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Map, Value, json};
use url::Url;
use uuid::Uuid;

use crate::{
    Backend, BackendCapabilities, BackendDetection, Capability, DispatchRequest, DispatchResult,
    Error, OpenResult, Result, SessionRegistry, StatusResult,
    command::open_uri,
    registry::{BackendSession, RunRecord},
    sanitize_branch, sanitize_workspace_name,
};

const SUPPORTED_AGENTS: &[&str] = &["claude", "codex", "cursor", "acp"];
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct ConductorConfig {
    /// Base URL for the versioned API, normally `https://api.conductor.build/v0/`.
    pub api_url: Url,
    /// Bearer token override. If absent, `CONDUCTOR_API_TOKEN` is used.
    pub bearer_token: Option<String>,
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            api_url: Url::parse("https://api.conductor.build/v0/")
                .expect("the built-in Conductor API URL is valid"),
            bearer_token: None,
        }
    }
}

pub struct ConductorBackend {
    config: ConductorConfig,
    registry: Arc<SessionRegistry>,
    client: Client,
}

impl ConductorBackend {
    pub fn new(config: ConductorConfig, registry: Arc<SessionRegistry>) -> Self {
        Self {
            config,
            registry,
            client: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .user_agent(format!(
                    "{}/{}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .expect("static Conductor HTTP client configuration is valid"),
        }
    }

    fn token(&self) -> Result<String> {
        self.config
            .bearer_token
            .clone()
            .filter(|token| !token.trim().is_empty())
            .or_else(|| {
                std::env::var("CONDUCTOR_API_KEY")
                    .ok()
                    .filter(|token| !token.trim().is_empty())
            })
            .or_else(|| {
                std::env::var("CONDUCTOR_API_TOKEN")
                    .ok()
                    .filter(|token| !token.trim().is_empty())
            })
            .ok_or(Error::ConductorTokenUnavailable)
    }

    fn authorized(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        let mut request = request.bearer_auth(self.token()?);
        if let Ok(session_id) = std::env::var("CONDUCTOR_SESSION_ID")
            && !session_id.trim().is_empty()
        {
            request = request.header("X-Conductor-Session-Id", session_id);
        }
        Ok(request)
    }

    async fn response_json(&self, response: reqwest::Result<Response>) -> Result<Value> {
        let response = response?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        serde_json::from_str(&body).map_err(Into::into)
    }

    async fn get(&self, url: Url) -> Result<Value> {
        let request = self.authorized(self.client.get(url))?;
        self.response_json(request.send().await).await
    }

    async fn post(&self, url: Url, body: &Value) -> Result<Value> {
        let request = self.authorized(self.client.post(url).json(body))?;
        self.response_json(request.send().await).await
    }

    async fn post_empty(&self, url: Url) -> Result<Value> {
        let request = self.authorized(self.client.post(url))?;
        self.response_json(request.send().await).await
    }

    async fn identity(&self) -> Result<()> {
        let value = self
            .get(root_endpoint(&self.config.api_url, &["me"])?)
            .await?;
        if value.get("userId").and_then(Value::as_str).is_none() {
            return Err(Error::InvalidResponse(
                "Conductor /me response has no userId".into(),
            ));
        }
        Ok(())
    }

    async fn find_project(&self, repository: &Repository) -> Result<Project> {
        let expected = repository
            .remote
            .as_ref()
            .map(|remote| normalize_remote(&remote.url))
            .ok_or_else(|| Error::ConductorProjectNotFound(repository.root.clone()))?;
        let mut offset = 0_u64;
        loop {
            let mut url = api_endpoint(&self.config.api_url, &["projects"])?;
            url.query_pairs_mut()
                .append_pair("limit", "100")
                .append_pair("offset", &offset.to_string());
            let page = parse_project_page(&self.get(url).await?)?;
            if let Some(project) = page
                .projects
                .into_iter()
                .find(|project| normalize_remote(&project.git_remote) == expected)
            {
                return Ok(project);
            }
            if !page.has_more {
                return Err(Error::ConductorProjectNotFound(repository.root.clone()));
            }
            if page.next_offset <= offset {
                return Err(Error::InvalidResponse(
                    "Conductor project pagination did not advance".into(),
                ));
            }
            offset = page.next_offset;
        }
    }

    async fn record(&self, run_id: &str) -> Result<RunRecord> {
        let record = self.registry.get(run_id).await?;
        if !matches!(record.session, BackendSession::Conductor { .. }) {
            return Err(Error::RunNotFound(run_id.to_string()));
        }
        Ok(record)
    }

    async fn transcript_delta(
        &self,
        session_id: &str,
        mut offset: u64,
    ) -> Result<(Option<String>, u64)> {
        let mut messages = Vec::new();
        loop {
            let mut url =
                api_endpoint(&self.config.api_url, &["sessions", session_id, "messages"])?;
            url.query_pairs_mut()
                .append_pair("limit", "100")
                .append_pair("offset", &offset.to_string());
            let page = parse_message_page(&self.get(url).await?)?;
            let count = page.messages.len() as u64;
            messages.extend(page.messages);
            offset += count;
            if !page.has_more {
                break;
            }
            if count == 0 {
                return Err(Error::InvalidResponse(
                    "Conductor transcript pagination did not advance".into(),
                ));
            }
        }
        Ok((transcript_text(&messages), offset))
    }
}

#[async_trait]
impl Backend for ConductorBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Conductor
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new([
            Capability::Detect,
            Capability::Dispatch,
            Capability::Refresh,
            Capability::SendInput,
            Capability::Stop,
            Capability::Open,
            Capability::DeleteWorktree,
            Capability::Remote,
        ])
    }

    async fn owns_run(&self, run_id: &str) -> bool {
        self.registry
            .get(run_id)
            .await
            .is_ok_and(|record| matches!(record.session, BackendSession::Conductor { .. }))
    }

    async fn detect(&self, repository: &Repository) -> Result<BackendDetection> {
        let result = async {
            self.identity().await?;
            self.find_project(repository).await?;
            Result::<()>::Ok(())
        }
        .await;
        Ok(BackendDetection {
            backend: self.kind(),
            available: result.is_ok(),
            manager_running: false,
            capabilities: self.capabilities(),
            message: result.err().map(|error| error.to_string()),
            compute_targets: Vec::new(),
        })
    }

    async fn dispatch(&self, request: DispatchRequest) -> Result<DispatchResult> {
        request.validate()?;
        crate::private::guard_dispatch(&request, self.kind()).await?;
        let agent = validate_agent(&request.agent)?;
        self.identity().await?;
        let project = self.find_project(&request.repository).await?;
        let issue_hash = stable_hash(&request.issue.key.canonical());
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
                .unwrap_or(&format!("agent/{workspace_name}-{}", &issue_hash[..8])),
        );
        let body = workspace_body(
            &project.id,
            &workspace_name,
            &branch,
            agent,
            request.model.as_deref(),
            request.effort.as_deref(),
            &request.prompt,
        );
        let dispatch_key = stable_hash(&format!("{}\0{}", request.issue.key.canonical(), body));

        let _guard = self.registry.conductor_dispatch_lock.lock().await;
        if let Some(existing) = self.registry.conductor_dispatch(&dispatch_key).await {
            return Ok(DispatchResult {
                run: existing.summary,
                capabilities: self.capabilities(),
            });
        }

        // Workspace creation is not idempotent, so this call is deliberately never retried.
        let created = parse_workspace_created(
            &self
                .post(api_endpoint(&self.config.api_url, &["workspaces"])?, &body)
                .await?,
        )?;
        let now = Utc::now();
        let run_id = Uuid::new_v4().to_string();
        let summary = RunSummary {
            confidential: false,
            id: run_id,
            issue_key: request.issue.key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Conductor,
                id: created.workspace_id.clone(),
                host: self.config.api_url.host_str().map(str::to_string),
                path: None,
                branch,
            }),
            agent: agent.into(),
            model: request.model.clone(),
            state: RunState::Running,
            message: None,
            session_id: Some(created.session_id.clone()),
            started_at: now,
            updated_at: now,
        };
        self.registry
            .insert(RunRecord {
                summary: summary.clone(),
                session: BackendSession::Conductor {
                    workspace_id: created.workspace_id.clone(),
                    session_id: created.session_id.clone(),
                    deep_link: created.deep_link.clone(),
                    project_id: project.id,
                    dispatch_key: dispatch_key.clone(),
                    message_offset: 0,
                },
                deletion: None,
            })
            .await?;
        if !created.initial_message_accepted {
            let message_id = stable_hash(&format!("{dispatch_key}:initial-message"));
            if let Err(error) = self
                .post(
                    api_endpoint(&self.config.api_url, &[
                        "sessions",
                        &created.session_id,
                        "messages",
                    ])?,
                    &json!({"messageId": message_id, "message": request.prompt}),
                )
                .await
            {
                let message = error.to_string();
                self.registry
                    .set_state(
                        &summary.id,
                        RunState::Failed,
                        Some(format!("initial message failed: {message}")),
                    )
                    .await?;
                return Err(Error::ConductorInitialMessage {
                    workspace_id: created.workspace_id,
                    session_id: created.session_id,
                    message,
                });
            }
        }
        Ok(DispatchResult {
            run: summary,
            capabilities: self.capabilities(),
        })
    }

    async fn refresh(&self, run_id: &str) -> Result<StatusResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Conductor {
            session_id,
            message_offset,
            ..
        } = &record.session
        else {
            unreachable!()
        };
        let status_url = api_endpoint(&self.config.api_url, &["sessions", session_id, "status"])?;
        let (status, (output, next_offset)) = tokio::try_join!(
            self.get(status_url),
            self.transcript_delta(session_id, *message_offset)
        )?;
        let (state, message) = parse_session_status(&status)?;
        let mut record = record;
        let summary = &mut record.summary;
        if summary.state != RunState::Cancelled {
            summary.state = state;
            summary.message = message;
            summary.updated_at = Utc::now();
        }
        let BackendSession::Conductor { message_offset, .. } = &mut record.session else {
            unreachable!()
        };
        *message_offset = next_offset;
        let summary = summary.clone();
        self.registry.update(record).await?;
        Ok(StatusResult {
            run: summary,
            output,
        })
    }

    async fn send_input(&self, run_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Err(Error::InvalidRequest("input cannot be empty".into()));
        }
        let record = self.record(run_id).await?;
        let BackendSession::Conductor { session_id, .. } = record.session else {
            unreachable!()
        };
        self.post(
            api_endpoint(&self.config.api_url, &["sessions", &session_id, "messages"])?,
            &json!({"messageId": Uuid::new_v4().to_string(), "message": text}),
        )
        .await?;
        self.registry
            .set_state(run_id, RunState::Running, None)
            .await?;
        Ok(())
    }

    async fn stop(&self, run_id: &str) -> Result<()> {
        let record = self.record(run_id).await?;
        if record.summary.state == RunState::Cancelled {
            return Ok(());
        }
        let BackendSession::Conductor { session_id, .. } = record.session else {
            unreachable!()
        };
        self.post_empty(api_endpoint(&self.config.api_url, &[
            "sessions",
            &session_id,
            "cancel",
        ])?)
        .await?;
        self.registry
            .set_state(
                run_id,
                RunState::Cancelled,
                Some("Conductor session cancellation requested".into()),
            )
            .await?;
        Ok(())
    }

    async fn open(&self, run_id: &str) -> Result<OpenResult> {
        let record = self.record(run_id).await?;
        let BackendSession::Conductor { deep_link, .. } = record.session else {
            unreachable!()
        };
        let uri = Url::parse(&deep_link).map_err(|error| {
            Error::InvalidResponse(format!("invalid Conductor deepLink: {error}"))
        })?;
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
        let BackendSession::Conductor { workspace_id, .. } = record.session else {
            unreachable!()
        };
        self.registry.begin_deletion(run_id, true, None).await?;
        match self
            .post_empty(api_endpoint(&self.config.api_url, &[
                "workspaces",
                &workspace_id,
                "archive",
            ])?)
            .await
        {
            Ok(_) | Err(Error::HttpStatus { status: 404, .. }) => {},
            Err(error) => {
                let _ = self.registry.cancel_deletion(run_id).await;
                return Err(error);
            },
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

#[derive(Debug, Eq, PartialEq)]
struct Project {
    id: String,
    git_remote: String,
}

#[derive(Debug, Eq, PartialEq)]
struct ProjectPage {
    projects: Vec<Project>,
    next_offset: u64,
    has_more: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceCreated {
    workspace_id: String,
    session_id: String,
    deep_link: String,
    initial_message_accepted: bool,
}

struct MessagePage {
    messages: Vec<Value>,
    has_more: bool,
}

fn validate_agent(agent: &str) -> Result<&str> {
    let normalized = agent.trim().to_ascii_lowercase();
    if normalized == "opencode" || normalized == "open-code" {
        return Err(Error::InvalidRequest(
            "Conductor does not support OpenCode; supported agents are claude, codex, cursor, and acp"
                .into(),
        ));
    }
    SUPPORTED_AGENTS
        .iter()
        .copied()
        .find(|supported| *supported == normalized)
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "Conductor agent {agent:?} is unsupported; expected one of: {}",
                SUPPORTED_AGENTS.join(", ")
            ))
        })
}

fn workspace_body(
    project_id: &str,
    name: &str,
    branch: &str,
    agent: &str,
    model: Option<&str>,
    effort: Option<&str>,
    message: &str,
) -> Value {
    let mut body = Map::from_iter([
        ("projectId".into(), Value::String(project_id.into())),
        ("name".into(), Value::String(name.into())),
        ("branch".into(), Value::String(branch.into())),
        ("agent".into(), Value::String(agent.into())),
        ("message".into(), Value::String(message.into())),
    ]);
    if let Some(model) = model {
        body.insert("model".into(), Value::String(model.into()));
    }
    if let Some(effort) = effort {
        body.insert("effort".into(), Value::String(effort.into()));
    }
    Value::Object(body)
}

fn parse_project_page(value: &Value) -> Result<ProjectPage> {
    let projects = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidResponse("Conductor projects response has no data".into()))?
        .iter()
        .map(|project| {
            let id = project
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidResponse("Conductor project has no id".into()))?;
            let git_remote = project
                .get("gitRemote")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::InvalidResponse("Conductor project has no gitRemote".into())
                })?;
            Ok(Project {
                id: id.into(),
                git_remote: git_remote.into(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let offset = value.get("offset").and_then(Value::as_u64).ok_or_else(|| {
        Error::InvalidResponse("Conductor projects response has no offset".into())
    })?;
    let has_more = value
        .get("hasMore")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            Error::InvalidResponse("Conductor projects response has no hasMore".into())
        })?;
    Ok(ProjectPage {
        next_offset: offset + projects.len() as u64,
        projects,
        has_more,
    })
}

fn parse_workspace_created(value: &Value) -> Result<WorkspaceCreated> {
    let string = |key| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                Error::InvalidResponse(format!("Conductor create response has no {key}"))
            })
    };
    let deep_link = string("deepLink")?;
    Url::parse(&deep_link)
        .map_err(|error| Error::InvalidResponse(format!("invalid Conductor deepLink: {error}")))?;
    Ok(WorkspaceCreated {
        workspace_id: string("workspaceId")?,
        session_id: string("sessionId")?,
        deep_link,
        initial_message_accepted: value.get("initialMessage").is_some(),
    })
}

fn parse_session_status(value: &Value) -> Result<(RunState, Option<String>)> {
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidResponse("Conductor session status is missing".into()))?;
    let message = value
        .get("errorMessage")
        .or_else(|| value.get("lastError"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let state = match status {
        "working" => RunState::Running,
        "idle" => RunState::Idle,
        "error" => RunState::Failed,
        unknown => {
            return Err(Error::InvalidResponse(format!(
                "unknown Conductor session status: {unknown}"
            )));
        },
    };
    Ok((state, message))
}

fn parse_message_page(value: &Value) -> Result<MessagePage> {
    let messages = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| Error::InvalidResponse("Conductor messages response has no data".into()))?;
    let has_more = value
        .get("hasMore")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            Error::InvalidResponse("Conductor messages response has no hasMore".into())
        })?;
    Ok(MessagePage { messages, has_more })
}

fn transcript_text(messages: &[Value]) -> Option<String> {
    let text = messages
        .iter()
        .filter_map(|message| {
            let content = content_text(message.get("content")?)?;
            let kind = message.get("type").and_then(Value::as_str);
            Some(kind.map_or(content.clone(), |kind| format!("{kind}: {content}")))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => {
            let text = values
                .iter()
                .filter_map(content_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        },
        Value::Object(object) => ["text", "content", "message", "output"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(content_text))
            .or_else(|| serde_json::to_string(value).ok()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        Value::Null => None,
    }
}

fn api_endpoint(base: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| Error::InvalidRequest("Conductor API URL cannot be a base URL".into()))?;
        path.pop_if_empty();
        path.extend(segments);
    }
    Ok(url)
}

fn root_endpoint(base: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| Error::InvalidRequest("Conductor API URL cannot be a base URL".into()))?;
        path.clear();
        path.extend(segments);
    }
    Ok(url)
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

fn stable_hash(value: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, value.as_bytes())
        .simple()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_opencode_with_a_clear_error() {
        let error = validate_agent("OpenCode").expect_err("OpenCode must be rejected");
        assert!(error.to_string().contains("does not support OpenCode"));
        assert_eq!(
            validate_agent("codex").expect("codex is supported"),
            "codex"
        );
    }

    #[test]
    fn builds_documented_workspace_body() {
        assert_eq!(
            workspace_body(
                "project_1",
                "fix-login",
                "agent/fix-login",
                "codex",
                Some("gpt-5.6-sol"),
                Some("high"),
                "Fix the login"
            ),
            json!({
                "projectId": "project_1",
                "name": "fix-login",
                "branch": "agent/fix-login",
                "agent": "codex",
                "model": "gpt-5.6-sol",
                "effort": "high",
                "message": "Fix the login"
            })
        );
    }

    #[test]
    fn model_changes_dispatch_identity() {
        let key = |model| {
            stable_hash(&format!(
                "issue\0{}",
                workspace_body(
                    "project",
                    "workspace",
                    "branch",
                    "claude",
                    model,
                    None,
                    "review"
                )
            ))
        };
        assert_ne!(key(None), key(Some("sonnet")));
        assert_ne!(key(Some("sonnet")), key(Some("opus")));
    }

    #[test]
    fn parses_projects_create_status_and_transcript() {
        let projects = parse_project_page(&json!({
            "data": [{"id": "p1", "name": "repo", "gitRemote": "git@github.com:Org/Repo.git"}],
            "offset": 0,
            "hasMore": false
        }))
        .expect("projects should parse");
        assert_eq!(projects.projects[0].id, "p1");
        assert_eq!(projects.next_offset, 1);
        assert_eq!(
            normalize_remote(&projects.projects[0].git_remote),
            normalize_remote("https://github.com/org/repo")
        );

        let created = parse_workspace_created(&json!({
            "workspaceId": "w1",
            "sessionId": "s1",
            "deepLink": "conductor://workspace/w1",
            "initialMessage": {"messageId": "m1", "state": "queued", "deepLink": "conductor://workspace/w1"}
        }))
        .expect("create response should parse");
        assert!(created.initial_message_accepted);
        assert_eq!(created.session_id, "s1");
        assert_eq!(
            parse_session_status(&json!({"status": "working", "updatedAt": "now"}))
                .expect("status should parse"),
            (RunState::Running, None)
        );

        let messages = vec![
            json!({"type": "user", "content": "Fix it"}),
            json!({"type": "assistant", "content": [{"type": "text", "text": "Fixed"}]}),
        ];
        assert_eq!(
            transcript_text(&messages).as_deref(),
            Some("user: Fix it\n\nassistant: Fixed")
        );
    }

    #[test]
    fn constructs_versioned_and_identity_urls() {
        let base = Url::parse("https://api.conductor.build/v0/").expect("URL should parse");
        assert_eq!(
            api_endpoint(&base, &["sessions", "s/1", "status"])
                .expect("endpoint should build")
                .as_str(),
            "https://api.conductor.build/v0/sessions/s%2F1/status"
        );
        assert_eq!(
            root_endpoint(&base, &["me"])
                .expect("endpoint should build")
                .as_str(),
            "https://api.conductor.build/me"
        );
        assert_eq!(
            api_endpoint(&base, &["workspaces", "w1", "archive"])
                .expect("archive endpoint should build")
                .as_str(),
            "https://api.conductor.build/v0/workspaces/w1/archive"
        );
    }
}
