use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

pub(crate) fn render(frame: &mut Frame<'_>) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new("agent-launcher")
            .alignment(Alignment::Center)
            .style(Style::new().add_modifier(Modifier::BOLD))
            .block(Block::new().borders(Borders::ALL)),
        header,
    );
    frame.render_widget(
        Paragraph::new("No agents configured yet.").alignment(Alignment::Center),
        body,
    );
    frame.render_widget(
        Line::from(" q / esc: quit ").alignment(Alignment::Center),
        footer,
    );
}
