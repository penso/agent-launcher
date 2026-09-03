use agent_launcher_core::RunState;
use ratatui::style::Color;

use crate::theme;

pub(crate) fn issue_icon(state: &str) -> &'static str {
    match state.to_ascii_lowercase().as_str() {
        "open" | "opened" | "in_progress" | "in progress" | "started" => "●",
        "todo" | "unstarted" | "backlog" => "○",
        "waiting" | "blocked" => "◷",
        "closed" | "done" | "completed" => "✓",
        "cancelled" | "canceled" => "⊘",
        "draft" => "◌",
        _ => "·",
    }
}

pub(crate) fn issue_color(state: &str) -> Color {
    match state.to_ascii_lowercase().as_str() {
        "closed" | "done" | "completed" => theme::done(),
        "draft" => theme::error(),
        "open" | "opened" | "in_progress" | "in progress" | "started" | "todo" | "unstarted"
        | "backlog" | "waiting" | "blocked" => theme::primary(),
        _ => theme::muted(),
    }
}

pub(crate) const fn run_label(state: RunState) -> &'static str {
    match state {
        RunState::Provisioning => "provisioning",
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::NeedsInput => "needs input",
        RunState::Idle => "idle",
        RunState::Completed => "completed",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Disconnected => "disconnected",
    }
}

pub(crate) const fn run_color(state: RunState) -> Color {
    match state {
        RunState::Completed => theme::done(),
        RunState::NeedsInput | RunState::Failed | RunState::Disconnected => theme::error(),
        RunState::Provisioning | RunState::Starting | RunState::Running => theme::primary(),
        RunState::Idle | RunState::Cancelled => theme::muted(),
    }
}
