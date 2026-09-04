use std::{future::Future, io, time::Duration};

use agent_launcher_core::{RunEvent, RuntimeSnapshot, WorktreeDeletePreview};
use agent_launcher_runtime::RuntimeHandle;
use crossterm::{
    cursor::Show,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    Error,
    app::{AppState, DeleteOverlay, DispatchOverlay, InputOverlay, Route},
    metrics::HostMetricsSampler,
    render::draw,
    rows::{IssueSort, display_rows_matching},
};

const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
const METRICS_INTERVAL: Duration = Duration::from_secs(5);
const AGENT_ACTIVITY_INTERVAL: Duration = Duration::from_secs(1);

enum UiActionResult {
    Runtime {
        result: agent_launcher_runtime::Result<()>,
        success: &'static str,
        close_issue: Option<String>,
    },
    DeletePreview {
        request_id: u64,
        result: agent_launcher_runtime::Result<Box<WorktreeDeletePreview>>,
    },
}

type UiActionSender = tokio::sync::mpsc::UnboundedSender<UiActionResult>;

/// Runs the interactive terminal UI against a live runtime handle.
pub async fn run(runtime: RuntimeHandle) -> Result<(), Error> {
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;

    let mut output = io::stdout();
    execute!(output, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let mut events = EventStream::new();
    let mut snapshots = runtime.subscribe();
    let mut snapshot = runtime.snapshot();
    let mut app = AppState::default();
    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel();
    if snapshot.initialized {
        app.agent_activity.initialize(&snapshot);
    }
    let mut metrics_sampler = HostMetricsSampler::new();
    metrics_sampler.sample(&mut app.host_metrics);
    let mut ticker = tokio::time::interval(ANIMATION_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut metrics_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + METRICS_INTERVAL,
        METRICS_INTERVAL,
    );
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut agent_activity_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + AGENT_ACTIVITY_INTERVAL,
        AGENT_ACTIVITY_INTERVAL,
    );
    agent_activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let termination = termination_signal();
    tokio::pin!(termination);
    let mut needs_draw = true;

    loop {
        if needs_draw {
            terminal.draw(|frame| draw(frame, &snapshot, &mut app))?;
            needs_draw = false;
        }
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        let should_quit = handle_key(
                            &mut app,
                            key,
                            &snapshot,
                            &runtime,
                            &action_tx,
                        );
                        if should_quit {
                            break;
                        }
                        needs_draw = true;
                    },
                    Some(Ok(Event::Resize(_, _))) => needs_draw = true,
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let selected_key = app.selected_issue(&snapshot).map(|issue| issue.key.clone());
                snapshot = snapshots.borrow().clone();
                if snapshot.initialized && !app.agent_activity.is_initialized() {
                    app.agent_activity.initialize(&snapshot);
                }
                app.reconcile_selection(&snapshot, selected_key.as_ref());
                app.reconcile_detail(&snapshot);
                app.reconcile_dispatch(&snapshot);
                needs_draw = true;
            }
            _ = ticker.tick() => {
                app.tick = app.tick.wrapping_add(1);
                if animation_active(&snapshot, &app) {
                    needs_draw = true;
                }
            }
            _ = metrics_tick.tick() => {
                metrics_sampler.sample(&mut app.host_metrics);
                needs_draw = true;
            }
            _ = agent_activity_tick.tick() => {
                if snapshot.initialized {
                    app.agent_activity.record(&snapshot);
                }
                if app.route == Route::Inbox {
                    needs_draw = true;
                }
            }
            Some(result) = action_rx.recv() => {
                apply_ui_action_result(&mut app, result);
                needs_draw = true;
            }
            signal = &mut termination => {
                signal?;
                break;
            }
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

fn animation_active(snapshot: &RuntimeSnapshot, app: &AppState) -> bool {
    app.input_overlay.is_some()
        || app.route == Route::Inbox
        || snapshot.refreshing
        || snapshot.runs.iter().any(|run| run.state.is_active())
}

fn handle_key(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    if app.route == Route::Inbox
        && key.code == KeyCode::Char('g')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.dispatch_overlay.is_none()
    {
        app.sort_overlay = false;
        app.command_overlay = !app.command_overlay;
        return false;
    }
    if app.reconcile_detail(snapshot) {
        return false;
    }
    if app.reconcile_dispatch(snapshot) {
        return false;
    }
    if app.input_overlay.is_some() {
        return handle_input_key(app, key, runtime, actions);
    }
    if app.delete_overlay.is_some() {
        return handle_delete_key(app, key, runtime, actions);
    }
    if app.dispatch_overlay.is_some() {
        return handle_dispatch_key(app, key, snapshot, runtime, actions);
    }
    if app.sort_overlay {
        handle_sort_key(app, key);
        return false;
    }
    if app.command_overlay {
        return handle_inbox_command_key(app, key, snapshot, runtime, actions);
    }

    let rows = display_rows_matching(snapshot, &app.search_query, app.issue_sort);
    match (app.route, key.code) {
        (Route::Inbox, KeyCode::Esc) => return true,
        (Route::Detail, KeyCode::Esc) => app.reset_detail(),
        (Route::Inbox, KeyCode::Enter) if !rows.is_empty() => {
            app.open_detail(snapshot);
        },
        (Route::Detail, KeyCode::Char('r')) => {
            app.status_message = Some("refreshing issue sources...".to_owned());
            let runtime = runtime.clone();
            spawn_runtime_action(actions, "refresh complete", None, async move {
                runtime.refresh().await
            });
        },
        (Route::Detail, KeyCode::Char('d')) => {
            dispatch_selected(app, snapshot, runtime, actions);
        },
        (Route::Detail, KeyCode::Char('o')) => open_selected(app, snapshot, runtime, actions),
        (Route::Detail, KeyCode::Char('s')) => stop_selected(app, snapshot, runtime, actions),
        (Route::Detail, KeyCode::Char('i')) => open_input(app, snapshot),
        (Route::Detail, KeyCode::Char('x')) => {
            open_delete(app, snapshot, runtime, actions);
        },
        (Route::Inbox, KeyCode::Up) => select_previous(app, rows.len()),
        (Route::Inbox, KeyCode::Down) => select_next(app, rows.len()),
        (Route::Inbox, KeyCode::PageUp) => {
            app.selected = app.selected.saturating_sub(app.visible_rows.max(1));
        },
        (Route::Inbox, KeyCode::PageDown) => {
            app.selected = app
                .selected
                .saturating_add(app.visible_rows.max(1))
                .min(rows.len().saturating_sub(1));
        },
        (Route::Inbox, KeyCode::Home) => app.selected = 0,
        (Route::Inbox, KeyCode::End) => app.selected = rows.len().saturating_sub(1),
        (Route::Detail, KeyCode::Up) => {
            app.detail_scroll = app.detail_scroll.saturating_sub(1);
        },
        (Route::Detail, KeyCode::Down) => {
            app.detail_scroll = app
                .detail_scroll
                .saturating_add(1)
                .min(app.detail_scroll_max);
        },
        (Route::Detail, KeyCode::PageUp) => {
            app.detail_scroll = app.detail_scroll.saturating_sub(8);
        },
        (Route::Detail, KeyCode::PageDown) => {
            app.detail_scroll = app
                .detail_scroll
                .saturating_add(8)
                .min(app.detail_scroll_max);
        },
        (Route::Detail, KeyCode::Home) => app.detail_scroll = 0,
        (Route::Detail, KeyCode::End) => app.detail_scroll = app.detail_scroll_max,
        (Route::Inbox, KeyCode::Backspace) => {
            app.search_query.pop();
            app.selected = 0;
            app.scroll = 0;
        },
        (Route::Inbox, KeyCode::Char(character)) if printable(character, key.modifiers) => {
            app.search_query.push(character);
            app.selected = 0;
            app.scroll = 0;
        },
        _ => {},
    }
    false
}

fn handle_inbox_command_key(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    match key.code {
        KeyCode::Char('?') => return false,
        KeyCode::Char('d') => {
            app.command_overlay = false;
            dispatch_selected(app, snapshot, runtime, actions);
        },
        KeyCode::Char('r') => {
            app.command_overlay = false;
            app.status_message = Some("refreshing issue sources...".to_owned());
            let runtime = runtime.clone();
            spawn_runtime_action(actions, "refresh complete", None, async move {
                runtime.refresh().await
            });
        },
        KeyCode::Char('s') => {
            app.command_overlay = false;
            app.sort_cursor = IssueSort::ALL
                .iter()
                .position(|sort| *sort == app.issue_sort)
                .unwrap_or_default();
            app.sort_overlay = true;
        },
        KeyCode::Char('c') => {
            app.command_overlay = false;
            app.search_query.clear();
            app.selected = 0;
            app.scroll = 0;
            app.status_message = Some("search cleared".to_owned());
        },
        KeyCode::Char('q') => return true,
        KeyCode::Esc => app.command_overlay = false,
        _ => {
            app.command_overlay = false;
            app.status_message = Some("unknown launcher command; press Ctrl+G for keybinds".into());
        },
    }
    false
}

fn handle_sort_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.sort_overlay = false,
        KeyCode::Up => {
            app.sort_cursor = app
                .sort_cursor
                .checked_sub(1)
                .unwrap_or(IssueSort::ALL.len() - 1);
        },
        KeyCode::Down => app.sort_cursor = (app.sort_cursor + 1) % IssueSort::ALL.len(),
        KeyCode::Enter => apply_sort(app, IssueSort::ALL[app.sort_cursor]),
        KeyCode::Char(character @ '1'..='5') => {
            let index = character as usize - '1' as usize;
            apply_sort(app, IssueSort::ALL[index]);
        },
        _ => {},
    }
}

fn apply_sort(app: &mut AppState, sort: IssueSort) {
    app.issue_sort = sort;
    app.sort_cursor = IssueSort::ALL
        .iter()
        .position(|candidate| *candidate == sort)
        .unwrap_or_default();
    app.sort_overlay = false;
    app.selected = 0;
    app.scroll = 0;
    app.status_message = Some(format!("issues sorted {}", sort.label()));
}

fn handle_delete_key(
    app: &mut AppState,
    key: KeyEvent,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    match key.code {
        KeyCode::Esc => {
            app.delete_overlay = None;
            app.delete_confirmation_visible = false;
        },
        KeyCode::Enter => {
            if !app.delete_confirmation_visible {
                app.status_message = Some("resize the terminal to review deletion warnings".into());
                return false;
            }
            let Some(overlay) = app.delete_overlay.take() else {
                return false;
            };
            let success =
                if overlay.preview.action == agent_launcher_core::WorktreeDeleteAction::Archive {
                    "workspace archived and run history deleted"
                } else {
                    "worktree and run history deleted"
                };
            app.status_message = Some("removing worktree...".to_owned());
            app.delete_confirmation_visible = false;
            let runtime = runtime.clone();
            let close_issue = Some(overlay.preview.run.issue_key.clone());
            spawn_runtime_action(actions, success, close_issue, async move {
                runtime.delete_worktree(overlay.preview).await
            });
        },
        _ => {},
    }
    false
}

fn handle_input_key(
    app: &mut AppState,
    key: KeyEvent,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    match key.code {
        KeyCode::Esc => app.input_overlay = None,
        KeyCode::Enter => {
            let Some(overlay) = app.input_overlay.take() else {
                return false;
            };
            let text = overlay.text.trim();
            if text.is_empty() {
                app.status_message = Some("input cancelled: response was empty".to_owned());
            } else {
                let text = text.to_owned();
                app.status_message = Some("sending input...".to_owned());
                let runtime = runtime.clone();
                spawn_runtime_action(actions, "input sent", None, async move {
                    runtime.send_input(overlay.run_id, text).await
                });
            }
        },
        KeyCode::Backspace => {
            if let Some(overlay) = app.input_overlay.as_mut() {
                overlay.text.pop();
            }
        },
        KeyCode::Char(character) if printable(character, key.modifiers) => {
            if let Some(overlay) = app.input_overlay.as_mut() {
                overlay.text.push(character);
            }
        },
        _ => {},
    }
    false
}

fn dispatch_selected(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) {
    let Some(issue_key) = dispatch_target(app, snapshot) else {
        return;
    };
    if snapshot
        .runs
        .iter()
        .any(|run| run.issue_key == issue_key.canonical() && run.state.is_active())
    {
        app.status_message = Some("run already active".to_owned());
        return;
    }
    if snapshot.prompt_profiles.len() > 1 {
        app.status_message = None;
        app.dispatch_overlay = Some(DispatchOverlay {
            issue_key,
            cursor: 0,
        });
        return;
    }
    let profile = snapshot.prompt_profiles.first().cloned();
    start_dispatch(app, runtime, actions, issue_key, profile);
}

fn handle_dispatch_key(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    let profile_count = snapshot.prompt_profiles.len();
    match key.code {
        KeyCode::Esc => app.dispatch_overlay = None,
        KeyCode::Up if profile_count > 0 => {
            if let Some(overlay) = app.dispatch_overlay.as_mut() {
                overlay.cursor = overlay.cursor.checked_sub(1).unwrap_or(profile_count - 1);
            }
        },
        KeyCode::Down if profile_count > 0 => {
            if let Some(overlay) = app.dispatch_overlay.as_mut() {
                overlay.cursor = (overlay.cursor + 1) % profile_count;
            }
        },
        KeyCode::Char(character @ '1'..='9') => {
            let index = character as usize - '1' as usize;
            if index < profile_count {
                dispatch_from_overlay(app, snapshot, runtime, actions, index);
            }
        },
        KeyCode::Enter if profile_count > 0 => {
            let index = app
                .dispatch_overlay
                .as_ref()
                .map_or(0, |overlay| overlay.cursor.min(profile_count - 1));
            dispatch_from_overlay(app, snapshot, runtime, actions, index);
        },
        _ => {},
    }
    false
}

fn dispatch_from_overlay(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    profile_index: usize,
) {
    let Some(overlay) = app.dispatch_overlay.take() else {
        return;
    };
    let Some(profile) = snapshot.prompt_profiles.get(profile_index).cloned() else {
        app.status_message = Some("selected prompt profile is no longer available".into());
        return;
    };
    start_dispatch(app, runtime, actions, overlay.issue_key, Some(profile));
}

fn start_dispatch(
    app: &mut AppState,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    issue_key: agent_launcher_core::IssueKey,
    profile: Option<String>,
) {
    let status = profile.as_deref().map_or_else(
        || "dispatching agent...".to_owned(),
        |profile| format!("dispatching {profile}..."),
    );
    app.status_message = Some(status);
    let runtime = runtime.clone();
    spawn_runtime_action(actions, "agent dispatched", None, async move {
        runtime.dispatch(issue_key, profile).await
    });
}

fn dispatch_target(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
) -> Option<agent_launcher_core::IssueKey> {
    selected_action_issue(app, snapshot).map(|issue| issue.key.clone())
}

fn open_selected(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run_id) = app.latest_run(snapshot, issue).map(|run| run.id.clone()) else {
        app.status_message = Some("dispatch the issue before opening a workspace".to_owned());
        return;
    };
    app.status_message = Some("opening workspace...".to_owned());
    let runtime = runtime.clone();
    spawn_runtime_action(actions, "workspace opened", None, async move {
        runtime.open(run_id).await
    });
}

fn stop_selected(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run_id) = app.latest_run(snapshot, issue).map(|run| run.id.clone()) else {
        app.status_message = Some("no run to stop".to_owned());
        return;
    };
    app.status_message = Some("stopping run...".to_owned());
    let runtime = runtime.clone();
    spawn_runtime_action(actions, "run stopped", None, async move {
        runtime.stop(run_id).await
    });
}

fn open_input(app: &mut AppState, snapshot: &RuntimeSnapshot) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run) = app.latest_run(snapshot, issue) else {
        app.status_message = Some("dispatch the issue before sending input".to_owned());
        return;
    };
    let prompt = snapshot
        .run_events
        .get(&run.id)
        .and_then(|events| {
            events.iter().rev().find_map(|event| match &event.payload {
                RunEvent::InputRequested { prompt } => Some(prompt.clone()),
                RunEvent::PermissionRequested { description, .. } => Some(description.clone()),
                _ => None,
            })
        })
        .or_else(|| run.message.clone())
        .unwrap_or_else(|| "Send a follow-up or help response to this agent.".to_owned());
    app.input_overlay = Some(InputOverlay {
        run_id: run.id.clone(),
        prompt,
        text: String::new(),
    });
}

fn open_delete(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run_id) = app.latest_run(snapshot, issue).map(|run| run.id.clone()) else {
        app.status_message = Some("no worktree to delete".to_owned());
        return;
    };
    app.status_message = Some("checking worktree safety...".to_owned());
    app.next_request_id = app.next_request_id.wrapping_add(1);
    let request_id = app.next_request_id;
    app.delete_preview_request = Some(request_id);
    let runtime = runtime.clone();
    let actions = actions.clone();
    tokio::spawn(async move {
        let _ = actions.send(UiActionResult::DeletePreview {
            request_id,
            result: runtime.preview_delete_worktree(run_id).await.map(Box::new),
        });
    });
}

fn selected_action_issue<'a>(
    app: &mut AppState,
    snapshot: &'a RuntimeSnapshot,
) -> Option<&'a agent_launcher_core::Issue> {
    if app.reconcile_detail(snapshot) {
        return None;
    }
    let issue = match app.route {
        Route::Inbox => app.selected_issue(snapshot),
        Route::Detail => app.detail_issue(snapshot),
    };
    if issue.is_none() {
        app.status_message = Some("no issue selected".to_owned());
    }
    issue
}

fn set_result(app: &mut AppState, result: agent_launcher_runtime::Result<()>, success: &str) {
    app.status_message = Some(match result {
        Ok(()) => success.to_owned(),
        Err(error) => format!("runtime error: {error}"),
    });
}

fn spawn_runtime_action(
    actions: &UiActionSender,
    success: &'static str,
    close_issue: Option<String>,
    future: impl Future<Output = agent_launcher_runtime::Result<()>> + Send + 'static,
) {
    let actions = actions.clone();
    tokio::spawn(async move {
        let _ = actions.send(UiActionResult::Runtime {
            result: future.await,
            success,
            close_issue,
        });
    });
}

fn apply_ui_action_result(app: &mut AppState, result: UiActionResult) {
    match result {
        UiActionResult::Runtime {
            result,
            success,
            close_issue: Some(issue_key),
        } => apply_delete_result(app, result, success, &issue_key),
        UiActionResult::Runtime {
            result, success, ..
        } => set_result(app, result, success),
        UiActionResult::DeletePreview { request_id, .. }
            if app.delete_preview_request != Some(request_id) => {},
        UiActionResult::DeletePreview {
            result: Ok(preview),
            ..
        } => {
            app.delete_preview_request = None;
            if app.route != Route::Detail {
                return;
            }
            app.status_message = None;
            app.delete_overlay = Some(DeleteOverlay { preview: *preview });
        },
        UiActionResult::DeletePreview {
            result: Err(error), ..
        } => {
            app.delete_preview_request = None;
            app.status_message = Some(format!("runtime error: {error}"));
        },
    }
}

fn apply_delete_result(
    app: &mut AppState,
    result: agent_launcher_runtime::Result<()>,
    success: &str,
    issue_key: &str,
) {
    match result {
        Ok(()) => {
            if app
                .detail_issue_key
                .as_ref()
                .is_some_and(|current| current.canonical() == issue_key)
            {
                app.reset_detail();
            }
            app.status_message = Some(success.to_owned());
        },
        Err(error) => app.status_message = Some(format!("runtime error: {error}")),
    }
}

fn select_previous(app: &mut AppState, count: usize) {
    if count == 0 {
        app.selected = 0;
    } else if app.selected == 0 {
        app.selected = count - 1;
    } else {
        app.selected -= 1;
    }
}

fn select_next(app: &mut AppState, count: usize) {
    if count == 0 {
        app.selected = 0;
    } else {
        app.selected = (app.selected + 1) % count;
    }
}

fn printable(character: char, modifiers: KeyModifiers) -> bool {
    !character.is_control()
        && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

#[cfg(unix)]
async fn termination_signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        _ = terminate.recv() => {},
        _ = hangup.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn termination_signal() -> io::Result<()> {
    std::future::pending().await
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::{
        Issue, IssueKey, IssueProvider, RunState, RunSummary, RuntimeSnapshot,
        WorktreeDeleteAction, WorktreeDeletePreview,
    };
    use chrono::{Duration, Utc};

    use super::*;

    fn issue(id: &str, age: Duration) -> Issue {
        Issue {
            key: IssueKey {
                provider: IssueProvider::Github,
                host: "github.com".to_owned(),
                repository: "acme/launcher".to_owned(),
                native_id: id.to_owned(),
            },
            identifier: format!("#{id}"),
            title: format!("Issue {id}"),
            description: None,
            state: "open".to_owned(),
            url: None,
            author: None,
            labels: Vec::new(),
            parent_id: None,
            blocked_by: Vec::new(),
            priority: None,
            created_at: Some(Utc::now() - age),
            updated_at: None,
        }
    }

    #[test]
    fn detail_dispatch_never_retargets_after_reorder_and_removal() {
        let first = issue("1", Duration::days(2));
        let selected = issue("2", Duration::days(1));
        let selected_key = selected.key.clone();
        let mut app = AppState {
            selected: 0,
            ..AppState::default()
        };
        let initial = RuntimeSnapshot {
            issues: vec![first.clone(), selected.clone()],
            ..RuntimeSnapshot::default()
        };

        assert!(app.open_detail(&initial));
        assert_eq!(app.detail_issue_key.as_ref(), Some(&selected_key));

        let mut reordered_first = first.clone();
        reordered_first.created_at = Some(Utc::now());
        let mut reordered_selected = selected;
        reordered_selected.created_at = Some(Utc::now() - Duration::days(3));
        let reordered = RuntimeSnapshot {
            issues: vec![reordered_first.clone(), reordered_selected],
            ..RuntimeSnapshot::default()
        };
        assert_eq!(
            app.selected_issue(&reordered).map(|issue| &issue.key),
            Some(&reordered_first.key)
        );
        assert_eq!(
            dispatch_target(&mut app, &reordered),
            Some(selected_key.clone())
        );

        let replacement = issue("3", Duration::hours(-1));
        let replacement_key = replacement.key.clone();
        let removed = RuntimeSnapshot {
            issues: vec![reordered_first, replacement],
            ..RuntimeSnapshot::default()
        };
        assert_eq!(
            app.selected_issue(&removed).map(|issue| &issue.key),
            Some(&replacement_key)
        );
        assert_eq!(dispatch_target(&mut app, &removed), None);
        assert_eq!(app.route, Route::Inbox);
        assert_eq!(app.detail_issue_key, None);
        assert_eq!(
            app.status_message.as_deref(),
            Some("selected issue is no longer available; returned to inbox")
        );
    }

    #[test]
    fn successful_delete_closes_detail_but_failure_keeps_it_open() {
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(issue("1", Duration::seconds(0)).key),
            ..AppState::default()
        };

        apply_delete_result(
            &mut app,
            Err(agent_launcher_runtime::Error::RunNotFound(
                "run-1".to_owned(),
            )),
            "deleted",
            &issue("1", Duration::zero()).key.canonical(),
        );
        assert_eq!(app.route, Route::Detail);
        assert!(app.status_message.as_deref().unwrap().contains("run-1"));

        apply_delete_result(
            &mut app,
            Ok(()),
            "deleted",
            &issue("1", Duration::zero()).key.canonical(),
        );
        assert_eq!(app.route, Route::Inbox);
        assert_eq!(app.detail_issue_key, None);
        assert_eq!(app.status_message.as_deref(), Some("deleted"));
    }

    #[test]
    fn completed_delete_does_not_close_an_unrelated_detail() {
        let current = issue("2", Duration::zero()).key;
        let deleted = issue("1", Duration::zero()).key.canonical();
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(current.clone()),
            ..AppState::default()
        };

        apply_delete_result(&mut app, Ok(()), "deleted", &deleted);

        assert_eq!(app.route, Route::Detail);
        assert_eq!(app.detail_issue_key.as_ref(), Some(&current));
        assert_eq!(app.status_message.as_deref(), Some("deleted"));
    }

    #[test]
    fn cancelled_delete_preview_cannot_reappear_in_the_inbox() {
        let mut app = AppState {
            route: Route::Detail,
            delete_preview_request: Some(7),
            ..AppState::default()
        };
        app.reset_detail();
        let now = Utc::now();
        let preview = WorktreeDeletePreview {
            run: RunSummary {
                id: "run-1".to_owned(),
                issue_key: issue("1", Duration::zero()).key.canonical(),
                workspace: None,
                agent: "opencode".to_owned(),
                state: RunState::Running,
                message: None,
                session_id: None,
                started_at: now,
                updated_at: now,
            },
            action: WorktreeDeleteAction::Delete,
            has_uncommitted_changes: false,
            has_ignored_files: false,
            unpushed_commits: 0,
            inspection_warning: None,
            inspection_fingerprint: Some("fingerprint".to_owned()),
        };

        apply_ui_action_result(&mut app, UiActionResult::DeletePreview {
            request_id: 7,
            result: Ok(Box::new(preview)),
        });

        assert_eq!(app.route, Route::Inbox);
        assert!(app.delete_overlay.is_none());
        assert!(app.delete_preview_request.is_none());
    }

    #[test]
    fn snapshot_reordering_preserves_the_selected_issue() {
        let selected = issue("2", Duration::days(1));
        let selected_key = selected.key.clone();
        let mut app = AppState::default();
        let initial = RuntimeSnapshot {
            issues: vec![issue("1", Duration::days(2)), selected],
            ..RuntimeSnapshot::default()
        };
        assert_eq!(
            app.selected_issue(&initial).map(|issue| &issue.key),
            Some(&selected_key)
        );

        let refreshed = RuntimeSnapshot {
            issues: vec![
                issue("1", Duration::days(2)),
                issue("2", Duration::days(1)),
                issue("3", Duration::zero()),
            ],
            ..RuntimeSnapshot::default()
        };
        app.reconcile_selection(&refreshed, Some(&selected_key));

        assert_eq!(
            app.selected_issue(&refreshed).map(|issue| &issue.key),
            Some(&selected_key)
        );
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn prompt_chooser_closes_if_its_issue_disappears() {
        let selected = issue("2", Duration::zero());
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                issue_key: selected.key,
                cursor: 0,
            }),
            ..AppState::default()
        };
        let snapshot = RuntimeSnapshot {
            issues: vec![issue("1", Duration::zero())],
            prompt_profiles: vec!["implementer".to_owned(), "reviewer".to_owned()],
            ..RuntimeSnapshot::default()
        };

        assert!(app.reconcile_dispatch(&snapshot));
        assert!(app.dispatch_overlay.is_none());
        assert!(app.status_message.as_deref().unwrap().contains("closed"));
    }

    #[tokio::test]
    async fn runtime_actions_complete_in_the_background() {
        let (actions, mut results) = tokio::sync::mpsc::unbounded_channel();
        let (release, wait) = tokio::sync::oneshot::channel();

        spawn_runtime_action(&actions, "complete", None, async move {
            wait.await.expect("test should release the action");
            Ok(())
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), results.recv())
                .await
                .is_err()
        );
        release.send(()).expect("action should still be running");
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), results.recv())
            .await
            .expect("action should finish")
            .expect("result channel should remain open");
        assert!(matches!(result, UiActionResult::Runtime {
            result: Ok(()),
            success: "complete",
            close_issue: None,
        }));
    }

    #[test]
    fn applying_sort_resets_navigation_and_reports_the_mode() {
        let mut app = AppState {
            selected: 8,
            scroll: 5,
            sort_overlay: true,
            ..AppState::default()
        };

        apply_sort(&mut app, IssueSort::Priority);

        assert_eq!(app.issue_sort, IssueSort::Priority);
        assert_eq!(app.selected, 0);
        assert_eq!(app.scroll, 0);
        assert!(!app.sort_overlay);
        assert_eq!(
            app.status_message.as_deref(),
            Some("issues sorted priority")
        );
    }
}
