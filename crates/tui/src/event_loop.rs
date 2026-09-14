use std::{future::Future, io, time::Duration};

use agent_launcher_core::{
    BackendKind, ComputeTargetAvailability, DispatchOptions, ModelSelection, RunEvent, RunState,
    RuntimeSnapshot, WorktreeDeletePreview,
};
use agent_launcher_runtime::RuntimeHandle;
use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
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
    app::{
        AppState, DeleteOverlay, DispatchOverlay, DispatchStage, InputOverlay, IssueDeleteOverlay,
        Route,
    },
    metrics::HostMetricsSampler,
    render::draw,
    rows::IssueSort,
};

const ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
const DEMO_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
const METRICS_INTERVAL: Duration = Duration::from_secs(5);
// Xterm title-stack operations; ignored by terminals without title-stack support.
const PUSH_TITLE: Print<&str> = Print("\x1b[22;0t");
const POP_TITLE: Print<&str> = Print("\x1b[23;0t");
const LAUNCHER_TITLE: SetTitle<&str> = SetTitle("launcher");

enum UiActionResult {
    Away {
        request_id: u64,
        result: agent_launcher_runtime::Result<()>,
    },
    Security {
        request_id: u64,
        result: agent_launcher_runtime::Result<()>,
    },
    Prompt {
        request_id: u64,
        issue_key: agent_launcher_core::IssueKey,
        name: String,
        result: Result<PromptReply, String>,
    },
    IssueDelete {
        issue_key: agent_launcher_core::IssueKey,
        result: agent_launcher_runtime::Result<()>,
    },
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

enum PromptReply {
    Preview(String),
    Loaded(agent_launcher_runtime::PromptDocument),
    Saved(agent_launcher_runtime::PromptDocument),
}

type UiActionSender = tokio::sync::mpsc::UnboundedSender<UiActionResult>;

/// Runs the interactive terminal UI against a live runtime handle.
pub async fn run(runtime: RuntimeHandle, layout: LayoutMode) -> Result<(), Error> {
    let mut cleanup = TerminalCleanup { title_saved: false };
    enable_raw_mode()?;

    let mut output = io::stdout();
    execute!(
        output,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
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
        demo_started: std::env::var_os("AGENT_LAUNCHER_DEMO_ACTIVITY")
            .map(|_| std::time::Instant::now()),
        ..Default::default()
    };
    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut metrics_sampler = HostMetricsSampler::new();
    metrics_sampler.sample(&mut app.host_metrics);
    let mut ticker = tokio::time::interval(ANIMATION_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut demo_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + DEMO_INTERVAL, DEMO_INTERVAL);
    demo_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut metrics_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + METRICS_INTERVAL,
        METRICS_INTERVAL,
    );
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let termination = termination_signal();
    tokio::pin!(termination);
    let mut needs_draw = true;

    loop {
        ensure_prompt_preview(&mut app, &runtime, &action_tx);
        if needs_draw {
            terminal.draw(|frame| draw(frame, &snapshot, &mut app))?;
            needs_draw = false;
        }
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        if crossterm::terminal::size().ok()
                                != Some((app.mouse.screen.width, app.mouse.screen.height))
                        {
                            app.issue_delete_confirmation_visible = false;
                            app.security_confirmation_visible = false;
                            app.away_quit_visible = false;
                        }
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
                    Some(Ok(Event::Paste(text))) => {
                        needs_draw |= handle_paste(&mut app, &text);
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
                        app.away_quit_visible = false;
                        app.issue_delete_confirmation_visible = false;
                        app.security_confirmation_visible = false;
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
            _ = demo_tick.tick(), if app.demo_started.is_some() => {
                // Redraw only: spinner cadence and telemetry sampling stay unchanged.
                needs_draw = true;
            }
            _ = metrics_tick.tick() => {
                metrics_sampler.sample(&mut app.host_metrics);
                needs_draw = true;
            }
            Some(result) = action_rx.recv() => {
                apply_ui_action_result(&mut app, result);
                app.reconcile_dispatch(&snapshot);
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
    if app.away_quit {
        match key.code {
            KeyCode::Esc => app.away_quit = false,
            KeyCode::Char('q') if app.away_quit_visible && key.kind == KeyEventKind::Press => {
                return true;
            },
            KeyCode::Char('m') if app.away_pending.is_none() => {
                app.away_quit = false;
                crate::away::open(app, snapshot);
                send_away(
                    app,
                    agent_launcher_core::RuntimeCommand::SetManual,
                    runtime,
                    actions,
                );
            },
            _ => {},
        }
        return false;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return request_quit(app, snapshot);
    }
    if app.away_overlay.is_some() {
        if let Some(command) = crate::away::prepare_command(app, key, snapshot) {
            send_away(app, command, runtime, actions);
        }
        return false;
    }
    if app.issue_delete_overlay.is_some() {
        if let Some(issue_key) = prepare_issue_delete(app, key) {
            let runtime = runtime.clone();
            let actions = actions.clone();
            tokio::spawn(async move {
                let result = runtime.delete_issue(issue_key.clone()).await;
                let _ = actions.send(UiActionResult::IssueDelete { issue_key, result });
            });
        }
        return false;
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

    if key.code == KeyCode::F(2) {
        crate::away::open(app, snapshot);
        return false;
    }

    if app.route == Route::Inbox {
        return handle_list_key(app, key, snapshot) && request_quit(app, snapshot);
    }
    if handle_issue_delete_shortcut(app, key, snapshot) {
        return false;
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
        KeyCode::Tab => {
            app.switch_tab();
            app.status_message = None;
        },
        KeyCode::BackTab => {
            app.set_tab(app.tab.previous());
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

fn request_quit(app: &mut AppState, snapshot: &RuntimeSnapshot) -> bool {
    if crate::away::needs_quit_warning(snapshot) || app.away_pending.is_some() {
        app.away_quit = true;
        app.away_quit_visible = false;
        false
    } else {
        true
    }
}

fn send_away(
    app: &mut AppState,
    command: agent_launcher_core::RuntimeCommand,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
) {
    if app.away_pending.is_some() {
        return;
    }
    app.next_request_id = app.next_request_id.wrapping_add(1);
    let request_id = app.next_request_id;
    app.away_pending = Some(request_id);
    app.status_message = Some("applying mode request...".into());
    let runtime = runtime.clone();
    let actions = actions.clone();
    tokio::spawn(async move {
        let result = runtime.send(command).await;
        let _ = actions.send(UiActionResult::Away { request_id, result });
    });
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
    if key.code == KeyCode::Char('g')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && app.dispatch_overlay.is_none()
        && app.input_overlay.is_none()
        && app.delete_overlay.is_none()
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
        KeyCode::Char('m') | KeyCode::F(2) => crate::away::open(app, snapshot),
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
        KeyCode::Char('q') => return request_quit(app, snapshot),
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

fn handle_issue_delete_shortcut(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
) -> bool {
    if app.route != Route::Detail || key.code != KeyCode::Char('X') {
        return false;
    }
    open_issue_delete(app, snapshot);
    true
}

fn open_issue_delete(app: &mut AppState, snapshot: &RuntimeSnapshot) {
    if app.route != Route::Detail || app.issue_delete_overlay.is_some() {
        return;
    }
    let Some(issue) = app.detail_issue(snapshot) else {
        return;
    };
    if issue.key.provider != agent_launcher_core::IssueProvider::Beads
        || issue.pull_request.is_some()
        || issue.security_advisory.is_some()
    {
        app.status_message = Some(
            "Unsupported: use provider tools. Source deletion supports Beads issues only, not PRs or private advisories; closing is not deletion.".into(),
        );
        return;
    }
    app.issue_delete_overlay = Some(IssueDeleteOverlay {
        issue_key: issue.key.clone(),
        identifier: issue.identifier.clone(),
        title: issue.title.clone(),
        pending: false,
    });
    // A late worktree inspection must not replace this confirmation.
    app.delete_preview_request = None;
    app.issue_delete_confirmation_visible = false;
    app.status_message = None;
}

fn prepare_issue_delete(
    app: &mut AppState,
    key: KeyEvent,
) -> Option<agent_launcher_core::IssueKey> {
    let overlay = app.issue_delete_overlay.as_mut()?;
    if overlay.pending {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            app.issue_delete_overlay = None;
            app.issue_delete_confirmation_visible = false;
            app.status_message = Some("source issue deletion cancelled".into());
        },
        KeyCode::Enter
            if app.issue_delete_confirmation_visible && key.kind == KeyEventKind::Press =>
        {
            overlay.pending = true;
            app.issue_delete_confirmation_visible = false;
            app.status_message = Some("deleting source issue...".into());
            return Some(overlay.issue_key.clone());
        },
        KeyCode::Enter => {
            app.status_message =
                Some("resize the terminal to review the full target and deletion warnings".into());
        },
        _ => {},
    }
    None
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

fn ensure_prompt_preview(app: &mut AppState, runtime: &RuntimeHandle, actions: &UiActionSender) {
    if app.dispatch_overlay.as_ref().is_some_and(|o| {
        o.stage == DispatchStage::Prompt
            && o.prompt.editor.is_none()
            && o.prompt.request.is_none()
            && o.prompt.preview.is_none()
    }) {
        prompt_request(app, runtime, actions, None, true);
    }
}

fn handle_paste(app: &mut AppState, text: &str) -> bool {
    if app.away_quit {
        return false;
    }
    if let Some(overlay) = app.away_overlay.as_mut() {
        if app.away_pending.is_none()
            && text.trim().bytes().all(|c| c.is_ascii_digit())
            && text.trim().len() <= 3
        {
            overlay.limit = text.trim().into();
            return true;
        }
        return false;
    }
    if let Some(editor) = app
        .dispatch_overlay
        .as_mut()
        .and_then(|o| o.settings.model_editor.as_mut())
    {
        editor.insert(&text.chars().filter(|c| !c.is_control()).collect::<String>());
        return true;
    }
    if let Some(overlay) = app.dispatch_overlay.as_mut()
        && matches!(overlay.stage, DispatchStage::Settings { .. })
        && overlay.settings.instructions_focused
        && let Some(editor) = &mut overlay.settings.instructions_editor
    {
        editor.insert(text);
        return true;
    }
    if let Some(editor) = app
        .dispatch_overlay
        .as_mut()
        .and_then(|o| o.prompt.editor.as_mut())
        && !editor.busy
        && !editor.discard
    {
        if editor.naming {
            editor.name.extend(text.chars().filter(|c| !c.is_control()));
        } else {
            editor.buffer.insert(text);
        }
        return true;
    }
    false
}

fn prompt_request(
    app: &mut AppState,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    save: Option<(String, Option<String>)>,
    preview: bool,
) {
    let Some(overlay) = app.dispatch_overlay.as_mut() else {
        return;
    };
    app.next_request_id = app.next_request_id.wrapping_add(1);
    let request_id = app.next_request_id;
    overlay.prompt.request = Some(request_id);
    overlay.prompt.loading_source = save.is_none() && !preview;
    let issue_key = overlay.issue_key.clone();
    let name = overlay
        .prompt
        .editor
        .as_ref()
        .map_or_else(|| overlay.prompt.name.clone(), |e| e.name.clone());
    let runtime = runtime.clone();
    let actions = actions.clone();
    tokio::spawn(async move {
        let result = if let Some((source, expected)) = save {
            runtime
                .save_prompt(name.clone(), source, expected)
                .await
                .map(PromptReply::Saved)
        } else if preview {
            runtime
                .load_prompt(name.clone())
                .await
                .map(|document| PromptReply::Preview(document.source))
        } else {
            runtime
                .load_prompt(name.clone())
                .await
                .map(PromptReply::Loaded)
        }
        .map_err(|error| error.to_string());
        let _ = actions.send(UiActionResult::Prompt {
            request_id,
            issue_key,
            name,
            result,
        });
    });
}

fn edit_prompt(
    view: &mut crate::app::PromptView,
    key: KeyEvent,
) -> Option<(String, Option<String>)> {
    let editor = view.editor.as_mut()?;
    if editor.busy {
        return None;
    }
    if view.error.is_some() && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        view.scroll = if key.code == KeyCode::PageUp {
            view.scroll.saturating_sub(8)
        } else {
            view.scroll.saturating_add(8).min(view.scroll_max)
        };
        return None;
    }
    if editor.discard {
        match key.code {
            KeyCode::Char('y') => {
                view.editor = None;
                view.request = None;
                view.error = None;
            },
            KeyCode::Esc | KeyCode::Char('n') => editor.discard = false,
            _ => {},
        }
        return None;
    }
    if key.code == KeyCode::Esc {
        if editor.original.as_deref() != Some(editor.buffer.text.as_str()) {
            editor.discard = true;
        } else {
            view.editor = None;
            view.error = None;
        }
        return None;
    }
    if key.code == KeyCode::Char('n')
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && editor.original.is_none()
    {
        editor.naming = true;
        return None;
    }
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && editor.original.is_none() {
        editor.naming = !editor.naming;
        return None;
    }
    if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let save = (editor.buffer.text.clone(), editor.original.clone());
        editor.busy = true;
        view.error = None;
        return Some(save);
    }
    if editor.naming {
        match key.code {
            KeyCode::Enter => editor.naming = false,
            KeyCode::Backspace => {
                editor.name.pop();
            },
            KeyCode::Char(c) if printable(c, key.modifiers) => editor.name.push(c),
            _ => {},
        }
        return None;
    }
    editor.buffer.key(key);
    None
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
    if app.security_launch_pending.is_some() {
        app.status_message = Some("private dispatch is still pending".into());
        return None;
    }
    let issue_key = dispatch_target(app, snapshot)?;
    if is_security(snapshot, &issue_key) && !security_supported(snapshot) {
        app.status_message = Some("Private dispatch requires local Native (no compute targets) or Herdr. No automatic public fallback.".into());
        return None;
    }
    if is_security(snapshot, &issue_key) && !security_ready(snapshot, &issue_key) {
        app.status_message = Some("Private advisory is read-only here: accept triage reports on GitHub manually first; only draft advisories can be dispatched.".into());
        return None;
    }
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
        } else if is_security(snapshot, &issue_key) {
            "private run retained; open its harness to review. Separate private-clone cleanup is unavailable".into()
        } else {
            "existing native run must be resumed or deleted before dispatching again".to_owned()
        });
        return None;
    }
    app.status_message = None;
    app.dispatch_overlay = staged_dispatch_overlay(issue_key, snapshot);
    None
}

fn is_pull_request(snapshot: &RuntimeSnapshot, key: &agent_launcher_core::IssueKey) -> bool {
    snapshot
        .issues
        .iter()
        .any(|issue| issue.key == *key && issue.pull_request.is_some())
}

fn is_security(snapshot: &RuntimeSnapshot, key: &agent_launcher_core::IssueKey) -> bool {
    snapshot
        .issues
        .iter()
        .any(|issue| issue.key == *key && issue.security_advisory.is_some())
}

fn security_supported(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.selected_backend == Some(BackendKind::Herdr)
        || (snapshot.selected_backend == Some(BackendKind::Native)
            && snapshot.compute_targets.is_empty())
}

fn security_ready(snapshot: &RuntimeSnapshot, key: &agent_launcher_core::IssueKey) -> bool {
    snapshot.issues.iter().any(|issue| {
        issue.key == *key && issue.security_advisory.is_some() && issue.state == "draft"
    })
}

fn staged_dispatch_overlay(
    issue_key: agent_launcher_core::IssueKey,
    snapshot: &RuntimeSnapshot,
) -> Option<DispatchOverlay> {
    let review = is_pull_request(snapshot, &issue_key);
    let security = is_security(snapshot, &issue_key);
    let stage = if security {
        DispatchStage::Settings {
            profile: None,
            target: None,
        }
    } else if !review {
        DispatchStage::Prompt
    } else if has_target_stage(snapshot) {
        DispatchStage::Target { profile: None }
    } else {
        DispatchStage::Settings {
            profile: None,
            target: None,
        }
    };
    Some(DispatchOverlay {
        settings: crate::app::LaunchSettings {
            backend: snapshot.selected_backend,
            default_harness: snapshot.selected_agent.clone(),
            default_model: snapshot.selected_model.clone(),
            target_choices: snapshot
                .compute_targets
                .iter()
                .map(|t| t.id.clone())
                .collect(),
            had_targets: has_target_stage(snapshot),
            review,
            security,
            instructions_editor: (!review && !security).then(Default::default),
            instructions_focused: !review && !security,
            ..Default::default()
        },
        prompt: crate::app::PromptView {
            name: snapshot
                .prompt_profiles
                .first()
                .cloned()
                .unwrap_or_default(),
            ..Default::default()
        },
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
    match dispatch_key(app, key, snapshot) {
        DispatchEffect::None => {},
        DispatchEffect::Load => prompt_request(app, runtime, actions, None, false),
        DispatchEffect::Save(source, expected) => {
            prompt_request(app, runtime, actions, Some((source, expected)), false);
        },
        DispatchEffect::Select(index) => {
            dispatch_from_overlay(app, snapshot, runtime, actions, index)
        },
    }
    false
}

#[derive(Debug, PartialEq)]
enum DispatchEffect {
    None,
    Load,
    Save(String, Option<String>),
    Select(usize),
}

fn dispatch_key(app: &mut AppState, key: KeyEvent, snapshot: &RuntimeSnapshot) -> DispatchEffect {
    if app
        .dispatch_overlay
        .as_ref()
        .is_some_and(|o| o.settings.privacy_confirmation)
    {
        match key.code {
            KeyCode::Esc => {
                app.dispatch_overlay = None;
                app.security_confirmation_visible = false;
            },
            KeyCode::Char('y')
                if key.kind == KeyEventKind::Press
                    && key.modifiers.is_empty()
                    && app.security_confirmation_visible =>
            {
                return DispatchEffect::Select(0);
            },
            KeyCode::Enter | KeyCode::Char('y') => {
                app.status_message = Some("Review the full privacy warning; resize if needed, then press y to consent. Enter never consents.".into());
            },
            _ => {},
        }
        return DispatchEffect::None;
    }
    if app
        .dispatch_overlay
        .as_ref()
        .is_some_and(|o| matches!(o.stage, DispatchStage::Settings { .. }))
    {
        return settings_key(app, key);
    }
    let option_count = dispatch_option_count(app, snapshot);
    if let Some(view) = app.dispatch_overlay.as_mut().map(|o| &mut o.prompt)
        && view.editor.is_some()
    {
        return edit_prompt(view, key).map_or(DispatchEffect::None, |(source, expected)| {
            DispatchEffect::Save(source, expected)
        });
    }
    let prompt_stage = app
        .dispatch_overlay
        .as_ref()
        .is_some_and(|o| o.stage == DispatchStage::Prompt);
    if prompt_stage {
        match key.code {
            KeyCode::Char('a') => {
                let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
                view.request = None;
                view.error = None;
                view.editor = Some(crate::app::PromptEditor {
                    name: String::new(),
                    naming: true,
                    buffer: crate::widgets::editor::Editor::new(
                        "{{ issue_title }}\n\n{{ issue_text }}".into(),
                    ),
                    original: None,
                    discard: false,
                    busy: false,
                });
                return DispatchEffect::None;
            },
            KeyCode::Char('e') => {
                let view = &app.dispatch_overlay.as_ref().unwrap().prompt;
                if !(view.name.is_empty() || view.loading_source && view.request.is_some()) {
                    return DispatchEffect::Load;
                }
                return DispatchEffect::None;
            },
            KeyCode::PageUp | KeyCode::PageDown => {
                let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
                view.scroll = if key.code == KeyCode::PageUp {
                    view.scroll.saturating_sub(8)
                } else {
                    view.scroll.saturating_add(8).min(view.scroll_max)
                };
                return DispatchEffect::None;
            },
            KeyCode::Char('r') => {
                let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
                view.request = None;
                view.preview = None;
                view.error = None;
                return DispatchEffect::None;
            },
            _ => {},
        }
    }
    match key.code {
        KeyCode::Esc => {
            let overlay = app.dispatch_overlay.as_mut().unwrap();
            if matches!(overlay.stage, DispatchStage::Target { .. }) && !overlay.settings.review {
                overlay.settings.target_cursor = overlay.cursor;
                overlay.stage = DispatchStage::Prompt;
                overlay.cursor = snapshot
                    .prompt_profiles
                    .iter()
                    .position(|p| *p == overlay.prompt.name)
                    .unwrap_or(0);
            } else {
                app.dispatch_overlay = None;
            }
        },
        KeyCode::Up if option_count > 0 => move_dispatch_cursor(app, snapshot, false),
        KeyCode::Down if option_count > 0 => move_dispatch_cursor(app, snapshot, true),
        KeyCode::Char(character @ '1'..='9') => {
            let index = character as usize - '1' as usize;
            if index < option_count {
                if let Some(overlay) = app.dispatch_overlay.as_mut() {
                    overlay.cursor = index;
                    if prompt_stage {
                        overlay.prompt.name = snapshot
                            .prompt_profiles
                            .get(index)
                            .cloned()
                            .unwrap_or_default();
                        overlay.prompt.preview = None;
                        overlay.prompt.request = None;
                        overlay.prompt.error = None;
                        overlay.prompt.scroll = 0;
                    }
                }
                if !prompt_stage {
                    return DispatchEffect::Select(index);
                }
            }
        },
        KeyCode::Enter if option_count > 0 => {
            if let Some(overlay) = &app.dispatch_overlay
                && prompt_stage
                && (overlay.prompt.request.is_some()
                    || !matches!(overlay.prompt.preview, Some(Ok(_)))
                    || snapshot
                        .prompt_profiles
                        .get(overlay.cursor)
                        .map(String::as_str)
                        .unwrap_or("")
                        != overlay.prompt.name)
            {
                return DispatchEffect::None;
            }
            let index = app
                .dispatch_overlay
                .as_ref()
                .map_or(0, |overlay| overlay.cursor.min(option_count - 1));
            return DispatchEffect::Select(index);
        },
        _ => {},
    }
    DispatchEffect::None
}

fn settings_key(app: &mut AppState, key: KeyEvent) -> DispatchEffect {
    let overlay = app.dispatch_overlay.as_mut().unwrap();
    let settings = &mut overlay.settings;
    if let Some(editor) = &mut settings.model_editor {
        match key.code {
            KeyCode::PageUp => settings.scroll = settings.scroll.saturating_sub(8),
            KeyCode::PageDown => {
                settings.scroll = settings.scroll.saturating_add(8).min(settings.scroll_max)
            },
            KeyCode::Esc => settings.model_editor = None,
            KeyCode::Enter => {
                if editor.text.trim().is_empty() {
                    app.status_message =
                        Some("Custom model cannot be empty; choose Harness default instead".into());
                } else {
                    settings.options.model = ModelSelection::Explicit(editor.text.trim().into());
                    settings.model_editor = None;
                    app.status_message = None;
                }
            },
            _ => editor.key(key),
        }
        return DispatchEffect::None;
    }
    if settings.instructions_focused
        && let Some(editor) = &mut settings.instructions_editor
    {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                settings.instructions_focused = false;
            },
            KeyCode::Enter | KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                settings.instructions_focused = false;
            },
            _ => editor.key(key),
        }
        return DispatchEffect::None;
    }
    match key.code {
        KeyCode::PageUp => settings.scroll = settings.scroll.saturating_sub(8),
        KeyCode::PageDown => {
            settings.scroll = settings.scroll.saturating_add(8).min(settings.scroll_max)
        },
        KeyCode::Char('h') => {
            let choices = settings.harness_choices();
            if !choices.is_empty() {
                settings.options.harness = match settings.options.harness.as_deref() {
                    None => Some(choices[0].into()),
                    Some(current) => choices
                        .iter()
                        .position(|h| *h == current)
                        .and_then(|i| choices.get(i + 1))
                        .map(|h| (*h).into()),
                };
                settings.options.model = ModelSelection::HarnessDefault;
            }
        },
        KeyCode::Char('1') => settings.options.model = ModelSelection::Inherit,
        KeyCode::Char('2') => settings.options.model = ModelSelection::HarnessDefault,
        KeyCode::Char('3') | KeyCode::Char('m') => {
            let text = match &settings.options.model {
                ModelSelection::Explicit(model) => model.clone(),
                _ => String::new(),
            };
            let cursor = text.len();
            settings.model_editor = Some(crate::widgets::editor::Editor { text, cursor });
        },
        KeyCode::Tab | KeyCode::BackTab if settings.instructions_editor.is_some() => {
            settings.instructions_focused = true;
        },
        KeyCode::Enter if key.kind == KeyEventKind::Press && key.modifiers.is_empty() => {
            return DispatchEffect::Select(0);
        },
        KeyCode::Esc => {
            if let DispatchStage::Settings { profile, .. } = &overlay.stage {
                if settings.had_targets {
                    overlay.stage = DispatchStage::Target {
                        profile: profile.clone(),
                    };
                    overlay.cursor = settings.target_cursor;
                } else if !settings.review && !settings.security {
                    overlay.stage = DispatchStage::Prompt;
                } else {
                    app.dispatch_overlay = None;
                }
            }
        },
        _ => {},
    }
    DispatchEffect::None
}

fn dispatch_from_overlay(
    app: &mut AppState,
    snapshot: &RuntimeSnapshot,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    index: usize,
) {
    if let Some(overlay) = &app.dispatch_overlay
        && overlay.stage == DispatchStage::Prompt
        && (overlay.prompt.editor.is_some()
            || overlay.prompt.request.is_some()
            || !matches!(overlay.prompt.preview, Some(Ok(_)))
            || snapshot
                .prompt_profiles
                .get(index)
                .map(String::as_str)
                .unwrap_or("")
                != overlay.prompt.name)
    {
        return;
    }
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
            select_prompt(app, snapshot, index);
            None
        },
        DispatchStage::Target { profile } => {
            let target = select_target(app, snapshot, index).ok()?;
            let overlay = app.dispatch_overlay.as_mut()?;
            overlay.settings.target_cursor = index;
            overlay.stage = DispatchStage::Settings { profile, target };
            app.status_message = None;
            None
        },
        DispatchStage::Settings { profile, target } => {
            let overlay = app.dispatch_overlay.as_ref()?;
            if overlay.settings.model_editor.is_some() || overlay.settings.instructions_focused {
                return None;
            }
            if snapshot.selected_backend != overlay.settings.backend
                || snapshot.selected_agent != overlay.settings.default_harness
                || snapshot.selected_model != overlay.settings.default_model
            {
                app.status_message = Some("Dispatch defaults changed; cancel and reopen the chooser to review the new configuration".into());
                if overlay.settings.security {
                    app.dispatch_overlay = None;
                    app.security_confirmation_visible = false;
                }
                return None;
            }
            if overlay.settings.security {
                if !security_ready(snapshot, &overlay.issue_key) || !security_supported(snapshot) {
                    app.dispatch_overlay = None;
                    app.security_confirmation_visible = false;
                    app.status_message = Some("Private dispatch unavailable; no automatic public fallback. Reopen after reviewing configuration.".into());
                    return None;
                }
                if !overlay.settings.privacy_confirmation {
                    app.dispatch_overlay.as_mut()?.settings.privacy_confirmation = true;
                    app.security_confirmation_visible = false;
                    app.status_message = None;
                    return None;
                }
                if !app.security_confirmation_visible || app.security_launch_pending.is_some() {
                    return None;
                }
                let mut overlay = app.dispatch_overlay.take()?;
                app.security_confirmation_visible = false;
                overlay.settings.options.expected_backend = overlay.settings.backend;
                return Some(LaunchAction::Security {
                    issue: overlay.issue_key,
                    options: overlay.settings.options,
                });
            }
            let mut overlay = app.dispatch_overlay.take()?;
            overlay.settings.options.expected_backend = overlay.settings.backend;
            if let Some(editor) = &overlay.settings.instructions_editor {
                overlay.settings.options.additional_instructions = editor.text.clone();
            }
            Some(launch_action(
                overlay.settings.review,
                overlay.issue_key,
                profile,
                target,
                overlay.settings.options,
            ))
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
) -> Option<(agent_launcher_core::IssueKey, Option<String>)> {
    if app.dispatch_overlay.as_ref()?.prompt.editor.is_some() {
        return None;
    }
    let profile = snapshot.prompt_profiles.get(index).cloned();
    if profile.is_none() && !(snapshot.prompt_profiles.is_empty() && index == 0) {
        app.status_message = Some("selected prompt profile is no longer available".into());
        return None;
    }
    let overlay = app.dispatch_overlay.as_mut()?;
    if overlay.settings.had_targets {
        overlay.cursor = overlay.settings.target_cursor;
        overlay.stage = DispatchStage::Target { profile };
        app.status_message = None;
        None
    } else {
        overlay.stage = DispatchStage::Settings {
            profile,
            target: None,
        };
        None
    }
}

fn has_target_stage(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.selected_backend == Some(BackendKind::Native) && !snapshot.compute_targets.is_empty()
}

fn dispatch_option_count(app: &AppState, snapshot: &RuntimeSnapshot) -> usize {
    app.dispatch_overlay
        .as_ref()
        .map_or(0, |overlay| match &overlay.stage {
            DispatchStage::Prompt => snapshot.prompt_profiles.len().max(1),
            DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
            DispatchStage::Settings { .. } => 1,
        })
}

fn move_dispatch_cursor(app: &mut AppState, snapshot: &RuntimeSnapshot, forward: bool) {
    let Some(overlay) = app.dispatch_overlay.as_mut() else {
        return;
    };
    let count = match &overlay.stage {
        DispatchStage::Prompt => snapshot.prompt_profiles.len().max(1),
        DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
        DispatchStage::Settings { .. } => 1,
    };
    for distance in 1..=count {
        let candidate = if forward {
            (overlay.cursor + distance) % count
        } else {
            (overlay.cursor + count - distance % count) % count
        };
        let enabled = match &overlay.stage {
            DispatchStage::Prompt | DispatchStage::Settings { .. } => true,
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
            if overlay.stage == DispatchStage::Prompt {
                overlay.prompt.name = snapshot
                    .prompt_profiles
                    .get(candidate)
                    .cloned()
                    .unwrap_or_default();
                overlay.prompt.preview = None;
                overlay.prompt.request = None;
                overlay.prompt.error = None;
                overlay.prompt.scroll = 0;
            }
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
    Security {
        issue: agent_launcher_core::IssueKey,
        options: DispatchOptions,
    },
    Review {
        issue: agent_launcher_core::IssueKey,
        target: Option<String>,
        options: DispatchOptions,
    },
    Dispatch {
        issue: agent_launcher_core::IssueKey,
        profile: Option<String>,
        target: Option<String>,
        options: DispatchOptions,
    },
}

fn launch_action(
    review: bool,
    issue: agent_launcher_core::IssueKey,
    profile: Option<String>,
    target: Option<String>,
    options: DispatchOptions,
) -> LaunchAction {
    if review {
        LaunchAction::Review {
            issue,
            target,
            options,
        }
    } else {
        LaunchAction::Dispatch {
            issue,
            profile,
            target,
            options,
        }
    }
}

fn start_launch(
    app: &mut AppState,
    runtime: &RuntimeHandle,
    actions: &UiActionSender,
    action: LaunchAction,
) {
    let (issue, profile, target, options) = match action {
        LaunchAction::Security { issue, options } => {
            if app.security_launch_pending.is_some() {
                return;
            }
            app.next_request_id = app.next_request_id.wrapping_add(1);
            let request_id = app.next_request_id;
            app.security_launch_pending = Some(request_id);
            app.status_message = Some("starting private security review...".into());
            let runtime = runtime.clone();
            let actions = actions.clone();
            tokio::spawn(async move {
                let result = runtime.dispatch_security(issue, options, true).await;
                let _ = actions.send(UiActionResult::Security { request_id, result });
            });
            return;
        },
        LaunchAction::Review {
            issue,
            target,
            options,
        } => {
            app.status_message = Some("starting PR review...".to_owned());
            let runtime = runtime.clone();
            spawn_runtime_action(actions, "PR review started", None, async move {
                runtime.review_with_options(issue, target, options).await
            });
            return;
        },
        LaunchAction::Dispatch {
            issue,
            profile,
            target,
            options,
        } => (issue, profile, target, options),
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
        runtime
            .dispatch_with_options(issue, profile, target, options)
            .await
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
    if issue.security_advisory.is_some()
        || app
            .latest_run(snapshot, issue)
            .is_some_and(|run| run.confidential)
    {
        app.status_message = Some("Private clone retained. Separate private-clone cleanup is unavailable; normal worktree deletion is blocked.".into());
        return;
    }
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
        app.status_message = Some(if app.tab == crate::app::InboxTab::Security {
            "no private advisory selected".to_owned()
        } else if app.tab == crate::app::InboxTab::PullRequests {
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
        UiActionResult::Away { request_id, result } => {
            if app.away_pending == Some(request_id) {
                app.away_pending = None;
                set_result(
                    app,
                    result,
                    "mode request applied; keep this TUI open for worker finalization",
                );
            }
        },
        UiActionResult::Security { request_id, result } => {
            if app.security_launch_pending != Some(request_id) {
                return;
            }
            app.security_launch_pending = None;
            // Runtime errors can contain provider response bodies. Keep this status generic.
            app.status_message = Some(if result.is_ok() {
                "private security review started"
            } else {
                "private security dispatch failed; check advisory access and local backend configuration (no public fallback)"
            }.into());
        },
        UiActionResult::Prompt {
            request_id,
            issue_key,
            name,
            result,
        } => {
            let Some(overlay) = app.dispatch_overlay.as_mut() else {
                return;
            };
            let view = &mut overlay.prompt;
            let current_name = view
                .editor
                .as_ref()
                .map_or(view.name.as_str(), |e| e.name.as_str());
            if overlay.stage != DispatchStage::Prompt
                || overlay.issue_key != issue_key
                || view.request != Some(request_id)
                || current_name != name
            {
                return;
            }
            view.request = None;
            view.loading_source = false;
            match result {
                Ok(PromptReply::Preview(text)) => view.preview = Some(Ok(text)),
                Ok(PromptReply::Loaded(document)) => {
                    view.error = None;
                    view.editor = Some(crate::app::PromptEditor {
                        name: document.name,
                        naming: false,
                        buffer: crate::widgets::editor::Editor::new(document.source.clone()),
                        original: Some(document.source),
                        discard: false,
                        busy: false,
                    });
                },
                Ok(PromptReply::Saved(document)) => {
                    view.name = document.name;
                    view.editor = None;
                    view.error = None;
                    view.preview = None;
                    view.scroll = 0;
                },
                Err(error) => {
                    view.scroll = 0;
                    if let Some(editor) = &mut view.editor {
                        editor.busy = false;
                    } else {
                        view.preview = Some(Err(error.clone()));
                    }
                    view.error = Some(error);
                },
            }
        },
        UiActionResult::IssueDelete { issue_key, result } => {
            if !app
                .issue_delete_overlay
                .as_ref()
                .is_some_and(|overlay| overlay.pending && overlay.issue_key == issue_key)
            {
                return;
            }
            app.issue_delete_overlay = None;
            app.issue_delete_confirmation_visible = false;
            apply_delete_result(
                app,
                result,
                "source issue deleted; worktrees and run history retained",
                &issue_key.canonical(),
            );
        },
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
        let _ = execute!(output, DisableMouseCapture, DisableBracketedPaste);
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
    fn away_pending_results_paste_and_search_are_isolated() {
        let snapshot = RuntimeSnapshot::default();
        let mut app = AppState::default();
        handle_list_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            &snapshot,
        );
        assert_eq!(app.search_query, "m");
        crate::away::open(&mut app, &snapshot);
        assert!(handle_paste(&mut app, "64"));
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "64");
        app.away_pending = Some(2);
        assert!(!handle_paste(&mut app, "1"));
        apply_ui_action_result(&mut app, UiActionResult::Away {
            request_id: 1,
            result: Ok(()),
        });
        assert_eq!(app.away_pending, Some(2));
        assert!(!request_quit(&mut app, &snapshot));
        assert!(app.away_quit);
        assert!(!app.away_quit_visible);
        apply_ui_action_result(&mut app, UiActionResult::Away {
            request_id: 2,
            result: Ok(()),
        });
        assert!(app.away_pending.is_none());
        assert_eq!(app.search_query, "m");
    }

    #[test]
    fn commands_available_in_detail_but_not_input_modal() {
        let mut app = AppState {
            route: Route::Detail,
            ..Default::default()
        };
        let key = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(handle_debug_key(&mut app, key));
        assert!(app.command_overlay);
        app.command_overlay = false;
        app.input_overlay = Some(InputOverlay {
            run_id: "run".into(),
            prompt: "Input".into(),
            text: String::new(),
        });
        assert!(!handle_debug_key(&mut app, key));
        assert!(!app.command_overlay);
    }

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
            security_advisory: None,
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

    #[test]
    fn issue_delete_is_detail_only_uppercase_and_rejects_unsupported_sources() {
        let mut target = issue("1", Duration::zero());
        target.key.provider = IssueProvider::Beads;
        let mut snapshot = RuntimeSnapshot {
            issues: vec![target.clone()],
            ..Default::default()
        };
        let mut app = AppState::default();
        assert!(!handle_issue_delete_shortcut(
            &mut app,
            KeyCode::Char('X').into(),
            &snapshot
        ));
        app.open_detail(&snapshot);
        assert!(!handle_issue_delete_shortcut(
            &mut app,
            KeyCode::Char('x').into(),
            &snapshot
        ));
        assert!(app.issue_delete_overlay.is_none());
        assert!(handle_issue_delete_shortcut(
            &mut app,
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
            &snapshot
        ));
        assert_eq!(
            app.issue_delete_overlay.as_ref().unwrap().issue_key,
            target.key
        );
        assert!(app.delete_overlay.is_none());
        prepare_issue_delete(&mut app, KeyCode::Esc.into());
        for unsupported in [
            issue("1", Duration::zero()),
            Issue {
                key: IssueKey {
                    provider: IssueProvider::Gitlab,
                    ..target.key.clone()
                },
                ..target.clone()
            },
            Issue {
                pull_request: pull_request("1").pull_request,
                ..target.clone()
            },
        ] {
            snapshot.issues = vec![unsupported.clone()];
            app.detail_issue_key = Some(unsupported.key);
            open_issue_delete(&mut app, &snapshot);
            assert!(app.issue_delete_overlay.is_none());
            assert!(
                app.status_message
                    .as_deref()
                    .unwrap()
                    .contains("Unsupported: use provider tools")
            );
        }
    }

    #[tokio::test]
    async fn issue_delete_captures_identity_blocks_duplicate_submit_and_matches_async_result() {
        let mut target = issue("id:%:1", Duration::zero());
        target.key.provider = IssueProvider::Beads;
        target.key.host = "local:host%".into();
        target.key.repository = "repo:one%".into();
        let mut snapshot = RuntimeSnapshot {
            issues: vec![target.clone()],
            ..Default::default()
        };
        let mut app = AppState::default();
        app.open_detail(&snapshot);
        open_issue_delete(&mut app, &snapshot);
        assert!(prepare_issue_delete(&mut app, KeyCode::Enter.into()).is_none());
        assert!(!app.issue_delete_overlay.as_ref().unwrap().pending);
        prepare_issue_delete(&mut app, KeyCode::Esc.into());
        assert!(app.issue_delete_overlay.is_none());
        open_issue_delete(&mut app, &snapshot);
        snapshot.issues.clear();
        assert!(!app.reconcile_detail(&snapshot));
        assert_eq!(
            app.issue_delete_overlay.as_ref().unwrap().title,
            target.title
        );
        app.issue_delete_confirmation_visible = true;
        let captured = prepare_issue_delete(&mut app, KeyCode::Enter.into()).unwrap();
        assert_eq!(captured, target.key);
        for key in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('X'),
            KeyCode::Char('x'),
        ] {
            assert!(prepare_issue_delete(&mut app, key.into()).is_none());
            assert!(app.issue_delete_overlay.as_ref().unwrap().pending);
        }
        let mut other = captured.clone();
        other.repository = "repo%3Aone%".into();
        assert_ne!(other.canonical(), captured.canonical());
        apply_ui_action_result(&mut app, UiActionResult::IssueDelete {
            issue_key: other.clone(),
            result: Ok(()),
        });
        assert!(app.issue_delete_overlay.is_some());
        app.detail_issue_key = Some(other.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            tx.send(UiActionResult::IssueDelete {
                issue_key: captured,
                result: Ok(()),
            })
            .unwrap();
        });
        apply_ui_action_result(&mut app, rx.recv().await.unwrap());
        assert!(app.issue_delete_overlay.is_none());
        assert_eq!(app.detail_issue_key, Some(other));
        assert_eq!(app.route, Route::Detail);
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("run history retained")
        );
    }

    #[test]
    fn issue_delete_result_closes_only_matching_detail_and_reports_errors() {
        for success in [false, true] {
            let mut target = issue("1", Duration::zero());
            target.key.provider = IssueProvider::Beads;
            let snapshot = RuntimeSnapshot {
                issues: vec![target.clone()],
                ..Default::default()
            };
            let mut app = AppState::default();
            app.open_detail(&snapshot);
            open_issue_delete(&mut app, &snapshot);
            app.issue_delete_confirmation_visible = true;
            let key = prepare_issue_delete(&mut app, KeyCode::Enter.into()).unwrap();
            apply_ui_action_result(&mut app, UiActionResult::IssueDelete {
                issue_key: key,
                result: if success {
                    Ok(())
                } else {
                    Err(agent_launcher_runtime::Error::RunAlreadyActive(
                        "run-1".into(),
                    ))
                },
            });
            assert_eq!(
                app.route,
                if success {
                    Route::Inbox
                } else {
                    Route::Detail
                }
            );
            assert!(app.issue_delete_overlay.is_none());
            if !success {
                assert!(
                    app.status_message
                        .as_deref()
                        .unwrap()
                        .contains("runtime error")
                );
            }
        }
    }

    #[test]
    fn issue_delete_requires_fully_drawn_target_and_warnings_and_blocks_mouse() {
        let mut target = issue("1", Duration::zero());
        target.key.provider = IssueProvider::Beads;
        target.title = "Target with Unicode 界 and a newline\nnot a warning".into();
        let snapshot = RuntimeSnapshot {
            issues: vec![target],
            ..Default::default()
        };
        let mut app = AppState::default();
        app.open_detail(&snapshot);
        open_issue_delete(&mut app, &snapshot);
        for (width, height, visible) in [
            (120, 40, true),
            (30, 8, false),
            (1, 1, false),
            (60, 40, true),
            (120, 12, false),
        ] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, &snapshot, &mut app))
                .unwrap();
            assert_eq!(
                app.issue_delete_confirmation_visible, visible,
                "{width}x{height}"
            );
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            if width == 120 && visible {
                for warning in [
                    "Permanent source deletion",
                    "dependency links",
                    "orphans dependents",
                    "run history are NOT deleted",
                    "Enter permanently delete issue",
                    "\\nnot a warning",
                    "provider    beads",
                    "host        \"github.com\"",
                    "repository  \"acme/launcher\"",
                    "exact key   \"beads:github.com:acme/launcher:1\"",
                ] {
                    assert!(text.contains(warning), "missing {warning}");
                }
            }
            if !visible {
                assert!(prepare_issue_delete(&mut app, KeyCode::Enter.into()).is_none());
            }
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::ScrollDown,
                    column: 5,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                },
                &snapshot,
                (width, height)
            ));
            assert_eq!(app.detail_scroll, 0);
            for key in [
                KeyCode::Char('d'),
                KeyCode::Char('x'),
                KeyCode::Tab,
                KeyCode::Down,
            ] {
                assert!(prepare_issue_delete(&mut app, key.into()).is_none());
            }
            assert!(!app.issue_delete_overlay.as_ref().unwrap().pending);
        }
        app.issue_delete_overlay.as_mut().unwrap().title = "very long target ".repeat(1000);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| draw(frame, &snapshot, &mut app))
            .unwrap();
        assert!(!app.issue_delete_confirmation_visible);
        assert!(prepare_issue_delete(&mut app, KeyCode::Enter.into()).is_none());
        assert!(prepare_issue_delete(&mut app, KeyCode::Esc.into()).is_none());
        assert!(app.issue_delete_overlay.is_none());
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

    fn security_issue(id: &str) -> Issue {
        let mut issue = issue(id, Duration::zero());
        issue.key.native_id = format!("advisory/{id}");
        issue.identifier = id.into();
        issue.title = "Private advisory title".into();
        issue.description = Some("CONFIDENTIAL BODY SENT ONLY AFTER CONSENT".into());
        issue.state = "draft".into();
        issue.security_advisory = Some(agent_launcher_core::SecurityAdvisoryMetadata {
            ghsa_id: id.into(),
            cve_id: Some("CVE-2026-1234".into()),
            severity: Some("critical".into()),
        });
        issue
    }

    #[test]
    fn all_three_tabs_preserve_filters_sort_scroll_and_reconcile_by_key() {
        use crate::app::InboxTab;
        let mut snapshot = RuntimeSnapshot::default();
        for id in 1..=8 {
            snapshot.issues.extend([
                issue(&id.to_string(), Duration::days(id)),
                pull_request(&(id + 10).to_string()),
                security_issue(&format!("GHSA-aaaa-bbbb-{id:04}")),
            ]);
        }
        let mut app = AppState::default();
        let filters = ["Issue", "Review", "CVE-2026-1234"];
        let sorts = [IssueSort::Newest, IssueSort::Oldest, IssueSort::Title];
        let mut keys = Vec::new();
        for tab in InboxTab::ALL {
            app.set_tab(tab);
            app.search_query = filters[tab.index()].into();
            app.issue_sort = sorts[tab.index()];
            app.selected = tab.index() + 2;
            app.scroll = tab.index() + 1;
            keys.push(app.selected_issue(&snapshot).unwrap().key.clone());
        }
        let mut refreshed = snapshot.clone();
        refreshed.issues.reverse();
        app.reconcile_lists(&snapshot, &refreshed);
        assert_eq!(app.tab, InboxTab::Security);
        for tab in [InboxTab::PullRequests, InboxTab::Issues, InboxTab::Security] {
            handle_list_key(&mut app, KeyCode::BackTab.into(), &refreshed);
            assert_eq!(app.tab, tab);
            assert_eq!(app.search_query, filters[tab.index()]);
            assert_eq!(app.issue_sort, sorts[tab.index()]);
            assert_eq!(app.scroll, tab.index() + 1);
            assert_eq!(
                app.selected_issue(&refreshed).unwrap().key,
                keys[tab.index()]
            );
        }
        refreshed.issues.retain(|issue| issue.key != keys[2]);
        app.reconcile_lists(&snapshot, &refreshed);
        assert!(app.selected < app.rows(&refreshed).len());
        app.search_query.clear();
        for c in "d123?".chars() {
            handle_list_key(&mut app, KeyCode::Char(c).into(), &refreshed);
        }
        assert_eq!(app.search_query, "d123?");
        assert!(app.dispatch_overlay.is_none());
        handle_list_key(&mut app, KeyCode::Tab.into(), &refreshed);
        assert_eq!(app.tab, InboxTab::Issues);
    }

    #[test]
    fn security_settings_require_full_warning_and_explicit_consent_for_stable_target() {
        let snapshot = RuntimeSnapshot {
            issues: vec![
                security_issue("GHSA-aaaa-bbbb-cccc"),
                security_issue("GHSA-dddd-eeee-ffff"),
            ],
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "claude".into(),
            prompt_profiles: vec!["must-not-load".into()],
            ..Default::default()
        };
        let mut app = AppState {
            tab: crate::app::InboxTab::Security,
            ..Default::default()
        };
        let target = app.selected_issue(&snapshot).unwrap().key.clone();
        assert!(prepare_dispatch(&mut app, &snapshot).is_none());
        let overlay = app.dispatch_overlay.as_ref().unwrap();
        assert!(overlay.settings.security);
        assert!(overlay.settings.instructions_editor.is_none());
        assert!(!overlay.settings.instructions_focused);
        assert!(overlay.settings.options.additional_instructions.is_empty());
        assert!(matches!(overlay.stage, DispatchStage::Settings {
            profile: None,
            target: None
        }));
        assert!(overlay.prompt.preview.is_none());
        assert!(overlay.prompt.request.is_none());
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Char('a').into(), &snapshot),
            DispatchEffect::None
        );
        assert!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .prompt
                .editor
                .is_none()
        );
        settings_key(&mut app, KeyCode::Char('2').into());
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .privacy_confirmation
        );
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Char('y').into(), &snapshot),
            DispatchEffect::None
        );
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::render::draw(frame, &snapshot, &mut app))
            .unwrap();
        assert!(app.security_confirmation_visible);
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
        ] {
            assert_eq!(
                dispatch_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char('y'), modifiers),
                    &snapshot
                ),
                DispatchEffect::None
            );
        }
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        assert_eq!(
            dispatch_key(
                &mut app,
                KeyEvent {
                    kind: KeyEventKind::Repeat,
                    ..KeyCode::Char('y').into()
                },
                &snapshot
            ),
            DispatchEffect::None
        );
        app.selected = 1;
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Char('y').into(), &snapshot),
            DispatchEffect::Select(0)
        );
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 0),
            Some(LaunchAction::Security {
                issue: target,
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Herdr),
                    model: ModelSelection::HarnessDefault,
                    ..Default::default()
                },
            })
        );
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        app.security_launch_pending = Some(42);
        app.reset_detail();
        assert_eq!(app.security_launch_pending, Some(42));
        assert!(prepare_dispatch(&mut app, &snapshot).is_none());
        apply_ui_action_result(&mut app, UiActionResult::Security {
            request_id: 41,
            result: Ok(()),
        });
        assert_eq!(app.security_launch_pending, Some(42));
        apply_ui_action_result(&mut app, UiActionResult::Security {
            request_id: 42,
            result: Ok(()),
        });
        assert!(app.security_launch_pending.is_none());
        app.security_launch_pending = Some(43);
        apply_ui_action_result(&mut app, UiActionResult::Security {
            request_id: 43,
            result: Err(agent_launcher_runtime::Error::PromptProfileNotFound(
                "CONFIDENTIAL_RESPONSE_BODY".into(),
            )),
        });
        assert!(
            !app.status_message
                .as_deref()
                .unwrap()
                .contains("CONFIDENTIAL_RESPONSE_BODY")
        );
    }

    #[test]
    fn security_rejects_unsupported_readonly_changed_and_missing_targets() {
        let mut snapshot = RuntimeSnapshot {
            issues: vec![security_issue("GHSA-aaaa-bbbb-cccc")],
            selected_backend: Some(BackendKind::Herdr),
            ..Default::default()
        };
        let mut app = AppState {
            tab: crate::app::InboxTab::Security,
            ..Default::default()
        };
        for backend in [
            None,
            Some(BackendKind::Superset),
            Some(BackendKind::Conductor),
        ] {
            snapshot.selected_backend = backend;
            assert!(prepare_dispatch(&mut app, &snapshot).is_none());
            assert!(app.dispatch_overlay.is_none());
            assert!(
                app.status_message
                    .as_deref()
                    .unwrap()
                    .contains("No automatic public fallback")
            );
        }
        snapshot.selected_backend = Some(BackendKind::Native);
        snapshot.compute_targets.push(compute_target(
            "remote",
            ComputeTargetAvailability::Online,
            0,
            None,
        ));
        assert!(prepare_dispatch(&mut app, &snapshot).is_none());
        assert!(app.dispatch_overlay.is_none());
        snapshot.compute_targets.clear();
        for state in ["triage", "closed", "published"] {
            snapshot.issues[0].state = state.into();
            prepare_dispatch(&mut app, &snapshot);
            assert!(app.dispatch_overlay.is_none());
            assert!(app.status_message.as_deref().unwrap().contains("read-only"));
        }
        snapshot.issues[0].state = "draft".into();
        app.route = Route::Detail;
        app.detail_issue_key = Some(snapshot.issues[0].key.clone());
        open_issue_delete(&mut app, &snapshot);
        assert!(app.issue_delete_overlay.is_none());
        app.reset_detail();
        prepare_dispatch(&mut app, &snapshot);
        select_dispatch(&mut app, &snapshot, 0);
        snapshot.compute_targets.push(compute_target(
            "remote",
            ComputeTargetAvailability::Online,
            0,
            None,
        ));
        app.security_confirmation_visible = true;
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(app.dispatch_overlay.is_none());
        snapshot.compute_targets.clear();
        prepare_dispatch(&mut app, &snapshot);
        select_dispatch(&mut app, &snapshot, 0);
        app.security_confirmation_visible = true;
        snapshot.selected_model = Some("changed-default".into());
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(app.dispatch_overlay.is_none());
        prepare_dispatch(&mut app, &snapshot);
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        assert!(app.dispatch_overlay.is_none());
        prepare_dispatch(&mut app, &snapshot);
        assert!(!app.security_confirmation_visible);
        select_dispatch(&mut app, &snapshot, 0);
        assert!(!app.security_confirmation_visible);
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        assert!(app.dispatch_overlay.is_none());
        prepare_dispatch(&mut app, &snapshot);
        select_dispatch(&mut app, &snapshot, 0);
        app.security_confirmation_visible = true;
        snapshot.issues[0].state = "closed".into();
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(app.dispatch_overlay.is_none());
        snapshot.issues[0].state = "draft".into();
        prepare_dispatch(&mut app, &snapshot);
        snapshot.issues.clear();
        assert!(app.reconcile_dispatch(&snapshot));
        assert!(app.dispatch_overlay.is_none());
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
            {
                assert!(prepare_dispatch(&mut app, &snapshot).is_none());
                select_dispatch(&mut app, &snapshot, 0)
            },
            Some(LaunchAction::Review {
                options: DispatchOptions::default(),
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
            {
                assert!(select_dispatch(&mut app, &snapshot, 2).is_none());
                select_dispatch(&mut app, &snapshot, 0)
            },
            Some(LaunchAction::Review {
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Native),
                    ..Default::default()
                },
                issue: pr.key.clone(),
                target: Some("ready".into()),
            })
        );
        assert!(app.dispatch_overlay.is_none());
        app.dispatch_overlay = staged_dispatch_overlay(pr.key.clone(), &snapshot);
        assert_eq!(
            {
                assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
                select_dispatch(&mut app, &snapshot, 0)
            },
            Some(LaunchAction::Review {
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Native),
                    ..Default::default()
                },
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
            {
                assert!(select_dispatch(&mut app, &snapshot, 1).is_none());
                dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
                select_dispatch(&mut app, &snapshot, 0)
            },
            Some(LaunchAction::Dispatch {
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Native),
                    ..Default::default()
                },
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
    fn herdr_profile_selection_requires_settings_without_a_target_stage() {
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
            {
                assert!(select_dispatch(&mut app, &snapshot, 1).is_none());
                dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
                select_dispatch(&mut app, &snapshot, 0)
            },
            Some(LaunchAction::Dispatch {
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Herdr),
                    ..Default::default()
                },
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
                confidential: false,
                model: None,
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
                settings: Default::default(),
                issue_key: selected.key,
                cursor: 0,
                stage: DispatchStage::Prompt,
                prompt: Default::default(),
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
                settings: crate::app::LaunchSettings {
                    had_targets: true,
                    ..Default::default()
                },
                issue_key: selected_key.clone(),
                prompt: Default::default(),
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
                settings: Default::default(),
                issue_key: selected.key,
                cursor: 0,
                stage: DispatchStage::Target { profile: None },
                prompt: Default::default(),
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
                settings: Default::default(),
                issue_key: selected.key,
                cursor: 1,
                prompt: Default::default(),
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
    fn zero_or_one_prompt_profile_always_opens_chooser() {
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
        assert_eq!(overlay.stage, DispatchStage::Prompt);

        snapshot.prompt_profiles = vec!["implementer".to_owned()];
        let overlay = staged_dispatch_overlay(selected.key.clone(), &snapshot).unwrap();
        assert_eq!(overlay.stage, DispatchStage::Prompt);

        snapshot.selected_backend = Some(BackendKind::Superset);
        assert_eq!(
            staged_dispatch_overlay(selected.key, &snapshot)
                .unwrap()
                .stage,
            DispatchStage::Prompt
        );
    }

    #[test]
    fn prompt_enter_requires_preview_then_settings_for_zero_or_one_profile() {
        let selected = issue("1", Duration::zero());
        for profiles in [vec![], vec!["alpha".to_owned()]] {
            let snapshot = RuntimeSnapshot {
                issues: vec![selected.clone()],
                prompt_profiles: profiles,
                selected_backend: Some(BackendKind::Superset),
                ..Default::default()
            };
            let mut app = AppState::default();
            assert!(prepare_dispatch(&mut app, &snapshot).is_none());
            for preview in [None, Some(Err("invalid template".into()))] {
                app.dispatch_overlay.as_mut().unwrap().prompt.preview = preview;
                assert_eq!(
                    dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                    DispatchEffect::None
                );
            }
            app.dispatch_overlay.as_mut().unwrap().prompt.preview =
                Some(Ok("{{ issue_text }}".into()));
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                DispatchEffect::Select(0)
            );
            assert_eq!(
                {
                    assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
                    dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
                    select_dispatch(&mut app, &snapshot, 0)
                },
                Some(LaunchAction::Dispatch {
                    options: DispatchOptions {
                        expected_backend: Some(BackendKind::Superset),
                        ..Default::default()
                    },
                    issue: selected.key.clone(),
                    profile: snapshot.prompt_profiles.first().cloned(),
                    target: None,
                })
            );
            assert!(app.dispatch_overlay.is_none());
        }
    }

    #[test]
    fn prompt_modal_events_preserve_new_drafts_through_load_save_and_paste() {
        let selected = issue("1", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            prompt_profiles: vec!["zeta".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        prepare_dispatch(&mut app, &snapshot);
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Char('e').into(), &snapshot),
            DispatchEffect::Load
        );
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(1);
        app.dispatch_overlay.as_mut().unwrap().prompt.loading_source = true;
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Char('e').into(), &snapshot),
            DispatchEffect::None
        );
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        dispatch_key(&mut app, KeyCode::Char('a').into(), &snapshot);
        assert!(handle_paste(&mut app, "alpha\r\n\u{1b}"));
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 1,
            issue_key: selected.key.clone(),
            name: "zeta".into(),
            result: Ok(PromptReply::Loaded(
                agent_launcher_runtime::PromptDocument {
                    name: "zeta".into(),
                    source: "obsolete".into(),
                },
            )),
        });
        assert_eq!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .prompt
                .editor
                .as_ref()
                .unwrap()
                .name,
            "alpha"
        );
        dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
        assert!(handle_paste(&mut app, "é界\r\nx\ry\u{1b}[31m\u{3}"));
        for key in [
            KeyCode::Home,
            KeyCode::Up,
            KeyCode::Up,
            KeyCode::Right,
            KeyCode::Delete,
            KeyCode::Enter,
        ] {
            assert_eq!(
                dispatch_key(&mut app, key.into(), &snapshot),
                DispatchEffect::None
            );
        }
        let draft = app
            .dispatch_overlay
            .as_ref()
            .unwrap()
            .prompt
            .editor
            .as_ref()
            .unwrap()
            .buffer
            .text
            .clone();
        assert!(draft.starts_with("é\n\nx\ny[31m"));
        assert!(!draft.contains('\r') && !draft.contains('\u{1b}') && !draft.contains('\u{3}'));
        dispatch_key(&mut app, KeyCode::BackTab.into(), &snapshot);
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            dispatch_key(&mut app, save, &snapshot),
            DispatchEffect::Save(draft.clone(), None)
        );
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(2);
        for key in [
            KeyCode::Char('a').into(),
            KeyCode::Char('e').into(),
            KeyCode::Tab.into(),
            KeyCode::Enter.into(),
            KeyCode::Esc.into(),
            save,
        ] {
            assert_eq!(dispatch_key(&mut app, key, &snapshot), DispatchEffect::None);
        }
        assert!(!handle_paste(&mut app, "lost edit"));
        app.route = Route::Detail;
        app.detail_issue_key = Some(selected.key.clone());
        assert!(!app.reconcile_detail(&RuntimeSnapshot::default()));
        assert!(!app.reconcile_dispatch(&RuntimeSnapshot::default()));
        assert_eq!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .prompt
                .editor
                .as_ref()
                .unwrap()
                .buffer
                .text,
            draft
        );
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 2,
            issue_key: selected.key.clone(),
            name: "alpha".into(),
            result: Err("conflict: file changed".into()),
        });
        app.dispatch_overlay.as_mut().unwrap().prompt.scroll_max = 30;
        dispatch_key(&mut app, KeyCode::PageDown.into(), &snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().prompt.scroll, 8);
        dispatch_key(&mut app, KeyCode::PageUp.into(), &snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().prompt.scroll, 0);
        assert_eq!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .prompt
                .editor
                .as_ref()
                .unwrap()
                .buffer
                .text,
            draft
        );
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        assert!(!handle_paste(&mut app, "y"));
        dispatch_key(&mut app, KeyCode::Char('n').into(), &snapshot);
        assert_eq!(
            dispatch_key(&mut app, save, &snapshot),
            DispatchEffect::Save(draft.clone(), None)
        );
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(3);
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 3,
            issue_key: selected.key,
            name: "alpha".into(),
            result: Ok(PromptReply::Saved(agent_launcher_runtime::PromptDocument {
                name: "alpha".into(),
                source: draft,
            })),
        });
        // Save acknowledgement may precede the UI consuming the inventory watch update.
        app.reconcile_dispatch(&snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().prompt.name, "alpha");
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        let mut published = snapshot;
        published.prompt_profiles.push("alpha".into());
        app.reconcile_dispatch(&published);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 1);
        app.dispatch_overlay.as_mut().unwrap().prompt.preview = Some(Ok("rendered alpha".into()));
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &published),
            DispatchEffect::Select(1)
        );
    }

    #[test]
    fn prompt_modal_selection_invalidates_load_and_keeps_captured_issue() {
        let first = issue("1", Duration::zero());
        let second = issue("2", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![first.clone(), second],
            prompt_profiles: vec!["alpha".into(), "beta".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        prepare_dispatch(&mut app, &snapshot);
        let captured = app.dispatch_overlay.as_ref().unwrap().issue_key.clone();
        app.selected = 1;
        dispatch_key(&mut app, KeyCode::Char('e').into(), &snapshot);
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(1);
        dispatch_key(&mut app, KeyCode::Char('2').into(), &snapshot);
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 1,
            issue_key: captured.clone(),
            name: "alpha".into(),
            result: Err("obsolete load error".into()),
        });
        let overlay = app.dispatch_overlay.as_ref().unwrap();
        assert_eq!(overlay.issue_key, captured);
        assert_eq!(overlay.prompt.name, "beta");
        assert!(overlay.prompt.error.is_none());
        assert!(overlay.prompt.editor.is_none());
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        assert!(app.dispatch_overlay.is_none());
    }

    #[test]
    fn prompt_results_reject_obsolete_identity_and_generation() {
        let selected = issue("1", Duration::zero());
        let snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            prompt_profiles: vec!["alpha".into(), "beta".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        assert_eq!(prepare_dispatch(&mut app, &snapshot), None);
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(10);
        for (request_id, key, name) in [
            (9, selected.key.clone(), "alpha"),
            (10, issue("2", Duration::zero()).key, "alpha"),
            (10, selected.key.clone(), "beta"),
        ] {
            apply_ui_action_result(&mut app, UiActionResult::Prompt {
                request_id,
                issue_key: key,
                name: name.into(),
                result: Ok(PromptReply::Preview("obsolete".into())),
            });
            assert!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .prompt
                    .preview
                    .is_none()
            );
        }
        move_dispatch_cursor(&mut app, &snapshot, true);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().prompt.name, "beta");
        app.dispatch_overlay = None;
        prepare_dispatch(&mut app, &snapshot);
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(11);
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 10,
            issue_key: selected.key.clone(),
            name: "alpha".into(),
            result: Ok(PromptReply::Preview("obsolete reopened".into())),
        });
        assert!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .prompt
                .preview
                .is_none()
        );
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 11,
            issue_key: selected.key,
            name: "alpha".into(),
            result: Ok(PromptReply::Preview("{{ issue_text }}".into())),
        });
        assert_eq!(
            app.dispatch_overlay.as_ref().unwrap().prompt.preview,
            Some(Ok("{{ issue_text }}".into()))
        );
        let mut reordered = snapshot.clone();
        reordered.prompt_profiles.reverse();
        app.reconcile_dispatch(&reordered);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 1);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().prompt.name, "alpha");
    }

    #[test]
    fn prompt_editor_retains_conflicts_and_selects_saved_name_after_inventory_reorder() {
        let selected = issue("1", Duration::zero());
        let mut snapshot = RuntimeSnapshot {
            issues: vec![selected.clone()],
            prompt_profiles: vec!["zeta".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        prepare_dispatch(&mut app, &snapshot);
        app.dispatch_overlay.as_mut().unwrap().prompt.request = Some(1);
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 1,
            issue_key: selected.key.clone(),
            name: "zeta".into(),
            result: Ok(PromptReply::Loaded(
                agent_launcher_runtime::PromptDocument {
                    name: "zeta".into(),
                    source: "{{ issue_title }}".into(),
                },
            )),
        });
        let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
        assert_eq!(
            view.editor.as_ref().unwrap().buffer.text,
            "{{ issue_title }}"
        );
        edit_prompt(view, KeyCode::Enter.into());
        let draft = view.editor.as_ref().unwrap().buffer.text.clone();
        let save_key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            edit_prompt(view, save_key),
            Some((draft.clone(), Some("{{ issue_title }}".into())))
        );
        assert!(edit_prompt(view, save_key).is_none());
        view.request = Some(2);
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 2,
            issue_key: selected.key.clone(),
            name: "zeta".into(),
            result: Err("conflict: file changed on disk".into()),
        });
        let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
        assert_eq!(view.editor.as_ref().unwrap().buffer.text, draft);
        assert!(!view.editor.as_ref().unwrap().busy);
        assert!(view.error.as_deref().unwrap().contains("conflict"));
        edit_prompt(view, KeyCode::Esc.into());
        assert!(view.editor.as_ref().unwrap().discard);
        edit_prompt(view, KeyCode::Esc.into());
        assert!(!view.editor.as_ref().unwrap().discard);
        edit_prompt(view, KeyCode::Esc.into());
        edit_prompt(view, KeyCode::Char('y').into());
        assert!(view.editor.is_none());

        view.editor = Some(crate::app::PromptEditor {
            name: "alpha".into(),
            naming: false,
            buffer: crate::widgets::editor::Editor::new(draft.clone()),
            original: None,
            discard: false,
            busy: false,
        });
        assert_eq!(edit_prompt(view, save_key), Some((draft.clone(), None)));
        view.request = Some(3);
        // The watch inventory can arrive before the save acknowledgement.
        snapshot.prompt_profiles.insert(0, "alpha".into());
        app.reconcile_dispatch(&snapshot);
        apply_ui_action_result(&mut app, UiActionResult::Prompt {
            request_id: 3,
            issue_key: selected.key,
            name: "alpha".into(),
            result: Ok(PromptReply::Saved(agent_launcher_runtime::PromptDocument {
                name: "alpha".into(),
                source: draft,
            })),
        });
        app.reconcile_dispatch(&snapshot);
        let overlay = app.dispatch_overlay.as_ref().unwrap();
        assert_eq!(overlay.cursor, 0);
        assert_eq!(overlay.prompt.name, "alpha");
        assert!(overlay.prompt.preview.is_none());
        assert!(overlay.prompt.editor.is_none());
    }

    #[test]
    fn target_stage_keeps_selected_profile_when_profiles_refresh() {
        let selected = issue("2", Duration::zero());
        let mut app = AppState {
            dispatch_overlay: Some(DispatchOverlay {
                settings: crate::app::LaunchSettings {
                    target_choices: vec!["ready".into()],
                    ..Default::default()
                },
                issue_key: selected.key.clone(),
                prompt: Default::default(),
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

    #[test]
    fn launch_settings_harness_kinds_and_model_defaults_are_backend_aware() {
        for (backend, expected) in [
            (BackendKind::Herdr, vec!["opencode", "claude", "pi"]),
            (BackendKind::Native, vec!["opencode"]),
            (BackendKind::Conductor, vec![
                "claude", "codex", "cursor", "acp",
            ]),
            (BackendKind::Superset, vec![]),
        ] {
            let snapshot = RuntimeSnapshot {
                issues: vec![pull_request("42")],
                selected_backend: Some(backend),
                selected_agent: "configured-custom".into(),
                selected_model: Some("provider/old-model".into()),
                ..Default::default()
            };
            let mut app = AppState {
                tab: crate::app::InboxTab::PullRequests,
                ..Default::default()
            };
            assert!(prepare_dispatch(&mut app, &snapshot).is_none());
            let settings = &app.dispatch_overlay.as_ref().unwrap().settings;
            assert_eq!(settings.harness_choices(), expected);
            assert_eq!(
                settings.model_label(),
                "Configured default: provider/old-model"
            );
            for harness in &expected {
                assert_eq!(
                    dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot),
                    DispatchEffect::None
                );
                let settings = &app.dispatch_overlay.as_ref().unwrap().settings;
                assert_eq!(settings.options.harness.as_deref(), Some(*harness));
                assert_eq!(settings.options.model, ModelSelection::HarnessDefault);
                dispatch_key(&mut app, KeyCode::Char('1').into(), &snapshot);
                assert_eq!(
                    app.dispatch_overlay
                        .as_ref()
                        .unwrap()
                        .settings
                        .model_label(),
                    if backend == BackendKind::Native {
                        "Configured default: provider/old-model"
                    } else {
                        "Configured default: uses harness default (different harness)"
                    }
                );
            }
            dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot);
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .options
                    .harness,
                None
            );
            dispatch_key(&mut app, KeyCode::Char('1').into(), &snapshot);
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .options
                    .model,
                ModelSelection::Inherit
            );
            app.dispatch_overlay
                .as_mut()
                .unwrap()
                .settings
                .default_model = None;
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .model_label(),
                "Configured default: uses harness default"
            );
        }
    }

    #[test]
    fn native_subagent_configured_model_summary_and_confirmed_options_agree() {
        for review in [false, true] {
            let selected = if review {
                pull_request("42")
            } else {
                issue("1", Duration::zero())
            };
            let snapshot = RuntimeSnapshot {
                issues: vec![selected.clone()],
                selected_backend: Some(BackendKind::Native),
                selected_agent: "reviewer".into(),
                selected_model: Some("openai/gpt-5.4".into()),
                prompt_profiles: vec!["reviewer".into()],
                ..Default::default()
            };
            let mut app = AppState {
                tab: if review {
                    crate::app::InboxTab::PullRequests
                } else {
                    crate::app::InboxTab::Issues
                },
                ..Default::default()
            };
            assert!(prepare_dispatch(&mut app, &snapshot).is_none());
            if !review {
                app.dispatch_overlay.as_mut().unwrap().prompt.preview = Some(Ok("source".into()));
                assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
                dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
            }
            dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot);
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .options
                    .model,
                ModelSelection::HarnessDefault
            );
            dispatch_key(&mut app, KeyCode::Char('1').into(), &snapshot);
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .model_label(),
                "Configured default: openai/gpt-5.4"
            );
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|frame| draw(frame, &snapshot, &mut app))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Configured default: openai/gpt-5.4"));
            assert!(!text.contains("different harness"));

            let mut changed = snapshot.clone();
            changed.selected_backend = Some(BackendKind::Herdr);
            assert!(select_dispatch(&mut app, &changed, 0).is_none());
            assert_eq!(
                app.dispatch_overlay.as_ref().unwrap().settings.backend,
                Some(BackendKind::Native)
            );
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                DispatchEffect::Select(0)
            );
            // Runtime resolves Native + opencode + Inherit to the configured model,
            // even when the configured OpenCode subagent is named "reviewer".
            let options = DispatchOptions {
                expected_backend: Some(BackendKind::Native),
                harness: Some("opencode".into()),
                model: ModelSelection::Inherit,
                ..Default::default()
            };
            let expected = if review {
                LaunchAction::Review {
                    issue: selected.key,
                    target: None,
                    options,
                }
            } else {
                LaunchAction::Dispatch {
                    issue: selected.key,
                    profile: Some("reviewer".into()),
                    target: None,
                    options,
                }
            };
            assert_eq!(select_dispatch(&mut app, &snapshot, 0), Some(expected));
        }
    }

    #[test]
    fn issue_instructions_focus_editing_guards_and_captured_launch_options() {
        let snapshot = RuntimeSnapshot {
            issues: vec![issue("1", Duration::zero()), issue("2", Duration::zero())],
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "claude".into(),
            selected_model: Some("sonnet".into()),
            prompt_profiles: vec!["reviewer".into()],
            ..Default::default()
        };
        let mut app = AppState {
            dispatch_overlay: staged_dispatch_overlay(snapshot.issues[0].key.clone(), &snapshot),
            ..Default::default()
        };
        app.dispatch_overlay.as_mut().unwrap().prompt.preview = Some(Ok("source".into()));
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::Select(0)
        );
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .instructions_focused
        );
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        for c in "hma123".chars() {
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Char(c).into(), &snapshot),
                DispatchEffect::None
            );
        }
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        assert!(handle_paste(&mut app, "é界\r\n```rust\ncode\n```\u{1b}"));
        for key in [
            KeyCode::Up,
            KeyCode::Home,
            KeyCode::Delete,
            KeyCode::End,
            KeyCode::Backspace,
            KeyCode::Char('é'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Down,
        ] {
            dispatch_key(&mut app, key.into(), &snapshot);
        }
        let draft = "hma123\né界\n```rust\nodé\n```";
        let settings = &app.dispatch_overlay.as_ref().unwrap().settings;
        assert_eq!(settings.instructions_editor.as_ref().unwrap().text, draft);
        assert_eq!(settings.options, DispatchOptions::default());
        assert!(settings.model_editor.is_none());
        for done in [
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyCode::Esc.into(),
        ] {
            assert_eq!(
                dispatch_key(&mut app, done, &snapshot),
                DispatchEffect::None
            );
            assert!(
                !app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .instructions_focused
            );
            assert_eq!(
                dispatch_key(&mut app, done, &snapshot),
                DispatchEffect::None
            );
            // Esc from controls goes back, without discarding instructions.
            if done.code == KeyCode::Esc {
                assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
            }
            dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
        }
        dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
        dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot);
        dispatch_key(&mut app, KeyCode::Char('m').into(), &snapshot);
        assert!(handle_paste(&mut app, "openai/gpt-5.4\n"));
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot),
            DispatchEffect::None
        );
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::None
        );
        let mut changed = snapshot.clone();
        changed.selected_model = None;
        assert!(select_dispatch(&mut app, &changed, 0).is_none());
        app.reconcile_dispatch(&snapshot);
        assert_eq!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .instructions_editor
                .as_ref()
                .unwrap()
                .text,
            draft
        );
        assert!(!handle_paste(&mut app, "not focused"));
        assert_eq!(
            dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
            DispatchEffect::Select(0)
        );
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 0),
            Some(LaunchAction::Dispatch {
                issue: snapshot.issues[0].key.clone(),
                profile: Some("reviewer".into()),
                target: None,
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Herdr),
                    harness: Some("opencode".into()),
                    model: ModelSelection::Explicit("openai/gpt-5.4".into()),
                    additional_instructions: draft.into(),
                },
            })
        );
        let fresh = staged_dispatch_overlay(snapshot.issues[1].key.clone(), &snapshot).unwrap();
        assert!(fresh.settings.instructions_editor.unwrap().text.is_empty());
        assert_eq!(fresh.settings.options, DispatchOptions::default());
    }

    #[test]
    fn model_drafts_confirm_cancel_and_launch_independently_for_pr_reviews() {
        let snapshot = RuntimeSnapshot {
            issues: vec![pull_request("42")],
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "claude".into(),
            selected_model: Some("sonnet".into()),
            ..Default::default()
        };
        let mut app = AppState {
            tab: crate::app::InboxTab::PullRequests,
            ..Default::default()
        };
        for harness in ["opencode", "claude", "pi"] {
            assert!(prepare_dispatch(&mut app, &snapshot).is_none());
            assert!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .instructions_editor
                    .is_none()
            );
            assert_eq!(
                app.dispatch_overlay.as_ref().unwrap().settings.options,
                DispatchOptions::default()
            );
            loop {
                dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot);
                if app
                    .dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .options
                    .harness
                    .as_deref()
                    == Some(harness)
                {
                    break;
                }
            }
            dispatch_key(&mut app, KeyCode::Char('m').into(), &snapshot);
            assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                DispatchEffect::None
            );
            assert!(
                app.status_message
                    .as_deref()
                    .unwrap()
                    .contains("cannot be empty")
            );
            assert!(handle_paste(&mut app, "é界\r\n\u{1b}"));
            for key in [KeyCode::Left, KeyCode::Backspace, KeyCode::Delete] {
                dispatch_key(&mut app, key.into(), &snapshot);
            }
            assert_eq!(
                app.dispatch_overlay
                    .as_ref()
                    .unwrap()
                    .settings
                    .model_editor
                    .as_ref()
                    .unwrap()
                    .text,
                ""
            );
            let model = if harness == "claude" {
                "sonnet"
            } else {
                "openai/gpt-5.4"
            };
            for c in model.chars() {
                dispatch_key(&mut app, KeyCode::Char(c).into(), &snapshot);
            }
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                DispatchEffect::None
            );
            let options = DispatchOptions {
                harness: Some(harness.into()),
                model: ModelSelection::Explicit(model.into()),
                ..Default::default()
            };
            assert_eq!(
                app.dispatch_overlay.as_ref().unwrap().settings.options,
                options
            );
            dispatch_key(&mut app, KeyCode::Char('m').into(), &snapshot);
            handle_paste(&mut app, "cancelled");
            dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
            assert_eq!(
                app.dispatch_overlay.as_ref().unwrap().settings.options,
                options
            );
            assert_eq!(
                dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot),
                DispatchEffect::Select(0)
            );
            assert_eq!(
                select_dispatch(&mut app, &snapshot, 0),
                Some(LaunchAction::Review {
                    issue: snapshot.issues[0].key.clone(),
                    target: None,
                    options: DispatchOptions {
                        expected_backend: Some(BackendKind::Herdr),
                        ..options
                    },
                })
            );
            assert!(app.dispatch_overlay.is_none());
        }
        assert_eq!(snapshot.selected_model.as_deref(), Some("sonnet"));
    }

    #[test]
    fn settings_keep_profile_target_and_options_across_refresh_and_back_navigation() {
        let mut snapshot = RuntimeSnapshot {
            issues: vec![issue("1", Duration::zero())],
            selected_backend: Some(BackendKind::Native),
            selected_agent: "custom-opencode-agent".into(),
            prompt_profiles: vec!["reviewer".into()],
            compute_targets: vec![
                compute_target("first", ComputeTargetAvailability::Online, 0, None),
                compute_target("second", ComputeTargetAvailability::Online, 0, None),
            ],
            ..Default::default()
        };
        let mut app = AppState::default();
        prepare_dispatch(&mut app, &snapshot);
        app.dispatch_overlay.as_mut().unwrap().prompt.preview = Some(Ok("source".into()));
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        move_dispatch_cursor(&mut app, &snapshot, true);
        snapshot.compute_targets.reverse();
        assert!(!app.reconcile_dispatch(&snapshot));
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 2);
        assert!(select_dispatch(&mut app, &snapshot, 2).is_none());
        assert!(handle_paste(
            &mut app,
            "Keep this draft\n```rust\n// unchanged\n```"
        ));
        dispatch_key(&mut app, KeyCode::Tab.into(), &snapshot);
        dispatch_key(&mut app, KeyCode::Char('m').into(), &snapshot);
        handle_paste(&mut app, "openai/gpt-5.4");
        dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot);
        snapshot.prompt_profiles = vec!["replacement".into(), "reviewer".into()];
        snapshot.issues.insert(0, issue("2", Duration::days(-1)));
        snapshot.compute_targets.reverse();
        app.reconcile_dispatch(&snapshot);
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        app.reconcile_dispatch(&snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 1);
        dispatch_key(&mut app, KeyCode::Esc.into(), &snapshot);
        app.reconcile_dispatch(&snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 1);
        assert_eq!(
            app.dispatch_overlay.as_ref().unwrap().prompt.name,
            "reviewer"
        );
        assert!(select_dispatch(&mut app, &snapshot, 1).is_none());
        app.reconcile_dispatch(&snapshot);
        assert_eq!(app.dispatch_overlay.as_ref().unwrap().cursor, 1);
        assert!(select_dispatch(&mut app, &snapshot, 1).is_none());
        assert_eq!(
            select_dispatch(&mut app, &snapshot, 0),
            Some(LaunchAction::Dispatch {
                issue: snapshot.issues[1].key.clone(),
                profile: Some("reviewer".into()),
                target: Some("first".into()),
                options: DispatchOptions {
                    expected_backend: Some(BackendKind::Native),
                    harness: None,
                    model: ModelSelection::Explicit("openai/gpt-5.4".into()),
                    additional_instructions: "Keep this draft\n```rust\n// unchanged\n```".into(),
                },
            })
        );
    }

    #[test]
    fn changed_defaults_do_not_overwrite_settings_and_harness_change_clears_custom_model() {
        let mut snapshot = RuntimeSnapshot {
            issues: vec![pull_request("42")],
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "claude".into(),
            selected_model: Some("sonnet".into()),
            ..Default::default()
        };
        let mut app = AppState {
            tab: crate::app::InboxTab::PullRequests,
            ..Default::default()
        };
        prepare_dispatch(&mut app, &snapshot);
        dispatch_key(&mut app, KeyCode::Char('m').into(), &snapshot);
        handle_paste(&mut app, "custom");
        snapshot.selected_model = Some("changed".into());
        app.reconcile_dispatch(&snapshot);
        dispatch_key(&mut app, KeyCode::Enter.into(), &snapshot);
        assert!(select_dispatch(&mut app, &snapshot, 0).is_none());
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("defaults changed")
        );
        let settings = &app.dispatch_overlay.as_ref().unwrap().settings;
        assert_eq!(settings.default_model.as_deref(), Some("sonnet"));
        assert_eq!(
            settings.options.model,
            ModelSelection::Explicit("custom".into())
        );
        dispatch_key(&mut app, KeyCode::Char('h').into(), &snapshot);
        assert_eq!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .options
                .model,
            ModelSelection::HarnessDefault
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
