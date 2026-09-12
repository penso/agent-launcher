use std::{future::Future, io, time::Duration};

use agent_launcher_core::{
    BackendKind, ComputeTargetAvailability, RunEvent, RunState, RuntimeSnapshot,
    WorktreeDeletePreview,
};
use agent_launcher_runtime::RuntimeHandle;
use crossterm::{
    cursor::Show,
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    style::Print,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    Error, LayoutMode,
    app::{AppState, DeleteOverlay, DispatchOverlay, DispatchStage, InputOverlay, Route},
    metrics::HostMetricsSampler,
    render::draw,
    rows::IssueSort,
};

const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
const METRICS_INTERVAL: Duration = Duration::from_secs(5);
const AGENT_ACTIVITY_INTERVAL: Duration = Duration::from_secs(1);
// Xterm title-stack operations; ignored by terminals without title-stack support.
const PUSH_TITLE: Print<&str> = Print("\x1b[22;0t");
const POP_TITLE: Print<&str> = Print("\x1b[23;0t");
const LAUNCHER_TITLE: SetTitle<&str> = SetTitle("launcher");

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
pub async fn run(runtime: RuntimeHandle, layout: LayoutMode) -> Result<(), Error> {
    let mut cleanup = TerminalCleanup { title_saved: false };
    enable_raw_mode()?;

    let mut output = io::stdout();
    execute!(output, EnterAlternateScreen, EnableMouseCapture)?;
    execute!(output, PUSH_TITLE)?;
    cleanup.title_saved = true;
    execute!(output, LAUNCHER_TITLE)?;
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;
    if std::env::var("HERDR_ENV").as_deref() == Ok("1") {
        // Cosmetic only: an unavailable host must not prevent the launcher from starting.
        let _ = tokio::time::timeout(Duration::from_secs(1), rename_herdr_tab()).await;
    }

    let mut events = EventStream::new();
    let mut snapshots = runtime.subscribe();
    let mut snapshot = runtime.snapshot();
    let mut app = AppState {
        layout,
        ..Default::default()
    };
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
                    Some(Ok(Event::Mouse(mouse))) => {
                        // Query the live size too: a mouse event can precede a queued resize.
                        match crossterm::terminal::size() {
                            Ok(size) if size == (app.mouse.screen.width, app.mouse.screen.height) => {
                                needs_draw = crate::mouse::handle_mouse(&mut app, mouse, &snapshot, size);
                            },
                            _ => {
                                app.mouse = Default::default();
                                needs_draw = true;
                            },
                        }
                    },
                    Some(Ok(Event::Resize(_, _))) => {
                        app.mouse = Default::default();
                        needs_draw = true;
                    },
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(error.into()),
                    None => break,
                }
            }
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let next = snapshots.borrow().clone();
                app.reconcile_lists(&snapshot, &next);
                snapshot = next;
                if snapshot.initialized && !app.agent_activity.is_initialized() {
                    app.agent_activity.initialize(&snapshot);
                }
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

async fn rename_herdr_tab() -> Option<()> {
    // Resolve the caller, not UI focus: inherited tab IDs can be missing or stale.
    let output = tokio::process::Command::new("herdr")
        .args(["pane", "current", "--current"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let tab_id = response.pointer("/result/pane/tab_id")?.as_str()?;
    herdr_tab_title_command(Some("1"), Some(tab_id))?
        .status()
        .await
        .ok()?
        .success()
        .then_some(())
}

fn herdr_tab_title_command(
    env: Option<&str>,
    tab_id: Option<&str>,
) -> Option<tokio::process::Command> {
    let tab_id = tab_id.filter(|id| !id.trim().is_empty())?;
    if env != Some("1") {
        return None;
    }
    // Never use UI focus or enumerate sibling tabs.
    let mut command = tokio::process::Command::new("herdr");
    command
        .args(["tab", "rename", "--", tab_id, "launcher"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    Some(command)
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
    if handle_debug_key(app, key) {
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

    if app.route == Route::Inbox {
        return handle_list_key(app, key, snapshot);
    }
    match (app.route, key.code) {
        (Route::Detail, KeyCode::Esc) => app.reset_detail(),
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
        _ => {},
    }
    false
}

fn handle_list_key(app: &mut AppState, key: KeyEvent, snapshot: &RuntimeSnapshot) -> bool {
    let count = app.rows(snapshot).len();
    match key.code {
        KeyCode::Esc => return true,
        KeyCode::Tab | KeyCode::BackTab => {
            app.switch_tab();
            app.status_message = None;
        },
        KeyCode::Enter => {
            app.open_detail(snapshot);
        },
        KeyCode::Up => select_previous(app, count),
        KeyCode::Down => select_next(app, count),
        KeyCode::PageUp => app.selected = app.selected.saturating_sub(app.visible_rows.max(1)),
        KeyCode::PageDown => {
            app.selected = app
                .selected
                .saturating_add(app.visible_rows.max(1))
                .min(count.saturating_sub(1))
        },
        KeyCode::Home => app.selected = 0,
        KeyCode::End => app.selected = count.saturating_sub(1),
        KeyCode::Backspace => {
            app.search_query.pop();
            app.selected = 0;
            app.scroll = 0;
        },
        KeyCode::Char(character) if printable(character, key.modifiers) => {
            app.search_query.push(character);
            app.selected = 0;
            app.scroll = 0;
        },
        _ => {},
    }
    false
}

fn handle_debug_key(app: &mut AppState, key: KeyEvent) -> bool {
    if app.debug_overlay {
        let step = match key.code {
            KeyCode::PageUp | KeyCode::PageDown => app.debug_page_size.max(1),
            _ => 1,
        };
        match key.code {
            KeyCode::Esc => app.debug_overlay = false,
            KeyCode::Up | KeyCode::PageUp => {
                app.debug_scroll = app.debug_scroll.saturating_sub(step);
            },
            KeyCode::Down | KeyCode::PageDown => {
                app.debug_scroll = app
                    .debug_scroll
                    .saturating_add(step)
                    .min(app.debug_scroll_max);
            },
            KeyCode::Home => app.debug_scroll = 0,
            KeyCode::End => app.debug_scroll = app.debug_scroll_max,
            _ => {},
        }
        return true;
    }
    if app.command_overlay && key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
        app.command_overlay = false;
        app.debug_overlay = true;
        app.debug_scroll = 0;
        app.debug_scroll_max = 0;
        app.debug_page_size = 0;
        return true;
    }
    if app.route == Route::Inbox
        && key.code == KeyCode::Char('g')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.dispatch_overlay.is_none()
    {
        app.sort_overlay = false;
        app.command_overlay = !app.command_overlay;
        return true;
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
    if let Some(action) = prepare_dispatch(app, snapshot) {
        start_launch(app, runtime, actions, action);
    }
}

fn prepare_dispatch(app: &mut AppState, snapshot: &RuntimeSnapshot) -> Option<LaunchAction> {
    let issue_key = dispatch_target(app, snapshot)?;
    if let Some(run) = snapshot.runs.iter().find(|run| {
        run.issue_key == issue_key.canonical()
            && (run.state.is_active()
                || (matches!(run.state, RunState::Failed | RunState::Disconnected)
                    && run
                        .workspace
                        .as_ref()
                        .is_some_and(|workspace| workspace.backend == BackendKind::Native)))
    }) {
        app.status_message = Some(if run.state.is_active() {
            "run already active".to_owned()
        } else {
            "existing native run must be resumed or deleted before dispatching again".to_owned()
        });
        return None;
    }
    if let Some(overlay) = staged_dispatch_overlay(issue_key.clone(), snapshot) {
        app.status_message = None;
        app.dispatch_overlay = Some(overlay);
        return None;
    }
    let profile = snapshot.prompt_profiles.first().cloned();
    Some(launch_action(snapshot, issue_key, profile, None))
}

fn is_pull_request(snapshot: &RuntimeSnapshot, key: &agent_launcher_core::IssueKey) -> bool {
    snapshot
        .issues
        .iter()
        .any(|issue| issue.key == *key && issue.pull_request.is_some())
}

fn staged_dispatch_overlay(
    issue_key: agent_launcher_core::IssueKey,
    snapshot: &RuntimeSnapshot,
) -> Option<DispatchOverlay> {
    let review = is_pull_request(snapshot, &issue_key);
    let stage = if !review && snapshot.prompt_profiles.len() > 1 {
        DispatchStage::Prompt
    } else if has_target_stage(snapshot) {
        DispatchStage::Target {
            profile: if review {
                None
            } else {
                snapshot.prompt_profiles.first().cloned()
            },
        }
    } else {
        return None;
    };
    Some(DispatchOverlay {
        issue_key,
        cursor: 0,
        stage,
    })
}

fn handle_dispatch_key(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) -> bool {
    let option_count = dispatch_option_count(app, snapshot);
    match key.code {
        KeyCode::Esc => app.dispatch_overlay = None,
        KeyCode::Up if option_count > 0 => move_dispatch_cursor(app, snapshot, false),
        KeyCode::Down if option_count > 0 => move_dispatch_cursor(app, snapshot, true),
        KeyCode::Char(character @ '1'..='9') => {
            let index = character as usize - '1' as usize;
            if index < option_count {
                if let Some(overlay) = app.dispatch_overlay.as_mut() {
                    overlay.cursor = index;
                }
                dispatch_from_overlay(app, snapshot, runtime, actions, index);
            }
        },
        KeyCode::Enter if option_count > 0 => {
            let index = app
                .dispatch_overlay
                .as_ref()
                .map_or(0, |overlay| overlay.cursor.min(option_count - 1));
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
    index: usize,
) {
    if let Some(action) = select_dispatch(app, snapshot, index) {
        start_launch(app, runtime, actions, action);
    }
}

fn select_dispatch(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    index: usize,
) -> Option<LaunchAction> {
    let stage = app.dispatch_overlay.as_ref()?.stage.clone();
    match stage {
        DispatchStage::Prompt => {
            let (issue_key, profile) = select_prompt(app, snapshot, index)?;
            Some(launch_action(snapshot, issue_key, Some(profile), None))
        },
        DispatchStage::Target { profile } => {
            let target = select_target(app, snapshot, index).ok()?;
            let issue_key = app
                .dispatch_overlay
                .as_ref()
                .expect("overlay exists")
                .issue_key
                .clone();
            app.dispatch_overlay = None;
            Some(launch_action(snapshot, issue_key, profile, target))
        },
    }
}

fn select_target(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    index: usize,
) -> Result<Option<String>, ()> {
    if index == 0 {
        if snapshot
            .compute_targets
            .iter()
            .any(|target| target.is_dispatchable())
        {
            return Ok(None);
        }
        app.status_message =
            Some("automatic target unavailable: no compute targets are dispatchable".into());
        return Err(());
    }
    let Some(target) = snapshot.compute_targets.get(index - 1) else {
        app.status_message = Some("selected compute target is no longer available".into());
        return Err(());
    };
    if !target.is_dispatchable() {
        app.status_message = Some(target_unavailable_message(target));
        return Err(());
    }
    Ok(Some(target.id.clone()))
}

fn select_prompt(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    index: usize,
) -> Option<(agent_launcher_core::IssueKey, String)> {
    let Some(profile) = snapshot.prompt_profiles.get(index).cloned() else {
        app.status_message = Some("selected prompt profile is no longer available".into());
        return None;
    };
    let overlay = app.dispatch_overlay.as_mut()?;
    if has_target_stage(snapshot) {
        overlay.cursor = 0;
        overlay.stage = DispatchStage::Target {
            profile: Some(profile),
        };
        app.status_message = None;
        None
    } else {
        let issue_key = overlay.issue_key.clone();
        app.dispatch_overlay = None;
        Some((issue_key, profile))
    }
}

fn has_target_stage(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.selected_backend == Some(BackendKind::Native) && !snapshot.compute_targets.is_empty()
}

fn dispatch_option_count(app: &AppState, snapshot: &RuntimeSnapshot) -> usize {
    app.dispatch_overlay
        .as_ref()
        .map_or(0, |overlay| match &overlay.stage {
            DispatchStage::Prompt => snapshot.prompt_profiles.len(),
            DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
        })
}

fn move_dispatch_cursor(app: &mut AppState, snapshot: &RuntimeSnapshot, forward: bool) {
    let Some(overlay) = app.dispatch_overlay.as_mut() else {
        return;
    };
    let count = match &overlay.stage {
        DispatchStage::Prompt => snapshot.prompt_profiles.len(),
        DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
    };
    for distance in 1..=count {
        let candidate = if forward {
            (overlay.cursor + distance) % count
        } else {
            (overlay.cursor + count - distance % count) % count
        };
        let enabled = match &overlay.stage {
            DispatchStage::Prompt => true,
            DispatchStage::Target { .. } if candidate == 0 => snapshot
                .compute_targets
                .iter()
                .any(|target| target.is_dispatchable()),
            DispatchStage::Target { .. } => {
                snapshot.compute_targets[candidate - 1].is_dispatchable()
            },
        };
        if enabled {
            overlay.cursor = candidate;
            break;
        }
    }
}

fn target_unavailable_message(target: &agent_launcher_core::ComputeTargetStatus) -> String {
    if let Some(message) = target.message.as_deref() {
        return format!("{} unavailable: {message}", target.name);
    }
    let status = if target.is_full() {
        "full"
    } else {
        match target.availability {
            ComputeTargetAvailability::Offline => "offline",
            ComputeTargetAvailability::Full => "full",
            ComputeTargetAvailability::Online | ComputeTargetAvailability::Wakeable => {
                "unavailable"
            },
        }
    };
    format!("{} is {status}", target.name)
}

#[derive(Debug, Eq, PartialEq)]
enum LaunchAction {
    Review {
        issue: agent_launcher_core::IssueKey,
        target: Option<String>,
    },
    Dispatch {
        issue: agent_launcher_core::IssueKey,
        profile: Option<String>,
        target: Option<String>,
    },
}

fn launch_action(
    snapshot: &RuntimeSnapshot,
    issue: agent_launcher_core::IssueKey,
    profile: Option<String>,
    target: Option<String>,
) -> LaunchAction {
    if is_pull_request(snapshot, &issue) {
        LaunchAction::Review { issue, target }
    } else {
        LaunchAction::Dispatch {
            issue,
            profile,
            target,
        }
    }
}

fn start_launch(
    app: &mut AppState,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    action: LaunchAction,
) {
    let (issue, profile, target) = match action {
        LaunchAction::Review { issue, target } => {
            app.status_message = Some("starting PR review...".to_owned());
            let runtime = runtime.clone();
            spawn_runtime_action(actions, "PR review started", None, async move {
                runtime.review(issue, target).await
            });
            return;
        },
        LaunchAction::Dispatch {
            issue,
            profile,
            target,
        } => (issue, profile, target),
    };
    let status = match (profile.as_deref(), target.as_deref()) {
        (Some(profile), Some(target)) => format!("dispatching {profile} on {target}..."),
        (None, Some(target)) => format!("dispatching agent on {target}..."),
        (Some(profile), None) => format!("dispatching {profile}..."),
        (None, None) => "dispatching agent...".to_owned(),
    };
    app.status_message = Some(status);
    let runtime = runtime.clone();
    spawn_runtime_action(actions, "agent dispatched", None, async move {
        if target.is_some() {
            runtime.dispatch_on_target(issue, profile, target).await
        } else {
            runtime.dispatch(issue, profile).await
        }
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
        app.status_message = Some(if app.tab == crate::app::InboxTab::PullRequests {
            "no PR selected".to_owned()
        } else {
            "no issue selected".to_owned()
        });
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

struct TerminalCleanup {
    title_saved: bool,
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // Attempt each restoration even if an earlier terminal write fails.
        let mut output = io::stdout();
        let _ = execute!(output, DisableMouseCapture);
        let _ = execute!(output, LeaveAlternateScreen);
        let _ = execute!(output, Show);
        if self.title_saved {
            let _ = execute!(output, POP_TITLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::{
        BackendKind, ComputeProvider, ComputeTargetAvailability, ComputeTargetStatus, Issue,
        IssueKey, IssueProvider, RunState, RunSummary, RuntimeSnapshot, WorktreeDeleteAction,
        WorktreeDeletePreview,
    };
    use chrono::{Duration, Utc};

    use super::*;

    #[test]
    fn terminal_title_commands_save_set_and_restore_only_the_local_title() {
        let mut output = Vec::new();
        execute!(output, PUSH_TITLE, LAUNCHER_TITLE, POP_TITLE).unwrap();
        assert_eq!(output, b"\x1b[22;0t\x1b]0;launcher\x07\x1b[23;0t");
    }

    #[test]
    fn herdr_title_command_requires_explicit_caller_context() {
        for (env, id) in [
            (None, Some("w1:t2")),
            (Some("0"), Some("w1:t2")),
            (Some("1"), None),
            (Some("1"), Some(" ")),
        ] {
            assert!(herdr_tab_title_command(env, id).is_none());
        }
        let command = herdr_tab_title_command(Some("1"), Some("w7:t9")).unwrap();
        assert_eq!(command.as_std().get_program(), "herdr");
        assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), [
            "tab", "rename", "--", "w7:t9", "launcher"
        ]);
    }

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
            pull_request: None,
            activity: None,
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

    fn compute_target(
        id: &str,
        availability: ComputeTargetAvailability,
        active_runs: usize,
        max_active_runs: Option<usize>,
    ) -> ComputeTargetStatus {
        ComputeTargetStatus {
            id: id.to_owned(),
            name: format!("Target {id}"),
            provider: ComputeProvider::Ssh,
            availability,
            active_runs,
            max_active_runs,
            cpu_percent: Some(25.0),
            memory_percent: Some(50.0),
            sampled_at: Utc::now(),
            message: None,
        }
    }

    fn pull_request(id: &str) -> Issue {
        Issue {
            title: format!("Review {id}"),
            pull_request: Some(agent_launcher_core::PullRequestMetadata {
                number: id.parse().unwrap(),
                additions: Some(12),
                deletions: None,
                base_ref: "main".into(),
                head_ref: "feature".into(),
                base_sha: "base-sha".into(),
                head_sha: "head-sha".into(),
                head_repository: Some("fork/launcher".into()),
            }),
            ..issue(id, Duration::zero())
        }
    }

    #[test]
    fn tabs_keep_independent_navigation_and_reconcile_the_hidden_selection() {
        let mut snapshot = RuntimeSnapshot {
            issues: (1..=10)
                .map(|id| issue(&id.to_string(), Duration::days(id)))
                .collect(),
            ..RuntimeSnapshot::default()
        };
        snapshot
            .issues
            .extend((11..=20).map(|id| pull_request(&id.to_string())));
        let mut app = AppState {
            search_query: "Issue".into(),
            selected: 5,
            scroll: 3,
            ..AppState::default()
        };
        let issue_key = app.selected_issue(&snapshot).unwrap().key.clone();
        handle_list_key(&mut app, KeyCode::Tab.into(), &snapshot);
        assert_eq!(app.tab, crate::app::InboxTab::PullRequests);
        assert_eq!((app.selected, app.scroll), (0, 0));
        assert!(app.search_query.is_empty());
        app.search_query = "Review".into();
        app.issue_sort = IssueSort::Oldest;
        app.selected = 4;
        app.scroll = 2;
        let pr_key = app.selected_issue(&snapshot).unwrap().key.clone();
        handle_list_key(&mut app, KeyCode::BackTab.into(), &snapshot);
        assert_eq!(app.search_query, "Issue");
        assert_eq!((app.selected, app.scroll), (5, 3));
        assert_eq!(app.issue_sort, IssueSort::Newest);

        let mut refreshed = snapshot.clone();
        refreshed.issues.reverse();
        refreshed.issues.push(issue("21", Duration::zero()));
        let mut new_pr = pull_request("22");
        new_pr.created_at = Some(Utc::now() - Duration::days(100));
        refreshed.issues.push(new_pr);
        app.reconcile_lists(&snapshot, &refreshed);
        assert_eq!(app.selected_issue(&refreshed).unwrap().key, issue_key);
        assert_eq!(app.scroll, 3);
        handle_list_key(&mut app, KeyCode::Tab.into(), &refreshed);
        assert_eq!(app.selected_issue(&refreshed).unwrap().key, pr_key);
        assert_eq!(app.scroll, 2);
        assert_eq!(app.search_query, "Review");
        assert_eq!(app.issue_sort, IssueSort::Oldest);

        refreshed.issues.retain(|issue| issue.key != pr_key);
        app.reconcile_lists(&snapshot, &refreshed);
        assert!(app.selected < app.rows(&refreshed).len());
    }

    #[test]
    fn printable_command_characters_remain_filter_text_on_both_tabs() {
        let snapshot = RuntimeSnapshot::default();
        let mut app = AppState::default();
        for tab in [KeyCode::Tab, KeyCode::BackTab] {
            for character in "drsg?".chars() {
                assert!(!handle_debug_key(&mut app, KeyCode::Char(character).into()));
                assert!(!handle_list_key(
                    &mut app,
                    KeyCode::Char(character).into(),
                    &snapshot
                ));
            }
            assert_eq!(app.search_query, "drsg?");
            assert!(app.dispatch_overlay.is_none());
            handle_list_key(&mut app, tab.into(), &snapshot);
        }
        assert_eq!(app.search_query, "drsg?");
    }

    #[test]
    fn debug_command_is_modal_and_preserves_list_state() {
        let mut app = AppState {
            search_query: "keep filter".into(),
            selected: 4,
            scroll: 2,
            ..Default::default()
        };
        assert!(handle_debug_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
        ));
        assert!(app.command_overlay);
        assert!(handle_debug_key(&mut app, KeyCode::Char('g').into()));
        assert!(app.debug_overlay);
        assert!(!app.command_overlay);
        app.debug_scroll_max = 20;
        app.debug_page_size = 5;
        for (key, expected) in [
            (KeyCode::Down, 1),
            (KeyCode::PageDown, 6),
            (KeyCode::Up, 5),
            (KeyCode::End, 20),
            (KeyCode::PageUp, 15),
            (KeyCode::Home, 0),
        ] {
            assert!(handle_debug_key(&mut app, key.into()));
            assert_eq!(app.debug_scroll, expected);
        }
        for key in [
            KeyCode::Char('d'),
            KeyCode::Tab,
            KeyCode::Enter,
            KeyCode::Backspace,
        ] {
            assert!(handle_debug_key(&mut app, key.into()));
        }
        assert!(handle_debug_key(&mut app, KeyCode::Esc.into()));
        assert!(!app.debug_overlay);
        assert_eq!(app.search_query, "keep filter");
        assert_eq!((app.selected, app.scroll), (4, 2));
        assert!(app.dispatch_overlay.is_none());
    }

    #[test]
    fn explicit_review_bypasses_profiles_and_opening_details_does_not_launch() {
        let pr = pull_request("42");
        let snapshot = RuntimeSnapshot {
            issues: vec![issue("1", Duration::zero()), pr.clone()],
            prompt_profiles: vec!["implementer".into(), "designer".into()],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState::default();
        handle_list_key(&mut app, KeyCode::Tab.into(), &snapshot);
        handle_list_key(&mut app, KeyCode::Enter.into(), &snapshot);
        assert_eq!(app.detail_issue_key, Some(pr.key.clone()));
        assert!(app.dispatch_overlay.is_none());
        assert!(app.status_message.is_none());
        assert_eq!(
            prepare_dispatch(&mut app, &snapshot),
            Some(LaunchAction::Review {
                issue: pr.key,
                target: None,
            })
        );
        assert!(app.dispatch_overlay.is_none());
    }

    #[test]
    fn review_target_flow_keeps_the_pr_key_and_never_an_issue_profile() {
        let pr = pull_request("42");
        let mut snapshot = RuntimeSnapshot {
            issues: vec![pr.clone()],
            prompt_profiles: vec!["implementer".into(), "designer".into()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![
                compute_target("offline", ComputeTargetAvailability::Offline, 0, Some(2)),
                compute_target("ready", ComputeTargetAvailability::Online, 0, Some(2)),
            ],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState {
            tab: crate::app::InboxTab::PullRequests,
            ..AppState::default()
        };
        assert_eq!(prepare_dispatch(&mut app, &snapshot), None);
        assert_eq!(
            app.dispatch_overlay.as_ref().unwrap().stage,
            DispatchStage::Target { profile: None }
        );
        assert_eq!(select_dispatch(&mut app, &snapshot, 1), None);
        assert!(app.status_message.as_deref().unwrap().contains("offline"));
        snapshot.issues.insert(0, pull_request("43"));
        snapshot.prompt_profiles.clear();
        assert!(!app.reconcile_dispatch(&snapshot));
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 2),
            Some(LaunchAction::Review {
                issue: pr.key.clone(),
                target: Some("ready".into()),
            })
        );
        assert!(app.dispatch_overlay.is_none());
        app.dispatch_overlay = staged_dispatch_overlay(pr.key.clone(), &snapshot);
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 0),
            Some(LaunchAction::Review {
                issue: pr.key.clone(),
                target: None,
            })
        );
        app.dispatch_overlay = staged_dispatch_overlay(pr.key.clone(), &snapshot);
        snapshot.issues.retain(|issue| issue.key != pr.key);
        assert!(app.reconcile_dispatch(&snapshot));
        assert!(app.dispatch_overlay.is_none());
    }

    #[test]
    fn issue_launch_still_uses_the_selected_prompt_and_target() {
        let selected = issue("1", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            prompt_profiles: vec!["implementer".into(), "designer".into()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![compute_target(
                "ready",
                ComputeTargetAvailability::Online,
                0,
                Some(2),
            )],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState::default();
        assert_eq!(prepare_dispatch(&mut app, &snapshot), None);
        assert_eq!(
            app.dispatch_overlay.as_ref().unwrap().stage,
            DispatchStage::Prompt
        );
        assert_eq!(select_dispatch(&mut app, &snapshot, 1), None);
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 1),
            Some(LaunchAction::Dispatch {
                issue: selected.key,
                profile: Some("designer".into()),
                target: Some("ready".into()),
            })
        );
    }

    #[tokio::test]
    async fn dispatch_worker_results_reach_visible_status() {
        for result in [
            Ok(()),
            Err(agent_launcher_runtime::Error::RunAlreadyActive(
                "run-1".into(),
            )),
            Err(agent_launcher_runtime::Error::LaunchFailed {
                run_id: "run-1".into(),
                message: "Initial prompt failed: invalid agent".into(),
            }),
        ] {
            let expected = match &result {
                Ok(()) => "agent dispatched".to_owned(),
                Err(error) => format!("runtime error: {error}"),
            };
            let (actions, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            spawn_runtime_action(&actions, "agent dispatched", None, async move { result });
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            let mut app = AppState::default();
            apply_ui_action_result(&mut app, result);
            assert_eq!(app.status_message.as_deref(), Some(expected.as_str()));
            let backend = ratatui::backend::TestBackend::new(120, 40);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw(frame, &RuntimeSnapshot::default(), &mut app))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains(&expected), "missing status: {expected}");
        }
    }

    #[test]
    fn herdr_profile_selection_finishes_the_chooser_without_a_target_stage() {
        let selected = issue("1", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            prompt_profiles: vec!["implementer".into(), "designer".into()],
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "claude".into(),
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState::default();
        assert_eq!(prepare_dispatch(&mut app, &snapshot), None);
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 1),
            Some(LaunchAction::Dispatch {
                issue: selected.key,
                profile: Some("designer".into()),
                target: None,
            })
        );
        assert!(app.dispatch_overlay.is_none());
        assert_eq!(snapshot.selected_agent, "claude");
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
                stage: DispatchStage::Prompt,
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

    #[test]
    fn prompt_selection_advances_to_targets_and_preserves_issue_and_profile() {
        let selected = issue("2", Duration::zero());
        let selected_key = selected.key.clone();
        let snapshot = RuntimeSnapshot {
            issues: vec![selected],
            prompt_profiles: vec!["implementer".to_owned(), "reviewer".to_owned()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![compute_target(
                "local",
                ComputeTargetAvailability::Online,
                0,
                Some(2),
            )],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                issue_key: selected_key.clone(),
                cursor: 0,
                stage: DispatchStage::Prompt,
            }),
            ..AppState::default()
        };

        assert_eq!(select_prompt(&mut app, &snapshot, 1), None);

        let overlay = app.dispatch_overlay.as_ref().unwrap();
        assert_eq!(overlay.issue_key, selected_key);
        assert_eq!(overlay.cursor, 0);
        assert_eq!(overlay.stage, DispatchStage::Target {
            profile: Some("reviewer".to_owned())
        });
    }

    #[test]
    fn target_navigation_skips_offline_and_full_targets() {
        let selected = issue("2", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![
                compute_target("offline", ComputeTargetAvailability::Offline, 0, Some(2)),
                compute_target("ready", ComputeTargetAvailability::Online, 0, Some(2)),
                compute_target("full", ComputeTargetAvailability::Online, 2, Some(2)),
            ],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                issue_key: selected.key,
                cursor: 0,
                stage: DispatchStage::Target { profile: None },
            }),
            ..AppState::default()
        };

        move_dispatch_cursor(&mut app, &snapshot, true);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 2);
        move_dispatch_cursor(&mut app, &snapshot, true);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 0);
        move_dispatch_cursor(&mut app, &snapshot, false);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 2);
    }

    #[test]
    fn selecting_a_disabled_target_reports_status_and_keeps_overlay() {
        let selected = issue("2", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![ComputeTargetStatus {
                message: Some("host unreachable".to_owned()),
                ..compute_target("offline", ComputeTargetAvailability::Offline, 0, Some(2))
            }],
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                issue_key: selected.key,
                cursor: 1,
                stage: DispatchStage::Target {
                    profile: Some("reviewer".to_owned()),
                },
            }),
            ..AppState::default()
        };

        assert_eq!(select_target(&mut app, &snapshot, 1), Err(()));
        assert!(app.dispatch_overlay.is_some());
        assert_eq!(
            app.status_message.as_deref(),
            Some("Target offline unavailable: host unreachable")
        );
    }

    #[test]
    fn native_targets_open_immediately_for_zero_or_one_prompt_profile() {
        let selected = issue("2", Duration::zero());
        let mut snapshot = RuntimeSnapshot {
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![compute_target(
                "local",
                ComputeTargetAvailability::Online,
                0,
                Some(2),
            )],
            ..RuntimeSnapshot::default()
        };

        let overlay = staged_dispatch_overlay(selected.key.clone(), &snapshot).unwrap();
        assert_eq!(overlay.stage, DispatchStage::Target { profile: None });

        snapshot.prompt_profiles = vec!["implementer".to_owned()];
        let overlay = staged_dispatch_overlay(selected.key.clone(), &snapshot).unwrap();
        assert_eq!(overlay.stage, DispatchStage::Target {
            profile: Some("implementer".to_owned())
        });

        snapshot.selected_backend = Some(BackendKind::Superset);
        assert!(staged_dispatch_overlay(selected.key, &snapshot).is_none());
    }

    #[test]
    fn target_stage_keeps_selected_profile_when_profiles_refresh() {
        let selected = issue("2", Duration::zero());
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                issue_key: selected.key.clone(),
                cursor: 1,
                stage: DispatchStage::Target {
                    profile: Some("reviewer".to_owned()),
                },
            }),
            ..AppState::default()
        };
        let snapshot = RuntimeSnapshot {
            issues: vec![selected],
            prompt_profiles: vec!["replacement".to_owned()],
            selected_backend: Some(BackendKind::Native),
            compute_targets: vec![compute_target(
                "ready",
                ComputeTargetAvailability::Online,
                0,
                Some(2),
            )],
            ..RuntimeSnapshot::default()
        };

        assert!(!app.reconcile_dispatch(&snapshot));
        assert_eq!(
            app.dispatch_overlay.as_ref().unwrap().stage,
            DispatchStage::Target {
                profile: Some("reviewer".to_owned())
            }
        );
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
