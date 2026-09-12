use std::collections::HashSet;

use agent_launcher_core::{
    BackendKind, ComputeTargetAvailability, ComputeTargetStatus, Issue, RunState, RuntimeSnapshot,
};
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
    app::{AppState, DispatchStage, InboxTab, Route},
    detail::draw_detail,
    format::{age_label, truncate},
    metrics::HostMetrics,
    rows::{DisplayRow, IssueSort, hierarchy_prefix},
    status::{issue_color, issue_icon, run_color, run_label},
    theme,
    widgets::render_bottom_edge,
};

pub(crate) fn draw(frame: &mut Frame<'_>, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(theme::bg())), area);
    app.reconcile_detail(snapshot);
    app.reconcile_dispatch(snapshot);
    app.mouse = crate::mouse::MouseGeometry {
        screen: area,
        route: app.route,
        tab: app.tab,
        blocked: app.input_overlay.is_some()
            || app.delete_overlay.is_some()
            || app.dispatch_overlay.is_some()
            || app.command_overlay
            || app.sort_overlay,
        ..Default::default()
    };
    app.visible_rows = 0;
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    match app.route {
        Route::Inbox => draw_inbox(frame, content, snapshot, app),
        Route::Detail => draw_detail(frame, content, snapshot, app),
    }
    if app.dispatch_overlay.is_some() {
        draw_dispatch_overlay(frame, content, snapshot, app);
    } else if app.route == Route::Inbox {
        if app.sort_overlay {
            draw_sort_overlay(frame, content, app);
        } else if app.command_overlay {
            draw_command_overlay(frame, content, app);
        }
    }
    draw_footer(frame, area, snapshot, &app.host_metrics);
    app.mouse.scroll = app.scroll;
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
    } else if !snapshot.initialized {
        "Loading sources..."
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
    let demo = std::env::var_os("AGENT_LAUNCHER_DEMO_ACTIVITY").is_some();
    let show_timeline = inner.height >= 3 && inner.width >= 32;
    let graph = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height - 1 - u16::from(show_timeline),
    );
    let data = if demo {
        demo_activity(graph.width, app.tick)
    } else {
        app.agent_activity.sparkline(graph.width as usize)
    };
    let color = if demo {
        theme::primary()
    } else {
        activity_color(app)
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("agent activity", Style::new().fg(color).bold()),
            Span::styled(
                if demo {
                    "  ·  demo"
                } else {
                    "  ·  live"
                },
                Style::new().fg(theme::muted()),
            ),
        ])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let status = if demo {
        let working = data.last().copied().unwrap_or(0).div_ceil(25);
        if working == 0 {
            "quiet · 15m".to_owned()
        } else if inner.width < 64 {
            format!("{working}w · 15m")
        } else {
            format!("{working} working · 15m")
        }
    } else if inner.width < 64 && app.agent_activity.attention > 0 {
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

    // Color encodes intensity, not run status: historical samples only store a score.
    for (column, value) in data.iter().enumerate() {
        let bar_color = match value {
            0..=24 => ratatui::style::Color::Rgb(131, 165, 152),
            25..=49 => theme::done(),
            50..=74 => ratatui::style::Color::Rgb(215, 185, 112),
            _ => theme::primary(),
        };
        frame.render_widget(
            Sparkline::default()
                .data(std::slice::from_ref(value))
                .max(100)
                .style(Style::new().fg(bar_color).bg(theme::panel())),
            Rect::new(graph.x + column as u16, graph.y, 1, graph.height),
        );
    }
    if show_timeline {
        for (index, label) in ["-15m", "-10m", "-5m", "now"].iter().enumerate() {
            let width = label.len() as u16;
            let x = graph.x + (graph.width - width) * index as u16 / 3;
            frame.render_widget(
                Paragraph::new(*label).style(Style::new().fg(theme::muted())),
                Rect::new(x, graph.bottom(), width, 1),
            );
        }
    }
    render_bottom_edge(
        area,
        theme::primary(),
        theme::panel(),
        theme::bg(),
        frame.buffer_mut(),
    );
}

fn demo_activity(width: u16, tick: u32) -> Vec<u64> {
    let samples = [
        0, 0, 4, 9, 6, 8, 28, 52, 34, 38, 36, 41, 18, 8, 3, 0, 0, 0, 6, 22, 68, 44, 48, 46, 72, 92,
        58, 54, 61, 32, 12, 16, 10, 8, 11, 6, 0, 0, 3, 8, 14, 42, 38, 45, 40, 43, 64, 48, 22, 9, 6,
        0, 0, 4, 18, 76, 32, 24, 28, 26, 44, 58, 52, 56, 54, 80, 96, 62, 42, 46, 38, 18, 12, 8, 16,
        34, 52, 48, 56, 50, 68, 58, 62, 54, 74, 60, 48, 52, 46, 58,
    ];
    // Advance one terminal column every 960ms using the existing animation ticker.
    let offset = (tick / 12) as usize;
    (0..width as usize)
        .map(|column| samples[((column + offset) * samples.len() / width as usize) % samples.len()])
        .collect()
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
    let focused = !app.command_overlay
        && !app.sort_overlay
        && app.dispatch_overlay.is_none()
        && app.input_overlay.is_none()
        && app.delete_overlay.is_none();
    if app.search_query.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.tab == InboxTab::PullRequests {
                "Search PRs…"
            } else {
                "Search issues…"
            })
            .style(Style::new().fg(theme::muted())),
            area,
        );
        if focused {
            frame.buffer_mut()[(area.x, area.y)]
                .set_fg(theme::panel())
                .set_bg(theme::text());
        }
        return;
    }
    let cursor = if focused {
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
    let mut tabs = Vec::new();
    let mut x = inner.x;
    for tab in [InboxTab::Issues, InboxTab::PullRequests] {
        if !tabs.is_empty() {
            tabs.push(Span::raw("  "));
            x += 2;
        }
        let label = format!(" {} ", tab.label());
        let width = label.len() as u16;
        app.mouse
            .tabs
            .push((Rect::new(x, inner.y, width, 1).intersection(inner), tab));
        x += width;
        tabs.push(Span::styled(
            label,
            if app.tab == tab {
                Style::new().fg(theme::bg()).bg(theme::primary()).bold()
            } else {
                Style::new().fg(theme::text()).bg(theme::element())
            },
        ));
    }
    tabs.push(Span::styled(
        if inner.width >= 62 {
            format!("  {metadata}")
        } else {
            String::new()
        },
        Style::new().fg(theme::muted()),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(tabs)),
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
    let rows = app.rows(snapshot);
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
    app.mouse.list = Rect::new(area.x, area.y + 1, area.width, visible_rows as u16);
    let columns = Columns::for_width(table_width);
    let number_width = rows
        .iter()
        .filter_map(|row| {
            snapshot.issues[row.issue_idx]
                .pull_request
                .as_ref()
                .map(|pr| pr.number.to_string().len() as u16 + 1)
        })
        .max()
        .unwrap_or(2);
    let pr_columns = pr_columns(Rect::new(area.x, area.y, table_width, 1), number_width);
    if app.tab == InboxTab::PullRequests {
        for (column, label) in pr_columns
            .into_iter()
            .zip(["PR", "title", "author", "diff", "status"])
        {
            frame.render_widget(
                Paragraph::new(label)
                    .style(Style::new().fg(theme::muted()))
                    .alignment(if label == "status" {
                        Alignment::Right
                    } else {
                        Alignment::Left
                    }),
                column,
            );
        }
    } else {
        draw_table_header(frame, Rect::new(area.x, area.y, table_width, 1), columns);
    }
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
            app.tab,
        );
        return;
    }

    let active = snapshot
        .runs
        .iter()
        .filter(|run| run_is_working(run.state))
        .map(|run| run.issue_key.as_str())
        .collect::<HashSet<_>>();
    for (screen_row, row) in rows.iter().skip(app.scroll).take(visible_rows).enumerate() {
        let index = app.scroll + screen_row;
        let Some(issue) = snapshot.issues.get(row.issue_idx) else {
            continue;
        };
        let row_area = Rect::new(area.x, area.y + 1 + screen_row as u16, table_width, 1);
        app.mouse.rows.push((row_area, index, issue.key.clone()));
        if issue.pull_request.is_some() {
            draw_pr_row(frame, row_area, issue, index == app.selected, pr_columns);
            continue;
        }
        draw_table_row(
            frame,
            row_area,
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
    ]);
    let state_width = (columns.state as u16).min(area.width);
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width - state_width, 1),
    );
    frame.render_widget(
        Paragraph::new(if columns.state <= 2 {
            "st"
        } else {
            "state"
        })
        .style(Style::new().fg(theme::muted()))
        .alignment(Alignment::Right),
        Rect::new(area.right() - state_width, area.y, state_width, 1),
    );
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
            truncate(&title, title_width),
            Style::new().fg(text_color).bg(bg),
        ),
    ]);
    // Keep state independent of the terminal-cell width of the title.
    let state_width = (columns.state as u16).min(area.width);
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width - state_width, 1),
    );
    frame.render_widget(
        Paragraph::new(truncate(state, state_width as usize))
            .style(Style::new().fg(state_color).bg(bg))
            .alignment(Alignment::Right),
        Rect::new(area.right() - state_width, area.y, state_width, 1),
    );
}

fn pr_columns(area: Rect, number_width: u16) -> [Rect; 5] {
    // Reserve identity, diff, and lifecycle before sharing the rest with title/author.
    let available = area.width.saturating_sub(6);
    let number = number_width.min(available);
    let status = 6.min(available.saturating_sub(number));
    let diff = 4.min(available.saturating_sub(number + status));
    let remaining = available.saturating_sub(number + status + diff);
    let author = (remaining / 3).min(12);
    let widths = [number, remaining - author, author, diff, status];
    let mut x = area.x.saturating_add(2).min(area.right());
    widths.map(|width| {
        let rect = Rect::new(x, area.y, width.min(area.right() - x), 1);
        x = rect.right().saturating_add(1).min(area.right());
        rect
    })
}

fn pr_change_indicator(additions: Option<u64>, deletions: Option<u64>) -> Line<'static> {
    let (Some(additions), Some(deletions)) = (additions, deletions) else {
        return Line::styled("?□□□", Style::new().fg(theme::muted()));
    };
    let total = u128::from(additions) + u128::from(deletions);
    let filled = match total {
        0 => 0,
        1..=10 => 1,
        11..=100 => 2,
        101..=1000 => 3,
        _ => 4,
    };
    let mut green = if total == 0 {
        0
    } else {
        (u128::from(additions) * filled + total / 2) / total
    };
    if additions > 0 && deletions > 0 && filled >= 2 {
        green = green.clamp(1, filled - 1);
    }
    Line::from(
        (0..4)
            .map(|cell| {
                let (glyph, color) = if cell >= filled {
                    ("□", theme::muted())
                } else if cell < green {
                    ("■", theme::done())
                } else {
                    ("■", theme::error())
                };
                Span::styled(glyph, Style::new().fg(color))
            })
            .collect::<Vec<_>>(),
    )
}

fn draw_pr_row(
    frame: &mut Frame<'_>,
    area: Rect,
    issue: &Issue,
    selected: bool,
    columns: [Rect; 5],
) {
    let pr = issue.pull_request.as_ref().expect("PR row");
    let bg = if selected {
        theme::element()
    } else {
        theme::panel()
    };
    let marker = if selected {
        "▶"
    } else {
        " "
    };
    frame.render_widget(Block::new().style(Style::new().bg(bg)), area);
    frame.render_widget(
        Paragraph::new(marker).style(Style::new().fg(theme::primary())),
        Rect::new(area.x, area.y, area.width.min(2), 1),
    );
    let lines = [
        Line::styled(format!("#{}", pr.number), Style::new().fg(theme::primary())),
        Line::styled(
            truncate(&issue.title, columns[1].width as usize),
            Style::new().fg(theme::text()),
        ),
        Line::styled(
            truncate(
                issue.author.as_deref().unwrap_or("unknown"),
                columns[2].width as usize,
            ),
            Style::new().fg(theme::muted()),
        ),
        pr_change_indicator(pr.additions, pr.deletions),
        Line::styled(
            issue.state.as_str(),
            Style::new().fg(issue_color(&issue.state)),
        )
        .alignment(Alignment::Right),
    ];
    for (mut column, line) in columns.into_iter().zip(lines) {
        column.y = area.y;
        frame.render_widget(Paragraph::new(line).style(Style::new().bg(bg)), column);
    }
}

fn draw_empty_state(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    query: &str,
    tab: InboxTab,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::new().fg(theme::primary())),
        Span::styled("Tip", Style::new().fg(theme::text()).bold()),
    ])];
    if !query.is_empty() {
        lines.push(Line::styled(
            format!(
                "No matching {}. Backspace edits · Ctrl+G, c clears.",
                tab.label()
            ),
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
    } else if !snapshot.initialized || snapshot.refreshing {
        lines.push(Line::styled(
            format!("Loading {}...", tab.label()),
            Style::new().fg(theme::primary()),
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
    } else {
        lines.push(Line::styled(
            format!("No {}. Press Ctrl+G, then r to refresh.", tab.label()),
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
    let rows = app.rows(snapshot);
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
    let working = snapshot
        .runs
        .iter()
        .filter(|run| run_is_working(run.state))
        .count();
    if snapshot.refreshing {
        Line::from(vec![
            Span::styled(
                theme::BRAILLE_SPINNER[app.tick as usize % theme::BRAILLE_SPINNER.len()],
                Style::new().fg(theme::primary()),
            ),
            Span::styled(" refreshing", Style::new().fg(theme::muted())),
        ])
    } else if working > 0 {
        Line::from(vec![
            Span::styled(
                theme::BRAILLE_SPINNER[app.tick as usize % theme::BRAILLE_SPINNER.len()],
                Style::new().fg(theme::primary()),
            ),
            Span::styled(
                format!(" {working} working"),
                Style::new().fg(theme::muted()),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("● ", Style::new().fg(theme::primary())),
            Span::styled("ready", Style::new().fg(theme::muted())),
        ])
    }
}

fn run_is_working(state: RunState) -> bool {
    matches!(
        state,
        RunState::Provisioning | RunState::Starting | RunState::Running
    )
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

fn draw_command_overlay(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
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
        command_help_line(
            "d",
            if app.tab == InboxTab::PullRequests {
                "review selected PR"
            } else {
                "dispatch selected issue"
            },
        ),
        command_help_line("r", "refresh issue sources"),
        command_help_line("s", "choose issue sorting"),
        command_help_line("c", "clear search"),
        command_help_line("q", "quit agent-launcher"),
        command_help_line("Tab / BackTab", "switch Issues / PRs"),
        Line::raw(""),
        Line::styled("Inbox", Style::new().fg(theme::primary()).bold()),
        command_help_line("↑/↓ · PgUp/PgDn · Home/End", "navigate; wheel scrolls"),
        command_help_line("Enter / click row", "open issue or PR details"),
        command_help_line("Backspace", "edit search"),
        command_help_line("Esc", "quit from the main inbox"),
        Line::styled("Details", Style::new().fg(theme::primary()).bold()),
        command_help_line(
            "d · o · s",
            if app.tab == InboxTab::PullRequests {
                "review PR · open · stop"
            } else {
                "dispatch · open · stop"
            },
        ),
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

fn draw_dispatch_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &AppState,
) {
    let Some(overlay) = app.dispatch_overlay.as_ref() else {
        return;
    };
    let item_count = match &overlay.stage {
        DispatchStage::Prompt => snapshot.prompt_profiles.len(),
        DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
    };
    let width = area.width.saturating_sub(2).min(86);
    let wanted_height = item_count.saturating_add(6) as u16;
    let height = area.height.saturating_sub(2).min(wanted_height.max(7));
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
    let horizontal_padding = if width >= 24 {
        2
    } else {
        1
    };
    let vertical_padding = u16::from(height >= 5);
    let panel = Block::new()
        .style(Style::new().bg(theme::element()))
        .padding(Padding::new(
            horizontal_padding,
            horizontal_padding,
            vertical_padding,
            vertical_padding,
        ));
    let inner = panel.inner(popup);
    frame.render_widget(panel, popup);
    if inner.is_empty() {
        return;
    }

    let issue_title = snapshot
        .issues
        .iter()
        .find(|issue| issue.key == overlay.issue_key)
        .map_or("Selected issue", |issue| issue.title.as_str());
    let review = snapshot
        .issues
        .iter()
        .any(|issue| issue.key == overlay.issue_key && issue.pull_request.is_some());
    let mut lines = match &overlay.stage {
        DispatchStage::Prompt => {
            dispatch_prompt_lines(snapshot, overlay.cursor, issue_title, inner)
        },
        DispatchStage::Target { profile } => dispatch_target_lines(
            snapshot,
            overlay.cursor,
            issue_title,
            profile.as_deref(),
            inner,
            review,
        ),
    };
    if let Some(status) = &app.status_message {
        let index = usize::from(lines.len() > 1);
        lines[index] = Line::styled(status.clone(), Style::new().fg(theme::error()));
    }
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

fn dispatch_prompt_lines<'a>(
    snapshot: &'a RuntimeSnapshot,
    cursor: usize,
    issue_title: &str,
    inner: Rect,
) -> Vec<Line<'a>> {
    let selected_name = snapshot
        .prompt_profiles
        .get(cursor)
        .map_or("unavailable", String::as_str);
    if inner.height < 5 {
        return vec![Line::from(vec![
            Span::styled("Prompt  ", Style::new().fg(theme::primary()).bold()),
            Span::styled(
                selected_name.to_owned(),
                Style::new().fg(theme::text()).bold(),
            ),
            Span::styled(
                "  Enter dispatch · Esc cancel",
                Style::new().fg(theme::muted()),
            ),
        ])];
    }

    let mut lines = vec![
        Line::styled(
            format!(
                "Choose prompt · {}/{}",
                cursor + 1,
                snapshot.prompt_profiles.len()
            ),
            Style::new().fg(theme::primary()).bold(),
        ),
        Line::styled(
            truncate(issue_title, inner.width as usize),
            Style::new().fg(theme::muted()),
        ),
        Line::styled(
            "↑/↓ select · Enter dispatch · 1-9 · Esc cancel",
            Style::new().fg(theme::muted()),
        ),
        Line::raw(""),
    ];
    let visible = (inner.height as usize).saturating_sub(4).max(1);
    let start = cursor
        .saturating_sub(visible / 2)
        .min(snapshot.prompt_profiles.len().saturating_sub(visible));
    lines.extend(
        snapshot
            .prompt_profiles
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, name)| dispatch_choice_line(index, cursor, true, name.clone())),
    );
    lines
}

fn dispatch_target_lines<'a>(
    snapshot: &'a RuntimeSnapshot,
    cursor: usize,
    issue_title: &str,
    profile: Option<&str>,
    inner: Rect,
    review: bool,
) -> Vec<Line<'a>> {
    let automatic_enabled = snapshot
        .compute_targets
        .iter()
        .any(ComputeTargetStatus::is_dispatchable);
    let selected_name = if cursor == 0 {
        "Automatic"
    } else {
        snapshot
            .compute_targets
            .get(cursor - 1)
            .map_or("unavailable", |target| target.name.as_str())
    };
    if inner.height < 5 {
        return vec![Line::from(vec![
            Span::styled("Target  ", Style::new().fg(theme::primary()).bold()),
            Span::styled(
                selected_name.to_owned(),
                Style::new().fg(theme::text()).bold(),
            ),
            Span::styled(
                if review {
                    "  Enter review PR · Esc cancel"
                } else {
                    "  Enter dispatch · Esc cancel"
                },
                Style::new().fg(theme::muted()),
            ),
        ])];
    }

    let count = snapshot.compute_targets.len() + 1;
    let subtitle = profile.map_or_else(
        || issue_title.to_owned(),
        |profile| {
            format!(
                "{} · {profile}",
                truncate(issue_title, inner.width as usize)
            )
        },
    );
    let mut lines = vec![
        Line::styled(
            format!(
                "{} · {}/{}",
                if review {
                    "Review PR: compute target"
                } else {
                    "Choose compute target"
                },
                cursor + 1,
                count
            ),
            Style::new().fg(theme::primary()).bold(),
        ),
        Line::styled(
            truncate(&subtitle, inner.width as usize),
            Style::new().fg(theme::muted()),
        ),
        Line::styled(
            if review {
                "↑/↓ select · Enter review PR · 1-9 · Esc cancel"
            } else {
                "↑/↓ select · Enter dispatch · 1-9 · Esc cancel"
            },
            Style::new().fg(theme::muted()),
        ),
        Line::raw(""),
    ];
    let visible = (inner.height as usize).saturating_sub(4).max(1);
    let start = cursor
        .saturating_sub(visible / 2)
        .min(count.saturating_sub(visible));
    lines.extend((start..count).take(visible).map(|index| {
        if index == 0 {
            let status = if automatic_enabled {
                "ready"
            } else {
                "unavailable"
            };
            dispatch_choice_line(
                index,
                cursor,
                automatic_enabled,
                format!("Automatic  {status}"),
            )
        } else {
            let target = &snapshot.compute_targets[index - 1];
            dispatch_choice_line(
                index,
                cursor,
                target.is_dispatchable(),
                target_summary(target),
            )
        }
    }));
    lines
}

fn dispatch_choice_line(
    index: usize,
    cursor: usize,
    enabled: bool,
    label: String,
) -> Line<'static> {
    let selected = index == cursor;
    let color = if !enabled {
        theme::secondary()
    } else if selected {
        theme::text()
    } else {
        theme::muted()
    };
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
            label,
            Style::new().fg(color).add_modifier(if selected {
                ratatui::style::Modifier::BOLD
            } else {
                ratatui::style::Modifier::empty()
            }),
        ),
    ])
}

fn target_summary(target: &ComputeTargetStatus) -> String {
    let status = if target.is_full() {
        "full"
    } else {
        match target.availability {
            ComputeTargetAvailability::Online => "online",
            ComputeTargetAvailability::Wakeable => "wakeable",
            ComputeTargetAvailability::Offline => "offline",
            ComputeTargetAvailability::Full => "full",
        }
    };
    let maximum = target
        .max_active_runs
        .map_or_else(|| "-".to_owned(), |maximum| maximum.to_string());
    let cpu = target
        .cpu_percent
        .map_or_else(|| "-".to_owned(), |cpu| format!("{cpu:.0}%"));
    let memory = target
        .memory_percent
        .map_or_else(|| "-".to_owned(), |memory| format!("{memory:.0}%"));
    format!(
        "{}  {status} · {}/{} active · CPU {cpu} · MEM {memory}",
        target.name, target.active_runs, maximum
    )
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
    // Ratatui expects the number of scroll positions, not the number of rows.
    let max_scroll = total.saturating_sub(visible);
    let mut state = ScrollbarState::new(max_scroll + 1)
        .position(scroll.min(max_scroll))
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
        BackendKind, BackendStatus, ComputeProvider, ComputeTargetAvailability,
        ComputeTargetStatus, EventEnvelope, IssueKey, IssueProvider, OutputStream, Repository,
        RepositoryRemote, RunEvent, RunState, RunSummary, SourceStatus, WorkspaceRef,
        WorktreeDeleteAction, WorktreeDeletePreview,
    };
    use chrono::{Duration, Utc};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
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
            pull_request: None,
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
            initialized: true,
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
            cpu_percent: Some(42.0),
            memory_percent: Some(57.0),
            sampled_at: Utc::now(),
            message: None,
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

    fn pr_snapshot() -> RuntimeSnapshot {
        let mut snapshot = normal_snapshot();
        let mut pr = issue("42", "Improve review flow");
        pr.pull_request = Some(agent_launcher_core::PullRequestMetadata {
            number: 42,
            additions: Some(128),
            deletions: None,
            base_ref: "main".into(),
            head_ref: "review-ui".into(),
            base_sha: "base-123".into(),
            head_sha: "head-456".into(),
            head_repository: Some("octocat/launcher".into()),
        });
        snapshot.issues.push(pr);
        snapshot
    }

    fn mouse(
        app: &mut AppState,
        snapshot: &RuntimeSnapshot,
        kind: MouseEventKind,
        x: u16,
        y: u16,
    ) -> bool {
        let size = (app.mouse.screen.width, app.mouse.screen.height);
        crate::mouse::handle_mouse(
            app,
            MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            },
            snapshot,
            size,
        )
    }

    fn mouse_snapshot() -> RuntimeSnapshot {
        let mut snapshot = pr_snapshot();
        let pr = snapshot.issues[1].clone();
        snapshot.issues = (1..=40)
            .map(|id| issue(&id.to_string(), &format!("Issue {id}")))
            .collect();
        snapshot.issues.extend((41..=80).map(|id| Issue {
            key: IssueKey {
                native_id: id.to_string(),
                ..pr.key.clone()
            },
            title: format!("Review {id}"),
            ..pr.clone()
        }));
        snapshot
    }

    #[test]
    fn mouse_tab_coordinates_preserve_independent_state() {
        let snapshot = mouse_snapshot();
        let mut app = AppState {
            selected: 8,
            scroll: 3,
            search_query: "Issue".into(),
            ..Default::default()
        };
        let buffer = render_buffer(80, 24, &snapshot, &mut app);
        for x in 4..12 {
            assert_eq!(buffer.cell((x, 4)).unwrap().bg, theme::primary());
            assert_eq!(buffer.cell((x, 4)).unwrap().fg, theme::bg());
        }
        for x in 14..19 {
            assert_eq!(buffer.cell((x, 4)).unwrap().bg, theme::element());
        }
        assert_eq!(app.mouse.tabs[0], (Rect::new(4, 4, 8, 1), InboxTab::Issues));
        assert_eq!(
            app.mouse.tabs[1],
            (Rect::new(14, 4, 5, 1), InboxTab::PullRequests)
        );
        let click = MouseEventKind::Down(MouseButton::Left);
        for (x, y) in [(3, 4), (12, 4), (13, 4), (19, 4), (14, 3), (14, 5)] {
            assert!(!mouse(&mut app, &snapshot, click, x, y));
        }
        assert!(!mouse(&mut app, &snapshot, click, 4, 4));
        assert!(mouse(&mut app, &snapshot, click, 16, 4));
        assert_eq!(app.tab, InboxTab::PullRequests);
        assert_eq!((app.selected, app.scroll), (0, 0));
        assert!(app.search_query.is_empty());
        app.search_query = "Review".into();
        app.issue_sort = IssueSort::Oldest;
        app.selected = 7;
        app.scroll = 4;
        let buffer = render_buffer(80, 24, &snapshot, &mut app);
        assert_eq!(buffer.cell((4, 4)).unwrap().bg, theme::element());
        for x in 14..19 {
            assert_eq!(buffer.cell((x, 4)).unwrap().bg, theme::primary());
        }
        assert_eq!(app.mouse.tabs[1].0, Rect::new(14, 4, 5, 1));
        assert!(mouse(&mut app, &snapshot, click, 9, 4));
        assert_eq!((app.selected, app.scroll), (8, 3));
        assert_eq!(app.search_query, "Issue");
        assert_eq!(app.issue_sort, IssueSort::Newest);
        render_buffer(80, 24, &snapshot, &mut app);
        assert!(mouse(&mut app, &snapshot, click, 14, 4));
        assert_eq!((app.selected, app.scroll), (7, 4));
        assert_eq!(app.search_query, "Review");
        assert_eq!(app.issue_sort, IssueSort::Oldest);
    }

    #[test]
    fn mouse_hover_selects_without_opening_and_keeps_immediate_click_valid() {
        let snapshot = mouse_snapshot();
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut app = AppState {
                tab,
                selected: 5,
                scroll: 5,
                ..Default::default()
            };
            render_buffer(80, 24, &snapshot, &mut app);
            let (rect, index, key) = app.mouse.rows[1].clone();
            let y = rect.bottom() - 1;
            assert!(mouse(&mut app, &snapshot, MouseEventKind::Moved, rect.x, y));
            assert_eq!(app.selected, index);
            assert_eq!(app.route, Route::Inbox);
            assert!(app.detail_issue_key.is_none());
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::Moved,
                rect.x,
                y
            ));
            let other_tab = app.mouse.tabs[usize::from(tab == InboxTab::Issues)].0;
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::Moved,
                other_tab.x,
                other_tab.y
            ));
            assert_eq!(app.tab, tab);
            assert_eq!(app.selected, index);
            assert!(mouse(
                &mut app,
                &snapshot,
                MouseEventKind::Down(MouseButton::Left),
                rect.x,
                y
            ));
            assert_eq!(app.detail_issue_key.as_ref(), Some(&key));
        }
    }

    #[test]
    fn mouse_rows_match_rendered_cells_and_scrolled_indices_at_responsive_sizes() {
        let snapshot = mouse_snapshot();
        for (width, height) in [(24, 9), (40, 14), (80, 24), (160, 60)] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                for scroll in [0, 5] {
                    let mut app = AppState {
                        tab,
                        scroll,
                        selected: scroll,
                        ..Default::default()
                    };
                    let buffer = render_buffer(width, height, &snapshot, &mut app);
                    for (rect, tab) in &app.mouse.tabs {
                        let text: String = (rect.x..rect.right())
                            .map(|x| buffer[(x, rect.y)].symbol())
                            .collect();
                        assert!(text.contains(tab.label()));
                    }
                    let rows = app.mouse.rows.clone();
                    let list = app.mouse.list;
                    assert_eq!(app.visible_rows, list.height as usize);
                    assert_eq!(rows.len(), app.visible_rows.min(40 - app.scroll));
                    for (screen_row, (rect, index, key)) in rows.iter().enumerate() {
                        assert_eq!(*index, app.scroll + screen_row);
                        assert_eq!(rect.height, 1);
                        assert_eq!(rect.y, list.y + screen_row as u16 * rect.height);
                        let text: String = (rect.x..rect.right())
                            .map(|x| buffer[(x, rect.y)].symbol())
                            .collect();
                        assert!(!text.trim().is_empty());
                        if width >= 40 {
                            assert!(text.contains(if tab == InboxTab::Issues {
                                "Issue"
                            } else {
                                "#42"
                            }));
                        }
                        for y in rect.y..rect.bottom() {
                            render_buffer(width, height, &snapshot, &mut app);
                            assert!(mouse(
                                &mut app,
                                &snapshot,
                                MouseEventKind::Down(MouseButton::Left),
                                rect.right() - 1,
                                y
                            ));
                            assert_eq!(app.selected, *index);
                            assert_eq!(app.detail_issue_key.as_ref(), Some(key));
                            assert!(app.dispatch_overlay.is_none());
                            app.reset_detail();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn mouse_wheel_scrolls_both_lists_without_wrapping_or_opening() {
        let snapshot = mouse_snapshot();
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut app = AppState {
                tab,
                ..Default::default()
            };
            for _ in 0..30 {
                render_buffer(80, 24, &snapshot, &mut app);
                let list = app.mouse.list;
                let old = app.scroll;
                let max = app.rows(&snapshot).len() - app.visible_rows;
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::ScrollDown,
                    list.x,
                    list.y
                ));
                assert_eq!(app.scroll, (old + 3).min(max));
                assert!((app.scroll..app.scroll + app.visible_rows).contains(&app.selected));
                assert_eq!(app.route, Route::Inbox);
            }
            for _ in 0..30 {
                render_buffer(80, 24, &snapshot, &mut app);
                let list = app.mouse.list;
                let old = app.scroll;
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::ScrollUp,
                    list.x,
                    list.y
                ));
                assert_eq!(app.scroll, old.saturating_sub(3));
            }
            assert_eq!(app.scroll, 0);
            app.search_query = "no matches".into();
            render_buffer(80, 24, &snapshot, &mut app);
            assert!(app.mouse.rows.is_empty());
            let list = app.mouse.list;
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::Down(MouseButton::Left),
                list.x,
                list.y
            ));
            assert!(mouse(
                &mut app,
                &snapshot,
                MouseEventKind::ScrollDown,
                list.x,
                list.y
            ));
            assert_eq!((app.selected, app.scroll), (0, 0));
        }
    }

    #[test]
    fn mouse_excludes_headers_gutters_blank_rows_and_stale_geometry() {
        let snapshot = normal_snapshot();
        let mut app = AppState::default();
        render_buffer(80, 24, &snapshot, &mut app);
        let row = app.mouse.rows[0].0;
        for (x, y) in [
            (row.x, row.y - 1),
            (row.x - 1, row.y),
            (row.right(), row.y),
            (row.x, row.bottom()),
            (0, 23),
        ] {
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::Down(MouseButton::Left),
                x,
                y
            ));
        }
        for (x, y) in [(row.x, row.y - 1), (row.x - 1, row.y), (0, 23)] {
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::ScrollDown,
                x,
                y
            ));
        }
        assert!(!crate::mouse::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE,
            },
            &snapshot,
            (81, 24)
        ));
        let mut changed = snapshot.clone();
        changed.issues[0].key.native_id = "replacement".into();
        assert!(!mouse(
            &mut app,
            &changed,
            MouseEventKind::Down(MouseButton::Left),
            row.x,
            row.y
        ));
        render_buffer(10, 4, &snapshot, &mut app);
        assert!(app.mouse.tabs.is_empty());
        assert!(app.mouse.rows.is_empty());
        assert_eq!(app.visible_rows, 0);
        assert!(!mouse(
            &mut app,
            &snapshot,
            MouseEventKind::Down(MouseButton::Left),
            row.x,
            row.y
        ));
    }

    #[test]
    fn mouse_detail_wheel_is_bounded_and_overlays_block_all_background_hits() {
        let mut snapshot = mouse_snapshot();
        for issue in &mut snapshot.issues {
            issue.description = Some("long description\n".repeat(100));
        }
        let mut app = AppState::default();
        for overlay in 0..5 {
            render_buffer(80, 24, &snapshot, &mut app);
            let row = app.mouse.rows[0].0;
            let tab = app.mouse.tabs[1].0;
            match overlay {
                0 => app.command_overlay = true,
                1 => app.sort_overlay = true,
                2 => {
                    app.dispatch_overlay = Some(crate::app::DispatchOverlay {
                        issue_key: snapshot.issues[0].key.clone(),
                        cursor: 0,
                        stage: DispatchStage::Prompt,
                    })
                },
                3 => {
                    app.input_overlay = Some(crate::app::InputOverlay {
                        run_id: "run".into(),
                        prompt: "input".into(),
                        text: String::new(),
                    })
                },
                _ => {
                    app.delete_overlay = Some(crate::app::DeleteOverlay {
                        preview: WorktreeDeletePreview {
                            run: RunSummary {
                                id: "run".into(),
                                issue_key: snapshot.issues[0].key.canonical(),
                                workspace: None,
                                agent: "opencode".into(),
                                state: RunState::Running,
                                message: None,
                                session_id: None,
                                started_at: Utc::now(),
                                updated_at: Utc::now(),
                            },
                            action: WorktreeDeleteAction::Delete,
                            has_uncommitted_changes: false,
                            has_ignored_files: false,
                            unpushed_commits: 0,
                            inspection_warning: None,
                            inspection_fingerprint: None,
                        },
                    })
                },
            }
            for kind in [
                MouseEventKind::Moved,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::ScrollDown,
            ] {
                for rect in [row, tab] {
                    assert!(!mouse(&mut app, &snapshot, kind, rect.x, rect.y));
                }
            }
            app = AppState::default();
        }
        assert!(app.open_detail(&snapshot));
        for _ in 0..50 {
            render_buffer(80, 24, &snapshot, &mut app);
            let detail = app.mouse.detail;
            let old = app.detail_scroll;
            assert!(!mouse(
                &mut app,
                &snapshot,
                MouseEventKind::ScrollDown,
                detail.x,
                detail.y - 2
            ));
            assert!(mouse(
                &mut app,
                &snapshot,
                MouseEventKind::ScrollDown,
                detail.x,
                detail.y
            ));
            assert_eq!(app.detail_scroll, (old + 3).min(app.detail_scroll_max));
        }
        assert_eq!(app.detail_scroll, app.detail_scroll_max);
        for _ in 0..50 {
            render_buffer(80, 24, &snapshot, &mut app);
            let detail = app.mouse.detail;
            assert!(mouse(
                &mut app,
                &snapshot,
                MouseEventKind::ScrollUp,
                detail.x,
                detail.y
            ));
        }
        assert_eq!(app.detail_scroll, 0);
        render_buffer(10, 4, &snapshot, &mut app);
        assert!(app.mouse.detail.is_empty());
    }

    #[test]
    fn pr_rows_show_metadata_and_state_without_leaking_into_issues() {
        let mut snapshot = pr_snapshot();
        let issues = render(112, 28, &snapshot, &mut AppState::default());
        assert!(issues.contains("Repair runtime dispatch"));
        assert!(!issues.contains("Improve review flow"));
        for state in ["open", "draft", "merged", "closed"] {
            snapshot.issues[1].state = state.into();
            for (width, height) in [(112, 48), (80, 24), (46, 16), (36, 12)] {
                let mut app = AppState {
                    tab: InboxTab::PullRequests,
                    ..AppState::default()
                };
                let text = render(width, height, &snapshot, &mut app);
                assert!(text.contains(" Issues    PRs "), "{text}");
                assert!(text.contains("#42"), "{text}");
                if width >= 80 {
                    assert!(text.contains("Improve review flow"), "{text}");
                    assert!(text.contains("octocat"), "{text}");
                }
                let row = text.lines().find(|line| line.contains("#42")).unwrap();
                assert!(row.contains("?□□□"), "{text}");
                assert!(row.contains(state), "{text}");
                assert!(!text.contains("+128 -?"), "{text}");
                assert!(!text.contains("Repair runtime dispatch"));
                if height == 48 {
                    assert!(text.contains("agent activity"));
                }
            }
        }
        let pr = snapshot.issues[1].pull_request.as_mut().unwrap();
        pr.additions = None;
        pr.deletions = Some(0);
        snapshot.issues[1].author = None;
        let text = render(80, 24, &snapshot, &mut AppState {
            tab: InboxTab::PullRequests,
            ..AppState::default()
        });
        assert!(text.contains("?□□□"));
        assert!(text.contains("unknown"));
    }

    #[test]
    fn pr_indicator_thresholds_ratios_and_extreme_counts() {
        for (additions, deletions, colors) in [
            (0, 0, "...."),
            (1, 0, "g..."),
            (0, 1, "r..."),
            (10, 0, "g..."),
            (11, 0, "gg.."),
            (100, 0, "gg.."),
            (101, 0, "ggg."),
            (1000, 0, "ggg."),
            (1001, 0, "gggg"),
            (0, 1001, "rrrr"),
            (5, 5, "g..."),
            (1, 9, "r..."),
            (1, 10, "gr.."),
            (99, 1, "gr.."),
            (50, 51, "grr."),
            (51, 50, "ggr."),
            (750, 251, "gggr"),
            (251, 750, "grrr"),
            (u64::MAX, 0, "gggg"),
            (0, u64::MAX, "rrrr"),
            (u64::MAX, 1, "gggr"),
            (1, u64::MAX, "grrr"),
            (u64::MAX, u64::MAX, "ggrr"),
        ] {
            let line = pr_change_indicator(Some(additions), Some(deletions));
            assert_eq!(line.width(), 4);
            for (span, color) in line.spans.iter().zip(colors.chars()) {
                assert_eq!(
                    span.content,
                    if color == '.' {
                        "□"
                    } else {
                        "■"
                    }
                );
                assert_eq!(
                    span.style.fg,
                    Some(match color {
                        'g' => theme::done(),
                        'r' => theme::error(),
                        _ => theme::muted(),
                    }),
                    "+{additions} -{deletions}: {colors}"
                );
            }
        }
        for counts in [
            (None, None),
            (Some(0), None),
            (None, Some(0)),
            (Some(u64::MAX), None),
        ] {
            let line = pr_change_indicator(counts.0, counts.1);
            assert_eq!(line.to_string(), "?□□□");
            assert_eq!(line.width(), 4);
            assert_eq!(line.style.fg, Some(theme::muted()));
        }
    }

    #[test]
    fn pr_columns_align_headers_and_preserve_context_at_narrow_widths() {
        let mut snapshot = pr_snapshot();
        snapshot.issues[1].title = "界 long title ".repeat(20);
        snapshot.issues[1].author = Some("界very-long-author-name".repeat(10));
        let pr = snapshot.issues[1].pull_request.as_mut().unwrap();
        pr.additions = Some(100);
        pr.deletions = Some(1);
        for width in [36, 46, 80, 112] {
            let mut app = AppState {
                tab: InboxTab::PullRequests,
                ..Default::default()
            };
            let buffer = render_buffer(width, 24, &snapshot, &mut app);
            let row = app.mouse.rows[0].0;
            let columns = pr_columns(row, 3);
            for (column, label) in columns.iter().zip(["P", "t", "a", "d"]) {
                assert_eq!(buffer[(column.x, row.y - 1)].symbol(), label);
            }
            assert_eq!(buffer[(row.right() - 1, row.y - 1)].symbol(), "s");
            assert_eq!(columns[3].width, 4);
            assert_eq!(columns[4].width, 6);
            assert!(columns[2].width <= 12);
            assert_eq!(buffer[(columns[0].x, row.y)].symbol(), "#");
            assert_eq!(buffer[(columns[3].x, row.y)].fg, theme::done());
            assert_eq!(buffer[(columns[3].x + 2, row.y)].fg, theme::error());
            assert_eq!(buffer[(columns[3].x + 3, row.y)].symbol(), "□");
            assert_eq!(buffer[(columns[4].right() - 4, row.y)].symbol(), "o");
            assert_eq!(buffer[(row.right() - 1, row.y)].symbol(), "n");
            for x in row.x..row.right() {
                // Ratatui resets the continuation cell of a wide glyph.
                if x > row.x && Line::raw(buffer[(x - 1, row.y)].symbol()).width() > 1 {
                    continue;
                }
                assert_eq!(
                    buffer[(x, row.y)].bg,
                    theme::element(),
                    "width {width}, x {x}"
                );
            }
        }
        for width in 0..36 {
            let area = Rect::new(3, 2, width, 1);
            for column in pr_columns(area, 21) {
                assert!(column.right() <= area.right());
            }
        }
    }

    #[test]
    fn inbox_states_stay_right_aligned_with_unicode_titles_and_run_states() {
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut snapshot = pr_snapshot();
            let template = snapshot.issues[usize::from(tab == InboxTab::PullRequests)].clone();
            snapshot.issues = [
                "Short",
                "Ellipsis… and café",
                "Mixed 界 title 界",
                "Combining e\u{301} title",
                "界…e\u{301} long title ",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, title)| Issue {
                key: IssueKey {
                    native_id: index.to_string(),
                    ..template.key.clone()
                },
                title: if index == 4 {
                    title.repeat(30)
                } else {
                    title.into()
                },
                author: Some(title.repeat(index + 1)),
                ..template.clone()
            })
            .collect();
            for run_state in [
                None,
                Some(RunState::Running),
                Some(RunState::NeedsInput),
                Some(RunState::Disconnected),
            ] {
                snapshot.runs = run_state.map_or_else(Vec::new, |state| {
                    snapshot
                        .issues
                        .iter()
                        .map(|issue| RunSummary {
                            id: issue.key.native_id.clone(),
                            issue_key: issue.key.canonical(),
                            workspace: None,
                            agent: "opencode".into(),
                            state,
                            message: None,
                            session_id: None,
                            started_at: Utc::now(),
                            updated_at: Utc::now(),
                        })
                        .collect()
                });
                for width in [36, 46, 54, 72, 80, 112] {
                    let mut app = AppState {
                        tab,
                        ..Default::default()
                    };
                    let buffer = render_buffer(width, 24, &snapshot, &mut app);
                    assert_eq!(app.mouse.rows.len(), snapshot.issues.len());
                    let right = app.mouse.rows[0].0.right();
                    for (row, ..) in &app.mouse.rows {
                        assert_eq!(row.right(), right);
                        let columns = Columns::for_width(row.width);
                        let expected = if tab == InboxTab::PullRequests {
                            "open".to_owned()
                        } else if columns.state <= 2 {
                            run_state
                                .map_or_else(
                                    || issue_icon("open"),
                                    |state| {
                                        if state.needs_attention() {
                                            "!"
                                        } else {
                                            "●"
                                        }
                                    },
                                )
                                .to_owned()
                        } else {
                            truncate(run_state.map_or("open", run_label), columns.state)
                        };
                        let start = right - Line::raw(expected.as_str()).width() as u16;
                        let actual: String = (start..right)
                            .map(|x| buffer[(x, row.y)].symbol())
                            .collect();
                        assert_eq!(actual, expected, "{tab:?}, width {width}, {run_state:?}");
                    }
                    let header_y = app.mouse.rows[0].0.y - 1;
                    let label = if tab == InboxTab::PullRequests {
                        "status"
                    } else if Columns::for_width(app.mouse.rows[0].0.width).state <= 2 {
                        "st"
                    } else {
                        "state"
                    };
                    let actual: String = (right - label.len() as u16..right)
                        .map(|x| buffer[(x, header_y)].symbol())
                        .collect();
                    assert_eq!(actual, label);
                }
            }
        }
    }

    #[test]
    fn pr_detail_has_comparison_context_and_explicit_review_controls() {
        let snapshot = pr_snapshot();
        let mut app = AppState {
            tab: InboxTab::PullRequests,
            ..AppState::default()
        };
        assert!(app.open_detail(&snapshot));
        let text = render(112, 48, &snapshot, &mut app);
        for expected in [
            "Pull Request",
            "main (base-123)",
            "review-ui (head-456)",
            "octocat/launcher",
            "+128 -?",
            "d review PR",
            "Opening details does not start a review",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("d dispatch"));
        assert!(app.dispatch_overlay.is_none());
        assert!(app.status_message.is_none());
    }

    #[test]
    fn pr_command_and_target_overlays_label_review_not_issue_dispatch() {
        let mut snapshot = pr_snapshot();
        let mut app = AppState {
            tab: InboxTab::PullRequests,
            command_overlay: true,
            ..AppState::default()
        };
        let text = render(90, 28, &snapshot, &mut app);
        assert!(text.contains("review selected PR"));
        assert!(text.contains("Tab / BackTab"));
        assert!(!text.contains("dispatch selected issue"));
        app.command_overlay = false;
        snapshot.selected_backend = Some(BackendKind::Native);
        snapshot.compute_targets = vec![compute_target(
            "ready",
            ComputeTargetAvailability::Online,
            0,
            Some(2),
        )];
        app.dispatch_overlay = Some(crate::app::DispatchOverlay {
            issue_key: snapshot.issues[1].key.clone(),
            cursor: 0,
            stage: DispatchStage::Target { profile: None },
        });
        let text = render(90, 28, &snapshot, &mut app);
        assert!(text.contains("Review PR: compute target"));
        assert!(text.contains("Enter review PR"));
        assert!(text.contains("Automatic"));
        assert!(!text.contains("Enter dispatch"));
        app.status_message = Some("Target ready is full".into());
        assert!(render(90, 28, &snapshot, &mut app).contains("Target ready is full"));
    }

    #[test]
    fn pr_empty_loading_error_and_filter_states_are_distinct() {
        let mut snapshot = normal_snapshot();
        let mut app = AppState {
            tab: InboxTab::PullRequests,
            ..AppState::default()
        };
        let text = render(90, 28, &snapshot, &mut app);
        assert!(text.contains("No PRs."));
        snapshot.initialized = false;
        assert!(render(90, 28, &snapshot, &mut app).contains("Loading PRs..."));
        snapshot.error = Some("GitHub unavailable".into());
        let text = render(90, 28, &snapshot, &mut app);
        assert!(text.contains("Could not load sources"));
        assert!(text.contains("GitHub unavailable"));
        snapshot = pr_snapshot();
        app.search_query = "zzzzzzzz".into();
        assert!(render(90, 28, &snapshot, &mut app).contains("No matching PRs"));
        for (width, height) in [(0, 0), (1, 1), (18, 5), (24, 9), (36, 12)] {
            render(width, height, &snapshot, &mut app);
        }
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
    fn idle_agents_do_not_look_like_they_are_working() {
        let mut snapshot = normal_snapshot();
        let now = Utc::now();
        snapshot.runs.push(RunSummary {
            id: "run-idle".to_owned(),
            issue_key: snapshot.issues[0].key.canonical(),
            workspace: None,
            agent: "opencode".to_owned(),
            state: RunState::Idle,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        });
        let mut app = AppState::default();

        let text = render(112, 28, &snapshot, &mut app);
        let issue_line = text
            .lines()
            .find(|line| line.contains("Repair runtime dispatch"))
            .expect("issue should be visible");

        assert!(issue_line.contains("idle"));
        assert!(
            theme::BRAILLE_SPINNER
                .iter()
                .all(|spinner| !issue_line.contains(spinner))
        );
        assert!(text.contains("● ready"));
        assert!(!text.contains("1 working"));
    }

    #[test]
    fn scrollbar_thumb_reaches_both_ends_of_the_list() {
        let mut terminal = Terminal::new(TestBackend::new(8, 12)).unwrap();
        for scroll in [0, 6, 13] {
            terminal
                .draw(|frame| render_scrollbar(frame, frame.area(), 24, 11, scroll))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let thumb: Vec<_> = (1..12)
                .filter(|&y| buffer[(7, y)].bg == theme::border())
                .collect();
            assert!(!thumb.is_empty());
            match scroll {
                0 => assert_eq!(thumb.first(), Some(&1)),
                13 => assert_eq!(thumb.last(), Some(&11)),
                _ => {
                    assert!(thumb[0] > 1);
                    assert!(*thumb.last().unwrap() < 11);
                },
            }
        }
    }

    #[test]
    fn both_inbox_tabs_scroll_the_thumb_to_the_last_visible_row() {
        let snapshot = mouse_snapshot();
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut app = AppState {
                tab,
                selected: 39,
                ..Default::default()
            };
            let buffer = render_buffer(80, 24, &snapshot, &mut app);
            let list = app.mouse.list;
            assert_eq!(app.scroll + app.visible_rows, app.rows(&snapshot).len());
            assert_eq!(
                buffer[(list.right() - 1, list.bottom() - 1)].bg,
                theme::border()
            );
            assert_eq!(buffer[(list.right() - 1, list.y)].bg, theme::element());
        }
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
        assert!(text.contains("Search issues…"));
        assert!(text.contains(" Issues    PRs   1 source · newest first"));
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
        assert!(!text.contains("Tab/BackTab"));
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
        for label in ["-15m", "-10m", "-5m", "now"] {
            assert!(before.contains(label));
        }
        assert_ne!(before, after);
        assert!(after.contains("1 working · 15m"));
        assert!(after.contains("Repair runtime dispatch"));
    }

    #[test]
    fn demo_activity_scrolls_and_keeps_scores_bounded() {
        assert!(demo_activity(0, 0).is_empty());
        for width in [1, 32, 90, 180] {
            let before = demo_activity(width, 0);
            assert_eq!(before, demo_activity(width, 11));
            let after = demo_activity(width, 12);
            assert_eq!(before[1..], after[..after.len() - 1]);
            assert!(
                demo_activity(width, u32::MAX)
                    .iter()
                    .all(|score| *score <= 100)
            );
        }
    }

    #[test]
    fn empty_search_shows_placeholder_with_block_cursor() {
        let text = render(112, 48, &normal_snapshot(), &mut AppState::default());
        let line = text
            .lines()
            .find(|line| line.contains("Search issues…"))
            .unwrap();
        assert!(!line.contains("type to filter"));
        assert!(!line.contains('█'));
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            for overlay in [false, true] {
                let app = AppState {
                    tab,
                    command_overlay: overlay,
                    ..Default::default()
                };
                terminal
                    .draw(|frame| draw_search(frame, frame.area(), &app))
                    .unwrap();
                let cell = terminal.backend().buffer().cell((0, 0)).unwrap();
                assert_eq!(cell.symbol(), "S");
                if overlay {
                    assert_ne!(cell.bg, theme::text());
                } else {
                    assert_eq!(cell.bg, theme::text());
                    assert_eq!(cell.fg, theme::panel());
                }
            }
        }
    }

    #[test]
    fn search_cursor_is_hidden_under_an_overlay() {
        let mut app = AppState {
            search_query: "Repair".to_owned(),
            command_overlay: true,
            ..AppState::default()
        };
        let text = render(112, 48, &normal_snapshot(), &mut app);
        assert!(!text.contains("Repair█"));
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
    fn dispatch_overlay_lists_prompt_profiles_for_the_stable_issue() {
        let mut snapshot = normal_snapshot();
        snapshot.prompt_profiles = vec![
            "designer".to_owned(),
            "implementer".to_owned(),
            "reviewer".to_owned(),
        ];
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 1,
                stage: crate::app::DispatchStage::Prompt,
            }),
            ..AppState::default()
        };

        let text = render(90, 28, &snapshot, &mut app);

        assert!(text.contains("Choose prompt"));
        assert!(text.contains("Repair runtime dispatch"));
        assert!(text.contains("designer"));
        assert!(text.contains("implementer"));
        assert!(text.contains("reviewer"));
        assert!(text.contains("Enter dispatch"));
    }

    #[test]
    fn dispatch_overlay_keeps_large_and_compact_selections_visible() {
        let mut snapshot = normal_snapshot();
        snapshot.prompt_profiles = (0..20).map(|index| format!("prompt-{index}")).collect();
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 15,
                stage: crate::app::DispatchStage::Prompt,
            }),
            ..AppState::default()
        };

        let normal = render(70, 12, &snapshot, &mut app);
        assert!(normal.contains("Choose prompt · 16/20"));
        assert!(normal.contains("prompt-15"));

        let compact = render(40, 5, &snapshot, &mut app);
        assert!(compact.contains("Prompt"));
        assert!(compact.contains("prompt-15"));
        assert!(compact.contains("Enter dispatch"));
    }

    #[test]
    fn target_overlay_lists_automatic_and_all_target_statuses() {
        let mut snapshot = normal_snapshot();
        snapshot.selected_backend = Some(BackendKind::Native);
        snapshot.compute_targets = vec![
            compute_target("ready", ComputeTargetAvailability::Online, 1, Some(3)),
            compute_target("offline", ComputeTargetAvailability::Offline, 0, Some(2)),
            compute_target("full", ComputeTargetAvailability::Online, 2, Some(2)),
        ];
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 0,
                stage: crate::app::DispatchStage::Target {
                    profile: Some("reviewer".to_owned()),
                },
            }),
            ..AppState::default()
        };

        let text = render(100, 28, &snapshot, &mut app);

        assert!(text.contains("Choose compute target"));
        assert!(text.contains("Repair runtime dispatch · reviewer"));
        assert!(text.contains("Automatic  ready"));
        assert!(text.contains("Target ready  online · 1/3 active · CPU 42% · MEM 57%"));
        assert!(text.contains("Target offline  offline · 0/2 active · CPU 42% · MEM 57%"));
        assert!(text.contains("Target full  full · 2/2 active · CPU 42% · MEM 57%"));
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
            initialized: true,
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
        assert!(text.contains("Search issues…"));
        assert!(text.contains(" Issues    PRs "));
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
