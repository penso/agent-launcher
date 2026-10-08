//! Harnesses and models found on this machine, offered by the launch settings
//! pickers. Detection is best effort: Herdr and Native start harnesses on this
//! host, so `PATH` and each harness's own model listing are a good guide, but a
//! missing entry never blocks a launch: custom model IDs stay available.

use std::{collections::HashMap, path::Path, time::Duration};

/// Long enough for `opencode models` to refresh its provider cache.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// Claude Code's model aliases, then current full model IDs.
const CLAUDE_MODELS: &[&str] = &[
    "sonnet",
    "opus",
    "haiku",
    "opusplan",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-fable-5-1",
    "claude-haiku-4-5-20251001",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Models {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HarnessCatalog {
    /// Whether each harness executable is on `PATH`, checked once per session.
    pub installed: HashMap<String, bool>,
    pub models: HashMap<String, Models>,
}

impl HarnessCatalog {
    /// `None` until the harness has been looked up.
    pub fn installed(&self, harness: &str) -> Option<bool> {
        self.installed.get(harness).copied()
    }

    /// Looks up harnesses not checked yet.
    pub fn detect<'a>(&mut self, harnesses: impl IntoIterator<Item = &'a str>) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for harness in harnesses {
            if !self.installed.contains_key(harness) {
                let found = std::env::split_paths(&path)
                    .any(|dir| is_executable(&dir.join(executable(harness))));
                self.installed.insert(harness.to_owned(), found);
            }
        }
    }
}

fn executable(harness: &str) -> &str {
    match harness {
        "cursor" => "cursor-agent",
        other => other,
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.with_extension("exe").is_file() || path.is_file()
    }
}

/// The models a harness reports, in its own order. Harnesses without a listing
/// return an empty list.
pub(crate) async fn list_models(harness: &str) -> Result<Vec<String>, String> {
    match harness {
        "claude" => Ok(CLAUDE_MODELS.iter().map(|m| (*m).to_owned()).collect()),
        "opencode" => run("opencode", &["models"])
            .await
            .map(|out| parse_opencode(&out)),
        "pi" => run("pi", &["--list-models"])
            .await
            .map(|out| parse_pi(&out)),
        _ => Ok(Vec::new()),
    }
}

async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = tokio::time::timeout(
        LIST_TIMEOUT,
        tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("{program} did not list models in time"))?
    .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} could not list models"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One `provider/model` per line.
fn parse_opencode(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| line.contains('/') && !line.contains(char::is_whitespace))
        .map(str::to_owned)
        .collect()
}

/// A `provider  model  context ...` table under a header row.
fn parse_pi(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let (provider, model) = (columns.next()?, columns.next()?);
            (provider != "provider").then(|| format!("{provider}/{model}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_harness_model_listings() {
        assert_eq!(
            parse_opencode("opencode/big-pickle\n  openai/gpt-5.4 \nWarning: stale cache\n"),
            ["opencode/big-pickle", "openai/gpt-5.4"]
        );
        assert_eq!(
            parse_pi(
                "provider        model              context\n\
                 github-copilot  claude-sonnet-5    1M\n\
                 openai          gpt-5.4            400K\n"
            ),
            ["github-copilot/claude-sonnet-5", "openai/gpt-5.4"]
        );
    }

    #[test]
    fn detection_checks_each_harness_once() {
        let mut catalog = HarnessCatalog::default();
        catalog.detect(["definitely-not-a-harness-xyz"]);
        assert_eq!(
            catalog.installed("definitely-not-a-harness-xyz"),
            Some(false)
        );
        assert_eq!(catalog.installed("claude"), None);
    }
}
