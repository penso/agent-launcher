use agent_launcher_core::RunState;
use ratatui::style::Color;

use crate::theme;

pub(crate) fn issue_icon(state: &str) -> &'static str {
    match state.to_ascii_lowercase().as_str() {
        "open" | "opened" | "in_progress" | "in progress" | "started" => "●",
        "todo" | "unstarted" | "backlog" => "○",
        "waiting" | "blocked" => "◷",
        "closed" | "merged" | "done" | "completed" => "✓",
        "cancelled" | "canceled" => "⊘",
        "draft" => "◌",
        _ => "·",
    }
}

pub(crate) fn issue_color(state: &str) -> Color {
    match state.to_ascii_lowercase().as_str() {
        "closed" | "merged" | "done" | "completed" => theme::done(),
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
        RunState::Provisioning | RunState::Starting | RunState::Running => theme::status_working(),
        RunState::NeedsInput => theme::status_blocked(),
        RunState::Idle => theme::status_idle(),
        RunState::Completed => theme::done(),
        RunState::Failed | RunState::Disconnected => theme::error(),
        RunState::Cancelled => theme::muted(),
    }
}

/// Run state in at most eight cells for list rows, in Herdr's vocabulary
/// (working / idle / blocked / done).
pub(crate) const fn run_short_label(state: RunState) -> &'static str {
    match state {
        RunState::Provisioning => "setup",
        RunState::Starting => "starting",
        RunState::Running => "working",
        RunState::NeedsInput => "blocked",
        RunState::Idle => "idle",
        RunState::Completed => "done",
        RunState::Failed => "failed",
        RunState::Cancelled => "stopped",
        RunState::Disconnected => "offline",
    }
}

/// One-cell mark for the harness that runs a dispatch, after Herdr's agent list.
pub(crate) fn agent_glyph(agent: &str) -> &'static str {
    match agent.to_ascii_lowercase().as_str() {
        "claude" => "✳",
        "opencode" => "▯",
        "codex" => "◎",
        "gemini" => "✦",
        "pi" => "π",
        _ => "◆",
    }
}

/// P0 red, P1 orange, P2 yellow; lower priorities stay quiet.
pub(crate) const fn priority_color(priority: i64) -> Color {
    match priority {
        i64::MIN..=0 => theme::error(),
        1 => theme::primary(),
        2 => theme::warning(),
        _ => theme::muted(),
    }
}
