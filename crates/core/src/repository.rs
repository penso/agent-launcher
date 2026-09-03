use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::IssueProvider;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repository {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub remote: Option<RepositoryRemote>,
    pub has_beads: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepositoryRemote {
    pub name: String,
    pub url: String,
    pub host: String,
    pub repository: String,
    pub provider: IssueProvider,
}

impl Repository {
    pub fn display_name(&self) -> String {
        self.remote
            .as_ref()
            .map(|remote| remote.repository.clone())
            .or_else(|| {
                self.root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| self.root.display().to_string())
    }
}
