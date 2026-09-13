//! Discovery and synchronization for repository issue sources.

mod beads;
mod error;
mod github;
mod gitlab;
mod repository;
mod source;

use std::path::Path;

use agent_launcher_core::{IssueProvider, Repository, RepositoryRemote};
pub use beads::BeadsSource;
pub use error::Error;
pub use github::GitHubSource;
pub use gitlab::GitLabSource;
pub use repository::{RemoteCoordinates, detect_repository, parse_remote_url};
pub use source::{IssueSource, SourceKey, SyncCheckpoint, SyncMode, SyncResult};

/// Detects a repository and constructs every issue source available for it.
pub async fn sources_from_cwd(
    cwd: impl AsRef<Path>,
) -> Result<(Repository, Vec<Box<dyn IssueSource>>), Error> {
    sources_from_cwd_with_remote(cwd, None).await
}

/// Detects a repository and constructs its issue sources, replacing the configured remote when
/// `remote_url` is provided.
pub async fn sources_from_cwd_with_remote(
    cwd: impl AsRef<Path>,
    remote_url: Option<&str>,
) -> Result<(Repository, Vec<Box<dyn IssueSource>>), Error> {
    let mut repository = detect_repository(cwd).await?;
    if let Some(remote_url) = remote_url {
        repository.remote = Some(command_line_remote(remote_url)?);
    }
    let sources = build_sources(&repository)?;
    Ok((repository, sources))
}

fn command_line_remote(remote_url: &str) -> Result<RepositoryRemote, Error> {
    let coordinates = parse_remote_url(remote_url)?;
    Ok(RepositoryRemote {
        name: "command-line".to_owned(),
        url: remote_url.to_owned(),
        host: coordinates.host,
        repository: coordinates.repository,
        provider: coordinates.provider,
    })
}

/// Constructs the remote source and local Beads source detected for a repository.
pub fn build_sources(repository: &Repository) -> Result<Vec<Box<dyn IssueSource>>, Error> {
    let mut sources: Vec<Box<dyn IssueSource>> = Vec::new();

    if let Some(remote) = &repository.remote {
        match remote.provider {
            IssueProvider::Github => {
                sources.push(Box::new(GitHubSource::from_remote(remote.clone())?));
            },
            IssueProvider::Gitlab => {
                sources.push(Box::new(GitLabSource::from_remote(remote.clone())?));
            },
            IssueProvider::Beads => {},
        }
    }

    if repository.has_beads {
        sources.push(Box::new(BeadsSource::new(repository)));
    }

    Ok(sources)
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::IssueProvider;

    use super::{GitHubSource, GitLabSource, IssueSource, command_line_remote};

    #[tokio::test]
    async fn remote_sources_explicitly_reject_deletion() {
        let sources: Vec<Box<dyn IssueSource>> = vec![
            Box::new(GitHubSource::new("github.com".into(), "acme/app".into(), None).unwrap()),
            Box::new(GitLabSource::new("gitlab.com".into(), "acme/app".into(), None).unwrap()),
        ];
        for source in sources {
            let scope = source.source_key();
            let key = agent_launcher_core::IssueKey {
                provider: scope.provider,
                host: scope.host.clone(),
                repository: scope.repository.clone(),
                native_id: "1".into(),
            };
            assert!(!source.supports_delete());
            assert!(matches!(
                source.delete_issue(&key).await,
                Err(super::Error::DeleteUnsupported)
            ));
        }
    }

    #[test]
    fn builds_sources_for_command_line_remote_formats() {
        let cases = [
            (
                "https://github.com/acme/launcher.git",
                IssueProvider::Github,
                "acme/launcher",
            ),
            (
                "git@gitlab.com:acme/tools/launcher.git",
                IssueProvider::Gitlab,
                "acme/tools/launcher",
            ),
        ];

        for (url, provider, name) in cases {
            let remote = command_line_remote(url).unwrap();
            // Explicit auth keeps this parsing/construction test off the local credential store.
            let source: Box<dyn IssueSource> = match remote.provider {
                IssueProvider::Github => {
                    Box::new(GitHubSource::new(remote.host, remote.repository, None).unwrap())
                },
                IssueProvider::Gitlab => {
                    Box::new(GitLabSource::new(remote.host, remote.repository, None).unwrap())
                },
                IssueProvider::Beads => unreachable!(),
            };
            assert_eq!(source.source_key().provider, provider);
            assert_eq!(source.source_key().repository, name);
        }
    }
}
