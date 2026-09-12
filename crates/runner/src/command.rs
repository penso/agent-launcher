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
            program: program.display().to_string(),
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
            program: program.display().to_string(),
            status: output
                .status
                .code()
                .map_or_else(|| "signal".to_string(), |code| code.to_string()),
            stderr: command_error_reason(&String::from_utf8_lossy(&output.stderr), args),
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
    // Output may contain an echoed prompt or terminal contents.
    Err(Error::InvalidResponse("command did not return JSON".into()))
}

fn command_error_reason(stderr: &str, args: &[OsString]) -> String {
    let mut reason = stderr.trim().to_owned();
    if let Ok(value) = serde_json::from_str::<Value>(&reason)
        && let Some(message) = value.pointer("/error/message").and_then(Value::as_str)
    {
        reason = match value.pointer("/error/code").and_then(Value::as_str) {
            Some(code) => format!("{message} [{code}]"),
            None => message.to_owned(),
        };
    }
    // Herdr uses a positional prompt; Superset uses --prompt. Do not leak
    // either if the child echoes it in a diagnostic.
    for (index, arg) in args.iter().enumerate() {
        let positional_prompt = index == 3
            && args.first().is_some_and(|arg| arg == "agent")
            && args.get(1).is_some_and(|arg| arg == "prompt");
        let flagged_prompt = index > 0 && args[index - 1] == "--prompt";
        if positional_prompt || flagged_prompt {
            let prompt = arg.to_string_lossy();
            if !prompt.is_empty() {
                reason = reason.replace(prompt.as_ref(), "[redacted]");
                let encoded =
                    serde_json::to_string(prompt.as_ref()).expect("string encodes as JSON");
                reason = reason.replace(&encoded[1..encoded.len() - 1], "[redacted]");
            }
        }
    }
    reason.split_whitespace().collect::<Vec<_>>().join(" ")
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

pub(crate) fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Object(values) => values
            .values()
            .any(|value| contains_string(value, expected)),
        _ => false,
    }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_command_reports_reason_before_context_without_arguments() {
        let args = [
            "-c",
            "printf 'repository trust required\\ntry again after approval\\n' >&2; exit 7",
            "private prompt never belongs in errors",
        ]
        .map(OsString::from);
        let error = run_output(Path::new("/bin/sh"), &args, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "repository trust required try again after approval (/bin/sh exited with status 7)"
        );
        assert!(!format!("{error:?}").contains("private prompt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_command_without_stderr_does_not_expose_stdout() {
        let args = ["-c", "printf 'private prompt'; exit 1"].map(OsString::from);
        let error = run_output(Path::new("/bin/sh"), &args, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "command failed without stderr (/bin/sh exited with status 1)"
        );
        assert!(!format!("{error:?}").contains("private prompt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_timeout_does_not_expose_arguments() {
        let args = ["-c", "exec sleep 5", "private prompt"].map(OsString::from);
        let error =
            run_output_with_timeout(Path::new("/bin/sh"), &args, None, Duration::from_millis(20))
                .await
                .unwrap_err();
        assert!(matches!(error, Error::CommandTimedOut { .. }));
        assert!(!error.to_string().contains("private prompt"));
        assert!(!format!("{error:?}").contains("private prompt"));
    }

    #[test]
    fn structured_errors_put_message_before_code_and_omit_envelope() {
        assert_eq!(
            command_error_reason(
                r#"{"id":"req","error":{"code":"linked_worktree_source","message":"New and open worktree actions start from the repo parent workspace."}}"#,
                &[],
            ),
            "New and open worktree actions start from the repo parent workspace. [linked_worktree_source]"
        );
    }

    #[test]
    fn echoed_prompts_are_redacted_in_plain_and_json_errors() {
        let prompt = "private prompt\nwith \"quotes\"";
        for args in [vec!["agent", "prompt", "launcher-123", prompt], vec![
            "--prompt", prompt,
        ]] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            for stderr in [
                format!("prompt rejected: {prompt}"),
                json!({"error": {"code": "rejected", "message": format!("prompt rejected: {prompt}")}}).to_string(),
                json!({"diagnostic": prompt}).to_string(),
            ] {
                let reason = command_error_reason(&stderr, &args);
                assert!(!reason.contains("private prompt"));
                assert!(reason.contains("[redacted]"));
            }
        }
    }

    #[test]
    fn invalid_json_does_not_expose_output() {
        let error = parse_json_output("private prompt").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid backend response: command did not return JSON"
        );
        assert!(!format!("{error:?}").contains("private prompt"));
    }
}
