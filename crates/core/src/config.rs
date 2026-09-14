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
    pub herdr_activity: HerdrActivityConfig,
    #[serde(skip)]
    pub prompt_profiles: Vec<PromptProfile>,
    /// Explicit editor root supplied at startup; None keeps profiles read-only.
    #[serde(skip)]
    pub prompt_root: Option<PathBuf>,
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
            herdr_activity: HerdrActivityConfig::default(),
            prompt_profiles: Vec::new(),
            prompt_root: None,
        }
    }
}

/// Read-only inventory, independent of the dispatch backend and selected pane.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrActivityConfig {
    pub enabled: bool,
    pub executable: String,
    pub ssh_executable: String,
    pub discover_local_sessions: bool,
    pub discover_saved_profiles: bool,
    pub discover_remote_sessions: bool,
    pub xdg_config_home: Option<String>,
    pub xdg_state_home: Option<String>,
    pub endpoints: Vec<HerdrActivityEndpointConfig>,
    pub exclusions: Vec<HerdrActivityEndpointConfig>,
    pub alias_groups: Vec<HerdrActivityAliasGroup>,
}

impl Default for HerdrActivityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: "herdr".into(),
            ssh_executable: "ssh".into(),
            discover_local_sessions: true,
            discover_saved_profiles: true,
            discover_remote_sessions: false,
            xdg_config_home: None,
            xdg_state_home: None,
            endpoints: Vec::new(),
            exclusions: Vec::new(),
            alias_groups: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrActivityEndpointConfig {
    /// None is local. Remote targets use OpenSSH aliases, user@host, or ssh:// URIs.
    pub target: Option<String>,
    pub session: String,
    pub enabled: bool,
    /// Absolute paths on the endpoint's host; no tilde or environment expansion.
    pub xdg_config_home: Option<String>,
    pub xdg_state_home: Option<String>,
}

impl Default for HerdrActivityEndpointConfig {
    fn default() -> Self {
        Self {
            target: None,
            session: "default".into(),
            enabled: true,
            xdg_config_home: None,
            xdg_state_home: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HerdrActivityAliasGroup {
    /// All members assert the same account/host. "local" may identify loopback.
    pub canonical: String,
    pub targets: Vec<String>,
}

pub fn valid_activity_session(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Parse a deliberately restricted SSH destination, never an option or shell fragment.
/// Bare IPv6 and host:port are rejected; use ssh://[IPv6]:port instead.
pub fn activity_ssh_target(value: &str) -> Result<(String, Option<u16>), String> {
    let invalid = || "invalid herdr_activity SSH target".to_owned();
    if value.is_empty() || value.len() > 512 || !value.is_ascii() {
        return Err(invalid());
    }
    let uri = value.starts_with("ssh://");
    let value = value.strip_prefix("ssh://").unwrap_or(value);
    let (user, host) = match value.split_once('@') {
        Some((user, host)) => {
            if user.is_empty()
                || user.starts_with('-')
                || !user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(invalid());
            }
            (Some(user), host)
        },
        None => (None, value),
    };
    let (host, port) = if uri && host.starts_with('[') {
        let (ip, rest) = host[1..].split_once(']').ok_or_else(invalid)?;
        let ip = ip.parse::<std::net::Ipv6Addr>().map_err(|_| invalid())?;
        (
            format!("[{ip}]"),
            if rest.is_empty() {
                None
            } else {
                Some(rest.strip_prefix(':').ok_or_else(invalid)?)
            },
        )
    } else if uri {
        match host.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (host.to_owned(), None),
        }
    } else {
        (host.to_owned(), None)
    };
    if !host.starts_with('[')
        && (host.is_empty()
            || !host.as_bytes()[0].is_ascii_alphanumeric()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    {
        return Err(invalid());
    }
    // Bracketed forms are accepted only through the validated URI branch.
    if host.starts_with('[') && !uri {
        return Err(invalid());
    }
    let port = port
        .map(|p| p.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(invalid))
        .transpose()?;
    Ok((
        user.map_or(host.clone(), |user| format!("{user}@{host}")),
        port,
    ))
}

impl HerdrActivityConfig {
    pub fn validate(&self) -> Result<(), String> {
        for executable in [&self.executable, &self.ssh_executable] {
            if executable.trim().is_empty()
                || executable.starts_with('-')
                || executable.chars().any(char::is_control)
            {
                return Err("invalid herdr_activity executable".into());
            }
        }
        let valid_path = |path: &Option<String>| {
            path.as_ref()
                .is_none_or(|p| p.starts_with('/') && !p.chars().any(char::is_control))
        };
        if !valid_path(&self.xdg_config_home)
            || !valid_path(&self.xdg_state_home)
            || self.endpoints.len() + self.exclusions.len() > 1024
            || self
                .alias_groups
                .iter()
                .map(|group| group.targets.len())
                .sum::<usize>()
                > 1024
        {
            return Err("invalid herdr_activity namespace or endpoint limit".into());
        }
        for endpoint in self.endpoints.iter().chain(&self.exclusions) {
            if !valid_activity_session(&endpoint.session)
                || !valid_path(&endpoint.xdg_config_home)
                || !valid_path(&endpoint.xdg_state_home)
            {
                return Err("invalid herdr_activity session or namespace".into());
            }
            if let Some(target) = &endpoint.target {
                activity_ssh_target(target)?;
            }
        }
        let mut members = std::collections::HashSet::new();
        let mut canonical_ids = std::collections::HashSet::new();
        for group in &self.alias_groups {
            if group.canonical.is_empty()
                || group.canonical.len() > 128
                || group.canonical.chars().any(char::is_control)
                || group.targets.is_empty()
                || !canonical_ids.insert(&group.canonical)
            {
                return Err("invalid herdr_activity alias group".into());
            }
            for target in &group.targets {
                let key = if target == "local" {
                    None
                } else {
                    Some(activity_ssh_target(target)?)
                };
                if !members.insert(key) {
                    return Err("overlapping herdr_activity alias groups".into());
                }
            }
        }
        Ok(())
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
        assert!(config.herdr_activity.enabled);
        config.herdr_activity.validate().unwrap();
    }

    #[test]
    fn activity_targets_and_sessions_are_strict() {
        for target in [
            "builder",
            "user@builder",
            "ssh://user@builder:2222",
            "ssh://[::1]:2222",
            "ssh://user@[2001:db8::1]",
        ] {
            assert!(activity_ssh_target(target).is_ok(), "{target}");
        }
        for target in [
            "",
            "-oProxyCommand=evil",
            "host name",
            "host;evil",
            "user@host@evil",
            "user$(evil)@host",
            "ssh://host/path",
            "ssh://host:0",
            "ssh://host:65536",
            "ssh://host:22?x",
            "::1",
            "host:22",
            "[::1]",
            "ssh://[bad]",
            "ssh://[::1]evil",
            "ssh://-host",
            "host\n",
        ] {
            assert!(activity_ssh_target(target).is_err(), "{target}");
        }
        assert_eq!(
            activity_ssh_target("ssh://me@host").unwrap(),
            activity_ssh_target("me@host").unwrap()
        );
        // An explicit 22 must override a nonstandard Port in the user's SSH config.
        assert_eq!(activity_ssh_target("ssh://host:22").unwrap().1, Some(22));
        for session in ["default", "a.b_-", &"a".repeat(64)] {
            assert!(valid_activity_session(session));
        }
        for session in ["", ".", "..", "../x", "a b", "a;id", &"a".repeat(65)] {
            assert!(!valid_activity_session(session));
        }
    }

    #[test]
    fn activity_config_rejects_ambiguous_aliases_and_namespaces() {
        let mut config = HerdrActivityConfig {
            xdg_config_home: Some("~/.config".into()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.xdg_config_home = None;
        config.alias_groups = vec![HerdrActivityAliasGroup {
            canonical: "host".into(),
            targets: vec!["builder".into(), "ssh://builder".into()],
        }];
        assert!(config.validate().is_err());
        config.alias_groups[0].targets.pop();
        assert!(config.validate().is_ok());
        assert!(
            toml::from_str::<AppConfig>("[herdr_activity]\ndiscover_remote_session = true")
                .is_err()
        );
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
