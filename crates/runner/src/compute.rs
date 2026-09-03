use std::{ffi::OsString, path::PathBuf};

pub use agent_launcher_core::WakeConfig;

use crate::{Result, command::run_output};

/// Starts the compute resource described by `config` and waits for its CLI to succeed.
pub async fn wake(config: &WakeConfig) -> Result<()> {
    let (program, args) = wake_command(config);
    run_output(&program, &args, None).await?;
    Ok(())
}

fn wake_command(config: &WakeConfig) -> (PathBuf, Vec<OsString>) {
    match config {
        WakeConfig::Daytona { sandbox } => ("daytona".into(), vec![
            "start".into(),
            sandbox.as_str().into(),
        ]),
        WakeConfig::Coder { workspace } => ("coder".into(), vec![
            "start".into(),
            workspace.as_str().into(),
            "--yes".into(),
        ]),
        WakeConfig::Azure { resource_group, vm } => ("az".into(), vec![
            "vm".into(),
            "start".into(),
            "--resource-group".into(),
            resource_group.as_str().into(),
            "--name".into(),
            vm.as_str().into(),
        ]),
        WakeConfig::Command { program, args } => (
            program.into(),
            args.iter().map(OsString::from).collect::<Vec<_>>(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_documented_provider_commands() {
        assert_eq!(
            wake_command(&WakeConfig::Daytona {
                sandbox: "dev box".into()
            }),
            ("daytona".into(), vec!["start".into(), "dev box".into()])
        );
        assert_eq!(
            wake_command(&WakeConfig::Coder {
                workspace: "api".into()
            }),
            ("coder".into(), vec![
                "start".into(),
                "api".into(),
                "--yes".into()
            ])
        );
        assert_eq!(
            wake_command(&WakeConfig::Azure {
                resource_group: "agents".into(),
                vm: "worker-1".into()
            }),
            ("az".into(), vec![
                "vm".into(),
                "start".into(),
                "--resource-group".into(),
                "agents".into(),
                "--name".into(),
                "worker-1".into(),
            ])
        );
    }

    #[test]
    fn preserves_custom_command_argv_boundaries() {
        let command = WakeConfig::Command {
            program: "wake helper".into(),
            args: vec!["one argument".into(), "; rm -rf /".into()],
        };
        assert_eq!(
            wake_command(&command),
            ("wake helper".into(), vec![
                "one argument".into(),
                "; rm -rf /".into()
            ])
        );
    }
}
