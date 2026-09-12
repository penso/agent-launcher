use std::env;

use agent_launcher_core::{Issue, IssueKey, IssueProvider, RepositoryRemote};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
    redirect,
};
use serde::Deserialize;
use url::Url;

use crate::{Error, IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

const INCREMENTAL_OVERLAP: Duration = Duration::minutes(5);
const FULL_RECONCILIATION_INTERVAL: Duration = Duration::hours(6);

pub struct GitLabSource {
    key: SourceKey,
    client: Client,
    endpoint: Url,
}

impl GitLabSource {
    pub fn from_remote(remote: RepositoryRemote) -> Result<Self, Error> {
        let token = if is_public_gitlab_host(&remote.host) {
            env::var("PRIVATE_TOKEN")
                .or_else(|_| env::var("GITLAB_TOKEN"))
                .ok()
        } else {
            None
        };
        Self::new(remote.host, remote.repository, token.as_deref())
    }

    pub fn new(host: String, repository: String, token: Option<&str>) -> Result<Self, Error> {
        let endpoint = gitlab_endpoint(&host, &repository)?;
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            headers.insert("private-token", HeaderValue::from_str(token)?);
        }
        let client = Client::builder()
            .user_agent("agent-launcher")
            .default_headers(headers)
            .redirect(redirect_policy(token.is_some()))
            .build()
            .map_err(Error::HttpClient)?;
        Ok(Self {
            key: SourceKey {
                provider: IssueProvider::Gitlab,
                host,
                repository,
            },
            client,
            endpoint,
        })
    }
}

#[async_trait]
impl IssueSource for GitLabSource {
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
                            "opened"
                        } else {
                            "all"
                        },
                    )
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
                if !full && let Some(since) = checkpoint.and_then(|value| value.updated_at) {
                    query.append_pair("updated_after", &(since - INCREMENTAL_OVERLAP).to_rfc3339());
                }
            }

            let response = self.client.get(url.clone()).send().await?;
            if !response.status().is_success() {
                return Err(http_status_error(response).await);
            }

            let next_page = response
                .headers()
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .and_then(|value| value.parse::<u32>().ok());
            let body = response.bytes().await?;
            let records: Vec<GitLabIssue> =
                serde_json::from_slice(&body).map_err(|error| Error::Json {
                    source: "GitLab",
                    error,
                })?;

            for record in records {
                updated_at = newest(updated_at, Some(record.updated_at));
                issues.push(record.into_issue(&self.key));
            }

            let Some(next_page) = next_page else {
                break;
            };
            page = next_page;
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
                ..SyncCheckpoint::default()
            },
            mode: if full {
                SyncMode::Full
            } else {
                SyncMode::Delta
            },
        })
    }
}

fn is_public_gitlab_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("gitlab.com")
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

fn gitlab_endpoint(host: &str, repository: &str) -> Result<Url, Error> {
    let base = format!("https://{host}/api/v4/");
    let mut url = Url::parse(&base)?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| Error::InvalidRemoteUrl(base))?;
    segments
        .pop_if_empty()
        .extend(["projects", repository, "issues"]);
    drop(segments);
    Ok(url)
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
struct GitLabIssue {
    user_notes_count: Option<u64>,
    iid: u64,
    title: String,
    description: Option<String>,
    state: String,
    web_url: String,
    author: Option<GitLabUser>,
    #[serde(default)]
    labels: Vec<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl GitLabIssue {
    fn into_issue(self, source: &SourceKey) -> Issue {
        Issue {
            pull_request: None,
            activity: Some(agent_launcher_core::ItemActivity {
                comments: self.user_notes_count,
                ..Default::default()
            }),
            key: IssueKey {
                provider: source.provider,
                host: source.host.clone(),
                repository: source.repository.clone(),
                native_id: self.iid.to_string(),
            },
            identifier: format!("#{}", self.iid),
            title: self.title,
            description: self.description,
            state: self.state,
            url: Some(self.web_url),
            author: self.author.map(|author| author.username),
            labels: self.labels,
            parent_id: None,
            blocked_by: Vec::new(),
            priority: None,
            created_at: Some(self.created_at),
            updated_at: Some(self.updated_at),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitLabUser {
    username: String,
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;
    use url::Url;

    use super::{GitLabIssue, gitlab_endpoint, is_public_gitlab_host, same_origin};
    use crate::SourceKey;

    #[test]
    fn maps_optional_user_notes_count() {
        for count in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!(0)),
            Some(serde_json::json!(42)),
        ] {
            let mut payload = serde_json::json!({"iid": 1, "title": "Notes", "state": "opened", "web_url": "https://gitlab.com/acme/app/-/issues/1", "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"});
            if let Some(count) = &count {
                payload["user_notes_count"] = count.clone();
            }
            let issue = serde_json::from_value::<GitLabIssue>(payload)
                .unwrap()
                .into_issue(&SourceKey {
                    provider: IssueProvider::Gitlab,
                    host: "gitlab.com".into(),
                    repository: "acme/app".into(),
                });
            assert_eq!(
                issue.activity.unwrap().comments,
                count.and_then(|n| n.as_u64())
            );
            assert_eq!(issue.activity.unwrap().commits, None);
            assert_eq!(issue.activity.unwrap().review_comments, None);
        }
    }

    #[test]
    fn encodes_nested_project_path() {
        let endpoint =
            gitlab_endpoint("gitlab.example.com", "group/team/app").expect("endpoint should build");
        assert_eq!(
            endpoint.as_str(),
            "https://gitlab.example.com/api/v4/projects/group%2Fteam%2Fapp/issues"
        );
    }

    #[test]
    fn converts_gitlab_issue() {
        let record: GitLabIssue = serde_json::from_value(serde_json::json!({
            "iid": 9,
            "title": "Improve discovery",
            "description": "Details",
            "state": "opened",
            "web_url": "https://gitlab.com/acme/app/-/issues/9",
            "author": { "username": "fox" },
            "labels": ["feature", "backend"],
            "created_at": "2026-02-01T00:00:00Z",
            "updated_at": "2026-02-03T00:00:00Z"
        }))
        .expect("valid GitLab issue");
        let issue = record.into_issue(&SourceKey {
            provider: IssueProvider::Gitlab,
            host: "gitlab.com".to_owned(),
            repository: "acme/app".to_owned(),
        });

        assert_eq!(issue.identifier, "#9");
        assert_eq!(issue.author.as_deref(), Some("fox"));
        assert_eq!(issue.labels, ["feature", "backend"]);
        assert_eq!(issue.state, "opened");
    }

    #[test]
    fn generic_tokens_are_scoped_to_the_exact_public_host() {
        assert!(is_public_gitlab_host("gitlab.com"));
        assert!(is_public_gitlab_host("GITLAB.COM"));
        assert!(!is_public_gitlab_host("gitlab.com.example.org"));
        assert!(!is_public_gitlab_host("gitlab.com:443"));
    }

    #[test]
    fn redirect_origins_include_scheme_host_and_port() {
        let origin = Url::parse("https://gitlab.com/api/v4/projects/acme/issues").unwrap();
        let same = Url::parse("https://gitlab.com/users/sign_in").unwrap();
        let other_host = Url::parse("https://example.org/users/sign_in").unwrap();
        let other_scheme = Url::parse("http://gitlab.com/users/sign_in").unwrap();

        assert!(same_origin(&origin, &same));
        assert!(!same_origin(&origin, &other_host));
        assert!(!same_origin(&origin, &other_scheme));
    }
}
