//! Local, inert Markdown presentation. HTML stays literal; images show alt text only.
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_SOURCE: usize = 64 * 1024;
const MAX_EVENTS: usize = 65_536;
const MAX_LINES: usize = 16_000;
const MAX_EXPANDED: usize = 128 * 1024;
const MAX_CELLS: usize = 1_000_000;

#[derive(Clone, Copy)]
pub struct MarkdownTheme {
    pub text: Style,
    pub heading: Style,
    pub muted: Style,
    pub code: Style,
    pub link: Style,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        use crate::theme;
        Self {
            text: Style::new().fg(theme::text()),
            heading: Style::new().fg(theme::primary()).bold(),
            muted: Style::new().fg(theme::muted()),
            code: Style::new().fg(theme::text()).bg(theme::element()),
            link: Style::new().fg(theme::secondary()).underlined(),
        }
    }
}

fn capped(source: &str) -> &str {
    &source[..source.floor_char_boundary(MAX_SOURCE.min(source.len()))]
}

// No Debug implementation: neither source nor transformed private text belongs in logs.
#[derive(Default)]
pub(crate) struct MarkdownCache {
    entry: Option<(String, bool, u16, Text<'static>)>,
}

impl MarkdownCache {
    pub(crate) fn render(&mut self, source: &str, width: u16) -> Text<'static> {
        let truncated = source.len() > MAX_SOURCE;
        let key = capped(source);
        if let Some((old, cut, old_width, text)) = &self.entry
            && old == key
            && *cut == truncated
            && *old_width == width
        {
            return text.clone();
        }
        let text = render(source, width, MarkdownTheme::default());
        self.entry = Some((key.to_owned(), truncated, width, text.clone()));
        text
    }
}

fn safe(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace('\t', "    ")
}

fn push(line: &mut Line<'static>, text: &str, style: Style) {
    if let Some(last) = line.spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
    } else if !text.is_empty() {
        line.spans.push(Span::styled(text.to_owned(), style));
    }
}

/// Wrap by terminal grapheme width, retaining styles and whitespace. A grapheme wider
/// than the entire viewport is shown as ASCII Unicode escapes rather than discarded.
fn wrap(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut result = Vec::new();
    let mut current = Line::default().style(line.style);
    let mut used = 0;
    for span in line.spans {
        for word in span.content.split_word_bounds() {
            // A space that would overflow ends the line rather than starting
            // the next one, so wrapped prose never begins with a blank.
            if used > 0 && word.chars().all(|c| c == ' ') && used + word.width() > width {
                result.push(std::mem::replace(
                    &mut current,
                    Line::default().style(line.style),
                ));
                used = 0;
                continue;
            }
            if !word.chars().any(char::is_whitespace)
                && word.width() <= width
                && used + word.width() > width
            {
                result.push(std::mem::replace(
                    &mut current,
                    Line::default().style(line.style),
                ));
                used = 0;
            }
            for grapheme in word.graphemes(true) {
                if grapheme == "\n" {
                    result.push(std::mem::replace(
                        &mut current,
                        Line::default().style(line.style),
                    ));
                    used = 0;
                    continue;
                }
                let escaped;
                let text = if grapheme.width() > width {
                    escaped = grapheme
                        .chars()
                        .map(|c| format!("\\u{{{:x}}}", c as u32))
                        .collect::<String>();
                    escaped.as_str()
                } else {
                    grapheme
                };
                for g in text.graphemes(true) {
                    let size = g.width();
                    if used + size > width {
                        result.push(std::mem::replace(
                            &mut current,
                            Line::default().style(line.style),
                        ));
                        used = 0;
                    }
                    push(&mut current, g, span.style);
                    used += size;
                }
            }
        }
    }
    result.push(current);
    result
}

#[derive(Default)]
struct Table {
    align: Vec<Alignment>,
    rows: Vec<Vec<Line<'static>>>,
}

impl Table {
    fn render(self, width: usize, theme: MarkdownTheme, budget: usize) -> Vec<Line<'static>> {
        let columns = self.align.len();
        if columns == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        // At least two cells per column, so even wide graphemes fit inside a grid.
        if columns.saturating_mul(5).saturating_add(1) > width {
            let labels = self
                .rows
                .first()
                .map(|row| {
                    row.iter()
                        .cloned()
                        .map(|mut cell| {
                            cell.style = theme.heading;
                            wrap(cell, width)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (index, row) in self.rows.iter().enumerate() {
                for (i, cell) in row.iter().enumerate() {
                    if index > 0
                        && let Some(label) = labels.get(i)
                    {
                        out.extend(label.iter().take(budget.saturating_sub(out.len())).cloned());
                    }
                    if out.len() >= budget {
                        return out;
                    }
                    out.extend(
                        wrap(cell.clone(), width)
                            .into_iter()
                            .take(budget.saturating_sub(out.len())),
                    );
                    if out.len() >= budget {
                        return out;
                    }
                }
                out.push(Line::default());
                if out.len() >= budget {
                    return out;
                }
            }
            return out;
        }
        let available = width - columns * 3 - 1;
        let mut sizes = vec![2; columns];
        let mut desired = sizes.clone();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(columns) {
                desired[i] = desired[i].max(cell.width().min(available));
            }
        }
        let mut remaining = available - columns * 2;
        while remaining > 0 {
            let mut changed = false;
            for i in 0..columns {
                if sizes[i] < desired[i] && remaining > 0 {
                    sizes[i] += 1;
                    remaining -= 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let border = Line::styled(
            format!(
                "+{}+",
                sizes
                    .iter()
                    .map(|n| "-".repeat(n + 2))
                    .collect::<Vec<_>>()
                    .join("+")
            ),
            theme.muted,
        );
        out.push(border.clone());
        for (index, row) in self.rows.into_iter().enumerate() {
            let cells = row
                .into_iter()
                .zip(&sizes)
                .map(|(cell, size)| wrap(cell, *size))
                .collect::<Vec<_>>();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for y in 0..height {
                if out.len() >= budget {
                    return out;
                }
                let mut line = Line::default();
                push(&mut line, "|", theme.muted);
                for (i, size) in sizes.iter().enumerate() {
                    let cell = cells
                        .get(i)
                        .and_then(|c| c.get(y))
                        .cloned()
                        .unwrap_or_default();
                    let gap = size.saturating_sub(cell.width());
                    let left = match self.align[i] {
                        Alignment::Right => gap,
                        Alignment::Center => gap / 2,
                        _ => 0,
                    };
                    push(&mut line, &" ".repeat(left + 1), theme.text);
                    line.spans.extend(cell.spans);
                    push(&mut line, &" ".repeat(gap - left + 1), theme.text);
                    push(&mut line, "|", theme.muted);
                }
                out.push(line);
            }
            if index == 0 && out.len() < budget {
                out.push(border.clone());
            }
        }
        if out.len() < budget {
            out.push(border);
        }
        out
    }
}

struct Renderer {
    lines: Vec<Line<'static>>,
    line: Line<'static>,
    width: usize,
    theme: MarkdownTheme,
    quote: usize,
    code: bool,
    lists: Vec<Option<u64>>,
    remaining: usize,
    limit: usize,
}

impl Renderer {
    fn block_gap(&mut self) {
        self.flush();
        if self.lines.len() < self.limit
            && self.lines.last().is_some_and(|line| !line.spans.is_empty())
        {
            self.lines.push(Line::default());
        }
    }

    fn append(&mut self, text: &str, style: Style) {
        let end = text.floor_char_boundary(text.len().min(self.remaining));
        push(&mut self.line, &text[..end], style);
        self.remaining = self.remaining.saturating_sub(text.len());
    }

    fn flush(&mut self) {
        if self.line.spans.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.line);
        if self.code {
            line.style = self.theme.code;
        }
        let prefix = format!(
            "{}{}",
            "> ".repeat(self.quote.min(16)),
            "  ".repeat(self.lists.len().saturating_sub(1).min(16))
        );
        let prefix = if prefix.len() + 2 <= self.width {
            prefix
        } else {
            String::new()
        };
        for mut line in wrap(line, self.width - prefix.len()) {
            if self.lines.len() >= self.limit {
                break;
            }
            if !prefix.is_empty() {
                line.spans
                    .insert(0, Span::styled(prefix.clone(), self.theme.muted));
            }
            self.lines.push(line);
        }
    }
}

/// Render untrusted Markdown to owned, pre-wrapped terminal lines without I/O or logging.
pub fn render(source: &str, width: u16, theme: MarkdownTheme) -> Text<'static> {
    if width == 0 {
        return Text::default();
    }
    let width = usize::from(width).min(4096);
    let mut r = Renderer {
        lines: Vec::new(),
        line: Line::default(),
        width,
        theme,
        quote: 0,
        code: false,
        lists: Vec::new(),
        remaining: MAX_EXPANDED,
        limit: MAX_LINES.min(MAX_CELLS / width),
    };
    let input = safe(capped(source));
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    let mut style = theme.text;
    let mut styles = Vec::new();
    let mut links = Vec::new();
    let mut table: Option<Table> = None;
    let mut limited = source.len() > MAX_SOURCE;
    for (count, event) in Parser::new_ext(&input, options).enumerate() {
        if count >= MAX_EVENTS || r.lines.len() >= r.limit || r.remaining == 0 {
            limited = true;
            break;
        }
        match event {
            Event::Start(tag) => {
                styles.push(style);
                match tag {
                    Tag::Heading { .. } => {
                        r.block_gap();
                        style = style.patch(theme.heading);
                    },
                    Tag::Strong => style = style.bold(),
                    Tag::Emphasis => style = style.italic(),
                    Tag::Strikethrough => style = style.crossed_out(),
                    Tag::CodeBlock(kind) => {
                        r.block_gap();
                        r.code = true;
                        style = style.patch(theme.code);
                        if let CodeBlockKind::Fenced(language) = kind
                            && !language.is_empty()
                        {
                            r.append(&safe(&language), theme.muted.patch(theme.code));
                            r.flush();
                        }
                    },
                    Tag::BlockQuote(kind) => {
                        r.block_gap();
                        r.quote += 1;
                        if let Some(kind) = kind {
                            push(&mut r.line, &format!("{kind:?}"), theme.heading);
                            r.flush();
                        }
                    },
                    Tag::List(start) => {
                        if r.lists.is_empty() {
                            r.block_gap();
                        } else {
                            r.flush();
                        }
                        r.lists.push(start);
                    },
                    Tag::Item => {
                        r.flush();
                        let marker = match r.lists.last_mut() {
                            Some(Some(n)) => {
                                let marker = format!("{n}. ");
                                *n = n.saturating_add(1);
                                marker
                            },
                            _ => "- ".to_owned(),
                        };
                        push(&mut r.line, &marker, theme.muted);
                    },
                    Tag::Link { dest_url, .. } => {
                        links.push(safe(&dest_url));
                        style = style.patch(theme.link);
                    },
                    Tag::Image { .. } => push(&mut r.line, "[image: ", theme.muted),
                    Tag::Table(align) => {
                        r.block_gap();
                        table = Some(Table {
                            align,
                            rows: Vec::new(),
                        });
                    },
                    Tag::TableHead | Tag::TableRow => {
                        if let Some(table) = &mut table {
                            table.rows.push(Vec::new());
                        }
                        if matches!(tag, Tag::TableHead) {
                            style = style.patch(theme.heading);
                        }
                    },
                    _ => {},
                }
            },
            Event::End(tag) => {
                match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) if r.lists.is_empty() => r.block_gap(),
                    TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => r.flush(),
                    TagEnd::CodeBlock => {
                        r.flush();
                        r.code = false;
                        r.block_gap();
                    },
                    TagEnd::BlockQuote(_) => {
                        r.flush();
                        r.quote = r.quote.saturating_sub(1);
                        if r.quote == 0 && r.lists.is_empty() {
                            r.block_gap();
                        }
                    },
                    TagEnd::List(_) => {
                        r.flush();
                        r.lists.pop();
                        if r.lists.is_empty() {
                            r.block_gap();
                        }
                    },
                    TagEnd::Link => {
                        if let Some(url) = links.pop() {
                            r.append(&format!(" ({url})"), style);
                        }
                    },
                    TagEnd::Image => push(&mut r.line, "]", theme.muted),
                    TagEnd::TableCell => {
                        if let Some(row) = table.as_mut().and_then(|t| t.rows.last_mut()) {
                            row.push(std::mem::take(&mut r.line));
                        }
                    },
                    TagEnd::Table => {
                        if let Some(table) = table.take() {
                            r.lines.extend(table.render(
                                r.width,
                                theme,
                                r.limit.saturating_sub(r.lines.len()),
                            ));
                            r.block_gap();
                        }
                    },
                    _ => {},
                }
                style = styles.pop().unwrap_or(theme.text);
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                r.append(&safe(&text), style)
            },
            Event::Code(text) => r.append(&safe(&text), style.patch(theme.code)),
            Event::SoftBreak | Event::HardBreak => push(&mut r.line, "\n", style),
            Event::TaskListMarker(checked) => push(
                &mut r.line,
                if checked {
                    "[x] "
                } else {
                    "[ ] "
                },
                style,
            ),
            Event::Rule => {
                r.block_gap();
                r.lines.push(Line::styled("-".repeat(r.width), theme.muted));
                r.block_gap();
            },
            _ => {},
        }
    }
    r.flush();
    while r.lines.last().is_some_and(|line| line.spans.is_empty()) {
        r.lines.pop();
    }
    if limited || r.lines.len() >= r.limit || r.remaining == 0 {
        r.lines.extend(wrap(
            Line::styled("[Description display limit reached]", theme.muted),
            r.width,
        ));
    }
    Text::from(r.lines)
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::Modifier,
        widgets::{Paragraph, Wrap},
    };

    use super::*;

    fn plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn markdown(source: &str, width: u16) -> Text<'static> {
        render(source, width, MarkdownTheme::default())
    }

    #[test]
    fn preserves_source_breaks_and_paragraph_spacing() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = "## Summary\n\n**Product:** example\n**Version:** 1.0\n**Severity:** high\n\nFirst paragraph.\n\nSecond paragraph."
                .replace('\n', newline);
            assert_eq!(
                plain(&markdown(&source, 80)),
                "Summary\n\nProduct: example\nVersion: 1.0\nSeverity: high\n\nFirst paragraph.\n\nSecond paragraph."
            );
        }
        assert_eq!(
            plain(&markdown("first  \nsecond\nthird", 80)),
            "first\nsecond\nthird"
        );
        assert_eq!(
            plain(&markdown("Before\n\n- one\n- two\n\nAfter", 80)),
            "Before\n\n- one\n- two\n\nAfter"
        );
    }

    #[test]
    fn nested_styles_restore_and_literals_remain_literal() {
        let text = markdown(
            "### Heading\n\n**bold *both* bold** normal ~~gone~~\n\n`**literal**` \\*escaped\\* \"quoted\"",
            120,
        );
        assert_eq!(
            plain(&text),
            "Heading\n\nbold both bold normal gone\n\n**literal** *escaped* \"quoted\""
        );
        let spans = &text.lines[2].spans;
        assert!(spans.iter().any(|s| {
            s.content == "both"
                && s.style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::ITALIC)
        }));
        assert!(
            spans
                .iter()
                .any(|s| s.content.contains(" normal ") && s.style.add_modifier.is_empty())
        );
        assert!(
            spans.iter().any(
                |s| s.content == "gone" && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
            )
        );
        assert_eq!(
            text.lines[4].spans[0].style.bg,
            MarkdownTheme::default().code.bg
        );
    }

    #[test]
    fn lists_quotes_alerts_code_and_links_are_inert() {
        let source = "3. **parent**\n   - *child*\n   - [x] checked\n4. final\n\n> [!WARNING]\n> caution\n\n```rust\n### not a heading\nlet x = `raw`;\n```\n\n[site](https://example.test/a)\n\n---";
        let text = markdown(source, 80);
        let output = plain(&text);
        for expected in [
            "3. parent",
            "  - child",
            "  - [x] checked",
            "4. final",
            "> Warning",
            "> caution",
            "rust",
            "### not a heading",
            "site (https://example.test/a)",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
        let code = text
            .lines
            .iter()
            .find(|l| l.to_string().contains("### not"))
            .unwrap();
        assert_eq!(code.style.bg, MarkdownTheme::default().code.bg);
        let final_item = text
            .lines
            .iter()
            .find(|l| l.to_string().contains("4. final"))
            .unwrap();
        assert!(final_item.spans.iter().all(|s| {
            !s.style
                .add_modifier
                .intersects(Modifier::BOLD | Modifier::ITALIC)
        }));
    }

    #[test]
    fn html_images_and_encoded_terminal_controls_never_execute() {
        let source = "<b>literal</b> ![diagram](https://example.test/private.png)\n\n<script>alert(1)</script>\n\n\u{1b}]52;c;secret\u{7}\u{1b}[31mred\u{9b}31m\u{1}\u{7f}\n\n&#27; &#x9d; [url](https://example.test/&#27;)";
        let text = plain(&markdown(source, 120));
        assert!(text.contains("<b>literal</b>"));
        assert!(text.contains("[image: diagram]"));
        assert!(!text.contains("private.png"));
        assert!(text.contains("<script>alert(1)</script>"));
        assert!(text.chars().all(|c| !c.is_control() || c == '\n'));
    }

    #[test]
    fn advisory_tables_fit_align_and_preserve_styled_graphemes() {
        let path = "src/security/very_long_unbroken_component/validation.rs";
        let source = format!(
            "## Impact\n\n| Path | Center | Count |\n| :--- | :---: | ---: |\n| `{path}` | **\u{754c}\u{1f469}\u{200d}\u{1f4bb}e\u{301}** | 42 |\n| a  b | middle | 7 |"
        );
        for width in [40, 80, 120] {
            let text = markdown(&source, width);
            assert!(text.lines.iter().all(|l| l.width() <= usize::from(width)));
            let grid = text
                .lines
                .iter()
                .filter(|l| l.to_string().starts_with('|'))
                .collect::<Vec<_>>();
            let borders = grid[0]
                .spans
                .iter()
                .enumerate()
                .filter(|(_, s)| s.content == "|")
                .map(|(i, _)| grid[0].spans[..i].iter().map(Span::width).sum::<usize>())
                .collect::<Vec<_>>();
            for line in &grid {
                let positions = line
                    .spans
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.content == "|")
                    .map(|(i, _)| line.spans[..i].iter().map(Span::width).sum::<usize>())
                    .collect::<Vec<_>>();
                assert_eq!(positions, borders);
            }
            let code = grid
                .iter()
                .flat_map(|l| &l.spans)
                .filter(|s| s.style.bg == MarkdownTheme::default().code.bg)
                .map(|s| s.content.as_ref())
                .collect::<String>();
            assert_eq!(code, path);
            let unicode = grid
                .iter()
                .flat_map(|l| &l.spans)
                .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
                .map(|s| s.content.as_ref())
                .collect::<String>();
            assert!(unicode.contains("\u{754c}\u{1f469}\u{200d}\u{1f4bb}e\u{301}"));
            let count = grid
                .iter()
                .find(|l| l.to_string().contains("42"))
                .unwrap()
                .to_string();
            assert!(count.ends_with("42 |"));
            let center = grid
                .iter()
                .find(|l| l.to_string().contains("middle"))
                .unwrap()
                .to_string();
            let cell = center.split('|').nth(2).unwrap();
            assert!(cell.len() - cell.trim_end().len() >= cell.len() - cell.trim_start().len());
            assert!(cell.len() - cell.trim_end().len() <= cell.len() - cell.trim_start().len() + 1);
        }
    }

    #[test]
    fn tiny_tables_stack_without_losing_content_or_breaking_graphemes() {
        for width in 1..=20 {
            let text = markdown(
                "| Key | Value |\n| --- | --- |\n| abcdef | **longword** |",
                width,
            );
            assert!(text.lines.iter().all(|l| l.width() <= usize::from(width)));
            let output = plain(&text);
            let ordered = if width < 11 {
                output.clone()
            } else {
                (1..=2)
                    .map(|column| {
                        output
                            .lines()
                            .filter(|l| l.starts_with('|'))
                            .filter_map(|l| l.split('|').nth(column))
                            .collect::<String>()
                    })
                    .collect::<String>()
            };
            let compact = ordered
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>();
            assert!(compact.contains("abcdef"));
            assert!(compact.contains("longword"));
            if width < 11 {
                assert!(!plain(&text).contains('|'));
            }
        }
        let line = Line::raw("\u{754c}\u{1f469}\u{200d}\u{1f4bb}e\u{301}");
        let narrow = wrap(line.clone(), 1);
        assert!(narrow.iter().all(|l| l.width() <= 1));
        assert!(
            narrow
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
                .contains("\\u{754c}")
        );
        assert_eq!(
            wrap(line, 2)
                .iter()
                .map(ToString::to_string)
                .collect::<String>(),
            "\u{754c}\u{1f469}\u{200d}\u{1f4bb}e\u{301}"
        );
    }

    #[test]
    fn multiline_cells_retain_whitespace_styles_and_fit_paragraph_without_reflow() {
        let cell = Line::from(vec![
            Span::styled("a  b\n", Style::new().bold()),
            Span::styled("\u{754c}e\u{301} longword", Style::new().italic()),
        ]);
        let table = Table {
            align: vec![Alignment::Left],
            rows: vec![vec![Line::raw("Header")], vec![cell]],
        };
        let lines = table.render(12, MarkdownTheme::default(), MAX_LINES);
        assert!(lines.iter().all(|l| l.width() <= 12));
        assert!(lines.iter().any(|l| l.to_string().contains("a  b")));
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains('\u{754c}')
                    && s.style.add_modifier.contains(Modifier::ITALIC))
        );
        let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        assert_eq!(paragraph.line_count(12), lines.len());
        let mut terminal = Terminal::new(TestBackend::new(12, lines.len() as u16)).unwrap();
        terminal
            .draw(|f| f.render_widget(paragraph, f.area()))
            .unwrap();
        for (y, line) in lines.iter().enumerate() {
            assert_eq!(
                terminal.backend().buffer()[(0, y as u16)].symbol(),
                &line.to_string()[..1]
            );
        }
    }

    #[test]
    fn display_limits_bound_source_expansion_columns_and_output() {
        let long = "x".repeat(MAX_SOURCE + 1);
        let text = markdown(&long, 1);
        assert!(text.lines.len() <= MAX_LINES + 40);
        // At width 1 the wrap drops spaces, so compare without them.
        assert!(
            plain(&text)
                .replace(['\n', ' '], "")
                .contains("displaylimit")
        );
        let references = format!(
            "{}\n\n[ref]: https://example.test/{}",
            "[x][ref] ".repeat(2000),
            "x".repeat(30_000)
        );
        let text = markdown(&references, 80);
        assert!(plain(&text).contains("display limit"));
        assert!(text.lines.len() <= MAX_LINES + 1);
        let columns = format!(
            "|{}\n|{}\n|{}",
            " h |".repeat(4000),
            " --- |".repeat(4000),
            " v |".repeat(4000)
        );
        let text = markdown(&columns, 20);
        assert!(text.lines.iter().all(|l| l.width() <= 20));
        assert!(text.lines.len() <= MAX_LINES + 2);
        assert!(markdown("text", 0).lines.is_empty());
    }

    #[test]
    fn cache_tracks_width_source_and_truncation() {
        let mut cache = MarkdownCache::default();
        let first = cache.render("**bold** longword", 80);
        let pointer = cache.entry.as_ref().unwrap().0.as_ptr();
        assert_eq!(cache.render("**bold** longword", 80), first);
        assert_eq!(cache.entry.as_ref().unwrap().0.as_ptr(), pointer);
        assert!(cache.render("**bold** longword", 4).lines.len() > first.lines.len());
        assert_eq!(plain(&cache.render("new", 80)), "new");
        cache.render(&"x".repeat(MAX_SOURCE), 80);
        assert!(!cache.entry.as_ref().unwrap().1);
        cache.render(&"x".repeat(MAX_SOURCE + 1), 80);
        assert!(cache.entry.as_ref().unwrap().1);
    }

    #[test]
    fn wrapped_prose_never_starts_with_a_space() {
        let text = render("aaaa bbbb cccc", 9, MarkdownTheme::default());
        let lines: Vec<String> = text.lines.iter().map(ToString::to_string).collect();
        assert_eq!(lines, ["aaaa bbbb", "cccc"]);
        let code = render("```\n    indented\n```", 20, MarkdownTheme::default());
        assert!(
            code.lines
                .iter()
                .any(|line| line.to_string() == "    indented")
        );
    }
}
