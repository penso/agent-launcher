use std::path::{Path, PathBuf};

use agent_launcher_core::{IssueProvider, Repository, RepositoryRemote};
use tokio::process::Command;
use tracing::debug;
use url::Url;

use crate::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCoordinates {
    pub host: String,
    pub repository: String,
    pub provider: IssueProvider,
}

pub async fn detect_repository(cwd: impl AsRef<Path>) -> Result<Repository, Error> {
    let cwd = cwd.as_ref();
    let root_output = run_git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let root = PathBuf::from(root_output.trim());
    if root.as_os_str().is_empty() {
        return Err(Error::MissingRepositoryRoot);
    }

    let common_dir_output = run_git(&root, &["rev-parse", "--git-common-dir"]).await?;
    let common_dir = PathBuf::from(common_dir_output.trim());
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        root.join(common_dir)
    };
    let git_dir = tokio::fs::canonicalize(&common_dir)
        .await
        .map_err(|source| Error::CommandIo {
            program: "git",
            source,
        })?;

    let remotes = read_remotes(&root).await?;
    let remote = select_remote(&remotes).and_then(|(name, url)| {
        parse_remote_url(url)
            .map(|coordinates| RepositoryRemote {
                name: name.clone(),
                url: url.clone(),
                host: coordinates.host,
                repository: coordinates.repository,
                provider: coordinates.provider,
            })
            .map_err(|error| {
                debug!(remote = name, %error, "ignoring unsupported issue remote");
                error
            })
            .ok()
    });

    Ok(Repository {
        has_beads: root.join(".beads").exists(),
        root,
        git_dir,
        remote,
    })
}

pub fn parse_remote_url(value: &str) -> Result<RemoteCoordinates, Error> {
    let (host, path) = if let Ok(url) = Url::parse(value) {
        match url.scheme() {
            "http" | "https" | "ssh" | "git" => {},
            _ => return Err(Error::InvalidRemoteUrl(value.to_owned())),
        }
        let hostname = url
            .host_str()
            .ok_or_else(|| Error::InvalidRemoteUrl(value.to_owned()))?;
        let hostname = if hostname.contains(':') {
            format!("[{hostname}]")
        } else {
            hostname.to_owned()
        };
        let host = url
            .port()
            .filter(|_| matches!(url.scheme(), "http" | "https"))
            .map_or(hostname.clone(), |port| format!("{hostname}:{port}"));
        (host, url.path().to_owned())
    } else {
        parse_scp_remote(value)?
    };

    let repository = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_matches('/'))
        .to_owned();
    if repository.split('/').count() < 2 || repository.ends_with('/') {
        return Err(Error::InvalidRemoteUrl(value.to_owned()));
    }

    let host = host.to_ascii_lowercase();
    let provider = provider_for(&host, &repository);
    Ok(RemoteCoordinates {
        host,
        repository,
        provider,
    })
}

fn parse_scp_remote(value: &str) -> Result<(String, String), Error> {
    let (authority, path) = value
        .split_once(':')
        .ok_or_else(|| Error::InvalidRemoteUrl(value.to_owned()))?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() || path.is_empty() || host.contains('/') {
        return Err(Error::InvalidRemoteUrl(value.to_owned()));
    }
    Ok((host.to_owned(), path.to_owned()))
}

fn provider_for(host: &str, repository: &str) -> IssueProvider {
    if host.contains("gitlab") {
        IssueProvider::Gitlab
    } else if host.contains("github") || host.contains("ghe") {
        IssueProvider::Github
    } else if repository.matches('/').count() > 1 {
        // Nested namespaces are supported by GitLab and not by GitHub.
        IssueProvider::Gitlab
    } else {
        IssueProvider::Github
    }
}

async fn read_remotes(root: &Path) -> Result<Vec<(String, String)>, Error> {
    let names = run_git(root, &["remote"]).await?;
    let mut remotes = Vec::new();
    for name in names.lines().filter(|name| !name.is_empty()) {
        let output = Command::new("git")
            .args(["-C"])
            .arg(root)
            .args(["remote", "get-url", name])
            .output()
            .await
            .map_err(|source| Error::CommandIo {
                program: "git",
                source,
            })?;
        if output.status.success() {
            let url = String::from_utf8(output.stdout).map_err(|source| Error::CommandOutput {
                program: "git",
                source,
            })?;
            remotes.push((name.to_owned(), url.trim().to_owned()));
        }
    }
    Ok(remotes)
}

fn select_remote(remotes: &[(String, String)]) -> Option<&(String, String)> {
    remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.iter().find(|(name, _)| name == "upstream"))
        .or_else(|| remotes.first())
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .await
        .map_err(|source| Error::CommandIo {
            program: "git",
            source,
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: "git",
            cwd: cwd.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|source| Error::CommandOutput {
        program: "git",
        source,
    })
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;

    use super::{parse_remote_url, select_remote};

    #[test]
    fn parses_common_remote_urls() {
        let cases = [
            (
                "git@github.com:owner/repo.git",
                "github.com",
                "owner/repo",
                IssueProvider::Github,
            ),
            (
                "https://github.example.com/owner/repo.git",
                "github.example.com",
                "owner/repo",
                IssueProvider::Github,
            ),
            (
                "ssh://git@gitlab.com/group/nested/repo.git",
                "gitlab.com",
                "group/nested/repo",
                IssueProvider::Gitlab,
            ),
            (
                "https://gitlab.internal/group/repo",
                "gitlab.internal",
                "group/repo",
                IssueProvider::Gitlab,
            ),
            (
                "git@code.internal:group/nested/repo.git",
                "code.internal",
                "group/nested/repo",
                IssueProvider::Gitlab,
            ),
            (
                "https://gitlab.internal:8443/group/repo.git",
                "gitlab.internal:8443",
                "group/repo",
                IssueProvider::Gitlab,
            ),
        ];

        for (url, host, repository, provider) in cases {
            let parsed = parse_remote_url(url).expect("remote should parse");
            assert_eq!(parsed.host, host);
            assert_eq!(parsed.repository, repository);
            assert_eq!(parsed.provider, provider);
        }
    }

    #[test]
    fn rejects_local_and_incomplete_remotes() {
        assert!(parse_remote_url("/tmp/repo").is_err());
        assert!(parse_remote_url("git@github.com:repo.git").is_err());
        assert!(parse_remote_url("file:///tmp/repo").is_err());
    }

    #[test]
    fn selects_origin_then_upstream_then_first_remote() {
        let remotes = vec![
            ("fork".to_owned(), "fork-url".to_owned()),
            ("upstream".to_owned(), "upstream-url".to_owned()),
            ("origin".to_owned(), "origin-url".to_owned()),
        ];
        assert_eq!(
            select_remote(&remotes).map(|remote| remote.0.as_str()),
            Some("origin")
        );

        let remotes = &remotes[..2];
        assert_eq!(
            select_remote(remotes).map(|remote| remote.0.as_str()),
            Some("upstream")
        );

        let remotes = &remotes[..1];
        assert_eq!(
            select_remote(remotes).map(|remote| remote.0.as_str()),
            Some("fork")
        );
    }
}
