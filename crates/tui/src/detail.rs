use agent_launcher_core::{
    EventEnvelope, Issue, RunEvent, RunSummary, RuntimeSnapshot, WorktreeDeleteAction,
};
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    app::AppState,
    format::{one_line, timestamp_label, truncate},
    status::{run_color, run_label},
    theme,
    widgets::{LeftBorderPanel, render_bottom_edge},
};

pub(crate) fn draw_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &mut AppState,
) {
    let Some(issue) = app.detail_issue(snapshot) else {
        frame.render_widget(
            Paragraph::new("The selected issue is no longer available. Press Esc to return.")
                .style(Style::new().fg(theme::muted()))
                .alignment(Alignment::Center),
            area,
        );
        return;
    };

    let margin = if area.width >= 56 {
        2
    } else {
        1
    };
    let available = area.inner(Margin::new(margin, usize::from(area.height >= 12) as u16));
    let content_width = app.layout.content_width(available.width);
    let content = Rect::new(
        available.x + available.width.saturating_sub(content_width) / 2,
        available.y,
        content_width,
        available.height,
    );
    frame.render_widget(Block::new().style(Style::new().bg(theme::panel())), content);
    if content.height < 4 || content.width < 18 {
        draw_tiny_detail(frame, content, issue, app.latest_run(snapshot, issue));
        if app.input_overlay.is_some() {
            draw_input_overlay(frame, area, app);
        }
        if app.delete_overlay.is_some() {
            draw_delete_overlay(frame, area, app);
        }
        return;
    }

    let status = app.visible_status(snapshot);
    let controls_height = if content.width >= 104 && issue.security_advisory.is_none() {
        1
    } else {
        2
    } + u16::from(status.is_some() && content.height >= 6);
    let header_height = if content.height >= 12 {
        3
    } else {
        2
    };
    let body_height = content
        .height
        .saturating_sub(header_height)
        .saturating_sub(controls_height);
    let header = Rect::new(content.x, content.y, content.width, header_height);
    let body = Rect::new(
        content.x,
        content.y.saturating_add(header_height),
        content.width,
        body_height,
    );
    let controls = Rect::new(
        content.x,
        body.y.saturating_add(body.height),
        content.width,
        controls_height,
    );
    let latest_run = app.latest_run(snapshot, issue);

    draw_detail_header(frame, header, issue, latest_run);
    draw_detail_body(frame, body, snapshot, issue, latest_run, app);
    draw_controls(
        frame,
        controls,
        status.as_deref(),
        issue.pull_request.is_some(),
        issue.security_advisory.is_some(),
    );
    if app.input_overlay.is_some() {
        draw_input_overlay(frame, area, app);
    }
    if app.delete_overlay.is_some() {
        draw_delete_overlay(frame, area, app);
    }
}

fn draw_tiny_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
) {
    let mut lines = vec![Line::styled(
        truncate(
            &format!("{} {}", issue.identifier, issue.title),
            area.width as usize,
        ),
        Style::new().fg(theme::primary()).bold(),
    )];
    if let Some(run) = latest_run {
        lines.push(Line::styled(
            run_label(run.state),
            Style::new().fg(run_color(run.state)).bold(),
        ));
    }
    let controls_height = area.height.min(2);
    let body = Rect::new(area.x, area.y, area.width, area.height - controls_height);
    frame.render_widget(Paragraph::new(lines), body);
    draw_controls(
        frame,
        Rect::new(area.x, body.bottom(), area.width, controls_height),
        None,
        issue.pull_request.is_some(),
        issue.security_advisory.is_some(),
    );
}

fn draw_detail_header(
    frame: &mut Frame<'_>,
    area: Rect,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
) {
    frame.render_widget(Block::new().style(Style::new().bg(theme::element())), area);
    let inner = Block::new().padding(Padding::new(2, 1, 0, 0)).inner(area);
    if inner.is_empty() {
        return;
    }
    let title = truncate(
        &format!("{}  {}", issue.identifier, issue.title),
        inner.width as usize,
    );
    let mut lines = vec![Line::styled(
        title,
        Style::new()
            .fg(theme::primary())
            .add_modifier(Modifier::BOLD),
    )];
    if area.height > 1 {
        let source = format!(
            "{}{} · {} · {}",
            if issue.security_advisory.is_some() {
                "PRIVATE · "
            } else {
                ""
            },
            issue.key.provider,
            issue.key.repository,
            issue.state
        );
        lines.push(Line::styled(source, Style::new().fg(theme::muted())));
    }
    if area.height > 2
        && let Some(run) = latest_run
    {
        let attention = if run.state.needs_attention() {
            "  ATTENTION"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled("latest run  ", Style::new().fg(theme::muted())),
            Span::styled(
                format!("{}{}", run_label(run.state), attention),
                Style::new().fg(run_color(run.state)).bold(),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_detail_body(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
    app: &mut AppState,
) {
    if area.is_empty() {
        return;
    }
    let padding = u16::from(area.height >= 3);
    let inner = Block::new()
        .padding(Padding::new(2, 1, padding, padding))
        .inner(area);
    if inner.is_empty() {
        return;
    }
    let lines = detail_lines(
        snapshot,
        issue,
        latest_run,
        inner.width,
        &mut app.markdown_cache,
    );
    app.mouse.detail = inner;
    // Markdown already fits this width, including table borders. Only metadata/output
    // still need Paragraph wrapping; its line count covers both paths for scrolling.
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .style(Style::new().fg(theme::text()).bg(theme::panel()));
    app.detail_scroll_max = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.detail_scroll = app.detail_scroll.min(app.detail_scroll_max);

    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), inner);
}

fn detail_lines(
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
    width: u16,
    markdown_cache: &mut crate::widgets::markdown::MarkdownCache,
) -> Vec<Line<'static>> {
    let mut lines = vec![section(if issue.security_advisory.is_some() {
        "PRIVATE Security Advisory"
    } else if issue.pull_request.is_some() {
        "Pull Request"
    } else {
        "Issue"
    })];
    if let Some(advisory) = &issue.security_advisory {
        lines.push(key_value("GHSA", &advisory.ghsa_id));
        lines.push(key_value(
            "CVE",
            advisory.cve_id.as_deref().unwrap_or("not assigned"),
        ));
        lines.push(key_value(
            "severity",
            advisory.severity.as_deref().unwrap_or("unknown"),
        ));
        lines.push(key_value(
            "privacy",
            "PRIVATE - local UI only; explicit consent required to dispatch",
        ));
        lines.push(key_value(
            "cleanup",
            "Private clone retained; separate cleanup unavailable",
        ));
        if issue.state != "draft" {
            lines.push(key_value(
                "dispatch",
                "Read-only: accept triage reports on GitHub manually; only drafts can launch",
            ));
        }
    }
    if let Some(pr) = &issue.pull_request {
        lines.push(key_value("number", &format!("#{}", pr.number)));
        lines.push(key_value(
            "base",
            &format!("{} ({})", pr.base_ref, pr.base_sha),
        ));
        lines.push(key_value(
            "head",
            &format!("{} ({})", pr.head_ref, pr.head_sha),
        ));
        lines.push(key_value(
            "head repo",
            pr.head_repository.as_deref().unwrap_or("unknown"),
        ));
        lines.push(key_value(
            "changes",
            &format!(
                "+{} -{}",
                pr.additions
                    .map_or_else(|| "?".to_owned(), |n| n.to_string()),
                pr.deletions
                    .map_or_else(|| "?".to_owned(), |n| n.to_string())
            ),
        ));
        lines.push(Line::styled(
            "Press d to review PR. Opening details does not start a review.",
            Style::new().fg(theme::muted()),
        ));
    }
    lines.push(key_value("id", &issue.key.canonical()));
    lines.push(key_value("source", &issue.key.provider.to_string()));
    lines.push(key_value("repository", &issue.key.repository));
    lines.push(key_value("state", &issue.state));
    let activity = issue.activity.unwrap_or_default();
    let mut counts = vec![("comments", activity.comments)];
    if issue.pull_request.is_some() {
        counts.extend([
            ("review comments", activity.review_comments),
            ("commits", activity.commits),
        ]);
    }
    for (label, count) in counts {
        lines.push(key_value(
            label,
            &count.map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
        ));
    }
    if let Some(priority) = issue.priority {
        lines.push(key_value("priority", &format!("P{priority}")));
    }
    if let Some(author) = issue.author.as_deref() {
        lines.push(key_value("author", author));
    }
    if !issue.labels.is_empty() {
        lines.push(key_value("labels", &issue.labels.join(", ")));
    }
    if !issue.blocked_by.is_empty() {
        lines.push(key_value("blocked by", &issue.blocked_by.join(", ")));
    }
    if let Some(url) = issue.url.as_deref() {
        lines.push(key_value("url", url));
    }

    lines.push(Line::raw(""));
    lines.push(section("Description"));
    match issue
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        Some(description) => lines.extend(markdown_cache.render(description, width).lines),
        None => {
            *markdown_cache = Default::default();
            lines.push(Line::styled(
                "No description.",
                Style::new().fg(theme::muted()),
            ));
        },
    }

    lines.push(Line::raw(""));
    lines.push(section("Latest run"));
    if let Some(run) = latest_run {
        if run.confidential {
            lines.push(key_value(
                "privacy",
                "PRIVATE run; launcher output is not persisted",
            ));
        }
        lines.push(key_value_styled(
            "state",
            run_label(run.state),
            run_color(run.state),
        ));
        lines.push(key_value("run", &run.id));
        lines.push(key_value("agent", &run.agent));
        lines.push(key_value("started", &timestamp_label(run.started_at)));
        lines.push(key_value("updated", &timestamp_label(run.updated_at)));
        if let Some(session_id) = run.session_id.as_deref() {
            lines.push(key_value("session", session_id));
        }
        if let Some(message) = run.message.as_deref() {
            lines.push(key_value_styled(
                "message",
                &one_line(message),
                if run.state.needs_attention() {
                    theme::error()
                } else {
                    theme::text()
                },
            ));
        }
        if let Some(workspace) = &run.workspace {
            lines.push(key_value("workspace", &workspace.id));
            lines.push(key_value("backend", &workspace.backend.to_string()));
            lines.push(key_value("branch", &workspace.branch));
            if let Some(host) = workspace.host.as_deref() {
                lines.push(key_value("host", host));
            }
            if let Some(path) = &workspace.path {
                lines.push(key_value("path", &path.display().to_string()));
            }
        }

        lines.push(Line::raw(""));
        if run.confidential || issue.security_advisory.is_some() {
            lines.push(Line::raw("Private run output is not persisted by launcher. Open the harness to review its session; it may retain its own history."));
            return lines;
        }
        lines.push(section("Recent persisted events / output"));
        let events = snapshot
            .run_events
            .get(&run.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if events.is_empty() {
            lines.push(Line::styled(
                "No persisted events for this run yet.",
                Style::new().fg(theme::muted()),
            ));
        } else {
            for event in events.iter().rev().take(24).rev() {
                lines.extend(event_lines(event));
            }
        }
    } else {
        lines.push(Line::styled(
            if issue.security_advisory.is_some() {
                "Not reviewed. Press d for settings, then explicit privacy consent."
            } else if issue.pull_request.is_some() {
                "Not reviewed. Press d to start a PR review."
            } else {
                "Not dispatched. Press d to start an agent."
            },
            Style::new().fg(theme::muted()),
        ));
    }
    lines
}

fn event_lines(event: &EventEnvelope) -> Vec<Line<'static>> {
    let time = event.timestamp.format("%H:%M:%S").to_string();
    let prefix = format!("{time}  ");
    match &event.payload {
        RunEvent::Output { stream, text } => {
            let mut lines = Vec::new();
            for (index, output) in text
                .lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .enumerate()
            {
                lines.push(Line::from(vec![
                    Span::styled(
                        if index == 0 {
                            format!("{prefix}{stream:?}  ")
                        } else {
                            "            ".to_owned()
                        },
                        Style::new().fg(theme::secondary()),
                    ),
                    Span::styled(output.to_owned(), Style::new().fg(theme::text())),
                ]));
            }
            if lines.is_empty() {
                lines.push(event_line(prefix, "output", "(empty)", theme::muted()));
            }
            lines
        },
        RunEvent::StateChanged { state, message } => vec![event_line(
            prefix,
            "state",
            &format!(
                "{}{}",
                run_label(*state),
                message
                    .as_deref()
                    .map(|message| format!(" · {}", one_line(message)))
                    .unwrap_or_default()
            ),
            run_color(*state),
        )],
        RunEvent::AssistantMessage { text } => {
            vec![event_line(prefix, "agent", &one_line(text), theme::text())]
        },
        RunEvent::ToolStarted { name } => {
            vec![event_line(
                prefix,
                "tool",
                &format!("{name} started"),
                theme::primary(),
            )]
        },
        RunEvent::ToolFinished { name, success } => vec![event_line(
            prefix,
            "tool",
            &format!(
                "{name} {}",
                if *success {
                    "finished"
                } else {
                    "failed"
                }
            ),
            if *success {
                theme::done()
            } else {
                theme::error()
            },
        )],
        RunEvent::InputRequested { prompt } => vec![event_line(
            prefix,
            "input",
            &one_line(prompt),
            theme::error(),
        )],
        RunEvent::PermissionRequested { id, description } => vec![event_line(
            prefix,
            "permission",
            &format!("{id} · {}", one_line(description)),
            theme::error(),
        )],
        RunEvent::Completed { success, message } => vec![event_line(
            prefix,
            "complete",
            message.as_deref().unwrap_or(if *success {
                "success"
            } else {
                "failed"
            }),
            if *success {
                theme::done()
            } else {
                theme::error()
            },
        )],
    }
}

fn event_line(
    prefix: String,
    kind: &'static str,
    value: &str,
    color: ratatui::style::Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(prefix, Style::new().fg(theme::secondary())),
        Span::styled(format!("{kind:<10}"), Style::new().fg(color)),
        Span::styled(value.to_owned(), Style::new().fg(theme::text())),
    ])
}

fn section(label: &'static str) -> Line<'static> {
    Line::styled(label, Style::new().fg(theme::primary()).bold())
}

fn key_value(label: &'static str, value: &str) -> Line<'static> {
    key_value_styled(label, value, theme::text())
}

fn key_value_styled(
    label: &'static str,
    value: &str,
    color: ratatui::style::Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::new().fg(theme::muted())),
        Span::styled(value.to_owned(), Style::new().fg(color)),
    ])
}

fn draw_controls(
    frame: &mut Frame<'_>,
    area: Rect,
    status: Option<&str>,
    review: bool,
    security: bool,
) {
    if area.is_empty() {
        return;
    }
    let controls = if security {
        if area.width < 38 {
            vec![control_line(&[("d", " private review"), (" Esc", " back")])]
        } else {
            vec![
                control_line(&[("d", " private review  "), ("o", " open  "), ("s", " stop")]),
                control_line(&[("i", " input  "), ("Esc", " back (private clone retained)")]),
            ]
        }
    } else if area.height == 1 && area.width < 38 {
        vec![control_line(&[
            ("Esc", " back"),
            (
                " d",
                if review {
                    " review"
                } else {
                    " dispatch"
                },
            ),
        ])]
    } else if area.width < 38 {
        vec![
            control_line(&[(
                "d",
                if review {
                    " review PR Esc"
                } else {
                    " dispatch Esc"
                },
            )]),
            control_line(&[("x", " worktree  "), ("X", " issue")]),
        ]
    } else if area.width >= 104 {
        vec![control_line(&[
            (
                "d",
                if review {
                    " review PR  "
                } else {
                    " dispatch   "
                },
            ),
            ("o", " open   "),
            ("s", " stop   "),
            ("i", " send input   "),
            ("x", " worktree   "),
            ("X", " issue   "),
            ("↑/↓", " scroll   "),
            ("Esc", " back"),
        ])]
    } else {
        vec![
            control_line(&[
                (
                    "d",
                    if review {
                        " review PR "
                    } else {
                        " dispatch  "
                    },
                ),
                ("o", " open  "),
                ("s", " stop  "),
                (
                    "i",
                    if area.width >= 40 {
                        " send input"
                    } else {
                        " input"
                    },
                ),
            ]),
            control_line(&[("x", " worktree  "), ("X", " issue  "), ("Esc", " back")]),
        ]
    };
    let mut controls = controls;
    if let Some(status) = status {
        if usize::from(area.height) > controls.len() {
            controls.push(Line::styled(
                status.to_owned(),
                Style::new().fg(theme::muted()),
            ));
        } else if let Some(last) = controls.last_mut() {
            last.spans.push(Span::styled(
                format!("  ·  {status}"),
                Style::new().fg(theme::muted()),
            ));
        }
    }
    frame.render_widget(Paragraph::new(controls).alignment(Alignment::Center), area);
}

fn control_line(controls: &[(&'static str, &'static str)]) -> Line<'static> {
    Line::from(
        controls
            .iter()
            .flat_map(|(key, description)| {
                [
                    Span::styled(*key, Style::new().fg(theme::text()).bold()),
                    Span::styled(*description, Style::new().fg(theme::muted())),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

fn draw_input_overlay(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let Some(overlay) = app.input_overlay.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(2).min(76);
    let height = area.height.saturating_sub(2).min(8);
    if width == 0 || height == 0 {
        return;
    }
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    if popup.width < 24 || popup.height < 6 {
        let text = if popup.height <= 1 {
            "Esc cancel".to_owned()
        } else {
            format!(
                "{}\nEsc cancel · Enter send",
                truncate(&overlay.text, popup.width as usize)
            )
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::new().fg(theme::text()).bg(theme::element()))
                .alignment(Alignment::Center),
            popup,
        );
        return;
    }
    let inner = LeftBorderPanel::new()
        .border_color(theme::primary())
        .content_bg(theme::element())
        .padding(Padding::new(1, 1, 1, 0))
        .render(popup, frame.buffer_mut());
    if inner.is_empty() {
        return;
    }
    let cursor = if (app.tick / 6).is_multiple_of(2) {
        "█"
    } else {
        " "
    };
    let prompt_width = inner.width as usize;
    let mut lines = vec![
        Line::styled("Send input", Style::new().fg(theme::primary()).bold()),
        Line::styled(
            truncate(&overlay.prompt, prompt_width),
            Style::new().fg(theme::muted()),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                truncate(&overlay.text, inner.width.saturating_sub(1) as usize),
                Style::new().fg(theme::text()),
            ),
            Span::styled(cursor, Style::new().fg(theme::text())),
        ]),
    ];
    if inner.height >= 6 {
        lines.push(Line::raw(""));
        lines.push(control_line(&[("Enter", " send   "), ("Esc", " cancel")]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::new().bg(theme::element())),
        inner,
    );
    render_bottom_edge(
        popup,
        theme::primary(),
        theme::element(),
        theme::bg(),
        frame.buffer_mut(),
    );
}

pub(crate) fn draw_issue_delete_overlay(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    app.issue_delete_confirmation_visible = false;
    let Some(overlay) = app.issue_delete_overlay.as_ref() else {
        return;
    };
    let width = area.width.saturating_sub(2).min(100);
    let height = area.height.saturating_sub(2);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::new().style(Style::new().bg(theme::element())), popup);
    let inner = popup.inner(Margin::new(1, 1));
    // Never truncate identity or warnings: confirmation is enabled only when all lines fit.
    let lines = vec![
        Line::styled(
            "Permanently delete source issue?",
            Style::new().fg(theme::error()).bold(),
        ),
        key_value("identifier", &format!("{:?}", overlay.identifier)),
        key_value("title", &format!("{:?}", overlay.title)),
        key_value("provider", overlay.issue_key.provider.as_str()),
        key_value("host", &format!("{:?}", overlay.issue_key.host)),
        key_value("repository", &format!("{:?}", overlay.issue_key.repository)),
        key_value("exact key", &format!("{:?}", overlay.issue_key.canonical())),
        Line::raw(""),
        Line::styled(
            "WARNING: Permanent source deletion. This cannot be undone.",
            Style::new().fg(theme::error()).bold(),
        ),
        Line::styled(
            "Removes dependency links, updates references, and orphans dependents.",
            Style::new().fg(theme::error()),
        ),
        Line::raw("Worktrees and run history are NOT deleted."),
        Line::raw("Active or resumable runs block deletion; resolve those runs first."),
        Line::raw(""),
        Line::raw(if overlay.pending {
            "Deleting source issue... Please wait."
        } else {
            "Enter permanently delete issue | Esc cancel"
        }),
    ];
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(Style::new().fg(theme::text()).bg(theme::element()));
    if inner.width >= 20 && paragraph.line_count(inner.width) <= usize::from(inner.height) {
        frame.render_widget(paragraph, inner);
        app.issue_delete_confirmation_visible = !overlay.pending;
    } else {
        frame.render_widget(
            Paragraph::new(if overlay.pending {
                "Deleting source issue... Please wait."
            } else {
                "Resize to review full target and permanent deletion warnings. Enter disabled. Esc cancel."
            })
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(theme::error()).bg(theme::element())),
            popup,
        );
    }
}

fn draw_delete_overlay(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    let width = area.width.saturating_sub(2).min(80);
    let height = area.height.saturating_sub(2).min(14);
    app.delete_confirmation_visible = width >= 28 && height >= 7;
    let Some(overlay) = app.delete_overlay.as_ref() else {
        return;
    };
    if width == 0 || height == 0 {
        return;
    }
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let action = match overlay.preview.action {
        WorktreeDeleteAction::Delete => "Delete worktree",
        WorktreeDeleteAction::Archive => "Archive workspace",
    };
    if !app.delete_confirmation_visible {
        frame.render_widget(
            Paragraph::new(format!("{action}?\nResize to review warnings.\nEsc cancel"))
                .style(Style::new().fg(theme::error()).bg(theme::element()))
                .alignment(Alignment::Center),
            popup,
        );
        return;
    }

    let inner = LeftBorderPanel::new()
        .border_color(theme::error())
        .content_bg(theme::element())
        .padding(Padding::new(1, 1, 1, 0))
        .render(popup, frame.buffer_mut());
    let preview = &overlay.preview;
    let workspace = preview.run.workspace.as_ref();
    let target = workspace
        .and_then(|workspace| workspace.path.as_ref())
        .map_or_else(
            || workspace.map_or_else(|| preview.run.id.clone(), |workspace| workspace.id.clone()),
            |path| path.display().to_string(),
        );
    let mut lines = vec![
        Line::styled(action, Style::new().fg(theme::error()).bold()),
        Line::styled(
            truncate(&target, inner.width as usize),
            Style::new().fg(theme::text()),
        ),
        Line::raw(""),
    ];
    if preview.has_uncommitted_changes {
        lines.push(Line::styled(
            "WARNING: uncommitted or untracked changes will be lost.",
            Style::new().fg(theme::error()).bold(),
        ));
    }
    if preview.has_ignored_files {
        lines.push(Line::styled(
            "WARNING: ignored files in this worktree will be lost.",
            Style::new().fg(theme::error()).bold(),
        ));
    }
    if preview.unpushed_commits > 0 {
        lines.push(Line::styled(
            format!(
                "WARNING: {} commit{} not found on any remote will remain only on the retained branch.",
                preview.unpushed_commits,
                if preview.unpushed_commits == 1 { "" } else { "s" }
            ),
            Style::new().fg(theme::error()).bold(),
        ));
    }
    if let Some(warning) = preview.inspection_warning.as_deref() {
        lines.push(Line::styled(
            warning.to_owned(),
            Style::new().fg(theme::error()),
        ));
    }
    if !preview.has_uncommitted_changes
        && !preview.has_ignored_files
        && preview.unpushed_commits == 0
        && preview.inspection_warning.is_none()
    {
        lines.push(Line::styled(
            "No uncommitted changes or unpushed commits were detected.",
            Style::new().fg(theme::done()),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        match preview.action {
            WorktreeDeleteAction::Delete => {
                "The branch is retained. The worktree and persisted run history are removed."
            },
            WorktreeDeleteAction::Archive => {
                "Conductor archives the workspace. Persisted run history is removed."
            },
        },
        Style::new().fg(theme::muted()),
    ));
    lines.push(Line::raw(""));
    lines.push(control_line(&[
        ("Enter", " confirm   "),
        ("Esc", " cancel"),
    ]));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::new().bg(theme::element())),
        inner,
    );
    render_bottom_edge(
        popup,
        theme::error(),
        theme::element(),
        theme::bg(),
        frame.buffer_mut(),
    );
}
