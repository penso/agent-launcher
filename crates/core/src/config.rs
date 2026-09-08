use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub backend: BackendConfig,
    pub agent: AgentConfig,
    pub superset_host: Option<String>,
    pub compute: Option<ComputeConfig>,
    pub ssh: Option<SshConfig>,
    pub notifications: NotificationConfig,
    #[serde(skip)]
    pub prompt_profiles: Vec<PromptProfile>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 60,
            backend: BackendConfig::Auto,
            agent: AgentConfig::default(),
            superset_host: None,
            compute: None,
            ssh: None,
            notifications: NotificationConfig::default(),
            prompt_profiles: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComputeConfig {
    pub placement: ComputePlacement,
    pub targets: Vec<ComputeTargetConfig>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComputePlacement {
    #[default]
    LeastLoaded,
    Random,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ComputeTargetConfig {
    Ssh {
        id: String,
        #[serde(default)]
        name: Option<String>,
        host: String,
        #[serde(default = "default_remote_root")]
        workspace_root: PathBuf,
        #[serde(default)]
        max_active_runs: Option<usize>,
        #[serde(default)]
        wake: Option<WakeConfig>,
    },
}

impl ComputeTargetConfig {
    pub fn id(&self) -> &str {
        match self {
            Self::Ssh { id, .. } => id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Ssh { id, name, .. } => name.as_deref().unwrap_or(id),
        }
    }

    pub const fn max_active_runs(&self) -> Option<usize> {
        match self {
            Self::Ssh {
                max_active_runs, ..
            } => *max_active_runs,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptProfile {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendConfig {
    #[default]
    Auto,
    Superset,
    Native,
    Herdr,
    Conductor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "opencode".to_string(),
            model: None,
            effort: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_remote_root")]
    pub workspace_root: PathBuf,
    pub wake: Option<WakeConfig>,
}

fn default_remote_root() -> PathBuf {
    PathBuf::from("~/.local/share/agent-launcher")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "kebab-case", deny_unknown_fields)]
pub enum WakeConfig {
    Daytona {
        sandbox: String,
    },
    Coder {
        workspace: String,
    },
    Azure {
        resource_group: String,
        vm: String,
    },
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    pub desktop: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { desktop: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_configuration_stays_valid() {
        let config: AppConfig = toml::from_str(include_str!("../../../config.example.toml"))
            .expect("example configuration should parse");
        assert!(matches!(config.backend, BackendConfig::Auto));
        assert_eq!(config.agent.name, "opencode");
        assert!(config.notifications.desktop);
    }

    #[test]
    fn multiple_compute_targets_parse() {
        let config: AppConfig = toml::from_str(
            r#"
            [compute]
            placement = "least-loaded"

            [[compute.targets]]
            provider = "ssh"
            id = "linux"
            host = "builder"
            max_active_runs = 4

            [[compute.targets]]
            provider = "ssh"
            id = "mac"
            name = "Mac Studio"
            host = "developer@mac"
            workspace_root = "/Users/developer/.agent-launcher"
            "#,
        )
        .expect("compute targets should parse");
        let compute = config.compute.expect("compute config should exist");
        assert_eq!(compute.placement, ComputePlacement::LeastLoaded);
        assert_eq!(compute.targets.len(), 2);
        assert_eq!(compute.targets[0].id(), "linux");
        assert_eq!(compute.targets[1].name(), "Mac Studio");
        assert_eq!(compute.targets[0].max_active_runs(), Some(4));
    }
}
