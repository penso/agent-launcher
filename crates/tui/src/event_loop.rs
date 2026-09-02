use std::io::{self, stdout};

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{Error, render::render};

pub async fn run() -> Result<(), Error> {
    enable_raw_mode()?;
    let _cleanup = TerminalCleanup;

    let mut output = stdout();
    execute!(output, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    let mut events = EventStream::new();

    terminal.draw(render)?;

    while let Some(event) = events.next().await {
        match event? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let should_quit = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));

                if should_quit {
                    break;
                }
            },
            Event::Resize(..) => {
                terminal.draw(render)?;
            },
            _ => {},
        }
    }

    Ok(())
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}
