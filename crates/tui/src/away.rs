use agent_launcher_core::{
    AppMode, AwayEntryState, AwayPhase, AwayRanking, BackendKind, RunState, RuntimeCommand,
    RuntimeSnapshot,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{app::AppState, theme};

pub(crate) struct AwayOverlay {
    pub tab: AppMode,
    pub tabs: Vec<(Rect, AppMode)>,
    pub limit: String,
    pub profile: Option<String>,
    pub ranking: AwayRanking,
    pub scroll: u16,
    pub scroll_max: u16,
    pub replace_limit: bool,
}

impl AwayOverlay {
    fn scroll_key(&mut self, code: KeyCode) -> bool {
        self.scroll = match code {
            KeyCode::Up => self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll.saturating_add(1).min(self.scroll_max),
            KeyCode::PageUp => self.scroll.saturating_sub(8),
            KeyCode::PageDown => self.scroll.saturating_add(8).min(self.scroll_max),
            KeyCode::Home => 0,
            KeyCode::End => self.scroll_max,
            _ => return false,
        };
        true
    }

    fn select_tab(&mut self, tab: AppMode) {
        if self.tab != tab {
            self.tab = tab;
            self.scroll = 0;
            self.scroll_max = 0;
            self.replace_limit = true;
            self.tabs.clear();
        }
    }
}

pub(crate) fn open(app: &mut AppState, snapshot: &RuntimeSnapshot) {
    app.command_overlay = false;
    app.away_overlay = Some(AwayOverlay {
        tab: snapshot.away.mode,
        tabs: Vec::new(),
        limit: snapshot.away.max_agents.to_string(),
        profile: snapshot.away.profile.clone().or_else(|| {
            (snapshot.away.mode == AppMode::Manual)
                .then(|| {
                    snapshot
                        .prompt_profiles
                        .iter()
                        .find(|name| name.as_str() == "implementer")
                        .or_else(|| snapshot.prompt_profiles.first())
                        .cloned()
                })
                .flatten()
        }),
        ranking: snapshot.away.ranking,
        scroll: 0,
        scroll_max: 0,
        replace_limit: true,
    });
}

pub(crate) fn needs_quit_warning(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.away.prioritizing
        || snapshot.away.entries.iter().any(|entry| {
            entry.run_id.as_ref().is_some_and(|id| {
                snapshot.runs.iter().find(|run| &run.id == id).map_or(
                    matches!(
                        entry.state,
                        AwayEntryState::Launching
                            | AwayEntryState::Running
                            | AwayEntryState::Attention
                    ),
                    |run| run.state.is_active() || run.state == RunState::Disconnected,
                )
            })
        })
}

pub(crate) fn prepare_command(
    app: &mut AppState,
    key: KeyEvent,
    snapshot: &RuntimeSnapshot,
) -> Option<RuntimeCommand> {
    if key.code == KeyCode::Esc {
        app.away_overlay = None;
        return None;
    }
    let overlay = app.away_overlay.as_mut()?;
    if app.away_pending.is_some() {
        return None;
    }
    if matches!(
        key.code,
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right
    ) {
        overlay.select_tab(match overlay.tab {
            AppMode::Manual => AppMode::Away,
            AppMode::Away => AppMode::Manual,
        });
        return None;
    }
    if overlay.scroll_key(key.code) {
        return None;
    }
    if overlay.tab == AppMode::Manual {
        return if matches!(key.code, KeyCode::Enter | KeyCode::Char('m'))
            && key.kind == KeyEventKind::Press
            && snapshot.away.mode != AppMode::Manual
        {
            Some(RuntimeCommand::SetManual)
        } else {
            None
        };
    }
    let key = if key.code == KeyCode::Enter {
        KeyEvent {
            code: KeyCode::Char(if snapshot.away.mode == AppMode::Manual {
                's'
            } else {
                'a'
            }),
            ..key
        }
    } else {
        key
    };
    if snapshot.away.mode == AppMode::Manual && matches!(key.code, KeyCode::Char('a' | 'l' | 'o')) {
        return None;
    }
    match key.code {
        KeyCode::Char(c @ '0'..='9') if overlay.replace_limit || overlay.limit.len() < 3 => {
            if overlay.replace_limit {
                overlay.limit.clear();
                overlay.replace_limit = false;
            }
            overlay.limit.push(c);
        },
        KeyCode::Backspace => {
            overlay.replace_limit = false;
            overlay.limit.pop();
        },
        KeyCode::Char(c @ ('+' | '-')) => {
            let value = overlay.limit.parse::<usize>().unwrap_or(5);
            overlay.limit = if c == '+' {
                value.saturating_add(1)
            } else {
                value.saturating_sub(1)
            }
            .clamp(1, 64)
            .to_string();
            overlay.replace_limit = true;
        },
        KeyCode::Char('p') => {
            let index = overlay
                .profile
                .as_ref()
                .and_then(|p| snapshot.prompt_profiles.iter().position(|v| v == p));
            overlay.profile = match (overlay.profile.is_some(), index) {
                (false, _) => snapshot.prompt_profiles.first().cloned(),
                (_, Some(i)) => snapshot.prompt_profiles.get(i + 1).cloned(),
                _ => None,
            };
        },
        KeyCode::Char('r') => {
            overlay.ranking = match overlay.ranking {
                AwayRanking::Agent => AwayRanking::SourcePriority,
                AwayRanking::SourcePriority => AwayRanking::Agent,
            }
        },
        KeyCode::Char('s' | 'l' | 'a' | 'm' | 'o') if key.kind == KeyEventKind::Press => {
            let command = match key.code {
                KeyCode::Char('m') => {
                    overlay.select_tab(AppMode::Manual);
                    return None;
                },
                KeyCode::Char('a') => {
                    if matches!(
                        snapshot.away.phase,
                        AwayPhase::Paused | AwayPhase::Attention
                    ) {
                        RuntimeCommand::ResumeAway
                    } else {
                        RuntimeCommand::PauseAway
                    }
                },
                KeyCode::Char('o') => RuntimeCommand::ReprioritizeAway {
                    ranking: overlay.ranking,
                },
                _ => {
                    let Ok(max_agents @ 1..=64) = overlay.limit.parse::<usize>() else {
                        app.status_message = Some("Max agents must be 1..=64".into());
                        return None;
                    };
                    if key.code == KeyCode::Char('l') {
                        RuntimeCommand::SetAwayConcurrency { max_agents }
                    } else {
                        if snapshot.away.mode != AppMode::Manual {
                            app.status_message = Some(
                                "Already Away; use pause/resume, change limit, or reprioritize"
                                    .into(),
                            );
                            return None;
                        }
                        if snapshot.selected_backend != Some(BackendKind::Herdr)
                            || snapshot.selected_agent != "opencode"
                        {
                            app.status_message =
                                Some("Away requires Herdr + OpenCode; no fallback".into());
                            return None;
                        }
                        if overlay
                            .profile
                            .as_ref()
                            .is_some_and(|p| !snapshot.prompt_profiles.contains(p))
                        {
                            app.status_message =
                                Some("Profile unavailable; choose another profile".into());
                            return None;
                        }
                        RuntimeCommand::StartAway {
                            max_agents,
                            profile: overlay.profile.clone(),
                            ranking: overlay.ranking,
                        }
                    }
                },
            };
            return Some(command);
        },
        _ => {},
    }
    None
}

pub(crate) fn handle_mouse(app: &mut AppState, event: MouseEvent, size: (u16, u16)) -> bool {
    if app.away_pending.is_some()
        || app.away_quit
        || (app.mouse.screen.width, app.mouse.screen.height) != size
    {
        return false;
    }
    let Some(overlay) = app.away_overlay.as_mut() else {
        return false;
    };
    if event.kind == MouseEventKind::Down(MouseButton::Left) {
        if let Some((_, tab)) = overlay
            .tabs
            .iter()
            .find(|(rect, _)| rect.contains((event.column, event.row).into()))
        {
            overlay.select_tab(*tab);
            return true;
        }
    } else if matches!(
        event.kind,
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
    ) {
        overlay.scroll = if event.kind == MouseEventKind::ScrollDown {
            overlay.scroll.saturating_add(3).min(overlay.scroll_max)
        } else {
            overlay.scroll.saturating_sub(3)
        };
        return true;
    }
    false
}

fn phase_label(phase: AwayPhase) -> &'static str {
    match phase {
        AwayPhase::Inactive => "Inactive",
        AwayPhase::Refreshing => "Refreshing",
        AwayPhase::Prioritizing => "Prioritizing",
        AwayPhase::Running => "Running",
        AwayPhase::Paused => "Paused",
        AwayPhase::Draining => "Draining",
        AwayPhase::QueueEmpty => "Queue empty",
        AwayPhase::Attention => "Attention",
    }
}

pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &mut AppState,
) {
    app.away_quit_visible = false;
    if !app.away_quit && app.away_overlay.is_none() {
        return;
    }
    let normal = Style::new().fg(theme::text()).bg(theme::panel());
    let muted = Style::new().fg(theme::muted());
    let accent = Style::new()
        .fg(theme::primary())
        .add_modifier(Modifier::BOLD);
    // De-emphasize the inbox without erasing the context behind the dialog.
    frame.buffer_mut().set_style(
        area,
        Style::new()
            .fg(theme::secondary())
            .add_modifier(Modifier::DIM),
    );
    if app.away_quit {
        let warning = vec![
            Line::styled("Leave this shift?", accent),
            Line::raw(""),
            Line::raw("Away workers are active or a mode request is pending."),
            Line::raw("Closing stops scheduling and finalization."),
            Line::raw("Workers may continue. Nothing is force-killed."),
            Line::raw(""),
            Line::from(vec![
                Span::styled(
                    " q ",
                    Style::new()
                        .fg(theme::bg())
                        .bg(theme::primary())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Quit   "),
                Span::styled("Esc", accent),
                Span::raw(" Cancel"),
            ]),
            Line::from(vec![
                Span::styled(" m ", accent),
                Span::raw(" Return to Manual and wait for workers"),
            ]),
        ];
        let popup = dialog_area(area, 12);
        let inner = popup.inner(Margin::new(2, 1));
        let paragraph = Paragraph::new(warning)
            .style(normal)
            .wrap(Wrap { trim: false });
        app.away_quit_visible =
            inner.width > 0 && paragraph.line_count(inner.width) <= inner.height as usize;
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(theme::border()))
                .style(normal),
            popup,
        );
        frame.render_widget(paragraph, inner);
        return;
    }
    let Some(overlay) = app.away_overlay.as_mut() else {
        return;
    };
    let away = &snapshot.away;
    let active = away.mode == AppMode::Away;
    let viewing_away = overlay.tab == AppMode::Away;
    let ready = snapshot.selected_backend == Some(BackendKind::Herdr)
        && snapshot.selected_agent == "opencode";
    let pending = app.away_pending.is_some();
    let selected = Style::new()
        .fg(theme::bg())
        .bg(theme::primary())
        .add_modifier(Modifier::BOLD);
    let field = |label: &str, value: String, hint: &str| {
        Line::from(vec![
            Span::styled(format!("{label:<16}"), muted),
            Span::styled(
                value,
                Style::new().fg(theme::text()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {hint}"), muted),
        ])
    };
    let mut lines = if viewing_away {
        vec![
            Line::styled("WORKER SETTINGS", accent),
            Line::from(vec![
                Span::styled("Max agents      ", muted),
                Span::styled(
                    format!(" {:>2} ", overlay.limit),
                    Style::new()
                        .fg(theme::primary())
                        .bg(theme::element())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  +/- adjust, or type a number", muted),
            ]),
            field(
                "Profile",
                overlay
                    .profile
                    .as_deref()
                    .unwrap_or("built-in prompt")
                    .into(),
                "[p]",
            ),
            field(
                "Priority",
                match overlay.ranking {
                    AwayRanking::Agent => "Agent ranked",
                    AwayRanking::SourcePriority => "Source priority",
                }
                .into(),
                "[r]",
            ),
            field(
                "Model",
                if active {
                    away.model.as_deref()
                } else {
                    snapshot.selected_model.as_deref()
                }
                .unwrap_or("harness default")
                .into(),
                "",
            ),
            Line::styled(
                if active {
                    "Apply limit / priority below. Profile applies on next start."
                } else {
                    "Ranks your backlog, then fills available agent slots."
                },
                muted,
            ),
            Line::raw(""),
        ]
    } else {
        vec![
            Line::styled("YOU CHOOSE WHAT RUNS", accent),
            Line::raw(""),
            Line::raw("Pick an issue, choose an agent, and launch when you're ready."),
            Line::styled("No automatic prioritization or queue refill.", muted),
            Line::raw(""),
            Line::styled("NO SETTINGS REQUIRED", accent),
            Line::styled(
                "Agent, model, and instructions are chosen for each launch.",
                muted,
            ),
            Line::raw(""),
            Line::styled(
                if active {
                    "Switching to Manual stops new automatic launches."
                } else {
                    "Manual is your current mode."
                },
                muted,
            ),
            Line::styled(
                "Existing workers finish normally. Their worktrees are retained.",
                muted,
            ),
        ]
    };
    if active && viewing_away {
        let count = |state| {
            away.entries
                .iter()
                .filter(|entry| entry.state == state)
                .count()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{}/{}",
                    away.occupied_slots(&snapshot.runs),
                    away.max_agents
                ),
                accent,
            ),
            Span::styled(" occupied    ", muted),
            Span::styled(count(AwayEntryState::Queued).to_string(), accent),
            Span::styled(" queued    ", muted),
            Span::styled(
                count(AwayEntryState::Finished).to_string(),
                Style::new().fg(theme::done()),
            ),
            Span::styled(" finished    ", muted),
            Span::styled(
                count(AwayEntryState::Attention).to_string(),
                Style::new().fg(theme::error()),
            ),
            Span::styled(" attention", muted),
        ]));
        lines.push(Line::raw(""));
    } else if away.occupied_slots(&snapshot.runs) > 0 {
        lines.push(Line::styled(
            format!(
                "{} existing workers already occupy slots.",
                away.occupied_slots(&snapshot.runs)
            ),
            muted,
        ));
    }
    if viewing_away && !ready {
        lines.push(Line::styled(
            "Requires Herdr + OpenCode. Select them before starting.",
            Style::new().fg(theme::error()),
        ));
    }
    if viewing_away && let Some(error) = &away.error {
        lines.push(Line::styled(error.clone(), Style::new().fg(theme::error())));
        lines.push(Line::raw(""));
    }
    if viewing_away {
        lines.push(Line::styled(
            format!("WORK QUEUE  /  {}", away.entries.len()),
            accent,
        ));
        if away.entries.is_empty() {
            lines.push(Line::styled(
                "No work queued yet",
                Style::new().fg(theme::text()).add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::styled(
                "Start Away to prioritize eligible issues automatically.",
                muted,
            ));
        }
        for (i, entry) in snapshot.away.entries.iter().enumerate() {
            let (label, color) = match entry.state {
                AwayEntryState::Queued => ("QUEUED", theme::muted()),
                AwayEntryState::Launching => ("STARTING", theme::primary()),
                AwayEntryState::Running => ("RUNNING", theme::primary()),
                AwayEntryState::Finished => ("FINISHED", theme::done()),
                AwayEntryState::Attention => ("ATTENTION", theme::error()),
                AwayEntryState::Skipped => ("SKIPPED", theme::secondary()),
            };
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(format!("{:>2}  ", i + 1), muted),
                Span::styled(
                    format!("{label:<10}"),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(entry.identifier.clone(), accent),
            ]));
            lines.push(Line::raw(format!("    {}", entry.title)));
            lines.push(Line::styled(format!("    {}", entry.reason), muted));
            if let Some(error) = &entry.error {
                lines.push(Line::styled(
                    format!("    {error}"),
                    Style::new().fg(theme::error()),
                ));
            }
        }
    }
    let popup = dialog_area(
        area,
        if away.entries.is_empty() {
            if active {
                26
            } else {
                24
            }
        } else {
            34
        },
    );
    frame.render_widget(Clear, popup);
    let border = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme::border()))
        .style(normal);
    frame.render_widget(border, popup);
    let inner = popup.inner(Margin::new(
        if popup.width >= 40 {
            3
        } else {
            1
        },
        1,
    ));
    let title = Rect::new(inner.x, inner.y, inner.width, inner.height.min(4));
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled("Operating mode", accent),
            Line::raw(""),
            Line::styled(
                format!(
                    "Active: {}{}",
                    if active {
                        "Away"
                    } else {
                        "Manual"
                    },
                    if active {
                        format!(" / {}", phase_label(away.phase))
                    } else {
                        String::new()
                    }
                ),
                muted,
            ),
        ]),
        title,
    );
    overlay.tabs.clear();
    if title.height >= 2 {
        let mut x = inner.x;
        for (mode, label) in [(AppMode::Manual, " Manual "), (AppMode::Away, " Away ")] {
            let width = (label.len() as u16).min(inner.right().saturating_sub(x));
            let rect = Rect::new(x, inner.y + 1, width, 1);
            if width > 0 {
                frame.render_widget(
                    Paragraph::new(label).style(if overlay.tab == mode {
                        selected
                    } else {
                        Style::new().fg(theme::text()).bg(theme::element())
                    }),
                    rect,
                );
                overlay.tabs.push((rect, mode));
            }
            x = x.saturating_add(width + 2);
        }
        if inner.right().saturating_sub(x) >= 20 {
            frame.render_widget(
                Paragraph::new("Tab / arrows to choose").style(muted),
                Rect::new(x, inner.y + 1, inner.right() - x, 1),
            );
        }
    }
    let footer_height = inner.height.saturating_sub(title.height).min(4);
    let body = Rect::new(
        inner.x,
        title.bottom(),
        inner.width,
        inner.height.saturating_sub(title.height + footer_height),
    );
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(normal);
    overlay.scroll_max = paragraph
        .line_count(body.width.max(1))
        .saturating_sub(body.height as usize)
        .min(u16::MAX as usize) as u16;
    overlay.scroll = overlay.scroll.min(overlay.scroll_max);
    frame.render_widget(paragraph.scroll((overlay.scroll, 0)), body);
    let primary = if pending {
        " Working... "
    } else if !viewing_away {
        if active {
            " Enter  Use Manual "
        } else {
            " Manual is active "
        }
    } else if !active {
        " Enter  Start Away "
    } else if matches!(away.phase, AwayPhase::Paused | AwayPhase::Attention) {
        " Enter  Resume Away "
    } else {
        " Enter  Pause Away "
    };
    let footer = Rect::new(inner.x, body.bottom(), inner.width, footer_height);
    let message = app.status_message.as_deref().unwrap_or(if viewing_away {
        "Keep launcher open. Finished agents exit; worktrees remain."
    } else {
        "Browsing tabs does not change the active mode."
    });
    let controls = if viewing_away && active {
        "l Apply limit   o Reprioritize   Esc Close"
    } else {
        "Tab Choose mode   Esc Close"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(message, muted),
            Line::from(vec![
                Span::styled(
                    primary,
                    if pending || (!active && (!viewing_away || !ready)) {
                        Style::new().fg(theme::muted()).bg(theme::element())
                    } else {
                        selected
                    },
                ),
                Span::styled(
                    if viewing_away {
                        "   Herdr + OpenCode"
                    } else {
                        ""
                    },
                    muted,
                ),
            ]),
            Line::styled(controls, muted),
            Line::styled(
                if overlay.scroll_max > 0 {
                    "Up/Down scroll settings and queue"
                } else {
                    ""
                },
                muted,
            ),
        ])
        .style(normal),
        footer,
    );
}

fn dialog_area(area: Rect, desired_height: u16) -> Rect {
    let width = if area.width >= 44 {
        area.width.saturating_sub(8).min(78)
    } else {
        area.width
    };
    let height = if area.height >= 16 {
        area.height.saturating_sub(4).min(desired_height)
    } else {
        area.height
    };
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn tabs_preview_modes_without_launching_and_preserve_away_draft() {
        let snapshot = RuntimeSnapshot {
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "opencode".into(),
            prompt_profiles: vec!["implementer".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        open(&mut app, &snapshot);
        assert_eq!(app.away_overlay.as_ref().unwrap().tab, AppMode::Manual);
        for c in ['3', 'p', 'r', 's', 'a', 'o', 'l'] {
            assert!(prepare_command(&mut app, key(c), &snapshot).is_none());
        }
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "5");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(prepare_command(&mut app, tab, &snapshot).is_none());
        prepare_command(&mut app, key('3'), &snapshot);
        prepare_command(&mut app, key('r'), &snapshot);
        prepare_command(&mut app, tab, &snapshot);
        assert_eq!(app.away_overlay.as_ref().unwrap().tab, AppMode::Manual);
        assert!(
            prepare_command(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &snapshot
            )
            .is_none()
        );
        prepare_command(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &snapshot,
        );
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "3");
        assert_eq!(
            app.away_overlay.as_ref().unwrap().ranking,
            AwayRanking::SourcePriority
        );
        assert!(matches!(
            prepare_command(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &snapshot
            ),
            Some(RuntimeCommand::StartAway {
                max_agents: 3,
                ranking: AwayRanking::SourcePriority,
                ..
            })
        ));
        app.away_pending = Some(42);
        assert!(prepare_command(&mut app, tab, &snapshot).is_none());
        assert_eq!(app.away_overlay.as_ref().unwrap().tab, AppMode::Away);
    }

    #[test]
    fn mode_tabs_are_clickable_sticky_and_show_distinct_attributes() {
        let snapshot = RuntimeSnapshot::default();
        let mut app = AppState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        open(&mut app, &snapshot);
        terminal
            .draw(|f| crate::render::draw(f, &snapshot, &mut app))
            .unwrap();
        let text = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(text(&terminal).contains("NO SETTINGS REQUIRED"));
        assert!(!text(&terminal).contains("Max agents"));
        let initial_tabs = app.away_overlay.as_ref().unwrap().tabs.clone();
        let rect = initial_tabs[1].0;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(!crate::mouse::handle_mouse(
            &mut app,
            click,
            &snapshot,
            (99, 30)
        ));
        assert!(crate::mouse::handle_mouse(
            &mut app,
            click,
            &snapshot,
            (100, 30)
        ));
        assert_eq!(app.away_overlay.as_ref().unwrap().tab, AppMode::Away);
        assert_eq!(snapshot.away.mode, AppMode::Manual);
        terminal
            .draw(|f| crate::render::draw(f, &snapshot, &mut app))
            .unwrap();
        assert!(text(&terminal).contains("Max agents"));
        assert!(text(&terminal).contains("Active: Manual"));
        assert!(!text(&terminal).contains("NO SETTINGS REQUIRED"));
        let tabs = app.away_overlay.as_ref().unwrap().tabs.clone();
        assert_eq!(
            tabs, initial_tabs,
            "switching tabs must not move the tab strip"
        );
        prepare_command(
            &mut app,
            KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            &snapshot,
        );
        terminal
            .draw(|f| crate::render::draw(f, &snapshot, &mut app))
            .unwrap();
        assert_eq!(app.away_overlay.as_ref().unwrap().tabs, tabs);
        let manual = tabs[0].0;
        assert!(crate::mouse::handle_mouse(
            &mut app,
            MouseEvent {
                column: manual.x,
                row: manual.y,
                ..click
            },
            &snapshot,
            (100, 30)
        ));
        assert_eq!(app.away_overlay.as_ref().unwrap().scroll, 0);
        assert!(
            !crate::mouse::handle_mouse(&mut app, click, &snapshot, (100, 30)),
            "old hitboxes must be invalidated until redraw"
        );
    }

    #[test]
    fn dialog_is_compact_and_closing_restores_every_color() {
        let snapshot = RuntimeSnapshot {
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "opencode".into(),
            ..Default::default()
        };
        for (width, height) in [(132, 42), (80, 24), (40, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut app = AppState::default();
            terminal
                .draw(|frame| crate::render::draw(frame, &snapshot, &mut app))
                .unwrap();
            let before = terminal.backend().buffer().clone();
            // The modal renderer is also safe to call unconditionally.
            terminal
                .draw(|frame| {
                    crate::render::draw(frame, &snapshot, &mut app);
                    draw(frame, frame.area(), &snapshot, &mut app);
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer(), &before);
            open(&mut app, &snapshot);
            terminal
                .draw(|frame| crate::render::draw(frame, &snapshot, &mut app))
                .unwrap();
            let popup = dialog_area(Rect::new(0, 0, width, height - 1), 24);
            let title_x = popup.x
                + if popup.width >= 40 {
                    3
                } else {
                    1
                };
            let title = &terminal.backend().buffer()[(title_x, popup.y + 1)];
            assert_eq!(title.fg, theme::primary());
            assert!(!title.modifier.contains(Modifier::DIM));
            if width >= 80 && height >= 24 {
                assert!(popup.y > 0);
                assert!(popup.width < width);
                assert!(popup.height < height);
            }
            prepare_command(
                &mut app,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &snapshot,
            );
            terminal
                .draw(|frame| crate::render::draw(frame, &snapshot, &mut app))
                .unwrap();
            assert_eq!(
                terminal.backend().buffer(),
                &before,
                "{width}x{height} colors after closing"
            );
        }
    }

    #[test]
    fn concurrency_replaces_default_and_supports_bounded_stepper() {
        let mut app = AppState::default();
        let snapshot = RuntimeSnapshot::default();
        open(&mut app, &snapshot);
        app.away_overlay.as_mut().unwrap().select_tab(AppMode::Away);
        prepare_command(&mut app, key('3'), &snapshot);
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "3");
        prepare_command(&mut app, key('+'), &snapshot);
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "4");
        prepare_command(&mut app, key('6'), &snapshot);
        prepare_command(&mut app, key('4'), &snapshot);
        prepare_command(&mut app, key('+'), &snapshot);
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "64");
        for _ in 0..70 {
            prepare_command(&mut app, key('-'), &snapshot);
        }
        assert_eq!(app.away_overlay.as_ref().unwrap().limit, "1");
    }

    #[test]
    fn defaults_validation_and_explicit_commands() {
        let mut snapshot = RuntimeSnapshot {
            selected_backend: Some(BackendKind::Herdr),
            selected_agent: "opencode".into(),
            prompt_profiles: vec!["implement".into()],
            ..Default::default()
        };
        let mut app = AppState {
            search_query: "existing search".into(),
            ..Default::default()
        };
        open(&mut app, &snapshot);
        app.away_overlay.as_mut().unwrap().select_tab(AppMode::Away);
        assert!(matches!(
            prepare_command(&mut app, key('s'), &snapshot),
            Some(RuntimeCommand::StartAway {
                max_agents: 5,
                profile: Some(ref p),
                ranking: AwayRanking::Agent
            }) if p == "implement"
        ));
        for limit in ["", "0", "65", "999"] {
            app.away_overlay.as_mut().unwrap().limit = limit.into();
            assert!(prepare_command(&mut app, key('s'), &snapshot).is_none());
        }
        for limit in [1, 64] {
            snapshot.away.mode = AppMode::Away;
            app.away_overlay.as_mut().unwrap().limit = limit.to_string();
            assert!(
                matches!(prepare_command(&mut app, key('l'), &snapshot), Some(RuntimeCommand::SetAwayConcurrency { max_agents }) if max_agents == limit)
            );
        }
        snapshot.away.mode = AppMode::Manual;
        assert!(prepare_command(&mut app, key('p'), &snapshot).is_none());
        assert!(app.away_overlay.as_ref().unwrap().profile.is_none());
        assert!(prepare_command(&mut app, key('p'), &snapshot).is_none());
        assert!(prepare_command(&mut app, key('r'), &snapshot).is_none());
        assert!(
            matches!(prepare_command(&mut app, key('s'), &snapshot), Some(RuntimeCommand::StartAway { profile: Some(p), ranking: AwayRanking::SourcePriority, .. }) if p == "implement")
        );
        assert!(prepare_command(&mut app, key('o'), &snapshot).is_none());
        snapshot.away.mode = AppMode::Away;
        assert!(matches!(
            prepare_command(&mut app, key('o'), &snapshot),
            Some(RuntimeCommand::ReprioritizeAway {
                ranking: AwayRanking::SourcePriority
            })
        ));
        app.away_pending = Some(1);
        assert!(prepare_command(&mut app, key('m'), &snapshot).is_none());
        prepare_command(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &snapshot,
        );
        assert!(app.away_overlay.is_none());
        assert_eq!(app.search_query, "existing search");
    }

    #[test]
    fn backend_gating_and_pause_resume() {
        let mut snapshot = RuntimeSnapshot::default();
        let mut app = AppState::default();
        open(&mut app, &snapshot);
        app.away_overlay.as_mut().unwrap().select_tab(AppMode::Away);
        assert!(prepare_command(&mut app, key('s'), &snapshot).is_none());
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("Herdr + OpenCode")
        );
        snapshot.away.mode = AppMode::Away;
        assert!(matches!(
            prepare_command(&mut app, key('a'), &snapshot),
            Some(RuntimeCommand::PauseAway)
        ));
        snapshot.away.phase = AwayPhase::Paused;
        assert!(matches!(
            prepare_command(&mut app, key('a'), &snapshot),
            Some(RuntimeCommand::ResumeAway)
        ));
        snapshot.away.phase = AwayPhase::Attention;
        assert!(matches!(
            prepare_command(&mut app, key('a'), &snapshot),
            Some(RuntimeCommand::ResumeAway)
        ));
        assert!(prepare_command(&mut app, key('m'), &snapshot).is_none());
        assert_eq!(app.away_overlay.as_ref().unwrap().tab, AppMode::Manual);
        assert!(matches!(
            prepare_command(
                &mut app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &snapshot
            ),
            Some(RuntimeCommand::SetManual)
        ));
    }

    #[test]
    fn new_mode_prefers_implementer_then_first_without_replacing_stored_profile() {
        let mut snapshot = RuntimeSnapshot {
            prompt_profiles: vec!["designer".into(), "implementer".into(), "reviewer".into()],
            ..Default::default()
        };
        let mut app = AppState::default();
        open(&mut app, &snapshot);
        assert_eq!(
            app.away_overlay.as_ref().unwrap().profile.as_deref(),
            Some("implementer")
        );
        snapshot.away.profile = Some("reviewer".into());
        open(&mut app, &snapshot);
        assert_eq!(
            app.away_overlay.as_ref().unwrap().profile.as_deref(),
            Some("reviewer")
        );
        snapshot.away.profile = None;
        snapshot.prompt_profiles.remove(1);
        open(&mut app, &snapshot);
        assert_eq!(
            app.away_overlay.as_ref().unwrap().profile.as_deref(),
            Some("designer")
        );
        snapshot.away.mode = AppMode::Away;
        open(&mut app, &snapshot);
        assert!(app.away_overlay.as_ref().unwrap().profile.is_none());
        snapshot.away.mode = AppMode::Manual;
        snapshot.prompt_profiles.clear();
        open(&mut app, &snapshot);
        assert!(app.away_overlay.as_ref().unwrap().profile.is_none());
    }

    #[test]
    fn mode_button_is_one_row_in_every_phase_and_does_not_change_list_geometry() {
        let mut snapshot = RuntimeSnapshot::default();
        let mut app = AppState::default();
        for width in [1, 12, 18, 24, 36, 60, 80, 99, 100, 120, 240] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            snapshot.away.mode = AppMode::Manual;
            terminal
                .draw(|f| crate::render::draw(f, &snapshot, &mut app))
                .unwrap();
            let list = app.mouse.list;
            for phase in [
                AwayPhase::Inactive,
                AwayPhase::Refreshing,
                AwayPhase::Prioritizing,
                AwayPhase::Running,
                AwayPhase::Paused,
                AwayPhase::Draining,
                AwayPhase::QueueEmpty,
                AwayPhase::Attention,
            ] {
                snapshot.away.mode = AppMode::Away;
                snapshot.away.phase = phase;
                snapshot.away.prioritizing = phase == AwayPhase::Prioritizing;
                terminal
                    .draw(|f| crate::render::draw(f, &snapshot, &mut app))
                    .unwrap();
                if width >= 2 {
                    assert_eq!(app.mouse.mode.height, 1);
                    assert!(app.mouse.mode.y > 0);
                } else {
                    assert!(app.mouse.mode.is_empty());
                }
                assert_eq!(app.mouse.list, list);
                let button = app.mouse.mode;
                let actual: String = (button.x..button.right())
                    .map(|x| terminal.backend().buffer()[(x, button.y)].symbol())
                    .collect();
                assert!(!actual.contains("QueueEmpty"));
                if width >= 60 {
                    assert!(actual.contains("Away"));
                }
            }
        }
    }

    #[test]
    fn readable_labels_and_controller_slot_remain_visible_in_manual_and_away() {
        let mut snapshot = RuntimeSnapshot::default();
        snapshot.away.prioritizing = true;
        snapshot.away.phase = AwayPhase::Draining;
        assert!(needs_quit_warning(&snapshot));
        snapshot.away.mode = AppMode::Away;
        snapshot.away.phase = AwayPhase::QueueEmpty;
        snapshot.away.ranking = AwayRanking::SourcePriority;
        assert_eq!(snapshot.away.occupied_slots(&snapshot.runs), 1);
        let mut app = AppState::default();
        open(&mut app, &snapshot);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| crate::render::draw(f, &snapshot, &mut app))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Source priority"));
        assert!(text.contains("built-in prompt"));
        assert!(!text.contains("SourcePriority"));
        assert!(!text.contains("configured default"));
    }

    #[test]
    fn small_screen_rendering_and_click_modal_isolation() {
        let snapshot = RuntimeSnapshot::default();
        for (width, height) in [(1, 1), (12, 4), (24, 8), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut app = AppState::default();
            terminal
                .draw(|f| crate::render::draw(f, &snapshot, &mut app))
                .unwrap();
            let click = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: app.mouse.mode.x,
                row: app.mouse.mode.y,
                modifiers: KeyModifiers::NONE,
            };
            if app.mouse.mode.is_empty() {
                open(&mut app, &snapshot);
            } else {
                assert!(crate::mouse::handle_mouse(
                    &mut app,
                    click,
                    &snapshot,
                    (width, height)
                ));
            }
            terminal
                .draw(|f| crate::render::draw(f, &snapshot, &mut app))
                .unwrap();
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, height)
            ));
            app.away_quit = true;
            terminal
                .draw(|f| crate::render::draw(f, &snapshot, &mut app))
                .unwrap();
            assert_eq!(app.away_quit_visible, width >= 80);
        }
    }

    #[test]
    fn owned_reservations_require_quit_warning() {
        let mut snapshot = RuntimeSnapshot::default();
        assert!(!needs_quit_warning(&snapshot));
        snapshot.away.mode = AppMode::Away;
        snapshot.away.entries.push(agent_launcher_core::AwayEntry {
            issue: agent_launcher_core::IssueKey {
                provider: agent_launcher_core::IssueProvider::Github,
                host: "github.com".into(),
                repository: "a/b".into(),
                native_id: "1".into(),
            },
            identifier: "#1".into(),
            title: "Fix".into(),
            reason: "Urgent".into(),
            state: AwayEntryState::Launching,
            run_id: Some("owned".into()),
            error: None,
        });
        assert!(needs_quit_warning(&snapshot));
        assert_eq!(snapshot.away.occupied_slots(&snapshot.runs), 1);
        snapshot.away.entries[0].state = AwayEntryState::Finished;
        assert!(!needs_quit_warning(&snapshot));
        assert_eq!(snapshot.away.occupied_slots(&snapshot.runs), 0);
    }
}
