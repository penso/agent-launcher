use std::path::PathBuf;

use reqwest::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("issue deletion is not supported by this source")]
    DeleteUnsupported,
    #[error("issue does not match this source or has an invalid native ID")]
    InvalidDeleteTarget,
    #[error("bd command timed out; mutation outcome may be uncertain")]
    CommandTimeout,
    #[error("GitHub throttled; retry after {retry_at}")]
    Throttled {
        retry_at: chrono::DateTime<chrono::Utc>,
    },
    #[error("failed to run {program}: {source}")]
    CommandIo {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{program} failed in {cwd}: {stderr}")]
    CommandFailed {
        program: &'static str,
        cwd: PathBuf,
        stderr: String,
    },

    #[error("{program} returned non-UTF-8 output: {source}")]
    CommandOutput {
        program: &'static str,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("git did not return a repository root")]
    MissingRepositoryRoot,

    #[error("invalid or unsupported remote URL: {0}")]
    InvalidRemoteUrl(String),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("failed to construct HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("{status} response from {url}: {body}")]
    HttpStatus {
        status: StatusCode,
        url: String,
        body: String,
    },

    #[error(
        "GitHub rate limit ({status}) from {url}; wait for the limit to reset; authenticate with `gh auth login --hostname {host}` or check the configured token (GH_TOKEN/GITHUB_TOKEN on github.com)"
    )]
    GitHubRateLimit {
        status: StatusCode,
        url: String,
        host: String,
    },

    #[error("invalid {source} JSON: {error}")]
    Json {
        source: &'static str,
        #[source]
        error: serde_json::Error,
    },

    #[error("invalid HTTP header value: {0}")]
    Header(#[from] reqwest::header::InvalidHeaderValue),
}
