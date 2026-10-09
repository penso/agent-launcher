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
    pub mode: Rect,
    /// Launch settings harness and model chips, and the picker open under one.
    pub launch_chips: Vec<(Rect, crate::app::PickerKind)>,
    pub launch_picker: Rect,
    pub launch_picker_rows: Vec<(Rect, usize)>,
    pub launch_instructions: Rect,
    /// The inbox listing before it is split, and the divider between the
    /// list and the preview (or the handle that reopens a closed preview).
    pub listing: Rect,
    pub divider: Rect,
}

pub(crate) fn handle_mouse(
    app: &mut AppState,
    event: MouseEvent,
    snapshot: &RuntimeSnapshot,
    size: (u16, u16),
) -> bool {
    if app.away_overlay.is_some() && !app.away_quit {
        return crate::away::handle_mouse(app, event, size);
    }
    if app.issue_delete_overlay.is_some() || app.away_quit {
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
    if app
        .dispatch_overlay
        .as_ref()
        .is_some_and(|o| matches!(o.stage, crate::app::DispatchStage::Settings { .. }))
    {
        if (hit.screen.width, hit.screen.height) != size {
            return false;
        }
        return handle_launch_settings(app, event);
    }
    // A divider drag continues until release, whatever the pointer crosses.
    if let Some(listing) = app.preview_drag {
        match event.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                let size = crate::render::preview_size_at(listing, event.column);
                let changed = size != app.preview_size;
                app.preview_size = size;
                return changed;
            },
            MouseEventKind::Up(_) | MouseEventKind::Down(_) => {
                app.preview_drag = None;
                return true;
            },
            _ => return false,
        }
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
    if event.kind == MouseEventKind::Down(MouseButton::Left) && hit.mode.contains(position) {
        crate::away::open(app, snapshot);
        return true;
    }
    if event.kind == MouseEventKind::Down(MouseButton::Left)
        && app.route == Route::Inbox
        && hit.divider.contains(position)
    {
        app.preview_drag = Some(hit.listing);
        return true;
    }
    match event.kind {
        MouseEventKind::Moved | MouseEventKind::Down(MouseButton::Left)
            if app.route == Route::Inbox =>
        {
            let hover = event.kind == MouseEventKind::Moved;
            if let Some((_, tab)) = hit.tabs.iter().find(|(rect, _)| rect.contains(position)) {
                if hover || *tab == app.tab {
                    return false;
                }
                app.set_tab(*tab);
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
                let count = app.list_len(snapshot);
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

/// Chips open their picker, picker rows choose, the wheel moves through the
/// list, and a click elsewhere closes it.
fn handle_launch_settings(app: &mut AppState, event: MouseEvent) -> bool {
    let position = Position::new(event.column, event.row);
    let hit = &app.mouse;
    let Some(overlay) = app.dispatch_overlay.as_mut() else {
        return false;
    };
    let settings = &mut overlay.settings;
    let mut closed = false;
    if let Some(picker) = &settings.picker {
        let items = settings.picker_items(picker.kind, &app.harness_catalog, &picker.filter);
        let picker = settings.picker.as_mut().unwrap();
        let row = hit
            .launch_picker_rows
            .iter()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, index)| *index);
        let inside = hit.launch_picker.contains(position);
        match event.kind {
            MouseEventKind::Moved => {
                return match row {
                    Some(index) if index != picker.cursor => {
                        picker.cursor = index;
                        true
                    },
                    _ => false,
                };
            },
            MouseEventKind::ScrollUp if inside => {
                picker.cursor = picker.cursor.saturating_sub(1);
                return true;
            },
            MouseEventKind::ScrollDown if inside => {
                picker.cursor = (picker.cursor + 1).min(items.len().saturating_sub(1));
                return true;
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = row {
                    if let Some(item) = items.into_iter().nth(index) {
                        settings.choose(item.action);
                        app.status_message = None;
                    }
                    return true;
                }
                if inside {
                    return false;
                }
                // Close, then let the click land on whatever is under it.
                settings.picker = None;
                closed = true;
            },
            _ => return false,
        }
    }
    if event.kind != MouseEventKind::Down(MouseButton::Left) {
        return false;
    }
    if let Some((_, kind)) = hit
        .launch_chips
        .iter()
        .find(|(rect, _)| rect.contains(position))
    {
        settings.open_picker(*kind, &app.harness_catalog);
        return true;
    }
    if settings.instructions_editor.is_some() && hit.launch_instructions.contains(position) {
        settings.model_editor = None;
        settings.instructions_focused = true;
        return true;
    }
    // A click that only closed the picker still needs a redraw.
    closed
}
