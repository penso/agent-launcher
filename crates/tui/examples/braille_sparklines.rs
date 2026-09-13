//! Run with: cargo run -p agent-launcher-tui --example braille_sparklines
use std::{
    io,
    time::{Duration, Instant},
};

use agent_launcher_tui::widgets::{BrailleSparkline, SparklineSample, SparklineVariant};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Paragraph},
};

fn main() -> io::Result<()> {
    ratatui::run(|terminal| {
        let started = Instant::now();
        let interval = Duration::from_nanos(1_000_000_000 / 30);
        let mut deadline = started;
        loop {
            terminal.draw(|frame| {
                let [header, graphs] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
                    .areas(frame.area());
                frame.render_widget(Paragraph::new("Braille sparklines | q/Esc: quit\nSame data: gaps, visible zero, dim isolated partial dots"), header);
                let panels = if graphs.width >= 60 {
                    Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(graphs)
                } else {
                    Layout::vertical([Constraint::Ratio(1, 3); 3]).split(graphs)
                };
                let width = panels.iter().map(|area| area.width.saturating_sub(2)).min().unwrap_or(0);
                let samples = (0..usize::from(width) * 2).map(|index| {
                    let phase = (index as f64 + started.elapsed().as_secs_f64() * 8.0) % 80.0;
                    SparklineSample {
                        value: if (28.0..34.0).contains(&phase) { None } else if phase < 8.0 { Some(0) } else {
                            Some(((20.0 + 18.0 * (phase / 6.0).sin()) * 1024.0).round() as u64)
                        },
                        partial: (50.0..58.0).contains(&phase),
                    }
                }).collect::<Vec<_>>();
                for (area, variant) in panels.iter().zip([SparklineVariant::Line, SparklineVariant::Dots, SparklineVariant::Filled]) {
                    let block = Block::bordered().title(format!("{variant:?} | max 40"));
                    let mut inner = block.inner(*area);
                    inner.width = width;
                    frame.render_widget(block, *area);
                    frame.render_widget(BrailleSparkline::new(&samples).max(40 * 1024)
                        .style(Style::new().fg(Color::Cyan)).variant(variant), inner);
                }
            })?;
            deadline += interval;
            if deadline < Instant::now() {
                deadline = Instant::now() + interval;
            }
            if event::poll(deadline.saturating_duration_since(Instant::now()))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return Ok(());
            }
        }
    })
}
