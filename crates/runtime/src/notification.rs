use agent_launcher_core::{RunState, RunSummary};

/// Delivers a desktop notification for a run requiring attention.
///
/// Implementations may block. The runtime always invokes this method on
/// Tokio's blocking pool.
pub trait DesktopNotifier: Send + Sync + 'static {
    fn notify(&self, run: &RunSummary) -> std::result::Result<(), String>;
}

/// Desktop notifier backed by the operating system's notification service.
#[derive(Clone, Copy, Debug, Default)]
pub struct NotifyRustNotifier;

impl DesktopNotifier for NotifyRustNotifier {
    fn notify(&self, run: &RunSummary) -> std::result::Result<(), String> {
        let summary = match run.state {
            RunState::NeedsInput => "Agent needs input",
            RunState::Failed => "Agent run failed",
            RunState::Disconnected => "Agent disconnected",
            _ => return Ok(()),
        };
        let body = run.message.as_deref().unwrap_or(&run.issue_key);
        notify_rust::Notification::new()
            .appname("Agent Launcher")
            .summary(summary)
            .body(body)
            .show()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Notification implementation useful when notifications are disabled or in tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopNotifier;

impl DesktopNotifier for NoopNotifier {
    fn notify(&self, _run: &RunSummary) -> std::result::Result<(), String> {
        Ok(())
    }
}
