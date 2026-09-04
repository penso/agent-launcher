use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub backend: BackendConfig,
    pub agent: AgentConfig,
    pub superset_host: Option<String>,
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
            ssh: None,
            notifications: NotificationConfig::default(),
            prompt_profiles: Vec::new(),
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
}
