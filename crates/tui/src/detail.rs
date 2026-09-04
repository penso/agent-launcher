use agent_launcher_core::{
    EventEnvelope, Issue, RunEvent, RunSummary, RuntimeSnapshot, WorktreeDeleteAction,
};
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Clear, Padding, Paragraph, Wrap},
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
    let content_width = available.width.min(104);
    let content = Rect::new(
        available.x + available.width.saturating_sub(content_width) / 2,
        available.y,
        content_width,
        available.height,
    );
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

    let controls_height = if content.width >= 82 {
        1
    } else {
        2
    };
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
        snapshot.error.as_deref().or(app.status_message.as_deref()),
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
    let mut lines = vec![
        Line::from(vec![
            Span::styled("agent ", Style::new().fg(theme::muted()).bold()),
            Span::styled("launcher", Style::new().fg(theme::text()).bold()),
        ]),
        Line::styled(
            truncate(
                &format!("{} {}", issue.identifier, issue.title),
                area.width as usize,
            ),
            Style::new().fg(theme::text()),
        ),
    ];
    if let Some(run) = latest_run {
        lines.push(Line::styled(
            run_label(run.state),
            Style::new().fg(run_color(run.state)).bold(),
        ));
    }
    lines.push(Line::styled(
        "Esc back · d dispatch · i input · x delete",
        Style::new().fg(theme::muted()),
    ));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_detail_header(
    frame: &mut Frame<'_>,
    area: Rect,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
) {
    let accent = latest_run
        .filter(|run| run.state.needs_attention())
        .map_or(theme::primary(), |_| theme::error());
    let inner = LeftBorderPanel::new()
        .border_color(accent)
        .content_bg(theme::panel())
        .padding(Padding::new(1, 1, 0, 0))
        .render(area, frame.buffer_mut());
    if inner.is_empty() {
        return;
    }
    let title = truncate(
        &format!("{}  {}", issue.identifier, issue.title),
        inner.width as usize,
    );
    let mut lines = vec![Line::styled(
        title,
        Style::new().fg(theme::text()).add_modifier(Modifier::BOLD),
    )];
    if area.height > 1 {
        let source = format!(
            "{} · {} · {}",
            issue.key.provider, issue.key.repository, issue.state
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
    let accent = latest_run
        .filter(|run| run.state.needs_attention())
        .map_or(theme::primary(), |_| theme::error());
    let inner = LeftBorderPanel::new()
        .border_color(accent)
        .content_bg(theme::element())
        .padding(Padding::new(1, 1, 1, 1))
        .render(area, frame.buffer_mut());
    if inner.is_empty() {
        return;
    }
    let lines = detail_lines(snapshot, issue, latest_run);
    let visible_height = inner.height;
    let inner_width = inner.width.max(1) as usize;
    app.detail_scroll_max = visual_line_count(&lines, inner_width)
        .saturating_sub(visible_height as usize)
        .min(u16::MAX as usize) as u16;
    app.detail_scroll = app.detail_scroll.min(app.detail_scroll_max);

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((app.detail_scroll, 0))
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(theme::text()).bg(theme::element())),
        inner,
    );
}

fn detail_lines(
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
) -> Vec<Line<'static>> {
    let mut lines = vec![section("Issue")];
    lines.push(key_value("id", &issue.key.canonical()));
    lines.push(key_value("source", &issue.key.provider.to_string()));
    lines.push(key_value("repository", &issue.key.repository));
    lines.push(key_value("state", &issue.state));
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
        Some(description) => lines.extend(
            description
                .lines()
                .map(|line| Line::styled(line.to_owned(), Style::new().fg(theme::text()))),
        ),
        None => lines.push(Line::styled(
            "No description.",
            Style::new().fg(theme::muted()),
        )),
    }

    lines.push(Line::raw(""));
    lines.push(section("Latest run"));
    if let Some(run) = latest_run {
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
            "Not dispatched. Press d to start an agent.",
            Style::new().fg(theme::muted()),
        ));
    }
    lines
}

fn visual_line_count(lines: &[Line<'_>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| {
            let characters = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            characters.max(1).div_ceil(width)
        })
        .sum()
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

fn draw_controls(frame: &mut Frame<'_>, area: Rect, status: Option<&str>) {
    if area.is_empty() {
        return;
    }
    let controls = if area.width >= 82 {
        vec![control_line(&[
            ("d", " dispatch   "),
            ("o", " open   "),
            ("s", " stop   "),
            ("i", " send input   "),
            ("x", " delete   "),
            ("↑/↓", " scroll   "),
            ("Esc", " back"),
        ])]
    } else {
        vec![
            control_line(&[
                ("d", " dispatch  "),
                ("o", " open  "),
                ("s", " stop  "),
                ("i", " input"),
            ]),
            control_line(&[("x", " delete  "), ("↑/↓", " scroll  "), ("Esc", " back")]),
        ]
    };
    let mut controls = controls;
    if let Some(status) = status
        && let Some(last) = controls.last_mut()
    {
        last.spans.push(Span::styled(
            format!("  ·  {status}"),
            Style::new().fg(theme::muted()),
        ));
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
