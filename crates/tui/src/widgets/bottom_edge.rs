use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
};

pub(crate) fn render_bottom_edge(
    area: Rect,
    border_color: Color,
    content_bg: Color,
    background: Color,
    buffer: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    let y = area.y + area.height - 1;
    for x in area.x.saturating_add(1)..area.x + area.width {
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol("▀")
                .set_style(Style::new().fg(content_bg).bg(background));
        }
    }
    if let Some(cell) = buffer.cell_mut((area.x, y)) {
        cell.set_symbol("╹")
            .set_style(Style::new().fg(border_color).bg(background));
    }
}
