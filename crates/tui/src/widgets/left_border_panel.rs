use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Padding, Widget},
};

/// Renders a thick left rail outside a separately styled content surface.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LeftBorderPanel {
    border_color: Color,
    content_bg: Option<Color>,
    padding: Padding,
}

impl LeftBorderPanel {
    pub(crate) const fn new() -> Self {
        Self {
            border_color: Color::Reset,
            content_bg: None,
            padding: Padding::ZERO,
        }
    }

    #[must_use]
    pub(crate) const fn border_color(mut self, color: Color) -> Self {
        self.border_color = color;
        self
    }

    #[must_use]
    pub(crate) const fn content_bg(mut self, color: Color) -> Self {
        self.content_bg = Some(color);
        self
    }

    #[must_use]
    pub(crate) const fn padding(mut self, padding: Padding) -> Self {
        self.padding = padding;
        self
    }

    pub(crate) fn render(self, area: Rect, buffer: &mut Buffer) -> Rect {
        let border = Block::new()
            .borders(Borders::LEFT)
            .border_type(BorderType::Thick)
            .border_style(Style::new().fg(self.border_color));
        let content_area = border.inner(area);
        border.render(area, buffer);

        let mut content = Block::new().padding(self.padding);
        if let Some(background) = self.content_bg {
            content = content.style(Style::new().bg(background));
        }
        let inner = content.inner(content_area);
        content.render(content_area, buffer);
        inner
    }
}

impl Default for LeftBorderPanel {
    fn default() -> Self {
        Self::new()
    }
}
