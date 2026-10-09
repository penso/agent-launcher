use agent_launcher_core::{
    EventEnvelope, Issue, RunEvent, RunSummary, RuntimeSnapshot, WorktreeDeleteAction,
};
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};

use crate::{
    app::{AppState, DetailTab},
    format::{age_label, one_line, timestamp_label, truncate},
    status::{agent_glyph, issue_color, priority_color, run_color, run_label},
    theme,
    widgets::{LeftBorderPanel, render_bottom_edge},
};

/// Descriptions wrap at this width even in wide windows, so lines stay readable.
const READING_WIDTH: u16 = 100;
/// Rendered description lines the Overview shows before pointing to its tab.
const OVERVIEW_DESCRIPTION_LINES: usize = 12;

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
        return;
    }

    let status = app.visible_status(snapshot);
    let controls_height = if content.width >= 104 && issue.security_advisory.is_none() {
        1
    } else {
        2
    } + u16::from(status.is_some() && content.height >= 6);
    // The tab strip leads, padded with a row above and below in its own
    // colour; then the title alone, spaced from the fields like the preview.
    let tabs_height = if content.height >= 16 {
        3
    } else {
        u16::from(content.height >= 8)
    };
    let title_gap = u16::from(content.height >= 12);
    let header_height = 1;
    let body_height = content
        .height
        .saturating_sub(tabs_height + title_gap + header_height)
        .saturating_sub(controls_height);
    let tabs = Rect::new(content.x, content.y, content.width, tabs_height);
    let header = Rect::new(
        content.x,
        tabs.bottom() + title_gap,
        content.width,
        header_height,
    );
    let body = Rect::new(content.x, header.bottom(), content.width, body_height);
    let controls = Rect::new(
        content.x,
        body.y.saturating_add(body.height),
        content.width,
        controls_height,
    );
    let latest_run = app.latest_run(snapshot, issue);

    draw_detail_header(frame, header, issue);
    draw_tabs(frame, tabs, app.detail_tab, latest_run);
    draw_detail_body(frame, body, snapshot, issue, latest_run, app);
    draw_controls(
        frame,
        controls,
        status.as_deref(),
        issue.pull_request.is_some(),
        issue.security_advisory.is_some(),
    );
}

/// The selected row's Overview beside the list, when the window is wide
/// enough to show both. Enter opens the full detail view.
pub(crate) fn draw_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &mut AppState,
) {
    frame.render_widget(Block::new().style(Style::new().bg(theme::panel())), area);
    let inner = Block::new().padding(Padding::new(2, 2, 1, 1)).inner(area);
    if inner.is_empty() {
        return;
    }
    let Some(issue) = app.selected_issue(snapshot) else {
        frame.render_widget(
            Paragraph::new("Select an item to preview it here.")
                .style(Style::new().fg(theme::muted())),
            inner,
        );
        return;
    };
    let latest_run = app.latest_run(snapshot, issue);
    // The title alone: the list beside it already shows the state.
    let title = Paragraph::new(Line::styled(
        issue.title.clone(),
        Style::new().fg(theme::text()).add_modifier(Modifier::BOLD),
    ))
    .wrap(Wrap { trim: true });
    let title_height = (title.line_count(inner.width) as u16).min(inner.height);
    frame.render_widget(title, Rect {
        height: title_height,
        ..inner
    });
    let inner = Rect {
        y: inner.y + title_height,
        height: inner.height - title_height,
        ..inner
    };
    // Title only: the ID and everything else are fields below.
    let mut lines = vec![Line::raw(""), field("ID", &issue.identifier)];
    // One cell narrower than the pane, so wrapped markdown never re-wraps.
    lines.extend(overview_lines(
        snapshot,
        issue,
        latest_run,
        inner.width.min(READING_WIDTH).saturating_sub(1),
        &mut app.markdown_cache,
        "Enter",
    ));
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(Style::new().fg(theme::text()));
    let clipped = paragraph.line_count(inner.width) > usize::from(inner.height);
    if clipped && inner.height > 2 {
        let body = Rect {
            height: inner.height - 1,
            ..inner
        };
        frame.render_widget(paragraph, body);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Enter", Style::new().fg(theme::primary()).bold()),
                Span::styled(" to read all", Style::new().fg(theme::muted())),
            ]))
            .alignment(Alignment::Right),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    } else {
        frame.render_widget(paragraph, inner);
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
    let controls_height = area.height.saturating_sub(1).min(2);
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

fn title_line(issue: &Issue) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{}  ", issue.identifier),
            Style::new()
                .fg(theme::primary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            issue.title.clone(),
            Style::new().fg(theme::text()).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// The item's state as a coloured pill, e.g. ` open ` or ` PRIVATE draft `.
fn state_pill(issue: &Issue) -> Span<'static> {
    let (state, color) = if issue.security_advisory.is_some() {
        (format!("PRIVATE {}", issue.state), theme::error())
    } else {
        (issue.state.clone(), issue_color(&issue.state))
    };
    Span::styled(
        format!(" {state} "),
        Style::new()
            .fg(theme::bg())
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_detail_header(frame: &mut Frame<'_>, area: Rect, issue: &Issue) {
    let inner = Block::new().padding(Padding::new(2, 2, 0, 0)).inner(area);
    if inner.is_empty() {
        return;
    }
    // `#376  title …                    open` with the pill on the right.
    // On narrow screens the title keeps the room and the pill drops.
    let pill = state_pill(issue);
    let mut title = title_line(issue);
    let width = usize::from(inner.width);
    let fits = title.width() + pill.width() + 2 <= width;
    let pill_width = if fits || width >= 60 {
        pill.width()
    } else {
        0
    };
    let room = width.saturating_sub(
        issue.identifier.chars().count()
            + 2
            + if pill_width > 0 {
                pill_width + 2
            } else {
                0
            },
    );
    if let Some(span) = title.spans.last_mut() {
        span.content = truncate(&span.content, room).into();
    }
    if pill_width > 0 && width > title.width() + pill_width + 2 {
        let pad = usize::from(inner.width) - title.width() - pill_width;
        title.spans.extend([Span::raw(" ".repeat(pad)), pill]);
    }
    frame.render_widget(Paragraph::new(title), inner);
}

/// Overview · Description · Agent · Details, with a dot on Agent while a run
/// exists so its output is not hidden behind the tab.
fn draw_tabs(frame: &mut Frame<'_>, area: Rect, current: DetailTab, run: Option<&RunSummary>) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Block::new().style(Style::new().bg(theme::element())), area);
    // The tabs sit on the strip's middle row.
    let area = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    // Narrow screens name only the current tab; the others keep their number.
    let compact = area.width < 64;
    let mut spans = vec![Span::raw(" ")];
    for (index, tab) in DetailTab::ALL.into_iter().enumerate() {
        let style = if tab == current {
            Style::new()
                .fg(theme::bg())
                .bg(theme::primary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::muted())
        };
        spans.push(Span::styled(
            if compact && tab != current {
                format!(" {} ", index + 1)
            } else {
                format!(" {} {} ", index + 1, tab.label())
            },
            style,
        ));
        if tab == DetailTab::Agent
            && let Some(run) = run
        {
            spans.push(Span::styled("●", Style::new().fg(run_color(run.state))));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    // The key hint sits apart, at the right edge, so it doesn't read as a tab.
    if !compact {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Tab", Style::new().fg(theme::text()).bold()),
                Span::styled(" switch  ", Style::new().fg(theme::muted())),
            ]))
            .alignment(Alignment::Right),
            area,
        );
    }
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
    let reading = inner.width.min(READING_WIDTH);
    let lines = match app.detail_tab {
        DetailTab::Overview => overview_lines(
            snapshot,
            issue,
            latest_run,
            reading,
            &mut app.markdown_cache,
            "2",
        ),
        DetailTab::Description => description_lines(issue, reading, &mut app.markdown_cache),
        DetailTab::Agent => agent_lines(snapshot, issue, latest_run, reading),
        DetailTab::Details => details_lines(issue, reading),
    };
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
    // A scrollbar in the right padding whenever the tab runs past the view.
    if app.detail_scroll_max > 0 && inner.right() < area.right() {
        let track = Rect::new(inner.right(), inner.y, 1, inner.height);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some(" "))
            .track_style(Style::new().bg(theme::element()))
            .thumb_symbol(" ")
            .thumb_style(Style::new().bg(theme::border()));
        // Ratatui counts scroll positions, not rows.
        let mut state = ScrollbarState::new(usize::from(app.detail_scroll_max) + 1)
            .position(usize::from(app.detail_scroll))
            .viewport_content_length(usize::from(inner.height));
        frame.render_stateful_widget(scrollbar, track, &mut state);
    }
}

/// What you scan first: branch, people, the agent, then the start of the
/// description, with a pointer to the rest.
fn overview_lines(
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
    width: u16,
    markdown_cache: &mut crate::widgets::markdown::MarkdownCache,
    more_key: &'static str,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(advisory) = &issue.security_advisory {
        lines.push(field("Advisory", &advisory.ghsa_id));
        lines.push(field(
            "Severity",
            advisory.severity.as_deref().unwrap_or("unknown"),
        ));
        lines.push(field_styled(
            "Privacy",
            "PRIVATE - local UI only; explicit consent required to dispatch",
            theme::error(),
        ));
    }
    if let Some(pr) = &issue.pull_request {
        lines.push(Line::from(vec![
            label("Branch"),
            Span::styled(pr.head_ref.clone(), Style::new().fg(theme::text())),
            Span::styled(" → ", Style::new().fg(theme::muted())),
            Span::styled(pr.base_ref.clone(), Style::new().fg(theme::text())),
        ]));
    }
    if let Some(author) = issue.author.as_deref() {
        lines.push(field("Author", author));
    }
    if !issue.labels.is_empty() {
        lines.push(field("Labels", &issue.labels.join(", ")));
    }
    if let Some(priority) = issue.priority {
        lines.push(Line::from(vec![
            label("Priority"),
            Span::styled(
                format!("P{priority}"),
                Style::new()
                    .fg(priority_color(priority))
                    .add_modifier(if priority <= 0 {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    if !issue.blocked_by.is_empty() {
        lines.push(field("Blocked by", &issue.blocked_by.join(", ")));
    }
    if let Some(pr) = &issue.pull_request
        && (pr.additions.is_some() || pr.deletions.is_some())
    {
        lines.push(Line::from(vec![
            label("Changes"),
            Span::styled(
                format!("+{}", pr.additions.unwrap_or(0)),
                Style::new().fg(theme::done()),
            ),
            Span::raw(" "),
            Span::styled(
                format!("−{}", pr.deletions.unwrap_or(0)),
                Style::new().fg(theme::error()),
            ),
        ]));
    }
    let activity = issue.activity.unwrap_or_default();
    let counts: Vec<String> = [(activity.commits, "commit"), (activity.comments, "comment")]
        .into_iter()
        .filter_map(|(count, noun)| {
            count.map(|count| {
                format!(
                    "{count} {noun}{}",
                    if count == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            })
        })
        .collect();
    if !counts.is_empty() {
        lines.push(field("Activity", &counts.join(" · ")));
    }
    lines.push(field("Source", &issue.key.provider.to_string()));
    lines.push(field("Repository", &issue.key.repository));
    let agent_label = if issue.pull_request.is_some() {
        "Review"
    } else {
        "Agent"
    };
    match latest_run {
        Some(run) => {
            let mut spans = vec![
                label(agent_label),
                Span::styled(
                    format!("{} ", agent_glyph(&run.agent)),
                    Style::new().fg(run_color(run.state)),
                ),
                Span::styled(
                    format!("{} {}", run.agent, run_label(run.state)),
                    Style::new().fg(run_color(run.state)).bold(),
                ),
                Span::styled(
                    format!(" for {}", age_label(Some(run.started_at))),
                    Style::new().fg(theme::muted()),
                ),
            ];
            if let Some(model) = run.model.as_deref().filter(|_| !run.confidential) {
                spans.push(Span::styled(
                    format!(" · {model}"),
                    Style::new().fg(theme::muted()),
                ));
            }
            if run.state.needs_attention() {
                spans.push(Span::styled(
                    "  ATTENTION",
                    Style::new().fg(theme::error()).add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(spans));
            let latest = (!run.confidential && issue.security_advisory.is_none())
                .then(|| snapshot.run_events.get(&run.id))
                .flatten()
                .and_then(|events| events.last());
            if let Some(event) = latest
                && let Some(text) = event_summary(&event.payload)
            {
                lines.push(Line::from(vec![
                    label("Latest"),
                    Span::styled(
                        format!("{}  ", event.timestamp.format("%H:%M")),
                        Style::new().fg(theme::secondary()),
                    ),
                    Span::styled(text, Style::new().fg(theme::text())),
                ]));
            }
        },
        None => lines.push(Line::from(vec![
            label(agent_label),
            Span::styled(not_started(issue), Style::new().fg(theme::muted())),
        ])),
    }

    lines.push(Line::raw(""));
    let description = description_lines(issue, width, markdown_cache);
    let total = description.len();
    lines.extend(description.into_iter().take(OVERVIEW_DESCRIPTION_LINES));
    if total > OVERVIEW_DESCRIPTION_LINES {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("Full description: {total} lines  "),
                Style::new().fg(theme::muted()),
            ),
            Span::styled(more_key, Style::new().fg(theme::text()).bold()),
            Span::styled(" to read", Style::new().fg(theme::muted())),
        ]));
    }
    lines
}

fn not_started(issue: &Issue) -> &'static str {
    if issue.security_advisory.is_some() {
        "Not reviewed. Press d for settings, then explicit privacy consent."
    } else if issue.pull_request.is_some() {
        "Not reviewed. Press d to start a PR review."
    } else {
        "Not dispatched. Press d to start an agent."
    }
}

/// One line for the Overview's "Latest" row, or `None` for empty output.
fn event_summary(event: &RunEvent) -> Option<String> {
    let text = match event {
        RunEvent::Output { text, .. } => text.lines().rev().find(|l| !l.trim().is_empty())?,
        RunEvent::StateChanged { message, state } => {
            return Some(
                message
                    .as_deref()
                    .map_or_else(|| run_label(*state).to_owned(), one_line),
            );
        },
        RunEvent::AssistantMessage { text } => text,
        RunEvent::ToolStarted { name } => return Some(format!("{name} started")),
        RunEvent::ToolFinished { name, success } => {
            return Some(format!(
                "{name} {}",
                if *success {
                    "finished"
                } else {
                    "failed"
                }
            ));
        },
        RunEvent::InputRequested { prompt } => prompt,
        RunEvent::PermissionRequested { description, .. } => description,
        RunEvent::Completed { success, message } => {
            return Some(message.clone().unwrap_or_else(|| {
                if *success {
                    "completed"
                } else {
                    "failed"
                }
                .to_owned()
            }));
        },
    };
    Some(one_line(text))
}

fn description_lines(
    issue: &Issue,
    width: u16,
    markdown_cache: &mut crate::widgets::markdown::MarkdownCache,
) -> Vec<Line<'static>> {
    match issue
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        Some(description) => markdown_cache.render(description, width).lines,
        None => {
            *markdown_cache = Default::default();
            vec![Line::styled(
                "No description.",
                Style::new().fg(theme::muted()),
            )]
        },
    }
}

/// Everything technical: identifiers, refs with SHAs, raw counts and the URL.
fn details_lines(issue: &Issue, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![section(if issue.security_advisory.is_some() {
        "PRIVATE Security Advisory"
    } else if issue.pull_request.is_some() {
        "Pull Request"
    } else {
        "Issue"
    })];
    if let Some(advisory) = &issue.security_advisory {
        lines.push(row(width, "GHSA", &advisory.ghsa_id));
        lines.push(row(
            width,
            "CVE",
            advisory.cve_id.as_deref().unwrap_or("not assigned"),
        ));
        lines.push(row(
            width,
            "severity",
            advisory.severity.as_deref().unwrap_or("unknown"),
        ));
        lines.push(row(
            width,
            "privacy",
            "PRIVATE - local UI only; explicit consent required to dispatch",
        ));
        lines.push(row(
            width,
            "cleanup",
            "Private clone retained; separate cleanup unavailable",
        ));
        if issue.state != "draft" {
            lines.push(row(
                width,
                "dispatch",
                "Read-only: accept triage reports on GitHub manually; only drafts can launch",
            ));
        }
    }
    if let Some(pr) = &issue.pull_request {
        lines.push(row(width, "number", &format!("#{}", pr.number)));
        lines.push(row(
            width,
            "base",
            &format!("{} ({})", pr.base_ref, pr.base_sha),
        ));
        lines.push(row(
            width,
            "head",
            &format!("{} ({})", pr.head_ref, pr.head_sha),
        ));
        lines.push(row(
            width,
            "head repo",
            pr.head_repository.as_deref().unwrap_or("unknown"),
        ));
        lines.push(row(
            width,
            "changes",
            &format!(
                "+{} -{}",
                pr.additions
                    .map_or_else(|| "?".to_owned(), |n| n.to_string()),
                pr.deletions
                    .map_or_else(|| "?".to_owned(), |n| n.to_string())
            ),
        ));
    }
    lines.push(row(width, "id", &issue.key.canonical()));
    lines.push(row(width, "source", &issue.key.provider.to_string()));
    lines.push(row(width, "repository", &issue.key.repository));
    lines.push(row(width, "state", &issue.state));
    let activity = issue.activity.unwrap_or_default();
    let mut counts = vec![("comments", activity.comments)];
    if issue.pull_request.is_some() {
        counts.extend([
            ("review comments", activity.review_comments),
            ("commits", activity.commits),
        ]);
    }
    for (label, count) in counts {
        lines.push(row(
            width,
            label,
            &count.map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
        ));
    }
    if let Some(priority) = issue.priority {
        lines.push(row_styled(
            width,
            "priority",
            &format!("P{priority}"),
            priority_color(priority),
        ));
    }
    if let Some(author) = issue.author.as_deref() {
        lines.push(row(width, "author", author));
    }
    if !issue.labels.is_empty() {
        lines.push(row(width, "labels", &issue.labels.join(", ")));
    }
    if !issue.blocked_by.is_empty() {
        lines.push(row(width, "blocked by", &issue.blocked_by.join(", ")));
    }
    if let Some(url) = issue.url.as_deref() {
        lines.push(row(width, "url", url));
    }
    lines
}

fn agent_lines(
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    latest_run: Option<&RunSummary>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![section("Latest run")];
    if let Some(run) = latest_run {
        if run.confidential {
            lines.push(row(
                width,
                "privacy",
                "PRIVATE run; launcher output is not persisted",
            ));
        }
        lines.push(row_styled(
            width,
            "state",
            run_label(run.state),
            run_color(run.state),
        ));
        lines.push(row(width, "run", &run.id));
        lines.push(row(width, "agent", &run.agent));
        lines.push(row(width, "started", &timestamp_label(run.started_at)));
        lines.push(row(width, "updated", &timestamp_label(run.updated_at)));
        if let Some(session_id) = run.session_id.as_deref() {
            lines.push(row(width, "session", session_id));
            // Normal Herdr sessions are agent IDs, not resumable OpenCode sessions.
            if run.agent == "opencode"
                && run.state == agent_launcher_core::RunState::Completed
                && session_id.starts_with("ses_")
                && session_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                let command = format!("opencode -s {session_id}");
                if let Some(path) = run
                    .workspace
                    .as_ref()
                    .and_then(|workspace| workspace.path.as_ref())
                {
                    let quoted_path = path.to_string_lossy().replace('\'', "'\\''");
                    lines.push(row(
                        width,
                        "resume",
                        &format!("cd -- '{quoted_path}' && {command}"),
                    ));
                } else {
                    lines.push(row(width, "resume", &command));
                }
                lines.push(Line::raw("Resume in the run's worktree, on its host if remote; not the launcher repository."));
            }
        }
        if let Some(message) = run.message.as_deref() {
            for (index, line) in message.lines().enumerate() {
                lines.push(row_styled(
                    width,
                    if index == 0 {
                        "message"
                    } else {
                        ""
                    },
                    line,
                    if run.state.needs_attention() {
                        theme::error()
                    } else {
                        theme::text()
                    },
                ));
            }
        }
        if let Some(workspace) = &run.workspace {
            lines.push(row(width, "workspace", &workspace.id));
            lines.push(row(width, "backend", &workspace.backend.to_string()));
            lines.push(row(width, "branch", &workspace.branch));
            if let Some(host) = workspace.host.as_deref() {
                lines.push(row(width, "host", host));
            }
            if let Some(path) = &workspace.path {
                lines.push(row(width, "path", &path.display().to_string()));
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
            not_started(issue),
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

/// Overview label column.
fn label(name: &'static str) -> Span<'static> {
    Span::styled(format!("{name:<12}"), Style::new().fg(theme::muted()))
}

fn field(name: &'static str, value: &str) -> Line<'static> {
    field_styled(name, value, theme::text())
}

fn field_styled(name: &'static str, value: &str, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        label(name),
        Span::styled(value.to_owned(), Style::new().fg(color)),
    ])
}

fn section(label: &'static str) -> Line<'static> {
    Line::styled(label, Style::new().fg(theme::primary()).bold())
}

/// Right edge of the value column in the Agent and Details tabs.
const FACT_WIDTH: u16 = 88;

fn row(width: u16, label: &str, value: &str) -> Line<'static> {
    row_styled(width, label, value, theme::text())
}

/// `label ····· value`, values right-aligned to one column edge so a list of
/// facts reads as a table. A value too long for that follows its label.
fn row_styled(width: u16, label: &str, value: &str, color: ratatui::style::Color) -> Line<'static> {
    let edge = usize::from(width.min(FACT_WIDTH));
    let used = Line::raw(label).width() + Line::raw(value).width();
    let value = Span::styled(value.to_owned(), Style::new().fg(color));
    let label_span = Span::styled(label.to_owned(), Style::new().fg(theme::muted()));
    match edge.checked_sub(used) {
        Some(gap) if gap >= 4 && label.is_empty() => {
            Line::from(vec![Span::raw(" ".repeat(gap)), value])
        },
        Some(gap) if gap >= 4 => Line::from(vec![
            label_span,
            Span::styled(
                format!(" {} ", "·".repeat(gap - 2)),
                Style::new().fg(theme::border()),
            ),
            value,
        ]),
        _ => Line::from(vec![label_span, Span::raw("  "), value]),
    }
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
    } else if area.height == 1 && area.width < 104 {
        vec![control_line(&[
            (
                "d",
                if review {
                    " review PR"
                } else {
                    " dispatch"
                },
            ),
            (" Esc", ""),
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

pub(crate) fn draw_input_overlay(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
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

pub(crate) fn draw_delete_overlay(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
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
