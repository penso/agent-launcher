use std::{
    collections::HashMap,
    env,
    process::{Command, Output, Stdio},
    sync::Mutex,
};

use agent_launcher_core::{Issue, IssueKey, IssueProvider, PullRequestMetadata, RepositoryRemote};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use reqwest::{
    Client,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, LINK},
    redirect,
};
use serde::Deserialize;
use url::Url;

use crate::{Error, IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

const INCREMENTAL_OVERLAP: Duration = Duration::minutes(5);
const FULL_RECONCILIATION_INTERVAL: Duration = Duration::hours(6);
const MAX_PR_DETAILS_PER_SYNC: usize = 10;

/// Synchronizes issues and pull requests under one source key. Full results
/// contain both open issues and open/draft PRs; deltas include closed transitions.
pub struct GitHubSource {
    key: SourceKey,
    client: Client,
    endpoint: Url,
    backoff: Mutex<Backoff>,
}

#[derive(Default)]
struct Backoff {
    retry_at: Option<DateTime<Utc>>,
    failures: u32,
}

impl Backoff {
    fn throttle(&mut self, deadline: Option<DateTime<Utc>>, now: DateTime<Utc>) {
        let seconds = 60 * 2_i64.pow(self.failures.min(6));
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(
            deadline
                .filter(|at| *at > now)
                .unwrap_or(now + Duration::seconds(seconds.min(3600))),
        );
    }
}

impl GitHubSource {
    async fn get(&self, url: Url) -> Result<reqwest::Response, Error> {
        if let Some(retry_at) = self.retry_at() {
            return Err(Error::Throttled { retry_at });
        }
        let response = self.client.get(url).send().await?;
        if response.status().is_success() {
            return Ok(response);
        }
        let deadline = retry_deadline(response.headers(), Utc::now());
        let error = http_status_error(response).await;
        if matches!(error, Error::GitHubRateLimit { .. }) {
            self.backoff.lock().unwrap().throttle(deadline, Utc::now());
        }
        Err(error)
    }

    fn pulls_endpoint(&self) -> Url {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .expect("GitHub API base URL")
            .pop()
            .push("pulls");
        url
    }

    async fn pull_request(&self, number: u64) -> Result<Issue, Error> {
        // Construct detail URLs locally rather than trusting URLs in API payloads.
        let mut url = self.pulls_endpoint();
        url.path_segments_mut()
            .expect("GitHub API base URL")
            .push(&number.to_string());
        let response = self.get(url).await?;
        let record: GitHubPullRequest =
            serde_json::from_slice(&response.bytes().await?).map_err(|error| Error::Json {
                source: "GitHub",
                error,
            })?;
        Ok(record.into_issue(&self.key))
    }

    pub fn from_remote(remote: RepositoryRemote) -> Result<Self, Error> {
        let token = resolve_token(
            &remote.host,
            |name| env::var(name).ok(),
            |command| command.output(),
        );
        Self::new(remote.host, remote.repository, token.as_deref())
    }

    pub fn new(host: String, repository: String, token: Option<&str>) -> Result<Self, Error> {
        let endpoint = github_endpoint(&host, &repository)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        if let Some(token) = token {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let client = Client::builder()
            .user_agent("agent-launcher")
            .default_headers(headers)
            .redirect(redirect_policy(token.is_some()))
            .build()
            .map_err(Error::HttpClient)?;
        Ok(Self {
            key: SourceKey {
                provider: IssueProvider::Github,
                host,
                repository,
            },
            client,
            endpoint,
            backoff: Mutex::new(Backoff::default()),
        })
    }
}

#[async_trait]
impl IssueSource for GitHubSource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    async fn sync(&self, checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error> {
        self.sync_with_cache(checkpoint, &[]).await
    }

    fn retry_at(&self) -> Option<DateTime<Utc>> {
        self.backoff
            .lock()
            .unwrap()
            .retry_at
            .filter(|at| *at > Utc::now())
    }

    async fn sync_with_cache(
        &self,
        checkpoint: Option<&SyncCheckpoint>,
        cached: &[Issue],
    ) -> Result<SyncResult, Error> {
        if let Some(retry_at) = self.retry_at() {
            return Err(Error::Throttled { retry_at });
        }
        let now = Utc::now();
        let full = checkpoint.and_then(|value| value.updated_at).is_none()
            || checkpoint
                .and_then(|value| value.last_full_at)
                .is_none_or(|last_full| now - last_full >= FULL_RECONCILIATION_INTERVAL);
        let mut issues = Vec::new();
        let mut pr_indices = HashMap::new();
        let mut updated_at = checkpoint.and_then(|value| value.updated_at);
        let mut page = 1_u32;

        loop {
            let mut url = self.endpoint.clone();
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair(
                        "state",
                        if full {
                            "open"
                        } else {
                            "all"
                        },
                    )
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
                if !full && let Some(since) = checkpoint.and_then(|value| value.updated_at) {
                    query.append_pair("since", &(since - INCREMENTAL_OVERLAP).to_rfc3339());
                }
            }

            let response = self.get(url).await?;

            let has_next = github_has_next(response.headers());
            let body = response.bytes().await?;
            let records: Vec<GitHubIssue> =
                serde_json::from_slice(&body).map_err(|error| Error::Json {
                    source: "GitHub",
                    error,
                })?;

            for record in records {
                updated_at = newest(updated_at, Some(record.updated_at));
                if record.pull_request.is_none() {
                    issues.push(record.into_issue(&self.key));
                } else if !full {
                    pr_indices.insert(record.number, issues.len());
                    issues.push(record.into_pull_request(&self.key));
                }
            }

            if !has_next {
                break;
            }
            page += 1;
        }

        {
            // Checkpoints do not prove cache completeness (e.g. older issue-only
            // clients). Reconcile all open PRs even when issues are incremental.
            let mut page = 1_u32;
            loop {
                let mut url = self.pulls_endpoint();
                url.query_pairs_mut()
                    .append_pair("state", "open")
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
                let response = self.get(url).await?;
                let has_next = github_has_next(response.headers());
                let records: Vec<GitHubPullRequest> =
                    serde_json::from_slice(&response.bytes().await?).map_err(|error| {
                        Error::Json {
                            source: "GitHub",
                            error,
                        }
                    })?;
                for record in records {
                    let number = record.issue.number;
                    let issue = record.into_issue(&self.key);
                    if let Some(&index) = pr_indices.get(&number) {
                        issues[index] = issue;
                    } else {
                        pr_indices.insert(number, issues.len());
                        issues.push(issue);
                    }
                }
                if !has_next {
                    break;
                }
                page += 1;
            }
        }

        // Closed transitions can leave the delta window before their detail turn.
        // Keep them eligible until the next open-only full reconciliation.
        if !full {
            for issue in cached {
                if let Some(metadata) = &issue.pull_request
                    && issue.key.provider == self.key.provider
                    && issue.key.host == self.key.host
                    && issue.key.repository == self.key.repository
                    && matches!(issue.state.as_str(), "closed" | "merged")
                    && !pr_indices.contains_key(&metadata.number)
                {
                    pr_indices.insert(metadata.number, issues.len());
                    issues.push(issue.clone());
                }
            }
        }
        let cached: HashMap<_, _> = cached.iter().map(|issue| (&issue.key, issue)).collect();
        let mut details = checkpoint.map(|c| c.pr_details.clone()).unwrap_or_default();
        let mut cursor = checkpoint.and_then(|c| c.pr_cursor);
        let mut pending = Vec::new();
        for (index, issue) in issues.iter_mut().enumerate() {
            let Some(metadata) = issue.pull_request.as_mut() else {
                continue;
            };
            let previous = cached
                .get(&issue.key)
                .and_then(|old| old.pull_request.as_ref());
            if let Some(old) = previous {
                metadata.additions = metadata.additions.or(old.additions);
                metadata.deletions = metadata.deletions.or(old.deletions);
                if metadata.head_sha.is_empty() {
                    metadata.head_sha.clone_from(&old.head_sha);
                    metadata.base_sha.clone_from(&old.base_sha);
                    metadata.head_ref.clone_from(&old.head_ref);
                    metadata.base_ref.clone_from(&old.base_ref);
                    metadata.head_repository.clone_from(&old.head_repository);
                }
            }
            let number = metadata.number;
            if previous.is_none_or(|old| old.additions.is_none() || old.deletions.is_none())
                || details.get(&issue.key.native_id) != Some(&pr_revision(issue))
            {
                pending.push((index, number));
            }
        }
        // Rotate attempts, including failures, so a permanently failing PR cannot
        // starve later entries. Only validity markers, not records, live here.
        if let Some(cursor) = cursor {
            pending.sort_by_key(|(_, number)| (*number <= cursor, *number));
        }
        let mut successful = true;
        for (index, number) in pending.into_iter().take(MAX_PR_DETAILS_PER_SYNC) {
            cursor = Some(number);
            match self.pull_request(number).await {
                Ok(mut detail) => {
                    let revision = pr_revision(&detail);
                    let metadata = detail.pull_request.as_mut().expect("PR detail metadata");
                    if metadata.additions.is_some() && metadata.deletions.is_some() {
                        details.insert(issues[index].key.native_id.clone(), revision);
                    } else {
                        details.remove(&issues[index].key.native_id);
                        let old = issues[index].pull_request.as_ref().unwrap();
                        metadata.additions = metadata.additions.or(old.additions);
                        metadata.deletions = metadata.deletions.or(old.deletions);
                    }
                    issues[index] = detail;
                },
                Err(Error::GitHubRateLimit { .. } | Error::Throttled { .. }) => {
                    successful = false;
                    break;
                },
                Err(_) => successful = false,
            }
        }
        if full {
            details.retain(|key, _| issues.iter().any(|issue| &issue.key.native_id == key));
        }
        if successful {
            *self.backoff.lock().unwrap() = Backoff::default();
        }

        Ok(SyncResult {
            issues,
            checkpoint: SyncCheckpoint {
                updated_at,
                etag: None,
                pr_details: details,
                pr_cursor: cursor,
                last_full_at: if full {
                    Some(now)
                } else {
                    checkpoint.and_then(|value| value.last_full_at)
                },
            },
            mode: if full {
                SyncMode::Full
            } else {
                SyncMode::Delta
            },
        })
    }
}

fn pr_revision(issue: &Issue) -> String {
    let metadata = issue.pull_request.as_ref().expect("PR revision metadata");
    format!(
        "{:?}:{}:{}:{}",
        issue.updated_at, metadata.head_sha, metadata.base_sha, issue.state
    )
}

fn is_public_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com")
}

fn resolve_token(
    host: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
    mut run: impl FnMut(&mut Command) -> std::io::Result<Output>,
) -> Option<String> {
    if is_public_github_host(host)
        && let Some(token) = lookup("GH_TOKEN").or_else(|| lookup("GITHUB_TOKEN"))
    {
        return Some(token);
    }
    let mut command = Command::new("gh");
    command.args(["auth", "token", "--hostname", &host.to_ascii_lowercase()]);
    // Ask for stored host credentials, never gh's generic environment overrides.
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        command.env_remove(name);
    }
    command.stdin(Stdio::null()).stderr(Stdio::null());
    let output = run(&mut command).ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!token.is_empty()).then_some(token)
}

fn redirect_policy(has_credentials: bool) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if has_credentials
            && attempt
                .previous()
                .first()
                .is_some_and(|origin| !same_origin(origin, attempt.url()))
        {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.origin() == right.origin()
}

fn github_endpoint(host: &str, repository: &str) -> Result<Url, Error> {
    let base = if host.eq_ignore_ascii_case("github.com") {
        "https://api.github.com/".to_owned()
    } else {
        format!("https://{host}/api/v3/")
    };
    let mut url = Url::parse(&base)?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| Error::InvalidRemoteUrl(base))?;
    segments
        .pop_if_empty()
        .extend(["repos"])
        .extend(repository.split('/'))
        .extend(["issues"]);
    drop(segments);
    Ok(url)
}

fn github_has_next(headers: &HeaderMap) -> bool {
    headers
        .get(LINK)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|link| link.split(';').any(|part| part.trim() == "rel=\"next\""))
        })
}

fn retry_deadline(headers: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let retry = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<i64>()
                .ok()
                .filter(|seconds| *seconds >= 0)
                .and_then(|seconds| now.checked_add_signed(Duration::try_seconds(seconds)?))
                .or_else(|| {
                    DateTime::parse_from_rfc2822(v)
                        .ok()
                        .map(|at| at.with_timezone(&Utc))
                })
        });
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
    retry.into_iter().chain(reset).filter(|at| *at > now).max()
}

async fn http_status_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let host = match response.url().host_str() {
        Some("api.github.com") => "github.com",
        Some(host) => host,
        None => "github.com",
    }
    .to_owned();
    let url = response.url().to_string();
    let limited = response
        .headers()
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0")
        || response.headers().contains_key("retry-after");
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| error.to_string());
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "GitHub API request failed".to_owned());
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || (status == reqwest::StatusCode::FORBIDDEN
            && (limited || message.to_ascii_lowercase().contains("rate limit")))
    {
        return Error::GitHubRateLimit { status, url, host };
    }
    let mut body: String = message.chars().take(300).collect();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        body.push_str("; check GitHub authentication and repository access");
    }
    Error::HttpStatus { status, url, body }
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
struct GitHubIssue {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    html_url: String,
    user: Option<GitHubUser>,
    #[serde(default)]
    labels: Vec<GitHubLabel>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    draft: bool,
}

impl GitHubIssue {
    fn into_pull_request(self, source: &SourceKey) -> Issue {
        let merged = self
            .pull_request
            .as_ref()
            .and_then(|pr| pr.get("merged_at"))
            .is_some_and(|value| !value.is_null());
        GitHubPullRequest {
            draft: self.draft,
            issue: self,
            merged,
            merged_at: None,
            additions: None,
            deletions: None,
            base: GitHubPullRequestRef::default(),
            head: GitHubPullRequestRef::default(),
        }
        .into_issue(source)
    }

    fn into_issue(self, source: &SourceKey) -> Issue {
        Issue {
            pull_request: None,
            key: IssueKey {
                provider: source.provider,
                host: source.host.clone(),
                repository: source.repository.clone(),
                native_id: self.number.to_string(),
            },
            identifier: format!("#{}", self.number),
            title: self.title,
            description: self.body,
            state: self.state,
            url: Some(self.html_url),
            author: self.user.map(|user| user.login),
            labels: self.labels.into_iter().map(|label| label.name).collect(),
            parent_id: None,
            blocked_by: Vec::new(),
            priority: None,
            created_at: Some(self.created_at),
            updated_at: Some(self.updated_at),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubPullRequest {
    #[serde(flatten)]
    issue: GitHubIssue,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged: bool,
    merged_at: Option<DateTime<Utc>>,
    additions: Option<u64>,
    deletions: Option<u64>,
    base: GitHubPullRequestRef,
    head: GitHubPullRequestRef,
}

impl GitHubPullRequest {
    fn into_issue(self, source: &SourceKey) -> Issue {
        let number = self.issue.number;
        let mut issue = self.issue.into_issue(source);
        issue.key.native_id = format!("pr/{number}");
        issue.state = if self.merged || self.merged_at.is_some() {
            "merged"
        } else if issue.state == "closed" {
            "closed"
        } else if self.draft {
            "draft"
        } else {
            "open"
        }
        .to_owned();
        issue.pull_request = Some(PullRequestMetadata {
            number,
            additions: self.additions,
            deletions: self.deletions,
            base_ref: self.base.reference,
            head_ref: self.head.reference,
            base_sha: self.base.sha,
            head_sha: self.head.sha,
            head_repository: self.head.repo.map(|repo| repo.full_name),
        });
        issue
    }
}

#[derive(Debug, Default, Deserialize)]
struct GitHubPullRequestRef {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
    repo: Option<GitHubRepository>,
}

#[derive(Debug, Deserialize)]
struct GitHubRepository {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GitHubLabel {
    name: String,
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use url::Url;

    use super::{GitHubIssue, GitHubPullRequest, GitHubSource, is_public_github_host, same_origin};
    use crate::{IssueSource, SourceKey, SyncCheckpoint, SyncMode};

    fn pr_payload(number: u64) -> Value {
        json!({
            "number": number, "title": "Review me", "body": "Details",
            "state": "open", "html_url": format!("https://github.com/acme/app/pull/{number}"),
            "user": {"login": "octocat"}, "labels": [{"name": "review"}],
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z",
            "base": {"ref": "main", "sha": "base-sha"},
            "head": {"ref": "feature", "sha": "head-sha", "repo": {"full_name": "fork/app"}},
            "additions": 12, "deletions": 0
        })
    }

    fn pr_list(number: u64) -> Value {
        let mut value = pr_payload(number);
        value.as_object_mut().unwrap().remove("additions");
        value.as_object_mut().unwrap().remove("deletions");
        value
    }

    #[tokio::test]
    async fn progressively_enriches_all_prs_and_reuses_persisted_cache_after_restart() {
        let mut cached = Vec::new();
        let mut checkpoint = SyncCheckpoint::default();
        for (round, numbers) in [(0, 1..11), (1, 11..21), (2, 21..26), (3, 0..0)] {
            let mut responses = vec![
                ("/repos/acme/app/issues?", 200, false, json!([])),
                (
                    "/repos/acme/app/pulls?",
                    200,
                    false,
                    json!((1..=25).map(pr_list).collect::<Vec<_>>()),
                ),
            ];
            for number in numbers {
                responses.push(("/repos/acme/app/pulls/", 200, false, pr_payload(number)));
            }
            // New source on each pass: no in-memory record cache may be required.
            let (source, server) = mock_source(responses).await;
            let result = source
                .sync_with_cache(Some(&checkpoint), &cached)
                .await
                .unwrap();
            server.await.unwrap();
            assert_eq!(result.issues.len(), 25);
            assert_eq!(
                result
                    .issues
                    .iter()
                    .filter(|issue| issue.pull_request.as_ref().unwrap().additions.is_some())
                    .count(),
                ((round + 1) * 10).min(25)
            );
            checkpoint =
                serde_json::from_value(serde_json::to_value(result.checkpoint).unwrap()).unwrap();
            cached = serde_json::from_value(serde_json::to_value(result.issues).unwrap()).unwrap();
        }
    }

    #[tokio::test]
    async fn invalidates_changed_metadata_and_preserves_counts_on_optional_failure() {
        let mut cached = Vec::new();
        let mut checkpoint = SyncCheckpoint::default();
        for round in 0..5 {
            let mut list = pr_list(7);
            let mut detail = pr_payload(7);
            if round >= 1 {
                list["head"]["sha"] = json!("new-head");
                detail["head"]["sha"] = json!("new-head");
            }
            if round >= 3 {
                list["updated_at"] = json!("2026-01-04T00:00:00Z");
                detail["updated_at"] = list["updated_at"].clone();
            }
            let status = if round == 3 {
                429
            } else if round == 1 {
                500
            } else {
                200
            };
            let (source, server) = mock_source(vec![
                ("/repos/acme/app/issues?", 200, false, json!([])),
                ("/repos/acme/app/pulls?", 200, false, json!([list])),
                ("/repos/acme/app/pulls/7 ", status, false, detail),
            ])
            .await;
            let result = source
                .sync_with_cache(Some(&checkpoint), &cached)
                .await
                .unwrap();
            server.await.unwrap();
            let pr = result.issues[0].pull_request.as_ref().unwrap();
            assert_eq!(pr.additions, Some(12));
            assert_eq!(pr.deletions, Some(0));
            if round >= 1 {
                assert_eq!(pr.head_sha, "new-head");
            }
            cached = result.issues;
            checkpoint = result.checkpoint;
        }
    }

    #[test]
    fn honors_retry_headers_and_bounds_exponential_fallback() {
        use chrono::{Duration, Utc};
        use reqwest::header::HeaderMap;
        let now = Utc::now();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "120".parse().unwrap());
        assert_eq!(
            super::retry_deadline(&headers, now),
            Some(now + Duration::seconds(120))
        );
        let reset = now + Duration::seconds(300);
        headers.insert(
            "x-ratelimit-reset",
            reset.timestamp().to_string().parse().unwrap(),
        );
        assert_eq!(
            super::retry_deadline(&headers, now).unwrap().timestamp(),
            reset.timestamp()
        );
        headers.remove("x-ratelimit-reset");
        headers.insert("retry-after", reset.to_rfc2822().parse().unwrap());
        assert_eq!(
            super::retry_deadline(&headers, now).unwrap().timestamp(),
            reset.timestamp()
        );
        headers.insert("retry-after", "invalid".parse().unwrap());
        assert_eq!(super::retry_deadline(&headers, now), None);
        let mut backoff = super::Backoff::default();
        for expected in [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600] {
            backoff.throttle(None, now);
            assert_eq!(backoff.retry_at, Some(now + Duration::seconds(expected)));
        }
        backoff.throttle(Some(reset), now);
        assert_eq!(backoff.retry_at, Some(reset));
    }

    #[tokio::test]
    async fn closed_prs_finish_enrichment_outside_delta_window_until_full_reconciliation() {
        let mut checkpoint = SyncCheckpoint {
            updated_at: Some(chrono::Utc::now()),
            last_full_at: Some(chrono::Utc::now()),
            ..SyncCheckpoint::default()
        };
        let mut cached = Vec::new();
        for (round, numbers) in [(0, 1..11), (1, 11..13), (2, 0..0), (3, 0..0)] {
            let stubs = if round == 0 {
                (1..=12)
                    .map(|number| {
                        let mut stub = pr_list(number);
                        stub["state"] = json!("closed");
                        stub["pull_request"] = json!({});
                        stub
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if round == 3 {
                checkpoint.last_full_at = None;
            }
            let mut responses = vec![
                ("/repos/acme/app/issues?", 200, false, json!(stubs)),
                ("/repos/acme/app/pulls?", 200, false, json!([])),
            ];
            for number in numbers {
                let mut detail = pr_payload(number);
                detail["state"] = json!("closed");
                responses.push(("/repos/acme/app/pulls/", 200, false, detail));
            }
            let (source, server) = mock_source(responses).await;
            let result = source
                .sync_with_cache(Some(&checkpoint), &cached)
                .await
                .unwrap();
            server.await.unwrap();
            assert_eq!(
                result.issues.len(),
                if round == 3 {
                    0
                } else {
                    12
                }
            );
            assert_eq!(
                result.checkpoint.pr_details.len(),
                if round == 3 {
                    0
                } else {
                    ((round + 1) * 10).min(12)
                }
            );
            checkpoint = result.checkpoint;
            cached = result.issues;
        }
    }

    #[tokio::test]
    async fn failing_early_details_do_not_starve_later_prs() {
        let mut cached = Vec::new();
        let mut checkpoint = SyncCheckpoint::default();
        for numbers in [(1..=10).collect::<Vec<_>>(), vec![
            11, 12, 1, 2, 3, 4, 5, 6, 7, 8,
        ]] {
            let mut responses = vec![
                ("/repos/acme/app/issues?", 200, false, json!([])),
                (
                    "/repos/acme/app/pulls?",
                    200,
                    false,
                    json!((1..=12).map(pr_list).collect::<Vec<_>>()),
                ),
            ];
            for number in numbers {
                responses.push((
                    "/repos/acme/app/pulls/",
                    if number <= 10 {
                        500
                    } else {
                        200
                    },
                    false,
                    pr_payload(number),
                ));
            }
            let (source, server) = mock_source(responses).await;
            let result = source
                .sync_with_cache(Some(&checkpoint), &cached)
                .await
                .unwrap();
            server.await.unwrap();
            cached = result.issues;
            checkpoint = result.checkpoint;
        }
        assert_eq!(
            cached
                .iter()
                .filter(|issue| issue.pull_request.as_ref().unwrap().additions.is_some())
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn response_headers_set_the_source_deadline() {
        let deadline = chrono::Utc::now() + chrono::Duration::minutes(10);
        for headers in [
            "Retry-After: 600\r\n".to_owned(),
            format!("Retry-After: {}\r\n", deadline.to_rfc2822()),
            format!(
                "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {}\r\n",
                deadline.timestamp()
            ),
        ] {
            let (source, server) = mock_source_with_headers(
                vec![("/repos/acme/app/issues?", 403, false, json!({}))],
                headers,
            )
            .await;
            assert!(matches!(
                source.sync(None).await,
                Err(crate::Error::GitHubRateLimit { .. })
            ));
            server.await.unwrap();
            assert!((source.retry_at().unwrap().timestamp() - deadline.timestamp()).abs() <= 1);
            assert!(matches!(
                source.sync(None).await,
                Err(crate::Error::Throttled { .. })
            ));
        }
    }

    #[tokio::test]
    async fn throttling_suppresses_every_request_until_expiry_and_resets_after_success() {
        for optional in [false, true] {
            let mut responses = Vec::new();
            if optional {
                responses.extend([
                    ("/repos/acme/app/issues?", 200, false, json!([])),
                    ("/repos/acme/app/pulls?", 200, false, json!([pr_list(7)])),
                ]);
            }
            responses.push((
                if optional {
                    "/repos/acme/app/pulls/7 "
                } else {
                    "/repos/acme/app/issues?"
                },
                429,
                false,
                json!({}),
            ));
            let throttled = responses.clone();
            responses.extend(throttled.clone());
            responses.extend(throttled);
            responses.extend([
                ("/repos/acme/app/issues?", 200, false, json!([])),
                ("/repos/acme/app/pulls?", 200, false, json!([])),
            ]);
            let (source, server) = mock_source(responses).await;
            for seconds in [60, 120, 240] {
                assert_eq!(source.sync(None).await.is_ok(), optional);
                let remaining = (source.retry_at().unwrap() - chrono::Utc::now()).num_seconds();
                assert!((seconds - 1..=seconds).contains(&remaining));
                for _ in 0..5 {
                    assert!(matches!(
                        source.sync(None).await,
                        Err(crate::Error::Throttled { .. })
                    ));
                }
                source.backoff.lock().unwrap().retry_at =
                    Some(chrono::Utc::now() - chrono::Duration::seconds(1));
            }
            source.sync(None).await.unwrap();
            server.await.unwrap();
            assert_eq!(source.retry_at(), None);
            assert_eq!(source.backoff.lock().unwrap().failures, 0);
        }
    }

    async fn mock_source(
        responses: Vec<(&'static str, u16, bool, Value)>,
    ) -> (GitHubSource, tokio::task::JoinHandle<()>) {
        mock_source_with_headers(responses, String::new()).await
    }

    async fn mock_source_with_headers(
        responses: Vec<(&'static str, u16, bool, Value)>,
        headers: String,
    ) -> (GitHubSource, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for (path, status, next, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path}")), "{request}");
                if path == "/repos/acme/app/pulls/" {
                    assert!(
                        request.starts_with(&format!("GET {path}{} ", body["number"])),
                        "{request}"
                    );
                }
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-token\r\n")
                );
                let body = body.to_string();
                // The adapter must not follow an untrusted pagination URL with credentials.
                let link = if next {
                    "Link: <https://untrusted.example/page>; rel=\"next\"\r\n"
                } else {
                    ""
                };
                socket.write_all(format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{link}{headers}Connection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
        });
        let mut source =
            GitHubSource::new("github.com".into(), "acme/app".into(), Some("test-token")).unwrap();
        source.endpoint = Url::parse(&format!("http://{address}/repos/acme/app/issues")).unwrap();
        (source, task)
    }

    #[test]
    fn maps_pr_metadata_states_and_missing_counts() {
        let source = SourceKey {
            provider: IssueProvider::Github,
            host: "github.com".into(),
            repository: "acme/app".into(),
        };
        for (state, draft, merged, expected) in [
            ("open", false, false, "open"),
            ("open", true, false, "draft"),
            ("closed", true, false, "closed"),
            ("closed", true, true, "merged"),
        ] {
            let mut payload = pr_payload(7);
            payload["state"] = json!(state);
            payload["draft"] = json!(draft);
            payload["merged"] = json!(merged);
            let issue = serde_json::from_value::<GitHubPullRequest>(payload)
                .unwrap()
                .into_issue(&source);
            assert_eq!(issue.state, expected);
            assert_eq!(issue.key.native_id, "pr/7");
            assert_eq!(issue.identifier, "#7");
            assert_eq!(issue.author.as_deref(), Some("octocat"));
            assert_eq!(issue.labels, ["review"]);
            let metadata = issue.pull_request.unwrap();
            assert_eq!(metadata.number, 7);
            assert_eq!(metadata.additions, Some(12));
            assert_eq!(metadata.deletions, Some(0));
            assert_eq!(metadata.base_ref, "main");
            assert_eq!(metadata.head_ref, "feature");
            assert_eq!(metadata.base_sha, "base-sha");
            assert_eq!(metadata.head_sha, "head-sha");
            assert_eq!(metadata.head_repository.as_deref(), Some("fork/app"));
        }
        let mut payload = pr_payload(7);
        payload.as_object_mut().unwrap().remove("additions");
        payload["deletions"] = Value::Null;
        payload["head"]["repo"] = Value::Null;
        payload["merged_at"] = json!("2026-01-03T00:00:00Z");
        let issue = serde_json::from_value::<GitHubPullRequest>(payload)
            .unwrap()
            .into_issue(&source);
        assert_eq!(issue.state, "merged");
        let metadata = issue.pull_request.unwrap();
        assert_eq!(metadata.additions, None);
        assert_eq!(metadata.deletions, None);
        assert_eq!(metadata.head_repository, None);
    }

    #[tokio::test]
    async fn full_sync_filters_stubs_and_paginates_issues_and_prs_with_authenticated_details() {
        let mut stub = pr_payload(7);
        stub["pull_request"] = json!({"url": "https://untrusted.example/pr"});
        let (source, server) = mock_source(vec![
            (
                "/repos/acme/app/issues?state=open&per_page=100&page=1 ",
                200,
                true,
                json!([stub]),
            ),
            (
                "/repos/acme/app/issues?state=open&per_page=100&page=2 ",
                200,
                false,
                json!([pr_payload(1)]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                200,
                true,
                json!([pr_payload(7)]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=2 ",
                200,
                false,
                json!([pr_payload(8)]),
            ),
            ("/repos/acme/app/pulls/7 ", 200, false, pr_payload(7)),
            ("/repos/acme/app/pulls/8 ", 200, false, pr_payload(8)),
        ])
        .await;
        let result = source.sync(None).await.unwrap();
        server.await.unwrap();
        assert_eq!(result.mode, SyncMode::Full);
        assert_eq!(
            result
                .issues
                .iter()
                .map(|issue| issue.key.native_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "pr/7", "pr/8"]
        );
        assert!(result.issues[0].pull_request.is_none());
        assert_eq!(
            result.issues[1].pull_request.as_ref().unwrap().additions,
            Some(12)
        );
    }

    #[tokio::test]
    async fn delta_sync_fetches_closed_pr_details() {
        let mut stub = pr_payload(7);
        stub["pull_request"] = json!({});
        let mut detail = pr_payload(7);
        detail["state"] = json!("closed");
        detail["merged"] = json!(true);
        let (source, server) = mock_source(vec![
            (
                "/repos/acme/app/issues?state=all&per_page=100&page=1&since=",
                200,
                false,
                json!([stub]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                200,
                false,
                json!([]),
            ),
            ("/repos/acme/app/pulls/7 ", 200, false, detail),
        ])
        .await;
        let result = source
            .sync(Some(&SyncCheckpoint {
                updated_at: Some(chrono::Utc::now()),
                last_full_at: Some(chrono::Utc::now()),
                etag: None,
                ..SyncCheckpoint::default()
            }))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(result.mode, SyncMode::Delta);
        assert_eq!(result.issues[0].state, "merged");
        assert_eq!(result.issues[0].key.native_id, "pr/7");
    }

    #[tokio::test]
    async fn recent_checkpoint_discovers_unchanged_paginated_prs_with_bounded_details() {
        for changed_pr in [false, true] {
            let checkpoint = SyncCheckpoint {
                updated_at: Some(chrono::Utc::now()),
                last_full_at: Some(chrono::Utc::now()),
                etag: None,
                ..SyncCheckpoint::default()
            };
            let mut stub = pr_payload(1);
            stub["pull_request"] = json!({});
            let mut closed = pr_payload(300);
            closed["state"] = json!("closed");
            closed["pull_request"] = json!({"merged_at": "2026-01-03T00:00:00Z"});
            let deltas = if changed_pr {
                json!([stub])
            } else {
                json!([])
            };
            let mut responses = vec![
                (
                    "/repos/acme/app/issues?state=all&per_page=100&page=1&since=",
                    200,
                    true,
                    deltas,
                ),
                (
                    "/repos/acme/app/issues?state=all&per_page=100&page=2&since=",
                    200,
                    false,
                    json!([closed, pr_payload(301)]),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                    200,
                    true,
                    json!((1..=100).map(pr_payload).collect::<Vec<_>>()),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=2 ",
                    200,
                    true,
                    json!((101..=200).map(pr_payload).collect::<Vec<_>>()),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=3 ",
                    200,
                    false,
                    json!((201..=205).map(pr_payload).collect::<Vec<_>>()),
                ),
            ];
            let numbers = if changed_pr {
                vec![1, 300, 2, 3, 4, 5, 6, 7, 8, 9]
            } else {
                vec![300, 1, 2, 3, 4, 5, 6, 7, 8, 9]
            };
            assert_eq!(numbers.len(), super::MAX_PR_DETAILS_PER_SYNC);
            for number in numbers {
                let mut detail = pr_payload(number);
                if number == 300 {
                    detail["state"] = json!("closed");
                    detail["merged"] = json!(true);
                }
                responses.push(("/repos/acme/app/pulls/", 200, false, detail));
            }
            let (source, server) = mock_source(responses).await;
            let result = source.sync(Some(&checkpoint)).await.unwrap();
            server.await.unwrap();
            assert_eq!(result.mode, SyncMode::Delta);
            assert_eq!(result.checkpoint.updated_at, checkpoint.updated_at);
            assert_eq!(result.checkpoint.last_full_at, checkpoint.last_full_at);
            assert_eq!(result.issues.len(), 207);
            let keys = result
                .issues
                .iter()
                .map(|issue| issue.key.canonical())
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(keys.len(), result.issues.len());
            for number in 1..=205 {
                let issue = result
                    .issues
                    .iter()
                    .find(|issue| issue.key.native_id == format!("pr/{number}"))
                    .unwrap();
                assert_eq!(issue.state, "open");
                assert_eq!(issue.pull_request.as_ref().unwrap().base_ref, "main");
                assert_eq!(issue.key.host, "github.com");
                assert_eq!(issue.key.repository, "acme/app");
            }
            assert_eq!(
                result
                    .issues
                    .iter()
                    .find(|issue| issue.key.native_id == "pr/300")
                    .unwrap()
                    .state,
                "merged"
            );
            assert!(
                result
                    .issues
                    .iter()
                    .find(|issue| issue.key.native_id == "301")
                    .unwrap()
                    .pull_request
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn missing_cursor_or_expired_checkpoint_uses_open_only_full_reconciliation() {
        for missing_cursor in [false, true] {
            let checkpoint = SyncCheckpoint {
                updated_at: (!missing_cursor).then(chrono::Utc::now),
                last_full_at: Some(
                    chrono::Utc::now()
                        - if missing_cursor {
                            chrono::Duration::zero()
                        } else {
                            chrono::Duration::hours(7)
                        },
                ),
                etag: None,
                ..SyncCheckpoint::default()
            };
            let (source, server) = mock_source(vec![
                (
                    "/repos/acme/app/issues?state=open&per_page=100&page=1 ",
                    200,
                    false,
                    json!([]),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                    200,
                    false,
                    json!([]),
                ),
            ])
            .await;
            let result = source.sync(Some(&checkpoint)).await.unwrap();
            server.await.unwrap();
            assert_eq!(result.mode, SyncMode::Full);
            assert!(result.checkpoint.last_full_at >= checkpoint.last_full_at);
        }
    }

    #[tokio::test]
    async fn delta_pr_pagination_failure_does_not_advance_checkpoint() {
        let (source, server) = mock_source(vec![
            (
                "/repos/acme/app/issues?state=all&per_page=100&page=1&since=",
                200,
                false,
                json!([]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                200,
                true,
                json!([pr_payload(1)]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=2 ",
                500,
                false,
                json!({}),
            ),
        ])
        .await;
        assert!(
            source
                .sync(Some(&SyncCheckpoint {
                    updated_at: Some(chrono::Utc::now()),
                    last_full_at: Some(chrono::Utc::now()),
                    etag: None,
                    ..SyncCheckpoint::default()
                }))
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_pr_requests_never_return_partial_full_replacements() {
        for (detail_failure, status) in [(false, 500), (true, 500), (true, 403), (true, 401)] {
            let mut responses = vec![(
                "/repos/acme/app/issues?",
                200,
                false,
                json!([pr_payload(1)]),
            )];
            if detail_failure {
                responses.push(("/repos/acme/app/pulls?", 200, false, json!([pr_payload(7)])));
                responses.push(("/repos/acme/app/pulls/7 ", status, false, json!({})));
            } else {
                responses.push(("/repos/acme/app/pulls?", status, false, json!({})));
            }
            let (source, server) = mock_source(responses).await;
            assert_eq!(source.sync(None).await.is_err(), !detail_failure);
            server.await.unwrap();
        }
    }

    #[test]
    fn converts_github_issue() {
        let record: GitHubIssue = serde_json::from_value(serde_json::json!({
            "number": 42,
            "title": "Repair sync",
            "body": "Details",
            "state": "open",
            "html_url": "https://github.com/acme/app/issues/42",
            "user": { "login": "octocat" },
            "labels": [{ "name": "bug" }],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-02T00:00:00Z"
        }))
        .expect("valid GitHub issue");
        let issue = record.into_issue(&SourceKey {
            provider: IssueProvider::Github,
            host: "github.com".to_owned(),
            repository: "acme/app".to_owned(),
        });

        assert_eq!(issue.identifier, "#42");
        assert_eq!(issue.author.as_deref(), Some("octocat"));
        assert_eq!(issue.labels, ["bug"]);
        assert_eq!(issue.key.native_id, "42");
    }

    #[test]
    fn recognizes_pull_request_records() {
        let record: GitHubIssue = serde_json::from_value(serde_json::json!({
            "number": 7,
            "title": "A pull request",
            "body": null,
            "state": "open",
            "html_url": "https://github.com/acme/app/pull/7",
            "user": null,
            "labels": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "pull_request": { "url": "https://api.github.com/repos/acme/app/pulls/7" }
        }))
        .expect("valid pull request record");

        assert!(record.pull_request.is_some());
    }

    #[test]
    fn generic_tokens_are_scoped_to_the_exact_public_host() {
        assert!(is_public_github_host("github.com"));
        assert!(is_public_github_host("GITHUB.COM"));
        assert!(!is_public_github_host("github.com.example.org"));
        assert!(!is_public_github_host("github.com:443"));
    }

    #[test]
    #[cfg(unix)]
    fn token_resolution_preserves_precedence_and_scopes_the_command() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{ExitStatus, Output},
        };

        for (gh, github, expected) in [
            (Some("first"), Some("second"), "first"),
            (None, Some("second"), "second"),
        ] {
            let token = super::resolve_token(
                "GITHUB.COM",
                |name| {
                    if name == "GH_TOKEN" {
                        gh
                    } else {
                        github
                    }
                    .map(str::to_owned)
                },
                |_| panic!("environment token must take precedence"),
            );
            assert_eq!(token.as_deref(), Some(expected));
        }
        for host in [
            "github.com",
            "github.com.example.org",
            "ghe.example.org",
            "github.com:443",
        ] {
            for (status, stdout, expected) in [
                (
                    0,
                    b" stored-test-token\n".as_slice(),
                    Some("stored-test-token"),
                ),
                (0, b"\n".as_slice(), None),
                (0, b"\xff".as_slice(), None),
                (1, b"ignored".as_slice(), None),
            ] {
                let token = super::resolve_token(
                    host,
                    |name| {
                        assert_eq!(
                            host, "github.com",
                            "must not read generic env for other hosts: {name}"
                        );
                        None
                    },
                    |command| {
                        assert_eq!(command.get_program(), "gh");
                        assert_eq!(command.get_args().collect::<Vec<_>>(), [
                            "auth",
                            "token",
                            "--hostname",
                            host
                        ]);
                        for name in [
                            "GH_TOKEN",
                            "GITHUB_TOKEN",
                            "GH_ENTERPRISE_TOKEN",
                            "GITHUB_ENTERPRISE_TOKEN",
                        ] {
                            assert!(
                                command
                                    .get_envs()
                                    .any(|(key, value)| key == name && value.is_none())
                            );
                        }
                        Ok(Output {
                            status: ExitStatus::from_raw(status << 8),
                            stdout: stdout.to_vec(),
                            stderr: Vec::new(),
                        })
                    },
                );
                assert_eq!(token.as_deref(), expected);
            }
        }
        assert!(
            super::resolve_token(
                "github.com",
                |_| None,
                |_| Err(std::io::ErrorKind::NotFound.into())
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn rate_limited_details_keep_paginated_lists_and_stop_enrichment() {
        for status in [403, 429] {
            let mut draft = pr_payload(7);
            draft["draft"] = json!(true);
            for field in ["additions", "deletions"] {
                draft.as_object_mut().unwrap().remove(field);
            }
            let mut other = draft.clone();
            other["number"] = json!(8);
            let (source, server) = mock_source(vec![
                (
                    "/repos/acme/app/issues?",
                    200,
                    false,
                    json!([pr_payload(1)]),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                    200,
                    true,
                    json!([draft]),
                ),
                (
                    "/repos/acme/app/pulls?state=open&per_page=100&page=2 ",
                    200,
                    false,
                    json!([other]),
                ),
                (
                    "/repos/acme/app/pulls/7 ",
                    status,
                    false,
                    json!({"message": "API rate limit exceeded"}),
                ),
            ])
            .await;
            let result = source.sync(None).await.unwrap();
            server.await.unwrap();
            assert_eq!(result.mode, SyncMode::Full);
            assert!(result.checkpoint.last_full_at.is_some());
            assert_eq!(result.issues.len(), 3);
            for issue in &result.issues[1..] {
                assert_eq!(issue.state, "draft");
                let metadata = issue.pull_request.as_ref().unwrap();
                assert_eq!(metadata.additions, None);
                assert_eq!(metadata.deletions, None);
                assert_eq!(metadata.base_ref, "main");
                assert_eq!(metadata.head_repository.as_deref(), Some("fork/app"));
            }
        }
    }

    #[tokio::test]
    async fn delta_rate_limit_keeps_stub_state_and_unknown_counts() {
        let mut merged = pr_payload(7);
        merged["state"] = json!("closed");
        merged["pull_request"] = json!({"merged_at": "2026-01-03T00:00:00Z"});
        let mut draft = pr_payload(8);
        draft["draft"] = json!(true);
        draft["pull_request"] = json!({});
        let (source, server) = mock_source(vec![
            (
                "/repos/acme/app/issues?state=all",
                200,
                false,
                json!([merged, draft]),
            ),
            (
                "/repos/acme/app/pulls?state=open&per_page=100&page=1 ",
                200,
                false,
                json!([]),
            ),
            (
                "/repos/acme/app/pulls/7 ",
                403,
                false,
                json!({"message": "You have exceeded a secondary rate limit."}),
            ),
        ])
        .await;
        let result = source
            .sync(Some(&SyncCheckpoint {
                updated_at: Some(chrono::Utc::now()),
                last_full_at: Some(chrono::Utc::now()),
                etag: None,
                ..SyncCheckpoint::default()
            }))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(result.mode, SyncMode::Delta);
        assert_eq!(result.issues[0].state, "merged");
        assert_eq!(result.issues[1].state, "draft");
        for issue in result.issues {
            let metadata = issue.pull_request.unwrap();
            assert_eq!(metadata.additions, None);
            assert_eq!(metadata.deletions, None);
        }
    }

    #[tokio::test]
    async fn list_rate_limits_are_fatal_and_actionable() {
        for pulls in [false, true] {
            let mut responses = Vec::new();
            if pulls {
                responses.push(("/repos/acme/app/issues?", 200, false, json!([])));
            }
            responses.push((
                if pulls {
                    "/repos/acme/app/pulls?"
                } else {
                    "/repos/acme/app/issues?"
                },
                403,
                false,
                json!({"message": "API rate limit exceeded", "extra": "x".repeat(10000)}),
            ));
            let (source, server) = mock_source(responses).await;
            let error = source.sync(None).await.unwrap_err();
            server.await.unwrap();
            assert!(matches!(error, crate::Error::GitHubRateLimit { .. }));
            let text = error.to_string();
            assert!(text.contains("gh auth login --hostname"));
            assert!(text.len() < 500);
        }
    }

    #[test]
    fn redirect_origins_include_scheme_host_and_port() {
        let origin = Url::parse("https://github.com/repos/acme/app/issues").unwrap();
        let same = Url::parse("https://github.com/login").unwrap();
        let other_host = Url::parse("https://example.org/login").unwrap();
        let other_scheme = Url::parse("http://github.com/login").unwrap();

        assert!(same_origin(&origin, &same));
        assert!(!same_origin(&origin, &other_host));
        assert!(!same_origin(&origin, &other_scheme));
    }
}
