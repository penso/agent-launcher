use std::{
    fs::{self, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use agent_launcher_core::{BackendKind, RuntimeSnapshot};
use chrono::Utc;

const MAX_BYTES: u64 = 1024 * 1024;

/// A deliberately narrow diagnostic sink, not a sink for arbitrary tracing fields.
/// Never pass external strings as operation/outcome: errors and output are omitted.
#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    last_failure: Option<String>,
    write_error: Option<String>,
}

impl Diagnostics {
    pub fn new(path: PathBuf) -> Self {
        let diagnostics = Self(Arc::new(Mutex::new(State {
            path: Some(path),
            ..State::default()
        })));
        diagnostics.record("diagnostics", None, None, "started");
        diagnostics
    }

    pub fn record(
        &self,
        operation: &'static str,
        backend: Option<BackendKind>,
        identity: Option<&str>,
        outcome: &'static str,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let failure = matches!(outcome, "failed" | "unavailable" | "throttled");
        let severity = if failure || outcome == "degraded" {
            "WARN"
        } else {
            "INFO"
        };
        let mut hash = DefaultHasher::new();
        identity.hash(&mut hash);
        let entry = format!(
            "{} {severity} operation={operation} backend={backend:?} ref={:016x} outcome={outcome}",
            Utc::now().to_rfc3339(),
            hash.finish()
        );
        if failure {
            state.last_failure = Some(format!(
                "{entry}; details omitted for privacy; check configuration/connectivity, then retry. Debug: Ctrl+G g"
            ));
        }
        let Some(path) = state.path.as_ref() else {
            return;
        };
        let result = (|| -> io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            if fs::metadata(path)
                .is_ok_and(|metadata| metadata.len() + entry.len() as u64 + 1 > MAX_BYTES)
            {
                let backup = path.with_extension("log.1");
                match fs::remove_file(&backup) {
                    Ok(()) => {},
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error),
                }
                fs::rename(path, backup)?;
            }
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            writeln!(options.open(path)?, "{entry}")
        })();
        if let Err(error) = result {
            // ErrorKind is bounded and cannot embed paths, credentials, or subprocess output.
            let warning = format!(
                "Diagnostic log unavailable ({:?}); fallback: stderr. Check data directory permissions/disk space. Debug: Ctrl+G g",
                error.kind()
            );
            if state.write_error.as_ref() != Some(&warning) {
                let _ = writeln!(io::stderr().lock(), "agent-launcher: {warning}");
            }
            let _ = writeln!(io::stderr().lock(), "{entry}");
            state.write_error = Some(warning);
        } else {
            state.write_error = None;
        }
    }

    pub fn apply(&self, snapshot: &mut RuntimeSnapshot) {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        snapshot.diagnostic_log_path = state.path.clone();
        snapshot.last_failure = state.last_failure.clone();
        snapshot.diagnostic_log_error = state.write_error.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_safe_failures_rotates_and_reports_unwritable_sink() {
        let root = std::env::temp_dir().join(format!(
            "launcher-diagnostics-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let path = root.join("diagnostics.log");
        let log = Diagnostics::new(path.clone());
        let sensitive = "prompt=secret Authorization: Bearer token https://user:password@host/?token=secret\nterminal output --password=secret";
        log.record(
            "dispatch",
            Some(BackendKind::Native),
            Some(sensitive),
            "failed",
        );
        let text = fs::read_to_string(&path).unwrap();
        let reopened = Diagnostics::new(path.clone());
        reopened.record("review", None, None, "succeeded");
        assert!(fs::read_to_string(&path).unwrap().starts_with(&text));
        assert!(text.contains("operation=dispatch"));
        assert!(text.contains("outcome=failed"));
        for secret in [
            "prompt",
            "Authorization",
            "token",
            "password",
            "terminal",
            "https",
        ] {
            assert!(!text.contains(secret));
        }
        let mut snapshot = RuntimeSnapshot::default();
        log.apply(&mut snapshot);
        assert_eq!(snapshot.diagnostic_log_path, Some(path.clone()));
        assert!(
            snapshot
                .last_failure
                .as_deref()
                .unwrap()
                .contains("dispatch")
        );
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES)
            .unwrap();
        log.record("stop", None, None, "succeeded");
        assert!(path.with_extension("log.1").exists());
        assert!(fs::metadata(&path).unwrap().len() < MAX_BYTES);
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES)
            .unwrap();
        log.record("open", None, None, "succeeded");
        assert_eq!(
            fs::metadata(path.with_extension("log.1")).unwrap().len(),
            MAX_BYTES
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        let broken = Diagnostics::new(path.join("not-a-directory.log"));
        broken.apply(&mut snapshot);
        assert!(
            snapshot
                .diagnostic_log_error
                .as_deref()
                .unwrap()
                .contains("stderr")
        );
        broken.record("open", None, None, "failed");
        broken.apply(&mut snapshot);
        assert!(snapshot.diagnostic_log_error.unwrap().contains("stderr"));
        assert!(snapshot.last_failure.unwrap().contains("open"));
        fs::remove_dir_all(root).unwrap();
    }
}
