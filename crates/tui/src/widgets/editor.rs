use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Debug, Default)]
pub(crate) struct Editor {
    pub text: String,
    pub cursor: usize,
}

impl Editor {
    pub fn new(text: String) -> Self {
        Self { text, cursor: 0 }
    }

    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        (
            before.bytes().filter(|b| *b == b'\n').count(),
            before.rsplit('\n').next().unwrap_or("").chars().count(),
        )
    }

    pub fn key(&mut self, key: KeyEvent) {
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
        let next = self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8());
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
        match key.code {
            KeyCode::Left => self.cursor = previous,
            KeyCode::Right => self.cursor = next,
            KeyCode::Home => self.cursor = start,
            KeyCode::End => self.cursor = end,
            KeyCode::Up | KeyCode::Down => {
                let (row, column) = self.position();
                let target = if key.code == KeyCode::Up {
                    row.saturating_sub(1)
                } else {
                    row + 1
                };
                if let Some(line) = self.text.split('\n').nth(target) {
                    let offset: usize = self
                        .text
                        .split('\n')
                        .take(target)
                        .map(|s| s.len() + 1)
                        .sum();
                    self.cursor = offset
                        + line
                            .char_indices()
                            .nth(column)
                            .map_or(line.len(), |(i, _)| i);
                }
            },
            KeyCode::Backspace => {
                self.text.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            },
            KeyCode::Delete => {
                self.text.replace_range(self.cursor..next, "");
            },
            KeyCode::Enter => self.insert("\n"),
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.insert(&c.to_string())
            },
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_and_newline_cursor_editing() {
        let mut editor = Editor::new("é界\nabc".into());
        editor.key(KeyCode::Right.into());
        assert_eq!(editor.cursor, 2);
        editor.key(KeyCode::Down.into());
        assert_eq!(editor.position(), (1, 1));
        editor.key(KeyCode::Home.into());
        editor.key(KeyCode::Backspace.into());
        assert_eq!(editor.text, "é界abc");
        editor.key(KeyCode::Left.into());
        editor.key(KeyCode::Delete.into());
        assert_eq!(editor.text, "éabc");
        editor.key(KeyCode::Enter.into());
        assert_eq!(editor.text, "é\nabc");
        editor.insert("x\r\ny\u{1b}");
        assert_eq!(editor.text, "é\nx\nyabc");
    }
}
