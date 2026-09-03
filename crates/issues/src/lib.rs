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
    use std::path::PathBuf;

    use agent_launcher_core::{IssueProvider, Repository};

    use super::{build_sources, command_line_remote};

    fn repository_with_remote(remote_url: &str) -> Repository {
        Repository {
            root: PathBuf::from("/repo"),
            git_dir: PathBuf::from("/repo/.git"),
            remote: Some(command_line_remote(remote_url).unwrap()),
            has_beads: false,
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
            let sources = build_sources(&repository_with_remote(url)).unwrap();
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].source_key().provider, provider);
            assert_eq!(sources[0].source_key().repository, name);
        }
    }
}
