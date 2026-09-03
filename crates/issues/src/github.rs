use std::env;

use agent_launcher_core::{Issue, IssueKey, IssueProvider, RepositoryRemote};
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

pub struct GitHubSource {
    key: SourceKey,
    client: Client,
    endpoint: Url,
}

impl GitHubSource {
    pub fn from_remote(remote: RepositoryRemote) -> Result<Self, Error> {
        let token = if is_public_github_host(&remote.host) {
            env::var("GH_TOKEN")
                .or_else(|_| env::var("GITHUB_TOKEN"))
                .ok()
        } else {
            None
        };
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
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
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
        })
    }
}

#[async_trait]
impl IssueSource for GitHubSource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    async fn sync(&self, checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error> {
        let now = Utc::now();
        let full = checkpoint
            .and_then(|value| value.last_full_at)
            .is_none_or(|last_full| now - last_full >= FULL_RECONCILIATION_INTERVAL);
        let mut issues = Vec::new();
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

            let response = self.client.get(url.clone()).send().await?;
            if !response.status().is_success() {
                return Err(http_status_error(response).await);
            }

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
                }
            }

            if !has_next {
                break;
            }
            page += 1;
        }

        Ok(SyncResult {
            issues,
            checkpoint: SyncCheckpoint {
                updated_at,
                etag: None,
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

fn is_public_github_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com")
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

async fn http_status_error(response: reqwest::Response) -> Error {
    let status = response.status();
    let url = response.url().to_string();
    let body = response
        .text()
        .await
        .unwrap_or_else(|error| error.to_string());
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
}

impl GitHubIssue {
    fn into_issue(self, source: &SourceKey) -> Issue {
        Issue {
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
    use url::Url;

    use super::{GitHubIssue, is_public_github_host, same_origin};
    use crate::SourceKey;

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
