use std::collections::HashSet;

use agent_launcher_core::{BackendKind, Issue, RuntimeSnapshot};
use ratatui::{
    Frame,
    layout::{Alignment, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Sparkline,
    },
};

use crate::{
    app::{AppState, Route},
    detail::draw_detail,
    format::{age_label, truncate},
    metrics::HostMetrics,
    rows::{DisplayRow, IssueSort, display_rows_matching, hierarchy_prefix},
    status::{issue_color, issue_icon, run_color, run_label},
    theme,
    widgets::render_bottom_edge,
};

pub(crate) fn draw(frame: &mut Frame<'_>, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(theme::bg())), area);
    app.reconcile_detail(snapshot);
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    match app.route {
        Route::Inbox => draw_inbox(frame, content, snapshot, app),
        Route::Detail => draw_detail(frame, content, snapshot, app),
    }
    if app.route == Route::Inbox {
        if app.sort_overlay {
            draw_sort_overlay(frame, content, app);
        } else if app.command_overlay {
            draw_command_overlay(frame, content);
        }
    }
    draw_footer(frame, area, snapshot, &app.host_metrics);
}

fn draw_inbox(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    if area.width < 24 || area.height < 8 {
        draw_tiny_inbox(frame, area, snapshot, app);
        return;
    }

    let body = area;
    let horizontal_margin = if body.width >= 80 {
        2
    } else {
        1
    };
    let panel_width = body.width.saturating_sub(horizontal_margin * 2).min(104);
    let full_logo = panel_width >= 62 && body.height >= 17;
    let logo_height: u16 = if full_logo {
        2
    } else {
        1
    };
    let logo_gap = u16::from(full_logo);
    let legend_height = if body.height >= 9 {
        2
    } else if body.height >= 7 {
        1
    } else {
        0
    };
    let fixed_height = logo_height
        .saturating_add(logo_gap)
        .saturating_add(legend_height);
    let panel_height = body.height.min(fixed_height.saturating_add(18));
    let panel = Rect::new(
        body.x + body.width.saturating_sub(panel_width) / 2,
        body.y + body.height.saturating_sub(panel_height) / 2,
        panel_width,
        panel_height,
    );

    let top_space = panel.y.saturating_sub(body.y);
    if panel.width >= 48 && top_space >= 7 {
        let chart_height = top_space.saturating_sub(2).min(10);
        let chart = Rect::new(
            panel.x,
            body.y + top_space.saturating_sub(chart_height) / 2,
            panel.width,
            chart_height,
        );
        draw_agent_activity(frame, chart, app);
    }

    let logo = Rect::new(panel.x, panel.y, panel.width, logo_height);
    draw_logo(frame, logo, full_logo);
    let listing_y = logo.y + logo.height + logo_gap;
    let listing_height = panel
        .y
        .saturating_add(panel.height)
        .saturating_sub(listing_y)
        .saturating_sub(legend_height);
    let listing = Rect::new(panel.x, listing_y, panel.width, listing_height);
    draw_listing(frame, listing, snapshot, app);
    if legend_height > 0 {
        draw_legends(
            frame,
            Rect::new(
                panel.x,
                listing.y + listing.height,
                panel.width,
                legend_height,
            ),
            snapshot,
            app,
        );
    }
}

fn draw_tiny_inbox(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &AppState) {
    let message = if let Some(error) = snapshot.error.as_deref() {
        error
    } else if no_source_detected(snapshot) {
        "No issue source\nAdd a git remote or .beads\nrestart agent-launcher"
    } else {
        app.status_message
            .as_deref()
            .unwrap_or("agent launcher\nTerminal too small\nCtrl+G commands")
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::new().fg(if snapshot.error.is_some() {
                theme::error()
            } else {
                theme::primary()
            }))
            .alignment(Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true }),
        area.inner(Margin::new(1, 1)),
    );
}

fn draw_logo(frame: &mut Frame<'_>, area: Rect, full: bool) {
    if !full {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("agent ", Style::new().fg(theme::muted()).bold()),
                Span::styled("launcher", Style::new().fg(theme::text()).bold()),
            ]))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    for index in 0..area.height.min(2) as usize {
        let agent = theme::AGENT_LOGO[index];
        let launcher = theme::LAUNCHER_LOGO[index];
        let width = agent.chars().count() + 3 + launcher.chars().count();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(agent, Style::new().fg(theme::muted()).bold()),
                Span::raw("   "),
                Span::styled(launcher, Style::new().fg(theme::text()).bold()),
            ])),
            Rect::new(
                area.x + area.width.saturating_sub(width as u16) / 2,
                area.y + index as u16,
                area.width.min(width as u16),
                1,
            ),
        );
    }
}

fn draw_agent_activity(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if area.width < 8 || area.height < 4 {
        return;
    }
    let panel = Block::new()
        .style(Style::new().bg(theme::panel()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    if inner.height < 2 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "agent activity",
                Style::new().fg(activity_color(app)).bold(),
            ),
            Span::styled("  ·  live", Style::new().fg(theme::muted())),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let status = if inner.width < 64 && app.agent_activity.attention > 0 {
        format!(
            "{}w · {}! · 15m",
            app.agent_activity.working, app.agent_activity.attention
        )
    } else if inner.width < 64 && app.agent_activity.working > 0 {
        format!("{}w · 15m", app.agent_activity.working)
    } else if inner.width < 64 && app.agent_activity.idle > 0 {
        format!("{} idle · 15m", app.agent_activity.idle)
    } else if app.agent_activity.attention > 0 {
        format!(
            "{} working · {} attention · 15m",
            app.agent_activity.working, app.agent_activity.attention
        )
    } else if app.agent_activity.working > 0 {
        format!("{} working · 15m", app.agent_activity.working)
    } else if app.agent_activity.idle > 0 {
        format!("{} idle · 15m", app.agent_activity.idle)
    } else {
        "quiet · 15m".to_owned()
    };
    frame.render_widget(
        Paragraph::new(status)
            .style(Style::new().fg(theme::secondary()))
            .alignment(Alignment::Right),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let graph = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let data = app.agent_activity.sparkline(graph.width as usize);
    frame.render_widget(
        Sparkline::default()
            .data(&data)
            .max(100)
            .style(Style::new().fg(activity_color(app)).bg(theme::panel())),
        graph,
    );
    render_bottom_edge(
        area,
        theme::primary(),
        theme::panel(),
        theme::bg(),
        frame.buffer_mut(),
    );
}

fn activity_color(app: &AppState) -> ratatui::style::Color {
    if app.agent_activity.attention > 0 {
        theme::error()
    } else if app.agent_activity.working > 0 {
        theme::primary()
    } else {
        theme::secondary()
    }
}

fn draw_search(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if area.is_empty() {
        return;
    }
    let cursor = if (app.tick / 6).is_multiple_of(2) {
        "█"
    } else {
        " "
    };
    let query = truncate(&app.search_query, area.width.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(query, Style::new().fg(theme::text())),
            Span::styled(cursor, Style::new().fg(theme::text())),
        ])),
        area,
    );
}

fn draw_listing(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    if area.is_empty() {
        app.visible_rows = 0;
        return;
    }
    let vertical_padding = u16::from(area.height >= 8);
    let panel = Block::new()
        .style(Style::new().bg(theme::panel()))
        .padding(Padding::new(2, 1, vertical_padding, 0));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    if inner.is_empty() {
        app.visible_rows = 0;
        return;
    }

    let source_count = snapshot.sources.len();
    let metadata = format!(
        "{} {} · {}",
        source_count,
        if source_count == 1 {
            "source"
        } else {
            "sources"
        },
        app.issue_sort.label()
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Inbox", Style::new().fg(theme::primary()).bold()),
            Span::styled(format!(" · {metadata}"), Style::new().fg(theme::muted())),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let roomy = inner.height >= 10;
    let search_y = inner.y.saturating_add(1).saturating_add(u16::from(roomy));
    draw_search(frame, Rect::new(inner.x, search_y, inner.width, 1), app);
    let table_y = search_y.saturating_add(1).saturating_add(u16::from(roomy));
    let bottom_edge_y = area.y.saturating_add(area.height).saturating_sub(1);
    draw_table(
        frame,
        Rect::new(
            inner.x,
            table_y,
            inner.width,
            bottom_edge_y.saturating_sub(table_y),
        ),
        snapshot,
        app,
    );
    render_bottom_edge(
        area,
        theme::primary(),
        theme::panel(),
        theme::bg(),
        frame.buffer_mut(),
    );
}

fn draw_table(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    if area.is_empty() {
        app.visible_rows = 0;
        return;
    }
    let rows = display_rows_matching(snapshot, &app.search_query, app.issue_sort);
    app.selected = app.selected.min(rows.len().saturating_sub(1));
    let visible_rows = area.height.saturating_sub(1) as usize;
    app.visible_rows = visible_rows;
    let max_scroll = rows.len().saturating_sub(visible_rows);
    app.scroll = app.scroll.min(max_scroll);
    if app.selected < app.scroll {
        app.scroll = app.selected;
    } else if app.selected >= app.scroll.saturating_add(visible_rows) {
        app.scroll = app
            .selected
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(max_scroll);
    }

    let scrollbar_width = u16::from(rows.len() > visible_rows) * 2;
    let table_width = area.width.saturating_sub(scrollbar_width);
    let columns = Columns::for_width(table_width);
    draw_table_header(frame, Rect::new(area.x, area.y, table_width, 1), columns);
    if rows.is_empty() {
        draw_empty_state(
            frame,
            Rect::new(
                area.x,
                area.y + 1,
                table_width,
                area.height.saturating_sub(1),
            ),
            snapshot,
            &app.search_query,
        );
        return;
    }

    let active = snapshot
        .runs
        .iter()
        .filter(|run| run.state.is_active())
        .map(|run| run.issue_key.as_str())
        .collect::<HashSet<_>>();
    for (screen_row, row) in rows.iter().skip(app.scroll).take(visible_rows).enumerate() {
        let index = app.scroll + screen_row;
        let Some(issue) = snapshot.issues.get(row.issue_idx) else {
            continue;
        };
        draw_table_row(
            frame,
            Rect::new(area.x, area.y + 1 + screen_row as u16, table_width, 1),
            snapshot,
            issue,
            *row,
            columns,
            index == app.selected,
            active.contains(issue.key.canonical().as_str()),
            app.tick,
        );
    }
    if rows.len() > visible_rows {
        render_scrollbar(frame, area, rows.len(), visible_rows, app.scroll);
    }
}

#[derive(Clone, Copy)]
struct Columns {
    age: usize,
    source: usize,
    state: usize,
}

impl Columns {
    fn for_width(width: u16) -> Self {
        if width >= 72 {
            Self {
                age: 6,
                source: 8,
                state: 12,
            }
        } else if width >= 46 {
            Self {
                age: 5,
                source: 5,
                state: 8,
            }
        } else {
            Self {
                age: 4,
                source: 3,
                state: 2,
            }
        }
    }

    fn title(self, width: u16) -> usize {
        width.saturating_sub(4 + self.age as u16 + self.source as u16 + self.state as u16) as usize
    }
}

fn draw_table_header(frame: &mut Frame<'_>, area: Rect, columns: Columns) {
    let title_width = columns.title(area.width);
    let line = Line::from(vec![
        Span::styled("    ", Style::new().fg(theme::muted())),
        Span::styled(
            format!("{:<width$}", "age", width = columns.age),
            Style::new().fg(theme::muted()),
        ),
        Span::styled(
            format!(
                "{:<width$}",
                if columns.source <= 3 {
                    "src"
                } else {
                    "source"
                },
                width = columns.source
            ),
            Style::new().fg(theme::muted()),
        ),
        Span::styled(
            format!("{:<title_width$}", "title"),
            Style::new().fg(theme::muted()),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                if columns.state <= 2 {
                    "st"
                } else {
                    "state"
                },
                width = columns.state
            ),
            Style::new().fg(theme::muted()),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_table_row(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    issue: &Issue,
    row: DisplayRow,
    columns: Columns,
    selected: bool,
    active: bool,
    tick: u32,
) {
    let bg = if selected {
        theme::element()
    } else {
        theme::panel()
    };
    frame.render_widget(Block::new().style(Style::new().bg(bg)), area);
    let latest_run = snapshot
        .runs
        .iter()
        .filter(|run| run.issue_key == issue.key.canonical())
        .max_by_key(|run| (run.updated_at, run.started_at));
    let activity = if active {
        theme::BRAILLE_SPINNER[tick as usize % theme::BRAILLE_SPINNER.len()]
    } else if latest_run.and_then(|run| run.workspace.as_ref()).is_some() {
        "●"
    } else {
        " "
    };
    let state_text = latest_run.map_or(issue.state.as_str(), |run| run_label(run.state));
    let state_color =
        latest_run.map_or_else(|| issue_color(&issue.state), |run| run_color(run.state));
    let state = if columns.state <= 2 {
        latest_run.map_or_else(
            || issue_icon(&issue.state),
            |run| {
                if run.state.needs_attention() {
                    "!"
                } else {
                    "●"
                }
            },
        )
    } else {
        state_text
    };
    let title_width = columns.title(area.width);
    let title = format!(
        "{}{} {}",
        hierarchy_prefix(row.depth, row.last_child),
        issue_icon(&issue.state),
        issue.title
    );
    let text_color = if row.context_only {
        theme::secondary()
    } else {
        theme::text()
    };
    let source = issue.key.provider.to_string();
    let line = Line::from(vec![
        Span::styled(
            if selected {
                "▶ "
            } else {
                "  "
            },
            Style::new().fg(theme::primary()).bg(bg),
        ),
        Span::styled(
            format!("{activity} "),
            Style::new().fg(theme::primary()).bg(bg),
        ),
        Span::styled(
            format!(
                "{:<width$}",
                age_label(issue.created_at),
                width = columns.age
            ),
            Style::new().fg(theme::muted()).bg(bg),
        ),
        Span::styled(
            format!(
                "{:<width$}",
                truncate(&source, columns.source),
                width = columns.source
            ),
            Style::new()
                .fg(if row.context_only {
                    theme::secondary()
                } else {
                    theme::primary()
                })
                .bg(bg),
        ),
        Span::styled(
            format!("{:<title_width$}", truncate(&title, title_width)),
            Style::new().fg(text_color).bg(bg),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                truncate(state, columns.state),
                width = columns.state
            ),
            Style::new().fg(state_color).bg(bg),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_empty_state(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, query: &str) {
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::new().fg(theme::primary())),
        Span::styled("Tip", Style::new().fg(theme::text()).bold()),
    ])];
    if !query.is_empty() {
        lines.push(Line::styled(
            "No matching issues. Backspace edits · Esc clears.",
            Style::new().fg(theme::muted()),
        ));
    } else if let Some(error) = snapshot.error.as_deref() {
        lines.push(Line::styled(
            format!("Could not load sources · {error}"),
            Style::new().fg(theme::error()),
        ));
        lines.push(Line::styled(
            "Press Ctrl+G, then r to retry.",
            Style::new().fg(theme::muted()),
        ));
    } else if no_source_detected(snapshot) {
        lines.push(Line::styled(
            "No issue source. Add a GitHub/GitLab remote or initialize .beads.",
            Style::new().fg(theme::text()),
        ));
        lines.push(Line::styled(
            "Restart agent-launcher after adding it.",
            Style::new().fg(theme::muted()),
        ));
    } else if snapshot.refreshing {
        lines.push(Line::styled(
            "Refreshing issue sources...",
            Style::new().fg(theme::primary()),
        ));
    } else {
        lines.push(Line::styled(
            "Inbox clear. Press Ctrl+G, then r to refresh.",
            Style::new().fg(theme::muted()),
        ));
    }
    let width = area.width.max(1) as usize;
    let height = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                .max(1)
                .div_ceil(width)
        })
        .sum::<usize>()
        .min(area.height as usize) as u16;
    let target = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height) / 2,
        area.width,
        height,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true }),
        target,
    );
}

fn draw_legends(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &AppState) {
    let rows = display_rows_matching(snapshot, &app.search_query, app.issue_sort);
    let start = if rows.is_empty() {
        0
    } else {
        app.scroll + 1
    };
    let end = (app.scroll + app.visible_rows).min(rows.len());
    let range = if rows.is_empty() {
        format!("0 of 0 · {}", app.issue_sort.label())
    } else {
        format!(
            "{start}-{end} of {} · {}",
            rows.len(),
            app.issue_sort.label()
        )
    };
    let range_width = range.chars().count().min(area.width as usize) as u16;
    let status_width = area.width.saturating_sub(range_width.saturating_add(2));
    if status_width > 0 {
        frame.render_widget(
            Paragraph::new(status_line(snapshot, app)),
            Rect::new(area.x, area.y, status_width, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(range)
            .style(Style::new().fg(theme::muted()))
            .alignment(Alignment::Right),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(shortcut_line(area.width)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

fn status_line<'a>(snapshot: &'a RuntimeSnapshot, app: &'a AppState) -> Line<'a> {
    if let Some(error) = snapshot.error.as_deref() {
        return Line::from(vec![
            Span::styled("● ", Style::new().fg(theme::error())),
            Span::styled(error, Style::new().fg(theme::error())),
        ]);
    }
    if let Some(status) = app.status_message.as_deref() {
        return Line::from(vec![
            Span::styled("● ", Style::new().fg(theme::primary())),
            Span::styled(status, Style::new().fg(theme::muted())),
        ]);
    }
    let active = snapshot
        .runs
        .iter()
        .filter(|run| run.state.is_active())
        .count();
    if snapshot.refreshing {
        Line::from(vec![
            Span::styled(
                theme::BRAILLE_SPINNER[app.tick as usize % theme::BRAILLE_SPINNER.len()],
                Style::new().fg(theme::primary()),
            ),
            Span::styled(" refreshing", Style::new().fg(theme::muted())),
        ])
    } else if active > 0 {
        Line::from(vec![
            Span::styled(
                theme::BRAILLE_SPINNER[app.tick as usize % theme::BRAILLE_SPINNER.len()],
                Style::new().fg(theme::primary()),
            ),
            Span::styled(format!(" {active} active"), Style::new().fg(theme::muted())),
        ])
    } else {
        Line::from(vec![
            Span::styled("● ", Style::new().fg(theme::primary())),
            Span::styled("ready", Style::new().fg(theme::muted())),
        ])
    }
}

fn shortcut_line(width: u16) -> Line<'static> {
    let shortcuts = if width >= 68 {
        &[
            ("↑/↓", " navigate   "),
            ("Enter", " open   "),
            ("Ctrl+G", " commands   "),
            ("Esc", " quit"),
        ][..]
    } else if width >= 32 {
        &[
            ("↑↓", " nav  "),
            ("Enter", " open  "),
            ("Ctrl+G", " commands"),
        ][..]
    } else {
        &[("Ctrl+G", " commands")][..]
    };
    Line::from(
        shortcuts
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

fn draw_command_overlay(frame: &mut Frame<'_>, area: Rect) {
    let width = area.width.saturating_sub(2).min(64);
    let height = area.height.saturating_sub(2).min(21);
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
    let panel = Block::new()
        .style(Style::new().bg(theme::element()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = panel.inner(popup);
    frame.render_widget(panel, popup);
    if inner.is_empty() {
        return;
    }

    let lines = vec![
        Line::from(vec![
            Span::styled("Ctrl+G", Style::new().fg(theme::primary()).bold()),
            Span::styled("  launcher commands", Style::new().fg(theme::text()).bold()),
        ]),
        Line::styled(
            "Press a command key · Esc cancels",
            Style::new().fg(theme::muted()),
        ),
        Line::raw(""),
        command_help_line("d", "dispatch selected issue"),
        command_help_line("r", "refresh issue sources"),
        command_help_line("s", "choose issue sorting"),
        command_help_line("c", "clear search"),
        command_help_line("q", "quit agent-launcher"),
        command_help_line("?", "keep this keybind sheet open"),
        Line::raw(""),
        Line::styled("Inbox", Style::new().fg(theme::primary()).bold()),
        command_help_line("↑/↓ · PgUp/PgDn · Home/End", "navigate issues"),
        command_help_line("Enter", "open issue details"),
        command_help_line("Backspace", "edit search"),
        command_help_line("Esc", "quit from the main inbox"),
        Line::styled("Details", Style::new().fg(theme::primary()).bold()),
        command_help_line("d · o · s", "dispatch · open · stop"),
        command_help_line("i · x · Esc", "input · delete · back"),
        command_help_line("Ctrl+C", "quit from anywhere"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::new().bg(theme::element()))
            .wrap(ratatui::widgets::Wrap { trim: false }),
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

fn command_help_line(key: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<30}"), Style::new().fg(theme::text()).bold()),
        Span::styled(action, Style::new().fg(theme::muted())),
    ])
}

fn draw_sort_overlay(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let width = area.width.saturating_sub(2).min(50);
    let height = area.height.saturating_sub(2).min(11);
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
    let panel = Block::new()
        .style(Style::new().bg(theme::element()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = panel.inner(popup);
    frame.render_widget(panel, popup);
    if inner.is_empty() {
        return;
    }

    let mut lines = vec![
        Line::styled("Sort issues", Style::new().fg(theme::primary()).bold()),
        Line::styled(
            "↑/↓ select · Enter apply · 1-5 · Esc cancel",
            Style::new().fg(theme::muted()),
        ),
        Line::raw(""),
    ];
    lines.extend(IssueSort::ALL.iter().enumerate().map(|(index, sort)| {
        let selected = index == app.sort_cursor;
        let current = *sort == app.issue_sort;
        Line::from(vec![
            Span::styled(
                format!(
                    "{} {}  ",
                    if selected {
                        "›"
                    } else {
                        " "
                    },
                    index + 1
                ),
                Style::new().fg(if selected {
                    theme::primary()
                } else {
                    theme::muted()
                }),
            ),
            Span::styled(
                format!("{:<18}", sort.label()),
                Style::new()
                    .fg(if selected {
                        theme::text()
                    } else {
                        theme::muted()
                    })
                    .add_modifier(if selected {
                        ratatui::style::Modifier::BOLD
                    } else {
                        ratatui::style::Modifier::empty()
                    }),
            ),
            Span::styled(
                if current {
                    "current"
                } else {
                    ""
                },
                Style::new().fg(theme::secondary()),
            ),
        ])
    }));
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(theme::element())),
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

fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    metrics: &HostMetrics,
) {
    if area.height == 0 {
        return;
    }
    let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1)
        .inner(Margin::new(u16::from(area.width >= 40), 0));
    let repository = snapshot.repository.as_ref().map_or_else(
        || "repository unavailable".to_owned(),
        |repository| abbreviated_path(&repository.root),
    );
    let source = footer_source_label(snapshot);
    let text = format!(
        "{repository}  ·  {source}  ·  {}  ·  {}",
        worktree_manager_label(snapshot),
        if snapshot.selected_agent.is_empty() {
            "none"
        } else {
            &snapshot.selected_agent
        }
    );
    let version = env!("CARGO_PKG_VERSION");
    if metrics.has_samples() && footer.width < 37 {
        frame.render_widget(
            Paragraph::new(format!(
                "CPU {}%  MEM {}%",
                metrics.cpu_percent, metrics.memory_percent
            ))
            .style(Style::new().fg(load_color(metrics.cpu_percent.max(metrics.memory_percent))))
            .alignment(Alignment::Right),
            footer,
        );
        return;
    }

    let version_width = version.len() as u16;
    let version_x = footer.x + footer.width.saturating_sub(version_width);
    let mut left_width = footer.width.saturating_sub(version_width + 1);
    if metrics.has_samples() {
        let available = footer.width.saturating_sub(version_width + 1);
        let spark_width = (footer.width / 5)
            .clamp(8, 24)
            .min(available.saturating_sub(23));
        let metrics_width = 23 + spark_width;
        let metrics_x = version_x.saturating_sub(metrics_width + 1);
        left_width = metrics_x.saturating_sub(footer.x + 1);
        let cpu_label = format!("CPU {:>3}% 15m ", metrics.cpu_percent);
        frame.render_widget(
            Paragraph::new(cpu_label).style(Style::new().fg(load_color(metrics.cpu_percent))),
            Rect::new(metrics_x, footer.y, 13, 1),
        );
        let spark_data = metrics.cpu_sparkline(spark_width as usize);
        frame.render_widget(
            Sparkline::default()
                .data(&spark_data)
                .max(100)
                .style(Style::new().fg(load_color(metrics.cpu_percent))),
            Rect::new(metrics_x + 13, footer.y, spark_width, 1),
        );
        frame.render_widget(
            Paragraph::new(format!("  MEM {:>3}%", metrics.memory_percent))
                .style(Style::new().fg(load_color(metrics.memory_percent))),
            Rect::new(metrics_x + 13 + spark_width, footer.y, 10, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(truncate(&text, left_width as usize)).style(Style::new().fg(theme::muted())),
        Rect::new(footer.x, footer.y, left_width, 1),
    );
    frame.render_widget(
        Paragraph::new(version)
            .style(Style::new().fg(theme::muted()))
            .alignment(Alignment::Right),
        footer,
    );
}

fn load_color(percent: u64) -> ratatui::style::Color {
    if percent >= 90 {
        theme::error()
    } else if percent >= 70 {
        theme::primary()
    } else {
        theme::muted()
    }
}

fn worktree_manager_label(snapshot: &RuntimeSnapshot) -> &'static str {
    [BackendKind::Superset, BackendKind::Herdr]
        .into_iter()
        .find(|kind| {
            snapshot
                .backends
                .iter()
                .any(|backend| backend.kind == *kind && backend.manager_running)
        })
        .map_or("please run this in a worktree manager", |kind| match kind {
            BackendKind::Superset => "superset",
            BackendKind::Herdr => "herdr",
            BackendKind::Native | BackendKind::Conductor => unreachable!(),
        })
}

fn footer_source_label(snapshot: &RuntimeSnapshot) -> String {
    if snapshot.sources.is_empty() {
        return "no source".to_owned();
    }
    let connected = snapshot
        .sources
        .iter()
        .filter(|source| source.connected)
        .count();
    if snapshot.sources.len() == 1 {
        let source = &snapshot.sources[0];
        let name = source.name.split(':').next().unwrap_or(&source.name);
        format!(
            "{name} {}",
            if source.connected {
                "online"
            } else {
                "offline"
            }
        )
    } else if connected == snapshot.sources.len() {
        format!("{} sources online", snapshot.sources.len())
    } else {
        format!("{connected}/{} sources online", snapshot.sources.len())
    }
}

fn abbreviated_path(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_owned();
    }
    path.strip_prefix(home).map_or_else(
        |_| path.display().to_string(),
        |relative| format!("~/{}", relative.display()),
    )
}

pub(crate) fn no_source_detected(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.sources.is_empty()
        && snapshot
            .repository
            .as_ref()
            .is_none_or(|repository| repository.remote.is_none() && !repository.has_beads)
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    total: usize,
    visible: usize,
    scroll: usize,
) {
    if area.height <= 1 || total == 0 {
        return;
    }
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some(" "))
        .track_style(Style::new().bg(theme::element()))
        .thumb_symbol(" ")
        .thumb_style(Style::new().bg(theme::border()));
    let mut state = ScrollbarState::new(total)
        .position(scroll)
        .viewport_content_length(visible);
    frame.render_stateful_widget(
        scrollbar,
        Rect::new(area.x, area.y + 1, area.width, area.height - 1),
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use agent_launcher_core::{
        BackendKind, BackendStatus, EventEnvelope, IssueKey, IssueProvider, OutputStream,
        Repository, RepositoryRemote, RunEvent, RunState, RunSummary, SourceStatus, WorkspaceRef,
        WorktreeDeleteAction, WorktreeDeletePreview,
    };
    use chrono::{Duration, Utc};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use super::*;

    fn issue(id: &str, title: &str) -> Issue {
        Issue {
            key: IssueKey {
                provider: IssueProvider::Github,
                host: "github.com".to_owned(),
                repository: "acme/launcher".to_owned(),
                native_id: id.to_owned(),
            },
            identifier: format!("#{id}"),
            title: title.to_owned(),
            description: Some("Detailed acceptance criteria".to_owned()),
            state: "open".to_owned(),
            url: Some(format!("https://github.com/acme/launcher/issues/{id}")),
            author: Some("octocat".to_owned()),
            labels: vec!["runtime".to_owned()],
            parent_id: None,
            blocked_by: Vec::new(),
            priority: Some(1),
            created_at: Some(Utc::now() - Duration::days(3)),
            updated_at: Some(Utc::now()),
        }
    }

    fn normal_snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            repository: Some(Repository {
                root: PathBuf::from("/repo"),
                git_dir: PathBuf::from("/repo/.git"),
                remote: Some(RepositoryRemote {
                    name: "origin".to_owned(),
                    url: "https://github.com/acme/launcher.git".to_owned(),
                    host: "github.com".to_owned(),
                    repository: "acme/launcher".to_owned(),
                    provider: IssueProvider::Github,
                }),
                has_beads: false,
            }),
            issues: vec![issue("7", "Repair runtime dispatch")],
            sources: vec![SourceStatus {
                name: "github:github.com:acme/launcher".to_owned(),
                connected: true,
                message: None,
            }],
            backends: vec![BackendStatus {
                kind: BackendKind::Superset,
                available: true,
                manager_running: true,
                message: None,
            }],
            selected_backend: Some(BackendKind::Superset),
            selected_agent: "opencode".to_owned(),
            ..RuntimeSnapshot::default()
        }
    }

    fn render_buffer(
        width: u16,
        height: u16,
        snapshot: &RuntimeSnapshot,
        app: &mut AppState,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, snapshot, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn render(width: u16, height: u16, snapshot: &RuntimeSnapshot, app: &mut AppState) -> String {
        let buffer = render_buffer(width, height, snapshot, app);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn search_is_part_of_the_inbox_surface() {
        let mut app = AppState {
            search_query: "Repair".to_owned(),
            ..AppState::default()
        };
        let text = render(112, 28, &normal_snapshot(), &mut app);
        let search_line = text.lines().find(|line| line.contains("Repair█")).unwrap();
        assert!(search_line.contains("Repair█"));
        assert!(!search_line.contains('┃'));
    }

    #[test]
    fn scrollbar_has_a_gutter_after_state_text() {
        let mut snapshot = normal_snapshot();
        for id in 8..32 {
            snapshot
                .issues
                .push(issue(&id.to_string(), &format!("Issue {id}")));
        }
        let mut app = AppState::default();
        let buffer = render_buffer(112, 28, &snapshot, &mut app);

        let (row, line) = (0..28)
            .map(|y| {
                let line = (0..112)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>();
                (y, line)
            })
            .find(|(_, line)| line.contains("Issue 31"))
            .unwrap();
        let state = line.rfind("open").unwrap();
        let gutter_x = line[..state].chars().count() as u16 + 4;

        assert_eq!(buffer.cell((gutter_x, row)).unwrap().bg, theme::panel());
        assert_eq!(
            buffer.cell((gutter_x + 1, row)).unwrap().bg,
            theme::border()
        );
    }

    #[test]
    fn normal_inbox_has_opencode_visual_contract_and_columns() {
        let text = render(112, 28, &normal_snapshot(), &mut AppState::default());
        assert!(text.contains(theme::AGENT_LOGO[0]));
        assert!(text.contains(theme::LAUNCHER_LOGO[0]));
        assert!(!text.contains("agents · issues · workspaces"));
        assert!(!text.contains("Filter issues..."));
        assert!(text.contains('█'));
        assert!(text.contains("Inbox · 1 source · newest first"));
        assert!(!text.contains('┃'));
        assert!(text.contains('╹'));
        assert!(text.contains("age"));
        assert!(text.contains("source"));
        assert!(text.contains("Repair runtime dispatch"));
        assert!(text.contains("1-1 of 1 · newest first"));
        assert!(text.contains("Enter open"));
        assert!(text.contains("Ctrl+G"));
        assert!(!text.contains("d dispatch"));
        assert!(!text.contains("Ctrl+P"));
        assert!(!text.contains("Tab"));
        assert!(text.contains("/repo  ·  github online  ·  superset  ·  opencode"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(!text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn tall_inbox_uses_upper_whitespace_for_live_agent_activity() {
        let mut snapshot = normal_snapshot();
        let mut app = AppState::default();
        app.agent_activity.record(&snapshot);

        let before = render(112, 48, &snapshot, &mut app);
        let now = Utc::now();
        snapshot.runs.push(RunSummary {
            id: "run-live".to_owned(),
            issue_key: snapshot.issues[0].key.canonical(),
            workspace: None,
            agent: "pi".to_owned(),
            state: RunState::Running,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        });
        app.agent_activity.record(&snapshot);
        let after = render(112, 48, &snapshot, &mut app);

        assert!(before.contains("agent activity  ·  live"));
        assert!(before.contains("quiet · 15m"));
        assert_ne!(before, after);
        assert!(after.contains("1 working · 15m"));
        assert!(after.contains("Repair runtime dispatch"));
    }

    #[test]
    fn inbox_command_overlay_lists_prefixed_and_contextual_keybinds() {
        let mut app = AppState {
            command_overlay: true,
            ..AppState::default()
        };

        let text = render(90, 28, &normal_snapshot(), &mut app);

        assert!(text.contains("Ctrl+G  launcher commands"));
        assert!(text.contains("dispatch selected issue"));
        assert!(text.contains("refresh issue sources"));
        assert!(text.contains("choose issue sorting"));
        assert!(text.contains("quit agent-launcher"));
        assert!(text.contains("d · o · s"));
        assert!(text.contains("i · x · Esc"));
        assert!(text.contains("Ctrl+C"));
    }

    #[test]
    fn sort_overlay_lists_modes_and_marks_the_current_sort() {
        let mut app = AppState {
            sort_overlay: true,
            sort_cursor: 2,
            issue_sort: IssueSort::Newest,
            ..AppState::default()
        };

        let text = render(80, 24, &normal_snapshot(), &mut app);

        assert!(text.contains("Sort issues"));
        assert!(text.contains("newest first      current"));
        assert!(text.contains("recently updated"));
        assert!(text.contains("priority"));
        assert!(text.contains("title A-Z"));
        assert!(text.contains("Enter apply"));
    }

    #[test]
    fn footer_reports_the_detected_worktree_manager() {
        let mut snapshot = normal_snapshot();
        snapshot.backends = vec![
            BackendStatus {
                kind: BackendKind::Superset,
                available: false,
                manager_running: false,
                message: Some("not running".to_owned()),
            },
            BackendStatus {
                kind: BackendKind::Herdr,
                available: false,
                manager_running: true,
                message: Some("Herdr 0.7.4 is too old".to_owned()),
            },
            BackendStatus {
                kind: BackendKind::Native,
                available: true,
                manager_running: false,
                message: None,
            },
        ];
        snapshot.selected_backend = Some(BackendKind::Native);

        let text = render(112, 28, &snapshot, &mut AppState::default());
        assert!(text.contains("/repo  ·  github online  ·  herdr  ·  opencode"));
    }

    #[test]
    fn footer_shows_current_host_load_and_cpu_history() {
        let mut app = AppState::default();
        app.host_metrics.record(82, 67);

        let text = render(112, 28, &normal_snapshot(), &mut app);

        assert!(text.contains("CPU  82% 15m"));
        assert!(text.contains("MEM  67%"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn footer_asks_for_a_worktree_manager_when_none_is_detected() {
        let mut snapshot = normal_snapshot();
        snapshot.backends = vec![BackendStatus {
            kind: BackendKind::Native,
            available: true,
            manager_running: false,
            message: None,
        }];
        snapshot.selected_backend = Some(BackendKind::Native);

        let text = render(112, 28, &snapshot, &mut AppState::default());
        assert!(text.contains("please run this in a worktree manager"));
        assert!(!text.contains(" ·  native  · "));
    }

    #[test]
    fn no_source_empty_state_is_actionable() {
        let snapshot = RuntimeSnapshot {
            repository: Some(Repository {
                root: PathBuf::from("/repo"),
                git_dir: PathBuf::from("/repo/.git"),
                remote: None,
                has_beads: false,
            }),
            ..RuntimeSnapshot::default()
        };
        let text = render(80, 20, &snapshot, &mut AppState::default());
        assert!(text.contains("● Tip"));
        assert!(text.contains("No issue source"));
        assert!(text.contains(".beads"));
        assert!(text.contains("Restart agent-launcher"));
    }

    #[test]
    fn narrow_layout_keeps_core_controls_visible() {
        let text = render(36, 12, &normal_snapshot(), &mut AppState::default());
        assert!(text.contains("agent launcher"));
        assert!(!text.contains("Filter issues..."));
        assert!(text.contains('█'));
        assert!(text.contains("Inbox · 1 source · newest first"));
        assert!(!text.contains('┃'));
        assert!(text.contains('╹'));
        assert!(text.contains("src"));
        assert!(text.contains("Repair"));
        assert!(text.contains("Enter open"));
        assert!(text.contains("Ctrl+G"));
    }

    #[test]
    fn normal_detail_uses_accent_rails_and_preserves_actions() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            ..AppState::default()
        };

        let text = render(88, 24, &snapshot, &mut app);
        assert!(text.contains('┃'));
        assert!(text.contains("#7  Repair runtime dispatch"));
        assert!(text.contains("Description"));
        assert!(text.contains("Detailed acceptance criteria"));
        assert!(text.contains("d dispatch"));
        assert!(text.contains("i send input"));
        assert!(text.contains("Esc back"));
        assert!(!text.contains('┌'));
    }

    #[test]
    fn narrow_detail_keeps_issue_context_and_actions() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            ..AppState::default()
        };

        let text = render(40, 10, &snapshot, &mut app);
        assert!(text.contains('┃'));
        assert!(text.contains("#7  Repair runtime dispatch"));
        assert!(text.contains("github · acme/launcher · open"));
        assert!(text.contains("d dispatch"));
        assert!(text.contains("i input"));
        assert!(text.contains("Esc back"));
    }

    #[test]
    fn fuzzy_hierarchy_render_keeps_parent_as_context() {
        let mut snapshot = normal_snapshot();
        snapshot.issues[0].key.native_id = "epic".to_owned();
        snapshot.issues[0].identifier = "EPIC-1".to_owned();
        snapshot.issues[0].title = "Release epic".to_owned();
        let mut child = issue("8", "Fix authentication handshake");
        child.parent_id = Some("epic".to_owned());
        snapshot.issues.push(child);
        let mut app = AppState {
            search_query: "authn".to_owned(),
            ..AppState::default()
        };

        let text = render(88, 22, &snapshot, &mut app);
        assert!(text.contains("authn█"));
        assert!(text.contains("Release epic"));
        assert!(text.contains("└─ ● Fix authentication"));
        assert!(!text.contains("Repair runtime dispatch"));
    }

    #[test]
    fn needs_input_detail_shows_run_and_persisted_output() {
        let mut snapshot = normal_snapshot();
        let now = Utc::now();
        let run = RunSummary {
            id: "run-7".to_owned(),
            issue_key: snapshot.issues[0].key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Superset,
                id: "workspace-7".to_owned(),
                host: Some("builder".to_owned()),
                path: Some(PathBuf::from("/work/agent-7")),
                branch: "agent/7".to_owned(),
            }),
            agent: "opencode".to_owned(),
            state: RunState::NeedsInput,
            message: Some("Approve the migration?".to_owned()),
            session_id: Some("session-7".to_owned()),
            started_at: now,
            updated_at: now,
        };
        snapshot.runs.push(run.clone());
        snapshot.run_events = HashMap::from([(run.id.clone(), vec![
            EventEnvelope {
                run_id: run.id.clone(),
                sequence: 0,
                timestamp: now,
                payload: RunEvent::InputRequested {
                    prompt: "Approve the migration?".to_owned(),
                },
            },
            EventEnvelope {
                run_id: run.id.clone(),
                sequence: 1,
                timestamp: now,
                payload: RunEvent::Output {
                    stream: OutputStream::Pty,
                    text: "migration plan ready".to_owned(),
                },
            },
        ])]);
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            ..AppState::default()
        };
        let text = render(100, 44, &snapshot, &mut app);
        assert!(text.contains("ATTENTION"));
        assert!(text.contains("needs input"));
        assert!(text.contains("session-7"));
        assert!(text.contains("workspace-7"));
        assert!(text.contains("migration plan ready"));
        assert!(text.contains("i send input"));
        assert!(text.contains('┃'));
    }

    #[test]
    fn input_overlay_uses_composer_surface_and_bottom_edge() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            input_overlay: Some(crate::app::InputOverlay {
                run_id: "run-7".to_owned(),
                prompt: "Approve the migration?".to_owned(),
                text: "yes".to_owned(),
            }),
            ..AppState::default()
        };

        let text = render(80, 24, &snapshot, &mut app);
        assert!(text.contains("Send input"));
        assert!(text.contains("Approve the migration?"));
        assert!(text.contains("yes█"));
        assert!(text.contains("Enter send"));
        assert!(text.contains("Esc cancel"));
        assert!(text.contains('╹'));
        assert!(!text.contains('┌'));
    }

    #[test]
    fn delete_overlay_requires_confirmation_and_shows_loss_warnings() {
        let mut snapshot = normal_snapshot();
        let now = Utc::now();
        let run = RunSummary {
            id: "run-7".to_owned(),
            issue_key: snapshot.issues[0].key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Herdr,
                id: "workspace-7".to_owned(),
                host: None,
                path: Some(PathBuf::from("/work/agent-7")),
                branch: "agent/7".to_owned(),
            }),
            agent: "opencode".to_owned(),
            state: RunState::Running,
            message: None,
            session_id: Some("session-7".to_owned()),
            started_at: now,
            updated_at: now,
        };
        snapshot.runs.push(run.clone());
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            delete_overlay: Some(crate::app::DeleteOverlay {
                preview: WorktreeDeletePreview {
                    run,
                    action: WorktreeDeleteAction::Delete,
                    has_uncommitted_changes: true,
                    has_ignored_files: true,
                    unpushed_commits: 2,
                    inspection_warning: None,
                    inspection_fingerprint: Some("fingerprint".to_owned()),
                },
            }),
            ..AppState::default()
        };

        let text = render(90, 26, &snapshot, &mut app);
        assert!(text.contains("Delete worktree"));
        assert!(text.contains("uncommitted or untracked changes will be lost"));
        assert!(text.contains("ignored files in this worktree will be lost"));
        assert!(text.contains("2 commits not found on any remote"));
        assert!(text.contains("branch is retained"));
        assert!(text.contains("Enter confirm"));
        assert!(text.contains("Esc cancel"));
        assert!(app.delete_confirmation_visible);

        let compact = render(24, 6, &snapshot, &mut app);
        assert!(compact.contains("Resize to review"));
        assert!(!compact.contains("Enter confirm"));
        assert!(!app.delete_confirmation_visible);
    }

    #[test]
    fn runtime_error_takes_priority_over_success_message() {
        let mut snapshot = normal_snapshot();
        snapshot.error = Some("backend mutation failed".to_owned());
        let mut app = AppState {
            status_message: Some("workspace opened".to_owned()),
            ..AppState::default()
        };

        let text = render(80, 20, &snapshot, &mut app);
        assert!(text.contains("backend mutation failed"));
        assert!(!text.contains("workspace opened"));
    }

    #[test]
    fn tiny_detail_keeps_input_cancel_control_visible() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            input_overlay: Some(crate::app::InputOverlay {
                run_id: "run-7".to_owned(),
                prompt: "Approve?".to_owned(),
                text: "yes".to_owned(),
            }),
            ..AppState::default()
        };

        let text = render(20, 5, &snapshot, &mut app);
        assert!(text.contains("Esc cancel"));
    }
}
