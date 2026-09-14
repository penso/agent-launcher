//! Confidential, full-only inventories. Persist only in the runtime's private cache.
use std::{
    collections::{HashMap, HashSet},
    env,
    sync::Mutex,
    time::{Duration, Instant},
};

use agent_launcher_core::{
    Issue, IssueKey, IssueProvider, PrivateAdvisoryFork, RepositoryRemote,
    SecurityAdvisoryMetadata, SecurityPreparation,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{
    Client, Method, Response, StatusCode,
    header::{ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH, LINK},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::time::{sleep, timeout};
use url::Url;

use crate::{Error, GitHubSource, IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_PAGES: usize = 100;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const INVALID: &str = "invalid security advisory response";
const CACHE_TTL: chrono::Duration = chrono::Duration::minutes(5);

#[derive(Deserialize, Serialize)]
struct InventoryCache {
    version: u8,
    source: String,
    pages: Vec<CachedPage>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CachedPage {
    url: String,
    state: String,
    etag: Option<String>,
    ids: Vec<String>,
    next: Option<String>,
}

pub struct GitHubSecuritySource {
    key: SourceKey,
    transport: Option<(Client, Url)>,
    authenticated: bool,
    poll_interval: Duration,
    prepare_timeout: Duration,
    cooldown: Mutex<Option<(Instant, DateTime<Utc>)>>,
}

impl GitHubSecuritySource {
    /// Initialization failures stay on this source, never disabling ordinary issues.
    pub fn from_remote(remote: RepositoryRemote) -> Self {
        let token = crate::github::resolve_token(
            &remote.host,
            |name| env::var(name).ok(),
            |command| command.output(),
        );
        Self::new(
            remote.host.clone(),
            remote.repository.clone(),
            token.as_deref(),
        )
        .unwrap_or(Self {
            key: SourceKey {
                provider: IssueProvider::Github,
                host: remote.host,
                repository: remote.repository,
            },
            transport: None,
            authenticated: false,
            poll_interval: POLL_INTERVAL,
            prepare_timeout: PREPARE_TIMEOUT,
            cooldown: Mutex::new(None),
        })
    }

    pub fn new(host: String, repository: String, token: Option<&str>) -> Result<Self, Error> {
        let web = Url::parse(&format!("https://{host}/"))
            .map_err(|_| Error::Security("invalid source host"))?;
        if web.host_str().is_none()
            || web.authority() != host
            || web.path() != "/"
            || web.query().is_some()
            || web.fragment().is_some()
            || !web.username().is_empty()
            || web.password().is_some()
            || !valid_name(&repository)
        {
            return Err(Error::Security("invalid source coordinates"));
        }
        let token = token.filter(|value| !value.trim().is_empty());
        let github = GitHubSource::new_with_redirect(
            host.clone(),
            repository.clone(),
            token,
            reqwest::redirect::Policy::none(),
        )
        .map_err(|_| Error::Security("could not initialize authenticated client"))?;
        let mut endpoint = github.endpoint;
        endpoint
            .path_segments_mut()
            .unwrap()
            .pop()
            .push("security-advisories");
        Ok(Self {
            key: SourceKey {
                provider: IssueProvider::Github,
                host,
                repository,
            },
            transport: Some((github.client, endpoint)),
            authenticated: token.is_some(),
            poll_interval: POLL_INTERVAL,
            prepare_timeout: PREPARE_TIMEOUT,
            cooldown: Mutex::new(None),
        })
    }

    /// Loopback-only transport override for cross-crate HTTP integration fixtures.
    #[cfg(feature = "test-support")]
    pub fn with_fixture_endpoint(mut self, endpoint: &str) -> Self {
        let endpoint = Url::parse(endpoint).expect("fixture endpoint");
        assert_eq!(endpoint.scheme(), "http");
        assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
        self.transport.as_mut().expect("fixture transport").1 = endpoint;
        self
    }

    fn transport(&self) -> Result<&(Client, Url), Error> {
        let transport = self
            .transport
            .as_ref()
            .ok_or(Error::Security("could not initialize security source"))?;
        if !self.authenticated {
            return Err(Error::SecurityAccessDenied);
        }
        Ok(transport)
    }

    // Do not use the ordinary error-body parser: even error messages may contain private data.
    async fn request(&self, method: Method, url: Url) -> Result<Response, Error> {
        self.request_conditional(method, url, None).await
    }

    async fn request_conditional(
        &self,
        method: Method,
        url: Url,
        etag: Option<&str>,
    ) -> Result<Response, Error> {
        if let Some(retry_at) = self.retry_at() {
            return Err(Error::SecurityRateLimited { retry_at });
        }
        let (client, endpoint) = self.transport()?;
        if url.origin() != endpoint.origin() {
            return Err(Error::Security("untrusted request origin"));
        }
        let post = method == Method::POST;
        let mut request = client
            .request(method, url.clone())
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(REQUEST_TIMEOUT);
        if let Some(etag) = etag {
            request = request.header(
                IF_NONE_MATCH,
                HeaderValue::from_str(etag).map_err(|_| Error::Security(INVALID))?,
            );
        }
        let response = request.send().await.map_err(|_| {
            Error::Security(if post {
                "fork request failed; outcome uncertain, recheck the advisory before retrying"
            } else {
                "request failed or timed out"
            })
        })?;
        if response.url() != &url {
            return Err(Error::Security("redirected security request rejected"));
        }
        let headers = response.headers();
        if response.status() == StatusCode::TOO_MANY_REQUESTS
            || (response.status() == StatusCode::FORBIDDEN
                && (headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0")
                    || headers.contains_key("retry-after")))
        {
            let now = Utc::now();
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.parse::<i64>()
                        .ok()
                        .and_then(chrono::Duration::try_seconds)
                        .and_then(|duration| now.checked_add_signed(duration))
                        .or_else(|| {
                            DateTime::parse_from_rfc2822(v)
                                .ok()
                                .map(|d| d.with_timezone(&Utc))
                        })
                });
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .and_then(|s| DateTime::from_timestamp(s, 0));
            let retry_at = retry_after
                .into_iter()
                .chain(reset)
                .filter(|at| *at > now)
                .max()
                .unwrap_or(now + chrono::Duration::minutes(1));
            let duration = (retry_at - now).to_std().unwrap_or(Duration::from_secs(60));
            *self.cooldown.lock().unwrap() = Some((Instant::now() + duration, retry_at));
            return Err(Error::SecurityRateLimited { retry_at });
        }
        Ok(response)
    }

    async fn advisory(&self, url: Url, id: &str) -> Result<Advisory, Error> {
        let response = self.request(Method::GET, url).await?;
        expect_status(&response, StatusCode::OK)?;
        let advisory: Advisory = read_json(response).await?;
        if advisory.ghsa_id != id || !valid_ghsa(&advisory.ghsa_id) {
            return Err(Error::Security("advisory identity mismatch"));
        }
        match advisory.state.as_str() {
            "draft" => Ok(advisory),
            "triage" => Err(Error::Security(
                "accept the report on GitHub manually before preparing a draft",
            )),
            _ => Err(Error::Security("only draft advisories can be prepared")),
        }
    }

    async fn inventory(
        &self,
        checkpoint: Option<&SyncCheckpoint>,
        cached: &[Issue],
    ) -> Result<SyncResult, Error> {
        let endpoint = &self.transport()?.1;
        if let Some(retry_at) = self.retry_at() {
            return Err(Error::SecurityRateLimited { retry_at });
        }
        let cache = self.valid_cache(checkpoint, cached);
        let now = Utc::now();
        if cache.is_some()
            && let Some(checkpoint) = checkpoint
            && checkpoint
                .last_full_at
                .is_some_and(|at| now >= at && now - at < CACHE_TTL)
        {
            return Ok(SyncResult {
                issues: cached.to_vec(),
                checkpoint: checkpoint.clone(),
                mode: SyncMode::Full,
            });
        }
        let cached_issues: HashMap<_, _> = cached
            .iter()
            .map(|issue| (issue.identifier.as_str(), issue))
            .collect();
        let mut manifest = InventoryCache {
            version: 1,
            source: self.cache_key(),
            pages: Vec::new(),
        };
        let mut issues = Vec::new();
        let mut ids = HashSet::new();
        let mut pages = 0;
        let mut remaining = MAX_BODY;
        for state in ["triage", "draft"] {
            let mut url = endpoint.clone();
            url.query_pairs_mut()
                .append_pair("state", state)
                .append_pair("per_page", "100");
            let mut visited = HashSet::new();
            loop {
                pages += 1;
                if pages > MAX_PAGES || !visited.insert(url.to_string()) {
                    return Err(Error::Security(
                        "pagination limit or repeated cursor; inventory discarded",
                    ));
                }
                let previous = cache.as_ref().and_then(|cache| {
                    cache
                        .pages
                        .iter()
                        .find(|page| page.url == url.as_str() && page.state == state)
                });
                let response = self
                    .request_conditional(
                        Method::GET,
                        url.clone(),
                        previous.and_then(|page| page.etag.as_deref()),
                    )
                    .await?;
                if response.status() != StatusCode::NOT_MODIFIED {
                    expect_status(&response, StatusCode::OK)?;
                }
                let etag = response
                    .headers()
                    .get(ETAG)
                    .map(|v| v.to_str().map(str::to_owned))
                    .transpose()
                    .map_err(|_| Error::Security(INVALID))?;
                let mut page = CachedPage {
                    url: url.to_string(),
                    state: state.into(),
                    etag,
                    ids: Vec::new(),
                    next: None,
                };
                let records = if response.status() == StatusCode::NOT_MODIFIED {
                    let previous = previous
                        .filter(|page| page.etag.is_some())
                        .ok_or(Error::Security("unexpected unvalidated 304 response"))?;
                    // A 304 on page one says nothing about later pages. Reuse its edge,
                    // then conditionally validate every reachable page before committing.
                    let next = if response.headers().contains_key(LINK) {
                        next_cursor(response.headers(), endpoint, state)?.map(|u| u.to_string())
                    } else {
                        previous.next.clone()
                    };
                    page.etag = page.etag.or_else(|| previous.etag.clone());
                    page.next = next;
                    previous
                        .ids
                        .iter()
                        .map(|id| (*cached_issues[id.as_str()]).clone())
                        .collect::<Vec<_>>()
                } else {
                    page.next =
                        next_cursor(response.headers(), endpoint, state)?.map(|u| u.to_string());
                    let records: Vec<Advisory> =
                        read_json_bounded(response, &mut remaining).await?;
                    if records.len() > 100 {
                        return Err(Error::Security("advisory page exceeds record limit"));
                    }
                    let mut active = Vec::new();
                    for advisory in records {
                        if !valid_ghsa(&advisory.ghsa_id) {
                            return Err(Error::Security(INVALID));
                        }
                        match advisory.state.as_str() {
                            "triage" | "draft" => {
                                active.push(advisory.into_issue(&self.key));
                            },
                            "published" | "closed" | "withdrawn" => {},
                            _ => return Err(Error::Security(INVALID)),
                        }
                    }
                    active
                };
                for issue in records {
                    page.ids.push(issue.identifier.clone());
                    if ids.insert(issue.identifier.clone()) {
                        issues.push(issue);
                    }
                }
                let next = page
                    .next
                    .as_deref()
                    .map(Url::parse)
                    .transpose()
                    .map_err(|_| Error::Security(INVALID))?;
                manifest.pages.push(page);
                match next {
                    Some(next) => url = next,
                    None => break,
                }
            }
        }
        let encoded = serde_json::to_string(&manifest).map_err(|_| Error::Security(INVALID))?;
        if encoded.len() > MAX_BODY {
            return Err(Error::Security("security checkpoint exceeds size limit"));
        }
        Ok(SyncResult {
            issues,
            checkpoint: SyncCheckpoint {
                etag: Some(encoded),
                last_full_at: Some(Utc::now()),
                ..SyncCheckpoint::default()
            },
            mode: SyncMode::Full,
        })
    }

    fn valid_cache(
        &self,
        checkpoint: Option<&SyncCheckpoint>,
        cached: &[Issue],
    ) -> Option<InventoryCache> {
        let checkpoint = checkpoint?;
        // A missing freshness timestamp requests HTTP verification while keeping
        // the structurally validated page manifest available for conditional GETs.
        let encoded = checkpoint.etag.as_deref().filter(|s| s.len() <= MAX_BODY)?;
        let cache: InventoryCache = serde_json::from_str(encoded).ok()?;
        if cache.version != 1 || cache.source != self.cache_key() || cache.pages.len() > MAX_PAGES {
            return None;
        }
        let endpoint = &self.transport.as_ref()?.1;
        let mut expected_ids = HashSet::new();
        let mut visited = HashSet::new();
        for state in ["triage", "draft"] {
            let mut url = endpoint.clone();
            url.query_pairs_mut()
                .append_pair("state", state)
                .append_pair("per_page", "100");
            loop {
                if !visited.insert(url.to_string()) {
                    return None;
                }
                let page = cache
                    .pages
                    .iter()
                    .find(|p| p.url == url.as_str() && p.state == state)?;
                if page.ids.len() > 100
                    || page
                        .etag
                        .as_ref()
                        .is_some_and(|e| e.len() > 16384 || HeaderValue::from_str(e).is_err())
                {
                    return None;
                }
                for id in &page.ids {
                    if !valid_ghsa(id) {
                        return None;
                    }
                    expected_ids.insert(id.as_str());
                }
                let Some(next) = &page.next else {
                    break;
                };
                let mut headers = HeaderMap::new();
                headers.insert(
                    LINK,
                    HeaderValue::from_str(&format!("<{next}>; rel=\"next\"")).ok()?,
                );
                url = next_cursor(&headers, endpoint, state).ok()??;
            }
        }
        if visited.len() != cache.pages.len() || expected_ids.len() != cached.len() {
            return None;
        }
        for issue in cached {
            if issue.key.provider != self.key.provider
                || issue.key.host != self.key.host
                || issue.key.repository != self.key.repository
                || issue.key.native_id != format!("advisory/{}", issue.identifier)
                || issue
                    .security_advisory
                    .as_ref()
                    .is_none_or(|m| m.ghsa_id != issue.identifier)
                || !matches!(issue.state.as_str(), "triage" | "draft")
                || !expected_ids.remove(issue.identifier.as_str())
            {
                return None;
            }
        }
        Some(cache)
    }

    async fn prepare(
        &self,
        key: &IssueKey,
        create_fork: bool,
    ) -> Result<SecurityPreparation, Error> {
        let id = key
            .native_id
            .strip_prefix("advisory/")
            .filter(|id| valid_ghsa(id))
            .ok_or(Error::Security("invalid advisory key"))?;
        if key.provider != self.key.provider
            || key.host != self.key.host
            || key.repository != self.key.repository
        {
            return Err(Error::Security("advisory key does not match source"));
        }
        let endpoint = &self.transport()?.1;
        let mut detail = endpoint.clone();
        detail.path_segments_mut().unwrap().push(id);
        let mut advisory = self.advisory(detail.clone(), id).await?;
        let mut created = false;
        if advisory.private_fork.is_none() {
            if !create_fork {
                return Err(Error::Security(
                    "no private fork; explicit fork-creation confirmation required",
                ));
            }
            let mut forks = detail.clone();
            forks.path_segments_mut().unwrap().push("forks");
            // Exactly one POST. Ignore the payload (which can contain a temporary clone token).
            let response = self.request(Method::POST, forks).await?;
            expect_status(&response, StatusCode::ACCEPTED)?;
            created = true;
            advisory = self.advisory(detail.clone(), id).await?;
        }
        loop {
            if let Some(linked) = &advisory.private_fork {
                if linked.id == 0
                    || !valid_name(&linked.full_name)
                    || linked.full_name.eq_ignore_ascii_case(&self.key.repository)
                    || !linked.private
                    || !valid_web_url(&linked.html_url, &self.key.host, &linked.full_name)
                {
                    return Err(Error::Security("invalid advisory-linked private fork"));
                }
                let mut repo_url = endpoint.clone();
                repo_url
                    .path_segments_mut()
                    .unwrap()
                    .pop()
                    .pop()
                    .pop()
                    .extend(linked.full_name.split('/'));
                let response = self.request(Method::GET, repo_url).await?;
                // A just-created fork can be temporarily inaccessible. All other status errors fail closed.
                if response.status() != StatusCode::ACCEPTED
                    && !(created && response.status() == StatusCode::NOT_FOUND)
                {
                    expect_status(&response, StatusCode::OK)?;
                    let repo: ForkRepository = read_json(response).await?;
                    if repo.linked.id != linked.id
                        || repo.linked.full_name != linked.full_name
                        || !repo.linked.private
                        || repo.archived
                        || repo.disabled
                        || !valid_web_url(&repo.linked.html_url, &self.key.host, &linked.full_name)
                        || !valid_branch(&repo.default_branch)
                    {
                        return Err(Error::Security(
                            "fork identity, privacy, availability or default branch verification failed",
                        ));
                    }
                    if !repo.permissions.is_some_and(|permissions| permissions.push) {
                        return Err(Error::Security(
                            "push permission on the private fork is required",
                        ));
                    }
                    let fork = PrivateAdvisoryFork {
                        id: linked.id,
                        host: self.key.host.clone(),
                        full_name: linked.full_name.clone(),
                        default_branch: repo.default_branch,
                    };
                    return Ok(SecurityPreparation {
                        issue: advisory.into_issue(&self.key),
                        fork,
                    });
                }
            }
            sleep(self.poll_interval).await;
            advisory = self.advisory(detail.clone(), id).await?;
        }
    }
}

#[async_trait]
impl IssueSource for GitHubSecuritySource {
    fn source_key(&self) -> &SourceKey {
        &self.key
    }

    fn is_confidential(&self) -> bool {
        true
    }

    fn cache_key(&self) -> String {
        format!("security:{}", self.key.canonical())
    }

    async fn sync(&self, _checkpoint: Option<&SyncCheckpoint>) -> Result<SyncResult, Error> {
        self.sync_with_cache(None, &[]).await
    }

    fn retry_at(&self) -> Option<DateTime<Utc>> {
        self.cooldown
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(until, _)| *until > Instant::now())
            .map(|(_, at)| *at)
    }

    async fn sync_with_cache(
        &self,
        checkpoint: Option<&SyncCheckpoint>,
        cached: &[Issue],
    ) -> Result<SyncResult, Error> {
        timeout(SYNC_TIMEOUT, self.inventory(checkpoint, cached))
            .await
            .map_err(|_| Error::Security("inventory timed out; partial results discarded"))?
    }

    async fn prepare_security(
        &self,
        key: &IssueKey,
        create_fork: bool,
    ) -> Result<SecurityPreparation, Error> {
        timeout(self.prepare_timeout, self.prepare(key, create_fork))
            .await
            .map_err(|_| {
                Error::Security(
                    "private fork preparation timed out; recheck the advisory before retrying",
                )
            })?
    }
}

fn expect_status(response: &Response, expected: StatusCode) -> Result<(), Error> {
    if response.status() == expected {
        return Ok(());
    }
    if matches!(response.status().as_u16(), 401 | 403 | 404) {
        return Err(Error::SecurityAccessDenied);
    }
    Err(Error::Security(match response.status().as_u16() {
        422 => "GitHub rejected the security request (422); recheck the advisory on GitHub",
        300..=399 => "security request redirect rejected",
        _ => "unexpected security API status",
    }))
}

async fn read_json<T: DeserializeOwned>(response: Response) -> Result<T, Error> {
    let mut remaining = MAX_BODY;
    read_json_bounded(response, &mut remaining).await
}

async fn read_json_bounded<T: DeserializeOwned>(
    mut response: Response,
    remaining: &mut usize,
) -> Result<T, Error> {
    timeout(REQUEST_TIMEOUT, async {
        if response
            .content_length()
            .is_some_and(|length| length > *remaining as u64)
        {
            return Err(Error::Security("security response exceeds size limit"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Security("security response read failed"))?
        {
            if chunk.len() > *remaining {
                return Err(Error::Security("security response exceeds size limit"));
            }
            *remaining -= chunk.len();
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| Error::Security(INVALID))
    })
    .await
    .map_err(|_| Error::Security("security response timed out"))?
}

fn next_cursor(headers: &HeaderMap, endpoint: &Url, state: &str) -> Result<Option<Url>, Error> {
    let invalid = || Error::Security("untrusted or malformed advisory pagination link");
    let mut next = None;
    for header in headers.get_all(LINK) {
        let header = header.to_str().map_err(|_| invalid())?;
        if header.len() > 16 * 1024 {
            return Err(invalid());
        }
        for link in header.split(',') {
            let mut parts = link.split(';');
            let target = parts.next().ok_or_else(invalid)?.trim();
            if !parts.any(|part| {
                part.trim().strip_prefix("rel=").is_some_and(|rel| {
                    rel.trim_matches('"')
                        .split_whitespace()
                        .any(|rel| rel == "next")
                })
            }) {
                continue;
            }
            let target = target
                .strip_prefix('<')
                .and_then(|value| value.strip_suffix('>'))
                .ok_or_else(invalid)?;
            let url = endpoint.join(target).map_err(|_| invalid())?;
            if next.is_some()
                || url.origin() != endpoint.origin()
                || url.path() != endpoint.path()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid());
            }
            let mut keys = HashSet::new();
            let mut cursor = false;
            let mut has_state = false;
            for (key, value) in url.query_pairs() {
                if !keys.insert(key.to_string()) {
                    return Err(invalid());
                }
                match key.as_ref() {
                    "state" if value == state => has_state = true,
                    "per_page" if value == "100" => {},
                    "before" | "after" if !value.is_empty() && value.len() <= 4096 && !cursor => {
                        cursor = true
                    },
                    "sort" if value == "created" => {},
                    "direction" if value == "desc" => {},
                    "state" | "per_page" | "before" | "after" => return Err(invalid()),
                    // Follow only server-supplied next links, never synthesize page hops.
                    // Unknown pagination parameters are safe on this exact origin/path.
                    _ if !value.is_empty() && value.len() <= 4096 => {},
                    _ => return Err(invalid()),
                }
            }
            if !has_state {
                return Err(invalid());
            }
            next = Some(url);
        }
    }
    Ok(next)
}

fn valid_ghsa(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("GHSA-") else {
        return false;
    };
    let parts: Vec<_> = rest.split('-').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            part.len() == 4
                && part
                    .bytes()
                    .all(|byte| b"23456789cfghjmpqrvwx".contains(&byte))
        })
}

fn valid_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 100
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.contains("..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
}

fn valid_web_url(value: &str, host: &str, name: &str) -> bool {
    // Compare to a synthesized URL, not an API-provided clone URL or hostname guess.
    Url::parse(value)
        .ok()
        .zip(Url::parse(&format!("https://{host}/{name}")).ok())
        .is_some_and(|(actual, expected)| actual == expected)
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 1024
        && branch != "@"
        && !branch.starts_with('-')
        && !branch.ends_with('.')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch
            .bytes()
            .any(|byte| byte <= 32 || byte == 127 || b"~^:?*[\\".contains(&byte))
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

// Deliberately no Debug, clone tokens, collaborators, or untrusted clone URLs.
#[derive(Deserialize)]
struct Advisory {
    ghsa_id: String,
    cve_id: Option<String>,
    severity: Option<String>,
    summary: String,
    description: Option<String>,
    state: String,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "Option::deserialize")]
    private_fork: Option<LinkedFork>,
}

#[derive(Deserialize)]
struct LinkedFork {
    id: u64,
    full_name: String,
    private: bool,
    html_url: String,
}

#[derive(Deserialize)]
struct ForkRepository {
    #[serde(flatten)]
    linked: LinkedFork,
    default_branch: String,
    archived: bool,
    disabled: bool,
    permissions: Option<Permissions>,
}

#[derive(Deserialize)]
struct Permissions {
    push: bool,
}

impl Advisory {
    fn into_issue(self, source: &SourceKey) -> Issue {
        Issue {
            key: IssueKey {
                provider: IssueProvider::Github,
                host: source.host.clone(),
                repository: source.repository.clone(),
                native_id: format!("advisory/{}", self.ghsa_id),
            },
            identifier: self.ghsa_id.clone(),
            url: Some(format!(
                "https://{}/{}/security/advisories/{}",
                source.host, source.repository, self.ghsa_id
            )),
            security_advisory: Some(SecurityAdvisoryMetadata {
                ghsa_id: self.ghsa_id,
                cve_id: self.cve_id,
                severity: self.severity,
            }),
            title: self.summary,
            description: self.description,
            state: self.state,
            created_at: self.created_at,
            updated_at: self.updated_at,
            pull_request: None,
            activity: None,
            author: None,
            labels: Vec::new(),
            parent_id: None,
            blocked_by: Vec::new(),
            priority: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const ID: &str = "GHSA-2345-cfgh-jmpq";
    const LIST: &str = "/repos/acme/app/security-advisories";
    const DETAIL: &str = "/repos/acme/app/security-advisories/GHSA-2345-cfgh-jmpq";
    const REPO: &str = "/repos/acme/app-ghsa";

    fn advisory(state: &str, fork: bool) -> Value {
        json!({
            "ghsa_id": ID, "summary": "Synthetic advisory", "description": "Fixture only",
            "state": state, "cve_id": "CVE-2026-12345", "severity": "high",
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z",
            "private_fork": if fork { repository() } else { Value::Null },
            "collaborating_users": [{"login": "unused-private-user"}],
            "temp_clone_token": "unused-secret"
        })
    }

    fn repository() -> Value {
        json!({"id": 42, "full_name": "acme/app-ghsa", "private": true,
            "html_url": "https://github.com/acme/app-ghsa", "default_branch": "main",
            "archived": false, "disabled": false, "permissions": {"push": true},
            "clone_url": "https://unused-secret@evil.example/ignore", "temp_clone_token": "unused-secret"})
    }

    fn key() -> IssueKey {
        IssueKey {
            provider: IssueProvider::Github,
            host: "github.com".into(),
            repository: "acme/app".into(),
            native_id: format!("advisory/{ID}"),
        }
    }

    struct Reply {
        request: String,
        status: u16,
        headers: String,
        body: Value,
        conditional: Option<bool>,
    }

    fn get(path: &str, status: u16, body: Value) -> Reply {
        Reply {
            request: format!("GET {path} "),
            status,
            headers: String::new(),
            body,
            conditional: None,
        }
    }

    fn post() -> Reply {
        Reply {
            request: format!("POST {DETAIL}/forks "),
            status: 202,
            headers: String::new(),
            body: repository(),
            conditional: None,
        }
    }

    async fn mock(replies: Vec<Reply>) -> (GitHubSecuritySource, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut source = GitHubSecuritySource::new(
            "github.com".into(),
            "acme/app".into(),
            Some("fixture-token"),
        )
        .unwrap();
        source.transport.as_mut().unwrap().1 = Url::parse(&format!("{base}{LIST}")).unwrap();
        source.poll_interval = Duration::from_millis(1);
        source.prepare_timeout = Duration::from_secs(2);
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = timeout(Duration::from_secs(3), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 1024];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 16 * 1024);
                }
                let request = String::from_utf8(request).unwrap();
                if let Some(expected) = reply.conditional {
                    assert_eq!(
                        request
                            .to_ascii_lowercase()
                            .contains("if-none-match: \"fixture\"\r\n"),
                        expected
                    );
                }
                if reply.status == 304 && reply.conditional != Some(false) {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("if-none-match: \"fixture\"\r\n")
                    );
                }
                assert!(
                    request.starts_with(&reply.request),
                    "unexpected fixture request"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer fixture-token\r\n")
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("x-github-api-version: 2022-11-28\r\n")
                );
                // Status 0 simulates an ambiguous transport failure, without retrying the POST.
                if reply.status == 0 {
                    continue;
                }
                let body = if reply.status == 304 {
                    String::new()
                } else {
                    reply.body.to_string()
                };
                let headers = reply.headers.replace("{base}", &base);
                let written = socket.write_all(format!("HTTP/1.1 {} Test\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", reply.status, body.len()).as_bytes()).await;
                // Oversized responses are rejected from headers without consuming the body.
                if body.len() <= MAX_BODY {
                    written.unwrap();
                }
            }
        });
        (source, task)
    }

    fn numbered(state: &str, number: usize) -> Value {
        let alphabet = b"23456789cfghjmpqrvwx";
        let suffix: String = (0..4)
            .rev()
            .map(|power| alphabet[(number / alphabet.len().pow(power)) % alphabet.len()] as char)
            .collect();
        let mut value = advisory(state, false);
        value["ghsa_id"] = json!(format!("GHSA-2345-cfgh-{suffix}"));
        value
    }

    fn expired(checkpoint: &SyncCheckpoint) -> SyncCheckpoint {
        SyncCheckpoint {
            last_full_at: Some(Utc::now() - CACHE_TTL - chrono::Duration::seconds(1)),
            ..checkpoint.clone()
        }
    }

    fn page(state: &str, later: bool, status: u16, body: Value) -> Reply {
        let mut reply = get(
            &format!(
                "{LIST}?state={state}&per_page=100{}",
                if later {
                    "&after=next"
                } else {
                    ""
                }
            ),
            status,
            body,
        );
        reply.headers = "ETag: \"fixture\"\r\n".into();
        if !later && status == 200 {
            reply.headers.push_str(&format!(
                "Link: <{{base}}{LIST}?state={state}&per_page=100&after=next>; rel=\"next\"\r\n"
            ));
        }
        reply
    }

    #[tokio::test]
    async fn more_than_100_per_state_across_two_pages_deduplicates_without_20_cap() {
        let mut replies = Vec::new();
        for (state, offset) in [("triage", 0), ("draft", 105)] {
            replies.push(page(
                state,
                false,
                200,
                json!(
                    (offset..offset + 100)
                        .map(|n| numbered(state, n))
                        .collect::<Vec<_>>()
                ),
            ));
            let mut last: Vec<_> = (offset + 100..offset + 105)
                .map(|n| numbered(state, n))
                .collect();
            last.push(numbered(state, offset));
            replies.push(page(state, true, 200, json!(last)));
        }
        let (source, server) = mock(replies).await;
        let result = source.sync(None).await.unwrap();
        server.await.unwrap();
        assert_eq!(result.issues.len(), 210);
        for state in ["triage", "draft"] {
            assert_eq!(
                result.issues.iter().filter(|i| i.state == state).count(),
                105
            );
        }
        assert!(
            source
                .valid_cache(Some(&result.checkpoint), &result.issues)
                .is_some()
        );
        // Reusing a fresh cache must not contact the now-closed fixture server or
        // advance the observation time, including after source reconstruction.
        let mut restarted = GitHubSecuritySource::new(
            "github.com".into(),
            "acme/app".into(),
            Some("fixture-token"),
        )
        .unwrap();
        restarted.transport = source.transport.clone();
        assert_eq!(
            restarted
                .sync_with_cache(Some(&result.checkpoint), &result.issues)
                .await
                .unwrap(),
            result
        );
        assert!(restarted.retry_at().is_none());
    }

    #[tokio::test]
    async fn conditional_refresh_validates_later_pages_and_failed_refresh_preserves_cache() {
        let mut replies = Vec::new();
        for (state, offset) in [("triage", 0), ("draft", 2)] {
            for later in [false, true] {
                replies.push(page(
                    state,
                    later,
                    200,
                    json!([numbered(state, offset + usize::from(later))]),
                ));
            }
        }
        for state in ["triage", "draft"] {
            for later in [false, true] {
                replies.push(page(state, later, 304, Value::Null));
            }
        }
        for state in ["triage", "draft"] {
            replies.push(page(state, false, 304, Value::Null));
            if state == "draft" {
                let mut changed = page(state, true, 200, json!([numbered(state, 10)]));
                changed.conditional = Some(true);
                replies.push(changed);
            } else {
                replies.push(page(state, true, 304, Value::Null));
            }
        }
        replies.push(page("triage", false, 304, Value::Null));
        replies.push(page(
            "triage",
            true,
            500,
            json!({"message": "unused-secret"}),
        ));
        let (source, server) = mock(replies).await;
        let initial = source.sync(None).await.unwrap();
        let unchanged = source
            .sync_with_cache(Some(&expired(&initial.checkpoint)), &initial.issues)
            .await
            .unwrap();
        assert_eq!(unchanged.issues, initial.issues);
        assert_eq!(unchanged.mode, SyncMode::Full);
        assert!(unchanged.checkpoint.last_full_at >= initial.checkpoint.last_full_at);
        let changed = source
            .sync_with_cache(Some(&expired(&unchanged.checkpoint)), &unchanged.issues)
            .await
            .unwrap();
        assert_eq!(changed.issues.len(), 4);
        assert_eq!(
            changed.issues[3].identifier,
            numbered("draft", 10)["ghsa_id"].as_str().unwrap()
        );
        let checkpoint = expired(&changed.checkpoint);
        let saved = checkpoint.clone();
        assert!(matches!(
            source
                .sync_with_cache(Some(&checkpoint), &changed.issues)
                .await,
            Err(Error::Security(_))
        ));
        assert_eq!(checkpoint, saved);
        assert!(
            source
                .valid_cache(Some(&checkpoint), &changed.issues)
                .is_some()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn changed_next_edges_remove_obsolete_pages_and_records() {
        let (source, server) = mock(vec![
            page("triage", false, 200, json!([numbered("triage", 0)])),
            page("triage", true, 200, json!([numbered("triage", 1)])),
            get(&format!("{LIST}?state=draft&per_page=100"), 200, json!([])),
            get(&format!("{LIST}?state=triage&per_page=100"), 200, json!([])),
            get(&format!("{LIST}?state=draft&per_page=100"), 200, json!([])),
        ])
        .await;
        let initial = source.sync(None).await.unwrap();
        assert_eq!(initial.issues.len(), 2);
        let removed = source
            .sync_with_cache(Some(&expired(&initial.checkpoint)), &initial.issues)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(removed.issues.is_empty());
        assert_eq!(removed.mode, SyncMode::Full);
        assert_eq!(
            source
                .valid_cache(Some(&removed.checkpoint), &removed.issues)
                .unwrap()
                .pages
                .len(),
            2
        );
        assert_eq!(
            source
                .sync_with_cache(Some(&removed.checkpoint), &[])
                .await
                .unwrap(),
            removed
        );
    }

    #[tokio::test]
    async fn incomplete_cache_refetches_unconditionally_and_unsolicited_304_fails() {
        let mut replies = vec![
            get(
                &format!("{LIST}?state=triage&per_page=100"),
                200,
                json!([advisory("triage", false)]),
            ),
            get(&format!("{LIST}?state=draft&per_page=100"), 200, json!([])),
        ];
        for reply in &mut replies {
            reply.headers = "ETag: \"fixture\"\r\n".into();
            reply.conditional = Some(false);
        }
        let mut refetch = get(
            &format!("{LIST}?state=triage&per_page=100"),
            200,
            json!([advisory("triage", false)]),
        );
        refetch.conditional = Some(false);
        replies.push(refetch);
        replies.push(get(
            &format!("{LIST}?state=draft&per_page=100"),
            200,
            json!([]),
        ));
        let (source, server) = mock(replies).await;
        let result = source.sync(None).await.unwrap();
        assert_eq!(
            source
                .sync_with_cache(Some(&result.checkpoint), &[])
                .await
                .unwrap()
                .issues,
            result.issues
        );
        server.await.unwrap();
        assert!(source.valid_cache(Some(&result.checkpoint), &[]).is_none());
        let mut malicious = result.checkpoint.clone();
        let mut manifest: InventoryCache =
            serde_json::from_str(result.checkpoint.etag.as_ref().unwrap()).unwrap();
        manifest.pages[0].next = Some("https://evil.example/?state=triage&after=x".into());
        malicious.etag = Some(serde_json::to_string(&manifest).unwrap());
        assert!(
            source
                .valid_cache(Some(&malicious), &result.issues)
                .is_none()
        );
        // A server's 304 is never evidence for an empty inventory without validators.
        let mut reply = get(
            &format!("{LIST}?state=triage&per_page=100"),
            304,
            Value::Null,
        );
        reply.conditional = Some(false);
        let (other, server) = mock(vec![reply]).await;
        assert!(
            other
                .sync_with_cache(Some(&result.checkpoint), &[])
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limits_are_sanitized_and_block_forced_requests_but_access_errors_are_distinct() {
        for status in [429, 403] {
            let mut reply = get(
                &format!("{LIST}?state=triage&per_page=100"),
                status,
                json!({"message": "unused-secret"}),
            );
            reply.headers = "Retry-After: 120\r\nX-RateLimit-Remaining: 0\r\n".into();
            let (source, server) = mock(vec![reply]).await;
            let before = Utc::now();
            assert!(matches!(
                source.sync(None).await,
                Err(Error::SecurityRateLimited { .. })
            ));
            server.await.unwrap();
            assert!(source.retry_at().unwrap() >= before + chrono::Duration::seconds(120));
            assert!(matches!(
                source.sync(None).await,
                Err(Error::SecurityRateLimited { .. })
            ));
        }
        for status in [401, 403, 404] {
            let (source, server) = mock(vec![get(
                &format!("{LIST}?state=triage&per_page=100"),
                status,
                json!({"message": "unused-secret"}),
            )])
            .await;
            assert!(matches!(
                source.sync(None).await,
                Err(Error::SecurityAccessDenied)
            ));
            assert!(source.retry_at().is_none());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn full_inventory_uses_both_states_and_cursor_links_without_secrets() {
        let mut first = get(
            &format!("{LIST}?state=triage&per_page=100"),
            200,
            json!([advisory("triage", false)]),
        );
        first.headers = format!(
            "Link: <{{base}}{LIST}?state=triage&per_page=100&after=cursor%2B1>; rel=\"next\"\r\n"
        );
        let mut second_advisory = advisory("draft", false);
        second_advisory["ghsa_id"] = json!("GHSA-6789-rvwx-2345");
        let (source, server) = mock(vec![
            first,
            get(
                &format!("{LIST}?state=triage&per_page=100&after=cursor%2B1"),
                200,
                json!([]),
            ),
            get(
                &format!("{LIST}?state=draft&per_page=100"),
                200,
                json!([
                    second_advisory,
                    advisory("published", false),
                    advisory("closed", false),
                    advisory("withdrawn", false)
                ]),
            ),
        ])
        .await;
        let result = source.sync(Some(&SyncCheckpoint::default())).await.unwrap();
        server.await.unwrap();
        assert!(source.is_confidential());
        assert_eq!(source.cache_key(), "security:github:github.com:acme/app");
        assert_eq!(result.mode, SyncMode::Full);
        assert!(result.checkpoint.last_full_at.is_some());
        assert!(result.checkpoint.etag.is_some());
        assert_eq!(result.issues.len(), 2);
        let issue = &result.issues[0];
        assert_eq!(issue.key, key());
        assert!(issue.is_security_advisory());
        assert!(issue.pull_request.is_none());
        assert_eq!(
            issue
                .security_advisory
                .as_ref()
                .unwrap()
                .severity
                .as_deref(),
            Some("high")
        );
        let serialized = serde_json::to_string(&result).unwrap();
        for secret in [
            "unused-secret",
            "unused-private-user",
            "private_fork",
            "clone_url",
        ] {
            assert!(!serialized.contains(secret));
        }
    }

    #[tokio::test]
    async fn partial_failures_and_missing_state_never_return_an_inventory() {
        let mut missing = advisory("draft", false);
        missing.as_object_mut().unwrap().remove("state");
        for (status, body) in [
            (401, json!({"message": "unused-secret"})),
            (403, json!({})),
            (404, json!({})),
            (500, json!({})),
            (200, json!([missing])),
        ] {
            let (source, server) = mock(vec![
                get(
                    &format!("{LIST}?state=triage&per_page=100"),
                    200,
                    json!([advisory("triage", false)]),
                ),
                get(&format!("{LIST}?state=draft&per_page=100"), status, body),
            ])
            .await;
            let error = source.sync(None).await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("unused-secret"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn authentication_is_required_and_coordinates_are_validated() {
        let source =
            GitHubSecuritySource::new("github.com".into(), "acme/app".into(), None).unwrap();
        assert!(
            source
                .sync(None)
                .await
                .unwrap_err()
                .to_string()
                .contains("authentication required")
        );
        assert!(source.prepare_security(&key(), true).await.is_err());
        for host in [
            "github.com/evil",
            "token@github.com",
            "github.com?secret",
            "github.com#secret",
        ] {
            assert!(GitHubSecuritySource::new(host.into(), "acme/app".into(), Some("x")).is_err());
        }
        let enterprise =
            GitHubSecuritySource::new("github.example:8443".into(), "acme/app".into(), Some("x"))
                .unwrap();
        assert_eq!(
            enterprise.transport.unwrap().1.as_str(),
            "https://github.example:8443/api/v3/repos/acme/app/security-advisories"
        );
    }

    #[test]
    fn cursor_validator_accepts_before_and_rejects_untrusted_links() {
        let endpoint =
            Url::parse("https://api.github.com/repos/acme/app/security-advisories").unwrap();
        let mut headers = HeaderMap::new();
        for cursor in ["before", "after", "page", "future_cursor"] {
            headers.insert(
                LINK,
                format!("<{endpoint}?state=draft&per_page=100&{cursor}=abc>; rel=\"next\"")
                    .parse()
                    .unwrap(),
            );
            assert!(next_cursor(&headers, &endpoint, "draft").unwrap().is_some());
        }
        for url in [
            "https://evil.example/repos/acme/app/security-advisories?state=draft&after=x",
            "https://api.github.com/repos/other/app/security-advisories?state=draft&after=x",
            "http://api.github.com/repos/acme/app/security-advisories?state=draft&after=x",
            "?state=published&after=x",
            "?after=x",
            "?state=draft&after=x&before=y",
            "?state=draft&after=x&after=y",
            "?state=draft&after=x#fragment",
            "?state=draft&per_page=1000&after=x",
        ] {
            headers.insert(LINK, format!("<{url}>; rel=\"next\"").parse().unwrap());
            assert!(next_cursor(&headers, &endpoint, "draft").is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn prepares_existing_fork_without_mutation_and_discards_api_secrets() {
        let (source, server) = mock(vec![
            get(DETAIL, 200, advisory("draft", true)),
            get(REPO, 200, repository()),
        ])
        .await;
        let prepared = source.prepare_security(&key(), false).await.unwrap();
        server.await.unwrap();
        assert_eq!(prepared.fork, PrivateAdvisoryFork {
            id: 42,
            host: "github.com".into(),
            full_name: "acme/app-ghsa".into(),
            default_branch: "main".into()
        });
        assert_eq!(prepared.issue.key, key());
        assert!(!format!("{prepared:?}").contains("unused-secret"));
    }

    #[tokio::test]
    async fn posts_once_then_polls_advisory_and_pending_repository() {
        let (source, server) = mock(vec![
            get(DETAIL, 200, advisory("draft", false)),
            post(),
            get(DETAIL, 200, advisory("draft", false)),
            get(DETAIL, 200, advisory("draft", true)),
            get(REPO, 202, json!({})),
            get(DETAIL, 200, advisory("draft", true)),
            get(REPO, 404, json!({})),
            get(DETAIL, 200, advisory("draft", true)),
            get(REPO, 200, repository()),
        ])
        .await;
        assert!(source.prepare_security(&key(), true).await.is_ok());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn preparation_rejects_states_malformed_identity_and_unconfirmed_creation() {
        let mut wrong_id = advisory("draft", true);
        wrong_id["ghsa_id"] = json!("GHSA-6789-rvwx-2345");
        let mut missing_fork = advisory("draft", false);
        missing_fork.as_object_mut().unwrap().remove("private_fork");
        let mut missing_state = advisory("draft", true);
        missing_state.as_object_mut().unwrap().remove("state");
        for body in [
            advisory("triage", false),
            advisory("published", true),
            advisory("closed", true),
            advisory("withdrawn", true),
            wrong_id,
            missing_fork,
            missing_state,
        ] {
            let (source, server) = mock(vec![get(DETAIL, 200, body)]).await;
            assert!(source.prepare_security(&key(), true).await.is_err());
            server.await.unwrap();
        }
        let (source, server) = mock(vec![get(DETAIL, 200, advisory("draft", false))]).await;
        assert!(
            source
                .prepare_security(&key(), false)
                .await
                .unwrap_err()
                .to_string()
                .contains("confirmation")
        );
        server.await.unwrap();
        for bad in [
            IssueKey {
                host: "other.example".into(),
                ..key()
            },
            IssueKey {
                repository: "other/repo".into(),
                ..key()
            },
            IssueKey {
                native_id: "advisory/../../evil".into(),
                ..key()
            },
            IssueKey {
                provider: IssueProvider::Gitlab,
                ..key()
            },
        ] {
            assert!(
                source
                    .prepare_security(&bad, true)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("key")
            );
        }
    }

    #[tokio::test]
    async fn rejects_untrusted_linked_forks_before_fetching_the_repository() {
        for (field, value) in [
            ("id", json!(0)),
            ("private", json!(false)),
            ("full_name", json!("acme/app")),
            ("full_name", json!("acme/../app")),
            ("full_name", json!("acme/app;touch")),
            ("html_url", json!("https://evil.example/acme/app-ghsa")),
        ] {
            let mut body = advisory("draft", true);
            body["private_fork"][field] = value;
            let (source, server) = mock(vec![get(DETAIL, 200, body)]).await;
            assert!(source.prepare_security(&key(), true).await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_public_wrong_identity_read_only_or_unusable_repositories() {
        for (field, value) in [
            ("id", json!(43)),
            ("private", json!(false)),
            ("full_name", json!("acme/other")),
            ("html_url", json!("https://evil.example/acme/app-ghsa")),
            ("archived", json!(true)),
            ("disabled", json!(true)),
            ("permissions", json!({"push": false})),
            ("permissions", Value::Null),
            ("default_branch", json!("../main")),
        ] {
            let mut repo = repository();
            repo[field] = value;
            let (source, server) = mock(vec![
                get(DETAIL, 200, advisory("draft", true)),
                get(REPO, 200, repo),
            ])
            .await;
            assert!(source.prepare_security(&key(), false).await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn permission_errors_are_sanitized_at_every_endpoint() {
        for status in [401, 403, 404, 422, 500] {
            for stage in 0..3 {
                let failure = json!({"message": "unused-secret"});
                let replies = match stage {
                    0 => vec![get(DETAIL, status, failure)],
                    1 => {
                        let mut reply = post();
                        reply.status = status;
                        reply.body = failure;
                        vec![get(DETAIL, 200, advisory("draft", false)), reply]
                    },
                    _ => vec![
                        get(DETAIL, 200, advisory("draft", true)),
                        get(REPO, status, failure),
                    ],
                };
                let (source, server) = mock(replies).await;
                let error = source.prepare_security(&key(), true).await.unwrap_err();
                assert!(!format!("{error:?} {error}").contains("unused-secret"));
                server.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn ambiguous_post_is_not_retried_and_next_dispatch_refetches_first() {
        let mut failed = post();
        failed.status = 0;
        let (source, server) = mock(vec![
            get(DETAIL, 200, advisory("draft", false)),
            failed,
            get(DETAIL, 200, advisory("draft", true)),
            get(REPO, 200, repository()),
        ])
        .await;
        assert!(
            source
                .prepare_security(&key(), true)
                .await
                .unwrap_err()
                .to_string()
                .contains("uncertain")
        );
        assert!(source.prepare_security(&key(), true).await.is_ok());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn preparation_deadline_is_bounded() {
        let (mut source, server) = mock(vec![
            get(DETAIL, 200, advisory("draft", false)),
            post(),
            get(DETAIL, 200, advisory("draft", false)),
        ])
        .await;
        source.poll_interval = Duration::from_secs(1);
        source.prepare_timeout = Duration::from_millis(100);
        assert!(
            source
                .prepare_security(&key(), true)
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_and_oversized_responses_fail_closed() {
        for headers in [
            "Location: https://evil.example/private\r\n".to_owned(),
            format!("Location: {{base}}{DETAIL}/other\r\n"),
        ] {
            let mut reply = get(DETAIL, 307, json!({}));
            reply.headers = headers;
            let (source, server) = mock(vec![reply]).await;
            assert!(
                source
                    .prepare_security(&key(), true)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("redirect")
            );
            server.await.unwrap();
        }
        let (source, server) = mock(vec![get(
            &format!("{LIST}?state=triage&per_page=100"),
            200,
            json!("x".repeat(MAX_BODY + 1)),
        )])
        .await;
        assert!(
            source
                .sync(None)
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn page_limit_and_repeated_cursors_discard_partial_results() {
        for repeated in [false, true] {
            let count = if repeated {
                2
            } else {
                MAX_PAGES
            };
            let mut replies = Vec::new();
            for page in 0..count {
                let path = if page == 0 {
                    format!("{LIST}?state=triage&per_page=100")
                } else {
                    format!("{LIST}?state=triage&per_page=100&after={page}")
                };
                let mut reply = get(&path, 200, json!([]));
                reply.headers = format!(
                    "Link: <{{base}}{LIST}?state=triage&per_page=100&after={}>; rel=\"next\"\r\n",
                    if repeated {
                        1
                    } else {
                        page + 1
                    }
                );
                replies.push(reply);
            }
            let (source, server) = mock(replies).await;
            assert!(
                source
                    .sync(None)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("pagination limit")
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn validates_names_ids_and_git_branch_refs() {
        assert!(valid_ghsa(ID));
        for id in [
            "GHSA-abcd-efgh-ijkl",
            "ghsa-2345-cfgh-jmpq",
            "GHSA-2345-cfgh-jmpq/evil",
        ] {
            assert!(!valid_ghsa(id));
        }
        for name in ["acme/app", "acme/app-ghsa_2345"] {
            assert!(valid_name(name));
        }
        for name in [
            "acme/app/extra",
            "acme/..",
            "acme/.app",
            "acme/-app",
            "acme/app\n",
            "acme/app$(x)",
            "acme/app%2fother",
        ] {
            assert!(!valid_name(name));
        }
        for branch in ["main", "security/fix-123", "fix_123"] {
            assert!(valid_branch(branch));
        }
        for branch in [
            "",
            "@",
            "-main",
            "main.lock",
            "foo/.bar",
            "foo//bar",
            "main.",
            "a..b",
            "a@{b",
            "a b",
            "a:b",
            "a\\b",
            "a\n",
            "a*",
            "a?",
            "a[",
        ] {
            assert!(!valid_branch(branch));
        }
    }
}
