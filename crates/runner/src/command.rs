use std::{ffi::OsString, path::Path, time::Duration};

use agent_launcher_core::BackendKind;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use url::Url;

use crate::{Error, Result};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn run_output(
    program: &Path,
    args: &[OsString],
    current_dir: Option<&Path>,
) -> Result<String> {
    run_output_with_timeout(program, args, current_dir, COMMAND_TIMEOUT).await
}

pub(crate) async fn run_output_with_timeout(
    program: &Path,
    args: &[OsString],
    current_dir: Option<&Path>,
    timeout: Duration,
) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| Error::CommandTimedOut {
            program: command_display(program, args),
            timeout,
        })?
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::ExecutableNotFound(program.display().to_string())
            } else {
                Error::Io(error)
            }
        })?;
    if !output.status.success() {
        return Err(Error::CommandFailed {
            program: command_display(program, args),
            status: output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string()),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) async fn run_json(
    program: &Path,
    args: &[OsString],
    current_dir: Option<&Path>,
) -> Result<Value> {
    let output = run_output(program, args, current_dir).await?;
    parse_json_output(&output)
}

pub(crate) fn parse_json_output(output: &str) -> Result<Value> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidResponse(
            "command returned empty output".into(),
        ));
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }

    // Some CLI versions print a warning before the JSON payload. Decode the
    // first complete object or array rather than relying on line boundaries.
    for (index, character) in trimmed.char_indices() {
        if character != '{' && character != '[' {
            continue;
        }
        let mut deserializer = serde_json::Deserializer::from_str(&trimmed[index..]);
        if let Ok(value) = Value::deserialize(&mut deserializer) {
            return Ok(value);
        }
    }
    Err(Error::InvalidResponse(format!(
        "command did not return JSON: {}",
        trimmed.chars().take(240).collect::<String>()
    )))
}

pub(crate) fn command_display(program: &Path, args: &[OsString]) -> String {
    std::iter::once(program.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(|part| shell_quote(&part.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    if let Value::Object(object) = value {
        for key in keys {
            if let Some(value) = object.get(*key).and_then(Value::as_str) {
                return Some(value.to_string());
            }
        }
        for child in object.values() {
            if let Some(value) = find_string(child, keys) {
                return Some(value);
            }
        }
    }
    None
}

pub(crate) async fn open_uri(uri: &Url, _backend: BackendKind) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(uri.as_str());
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(uri.as_str());
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", uri.as_str()]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err(Error::UnsupportedCapability {
        backend: _backend,
        capability: crate::Capability::Open,
    });

    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        command.kill_on_drop(true);
        let status = tokio::time::timeout(OPEN_TIMEOUT, command.status())
            .await
            .map_err(|_| Error::CommandTimedOut {
                program: "system URL opener".into(),
                timeout: OPEN_TIMEOUT,
            })??;
        if !status.success() {
            return Err(Error::CommandFailed {
                program: "system URL opener".into(),
                status: status.to_string(),
                stderr: String::new(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_json_after_cli_warning() {
        let value = parse_json_output("warning: using local host\n{\"id\":\"ws_1\"}\n")
            .expect("JSON should parse");
        assert_eq!(value, json!({"id": "ws_1"}));
    }

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote("it's safe"), "'it'\\''s safe'");
        assert_eq!(shell_quote(""), "''");
    }
}
