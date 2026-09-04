#[cfg(feature = "tui")]
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "tui")]
use agent_launcher_core::{AppConfig, PromptProfile};
#[cfg(feature = "tui")]
use agent_launcher_issues::sources_from_cwd_with_remote;
#[cfg(feature = "tui")]
use agent_launcher_runner::{
    Backend, ConductorBackend, ConductorConfig, HerdrBackend, HerdrConfig, NativeBackend,
    NativeConfig, NativeSshConfig, Runner, SessionRegistry, SupersetBackend, SupersetConfig,
};
#[cfg(feature = "tui")]
use agent_launcher_runtime::RuntimeService;
#[cfg(feature = "tui")]
use agent_launcher_store::{Store, StoreError};
use clap::Parser;
#[cfg(feature = "tui")]
use thiserror::Error;
#[cfg(feature = "tui")]
use uuid::Uuid;

/// Repository issue inbox and coding-agent launcher.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Override the detected GitHub or GitLab repository remote.
    #[arg(long, value_name = "URL", value_parser = validate_remote_url)]
    remote: Option<String>,
}

fn validate_remote_url(value: &str) -> Result<String, String> {
    agent_launcher_issues::parse_remote_url(value)
        .map(|_| value.to_owned())
        .map_err(|error| error.to_string())
}

#[cfg(feature = "tui")]
#[derive(Debug, Error)]
enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("could not determine the current directory: {0}")]
    CurrentDirectory(#[source] std::io::Error),
    #[error("could not determine a user configuration directory")]
    ConfigDirectoryUnavailable,
    #[error("could not determine a user data directory")]
    DataDirectoryUnavailable,
    #[error("could not read configuration at {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse configuration at {path}: {source}")]
    ParseConfig {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not {operation} prompt profiles at {path}: {source}")]
    PromptProfiles {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("prompt profile `{name}` at {path} is empty")]
    EmptyPromptProfile { name: String, path: PathBuf },
    #[error("could not create data directory at {path}: {source}")]
    CreateDataDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Issues(#[from] agent_launcher_issues::Error),
    #[error(transparent)]
    Runner(#[from] agent_launcher_runner::Error),
    #[error(transparent)]
    Store(#[from] agent_launcher_store::StoreError),
    #[error(transparent)]
    Runtime(#[from] agent_launcher_runtime::Error),
    #[error(transparent)]
    Tui(#[from] agent_launcher_tui::Error),
}

#[cfg(feature = "tui")]
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = run(cli.remote.as_deref()).await;
    if let Err(error) = result {
        eprintln!("agent-launcher: {error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "tui")]
async fn run(remote_url: Option<&str>) -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(Error::CurrentDirectory)?;
    let (repository, sources) = sources_from_cwd_with_remote(&cwd, remote_url).await?;
    let mut config = load_config().await?;
    config.prompt_profiles = load_prompt_profiles(&config_root()?).await?;
    validate_config(&config)?;
    let data_dir = repository_data_dir(&repository.git_dir)?;
    tokio::fs::create_dir_all(&data_dir)
        .await
        .map_err(|source| Error::CreateDataDirectory {
            path: data_dir.clone(),
            source,
        })?;

    let store = Store::open(data_dir.join("state.sqlite3")).await?;
    let registry = SessionRegistry::load(Some(data_dir.join("runner-sessions.json"))).await?;
    for deletion in registry.pending_deletions().await {
        if deletion.completed {
            if let Err(error) = store.delete_run(&deletion.run_id).await
                && !matches!(error, StoreError::RunNotFound(_))
            {
                return Err(error.into());
            }
            registry.finalize_deletion(&deletion.run_id).await?;
        } else {
            registry.cancel_deletion(&deletion.run_id).await?;
        }
    }
    for run in registry.summaries().await {
        store.upsert_run(&run).await?;
    }
    let native_config = NativeConfig {
        ssh: config.ssh.as_ref().map(|ssh| NativeSshConfig {
            destination: ssh.host.clone(),
            workspace_root: ssh.workspace_root.clone(),
            wake: ssh.wake.clone(),
        }),
        ..NativeConfig::default()
    };
    let backends: Vec<Arc<dyn Backend>> = vec![
        Arc::new(SupersetBackend::new(
            SupersetConfig {
                host: config.superset_host.clone(),
                required_agent: Some(config.agent.name.clone()),
                ..SupersetConfig::default()
            },
            Arc::clone(&registry),
        )),
        Arc::new(NativeBackend::new(native_config, Arc::clone(&registry))),
        Arc::new(HerdrBackend::new(
            HerdrConfig::default(),
            Arc::clone(&registry),
        )),
        Arc::new(ConductorBackend::new(
            ConductorConfig::default(),
            Arc::clone(&registry),
        )),
    ];
    let runner = Arc::new(Runner::new(backends));
    let runtime = RuntimeService::start(repository, sources, store, runner, config);
    let tui_result = agent_launcher_tui::run(runtime.clone()).await;
    let shutdown_result = runtime.shutdown().await;
    tui_result?;
    shutdown_result?;
    Ok(())
}

#[cfg(feature = "tui")]
fn validate_config(config: &AppConfig) -> Result<(), Error> {
    if config.poll_interval_seconds == 0 {
        return Err(Error::Usage(
            "poll_interval_seconds must be greater than zero".to_owned(),
        ));
    }
    if config.agent.name.trim().is_empty() {
        return Err(Error::Usage("agent.name cannot be empty".to_owned()));
    }
    if let Some(host) = config.superset_host.as_deref()
        && host.trim().is_empty()
    {
        return Err(Error::Usage("superset_host cannot be empty".to_owned()));
    }
    if let Some(ssh) = &config.ssh {
        if ssh.host.trim().is_empty() {
            return Err(Error::Usage("ssh.host cannot be empty".to_owned()));
        }
        if ssh.workspace_root.as_os_str().is_empty() {
            return Err(Error::Usage(
                "ssh.workspace_root cannot be empty".to_owned(),
            ));
        }
    }
    if matches!(
        config.backend,
        agent_launcher_core::BackendConfig::Conductor
    ) && config.agent.name == "opencode"
    {
        return Err(Error::Usage(
            "Conductor does not support the OpenCode agent; choose claude, codex, cursor, or acp"
                .to_owned(),
        ));
    }
    Ok(())
}

#[cfg(feature = "tui")]
async fn load_config() -> Result<AppConfig, Error> {
    let primary = config_path()?;
    let mut paths = vec![primary.clone()];
    if let Some(previous) = dirs::config_dir()
        .map(|root| root.join("agent-launcher").join("config.toml"))
        .filter(|path| *path != primary)
    {
        paths.push(previous);
    }
    for path in paths {
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => {
                return toml::from_str(&contents)
                    .map_err(|source| Error::ParseConfig { path, source });
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(source) => return Err(Error::ReadConfig { path, source }),
        }
    }
    Ok(AppConfig::default())
}

#[cfg(feature = "tui")]
fn config_path() -> Result<PathBuf, Error> {
    Ok(config_root()?.join("config.toml"))
}

#[cfg(feature = "tui")]
fn config_root() -> Result<PathBuf, Error> {
    dirs::home_dir()
        .map(|home| config_root_from_home(&home))
        .ok_or(Error::ConfigDirectoryUnavailable)
}

#[cfg(feature = "tui")]
fn config_root_from_home(home: &Path) -> PathBuf {
    home.join(".config").join("agent-launcher")
}

#[cfg(feature = "tui")]
async fn load_prompt_profiles(config_root: &Path) -> Result<Vec<PromptProfile>, Error> {
    const DEFAULTS: [(&str, &str); 3] = [
        ("designer", include_str!("../../../prompts/designer.md")),
        (
            "implementer",
            include_str!("../../../prompts/implementer.md"),
        ),
        ("reviewer", include_str!("../../../prompts/reviewer.md")),
    ];

    let agents_dir = config_root.join("agents");
    tokio::fs::create_dir_all(config_root)
        .await
        .map_err(|source| Error::PromptProfiles {
            operation: "create",
            path: config_root.to_owned(),
            source,
        })?;
    if !tokio::fs::try_exists(&agents_dir)
        .await
        .map_err(|source| Error::PromptProfiles {
            operation: "inspect",
            path: agents_dir.clone(),
            source,
        })?
    {
        let temporary = config_root.join(format!(".agents.tmp-{}", Uuid::new_v4()));
        for (name, template) in DEFAULTS {
            let directory = temporary.join(name);
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(|source| Error::PromptProfiles {
                    operation: "create",
                    path: directory.clone(),
                    source,
                })?;
            let path = directory.join("prompt.md");
            tokio::fs::write(&path, template)
                .await
                .map_err(|source| Error::PromptProfiles {
                    operation: "write",
                    path,
                    source,
                })?;
        }
        if let Err(source) = tokio::fs::rename(&temporary, &agents_dir).await {
            let won_race = tokio::fs::try_exists(&agents_dir).await.unwrap_or(false);
            let _ = tokio::fs::remove_dir_all(&temporary).await;
            if !won_race {
                return Err(Error::PromptProfiles {
                    operation: "initialize",
                    path: agents_dir.clone(),
                    source,
                });
            }
        }
    }

    let mut entries =
        tokio::fs::read_dir(&agents_dir)
            .await
            .map_err(|source| Error::PromptProfiles {
                operation: "read",
                path: agents_dir.clone(),
                source,
            })?;
    let mut profiles = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| Error::PromptProfiles {
            operation: "read",
            path: agents_dir.clone(),
            source,
        })?
    {
        let file_type = entry
            .file_type()
            .await
            .map_err(|source| Error::PromptProfiles {
                operation: "inspect",
                path: entry.path(),
                source,
            })?;
        let directory = entry.path();
        let is_directory = file_type.is_dir()
            || (file_type.is_symlink()
                && tokio::fs::metadata(&directory)
                    .await
                    .is_ok_and(|metadata| metadata.is_dir()));
        if !is_directory {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = directory.join("prompt.md");
        let template = match tokio::fs::read_to_string(&path).await {
            Ok(template) => template,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::PromptProfiles {
                    operation: "read",
                    path,
                    source,
                });
            },
        };
        if template.trim().is_empty() {
            return Err(Error::EmptyPromptProfile { name, path });
        }
        profiles.push(PromptProfile { name, path });
    }
    profiles.sort_by(|left, right| {
        let left_default = left.name != "implementer";
        let right_default = right.name != "implementer";
        left_default
            .cmp(&right_default)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(profiles)
}

#[cfg(feature = "tui")]
fn repository_data_dir(git_dir: &Path) -> Result<PathBuf, Error> {
    let identity = git_dir.to_string_lossy();
    let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes());
    dirs::data_local_dir()
        .map(|root| {
            root.join("agent-launcher")
                .join("repositories")
                .join(id.to_string())
        })
        .ok_or(Error::DataDirectoryUnavailable)
}

#[cfg(not(feature = "tui"))]
fn main() {
    let _ = Cli::parse();
    eprintln!("agent-launcher was built without TUI support");
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_remote_urls() {
        let https = Cli::try_parse_from([
            "agent-launcher",
            "--remote",
            "https://github.com/acme/launcher.git",
        ])
        .unwrap();
        assert_eq!(
            https.remote.as_deref(),
            Some("https://github.com/acme/launcher.git")
        );

        let git = Cli::try_parse_from([
            "agent-launcher",
            "--remote",
            "git@gitlab.com:acme/tools/launcher.git",
        ])
        .unwrap();
        assert_eq!(
            git.remote.as_deref(),
            Some("git@gitlab.com:acme/tools/launcher.git")
        );

        assert!(Cli::try_parse_from(["agent-launcher", "--remote", "/tmp/repo"]).is_err());
        assert!(Cli::try_parse_from(["agent-launcher", "--wat"]).is_err());
    }

    #[test]
    fn validates_required_configuration_values() {
        let mut config = AppConfig::default();
        config.agent.name.clear();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn uses_the_home_config_directory_on_every_platform() {
        assert_eq!(
            config_root_from_home(Path::new("/home/agent")).join("config.toml"),
            PathBuf::from("/home/agent/.config/agent-launcher/config.toml")
        );
    }

    #[tokio::test]
    async fn seeds_preserves_and_discovers_prompt_profiles() {
        let root = std::env::temp_dir().join(format!("agent-launcher-prompts-{}", Uuid::new_v4()));
        let (seeded, concurrent) =
            tokio::join!(load_prompt_profiles(&root), load_prompt_profiles(&root));
        let seeded = seeded.unwrap();
        assert_eq!(seeded, concurrent.unwrap());
        assert_eq!(
            seeded
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["implementer", "designer", "reviewer"]
        );
        let reviewer = root.join("agents/reviewer/prompt.md");
        tokio::fs::write(&reviewer, "Custom {{ issue_title }}")
            .await
            .unwrap();
        let custom = root.join("agents/security");
        tokio::fs::create_dir_all(&custom).await.unwrap();
        tokio::fs::write(custom.join("prompt.md"), "Audit {{ issue_text }}")
            .await
            .unwrap();
        tokio::fs::remove_dir_all(root.join("agents/designer"))
            .await
            .unwrap();

        let reloaded = load_prompt_profiles(&root).await.unwrap();

        assert_eq!(
            reloaded
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["implementer", "reviewer", "security"]
        );
        assert_eq!(
            tokio::fs::read_to_string(reviewer).await.unwrap(),
            "Custom {{ issue_title }}"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
