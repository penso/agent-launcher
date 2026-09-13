use agent_launcher_core::{IssueKey, RuntimeSnapshot};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::app::{AppState, InboxTab, Route};

/// Rebuilt each frame, using the same rectangles as the rendered widgets.
#[derive(Default)]
pub(crate) struct MouseGeometry {
    pub screen: Rect,
    pub route: Route,
    pub tab: InboxTab,
    pub tabs: Vec<(Rect, InboxTab)>,
    pub rows: Vec<(Rect, usize, IssueKey)>,
    pub list: Rect,
    pub scroll: usize,
    pub detail: Rect,
    pub debug: Rect,
    pub blocked: bool,
}

pub(crate) fn handle_mouse(
    app: &mut AppState,
    event: MouseEvent,
    snapshot: &RuntimeSnapshot,
    size: (u16, u16),
) -> bool {
    if app.issue_delete_overlay.is_some() {
        return false;
    }
    let hit = &app.mouse;
    if app.debug_overlay {
        if (hit.screen.width, hit.screen.height) != size
            || !hit.debug.contains(Position::new(event.column, event.row))
        {
            return false;
        }
        let previous = app.debug_scroll;
        match event.kind {
            MouseEventKind::ScrollUp => app.debug_scroll = app.debug_scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                app.debug_scroll = app.debug_scroll.saturating_add(3).min(app.debug_scroll_max);
            },
            _ => return false,
        }
        return previous != app.debug_scroll;
    }
    if hit.blocked
        || app.input_overlay.is_some()
        || app.delete_overlay.is_some()
        || app.dispatch_overlay.is_some()
        || app.command_overlay
        || app.sort_overlay
        || hit.screen.is_empty()
        || (hit.screen.width, hit.screen.height) != size
        || hit.route != app.route
        || hit.tab != app.tab
        || hit.scroll != app.scroll
    {
        return false;
    }
    let position = Position::new(event.column, event.row);
    match event.kind {
        MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left)
            if app.route == Route::Inbox =>
        {
            let hover = event.kind == MouseEventKind::Moved;
            if let Some((_, tab)) = hit.tabs.iter().find(|(rect, _)| rect.contains(position)) {
                if hover || *tab == app.tab {
                    return false;
                }
                app.switch_tab();
                app.status_message = None;
            } else if let Some((_, index, key)) =
                hit.rows.iter().find(|(rect, ..)| rect.contains(position))
            {
                // Never open a different item if the snapshot/filter changed since drawing.
                let rows = app.rows(snapshot);
                if !rows.get(*index).is_some_and(|row| {
                    snapshot
                        .issues
                        .get(row.issue_idx)
                        .is_some_and(|issue| issue.key == *key)
                }) {
                    return false;
                }
                if hover {
                    if app.selected == *index {
                        return false;
                    }
                    app.selected = *index;
                    // Selection alone leaves hit geometry valid for an immediate click.
                    return true;
                }
                app.selected = *index;
                app.open_detail(snapshot);
            } else {
                return false;
            }
        },
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = event.kind == MouseEventKind::ScrollDown;
            if app.route == Route::Inbox && hit.list.contains(position) && app.visible_rows > 0 {
                let count = app.rows(snapshot).len();
                let max_scroll = count.saturating_sub(app.visible_rows);
                app.scroll = if down {
                    app.scroll.saturating_add(3).min(max_scroll)
                } else {
                    app.scroll.saturating_sub(3).min(max_scroll)
                };
                // Keep selection visible so keyboard navigation continues from this viewport.
                app.selected = app.selected.clamp(
                    app.scroll,
                    app.scroll
                        .saturating_add(app.visible_rows - 1)
                        .min(count.saturating_sub(1)),
                );
            } else if app.route == Route::Detail && hit.detail.contains(position) {
                app.detail_scroll = if down {
                    app.detail_scroll
                        .saturating_add(3)
                        .min(app.detail_scroll_max)
                } else {
                    app.detail_scroll.saturating_sub(3)
                };
            } else {
                return false;
            }
        },
        _ => return false,
    }
    // Wait for the resulting frame before accepting another hit on this geometry.
    app.mouse = MouseGeometry::default();
    true
}
