use std::{io, time::Duration};

use agent_launcher_core::{RunEvent, RuntimeSnapshot};
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
    app::{AppState, InputOverlay, Route},
    render::draw,
    rows::display_rows_matching,
};

const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);

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
    let mut ticker = tokio::time::interval(ANIMATION_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                        let should_quit = tokio::select! {
                            should_quit = handle_key(&mut app, key, &snapshot, &runtime) => should_quit,
                            signal = &mut termination => {
                                signal?;
                                true
                            },
                        };
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
                snapshot = snapshots.borrow().clone();
                app.clamp_selection(&snapshot);
                app.reconcile_detail(&snapshot);
                needs_draw = true;
            }
            _ = ticker.tick() => {
                app.tick = app.tick.wrapping_add(1);
                if animation_active(&snapshot, &app) {
                    needs_draw = true;
                }
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

async fn handle_key(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    if app.reconcile_detail(snapshot) {
        return false;
    }
    if app.input_overlay.is_some() {
        return handle_input_key(app, key, runtime).await;
    }

    let rows = display_rows_matching(snapshot, &app.search_query);
    match (app.route, key.code) {
        (Route::Inbox, KeyCode::Esc) if !app.search_query.is_empty() => {
            app.search_query.clear();
            app.selected = 0;
            app.scroll = 0;
        },
        (Route::Inbox, KeyCode::Esc) => return true,
        (Route::Detail, KeyCode::Esc) => app.reset_detail(),
        (Route::Inbox, KeyCode::Enter) if !rows.is_empty() => {
            app.open_detail(snapshot);
        },
        (_, KeyCode::Char('r')) if app.search_query.is_empty() => {
            set_result(app, runtime.refresh().await, "refresh complete");
        },
        (_, KeyCode::Char('d')) if app.route == Route::Detail || app.search_query.is_empty() => {
            dispatch_selected(app, snapshot, runtime).await;
        },
        (Route::Detail, KeyCode::Char('o')) => open_selected(app, snapshot, runtime).await,
        (Route::Detail, KeyCode::Char('s')) => stop_selected(app, snapshot, runtime).await,
        (Route::Detail, KeyCode::Char('i')) => open_input(app, snapshot),
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

async fn handle_input_key(app: &mut AppState, key: KeyEvent, runtime: &RuntimeHandle) -> bool {
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
                set_result(
                    app,
                    runtime.send_input(overlay.run_id, text).await,
                    "input sent",
                );
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

async fn dispatch_selected(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
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
    set_result(app, runtime.dispatch(issue_key).await, "agent dispatched");
}

fn dispatch_target(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
) -> Option<agent_launcher_core::IssueKey> {
    selected_action_issue(app, snapshot).map(|issue| issue.key.clone())
}

async fn open_selected(app: &mut AppState, snapshot: &RuntimeSnapshot, runtime: &RuntimeHandle) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run_id) = app.latest_run(snapshot, issue).map(|run| run.id.clone()) else {
        app.status_message = Some("dispatch the issue before opening a workspace".to_owned());
        return;
    };
    set_result(app, runtime.open(run_id).await, "workspace opened");
}

async fn stop_selected(app: &mut AppState, snapshot: &RuntimeSnapshot, runtime: &RuntimeHandle) {
    let Some(issue) = selected_action_issue(app, snapshot) else {
        return;
    };
    let Some(run_id) = app.latest_run(snapshot, issue).map(|run| run.id.clone()) else {
        app.status_message = Some("no run to stop".to_owned());
        return;
    };
    set_result(app, runtime.stop(run_id).await, "run stopped");
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
    use agent_launcher_core::{Issue, IssueKey, IssueProvider, RuntimeSnapshot};
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
            selected: 1,
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
}
