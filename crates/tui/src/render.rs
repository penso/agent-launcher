use std::collections::HashSet;

use agent_launcher_core::{
    BackendKind, ComputeTargetAvailability, ComputeTargetStatus, Issue, RunState, RuntimeSnapshot,
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Margin, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Sparkline, Widget,
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
    widgets::{BrailleSparkline, SparklineSample, SparklineVariant, render_bottom_edge},
};

pub(crate) fn draw(frame: &mut Frame<'_>, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    app.security_confirmation_visible = false;
    app.activity_history_origin = crate::activity::history_origin(
        &snapshot.herdr_activity,
        chrono::Utc::now(),
        app.activity_history_origin,
    );
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(theme::bg())), area);
    app.reconcile_detail(snapshot);
    app.reconcile_dispatch(snapshot);
    app.mouse = crate::mouse::MouseGeometry {
        screen: area,
        route: app.route,
        tab: app.tab,
        blocked: app.input_overlay.is_some()
            || app.issue_delete_overlay.is_some()
            || app.delete_overlay.is_some()
            || app.dispatch_overlay.is_some()
            || app.command_overlay
            || app.debug_overlay
            || app.sort_overlay
            || app.away_overlay.is_some()
            || app.away_quit,
        ..Default::default()
    };
    app.visible_rows = 0;
    let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let overlay_area = content;
    match app.route {
        Route::Inbox => draw_inbox(frame, content, snapshot, app),
        Route::Detail => draw_detail(frame, content, snapshot, app),
    }
    if app.debug_overlay {
        draw_debug_overlay(frame, overlay_area, snapshot, app);
    } else if app.dispatch_overlay.is_some() {
        draw_dispatch_overlay(frame, overlay_area, snapshot, app);
    } else if app.sort_overlay {
        draw_sort_overlay(frame, overlay_area, app);
    } else if app.command_overlay {
        draw_command_overlay(frame, overlay_area, app);
    }
    app.issue_delete_confirmation_visible = false;
    if app.input_overlay.is_some() {
        crate::detail::draw_input_overlay(frame, overlay_area, app);
    }
    if app.delete_overlay.is_some() {
        crate::detail::draw_delete_overlay(frame, overlay_area, app);
    }
    if app.issue_delete_overlay.is_some() {
        crate::detail::draw_issue_delete_overlay(frame, overlay_area, app);
    }
    if area.height > 0 {
        let mut footer_area = area;
        if app.mouse.mode.is_empty() {
            let button_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
            app.mouse.mode = draw_mode_button(frame, button_area, snapshot);
            footer_area.width = app.mouse.mode.x.saturating_sub(area.x + 1);
        }
        draw_footer(frame, footer_area, snapshot, &app.host_metrics, app.tab);
    }
    app.away_quit_visible = false;
    if app.away_overlay.is_some() || app.away_quit {
        crate::away::draw(frame, overlay_area, snapshot, app);
    }
    app.mouse.scroll = app.scroll;
}

fn draw_mode_button(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot) -> Rect {
    use agent_launcher_core::AppMode;

    let away = &snapshot.away;
    let active = away.mode == AppMode::Away;
    let mode = if active {
        "Away"
    } else {
        "Manual"
    };
    let Some(label) = [format!(" {mode} "), mode.to_owned()]
        .into_iter()
        .find(|label| label.len() <= usize::from(area.width))
    else {
        return Rect::default();
    };
    if area.height == 0 {
        return Rect::default();
    }
    let button = Rect::new(
        area.right() - label.len() as u16,
        area.y,
        label.len() as u16,
        1,
    );
    let style = if active {
        Style::new().fg(theme::bg()).bg(theme::primary()).bold()
    } else {
        Style::new().fg(theme::text()).bg(theme::element())
    };
    frame.render_widget(Paragraph::new(label).style(style), button);
    button
}

fn draw_inbox(frame: &mut Frame<'_>, area: Rect, snapshot: &RuntimeSnapshot, app: &mut AppState) {
    if area.width < 24 || area.height < 7 {
        draw_tiny_inbox(frame, area, snapshot, app);
        return;
    }

    let body = area;
    let horizontal_margin = if body.width >= 80 {
        2
    } else {
        1
    };
    let panel_width = app
        .layout
        .content_width(body.width.saturating_sub(horizontal_margin * 2));
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
    let top_space = body.height.saturating_sub(panel_height) / 2;
    let (top_space, panel_height) = if app.layout == crate::LayoutMode::Flexible {
        // Keep activity compact on tall screens and give the remaining height to rows.
        let top_space = if panel_width >= 48 {
            top_space.min(8)
        } else {
            0
        };
        (top_space, body.height.saturating_sub(top_space))
    } else {
        (top_space, panel_height)
    };
    let panel = Rect::new(
        body.x + body.width.saturating_sub(panel_width) / 2,
        body.y + top_space,
        panel_width,
        panel_height,
    );

    let show_activity = panel.width >= 48 && top_space >= 7;
    if show_activity {
        let chart_height = top_space.saturating_sub(2).min(10);
        let chart = Rect::new(
            panel.x,
            body.y + top_space.saturating_sub(chart_height) / 2,
            panel.width,
            chart_height,
        );
        draw_agent_activity(frame, chart, app, snapshot);
    }

    let listing_y = if show_activity {
        panel.y
    } else {
        let logo = Rect::new(panel.x, panel.y, panel.width, logo_height);
        draw_logo(frame, logo, full_logo);
        logo.bottom() + logo_gap
    };
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
    let status = app.visible_status(snapshot);
    let message = if let Some(status) = status.as_deref() {
        status
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

fn draw_agent_activity(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &AppState,
    snapshot: &RuntimeSnapshot,
) {
    let now = chrono::Utc::now();
    let demo_elapsed = app.demo_started.map(|start| start.elapsed());
    let demo = demo_elapsed.is_some();
    let generated;
    let activity = if demo {
        generated = crate::activity::demo_snapshot(demo_elapsed.unwrap(), now);
        &generated
    } else {
        &snapshot.herdr_activity
    };
    draw_activity_panel(
        frame,
        area,
        activity,
        now,
        demo,
        if demo {
            None
        } else {
            app.activity_history_origin
        },
        demo_elapsed,
    );
}

fn draw_activity_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &agent_launcher_core::HerdrActivitySnapshot,
    now: chrono::DateTime<chrono::Utc>,
    demo: bool,
    origin: Option<chrono::DateTime<chrono::Utc>>,
    demo_elapsed: Option<std::time::Duration>,
) {
    use agent_launcher_core::ActivityCompleteness;

    if area.width < 8 || area.height < 4 {
        return;
    }
    let panel = Block::new()
        .style(Style::new().bg(theme::panel()))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    let current = snapshot
        .enabled
        .then(|| crate::activity::current_sample(snapshot, now))
        .flatten();
    let incomplete = snapshot.discovery_error.is_some()
        || current.is_some_and(|s| {
            !s.inventory_complete || s.completeness != ActivityCompleteness::Complete
        })
        || (snapshot.discover_remote_sessions && (snapshot.discovering || current.is_none()));
    let scope = if !snapshot.enabled {
        "disabled".to_owned()
    } else if let Some(sample) = current {
        format!(
            "{}/{} {}",
            sample.fresh_endpoints,
            sample.expected_endpoints,
            if sample.completeness == ActivityCompleteness::Missing || sample.counts.is_none() {
                "missing"
            } else if incomplete {
                "partial"
            } else {
                "sessions"
            }
        )
    } else {
        "unobserved".to_owned()
    };
    let data = if let Some(elapsed) = demo_elapsed {
        crate::activity::demo_buckets(elapsed, usize::from(inner.width) * 2)
    } else {
        crate::activity::buckets(snapshot, usize::from(inner.width) * 2, now)
    };
    let peak = data
        .iter()
        .filter_map(|bucket| bucket.working)
        .max()
        .unwrap_or(0);
    // A stable demo scale avoids vertical pumping as peaks enter or leave the window.
    // Fixed-point values are presentation-only; labels remain in agents.
    let peak = if demo_elapsed.is_some() {
        8
    } else {
        peak
    };
    let counts = current
        .filter(|s| s.completeness != ActivityCompleteness::Missing)
        .and_then(|s| s.counts.as_ref());
    let prefix = if current
        .is_some_and(|s| s.completeness == ActivityCompleteness::Partial || !s.inventory_complete)
    {
        ">="
    } else {
        ""
    };
    let mut statuses = Vec::new();
    if let Some(c) = counts {
        if c.blocked > 0 {
            statuses.push(format!(
                "{prefix}{} working | {} blocked",
                c.working, c.blocked
            ));
        }
        statuses.push(format!("{prefix}{} working", c.working));
        statuses.push(format!(
            "{prefix}{}w",
            compact_activity_count(u128::from(c.working))
        ));
    }
    statuses.push(String::new());
    // Fit both pieces before drawing: right alignment must never erase scope or the demo label.
    let mut header = (String::new(), String::new());
    'fit: for status in statuses {
        for title in ["Herdr activity", "Herdr", ""] {
            let label = [
                title,
                if demo {
                    "demo"
                } else {
                    ""
                },
                &scope,
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
            if label.len() + status.len() + usize::from(!status.is_empty())
                <= usize::from(inner.width)
            {
                header = (label, status);
                break 'fit;
            }
        }
    }
    if header.0.is_empty() {
        header.0 = if demo {
            format!("demo | {scope}")
        } else {
            scope
        };
    }
    let status_width = header.1.len() as u16;
    frame.render_widget(
        Paragraph::new(header.0).style(Style::new().fg(theme::secondary())),
        Rect::new(
            inner.x,
            inner.y,
            inner
                .width
                .saturating_sub(status_width + u16::from(status_width > 0)),
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(header.1).style(Style::new().fg(theme::text())),
        Rect::new(inner.right() - status_width, inner.y, status_width, 1),
    );
    let show_labels = inner.height >= 5 && inner.width >= 16;
    let graph = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1 + u16::from(show_labels)),
    );
    let samples = data
        .iter()
        .map(|bucket| SparklineSample {
            value: bucket.working,
            partial: bucket.partial,
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        BrailleSparkline::new(&samples)
            .max(if demo_elapsed.is_some() {
                peak * crate::activity::DEMO_PRECISION
            } else {
                peak
            })
            .style(Style::new().fg(theme::primary()).bg(theme::panel()))
            .variant(SparklineVariant::Line),
        graph,
    );
    let leading = if let Some(elapsed) = demo_elapsed {
        crate::activity::demo_unobserved_columns(elapsed, graph.width.into())
    } else {
        crate::activity::unobserved_columns(snapshot, graph.width.into(), now, origin)
    } as u16;
    if leading > 0 && !graph.is_empty() {
        let full = graph.width >= 62 && graph.height >= 2;
        let lines = if full {
            (0..2)
                .map(|i| format!("{}   {}", theme::AGENT_LOGO[i], theme::LAUNCHER_LOGO[i]))
                .collect::<Vec<_>>()
        } else {
            vec!["agent launcher".to_owned()]
        };
        let width = lines[0].chars().count() as u16;
        let logo = Rect::new(
            graph.x + graph.width.saturating_sub(width) / 2,
            graph.y + (graph.height - lines.len() as u16) / 2,
            width.min(graph.width),
            lines.len() as u16,
        );
        let mut buffer = Buffer::empty(logo);
        Paragraph::new(lines.join("\n"))
            .style(Style::new().fg(theme::muted()).bg(theme::panel()).bold())
            .render(logo, &mut buffer);
        // Crop whole cells, never half of a Braille column or any recorded gap.
        for y in logo.y..logo.bottom() {
            for x in logo.x..logo.right().min(graph.x + leading) {
                frame.buffer_mut()[(x, y)] = buffer[(x, y)].clone();
            }
        }
    }
    if show_labels {
        let peak_label = format!("peak {peak} agents");
        for (label, alignment) in [
            ("-15m", Alignment::Left),
            (peak_label.as_str(), Alignment::Center),
            ("now", Alignment::Right),
        ] {
            if alignment == Alignment::Center && label.len() + 10 > usize::from(inner.width) {
                continue;
            }
            let width = label.len() as u16;
            let x = match alignment {
                Alignment::Left => inner.x,
                Alignment::Center => inner.x + (inner.width - width) / 2,
                Alignment::Right => inner.right() - width,
            };
            frame.render_widget(
                Paragraph::new(label).style(Style::new().fg(theme::muted())),
                Rect::new(x, graph.bottom(), width, 1),
            );
        }
    }
}

fn draw_search(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    if area.is_empty() {
        return;
    }
    let focused = !app.command_overlay
        && !app.debug_overlay
        && !app.sort_overlay
        && app.dispatch_overlay.is_none()
        && app.input_overlay.is_none()
        && app.delete_overlay.is_none();
    if app.search_query.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.tab == InboxTab::Security {
                "Search private advisories (GHSA / CVE / severity)..."
            } else if app.tab == InboxTab::PullRequests {
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

    let metadata = app.issue_sort.label();
    let mut tabs = Vec::new();
    let mut x = inner.x;
    let compact = inner.width < 27;
    for tab in InboxTab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw(if compact {
                " "
            } else {
                "  "
            }));
            x += if compact {
                1
            } else {
                2
            };
        }
        let label = if compact {
            tab.label().to_owned()
        } else {
            format!(" {} ", tab.label())
        };
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
    // Reserve room for either mode so switching does not move the control.
    let minimum = " Manual ".len();
    let available = inner.right().saturating_sub(x + 1);
    if usize::from(available) >= minimum {
        app.mouse.mode = draw_mode_button(frame, Rect::new(x + 1, inner.y, available, 1), snapshot);
    }
    let tabs_right = if app.mouse.mode.is_empty() {
        inner.right()
    } else {
        app.mouse.mode.x - 1
    };
    tabs.push(Span::styled(
        if inner.width >= 62 && usize::from(tabs_right.saturating_sub(x)) >= metadata.len() + 2 {
            format!("  {metadata}")
        } else {
            String::new()
        },
        Style::new().fg(theme::muted()),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(tabs)),
        Rect::new(inner.x, inner.y, tabs_right.saturating_sub(inner.x), 1),
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
    if app.tab == InboxTab::Security {
        for (column, label) in security_columns(Rect::new(area.x, area.y, table_width, 1))
            .into_iter()
            .zip(["", "age", "severity", "title (private)", "state"])
        {
            frame.render_widget(
                Paragraph::new(label)
                    .style(Style::new().fg(theme::muted()))
                    .alignment(if label == "state" {
                        Alignment::Right
                    } else {
                        Alignment::Left
                    }),
                column,
            );
        }
    } else if app.tab == InboxTab::PullRequests {
        for (column, label) in pr_columns
            .into_iter()
            .zip(["PR", "title", "author", "diff", "status", "activity"])
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
        if let Some(advisory) = &issue.security_advisory {
            let style = Style::new().fg(theme::text()).bg(if index == app.selected {
                theme::element()
            } else {
                theme::panel()
            });
            frame.render_widget(Block::new().style(style), row_area);
            let severity = advisory.severity.as_deref().unwrap_or("unknown");
            let severity_color = match severity.to_ascii_lowercase().as_str() {
                "critical" => theme::error(),
                "high" => theme::primary(),
                "medium" => theme::secondary(),
                _ => theme::muted(),
            };
            let state_color = match issue.state.to_ascii_lowercase().as_str() {
                "triage" => theme::primary(),
                "draft" => theme::secondary(),
                "published" | "closed" => theme::done(),
                _ => theme::muted(),
            };
            let age = age_label(issue.created_at);
            for (index, (column, (text, color))) in security_columns(row_area)
                .into_iter()
                .zip([
                    (
                        if index == app.selected {
                            "▶"
                        } else {
                            " "
                        },
                        theme::primary(),
                    ),
                    (age.as_str(), theme::muted()),
                    (severity, severity_color),
                    (issue.title.as_str(), theme::text()),
                    (issue.state.as_str(), state_color),
                ])
                .enumerate()
            {
                frame.render_widget(
                    Paragraph::new(truncate(text, column.width as usize))
                        .style(style.fg(color))
                        .alignment(if index == 4 {
                            Alignment::Right
                        } else {
                            Alignment::Left
                        }),
                    column,
                );
            }
            continue;
        }
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

fn security_columns(area: Rect) -> [Rect; 5] {
    let marker = area.width.min(2);
    let remaining = area.width - marker;
    let age = 6.min(remaining / 3);
    let remaining = remaining - age;
    let state = 10.min(remaining / 2);
    let severity = if area.width >= 36 {
        9
    } else {
        0
    };
    let title = remaining - state - severity;
    let mut x = area.x;
    [marker, age, severity, title, state].map(|width| {
        // Leave a cell between content columns, even when the title fills its space.
        let content_width = if x + width < area.right() {
            width.saturating_sub(1)
        } else {
            width
        };
        let column = Rect::new(x, area.y, content_width, area.height);
        x += width;
        column
    })
}

#[derive(Clone, Copy)]
struct Columns {
    activity: usize,
    age: usize,
    source: usize,
    state: usize,
}

impl Columns {
    fn for_width(width: u16) -> Self {
        if width >= 72 {
            Self {
                activity: 8,
                age: 6,
                source: 8,
                state: 12,
            }
        } else if width >= 46 {
            Self {
                activity: 0,
                age: 5,
                source: 5,
                state: 8,
            }
        } else {
            Self {
                activity: 0,
                age: 4,
                source: 3,
                state: 2,
            }
        }
    }

    fn title(self, width: u16) -> usize {
        width.saturating_sub(
            4 + self.age as u16 + self.source as u16 + self.state as u16 + self.activity as u16,
        ) as usize
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
    let activity_width = columns.activity as u16;
    frame.render_widget(
        Paragraph::new("activity").style(Style::new().fg(theme::muted())),
        Rect::new(
            area.right() - state_width - activity_width,
            area.y,
            activity_width,
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width - state_width - activity_width, 1),
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
    let activity_width = columns.activity as u16;
    frame.render_widget(
        Paragraph::new(item_activity_line(issue, false)).style(Style::new().bg(bg)),
        Rect::new(
            area.right() - state_width - activity_width,
            area.y,
            activity_width,
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(line),
        Rect::new(area.x, area.y, area.width - state_width - activity_width, 1),
    );
    frame.render_widget(
        Paragraph::new(truncate(state, state_width as usize))
            .style(Style::new().fg(state_color).bg(bg))
            .alignment(Alignment::Right),
        Rect::new(area.right() - state_width, area.y, state_width, 1),
    );
}

fn pr_columns(area: Rect, number_width: u16) -> [Rect; 6] {
    // Reserve identity, diff, and lifecycle before sharing the rest with title/author.
    let available = area.width.saturating_sub(6);
    let number = number_width.min(available);
    let status = 6.min(available.saturating_sub(number));
    let diff_cells = match area.width {
        160.. => 8,
        120.. => 6,
        _ => 4,
    };
    let diff = diff_cells.min(available.saturating_sub(number + status));
    let remaining = available.saturating_sub(number + status + diff);
    let activity = match area.width {
        100.. => 15,
        76.. => 8,
        _ => 0,
    }
    .min(remaining.saturating_sub(24));
    let remaining = remaining.saturating_sub(activity + u16::from(activity > 0));
    let author = (remaining / 3).min(12);
    let widths = [number, remaining - author, author, activity, diff, status];
    let mut x = area.x.saturating_add(2).min(area.right());
    let rects = widths.map(|width| {
        let rect = Rect::new(x, area.y, width.min(area.right() - x), 1);
        x = rect
            .right()
            .saturating_add(u16::from(width > 0))
            .min(area.right());
        rect
    });
    [rects[0], rects[1], rects[2], rects[4], rects[5], rects[3]]
}

fn compact_activity_count(count: u128) -> String {
    for (scale, suffix) in [
        (1_000_000_000_000_000_000, "E"),
        (1_000_000_000_000_000, "P"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "k"),
    ] {
        if count >= scale {
            return format!("{}{suffix}", count / scale);
        }
    }
    count.to_string()
}

fn item_activity_line(issue: &Issue, show_commits: bool) -> Line<'static> {
    let activity = issue.activity.unwrap_or_default();
    let (discussion, partial) = if issue.pull_request.is_some() {
        match (activity.comments, activity.review_comments) {
            (Some(a), Some(b)) => (Some(u128::from(a) + u128::from(b)), false),
            (a, b) => (a.or(b).map(u128::from), a.or(b).is_some()),
        }
    } else {
        (activity.comments.map(u128::from), false)
    };
    let count = discussion.map_or_else(
        || "?".to_owned(),
        |n| {
            let compact = compact_activity_count(n);
            format!(
                "{compact}{}",
                if partial {
                    "+"
                } else {
                    ""
                }
            )
        },
    );
    let mut spans = vec![Span::styled(
        format!("≡{count:<6} "),
        Style::new().fg(if discussion.is_some_and(|n| n >= 10) {
            theme::primary()
        } else {
            theme::muted()
        }),
    )];
    if show_commits {
        let count = activity
            .commits
            .map_or_else(|| "?".to_owned(), |n| compact_activity_count(u128::from(n)));
        spans.push(Span::styled(
            format!("○{count}"),
            Style::new().fg(if activity.commits.is_some_and(|n| n >= 10) {
                theme::primary()
            } else {
                theme::muted()
            }),
        ));
    }
    Line::from(spans)
}

fn pr_change_indicator(
    additions: Option<u64>,
    deletions: Option<u64>,
    cells: u16,
) -> Line<'static> {
    if cells == 0 {
        return Line::default();
    }
    let (Some(additions), Some(deletions)) = (additions, deletions) else {
        return Line::styled(
            format!("?{}", "□".repeat(usize::from(cells - 1))),
            Style::new().fg(theme::muted()),
        );
    };
    let total = u128::from(additions) + u128::from(deletions);
    let bucket = match total {
        0 => 0,
        1..=10 => 1,
        11..=100 => 2,
        101..=1000 => 3,
        _ => 4,
    };
    // Preserve quarter-capacity size buckets at every width, rounding ties up.
    let filled = if total == 0 {
        0
    } else {
        ((bucket * u128::from(cells) + 2) / 4).max(1)
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
        (0..u128::from(cells))
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
    columns: [Rect; 6],
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
        pr_change_indicator(pr.additions, pr.deletions, columns[3].width),
        Line::styled(
            issue.state.as_str(),
            Style::new().fg(issue_color(&issue.state)),
        )
        .alignment(Alignment::Right),
        item_activity_line(issue, columns[5].width >= 15),
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
    } else if tab == InboxTab::Security {
        let sources = snapshot
            .sources
            .iter()
            .filter(|source| source.name.starts_with("security:github:"))
            .collect::<Vec<_>>();
        let message = if !snapshot.initialized || snapshot.refreshing {
            "Loading private GitHub advisories...".to_owned()
        } else if sources.is_empty() {
            "No GitHub advisory source. Configure a GitHub repository and authenticate with advisory access.".to_owned()
        } else if sources.iter().any(|source| !source.connected) {
            "Private advisory access unavailable or unauthorized. Check GitHub authentication and repository advisory permissions; Ctrl+G, r retries.".to_owned()
        } else {
            "No private advisories in triage or draft. Closed/published advisory history is excluded. Ctrl+G, r refreshes.".to_owned()
        };
        lines.push(Line::styled(message, Style::new().fg(theme::muted())));
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
    if let Some(status) = app.visible_status(snapshot) {
        let color = if snapshot.error.is_some() || snapshot.diagnostic_log_error.is_some() {
            theme::error()
        } else {
            theme::muted()
        };
        return Line::from(vec![
            Span::styled("● ", Style::new().fg(color)),
            Span::styled(status, Style::new().fg(color)),
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
    let height = area.height.saturating_sub(2).min(26);
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
    if width < 48 || height < 12 {
        frame.render_widget(
            Paragraph::new(
                "m mode\ng debug\nd dispatch/review\nr refresh\ns sort\nc clear search\nq quit\nEsc cancel",
            )
            .style(Style::new().bg(theme::element()).fg(theme::primary())),
            popup,
        );
        return;
    }
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
        command_help_line(
            "d",
            if app.tab == InboxTab::Security {
                "private review (settings, then privacy consent)"
            } else if app.tab == InboxTab::PullRequests {
                "review selected PR"
            } else {
                "dispatch selected issue"
            },
        ),
        command_help_line("r", "refresh issue sources"),
        command_help_line("m", "Manual / Away mode and ranked queue"),
        command_help_line("s", "choose issue sorting"),
        command_help_line("g", "debug runtime status"),
        command_help_line("c", "clear search"),
        command_help_line("q", "quit agent-launcher"),
        command_help_line("Tab / BackTab", "next / previous: Issues / PRs / Security"),
        Line::styled("Inbox", Style::new().fg(theme::primary()).bold()),
        command_help_line("↑/↓ · PgUp/PgDn · Home/End", "navigate; wheel scrolls"),
        command_help_line("Enter / click row", "open issue or PR details"),
        command_help_line("Backspace", "edit search"),
        command_help_line("Esc", "quit from the main inbox"),
        Line::styled("Details", Style::new().fg(theme::primary()).bold()),
        command_help_line(
            "d · o · s",
            if app.tab == InboxTab::Security {
                "private review · open · stop"
            } else if app.tab == InboxTab::PullRequests {
                "review PR · open · stop"
            } else {
                "dispatch · open · stop"
            },
        ),
        command_help_line("i · Esc", "input · back"),
        command_help_line(
            "x worktree · X issue",
            if app.tab == InboxTab::Security {
                "blocked; private clone retained"
            } else {
                "detail only; confirm"
            },
        ),
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

fn draw_security_confirmation(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    app.security_confirmation_visible = false;
    let Some(overlay) = app.dispatch_overlay.as_ref() else {
        return;
    };
    let settings = &overlay.settings;
    let popup = area.inner(Margin::new(1, 1));
    if popup.is_empty() {
        return;
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::new().style(Style::new().bg(theme::element())), popup);
    let inner = popup.inner(Margin::new(1, 1));
    let lines = vec![
        Line::styled(
            "PRIVATE SECURITY REVIEW - EXPLICIT CONSENT",
            Style::new().fg(theme::error()).bold(),
        ),
        Line::raw(format!("Advisory: {}", overlay.issue_key.canonical())),
        Line::raw(format!(
            "Backend: {} | Harness: {}",
            settings
                .backend
                .map_or_else(|| "unavailable".into(), |b| b.to_string()),
            settings.options.harness.as_deref().unwrap_or(
                if settings.backend == Some(BackendKind::Native) {
                    "opencode"
                } else {
                    &settings.default_harness
                }
            )
        )),
        Line::raw(settings.model_label()),
        Line::raw(""),
        Line::raw(
            "This sends the confidential advisory AND private repository code to the chosen harness/model provider, which may be a cloud service. Review that provider's data retention and privacy policy before consenting.",
        ),
        Line::raw(
            "The request may create the advisory's temporary private fork on GitHub, where CI and integrations are disabled. Work uses a local isolated clone with private remotes only, never a public fallback.",
        ),
        Line::raw(
            "No automatic public PR, advisory publication, or merge is requested by this workflow. Do not publish this private worktree or advisory; these restrictions are not a sandbox.",
        ),
        Line::raw(
            "The model/harness may write its own sessions and history. The agent is NOT OS-sandboxed. Git hooks and remote checks are defense in depth, not a sandbox or a guarantee against disclosure.",
        ),
        Line::raw(
            "Private-clone cleanup is separate and unavailable here; the clone is retained. Advisory content is cached locally with owner-only permissions, without encryption. Private run output is not stored in launcher SQLite or runtime event history. Backend/harness transcripts and other local copies may persist.",
        ),
        Line::raw(""),
        Line::styled(
            "y I reviewed this warning and consent to private dispatch | Esc cancel",
            Style::new().fg(theme::primary()).bold(),
        ),
        Line::raw(
            "Enter does not consent. No advisory or code is sent to the harness/model provider before explicit consent.",
        ),
    ];
    let paragraph = Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
    if inner.width >= 20 && paragraph.line_count(inner.width) <= usize::from(inner.height) {
        frame.render_widget(paragraph, inner);
        app.security_confirmation_visible = true;
    } else {
        frame.render_widget(Paragraph::new("Resize to review the FULL privacy warning and target. Confirmation disabled. Esc cancel.")
            .wrap(ratatui::widgets::Wrap { trim: false }).style(Style::new().fg(theme::error())), popup);
    }
}

fn draw_dispatch_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &mut AppState,
) {
    if app
        .dispatch_overlay
        .as_ref()
        .is_some_and(|overlay| overlay.settings.privacy_confirmation)
    {
        draw_security_confirmation(frame, area, app);
        return;
    }
    let Some(overlay) = app.dispatch_overlay.as_mut() else {
        return;
    };
    let item_count = match &overlay.stage {
        DispatchStage::Prompt => snapshot.prompt_profiles.len().max(1),
        DispatchStage::Target { .. } => snapshot.compute_targets.len() + 1,
        DispatchStage::Settings { .. } => 17,
    };
    let width = area
        .width
        .saturating_sub(2)
        .min(if overlay.stage == DispatchStage::Prompt {
            140
        } else {
            86
        });
    let wanted_height = if overlay.stage == DispatchStage::Prompt
        || (matches!(overlay.stage, DispatchStage::Settings { .. })
            && overlay.settings.instructions_editor.is_some())
    {
        area.height.saturating_sub(2)
    } else {
        item_count.saturating_add(6) as u16
    };
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

    if overlay.stage == DispatchStage::Prompt {
        draw_prompt_view(frame, inner, snapshot, overlay);
        return;
    }

    if let DispatchStage::Settings { profile, target } = &overlay.stage {
        let settings = &mut overlay.settings;
        let backend = settings
            .backend
            .map_or_else(|| "unavailable".into(), |b| b.to_string());
        let default = if settings.security && settings.backend == Some(BackendKind::Native) {
            "opencode"
        } else if settings.default_harness.is_empty() {
            "backend default"
        } else {
            &settings.default_harness
        };
        let harness = settings.options.harness.as_deref().map_or_else(
            || format!("Configured default ({default})"),
            |h| h.to_owned(),
        );
        let kinds = settings.harness_choices().join(", ");
        let mut lines = vec![
            Line::styled("Launch settings", Style::new().fg(theme::primary()).bold()),
            Line::raw(format!("Issue: {}", overlay.issue_key.canonical())),
            Line::raw(format!(
                "Profile: {}",
                if settings.security {
                    "Private security review (fixed; no custom prompt)"
                } else if settings.review {
                    "PR review (no issue profile)"
                } else {
                    profile.as_deref().unwrap_or("Built-in default")
                }
            )),
            Line::raw(format!("Backend: {backend}")),
            Line::raw(format!(
                "Target: {}",
                target.as_deref().unwrap_or(if settings.security {
                    "Local isolated private clone only"
                } else if settings.had_targets {
                    "Automatic"
                } else {
                    "Backend-managed"
                })
            )),
            Line::raw(format!("Harness: {harness}")),
            Line::raw(if kinds.is_empty() {
                "Harness choices: configured preset only".into()
            } else {
                format!("h cycle: Configured default, {kinds} (supported kinds)")
            }),
            Line::raw(settings.model_label()),
            Line::raw("1 Configured default / 2 Harness default / 3 Custom model"),
            Line::raw("m edit custom model; h resets model to Harness default"),
            Line::raw("Examples: Claude sonnet; OpenCode/Pi openai/gpt-5.4"),
            Line::raw("Manual model ID, not a discovered or installed catalog."),
            Line::raw("Availability is determined by the harness on the selected host."),
            Line::raw("This launch only; no config changes, installs or permission changes."),
        ];
        if let Some(status) = &app.status_message {
            lines.push(Line::styled(
                status.clone(),
                Style::new().fg(theme::error()),
            ));
        }
        let editing = settings.model_editor.is_some();
        let instructions = settings.instructions_editor.is_some() && !editing;
        let footer = if editing {
            "Enter confirm field (does not launch)\nEsc cancel field / PgUp/PgDn scroll"
        } else if instructions && settings.instructions_focused {
            "Enter newline | Tab settings | Ctrl+S done | Esc settings"
        } else if instructions {
            "Enter launch | Tab instructions | Esc back | PgUp/PgDn summary"
        } else if settings.security {
            "Enter review privacy warning (does not launch) / Esc cancel / PgUp/PgDn scroll"
        } else {
            "Enter launch / Esc back / PgUp/PgDn scroll"
        };
        let footer = Paragraph::new(footer)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .style(Style::new().fg(theme::primary()));
        let footer_height = footer
            .line_count(inner.width)
            .min(usize::from(inner.height)) as u16;
        let available_height = inner
            .height
            .saturating_sub(footer_height + u16::from(editing));
        let body = Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        let body_height = if instructions {
            (body.line_count(inner.width).min(u16::MAX as usize) as u16).min(available_height / 2)
        } else {
            available_height
        };
        settings.scroll_max = body
            .line_count(inner.width)
            .saturating_sub(usize::from(body_height))
            .min(u16::MAX as usize) as u16;
        settings.scroll = settings.scroll.min(settings.scroll_max);
        frame.render_widget(body.scroll((settings.scroll, 0)), Rect {
            height: body_height,
            ..inner
        });
        frame.render_widget(
            footer,
            Rect::new(
                inner.x,
                inner.bottom() - footer_height,
                inner.width,
                footer_height,
            ),
        );
        if instructions && let Some(editor) = &settings.instructions_editor {
            let field = Rect::new(
                inner.x,
                inner.y + body_height,
                inner.width,
                available_height.saturating_sub(body_height),
            );
            let block = Block::bordered()
                .title("Additional instructions (this dispatch only)")
                .title_bottom("Appended after prompt; profile unchanged")
                .border_style(Style::new().fg(if settings.instructions_focused {
                    theme::primary()
                } else {
                    theme::muted()
                }));
            let body = block.inner(field);
            frame.render_widget(block, field);
            let (row, _) = editor.position();
            let top = row.saturating_sub(usize::from(body.height.saturating_sub(1)));
            let before = &editor.text[..editor.cursor];
            let column = Line::raw(before.rsplit('\n').next().unwrap_or("")).width();
            let left = column.saturating_sub(usize::from(body.width.saturating_sub(1)));
            frame.render_widget(
                Paragraph::new(editor.text.as_str()).scroll((
                    top.min(u16::MAX as usize) as u16,
                    left.min(u16::MAX as usize) as u16,
                )),
                body,
            );
            if settings.instructions_focused && !body.is_empty() {
                frame.set_cursor_position((
                    body.x + (column - left) as u16,
                    body.y + (row - top) as u16,
                ));
            }
        }
        if let Some(editor) = &settings.model_editor
            && body_height + footer_height < inner.height
        {
            let field = Rect::new(inner.x, inner.y + body_height, inner.width, 1);
            let column = Line::raw(&editor.text[..editor.cursor]).width();
            let left = column.saturating_sub(usize::from(field.width.saturating_sub(1)));
            frame.render_widget(
                Paragraph::new(editor.text.as_str())
                    .scroll((0, left.min(u16::MAX as usize) as u16)),
                field,
            );
            frame.set_cursor_position((field.x + (column - left) as u16, field.y));
        }
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
        DispatchStage::Prompt => unreachable!("prompt view rendered above"),
        DispatchStage::Settings { .. } => unreachable!("settings rendered above"),
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
}

fn draw_prompt_view(
    frame: &mut Frame<'_>,
    inner: Rect,
    snapshot: &RuntimeSnapshot,
    overlay: &mut crate::app::DispatchOverlay,
) {
    use ratatui::{
        layout::{Constraint, Layout},
        widgets::Wrap,
    };
    let view = &mut overlay.prompt;
    let style = Style::new().fg(theme::text()).bg(theme::element());
    let name = if view.name.is_empty() {
        "Built-in default"
    } else {
        &view.name
    };
    let title = if let Some(editor) = &view.editor {
        format!("Edit prompt: {}", editor.name)
    } else if view.loading_source && view.request.is_some() {
        format!("Loading source: {name}")
    } else {
        format!("Choose prompt: {name}")
    };
    let editor_footer = view.editor.as_ref().map(|editor| {
        let help = if editor.busy {
            "Saving..."
        } else if editor.discard {
            "Discard draft? y discard / n or Esc keep editing"
        } else if editor.naming {
            "Enter/Tab edit / Ctrl+S save / Esc cancel"
        } else if editor.original.is_none() {
            "Ctrl+S save / Esc cancel\nTab name / Ctrl+N name"
        } else {
            "Ctrl+S save / Esc cancel"
        };
        if view.error.is_some() {
            format!("{help}\nPgUp/PgDn scroll error")
        } else {
            help.to_owned()
        }
    });
    let warning_height = if inner.height >= 12 {
        2
    } else {
        0
    };
    let chooser_footer = if view.name.is_empty() {
        "Enter next / Esc cancel\nUp/Down 1-9 select / PgUp/PgDn\na add / r reload (built-in is read-only)"
    } else {
        "Enter next / Esc cancel\nUp/Down 1-9 select / PgUp/PgDn\na add / e edit / r reload"
    };
    let footer_height = {
        Paragraph::new(editor_footer.as_deref().unwrap_or(chooser_footer))
            .wrap(Wrap { trim: false })
            .line_count(inner.width)
            .min(usize::from(
                if view.error.is_some() && view.editor.is_some() {
                    inner.height / 3
                } else {
                    inner.height.saturating_sub(3 + warning_height)
                },
            )) as u16
    };
    let error_height = if view.editor.is_some() {
        view.error.as_ref().map_or(0, |error| {
            Paragraph::new(error.as_str())
                .wrap(Wrap { trim: false })
                .line_count(inner.width)
                .min(usize::from(
                    inner
                        .height
                        .saturating_sub(2 + warning_height + footer_height),
                )) as u16
        })
    } else {
        0
    };
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(warning_height),
        Constraint::Min(1),
        Constraint::Length(error_height),
        Constraint::Length(footer_height),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(title).style(style.fg(theme::primary()).bold()),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new("Shared user config: changes affect ALL repositories.")
            .wrap(Wrap { trim: false })
            .style(style.fg(theme::muted())),
        sections[1],
    );
    if let Some(editor) = &view.editor {
        if let Some(error) = &view.error {
            let error = Paragraph::new(error.as_str())
                .wrap(Wrap { trim: false })
                .style(style.fg(theme::error()));
            view.scroll_max = error
                .line_count(sections[3].width)
                .saturating_sub(usize::from(sections[3].height))
                .min(u16::MAX as usize) as u16;
            view.scroll = view.scroll.min(view.scroll_max);
            frame.render_widget(error.scroll((view.scroll, 0)), sections[3]);
        }
        frame.render_widget(
            Paragraph::new(editor_footer.unwrap_or_default())
                .wrap(Wrap { trim: false })
                .style(style.fg(if view.error.is_some() {
                    theme::error()
                } else {
                    theme::muted()
                })),
            sections[4],
        );
        if editor.naming {
            let name = format!("Name: {}", editor.name);
            let column = Line::raw(name.as_str()).width();
            let left = column.saturating_sub(usize::from(sections[2].width.saturating_sub(1)));
            frame.render_widget(
                Paragraph::new(name)
                    .scroll((0, left.min(u16::MAX as usize) as u16))
                    .style(style),
                sections[2],
            );
            if !sections[2].is_empty() && !editor.discard && !editor.busy {
                frame.set_cursor_position((sections[2].x + (column - left) as u16, sections[2].y));
            }
        } else {
            let body = sections[2];
            let (row, _) = editor.buffer.position();
            let top = row.saturating_sub(usize::from(body.height.saturating_sub(1)));
            let before = &editor.buffer.text[..editor.buffer.cursor];
            let column = Line::raw(before.rsplit('\n').next().unwrap_or("")).width();
            let left = column.saturating_sub(usize::from(body.width.saturating_sub(1)));
            frame.render_widget(
                Paragraph::new(editor.buffer.text.as_str())
                    .scroll((
                        top.min(u16::MAX as usize) as u16,
                        left.min(u16::MAX as usize) as u16,
                    ))
                    .style(style),
                body,
            );
            if !body.is_empty() && !editor.discard && !editor.busy {
                frame.set_cursor_position((
                    body.x + (column - left) as u16,
                    body.y + (row - top) as u16,
                ));
            }
        }
        return;
    }
    frame.render_widget(
        Paragraph::new(chooser_footer)
            .wrap(Wrap { trim: false })
            .style(style.fg(theme::muted())),
        sections[4],
    );
    let panes = if inner.width >= 86 {
        Layout::horizontal([Constraint::Length(28), Constraint::Min(1)])
            .spacing(1)
            .split(sections[2])
    } else {
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)])
            .spacing(u16::from(sections[2].height > 4))
            .split(sections[2])
    };
    let names = if snapshot.prompt_profiles.is_empty() {
        vec!["Built-in default".to_owned()]
    } else {
        snapshot.prompt_profiles.clone()
    };
    let cursor = snapshot
        .prompt_profiles
        .iter()
        .position(|n| *n == view.name)
        .unwrap_or(overlay.cursor);
    let start = cursor.saturating_sub(usize::from(panes[0].height) / 2);
    let lines: Vec<_> = names
        .iter()
        .enumerate()
        .skip(start)
        .map(|(i, name)| dispatch_choice_line(i, cursor, true, name.clone()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(style.bg(theme::panel()))
            .block(Block::default().padding(Padding::horizontal(u16::from(panes[0].width > 2)))),
        panes[0],
    );
    let (text, error) = match &view.preview {
        Some(Ok(text)) => (text.as_str(), false),
        Some(Err(error)) => (error.as_str(), true),
        None => ("Loading template...", false),
    };
    let preview = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .style(style.fg(if error {
            theme::error()
        } else {
            theme::text()
        }));
    let max = preview
        .line_count(panes[1].width)
        .saturating_sub(usize::from(panes[1].height))
        .min(u16::MAX as usize) as u16;
    view.scroll_max = max;
    view.scroll = view.scroll.min(max);
    frame.render_widget(preview.scroll((view.scroll, 0)), panes[1]);
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
                "  Enter settings · Esc back",
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
            "↑/↓ select · Enter settings · 1-9 · Esc back",
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
    tab: InboxTab,
) {
    if area.height == 0 {
        return;
    }
    let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1)
        .inner(Margin::new(u16::from(area.width >= 40), 0));
    let repository = snapshot.repository.as_ref().map_or_else(
        || "repository unavailable".to_owned(),
        |repository| {
            repository.remote.as_ref().map_or_else(
                || abbreviated_path(&repository.root),
                |remote| format!("{}/{}", remote.host, remote.repository),
            )
        },
    );
    let mut text = footer_source_label(snapshot, tab);
    text.spans.push(Span::raw(format!(" {repository}")));
    let identity_width = text.width();
    let version = env!("CARGO_PKG_VERSION");

    let version_width = version.len() as u16;
    let version_x = footer.x + footer.width.saturating_sub(version_width);
    let mut left_width = footer.width.saturating_sub(version_width + 1);
    // Keep the remote and source status ahead of optional host metrics.
    if metrics.has_samples() && usize::from(left_width) >= identity_width + 32 {
        let available = footer.width.saturating_sub(version_width + 1);
        let spark_width = (footer.width / 5)
            .clamp(8, 24)
            .min(available.saturating_sub(identity_width as u16 + 24));
        let metrics_width = 23 + spark_width;
        let metrics_x = version_x.saturating_sub(metrics_width + 1);
        left_width = metrics_x.saturating_sub(footer.x + 1);
        let cpu_label = format!("CPU {:>3}% 15m ", metrics.cpu_percent);
        frame.render_widget(
            Paragraph::new(cpu_label).style(Style::new().fg(load_color(metrics.cpu_percent))),
            Rect::new(metrics_x, footer.y, 13, 1),
        );
        let spark_area = Rect::new(metrics_x + 13, footer.y, spark_width, 1);
        if metrics.has_cpu_history() {
            let spark_data = metrics.cpu_sparkline(usize::from(spark_width));
            frame.render_widget(
                Sparkline::default().data(&spark_data).max(100).style(
                    Style::new()
                        .fg(load_color(metrics.cpu_percent))
                        .bg(theme::bg()),
                ),
                spark_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(if spark_width >= 10 {
                    "warming up"
                } else {
                    "warming"
                })
                .alignment(Alignment::Center)
                .style(Style::new().fg(theme::muted()).bg(theme::bg())),
                spark_area,
            );
        }
        frame.render_widget(
            Paragraph::new(format!("  MEM {:>3}%", metrics.memory_percent))
                .style(Style::new().fg(load_color(metrics.memory_percent))),
            Rect::new(metrics_x + 13 + spark_width, footer.y, 10, 1),
        );
    }
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(theme::muted())),
        Rect::new(footer.x, footer.y, left_width, 1),
    );
    frame.render_widget(
        Paragraph::new(version)
            .style(Style::new().fg(theme::muted()))
            .alignment(Alignment::Right),
        Rect::new(version_x, footer.y, footer.width.min(version_width), 1),
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

fn draw_debug_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &RuntimeSnapshot,
    app: &mut AppState,
) {
    let width = area.width.min(90);
    let height = area.height.min(30);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    app.mouse.debug = popup;
    app.debug_page_size = height.saturating_sub(2);
    app.debug_scroll_max = 0;
    if popup.is_empty() {
        app.debug_scroll = 0;
        return;
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(Block::new().style(Style::new().bg(theme::element())), popup);
    frame.render_widget(
        Paragraph::new("Debug | Esc close").style(Style::new().fg(theme::primary()).bold()),
        Rect::new(popup.x, popup.y, width, 1),
    );
    let body = Rect::new(popup.x, popup.y + 1, width, app.debug_page_size);
    if body.is_empty() {
        app.debug_scroll = 0;
        return;
    }
    let mut lines = vec![
        format!(
            "Backend selected: {}",
            snapshot
                .selected_backend
                .map_or_else(|| "none".into(), |kind| format!("{kind:?}").to_lowercase())
        ),
        format!(
            "Worktree manager detected: {}",
            worktree_manager_label(snapshot)
        ),
        format!(
            "Selected agent: {}",
            if snapshot.selected_agent.trim().is_empty() {
                "none"
            } else {
                &snapshot.selected_agent
            }
        ),
        format!(
            "Initialized: {} | Refreshing: {}",
            snapshot.initialized, snapshot.refreshing
        ),
        format!(
            "Last refresh: {}",
            snapshot
                .last_refreshed_at
                .map_or_else(|| "never".into(), |time| time.to_rfc3339())
        ),
        format!("Error: {}", snapshot.error.as_deref().unwrap_or("none")),
    ];
    if let Some(path) = &snapshot.diagnostic_log_path {
        lines.push(format!("Diagnostic log: {}", path.display()));
    }
    if let Some(error) = &snapshot.diagnostic_log_error {
        lines.push(format!("Logging warning: {error}"));
    }
    if let Some(failure) = &snapshot.last_failure {
        lines.push(format!("Last diagnostic failure: {failure}"));
    }
    if let Some(repository) = &snapshot.repository {
        lines.push(format!("Local path: {}", repository.root.display()));
        lines.push(format!(
            "Effective host/repo: {}",
            repository.remote.as_ref().map_or_else(
                || "none".into(),
                |remote| format!("{}/{}", remote.host, remote.repository)
            )
        ));
    } else {
        lines.push("Repository: unavailable".into());
    }
    lines.push(format!("Backends: {}", snapshot.backends.len()));
    for backend in &snapshot.backends {
        lines.push(format!(
            "  {:?}: {} | manager running: {}",
            backend.kind,
            if backend.available {
                "available"
            } else {
                "unavailable"
            },
            backend.manager_running
        ));
        if let Some(message) = &backend.message {
            lines.push(format!("  Message: {message}"));
        }
    }
    lines.push(format!(
        "Compute targets: {}",
        snapshot.compute_targets.len()
    ));
    for target in &snapshot.compute_targets {
        lines.push(format!(
            "  {}: {:?} | active: {} / {} | dispatchable: {}",
            target.name,
            target.availability,
            target.active_runs,
            target
                .max_active_runs
                .map_or_else(|| "unlimited".into(), |max| max.to_string()),
            target.is_dispatchable()
        ));
        if let Some(message) = &target.message {
            lines.push(format!("  Message: {message}"));
        }
    }
    let activity = &snapshot.herdr_activity;
    lines.push(format!(
        "Herdr activity: {}",
        if activity.enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));
    if activity.enabled
        || !activity.endpoints.is_empty()
        || activity.discovery_error.is_some()
        || activity.persistence_error.is_some()
    {
        lines.push(format!(
            "  Remote session discovery configured: {} | discovering: {}",
            activity.discover_remote_sessions, activity.discovering
        ));
        lines.push(format!(
            "  Scope: {} sessions",
            if activity.discover_remote_sessions {
                "discovered"
            } else {
                "configured"
            }
        ));
        if let Some(sample) = activity
            .samples
            .iter()
            .max_by_key(|sample| sample.sampled_at)
        {
            lines.push(format!(
                "  Latest sample: {} | {:?} | inventory complete: {}",
                sample.sampled_at.to_rfc3339(),
                sample.completeness,
                sample.inventory_complete
            ));
            lines.push(format!(
                "  Coverage: {}/{} fresh | {} stale | {} failed | {} never observed | {} excluded",
                sample.fresh_endpoints,
                sample.expected_endpoints,
                sample.stale_endpoints,
                sample.failed_endpoints,
                sample.never_observed_endpoints,
                sample.excluded_endpoints
            ));
            lines.push(sample.counts.as_ref().map_or_else(
                || "  Counts unavailable".to_owned(),
                |c| {
                    format!(
                        "  Totals: {} working | {} blocked | {} unseen done | {} idle | {} unknown",
                        c.working, c.blocked, c.unseen_done, c.idle, c.unknown
                    )
                },
            ));
        } else {
            lines.push("  Counts unavailable: unobserved".to_owned());
        }
        lines.push(format!(
            "  Discovery error: {}",
            activity_diagnostic_code(activity.discovery_error.as_deref())
        ));
        lines.push(format!(
            "  Persistence error: {}",
            activity_diagnostic_code(activity.persistence_error.as_deref())
        ));
        for endpoint in &activity.endpoints {
            // IDs are user-visible metadata, not command output; escape terminal controls.
            lines.push(format!(
                "  Endpoint ID: {}",
                endpoint.endpoint_id.escape_debug()
            ));
            lines.push(format!(
                "    Transport: {:?} | Freshness: {:?}",
                endpoint.transport, endpoint.freshness
            ));
            lines.push(format!(
                "    Error kind: {}",
                activity_diagnostic_code(endpoint.error_kind.as_deref())
            ));
            lines.push(format!(
                "    Last success: {}",
                endpoint
                    .last_success_at
                    .map_or_else(|| "never".into(), |time| time.to_rfc3339())
            ));
        }
    }
    lines.push(format!("Sources: {}", snapshot.sources.len()));
    for source in &snapshot.sources {
        lines.push(format!(
            "  {}: {}",
            source.name,
            if source.connected {
                "connected"
            } else {
                "disconnected"
            }
        ));
        lines.push(format!(
            "  Message: {}",
            source.message.as_deref().unwrap_or("none")
        ));
    }
    let paragraph = Paragraph::new(lines.join("\n"))
        .style(Style::new().fg(theme::text()).bg(theme::element()))
        .wrap(ratatui::widgets::Wrap { trim: false });
    app.debug_scroll_max = paragraph
        .line_count(body.width)
        .saturating_sub(usize::from(body.height))
        .min(usize::from(u16::MAX)) as u16;
    app.debug_scroll = app.debug_scroll.min(app.debug_scroll_max);
    frame.render_widget(paragraph.scroll((app.debug_scroll, 0)), body);
    frame.render_widget(
        Paragraph::new(format!(
            "{}/{} | Arrows PgUp/PgDn Home/End wheel",
            u32::from(app.debug_scroll) + 1,
            u32::from(app.debug_scroll_max) + 1
        ))
        .style(Style::new().fg(theme::muted())),
        Rect::new(popup.x, body.bottom(), width, 1),
    );
}

fn activity_diagnostic_code(value: Option<&str>) -> &str {
    match value {
        None => "none",
        Some(code)
            if !code.is_empty()
                && code.len() <= 80
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) =>
        {
            code
        },
        Some(_) => "invalid diagnostic code",
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
        .map_or("none detected", |kind| match kind {
            BackendKind::Superset => "superset",
            BackendKind::Herdr => "herdr",
            BackendKind::Native | BackendKind::Conductor => unreachable!(),
        })
}

fn footer_source_label(snapshot: &RuntimeSnapshot, tab: InboxTab) -> Line<'static> {
    let sources = snapshot
        .sources
        .iter()
        .filter(|source| source.name.starts_with("security:github:") == (tab == InboxTab::Security))
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Line::from("no source");
    }
    let connected = sources.iter().filter(|source| source.connected).count();
    let throttled = sources.iter().any(|source| {
        source
            .message
            .as_deref()
            .is_some_and(|message| message.contains("throttled"))
    });
    let color = if throttled || (connected > 0 && connected < sources.len()) {
        theme::primary()
    } else if connected == 0 {
        theme::error()
    } else {
        theme::done()
    };
    let label = if sources.len() == 1 {
        let source = sources[0];
        let name = source.name.split(':').next().unwrap_or(&source.name);
        if name == "github" {
            return Line::from(Span::styled("\u{f09b}", Style::new().fg(color)));
        } else {
            name.to_owned()
        }
    } else if connected == sources.len() {
        format!("{} sources", sources.len())
    } else {
        format!("{connected}/{} sources", sources.len())
    };
    Line::from(vec![
        Span::raw(format!("{label} ")),
        Span::styled("●", Style::new().fg(color)),
    ])
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
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};

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
            security_advisory: None,
            activity: None,
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
                supports_delete: false,
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

    fn security_snapshot() -> RuntimeSnapshot {
        let mut snapshot = normal_snapshot();
        let mut advisory = issue(
            "advisory/GHSA-aaaa-bbbb-cccc",
            "Confidential advisory title",
        );
        advisory.state = "draft".into();
        advisory.description = Some("PRIVATE_BODY_SENTINEL".into());
        advisory.security_advisory = Some(agent_launcher_core::SecurityAdvisoryMetadata {
            ghsa_id: "GHSA-aaaa-bbbb-cccc".into(),
            cve_id: Some("CVE-2026-1234".into()),
            severity: Some("critical".into()),
        });
        snapshot.issues.push(advisory);
        snapshot.sources.push(SourceStatus {
            name: "security:github:github.com:acme/launcher".into(),
            connected: true,
            supports_delete: false,
            message: None,
        });
        snapshot.selected_backend = Some(BackendKind::Herdr);
        snapshot
    }

    #[test]
    fn security_rendering_private_details_and_direct_filtered_mouse_selection() {
        let snapshot = security_snapshot();
        let mut app = AppState::default();
        let text = render(120, 48, &snapshot, &mut app);
        assert!(!text.contains("Confidential advisory title"));
        assert!(!text.contains("PRIVATE_BODY_SENTINEL"));
        let tab = app
            .mouse
            .tabs
            .iter()
            .find(|(_, tab)| *tab == InboxTab::Security)
            .unwrap()
            .0;
        assert!(crate::mouse::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: tab.x,
                row: tab.y,
                modifiers: KeyModifiers::NONE
            },
            &snapshot,
            (120, 48)
        ));
        assert_eq!(app.tab, InboxTab::Security);
        app.search_query = "CVE-2026-1234".into();
        let text = render(120, 48, &snapshot, &mut app);
        for expected in [
            "critical",
            "Confidential advisory title",
            "draft",
            "private",
            "3d",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("PRIVATE_BODY_SENTINEL"));
        assert!(!text.contains("GHSA-aaaa-bbbb-cccc"));
        let row = app.mouse.rows[0].0;
        assert!(crate::mouse::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE
            },
            &snapshot,
            (120, 48)
        ));
        let text = render(120, 60, &snapshot, &mut app);
        assert!(text.contains("PRIVATE_BODY_SENTINEL"));
        assert!(text.contains("PRIVATE Security Advisory"));
        assert!(text.contains("GHSA-aaaa-bbbb-cccc"));
        assert!(text.contains("CVE-2026-1234"));
        assert!(!text.contains("review PR"));
        assert!(!text.contains("x worktree"));
        assert!(!text.contains("X issue"));
        for (width, height) in [(80, 24), (30, 12), (10, 6), (1, 1)] {
            render(width, height, &snapshot, &mut app);
        }
        app.reset_detail();
        app.set_tab(InboxTab::Issues);
        app.debug_overlay = true;
        let text = render(120, 60, &snapshot, &mut app);
        assert!(!text.contains("PRIVATE_BODY_SENTINEL"));
        assert!(!text.contains("Confidential advisory title"));
    }

    #[test]
    fn security_table_colors_spacing_and_narrow_widths() {
        let mut snapshot = security_snapshot();
        let mut app = AppState {
            tab: InboxTab::Security,
            ..Default::default()
        };
        for (state, state_color) in [
            ("triage", theme::primary()),
            ("draft", theme::secondary()),
            ("published", theme::done()),
            ("closed", theme::done()),
            ("unknown", theme::muted()),
        ] {
            snapshot.issues[1].state = state.into();
            for (severity, color) in [
                ("critical", theme::error()),
                ("high", theme::primary()),
                ("medium", theme::secondary()),
                ("low", theme::muted()),
                ("unknown", theme::muted()),
            ] {
                snapshot.issues[1]
                    .security_advisory
                    .as_mut()
                    .unwrap()
                    .severity = Some(severity.into());
                for width in (1..=24).chain([36, 80]) {
                    for title in ["Long title ".repeat(20), "界面🔒e\u{301}".repeat(20)] {
                        snapshot.issues[1].title = title;
                        let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
                        terminal
                            .draw(|frame| draw_table(frame, frame.area(), &snapshot, &mut app))
                            .unwrap();
                        let buffer = terminal.backend().buffer();
                        let columns = security_columns(Rect::new(0, 1, width, 1));
                        assert_eq!(buffer[(0, 1)].symbol(), "▶");
                        if width >= 24 {
                            assert_eq!(buffer[(columns[1].x, 1)].symbol(), "3");
                            assert_eq!(buffer[(columns[1].x + 1, 1)].symbol(), "d");
                            assert_eq!(buffer[(width - 1, 1)].fg, state_color);
                            let displayed = truncate(state, columns[4].width as usize);
                            assert_eq!(
                                buffer[(width - 1, 1)].symbol(),
                                displayed.chars().last().unwrap().to_string()
                            );
                            assert_eq!(buffer[(columns[4].x - 1, 1)].symbol(), " ");
                        }
                        if width >= 36 {
                            assert_eq!(buffer[(columns[2].x, 1)].fg, color);
                        } else {
                            assert_eq!(columns[2].width, 0);
                        }
                        for x in 0..width {
                            // Wide glyph continuation cells are reset by TestBackend.
                            if x > 0 && Line::raw(buffer[(x - 1, 1)].symbol()).width() > 1 {
                                continue;
                            }
                            assert_eq!(buffer[(x, 1)].bg, theme::element());
                        }
                    }
                }
            }
        }
        assert_eq!(issue_color("draft"), theme::error());
    }

    #[test]
    fn security_confirmation_is_disabled_until_entire_warning_fits_and_after_resize() {
        let snapshot = security_snapshot();
        let mut app = AppState {
            tab: InboxTab::Security,
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                issue_key: snapshot.issues[1].key.clone(),
                cursor: 0,
                prompt: Default::default(),
                settings: crate::app::LaunchSettings {
                    backend: Some(BackendKind::Herdr),
                    default_harness: "claude".into(),
                    security: true,
                    privacy_confirmation: true,
                    ..Default::default()
                },
                stage: DispatchStage::Settings {
                    profile: None,
                    target: None,
                },
            }),
            ..Default::default()
        };
        for (width, height) in [(120, 40), (30, 12), (120, 40), (1, 1), (120, 40)] {
            let text = render(width, height, &snapshot, &mut app)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(app.security_confirmation_visible, width == 120);
            assert!(!text.contains("PRIVATE_BODY_SENTINEL"));
            if width == 120 {
                for expected in [
                    "GHSA-aaaa-bbbb-cccc",
                    "Harness: claude",
                    "confidential advisory AND private",
                    "cloud service",
                    "temporary private fork",
                    "private remotes only",
                    "CI and integrations are disabled",
                    "No automatic public PR",
                    "Advisory content is cached locally",
                    "Private run output is not stored in launcher SQLite or runtime event history",
                    "NOT OS-sandboxed",
                    "sessions and history",
                    "not a sandbox",
                    "y I reviewed",
                    "Enter does not consent",
                ] {
                    assert!(text.contains(expected), "missing {expected}");
                }
            } else if width == 30 {
                assert!(text.contains("Confirmation disabled"));
            }
        }
        app.dispatch_overlay
            .as_mut()
            .unwrap()
            .settings
            .options
            .model = agent_launcher_core::ModelSelection::Explicit(
            "provider/very-long-model-name ".repeat(300),
        );
        render(120, 40, &snapshot, &mut app);
        assert!(!app.security_confirmation_visible);
    }

    #[test]
    fn security_empty_states_use_only_security_source_health() {
        let mut snapshot = security_snapshot();
        snapshot.issues.clear();
        let mut app = AppState {
            tab: InboxTab::Security,
            ..Default::default()
        };
        snapshot.error = Some("ordinary source error".into());
        let text = render(120, 48, &snapshot, &mut app);
        assert!(text.contains("No private advisories"));
        assert!(!text.contains("ordinary source error"));
        snapshot.error = None;
        snapshot.sources[1].connected = false;
        assert_eq!(
            footer_source_label(&snapshot, InboxTab::Issues).spans[0]
                .style
                .fg,
            Some(theme::done())
        );
        assert_eq!(
            footer_source_label(&snapshot, InboxTab::Security).spans[1]
                .style
                .fg,
            Some(theme::error())
        );
        let text = render(120, 48, &snapshot, &mut app);
        assert!(text.contains("unavailable or unauthorized"));
        app.set_tab(InboxTab::Issues);
        let text = render(120, 48, &snapshot, &mut app);
        assert!(text.contains("No Issues"));
        assert!(!text.contains("unauthorized"));
        app.set_tab(InboxTab::Security);
        snapshot.sources.pop();
        let text = render(120, 48, &snapshot, &mut app);
        assert!(text.contains("No GitHub advisory source"));
    }

    #[test]
    fn private_run_details_do_not_render_persisted_output_or_model_and_inputs_fit() {
        let mut snapshot = security_snapshot();
        let now = Utc::now();
        snapshot.runs.push(RunSummary {
            id: "private-run".into(),
            issue_key: snapshot.issues[1].key.canonical(),
            confidential: true,
            model: Some("MODEL_SENTINEL".into()),
            workspace: None,
            agent: "opencode".into(),
            state: RunState::NeedsInput,
            message: None,
            session_id: None,
            started_at: now,
            updated_at: now,
        });
        snapshot
            .run_events
            .insert("private-run".into(), vec![EventEnvelope {
                run_id: "private-run".into(),
                sequence: 0,
                timestamp: now,
                payload: RunEvent::Output {
                    stream: OutputStream::Pty,
                    text: "OUTPUT_SENTINEL".into(),
                },
            }]);
        let mut app = AppState {
            tab: InboxTab::Security,
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[1].key.clone()),
            ..Default::default()
        };
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            app.layout = layout;
            let text = render(120, 60, &snapshot, &mut app);
            assert!(text.contains("PRIVATE run"));
            assert!(text.contains("not persisted"));
            assert!(!text.contains("MODEL_SENTINEL"));
            assert!(!text.contains("OUTPUT_SENTINEL"));
            assert!(text.contains("i input"));
            assert!(text.contains("Esc back"));
            app.input_overlay = Some(crate::app::InputOverlay {
                run_id: "private-run".into(),
                prompt: "Private follow-up".into(),
                text: "Review locally".into(),
            });
            for (width, height) in [(120, 60), (80, 24), (30, 12), (1, 1)] {
                let text = render(width, height, &snapshot, &mut app);
                assert!(app.mouse.blocked);
                assert!(!text.contains("OUTPUT_SENTINEL"));
                assert!(!text.contains("MODEL_SENTINEL"));
            }
            app.input_overlay = None;
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
    fn mode_button_shares_tabs_or_footer_without_overlap_and_preserves_geometry() {
        use agent_launcher_core::{AppMode, AwayPhase};

        let mut snapshot = mouse_snapshot();
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            for (width, height) in [(1, 1), (8, 3), (24, 7), (40, 24), (80, 24), (160, 60)] {
                for route in [Route::Inbox, Route::Detail] {
                    let mut app = AppState {
                        layout,
                        ..Default::default()
                    };
                    if route == Route::Detail {
                        assert!(app.open_detail(&snapshot));
                    }
                    let mut geometry = None;
                    for phase in [
                        AwayPhase::Inactive,
                        AwayPhase::Running,
                        AwayPhase::Paused,
                        AwayPhase::Attention,
                    ] {
                        snapshot.away.mode = if phase == AwayPhase::Inactive {
                            AppMode::Manual
                        } else {
                            AppMode::Away
                        };
                        snapshot.away.phase = phase;
                        app.host_metrics.record(20, 55);
                        let buffer = render_buffer(width, height, &snapshot, &mut app);
                        let current = (app.mouse.list, app.mouse.detail, app.visible_rows);
                        if let Some(previous) = geometry {
                            assert_eq!(current, previous);
                        }
                        geometry = Some(current);
                        let button = app.mouse.mode;
                        if width < 2 {
                            assert!(button.is_empty());
                            continue;
                        }
                        assert_eq!(button.height, 1);
                        let label: String = (button.x..button.right())
                            .map(|x| buffer[(x, button.y)].symbol())
                            .collect();
                        assert_eq!(
                            label.trim(),
                            if phase == AwayPhase::Inactive {
                                "Manual"
                            } else {
                                "Away"
                            }
                        );
                        assert!(!render(width, height, &snapshot, &mut app).contains("MODE:"));
                        if route == Route::Inbox && width >= 80 {
                            assert_eq!(button.y, app.mouse.tabs[0].0.y);
                            assert_eq!(button.right(), app.mouse.list.right());
                            assert!(label.contains(if phase == AwayPhase::Inactive {
                                "Manual"
                            } else {
                                "Away"
                            }));
                        } else {
                            assert_eq!(button.y, height - 1);
                            assert_eq!(button.right(), width);
                        }
                        for (tab, _) in &app.mouse.tabs {
                            assert!(!button.intersects(*tab));
                        }
                        assert!(!button.intersects(app.mouse.detail));
                        assert_eq!(
                            buffer[(button.x, button.y)].bg,
                            if phase == AwayPhase::Inactive {
                                theme::element()
                            } else {
                                theme::primary()
                            }
                        );
                        assert_eq!(
                            buffer[(button.x, button.y)].fg,
                            if phase == AwayPhase::Inactive {
                                theme::text()
                            } else {
                                theme::bg()
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mode_button_clicks_reject_stale_and_overlay_geometry() {
        let snapshot = mouse_snapshot();
        for (width, route) in [(80, Route::Inbox), (40, Route::Inbox), (80, Route::Detail)] {
            let mut app = AppState::default();
            if route == Route::Detail {
                assert!(app.open_detail(&snapshot));
            }
            render_buffer(width, 24, &snapshot, &mut app);
            let button = app.mouse.mode;
            let click = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: button.x,
                row: button.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width + 1, 24)
            ));
            app.command_overlay = true;
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
            render_buffer(width, 24, &snapshot, &mut app);
            app.command_overlay = false;
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
            render_buffer(width, 24, &snapshot, &mut app);
            app.route = if route == Route::Inbox {
                Route::Detail
            } else {
                Route::Inbox
            };
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
            app.route = route;
            assert!(crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
            assert!(app.away_overlay.is_some());
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
            app.away_overlay = None;
            render_buffer(1, 1, &snapshot, &mut app);
            assert!(app.mouse.mode.is_empty());
            assert!(!crate::mouse::handle_mouse(
                &mut app,
                click,
                &snapshot,
                (width, 24)
            ));
        }
    }

    #[test]
    fn mode_button_shows_only_mode_regardless_of_slots_or_phase() {
        use agent_launcher_core::{AppMode, AwayPhase};
        let mut snapshot = RuntimeSnapshot::default();
        snapshot.away.mode = AppMode::Away;
        snapshot.away.prioritizing = true;
        for (phase, width, expected) in [
            (AwayPhase::Running, 13, " Away "),
            (AwayPhase::Paused, 17, " Away "),
            (AwayPhase::Attention, 20, " Away "),
            (AwayPhase::Attention, 30, " Away "),
        ] {
            snapshot.away.phase = phase;
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            let mut button = Rect::default();
            terminal
                .draw(|frame| button = draw_mode_button(frame, frame.area(), &snapshot))
                .unwrap();
            let label: String = (button.x..button.right())
                .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                .collect();
            assert_eq!(label, expected);
        }
    }

    #[test]
    fn flexible_layout_gives_extra_width_to_titles_and_activity_not_metadata_or_logo() {
        let mut snapshot = pr_snapshot();
        for issue in &mut snapshot.issues {
            issue.title = "T".repeat(300);
            issue.author = Some("a".repeat(40));
        }
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let baseline = render_buffer(108, 60, &snapshot, &mut AppState {
                tab,
                layout: crate::LayoutMode::Fixed,
                ..Default::default()
            });
            for width in [160, 240] {
                let mut fixed = AppState {
                    tab,
                    layout: crate::LayoutMode::Fixed,
                    ..Default::default()
                };
                let fixed_buffer = render_buffer(width, 60, &snapshot, &mut fixed);
                let mut flexible = AppState {
                    tab,
                    ..Default::default()
                };
                assert_eq!(flexible.layout, crate::LayoutMode::Flexible);
                let buffer = render_buffer(width, 60, &snapshot, &mut flexible);
                // All content, including the mode button, stays within the centered panel.
                for y in 0..59 {
                    for x in 0..108 {
                        assert_eq!(baseline[(x, y)], fixed_buffer[(x + (width - 108) / 2, y)]);
                    }
                }
                let fixed_row = fixed.mouse.rows[0].0;
                let row = flexible.mouse.rows[0].0;
                assert_eq!((row.x, row.right()), (4, width - 3));
                assert!(row.y < fixed_row.y);
                assert!(flexible.visible_rows > fixed.visible_rows);
                assert_eq!(flexible.scroll, fixed.scroll);
                let title_cells = |buffer: &Buffer, row: Rect| {
                    (row.x..row.right())
                        .filter(|&x| buffer[(x, row.y)].symbol() == "T")
                        .count()
                };
                assert_eq!(
                    title_cells(&buffer, row) - title_cells(&fixed_buffer, fixed_row),
                    usize::from(
                        width
                            - 108
                            - if tab == InboxTab::PullRequests {
                                pr_columns(row, 3)[3].width - 4
                            } else {
                                0
                            }
                    )
                );
                if tab == InboxTab::PullRequests {
                    let columns = pr_columns(row, 3);
                    let old = pr_columns(fixed_row, 3);
                    for index in [0, 2, 4] {
                        assert_eq!(columns[index].width, old[index].width);
                    }
                    assert_eq!(columns[2].width, 12);
                    assert_eq!(columns[4].width, 6);
                    assert_eq!(
                        (columns[2].x..columns[2].right())
                            .filter(|&x| buffer[(x, row.y)].symbol() == "a")
                            .count(),
                        11 // The final author cell is the truncation ellipsis.
                    );
                } else {
                    let columns = Columns::for_width(row.width);
                    assert_eq!((columns.age, columns.source, columns.state), (6, 8, 12));
                }
                let status: String = (row.right() - 4..row.right())
                    .map(|x| buffer[(x, row.y)].symbol())
                    .collect();
                assert_eq!(status, "open");
                for y in 0..59 {
                    for x in [0, 1, width - 2, width - 1] {
                        assert_eq!(buffer[(x, y)].symbol(), " ");
                        assert_eq!(buffer[(x, y)].bg, theme::bg());
                    }
                }
                // Both logo lines retain their literal glyphs and centered position.
                for index in 0..2 {
                    let logo = format!(
                        "{}   {}",
                        theme::AGENT_LOGO[index],
                        theme::LAUNCHER_LOGO[index]
                    );
                    let x = (width - logo.chars().count() as u16) / 2;
                    let y = 3 + index as u16;
                    let actual: String = (x..x + logo.chars().count() as u16)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    assert_eq!(actual, logo);
                    for x in x..x + logo.chars().count() as u16 {
                        assert_eq!(buffer[(x, y)], fixed_buffer[(x, 8 + index as u16)]);
                    }
                }
                // The activity panel uses the same expanded edges as the listing.
                assert_eq!(buffer[(2, 4)].bg, theme::panel());
                assert_eq!(buffer[(width - 3, 4)].bg, theme::panel());
                assert_eq!(fixed_buffer[(2, 4)].bg, theme::bg());
            }
        }
    }

    #[test]
    fn flexible_height_expands_rows_to_bottom_with_activity_controls_and_mouse_geometry() {
        let mut snapshot = pr_snapshot();
        let templates = snapshot.issues.clone();
        snapshot.issues = (0..200)
            .map(|id| {
                let mut issue = templates[id % 2].clone();
                issue.key.native_id = id.to_string();
                issue.title = format!("Visible row ID {id:03}");
                if let Some(pr) = issue.pull_request.as_mut() {
                    pr.number = id as u64 + 1;
                }
                issue
            })
            .collect();
        for width in [80, 160, 240] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                for height in [48, 80] {
                    let mut fixed = AppState {
                        tab,
                        layout: crate::LayoutMode::Fixed,
                        ..Default::default()
                    };
                    render_buffer(width, height, &snapshot, &mut fixed);
                    assert_eq!(fixed.visible_rows, 14);
                    assert_eq!(fixed.mouse.list.y, (height - 24) / 2 + 6);
                    for selected in [0, 99] {
                        let mut app = AppState {
                            tab,
                            selected,
                            ..Default::default()
                        };
                        let buffer = render_buffer(width, height, &snapshot, &mut app);
                        let line = |y| {
                            (0..width)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                        };
                        assert_eq!(app.visible_rows, usize::from(height - 18));
                        assert!(app.visible_rows > fixed.visible_rows);
                        assert_eq!(app.mouse.rows.len(), app.visible_rows);
                        assert_eq!(app.mouse.list.bottom(), height - 4);
                        assert_eq!(app.mouse.list.y, 14);
                        assert_eq!(app.mouse.tabs[0].0.y, 9);
                        assert!(line(2).contains("Herdr activity"));
                        assert!(line(2).contains("disabled"));
                        assert!(line(5).trim().is_empty());
                        assert!(line(7).trim().is_empty());
                        assert!(line(3).contains(theme::AGENT_LOGO[0]));
                        assert!(line(4).contains(theme::LAUNCHER_LOGO[1]));
                        assert!(line(10).trim().is_empty());
                        assert!(line(height - 4).contains('╹'));
                        assert!(line(height - 3).contains(&format!(
                            "{}-{} of 100",
                            app.scroll + 1,
                            app.scroll + app.visible_rows
                        )));
                        assert!(line(height - 2).contains("Ctrl+G"));
                        assert!(line(height - 1).contains("github.com/acme/launcher"));
                        assert!(line(height - 1).contains(env!("CARGO_PKG_VERSION")));
                        let list = app.mouse.list;
                        let thumb_y = if selected == 0 {
                            list.y
                        } else {
                            list.bottom() - 1
                        };
                        assert_eq!(buffer[(list.right() - 1, thumb_y)].bg, theme::border());
                        if selected == 99 {
                            assert_eq!(app.scroll + app.visible_rows, 100);
                        }
                        let (row, index, key) = app.mouse.rows.last().unwrap().clone();
                        assert_eq!(row.y, height - 5);
                        let issue = snapshot
                            .issues
                            .iter()
                            .find(|issue| issue.key == key)
                            .unwrap();
                        assert!(line(row.y).contains(&issue.title));
                        if let Some(pr) = &issue.pull_request {
                            assert!(line(row.y).contains(&format!("#{}", pr.number)));
                        }
                        let click = MouseEventKind::Down(MouseButton::Left);
                        for y in [
                            2,
                            8,
                            list.y - 1,
                            height - 4,
                            height - 3,
                            height - 2,
                            height - 1,
                        ] {
                            assert!(!mouse(&mut app, &snapshot, click, row.right() - 1, y));
                        }
                        assert_eq!(
                            mouse(
                                &mut app,
                                &snapshot,
                                MouseEventKind::Moved,
                                row.right() - 1,
                                row.y
                            ),
                            selected != index
                        );
                        assert_eq!(app.selected, index);
                        assert_eq!(app.route, Route::Inbox);
                        assert!(mouse(&mut app, &snapshot, click, row.right() - 1, row.y));
                        assert_eq!(app.detail_issue_key, Some(key));
                        assert_eq!(app.route, Route::Detail);
                    }
                }
            }
        }
    }

    #[test]
    fn flexible_height_resize_preserves_independent_tab_selection_and_detail_controls() {
        let snapshot = mouse_snapshot();
        let mut app = AppState {
            selected: 35,
            search_query: "Issue".into(),
            ..Default::default()
        };
        let issue_key = app.selected_issue(&snapshot).unwrap().key.clone();
        app.switch_tab();
        app.selected = 22;
        app.search_query = "Review".into();
        let pr_key = app.selected_issue(&snapshot).unwrap().key.clone();
        for height in [24, 48, 80, 32, 9, 80, 48, 24] {
            for (tab, selected, query, key) in [
                (InboxTab::Issues, 35, "Issue", &issue_key),
                (InboxTab::PullRequests, 22, "Review", &pr_key),
            ] {
                app.set_tab(tab);
                render_buffer(160, height, &snapshot, &mut app);
                assert_eq!(app.tab, tab);
                assert_eq!(app.selected, selected);
                assert_eq!(app.search_query, query);
                assert_eq!(&app.selected_issue(&snapshot).unwrap().key, key);
                assert!((app.scroll..app.scroll + app.visible_rows).contains(&selected));
                assert!(
                    app.mouse
                        .rows
                        .iter()
                        .any(|(_, index, row_key)| *index == selected && row_key == key)
                );
            }
        }
        app.open_detail(&snapshot);
        let mut previous_height = 0;
        for height in [24, 48, 80] {
            let buffer = render_buffer(160, height, &snapshot, &mut app);
            assert!(app.mouse.detail.height > previous_height);
            previous_height = app.mouse.detail.height;
            let controls: String = (0..160).map(|x| buffer[(x, height - 3)].symbol()).collect();
            assert!(controls.contains("Esc back"));
            assert!(controls.contains("review PR"));
            assert!(app.mouse.detail.bottom() <= height - 3);
        }
    }

    #[test]
    fn flexible_mouse_hits_far_right_rows_and_detail_but_not_padding() {
        let mut snapshot = mouse_snapshot();
        for issue in &mut snapshot.issues {
            issue.description = Some("long detail line ".repeat(1000));
        }
        for width in [160, 240] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                let mut app = AppState {
                    layout: crate::LayoutMode::Flexible,
                    tab,
                    scroll: 3,
                    selected: 3,
                    ..Default::default()
                };
                render_buffer(width, 24, &snapshot, &mut app);
                let (row, index, key) = app.mouse.rows[1].clone();
                assert_eq!(row.right(), width - 5); // Two cells reserved for the scrollbar.
                assert_eq!(index, 4);
                let click = MouseEventKind::Down(MouseButton::Left);
                for x in [0, 1, width - 2, width - 1, row.right()] {
                    assert!(!mouse(&mut app, &snapshot, click, x, row.y));
                }
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::Moved,
                    row.right() - 1,
                    row.y
                ));
                assert_eq!(app.selected, index);
                assert_eq!(app.route, Route::Inbox);
                assert!(mouse(&mut app, &snapshot, click, row.right() - 1, row.y));
                assert_eq!(app.detail_issue_key, Some(key));
                render_buffer(width, 24, &snapshot, &mut app);
                let detail = app.mouse.detail;
                assert_eq!((detail.x, detail.right()), (4, width - 3));
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::ScrollDown,
                    detail.right() - 1,
                    detail.y
                ));
                assert_eq!(app.detail_scroll, 3);
                let mut fixed = AppState {
                    layout: crate::LayoutMode::Fixed,
                    route: Route::Detail,
                    detail_issue_key: app.detail_issue_key.clone(),
                    ..Default::default()
                };
                render_buffer(width, 24, &snapshot, &mut fixed);
                assert_eq!(fixed.mouse.detail.width, 101);
                assert_eq!(fixed.mouse.detail.height, detail.height);
                app.debug_overlay = true;
                fixed.debug_overlay = true;
                render_buffer(width, 24, &snapshot, &mut app);
                render_buffer(width, 24, &snapshot, &mut fixed);
                assert_eq!(app.mouse.debug, fixed.mouse.debug);
                assert_eq!(app.mouse.debug.width, 90);
                assert!(app.mouse.blocked);
            }
        }
    }

    #[test]
    fn layout_modes_are_safe_at_narrow_and_tiny_sizes() {
        let snapshot = pr_snapshot();
        for width in [0, 1, 2, 8, 18, 23, 24, 36, 55, 56, 62, 79, 80, 104, 108] {
            for height in [0, 1, 3, 7, 8, 9, 17, 24, 31, 32, 38, 40, 48, 80] {
                for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                    for route in [Route::Inbox, Route::Detail] {
                        for debug_overlay in [false, true] {
                            let mut fixed = AppState {
                                layout: crate::LayoutMode::Fixed,
                                tab,
                                route,
                                debug_overlay,
                                detail_issue_key: Some(
                                    snapshot.issues[usize::from(tab == InboxTab::PullRequests)]
                                        .key
                                        .clone(),
                                ),
                                ..Default::default()
                            };
                            let mut flexible = AppState {
                                tab,
                                route,
                                debug_overlay,
                                detail_issue_key: fixed.detail_issue_key.clone(),
                                layout: crate::LayoutMode::Flexible,
                                ..Default::default()
                            };
                            let fixed_buffer = render_buffer(width, height, &snapshot, &mut fixed);
                            let flexible_buffer =
                                render_buffer(width, height, &snapshot, &mut flexible);
                            if height <= 17 || width < 24 || route == Route::Detail {
                                assert_eq!(
                                    fixed_buffer, flexible_buffer,
                                    "{width}x{height}, {route:?}, {tab:?}"
                                );
                                assert_eq!(fixed.mouse.rows, flexible.mouse.rows);
                            }
                            for app in [&fixed, &flexible] {
                                assert!(
                                    app.mouse.rows.iter().all(|(rect, ..)| rect.right() <= width
                                        && rect.bottom() < height)
                                );
                            }
                            if route == Route::Inbox && width >= 24 && height >= 10 {
                                assert!(flexible.visible_rows >= fixed.visible_rows);
                                assert!(flexible.mouse.list.bottom() <= height - 4);
                                if !debug_overlay {
                                    let commands: String = (0..width)
                                        .map(|x| flexible_buffer[(x, height - 2)].symbol())
                                        .collect();
                                    assert!(
                                        commands.contains("Ctrl+G"),
                                        "{width}x{height}: {commands}"
                                    );
                                }
                            }
                            assert_eq!(fixed.mouse.detail, flexible.mouse.detail);
                        }
                    }
                }
            }
        }
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
                        settings: Default::default(),
                        prompt: Default::default(),
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
                                confidential: false,
                                model: None,
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
                    assert!(text.contains("Herdr activity"));
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
            let line = pr_change_indicator(Some(additions), Some(deletions), 4);
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
            let line = pr_change_indicator(counts.0, counts.1, 4);
            assert_eq!(line.to_string(), "?□□□");
            assert_eq!(line.width(), 4);
            assert_eq!(line.style.fg, Some(theme::muted()));
        }
    }

    #[test]
    fn pr_indicator_scales_size_buckets_and_colors_to_cell_capacity() {
        for (cells, fills) in [
            (4, [0, 1, 1, 2, 2, 3, 3, 4]),
            (6, [0, 2, 2, 3, 3, 5, 5, 6]),
            (8, [0, 2, 2, 4, 4, 6, 6, 8]),
        ] {
            for (total, filled) in [0, 1, 10, 11, 100, 101, 1000, 1001].into_iter().zip(fills) {
                for (additions, deletions, color) in
                    [(total, 0, theme::done()), (0, total, theme::error())]
                {
                    let line = pr_change_indicator(Some(additions), Some(deletions), cells);
                    assert_eq!(line.width(), usize::from(cells));
                    for (index, span) in line.spans.iter().enumerate() {
                        assert_eq!(
                            span.content,
                            if index < filled {
                                "■"
                            } else {
                                "□"
                            }
                        );
                        assert_eq!(
                            span.style.fg,
                            Some(if index < filled {
                                color
                            } else {
                                theme::muted()
                            })
                        );
                    }
                }
            }
            for (additions, deletions, green, red) in [
                (u64::MAX, 0, cells, 0),
                (0, u64::MAX, 0, cells),
                (u64::MAX, 1, cells - 1, 1),
                (1, u64::MAX, 1, cells - 1),
                (u64::MAX, u64::MAX, cells / 2, cells / 2),
                (50, 51, cells / 2 - 1, fills[5] as u16 - (cells / 2 - 1)),
            ] {
                let line = pr_change_indicator(Some(additions), Some(deletions), cells);
                assert_eq!(line.width(), usize::from(cells));
                assert_eq!(
                    line.spans
                        .iter()
                        .filter(|span| span.style.fg == Some(theme::done()))
                        .count(),
                    usize::from(green)
                );
                assert_eq!(
                    line.spans
                        .iter()
                        .filter(|span| span.style.fg == Some(theme::error()))
                        .count(),
                    usize::from(red)
                );
            }
            for counts in [(None, None), (Some(0), None), (None, Some(u64::MAX))] {
                let line = pr_change_indicator(counts.0, counts.1, cells);
                assert_eq!(line.width(), usize::from(cells));
                assert_eq!(
                    line.to_string(),
                    format!("?{}", "□".repeat(usize::from(cells - 1)))
                );
                assert_eq!(line.style.fg, Some(theme::muted()));
            }
        }
        for counts in [
            (None, None),
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(u64::MAX), Some(u64::MAX)),
        ] {
            for cells in 0..4 {
                assert_eq!(
                    pr_change_indicator(counts.0, counts.1, cells).width(),
                    usize::from(cells)
                );
            }
        }
        assert_eq!(pr_change_indicator(Some(1), Some(0), 1).to_string(), "■");
    }

    #[test]
    fn pr_diff_resizes_at_table_width_boundaries_with_aligned_headers_and_mouse_hits() {
        let mut snapshot = pr_snapshot();
        let mut pr = snapshot.issues.pop().unwrap();
        pr.title = "T".repeat(300);
        pr.author = Some("a".repeat(40));
        let metadata = pr.pull_request.as_mut().unwrap();
        metadata.additions = Some(1001);
        metadata.deletions = Some(0);
        for count in [2, 20] {
            snapshot.issues = (0..count)
                .map(|index| Issue {
                    key: IssueKey {
                        native_id: index.to_string(),
                        ..pr.key.clone()
                    },
                    ..pr.clone()
                })
                .collect();
            let mut app = AppState {
                tab: InboxTab::PullRequests,
                ..Default::default()
            };
            // Resize both ways; scrollbar reservation must affect the breakpoint too.
            for table_width in [119, 120, 121, 133, 159, 160, 161, 160, 159, 120, 119] {
                let screen_width = table_width
                    + 7
                    + if count > 2 {
                        2
                    } else {
                        0
                    };
                let buffer = render_buffer(screen_width, 24, &snapshot, &mut app);
                let (row, index, key) = app.mouse.rows[1].clone();
                assert_eq!(row.width, table_width);
                let columns = pr_columns(row, 3);
                let expected_cells = match table_width {
                    160.. => 8,
                    120.. => 6,
                    _ => 4,
                };
                assert_eq!(columns[3].width, expected_cells);
                assert_eq!(
                    columns[1].width,
                    table_width - 27 - expected_cells - columns[5].width - 1
                );
                assert_eq!(columns[2].width, 12);
                assert_eq!(columns[4].width, 6);
                assert_eq!(columns[4].right(), row.right());
                for (column, label) in columns
                    .iter()
                    .zip(["PR", "title", "author", "diff", "status", "activity"])
                {
                    let x = if label == "status" {
                        column.right() - label.len() as u16
                    } else {
                        column.x
                    };
                    let actual: String = (x..x + label.len() as u16)
                        .map(|x| buffer[(x, app.mouse.list.y - 1)].symbol())
                        .collect();
                    assert_eq!(actual, label);
                }
                for (column, expected) in [
                    (
                        columns[1],
                        format!("{}…", "T".repeat(usize::from(columns[1].width - 1))),
                    ),
                    (columns[2], format!("{}…", "a".repeat(11))),
                    (columns[3], "■".repeat(usize::from(expected_cells))),
                    (columns[4], "  open".to_owned()),
                ] {
                    let actual: String = (column.x..column.right())
                        .map(|x| buffer[(x, row.y)].symbol())
                        .collect();
                    assert_eq!(actual, expected);
                }
                assert!(!mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::Moved,
                    row.right(),
                    row.y
                ));
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::Moved,
                    columns[3].right() - 1,
                    row.y
                ));
                assert_eq!(app.selected, index);
                let selected_buffer = render_buffer(screen_width, 24, &snapshot, &mut app);
                assert_eq!(
                    selected_buffer[(row.right() - 1, row.y)].bg,
                    theme::element()
                );
                assert!(mouse(
                    &mut app,
                    &snapshot,
                    MouseEventKind::Down(MouseButton::Left),
                    row.right() - 1,
                    row.y
                ));
                assert_eq!(app.detail_issue_key, Some(key));
                app.route = Route::Inbox;
                app.selected = 0;
            }
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
                layout: crate::LayoutMode::Fixed,
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
                            confidential: false,
                            model: None,
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
        let mut snapshot = pr_snapshot();
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
        for counts in [
            (Some(0), Some(0)),
            (Some(u64::MAX), Some(u64::MAX)),
            (None, Some(17)),
        ] {
            let pr = snapshot.issues[1].pull_request.as_mut().unwrap();
            pr.additions = counts.0;
            pr.deletions = counts.1;
            let text = render(112, 48, &snapshot, &mut app);
            let expected = format!(
                "+{} -{}",
                counts.0.map_or_else(|| "?".to_owned(), |n| n.to_string()),
                counts.1.map_or_else(|| "?".to_owned(), |n| n.to_string()),
            );
            assert!(text.contains(&expected));
        }
    }

    #[test]
    fn activity_counts_are_compact_partial_and_warm_without_overflow() {
        for (count, expected) in [
            (0, "0"),
            (999, "999"),
            (1000, "1k"),
            (1999, "1k"),
            (1_000_000, "1M"),
            (u128::from(u64::MAX), "18E"),
        ] {
            assert_eq!(compact_activity_count(count), expected);
        }
        let mut snapshot = pr_snapshot();
        let issue = &mut snapshot.issues[1];
        for (comments, reviews, commits, expected, color) in [
            (None, None, None, "≡?      ○?", theme::muted()),
            (Some(0), Some(0), Some(0), "≡0      ○0", theme::muted()),
            (Some(0), None, None, "≡0+     ○?", theme::muted()),
            (None, Some(5), Some(1), "≡5+     ○1", theme::muted()),
            (Some(5), Some(5), Some(10), "≡10     ○10", theme::primary()),
            (
                Some(u64::MAX),
                Some(u64::MAX),
                Some(u64::MAX),
                "≡36E    ○18E",
                theme::primary(),
            ),
        ] {
            issue.activity = Some(agent_launcher_core::ItemActivity {
                comments,
                review_comments: reviews,
                commits,
            });
            let line = item_activity_line(issue, true);
            assert_eq!(line.to_string(), expected);
            assert_eq!(line.spans[0].style.fg, Some(color));
            assert!(line.width() <= 15);
            assert_eq!(item_activity_line(issue, false).width(), 8);
        }
        issue.pull_request = None;
        issue.activity = Some(agent_launcher_core::ItemActivity {
            comments: Some(3),
            review_comments: Some(9),
            commits: None,
        });
        assert_eq!(item_activity_line(issue, false).to_string(), "≡3      ");
    }

    #[test]
    fn activity_headers_rows_and_hover_align_across_unicode_resize_breakpoints() {
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut snapshot = pr_snapshot();
            for issue in &mut snapshot.issues {
                issue.title = "界 e\u{301} café ".repeat(30);
                issue.activity = Some(agent_launcher_core::ItemActivity {
                    comments: Some(1234),
                    review_comments: Some(0),
                    commits: Some(42),
                });
            }
            let mut app = AppState {
                tab,
                ..Default::default()
            };
            for table_width in [71, 72, 75, 76, 99, 100, 120, 100, 76, 72, 40] {
                let buffer = render_buffer(
                    table_width
                        + if table_width >= 73 {
                            7
                        } else {
                            5
                        },
                    24,
                    &snapshot,
                    &mut app,
                );
                let row = app.mouse.rows[0].0;
                assert_eq!(row.width, table_width);
                let (x, width) = if tab == InboxTab::PullRequests {
                    let columns = pr_columns(row, 3);
                    assert_eq!(columns[4].right(), row.right());
                    (columns[5].x, columns[5].width)
                } else {
                    let columns = Columns::for_width(row.width);
                    (
                        row.right() - columns.state as u16 - columns.activity as u16,
                        columns.activity as u16,
                    )
                };
                if width > 0 {
                    let header: String = (x..x + 8)
                        .map(|x| buffer[(x, app.mouse.list.y - 1)].symbol())
                        .collect();
                    assert_eq!(header, "activity");
                    assert_eq!(buffer[(x, row.y)].symbol(), "≡");
                    assert_eq!(buffer[(x, row.y)].fg, theme::primary());
                    assert_eq!(buffer[(x, row.y)].bg, theme::element());
                    assert_eq!(buffer[(x + 1, row.y)].symbol(), "1");
                    if width >= 15 {
                        assert_eq!(buffer[(x + 8, row.y)].symbol(), "○");
                    }
                    mouse(&mut app, &snapshot, MouseEventKind::Moved, x, row.y);
                    assert_eq!(app.selected, 0);
                    assert!(app.detail_issue_key.is_none());
                }
            }
        }
    }

    #[test]
    fn details_show_exact_activity_and_explicit_unknowns_for_both_tabs() {
        for tab in [InboxTab::Issues, InboxTab::PullRequests] {
            let mut snapshot = pr_snapshot();
            let index = usize::from(tab == InboxTab::PullRequests);
            snapshot.issues[index].activity = Some(agent_launcher_core::ItemActivity {
                comments: Some(12345),
                review_comments: Some(0),
                commits: Some(u64::MAX),
            });
            let mut app = AppState {
                tab,
                ..Default::default()
            };
            app.open_detail(&snapshot);
            let text = render(112, 60, &snapshot, &mut app);
            assert!(text.contains("12345"));
            assert!(!text.contains("12k"));
            if tab == InboxTab::PullRequests {
                assert!(text.contains("review comments"));
                assert!(text.contains(&u64::MAX.to_string()));
            } else {
                assert!(!text.contains("review comments"));
            }
            snapshot.issues[index].activity = None;
            let text = render(112, 60, &snapshot, &mut app);
            assert!(
                text.lines()
                    .any(|line| line.contains("comments") && line.contains("unknown"))
            );
        }
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
            settings: Default::default(),
            prompt: Default::default(),
            issue_key: snapshot.issues[1].key.clone(),
            cursor: 0,
            stage: DispatchStage::Target { profile: None },
        });
        let text = render(90, 28, &snapshot, &mut app);
        assert!(text.contains("Review PR: compute target"));
        assert!(text.contains("Enter settings"));
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
            confidential: false,
            model: None,
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
    fn footer_identifies_the_effective_remote_without_exposing_its_url() {
        let mut snapshot = normal_snapshot();
        let repository = snapshot.repository.as_mut().unwrap();
        repository.root = PathBuf::from("/a/very/long/local/worktree/unrelated-to-the-remote");
        let remote = repository.remote.as_mut().unwrap();
        remote.repository = "moltis-org/moltis".into();
        remote.url = "https://secret@github.com/moltis-org/moltis".into();
        for host in ["github.com", "git.example.com"] {
            snapshot
                .repository
                .as_mut()
                .unwrap()
                .remote
                .as_mut()
                .unwrap()
                .host = host.into();
            let text = render(112, 28, &snapshot, &mut AppState::default());
            assert!(text.contains(&format!("{host}/moltis-org/moltis")));
            assert!(!text.contains("secret"));
            assert!(!text.contains("unrelated-to-the-remote"));
        }
        snapshot.repository.as_mut().unwrap().remote = None;
        snapshot.repository.as_mut().unwrap().root = PathBuf::from("/local-only");
        assert!(render(112, 28, &snapshot, &mut AppState::default()).contains("/local-only"));
    }

    #[test]
    fn normal_inbox_has_opencode_visual_contract_and_columns() {
        let text = render(112, 28, &normal_snapshot(), &mut AppState {
            layout: crate::LayoutMode::Fixed,
            ..Default::default()
        });
        assert!(text.contains(theme::AGENT_LOGO[0]));
        assert!(text.contains(theme::LAUNCHER_LOGO[0]));
        assert!(!text.contains("agents · issues · workspaces"));
        assert!(!text.contains("Filter issues..."));
        assert!(text.contains("Search issues…"));
        assert!(text.contains(" Issues    PRs    Security   newest first"));
        assert!(!text.contains("1 source"));
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
        assert!(text.contains("\u{f09b} github.com/acme/launcher"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(!text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn tall_inbox_uses_upper_whitespace_for_live_agent_activity() {
        let mut snapshot = normal_snapshot();
        snapshot.herdr_activity.enabled = true;
        let mut app = AppState {
            layout: crate::LayoutMode::Fixed,
            ..Default::default()
        };
        let before = render(112, 48, &snapshot, &mut app);
        let now = Utc::now();
        snapshot
            .herdr_activity
            .samples
            .push(crate::activity::sample(
                now,
                1,
                agent_launcher_core::ActivityCompleteness::Complete,
            ));
        let after = render(112, 48, &snapshot, &mut app);

        assert!(before.contains("Herdr activity"));
        assert!(before.contains("unobserved"));
        assert!(!before.contains("quiet"));
        for label in ["peak 0 agents", "-15m", "now"] {
            assert!(before.contains(label));
        }
        assert_ne!(before, after);
        assert!(after.contains("1 working"));
        assert!(after.contains("1/1 sessions"));
        assert!(after.contains("Repair runtime dispatch"));
    }

    #[test]
    fn live_activity_counts_markers_and_narrow_coverage() {
        use agent_launcher_core::{ActivityCompleteness, HerdrActivitySnapshot};
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for width in [24, 48, 64, 112, 180, 454] {
            for count in [0, 1, 3, 1000, u64::MAX] {
                let mut sample = crate::activity::sample(now, count, ActivityCompleteness::Partial);
                sample.fresh_endpoints = 1;
                sample.expected_endpoints = 2;
                let snapshot = HerdrActivitySnapshot {
                    enabled: true,
                    samples: vec![sample],
                    ..Default::default()
                };
                let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
                terminal
                    .draw(|frame| {
                        draw_activity_panel(frame, frame.area(), &snapshot, now, false, None, None)
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let text = (0..8)
                    .map(|y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("1/2 partial"), "{width}: {text}");
                assert!(!text.contains('?'));
                assert!(!text.contains('~'));
                assert!(text.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)));
                if width >= 112 {
                    assert!(text.contains(&format!("peak {count} agents")));
                    assert!(text.contains(&format!(">={count} working")));
                    assert!(!text.contains("discovery incomplete"));
                }
            }
        }
        let snapshot = HerdrActivitySnapshot {
            enabled: true,
            samples: vec![crate::activity::sample(
                now,
                0,
                ActivityCompleteness::Complete,
            )],
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(454, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_activity_panel(frame, frame.area(), &snapshot, now, false, None, None)
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(451, 5)].symbol(), "\u{2840}");
        assert_eq!(buffer[(450, 5)].symbol(), " ");
    }

    #[test]
    fn activity_keeps_details_in_debug_instead_of_graph() {
        use agent_launcher_core::{ActivityCompleteness, ActivityCounts, HerdrActivitySnapshot};
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let mut sample = crate::activity::sample(now, 7, ActivityCompleteness::Partial);
        sample.counts = Some(ActivityCounts {
            working: 7,
            blocked: 2,
            unseen_done: 1,
            idle: 3,
            unknown: 4,
        });
        sample.expected_endpoints = 5;
        sample.fresh_endpoints = 2;
        sample.stale_endpoints = 1;
        sample.failed_endpoints = 2;
        sample.never_observed_endpoints = 2;
        sample.excluded_endpoints = 3;
        let snapshot = HerdrActivitySnapshot {
            enabled: true,
            samples: vec![sample],
            persistence_error: Some("private diagnostic".to_owned()),
            discovery_error: Some("private diagnostic".to_owned()),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(240, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_activity_panel(frame, frame.area(), &snapshot, now, false, None, None)
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..8)
            .map(|y| {
                (0..240)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(">=7 working | 2 blocked"));
        assert!(text.contains("2/5 partial"));
        for label in [
            "unseen done",
            "idle",
            "unknown",
            "stale",
            "failed",
            "excluded",
            "error",
        ] {
            assert!(!text.contains(label), "{label}: {text}");
        }
        let mut runtime = normal_snapshot();
        runtime.herdr_activity = snapshot;
        let text = render(240, 100, &runtime, &mut AppState {
            debug_overlay: true,
            ..Default::default()
        });
        for label in [
            "7 working | 2 blocked | 1 unseen done | 3 idle | 4 unknown",
            "2/5 fresh",
            "1 stale",
            "2 failed",
            "2 never observed",
            "3 excluded",
            "inventory complete: false",
            "Scope: configured sessions",
            "Discovery error:",
            "Persistence error:",
        ] {
            assert!(text.contains(label), "{label}: {text}");
        }
        assert!(!text.contains("private diagnostic"));
    }

    #[test]
    fn live_activity_scope_and_freshness_survive_default_and_narrow_widths() {
        use agent_launcher_core::{ActivityCompleteness, HerdrActivitySnapshot};
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for width in [24, 48, 64, 104] {
            for remote in [false, true] {
                for state in [
                    "complete",
                    "failed",
                    "partial",
                    "discovering",
                    "unobserved",
                    "disabled",
                ] {
                    let mut sample =
                        crate::activity::sample(now, 3, ActivityCompleteness::Complete);
                    sample.expected_endpoints = 5;
                    sample.fresh_endpoints = 4;
                    sample.stale_endpoints = 1;
                    sample.failed_endpoints = 1;
                    sample.excluded_endpoints = 2;
                    sample.inventory_complete = state != "partial";
                    let snapshot = HerdrActivitySnapshot {
                        enabled: state != "disabled",
                        discover_remote_sessions: remote,
                        discovering: state == "discovering",
                        discovery_error: (state == "failed").then(|| "permission".into()),
                        persistence_error: Some("sample-write-failed".into()),
                        samples: if state == "unobserved" {
                            vec![]
                        } else {
                            vec![sample]
                        },
                        ..Default::default()
                    };
                    let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
                    terminal
                        .draw(|frame| {
                            draw_activity_panel(
                                frame,
                                frame.area(),
                                &snapshot,
                                now,
                                false,
                                None,
                                None,
                            )
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let header = (0..width)
                        .map(|x| buffer[(x, 1)].symbol())
                        .collect::<String>();
                    if state == "disabled" {
                        assert!(header.contains("disabled"), "{header}");
                        assert!(!header.contains("unobserved"));
                        assert!(!header.contains("fresh"));
                        continue;
                    }
                    assert!(
                        header.contains(if state == "unobserved" {
                            "unobserved"
                        } else {
                            "4/5"
                        }),
                        "{width} {state}: {header}"
                    );
                    let incomplete = matches!(state, "failed" | "partial")
                        || (remote && matches!(state, "discovering" | "unobserved"));
                    if state != "unobserved" {
                        assert!(
                            header.contains(if incomplete {
                                "partial"
                            } else {
                                "sessions"
                            }),
                            "{width} {state}: {header}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn animated_demo_uses_shared_widget_styles_and_never_leaves_underlines() {
        use std::time::Duration;
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for (width, height) in [(24, 4), (48, 6), (150, 8), (600, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let frames = (0..60).map(|frame| {
                Duration::from_secs(18) + Duration::from_nanos(frame * (1_000_000_000 / 30))
            });
            for elapsed in [
                Duration::ZERO,
                Duration::from_secs(9),
                Duration::from_secs(18) - Duration::from_nanos(1),
                Duration::MAX,
            ]
            .into_iter()
            .chain(frames)
            {
                let snapshot = crate::activity::demo_snapshot(elapsed, now);
                terminal
                    .draw(|frame| {
                        let area = frame.area();
                        frame
                            .buffer_mut()
                            .set_style(area, Style::new().add_modifier(Modifier::UNDERLINED));
                        draw_activity_panel(frame, area, &snapshot, now, true, None, Some(elapsed));
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for y in 2..height - 1 - u16::from(height >= 7) {
                    for x in 2..width - 2 {
                        let cell = &buffer[(x, y)];
                        assert!(!cell.modifier.contains(Modifier::UNDERLINED));
                        if cell
                            .symbol()
                            .chars()
                            .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
                        {
                            assert_eq!(cell.fg, theme::primary());
                            assert_eq!(cell.bg, theme::panel());
                        }
                    }
                }
                if height >= 7 && elapsed >= Duration::from_secs(18) {
                    assert!(buffer.content.iter().any(|cell| {
                        cell.modifier.contains(Modifier::DIM)
                            && cell
                                .symbol()
                                .chars()
                                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
                    }));
                    let footer = (0..width)
                        .map(|x| buffer[(x, height - 2)].symbol())
                        .collect::<String>();
                    assert!(footer.contains("peak 8 agents"));
                }
            }
        }
    }

    #[test]
    fn demo_activity_scrolls_with_bounded_counts_and_real_sample_cadence() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        use std::time::Duration;
        assert!(
            crate::activity::demo_snapshot(Duration::ZERO, now)
                .samples
                .is_empty()
        );
        assert_eq!(
            crate::activity::demo_snapshot(Duration::from_millis(80), now)
                .samples
                .len(),
            2
        );
        assert_eq!(
            crate::activity::demo_snapshot(Duration::from_millis(17920), now)
                .samples
                .len(),
            448
        );
        let before = crate::activity::demo_snapshot(Duration::from_secs(18), now);
        let after = crate::activity::demo_snapshot(Duration::from_millis(18080), now);
        assert_eq!(before.samples.len(), 450);
        assert_eq!(after.samples.len(), 450);
        for (a, b) in before.samples[2..].iter().zip(&after.samples) {
            assert_eq!(a.counts, b.counts);
            assert_eq!(a.completeness, b.completeness);
        }
        assert_eq!(before.samples.last().unwrap().sampled_at, now);
        assert!(
            before
                .samples
                .windows(2)
                .all(|s| s[1].sampled_at - s[0].sampled_at == chrono::Duration::seconds(2))
        );
        assert!(
            crate::activity::demo_snapshot(Duration::from_secs(u64::from(u32::MAX)), now)
                .samples
                .iter()
                .filter_map(|s| s.counts.as_ref())
                .all(|c| c.working <= 8)
        );
    }

    #[test]
    fn live_and_demo_share_compact_panel_geometry_glyphs_and_styles() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let snapshot = crate::activity::demo_snapshot(std::time::Duration::from_secs(18), now);
        for (width, height) in [(24, 8), (48, 8), (64, 8), (104, 8), (48, 6), (104, 6)] {
            let graph_bottom = if height == 8 {
                6
            } else {
                5
            };
            let mut live = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut demo = Terminal::new(TestBackend::new(width, height)).unwrap();
            live.draw(|frame| {
                draw_activity_panel(frame, frame.area(), &snapshot, now, false, None, None)
            })
            .unwrap();
            demo.draw(|frame| {
                draw_activity_panel(frame, frame.area(), &snapshot, now, true, None, None)
            })
            .unwrap();
            let live = live.backend().buffer();
            let demo = demo.backend().buffer();
            for y in 0..height {
                if y != 1 {
                    for x in 0..width {
                        assert_eq!(live[(x, y)], demo[(x, y)], "{width}: {x},{y}");
                    }
                }
            }
            for buffer in [live, demo] {
                let header = (0..width)
                    .map(|x| buffer[(x, 1)].symbol())
                    .collect::<String>();
                assert!(header.contains("2/2 sessions"), "{width}: {header}");
                for verbose in [
                    "0 blocked",
                    "idle",
                    "unknown",
                    "stale",
                    "failed",
                    "never",
                    "excluded",
                    "coverage",
                    "peak-per-bucket",
                ] {
                    assert!(!header.contains(verbose), "{header}");
                }
                let graph_rows = (2..graph_bottom)
                    .filter(|&y| {
                        (2..width - 2).any(|x| {
                            buffer[(x, y)]
                                .symbol()
                                .chars()
                                .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
                        })
                    })
                    .count();
                assert!(graph_rows >= 3, "{width}: only {graph_rows} graph rows");
                assert!((2..graph_bottom).any(|y| {
                    (2..width - 2).any(|x| buffer[(x, y)].modifier.contains(Modifier::DIM))
                }));
                assert!(
                    (2..width - 2)
                        .any(|x| (2..graph_bottom).all(|y| buffer[(x, y)].symbol() == " "))
                );
                for y in 0..height {
                    for x in [0, 1, width - 2, width - 1] {
                        assert!(
                            !buffer[(x, y)]
                                .symbol()
                                .chars()
                                .any(|c| c.is_ascii_alphanumeric())
                        );
                    }
                }
            }
            let demo_header = (0..width)
                .map(|x| demo[(x, 1)].symbol())
                .collect::<String>();
            assert!(demo_header.contains("demo"));
            if width == 104 {
                let live_header = (0..width)
                    .map(|x| live[(x, 1)].symbol())
                    .collect::<String>();
                assert_eq!(
                    live_header.split_whitespace().collect::<Vec<_>>(),
                    demo_header
                        .replace(" | demo", "")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn activity_panel_bottom_padding_has_no_border_artifacts() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for (width, height) in [(24, 4), (48, 6), (104, 8)] {
            for tick in [0, 112, 225] {
                let snapshot = crate::activity::demo_snapshot(
                    std::time::Duration::from_millis(tick * 80),
                    now,
                );
                for demo in [false, true] {
                    let mut terminal =
                        Terminal::new(TestBackend::new(width + 4, height + 4)).unwrap();
                    let area = Rect::new(2, 2, width, height);
                    terminal
                        .draw(|frame| {
                            draw_activity_panel(frame, area, &snapshot, now, demo, None, None);
                        })
                        .unwrap();
                    for x in area.x..area.right() {
                        let cell = &terminal.backend().buffer()[(x, area.bottom() - 1)];
                        assert_eq!(cell.symbol(), " ");
                        assert_eq!(cell.bg, theme::panel());
                    }
                }
            }
        }
    }

    #[test]
    fn activity_logo_is_centered_muted_and_cropped_before_whole_recorded_columns() {
        use agent_launcher_core::{
            ActivityCompleteness::{Complete, Missing},
            HerdrActivitySnapshot,
        };
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let old = Some(now - chrono::Duration::minutes(16));
        for (width, height) in [
            (24, 4),
            (104, 4),
            (24, 5),
            (65, 8),
            (66, 8),
            (104, 8),
            (454, 6),
        ] {
            let area = Rect::new(3, 2, width, height);
            let graph = Rect::new(5, 4, width - 4, height - 3 - u16::from(height >= 7));
            let render_panel = |snapshot: &HerdrActivitySnapshot, demo, origin| {
                let mut terminal = Terminal::new(TestBackend::new(width + 6, height + 4)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(Paragraph::new("outside"), Rect::new(0, 0, 7, 1));
                        draw_activity_panel(frame, area, snapshot, now, demo, origin, None);
                    })
                    .unwrap();
                terminal.backend().buffer().clone()
            };
            let empty = HerdrActivitySnapshot::default();
            let initial = render_panel(&empty, false, None);
            let full = graph.width >= 62 && graph.height >= 2;
            let lines = if full {
                (0..2)
                    .map(|i| format!("{}   {}", theme::AGENT_LOGO[i], theme::LAUNCHER_LOGO[i]))
                    .collect::<Vec<_>>()
            } else {
                vec!["agent launcher".to_owned()]
            };
            for (i, line) in lines.iter().enumerate() {
                let x = graph.x + (graph.width - line.chars().count() as u16) / 2;
                let y = graph.y + (graph.height - lines.len() as u16) / 2 + i as u16;
                for (offset, c) in line.chars().enumerate() {
                    let cell = &initial[(x + offset as u16, y)];
                    assert_eq!(cell.symbol(), c.to_string());
                    assert_eq!(cell.fg, theme::muted());
                    assert_eq!(cell.bg, theme::panel());
                }
            }
            let mut snapshots = [0, 1, 112, 225, 226, 450]
                .map(|tick| {
                    crate::activity::demo_snapshot(std::time::Duration::from_millis(tick * 80), now)
                })
                .to_vec();
            for completeness in [Complete, Missing] {
                for age in [0, 450, 899, 960] {
                    snapshots.push(HerdrActivitySnapshot {
                        samples: vec![crate::activity::sample(
                            now - chrono::Duration::seconds(age),
                            0,
                            completeness,
                        )],
                        ..Default::default()
                    });
                }
            }
            snapshots.push(HerdrActivitySnapshot {
                samples: vec![crate::activity::sample(
                    now + chrono::Duration::nanoseconds(1),
                    8,
                    Complete,
                )],
                ..Default::default()
            });
            for snapshot in snapshots {
                let live = render_panel(&snapshot, false, None);
                let demo = render_panel(&snapshot, true, None);
                let bare = render_panel(&snapshot, false, old);
                let leading =
                    crate::activity::unobserved_columns(&snapshot, graph.width.into(), now, None)
                        as u16;
                for y in 0..height + 4 {
                    for x in 0..width + 6 {
                        let in_graph = graph.contains((x, y).into());
                        if in_graph {
                            assert_eq!(live[(x, y)], demo[(x, y)]);
                            let expected = if x < graph.x + leading {
                                &initial
                            } else {
                                &bare
                            };
                            assert_eq!(
                                live[(x, y)],
                                expected[(x, y)],
                                "{width}x{height} at {x},{y}, leading {leading}"
                            );
                        } else {
                            assert_eq!(live[(x, y)], bare[(x, y)]);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn live_history_origin_survives_empty_snapshots_and_small_terminal_resize() {
        let mut snapshot = normal_snapshot();
        let old = Utc::now() - chrono::Duration::minutes(16);
        snapshot
            .herdr_activity
            .samples
            .push(crate::activity::sample(
                old,
                0,
                agent_launcher_core::ActivityCompleteness::Missing,
            ));
        let mut app = AppState::default();
        render_buffer(40, 15, &snapshot, &mut app);
        assert_eq!(app.activity_history_origin, Some(old));
        snapshot.herdr_activity.samples.clear();
        for (width, height) in [(112, 48), (160, 80), (40, 15), (112, 48)] {
            let text = render(width, height, &snapshot, &mut app);
            assert_eq!(app.activity_history_origin, Some(old));
            if width >= 112 {
                assert!(!text.contains(theme::AGENT_LOGO[0]));
                assert!(!text.contains("agent launcher"));
            } else {
                assert!(text.contains("agent launcher"));
            }
        }
    }

    #[test]
    fn activity_missing_counts_are_not_observed_zero() {
        use agent_launcher_core::ActivityCompleteness::{Complete, Missing};
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        for width in [24, 48, 64, 104] {
            for completeness in [Complete, Missing] {
                let snapshot = agent_launcher_core::HerdrActivitySnapshot {
                    enabled: true,
                    samples: vec![crate::activity::sample(now, 0, completeness)],
                    ..Default::default()
                };
                let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
                terminal
                    .draw(|frame| {
                        draw_activity_panel(frame, frame.area(), &snapshot, now, false, None, None)
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let header = (0..width)
                    .map(|x| buffer[(x, 1)].symbol())
                    .collect::<String>();
                if completeness == Missing {
                    assert!(header.contains("0/1 missing"), "{header}");
                    assert!(!header.contains("working"));
                    assert!((2..6).all(|y| buffer[(width - 3, y)].symbol() == " "));
                } else {
                    assert!(
                        header.contains("0 working") || header.contains("0w"),
                        "{header}"
                    );
                    assert!(header.contains("1/1 sessions"));
                }
            }
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
            for overlay in 0..3 {
                let app = AppState {
                    tab,
                    command_overlay: overlay == 1,
                    debug_overlay: overlay == 2,
                    ..Default::default()
                };
                terminal
                    .draw(|frame| draw_search(frame, frame.area(), &app))
                    .unwrap();
                let cell = terminal.backend().buffer().cell((0, 0)).unwrap();
                assert_eq!(cell.symbol(), "S");
                if overlay != 0 {
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
        assert!(text.contains("i · Esc"));
        assert!(text.contains("x worktree · X issue"));
        assert!(text.contains("detail only; confirm"));
        assert!(!text.contains("i · x · Esc"));
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
                settings: Default::default(),
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 1,
                prompt: crate::app::PromptView {
                    name: "implementer".into(),
                    preview: Some(Ok("Repair runtime dispatch".into())),
                    ..Default::default()
                },
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
        assert!(text.contains("Enter next"));
    }

    #[test]
    fn prompt_preview_is_responsive_scrollable_and_dispatch_bottom_is_flat() {
        let mut snapshot = normal_snapshot();
        snapshot.issues[0].description = Some("ISSUE BODY MUST NOT APPEAR".into());
        snapshot.prompt_profiles = vec!["alpha".into()];
        for width in [120, 60] {
            let mut app = AppState {
                dispatch_overlay: Some(crate::app::DispatchOverlay {
                    settings: Default::default(),
                    issue_key: snapshot.issues[0].key.clone(),
                    cursor: 0,
                    stage: DispatchStage::Prompt,
                    prompt: crate::app::PromptView {
                        name: "alpha".into(),
                        preview: Some(Ok("{{ issue_text }}\n**literal stars**\nlast".into())),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| draw_dispatch_overlay(frame, frame.area(), &snapshot, &mut app))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |needle: &str| {
                (0..24)
                    .find(|y| {
                        let text: String = (0..width).map(|x| buffer[(x, *y)].symbol()).collect();
                        text.contains(needle)
                    })
                    .unwrap()
            };
            if width >= 90 {
                assert_eq!(row("1  alpha"), row("{{ issue_text }}"));
            } else {
                assert!(row("1  alpha") < row("{{ issue_text }}"));
            }
            let list_y = row("1  alpha");
            let list_x = (0..width)
                .find(|x| buffer[(*x, list_y)].symbol() == "a")
                .unwrap();
            let preview_y = row("{{ issue_text }}");
            let preview_x = (0..width)
                .find(|x| buffer[(*x, preview_y)].symbol() == "{")
                .unwrap();
            assert_ne!(theme::panel(), theme::element());
            assert_eq!(buffer[(list_x, list_y)].bg, theme::panel());
            assert_eq!(buffer[(list_x, list_y)].fg, theme::text());
            assert_eq!(buffer[(list_x - 6, list_y)].bg, theme::panel());
            assert_eq!(buffer[(list_x - 6, list_y)].symbol(), " ");
            assert_eq!(buffer[(preview_x, preview_y)].bg, theme::element());
            let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("**literal stars**"));
            assert!(!text.contains(snapshot.issues[0].description.as_deref().unwrap()));
            for x in 1..width - 1 {
                assert_eq!(buffer[(x, 22)].bg, theme::element());
                assert_eq!(buffer[(x, 22)].symbol(), " ");
            }
            let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
            view.preview = Some(Ok((0..100).map(|i| format!("line {i}\n")).collect()));
            view.scroll = u16::MAX;
            terminal
                .draw(|frame| draw_dispatch_overlay(frame, frame.area(), &snapshot, &mut app))
                .unwrap();
            let view = &app.dispatch_overlay.as_ref().unwrap().prompt;
            assert_eq!(view.scroll, view.scroll_max);
            assert!(view.scroll > 0);
        }
    }

    #[test]
    fn prompt_editor_and_errors_render_at_narrow_and_tiny_sizes() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                settings: Default::default(),
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 0,
                stage: DispatchStage::Prompt,
                prompt: crate::app::PromptView {
                    name: "alpha".into(),
                    error: Some("conflict: file changed".into()),
                    editor: Some(crate::app::PromptEditor {
                        name: "alpha".into(),
                        naming: false,
                        buffer: crate::widgets::editor::Editor::new(
                            "{{ issue_title }}\nraw source".into(),
                        ),
                        original: None,
                        discard: false,
                        busy: false,
                    }),
                    ..Default::default()
                },
            }),
            ..Default::default()
        };
        for (width, height) in [(120, 24), (40, 24), (20, 10), (1, 1)] {
            let text = render(width, height, &snapshot, &mut app);
            if width >= 20 {
                assert!(text.contains("conflict:"));
            }
            if width >= 40 {
                assert!(text.contains("{{ issue_title }}"));
                assert!(text.contains("conflict: file changed"));
                assert!(text.contains("Ctrl+S save"));
            }
        }
        app.dispatch_overlay.as_mut().unwrap().prompt.error =
            Some((0..100).map(|i| format!("error detail {i}\n")).collect());
        let text = render(40, 24, &snapshot, &mut app);
        assert!(text.contains("error detail 0"));
        assert!(text.contains("PgUp/PgDn scroll error"));
        let view = &mut app.dispatch_overlay.as_mut().unwrap().prompt;
        assert!(view.scroll_max > 0);
        view.scroll = view.scroll_max;
        let text = render(40, 24, &snapshot, &mut app);
        assert!(text.contains("error detail 99"));
        assert!(text.contains("{{ issue_title }}"));
    }

    #[test]
    fn dispatch_overlay_keeps_large_and_compact_selections_visible() {
        let mut snapshot = normal_snapshot();
        snapshot.prompt_profiles = (0..20).map(|index| format!("prompt-{index}")).collect();
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                settings: Default::default(),
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 15,
                prompt: crate::app::PromptView {
                    name: "prompt-15".into(),
                    ..Default::default()
                },
                stage: crate::app::DispatchStage::Prompt,
            }),
            ..AppState::default()
        };

        let normal = render(70, 12, &snapshot, &mut app);
        assert!(normal.contains("Choose prompt: prompt-15"));
        assert!(normal.contains("prompt-15"));

        let compact = render(40, 18, &snapshot, &mut app);
        assert!(compact.contains("Choose prompt"));
        assert!(compact.contains("prompt-15"));
        assert!(compact.contains("Enter next"));
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
                settings: Default::default(),
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 0,
                prompt: Default::default(),
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
    fn launch_settings_render_summary_manual_model_help_and_unicode_editor() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                settings: crate::app::LaunchSettings {
                    backend: Some(BackendKind::Herdr),
                    default_harness: "claude".into(),
                    ..Default::default()
                },
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 0,
                prompt: Default::default(),
                stage: DispatchStage::Settings {
                    profile: Some("reviewer".into()),
                    target: Some("host-42".into()),
                },
            }),
            ..Default::default()
        };
        let text = render(120, 40, &snapshot, &mut app);
        for expected in [
            "Launch settings",
            "Profile: reviewer",
            "Target: host-42",
            "Backend: herdr",
            "Configured default (claude)",
            "uses harness default",
            "opencode, claude, pi",
            "supported kinds",
            "sonnet",
            "openai/gpt-5.4",
            "not a discovered",
            "Enter launch",
            "Esc back",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        for (width, height) in [(60, 24), (30, 12), (10, 6), (1, 1)] {
            render(width, height, &snapshot, &mut app);
        }
        let settings = &mut app.dispatch_overlay.as_mut().unwrap().settings;
        settings.model_editor = Some(crate::widgets::editor::Editor {
            text: "界é".repeat(100),
            cursor: 5,
        });
        for (width, height) in [(120, 40), (60, 24), (30, 12), (10, 6), (1, 1)] {
            let text = render(width, height, &snapshot, &mut app);
            if width >= 60 {
                assert!(text.contains("Enter confirm field"));
                assert!(text.contains("does not launch"));
                assert!(text.contains('界'));
                assert!(text.contains('é'));
            }
        }
    }

    #[test]
    fn issue_instructions_render_focus_footer_and_scroll_unicode_without_reformatting() {
        let snapshot = normal_snapshot();
        let draft = "界é\n```rust\n  keep_indent();\n```";
        let mut app = AppState {
            dispatch_overlay: Some(crate::app::DispatchOverlay {
                settings: crate::app::LaunchSettings {
                    backend: Some(BackendKind::Herdr),
                    default_harness: "claude".into(),
                    instructions_editor: Some(crate::widgets::editor::Editor {
                        text: draft.into(),
                        cursor: draft.len(),
                    }),
                    instructions_focused: true,
                    ..Default::default()
                },
                issue_key: snapshot.issues[0].key.clone(),
                cursor: 0,
                prompt: Default::default(),
                stage: DispatchStage::Settings {
                    profile: Some("reviewer".into()),
                    target: None,
                },
            }),
            ..Default::default()
        };
        let text = render(120, 40, &snapshot, &mut app);
        for expected in [
            "Profile: reviewer",
            "Configured default (claude)",
            "Additional instructions (this dispatch only)",
            "profile unchanged",
            "Enter newline",
            "Tab settings",
            "Ctrl+S done",
            "界",
            "é",
            "  keep_indent();",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("Enter launch"));
        for (width, height) in [(60, 24), (30, 12), (10, 6), (1, 1)] {
            render(width, height, &snapshot, &mut app);
        }
        assert!(
            app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .instructions_focused
        );
        app.dispatch_overlay
            .as_mut()
            .unwrap()
            .settings
            .instructions_focused = false;
        let text = render(120, 40, &snapshot, &mut app);
        assert!(text.contains("Tab instructions"));
        assert!(text.contains("Enter launch"));
        assert!(
            !app.dispatch_overlay
                .as_ref()
                .unwrap()
                .settings
                .instructions_focused
        );
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
    fn footer_omits_the_detected_worktree_manager_and_agent() {
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
        assert!(text.contains("\u{f09b} github.com/acme/launcher"));
        assert!(!text.contains("herdr"));
        assert!(!text.contains("opencode"));
    }

    #[test]
    fn debug_renders_live_snapshot_and_scrolls_responsively() {
        let mut snapshot = normal_snapshot();
        snapshot
            .repository
            .as_mut()
            .unwrap()
            .remote
            .as_mut()
            .unwrap()
            .url = "https://secret-token@github.com/acme/launcher".into();
        snapshot.backends[0].message = Some("backend unavailable right now".into());
        snapshot.sources[0].connected = false;
        snapshot.sources[0].message = Some("GitHub throttled; retry after tomorrow".into());
        snapshot.error = Some("refresh failed".into());
        snapshot.compute_targets.push(compute_target(
            "remote",
            ComputeTargetAvailability::Offline,
            1,
            Some(2),
        ));
        snapshot.last_refreshed_at = Some(Utc::now());
        let mut app = AppState {
            debug_overlay: true,
            ..Default::default()
        };
        let text = render(112, 40, &snapshot, &mut app);
        for expected in [
            "Debug",
            "Backend selected:",
            "Worktree manager detected: superset",
            "Selected agent: opencode",
            "backend unavailable right now",
            "Local path: /repo",
            "Effective host/repo: github.com/acme/launcher",
            "Compute targets: 1",
            "Target remote: Offline | active: 1 / 2 | dispatchable: false",
            "disconnected",
            "GitHub throttled",
            "refresh failed",
            "Last refresh:",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("secret-token"));
        assert!(text.contains(&snapshot.last_refreshed_at.unwrap().to_rfc3339()));
        snapshot.selected_agent = "changed-agent".into();
        assert!(render(112, 40, &snapshot, &mut app).contains("changed-agent"));
        for (width, height) in [(0, 0), (1, 1), (10, 4), (24, 10), (40, 14), (80, 24)] {
            app.debug_scroll = u16::MAX;
            let text = render(width, height, &snapshot, &mut app);
            assert!(app.mouse.blocked);
            assert!(!text.contains("secret-token"));
            if app.debug_page_size > 0 && width > 0 {
                assert!(app.debug_scroll <= app.debug_scroll_max);
            }
        }
        let empty = RuntimeSnapshot::default();
        app.debug_scroll = 0;
        let text = render(100, 30, &empty, &mut app);
        for expected in [
            "Backend selected: none",
            "Selected agent: none",
            "Repository: unavailable",
            "Last refresh: never",
            "Backends: 0",
            "Sources: 0",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        app.debug_scroll = u16::MAX;
        let text = render(40, 12, &snapshot, &mut app);
        assert!(app.debug_scroll > 0);
        assert!(text.contains("retry after"));
        assert!(text.contains("tomorrow"));
        snapshot.sources.clear();
        render(112, 40, &snapshot, &mut app);
        assert_eq!(app.debug_scroll, 0);
    }

    #[test]
    fn debug_exposes_activity_endpoint_health_and_sanitized_reasons() {
        use agent_launcher_core::{
            ActivityEndpointHealth, ActivityFreshness, ActivityTransportState,
        };
        let mut snapshot = RuntimeSnapshot::default();
        snapshot.herdr_activity.enabled = true;
        snapshot.herdr_activity.discovery_error = Some("authentication".into());
        snapshot.herdr_activity.persistence_error = Some("sample-write-failed".into());
        for reason in [
            "permission",
            "authentication",
            "server-stopped",
            "unsupported-method",
        ] {
            snapshot
                .herdr_activity
                .endpoints
                .push(ActivityEndpointHealth {
                    endpoint_id: format!("local/{reason}"),
                    transport: ActivityTransportState::Failed,
                    freshness: ActivityFreshness::NeverObserved,
                    last_success_at: None,
                    error_kind: Some(reason.into()),
                });
        }
        snapshot
            .herdr_activity
            .endpoints
            .push(ActivityEndpointHealth {
                endpoint_id: "local/default\n\u{1b}[31m".into(),
                transport: ActivityTransportState::Reachable,
                freshness: ActivityFreshness::Stale,
                last_success_at: Some(chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap()),
                error_kind: Some("ssh://user:secret@host/raw-output\n\u{1b}".into()),
            });
        for width in [48, 104] {
            let mut app = AppState {
                debug_overlay: true,
                ..Default::default()
            };
            let mut text = render(width, 30, &snapshot, &mut app);
            for scroll in 1..=app.debug_scroll_max {
                app.debug_scroll = scroll;
                text.push_str(&render(width, 30, &snapshot, &mut app));
            }
            for label in [
                "Herdr activity: enabled",
                "Discovery error: authentication",
                "Persistence error: sample-write-failed",
                "Endpoint ID: local/permission",
                "Transport: Failed",
                "Freshness: NeverObserved",
                "Transport: Reachable",
                "Freshness: Stale",
                "Last success: never",
                "invalid diagnostic code",
            ] {
                assert!(text.contains(label), "{width} missing {label}");
            }
            for reason in [
                "permission",
                "authentication",
                "server-stopped",
                "unsupported-method",
            ] {
                assert!(text.contains(&format!("Error kind: {reason}")));
            }
            assert!(text.contains("local/default\\n\\u{1b}[31m"));
            assert!(!text.contains("secret"));
            assert!(!text.contains('\u{1b}'));
        }
        assert_eq!(activity_diagnostic_code(None), "none");
        assert_eq!(
            activity_diagnostic_code(Some("future-reason_code")),
            "future-reason_code"
        );
    }

    #[test]
    fn debug_exposes_persistent_log_and_nonfatal_logging_warning() {
        let snapshot = RuntimeSnapshot {
            diagnostic_log_path: Some("/data/repo/diagnostics.log".into()),
            diagnostic_log_error: Some("Diagnostic log unavailable; fallback: stderr".into()),
            last_failure: Some("operation=dispatch outcome=failed".into()),
            ..RuntimeSnapshot::default()
        };
        let mut app = AppState {
            debug_overlay: true,
            ..Default::default()
        };
        let text = render(112, 40, &snapshot, &mut app);
        assert!(text.contains("/data/repo/diagnostics.log"));
        assert!(text.contains("fallback: stderr"));
        assert!(text.contains("operation=dispatch outcome=failed"));
    }

    #[test]
    fn debug_mouse_geometry_blocks_background_before_and_after_drawing() {
        let snapshot = normal_snapshot();
        let mut app = AppState::default();
        render(80, 24, &snapshot, &mut app);
        let row = app.mouse.rows[0].0;
        app.debug_overlay = true;
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::ScrollDown,
        ] {
            assert!(!mouse(&mut app, &snapshot, kind, row.x, row.y));
        }
        render(40, 12, &snapshot, &mut app);
        let pane = app.mouse.debug;
        assert!(app.debug_scroll_max > 0);
        assert!(mouse(
            &mut app,
            &snapshot,
            MouseEventKind::ScrollDown,
            pane.x,
            pane.y
        ));
        assert_eq!(app.debug_scroll, 3);
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
        ] {
            assert!(!mouse(&mut app, &snapshot, kind, pane.x, pane.y));
        }
        assert_eq!((app.selected, app.scroll), (0, 0));
        app.debug_overlay = false;
        assert!(!mouse(
            &mut app,
            &snapshot,
            MouseEventKind::Down(MouseButton::Left),
            pane.x,
            pane.y
        ));
    }

    #[test]
    fn debug_command_is_discoverable_in_compact_overlay() {
        for (width, height) in [(24, 8), (40, 12), (80, 24)] {
            let mut app = AppState {
                command_overlay: true,
                ..Default::default()
            };
            let text = render(width, height, &normal_snapshot(), &mut app);
            assert!(text.contains("debug"), "{width}x{height}: {text}");
        }
    }

    #[test]
    fn footer_source_status_is_compact_and_colored() {
        let mut snapshot = normal_snapshot();
        for (connected, message, color) in [
            (true, None, theme::done()),
            (false, Some("connection failed"), theme::error()),
            (
                true,
                Some("GitHub throttled; retry after 2026-09-11"),
                theme::primary(),
            ),
            (
                false,
                Some("GitHub throttled; retry after 2026-09-11"),
                theme::primary(),
            ),
        ] {
            snapshot.sources[0].connected = connected;
            snapshot.sources[0].message = message.map(str::to_owned);
            let label = footer_source_label(&snapshot, InboxTab::Issues);
            assert_eq!(label.to_string(), "\u{f09b}");
            assert_eq!(label.spans[0].style.fg, Some(color));

            // Include widths that clip later spans, with and without host metrics.
            for width in [40, 60, 80, 112] {
                for sampled in [false, true] {
                    let mut app = AppState::default();
                    if sampled {
                        app.host_metrics.record(82, 67);
                    }
                    let buffer = render_buffer(width, 28, &snapshot, &mut app);
                    let icon = (0..width)
                        .map(|x| buffer.cell((x, 27)).unwrap())
                        .find(|cell| cell.symbol() == "\u{f09b}")
                        .expect("source icon remains visible");
                    assert_eq!(icon.fg, color);
                }
            }
        }

        snapshot.sources[0].message = None;
        snapshot.sources[0].connected = true;
        for name in ["gitlab:gitlab.com:acme/repo", "beads"] {
            snapshot.sources[0].name = name.into();
            assert_eq!(
                footer_source_label(&snapshot, InboxTab::Issues).to_string(),
                format!("{} ●", name.split(':').next().unwrap())
            );
        }
        snapshot.sources.push(normal_snapshot().sources.remove(0));
        for (first, second, label, color) in [
            (true, true, "2 sources ●", theme::done()),
            (true, false, "1/2 sources ●", theme::primary()),
            (false, true, "1/2 sources ●", theme::primary()),
            (false, false, "0/2 sources ●", theme::error()),
        ] {
            snapshot.sources[0].connected = first;
            snapshot.sources[1].connected = second;
            let status = footer_source_label(&snapshot, InboxTab::Issues);
            assert_eq!(status.to_string(), label);
            assert_eq!(status.spans[1].style.fg, Some(color));
        }
        for source in &mut snapshot.sources {
            source.connected = true;
        }
        snapshot.sources[1].message = Some("GitHub throttled; retry after later".into());
        let status = footer_source_label(&snapshot, InboxTab::Issues);
        assert_eq!(status.to_string(), "2 sources ●");
        assert_eq!(status.spans[1].style.fg, Some(theme::primary()));
        snapshot.sources.clear();
        assert_eq!(
            footer_source_label(&snapshot, InboxTab::Issues).to_string(),
            "no source"
        );
    }

    #[test]
    fn footer_clips_identity_without_overlapping_metrics_or_version() {
        let snapshot = normal_snapshot();
        let identity = "\u{f09b} github.com/acme/launcher";
        let version = env!("CARGO_PKG_VERSION");
        for width in 0..=120 {
            for sampled in [false, true] {
                let mut metrics = HostMetrics::default();
                if sampled {
                    metrics.record(82, 67);
                }
                let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
                terminal
                    .draw(|frame| {
                        draw_footer(frame, frame.area(), &snapshot, &metrics, InboxTab::Issues)
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let text: String = (0..width)
                    .map(|x| buffer.cell((x, 0)).unwrap().symbol())
                    .collect();
                let text = text.trim();
                if usize::from(width) >= version.len() {
                    assert!(text.ends_with(version), "width {width}: {text}");
                    let left = text.strip_suffix(version).unwrap().trim_end();
                    if let Some((left, metrics)) = left.split_once("CPU") {
                        assert!(left.starts_with("\u{f09b} github.com/acme/launcher"));
                        assert!(identity.starts_with(left.trim_end()));
                        assert!(metrics.contains("82% 15m"));
                        assert!(metrics.ends_with("MEM  67%"));
                    } else {
                        assert!(identity.starts_with(left), "width {width}: {text}");
                    }
                }
            }
        }
    }

    #[test]
    fn cpu_sparkline_uses_standard_bars_and_stays_in_footer_row() {
        let snapshot = normal_snapshot();
        let mut metrics = HostMetrics::default();
        for cpu in [0, 12, 25, 50, 75, 100, 100, 25, 100] {
            metrics.record(cpu, 64);
        }
        for width in [112, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
            terminal
                .draw(|frame| {
                    draw_footer(
                        frame,
                        Rect::new(0, 1, width, 3),
                        &snapshot,
                        &metrics,
                        InboxTab::Issues,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut bars = 0;
            for y in 0..5 {
                for x in 0..width {
                    let cell = &buffer[(x, y)];
                    if y != 3 {
                        assert_eq!(cell.symbol(), " ");
                    }
                    for ch in cell.symbol().chars() {
                        assert!(!('\u{2800}'..='\u{28ff}').contains(&ch));
                        if ('\u{2581}'..='\u{2588}').contains(&ch) {
                            bars += 1;
                            assert_eq!(y, 3);
                            assert_eq!(cell.bg, theme::bg());
                            assert_eq!(cell.fg, load_color(100));
                            assert!(!cell.modifier.contains(Modifier::UNDERLINED));
                        }
                    }
                }
            }
            assert!(bars > 0);
        }
    }

    #[test]
    fn cpu_footer_warms_up_before_showing_history() {
        let mut app = AppState::default();
        for count in 1..=3 {
            app.host_metrics.record(20, 55);
            let text = render(112, 28, &normal_snapshot(), &mut app);
            let footer = text.lines().last().unwrap();
            assert!(footer.contains("CPU  20%"));
            assert!(footer.contains("MEM  55%"));
            assert_eq!(footer.contains("warming up"), count < 3);
            assert_eq!(
                footer
                    .chars()
                    .any(|ch| ('\u{2581}'..='\u{2588}').contains(&ch)),
                count >= 3
            );
        }
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
    fn footer_omits_worktree_manager_setup_advice() {
        let mut snapshot = normal_snapshot();
        snapshot.backends = vec![BackendStatus {
            kind: BackendKind::Native,
            available: true,
            manager_running: false,
            message: None,
        }];
        snapshot.selected_backend = Some(BackendKind::Native);

        let text = render(112, 28, &snapshot, &mut AppState::default());
        assert!(!text.contains("please run this in a worktree manager"));
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
    fn normal_detail_has_square_panel_with_warm_headings_and_preserves_actions() {
        let snapshot = normal_snapshot();
        let mut app = AppState {
            layout: crate::LayoutMode::Fixed,
            route: Route::Detail,
            detail_issue_key: Some(snapshot.issues[0].key.clone()),
            ..AppState::default()
        };

        let text = render(88, 24, &snapshot, &mut app);
        assert!(!text.contains('┃'));
        assert!(text.contains("#7  Repair runtime dispatch"));
        assert!(text.contains("Description"));
        assert!(text.contains("Detailed acceptance criteria"));
        assert!(text.contains("d dispatch"));
        assert!(text.contains("i send input"));
        assert!(text.contains("x worktree"));
        assert!(text.contains("X issue"));
        assert!(text.contains("Esc back"));
        assert!(!text.contains('┌'));
        let buffer = render_buffer(88, 24, &snapshot, &mut app);
        assert_eq!(buffer[(4, 1)].fg, theme::primary());
        assert!(
            buffer[(4, 1)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        for y in 0..23 {
            for x in 0..88 {
                let expected = if (2..86).contains(&x) && (1..22).contains(&y) {
                    if y < 4 {
                        theme::element()
                    } else {
                        theme::panel()
                    }
                } else {
                    theme::bg()
                };
                assert_eq!(buffer[(x, y)].bg, expected, "at {x},{y}");
            }
        }
        let body = app.mouse.detail;
        assert_eq!(buffer[(body.x, body.y)].symbol(), "I");
        assert_eq!(buffer[(body.x, body.y)].fg, theme::primary());
        assert!(
            buffer[(body.x, body.y)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        for y in 1..body.bottom() {
            let rail = &buffer[(body.x - 2, y)];
            assert_eq!(rail.symbol(), " ");
            assert_ne!(rail.fg, theme::primary());
        }
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
        assert!(!text.contains('┃'));
        assert!(text.contains("#7  Repair runtime dispatch"));
        assert!(text.contains("github · acme/launcher · open"));
        assert!(text.contains("d dispatch"));
        assert!(text.contains("i input"));
        assert!(text.contains("Esc back"));
    }

    #[test]
    fn detail_controls_keep_full_labels_when_they_fit() {
        let snapshot = pr_snapshot();
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                let mut app = AppState {
                    layout,
                    tab,
                    ..Default::default()
                };
                assert!(app.open_detail(&snapshot));
                for width in [42, 44, 56, 80, 88, 100, 107, 108, 160] {
                    let text = render(width, 24, &snapshot, &mut app);
                    for label in [
                        "i send input",
                        "x worktree",
                        "X issue",
                        "o open",
                        "s stop",
                        "Esc back",
                    ] {
                        assert!(
                            text.contains(label),
                            "{layout:?} {tab:?} width {width}: missing {label}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn detail_surfaces_fill_header_body_and_controls_including_unicode_and_tiny_sizes() {
        let mut snapshot = normal_snapshot();
        snapshot.issues[0].title = "Unicode \u{754c} detail".into();
        snapshot.issues[0].description = Some("Body \u{754c} content".into());
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            let mut app = AppState {
                layout,
                ..Default::default()
            };
            assert!(app.open_detail(&snapshot));
            app.status_message = Some("Status \u{754c}".into());
            for (width, height) in [(18, 5), (24, 7), (40, 10), (88, 24), (160, 40)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let completed = terminal
                    .draw(|frame| draw(frame, &snapshot, &mut app))
                    .unwrap();
                let available = Rect::new(0, 0, width, height - 1).inner(Margin::new(
                    if width >= 56 {
                        2
                    } else {
                        1
                    },
                    u16::from(height >= 14),
                ));
                let panel_width = layout.content_width(available.width);
                let panel = Rect::new(
                    available.x + (available.width - panel_width) / 2,
                    available.y,
                    panel_width,
                    available.height,
                );
                let header_height = if panel.width < 18 || panel.height < 4 {
                    0
                } else if panel.height >= 12 {
                    3
                } else {
                    2
                };
                for y in panel.y..panel.bottom() {
                    for x in panel.x..panel.right() {
                        let expected = if y < panel.y + header_height {
                            theme::element()
                        } else {
                            theme::panel()
                        };
                        assert_eq!(
                            completed.buffer[(x, y)].bg,
                            expected,
                            "{layout:?} {width}x{height} at {x},{y}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn all_detail_kinds_style_only_description_and_keep_latest_run_reachable() {
        let mut snapshot = security_snapshot();
        snapshot.issues.push(pr_snapshot().issues.pop().unwrap());
        let source = "### Impact\n\n**Bold** *Italic* normal `inline`\n\n| Path | Risk |\n| --- | ---: |\n| `src/long_component/validation.rs` | high |\n\n```rust\n### literal code\n```";
        for issue in &mut snapshot.issues {
            issue.description = Some(source.into());
        }
        for tab in [InboxTab::Issues, InboxTab::PullRequests, InboxTab::Security] {
            let mut app = AppState {
                tab,
                ..Default::default()
            };
            assert!(app.open_detail(&snapshot));
            let buffer = render_buffer(120, 70, &snapshot, &mut app);
            let locate = |needle: &str| {
                (0..70)
                    .find_map(|y| {
                        let row = (0..120)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>();
                        row.find(needle).map(|x| (x as u16, y))
                    })
                    .unwrap_or_else(|| panic!("missing {needle}"))
            };
            let bold = buffer[locate("Bold")].clone();
            assert!(bold.modifier.contains(Modifier::BOLD));
            assert!(!bold.modifier.contains(Modifier::ITALIC));
            let italic = buffer[locate("Italic")].clone();
            assert!(italic.modifier.contains(Modifier::ITALIC));
            assert!(!italic.modifier.contains(Modifier::BOLD));
            let normal = buffer[locate("normal")].clone();
            assert!(
                !normal
                    .modifier
                    .intersects(Modifier::BOLD | Modifier::ITALIC)
            );
            assert_eq!(normal.bg, theme::panel());
            assert_eq!(buffer[locate("inline")].bg, theme::element());
            assert_eq!(buffer[locate("### literal code")].bg, theme::element());
            let heading = buffer[locate("Impact")].clone();
            assert_eq!(heading.fg, theme::primary());
            assert!(heading.modifier.contains(Modifier::BOLD));
            assert_eq!(buffer[locate("repository")].bg, theme::panel());
            assert_eq!(
                buffer[locate(&app.detail_issue(&snapshot).unwrap().title)].bg,
                theme::element()
            );
            let text = render(120, 70, &snapshot, &mut app);
            assert!(!text.contains("### Impact"));
            assert!(!text.contains("**Bold**"));
            assert!(!text.contains("```"));
            assert_eq!(
                app.detail_issue(&snapshot).unwrap().description.as_deref(),
                Some(source)
            );
            for width in [40, 80, 120, 40] {
                app.detail_scroll = u16::MAX;
                let text = render(width, 24, &snapshot, &mut app);
                assert_eq!(app.detail_scroll, app.detail_scroll_max);
                assert!(text.contains("Latest run"));
                assert!(text.contains(if tab == InboxTab::Issues {
                    "Not dispatched."
                } else {
                    "Not reviewed."
                }));
            }
            let key = app.detail_issue_key.as_ref().unwrap();
            snapshot
                .issues
                .iter_mut()
                .find(|i| &i.key == key)
                .unwrap()
                .description = Some("**Updated**".into());
            app.detail_scroll = 0;
            assert!(render(120, 70, &snapshot, &mut app).contains("Updated"));
            app.reset_detail();
        }
    }

    #[test]
    fn detail_actions_survive_small_and_tiny_layouts_without_rails() {
        let snapshot = pr_snapshot();
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                let mut app = AppState {
                    layout,
                    tab,
                    ..Default::default()
                };
                assert!(app.open_detail(&snapshot));
                for (width, height) in [(18, 5), (20, 5), (24, 7), (36, 10), (40, 10), (88, 4)] {
                    let text = render(width, height, &snapshot, &mut app);
                    assert!(!text.contains('┃'));
                    for action in [
                        if tab == InboxTab::PullRequests {
                            "d review PR"
                        } else {
                            "d dispatch"
                        },
                        "Esc",
                    ] {
                        assert!(
                            text.contains(action),
                            "{width}x{height}: missing {action}: {text}"
                        );
                    }
                    assert!(text.contains(if tab == InboxTab::PullRequests {
                        "#42"
                    } else {
                        "#7"
                    }));
                    assert!(app.dispatch_overlay.is_none());
                }
            }
        }
    }

    #[test]
    fn panel_detail_resize_keeps_mouse_insets_and_reaches_end_of_wrapped_content() {
        let mut snapshot = pr_snapshot();
        for issue in &mut snapshot.issues {
            issue.description =
                Some("Wide words and narrow wrapping \u{754c}\u{754c}\u{754c}. ".repeat(80));
            issue.blocked_by = vec!["dependency-1".into()];
        }
        for layout in [crate::LayoutMode::Fixed, crate::LayoutMode::Flexible] {
            for tab in [InboxTab::Issues, InboxTab::PullRequests] {
                let mut app = AppState {
                    layout,
                    tab,
                    ..Default::default()
                };
                assert!(app.open_detail(&snapshot));
                let key = app.detail_issue_key.clone();
                for width in [40, 88, 160, 240, 40] {
                    app.detail_scroll = u16::MAX;
                    let text = render(width, 24, &snapshot, &mut app);
                    assert_eq!(app.detail_scroll, app.detail_scroll_max);
                    assert!(text.contains(if tab == InboxTab::PullRequests {
                        "Not reviewed."
                    } else {
                        "Not dispatched."
                    }));
                    assert_eq!(app.detail_issue_key, key);
                    let body = app.mouse.detail;
                    // TestBackend receives only emitted cells, not wide-glyph continuation
                    // cells. Inspect the complete frame to assert every panel cell's style.
                    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                    let completed = terminal
                        .draw(|frame| draw(frame, &snapshot, &mut app))
                        .unwrap();
                    let buffer = completed.buffer;
                    for y in body.y..body.bottom() {
                        for x in body.x - 2..body.right() + 1 {
                            assert_eq!(
                                buffer[(x, y)].bg,
                                theme::panel(),
                                "{layout:?} {tab:?} width {width} at {x},{y}"
                            );
                        }
                    }
                    for (x, y) in [
                        (body.x - 1, body.y),
                        (body.right(), body.y),
                        (body.x, body.y - 1),
                        (body.x, body.bottom()),
                    ] {
                        assert!(!mouse(&mut app, &snapshot, MouseEventKind::ScrollUp, x, y));
                    }
                    assert!(mouse(
                        &mut app,
                        &snapshot,
                        MouseEventKind::ScrollUp,
                        body.right() - 1,
                        body.y
                    ));
                    assert_eq!(app.detail_scroll, app.detail_scroll_max.saturating_sub(3));
                }
                app.detail_scroll = 0;
                let text = render(112, 48, &snapshot, &mut app);
                for metadata in [
                    "dependency-1",
                    "octocat",
                    "runtime",
                    "P1",
                    "https://github.com/acme/launcher/issues/",
                ] {
                    assert!(text.contains(metadata), "missing {metadata}");
                }
            }
        }
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
            confidential: false,
            model: None,
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
        assert!(!text.contains('┃'));
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
            confidential: false,
            model: None,
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
    fn completed_opencode_resume_is_not_a_herdr_agent_id() {
        let mut snapshot = normal_snapshot();
        snapshot.runs.push(RunSummary {
            confidential: false,
            model: None,
            id: "away-run".into(),
            issue_key: snapshot.issues[0].key.canonical(),
            workspace: Some(WorkspaceRef {
                backend: BackendKind::Herdr,
                id: "workspace".into(),
                host: None,
                path: Some(PathBuf::from("/work/away tree")),
                branch: "away/fix".into(),
            }),
            agent: "opencode".into(),
            state: RunState::Completed,
            message: Some("Implemented the fix.\nTests passed.".into()),
            session_id: Some("ses_real123".into()),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        });
        let mut app = AppState::default();
        assert!(app.open_detail(&snapshot));
        let text = render(120, 60, &snapshot, &mut app);
        assert!(text.contains(" Manual "));
        assert!(text.contains("Implemented the fix."));
        assert!(text.contains("Tests passed."));
        assert!(text.contains("cd -- '/work/away tree' && opencode -s ses_real123"));
        snapshot.runs[0].session_id = Some("herdr-agent-123".into());
        let text = render(120, 60, &snapshot, &mut app);
        assert!(!text.contains("opencode -s"));
    }

    #[test]
    fn action_status_remains_visible_alongside_runtime_errors() {
        let mut snapshot = normal_snapshot();
        snapshot.error = Some("backend mutation failed".to_owned());
        let mut app = AppState {
            status_message: Some("workspace opened".to_owned()),
            ..AppState::default()
        };

        for status in [
            "opening workspace...",
            "workspace opened",
            "runtime error: open failed",
        ] {
            app.status_message = Some(status.into());
            let text = render(112, 40, &snapshot, &mut app);
            assert!(text.contains("backend mutation failed"));
            assert!(text.contains(status));
            app.route = Route::Detail;
            app.detail_issue_key = Some(snapshot.issues[0].key.clone());
            assert!(render(112, 40, &snapshot, &mut app).contains(status));
            app.route = Route::Inbox;
        }
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
