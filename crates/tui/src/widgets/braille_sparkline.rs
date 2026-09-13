//! Original sparkline implementation inspired by the builder/Widget API and Braille
//! pixel approach of <https://github.com/penso/ratatui-braille-bar>.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::Widget,
};

/// A domain-independent observation. `None` is a gap, not an observed zero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SparklineSample {
    pub value: Option<u64>,
    /// A lower bound: never connected to neighbors, and marked by an underline.
    pub partial: bool,
}

/// Geometry at two horizontal dots per cell and four vertical dots per row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SparklineVariant {
    /// Connect adjacent complete samples; partial samples remain isolated dots.
    #[default]
    Line,
    /// Plot observations without interpolation.
    Dots,
    /// Fill each complete sample's column from zero; partial samples remain dots.
    Filled,
}

/// A borrowed, left-to-right Braille sparkline with no domain or theme dependencies.
///
/// Each sample occupies one horizontal dot. At most `area.width * 2` samples are
/// shown, without resampling or stretching. Missing and unused columns stay blank.
/// Zero is a visible bottom dot. Partial dots underline their entire shared cell.
///
/// ```
/// use agent_launcher_tui::widgets::{BrailleSparkline, SparklineSample, SparklineVariant};
/// use ratatui::style::{Color, Style};
/// let samples = [SparklineSample { value: Some(42), partial: false }];
/// let widget = BrailleSparkline::new(&samples)
///     .max(100)
///     .style(Style::new().fg(Color::Cyan))
///     .variant(SparklineVariant::Filled);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct BrailleSparkline<'a> {
    samples: &'a [SparklineSample],
    max: Option<u64>,
    style: Style,
    variant: SparklineVariant,
}

impl<'a> BrailleSparkline<'a> {
    /// Borrow samples; defaults to Line, automatic scaling, and the default style.
    pub fn new(samples: &'a [SparklineSample]) -> Self {
        Self {
            samples,
            max: None,
            style: Style::default(),
            variant: SparklineVariant::Line,
        }
    }

    /// Fix the scale ceiling. Larger observations clamp to it; zero uses one.
    pub fn max(mut self, max: u64) -> Self {
        self.max = Some(max);
        self
    }

    /// Scale to the largest visible observation, including partial lower bounds.
    /// Empty or all-zero data uses a ceiling of one. This is the default.
    pub fn auto_max(mut self) -> Self {
        self.max = None;
        self
    }

    /// Style the entire area, including blank cells. Partial dots add UNDERLINED.
    pub fn style(mut self, style: impl Into<Style>) -> Self {
        self.style = style.into();
        self
    }

    pub fn variant(mut self, variant: SparklineVariant) -> Self {
        self.variant = variant;
        self
    }
}

impl Widget for BrailleSparkline<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let clip = area.intersection(buf.area);
        if clip.is_empty() {
            return;
        }
        for y in clip.y..clip.bottom() {
            for x in clip.x..clip.right() {
                buf[(x, y)]
                    .set_symbol(" ")
                    .set_style(Style::new().remove_modifier(Modifier::UNDERLINED))
                    .set_style(self.style);
            }
        }
        let samples = &self.samples[..self.samples.len().min(usize::from(area.width) * 2)];
        let max = u128::from(
            self.max
                .unwrap_or_else(|| samples.iter().filter_map(|s| s.value).max().unwrap_or(0))
                .max(1),
        );
        let top = u128::from(area.height) * 4 - 1;
        let mut dot = |x: usize, y: u128, partial: bool| {
            let cell_x = u32::from(area.x) + (x / 2) as u32;
            let cell_y = u32::from(area.y) + (y / 4) as u32;
            if cell_x < u32::from(clip.x)
                || cell_x >= u32::from(clip.right())
                || cell_y < u32::from(clip.y)
                || cell_y >= u32::from(clip.bottom())
            {
                return;
            }
            let cell = &mut buf[(cell_x as u16, cell_y as u16)];
            let bits = cell.symbol().chars().next().unwrap_or(' ') as u32;
            let bits = if (0x2800..=0x28ff).contains(&bits) {
                bits - 0x2800
            } else {
                0
            };
            let mask = [[1, 2, 4, 64], [8, 16, 32, 128]][x % 2][(y % 4) as usize];
            cell.set_char(char::from_u32(0x2800 + (bits | mask)).unwrap());
            if partial {
                cell.set_style(Style::new().add_modifier(Modifier::UNDERLINED));
            }
        };
        let mut previous = None;
        for (x, sample) in samples.iter().enumerate() {
            let Some(value) = sample.value else {
                previous = None;
                continue;
            };
            // Round with exact integer arithmetic, even for a u64::MAX scale.
            let y = top - (u128::from(value).min(max) * top + max / 2) / max;
            dot(x, y, sample.partial);
            if sample.partial {
                previous = None;
                continue;
            }
            match self.variant {
                SparklineVariant::Line => {
                    if let Some(y1) = previous {
                        // Adjacent columns: the top endpoint owns the midpoint tie,
                        // matching the existing Canvas/Bresenham trace rasterization.
                        let distance = y.abs_diff(y1);
                        if distance > 0 {
                            for row in y.min(y1)..=y.max(y1) {
                                let upper = row - y.min(y1) <= distance / 2;
                                let column = if upper == (y1 < y) {
                                    x - 1
                                } else {
                                    x
                                };
                                dot(column, row, false);
                            }
                        }
                    }
                },
                SparklineVariant::Dots => {},
                SparklineVariant::Filled => {
                    for row in y..=top {
                        dot(x, row, false);
                    }
                },
            }
            previous = Some(y);
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        style::Color,
        symbols::Marker,
        widgets::canvas::{Canvas, Line, Points},
    };

    use super::*;

    const VARIANTS: [SparklineVariant; 3] = [
        SparklineVariant::Line,
        SparklineVariant::Dots,
        SparklineVariant::Filled,
    ];

    fn samples(data: &[(Option<u64>, bool)]) -> Vec<SparklineSample> {
        data.iter()
            .map(|&(value, partial)| SparklineSample { value, partial })
            .collect()
    }

    fn dots(
        width: u16,
        height: u16,
        data: &[(Option<u64>, bool)],
        max: u64,
        variant: SparklineVariant,
    ) -> Vec<(u16, u16)> {
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        BrailleSparkline::new(&samples(data))
            .max(max)
            .variant(variant)
            .render(buffer.area, &mut buffer);
        let mut dots = Vec::new();
        for x in 0..width {
            for y in 0..height {
                let glyph = buffer[(x, y)].symbol().chars().next().unwrap();
                if glyph == ' ' {
                    continue;
                }
                assert!(('\u{2801}'..='\u{28ff}').contains(&glyph));
                let bits = glyph as u32 - 0x2800;
                for (dx, masks) in [[1, 2, 4, 64], [8, 16, 32, 128]].iter().enumerate() {
                    for (dy, mask) in masks.iter().enumerate() {
                        if bits & mask != 0 {
                            dots.push((x * 2 + dx as u16, (height - 1 - y) * 4 + 3 - dy as u16));
                        }
                    }
                }
            }
        }
        dots.sort_unstable();
        dots
    }

    #[test]
    fn exact_dot_precision_and_extreme_scale() {
        for variant in VARIANTS {
            assert_eq!(
                dots(2, 1, &[0, 1, 2, 3].map(|n| (Some(n), true)), 3, variant),
                [(0, 0), (1, 1), (2, 2), (3, 3)]
            );
            for count in 0..8 {
                assert_eq!(dots(1, 2, &[(Some(count), true)], 7, variant), [(
                    0,
                    count as u16
                )]);
            }
            for (value, max, expected) in [
                (u64::MAX, u64::MAX, 7),
                (u64::MAX / 2, u64::MAX, 3),
                (u64::MAX, 1, 7),
                (0, 0, 0),
            ] {
                assert_eq!(dots(1, 2, &[(Some(value), true)], max, variant), [(
                    0, expected
                )]);
            }
        }
    }

    #[test]
    fn variants_have_distinct_geometry() {
        let data = [(Some(0), false), (Some(7), false)];
        let line = dots(1, 2, &data, 7, SparklineVariant::Line);
        assert_eq!(line.len(), 8);
        assert!(line.contains(&(0, 0)) && line.contains(&(1, 7)));
        assert_eq!(dots(1, 2, &data, 7, SparklineVariant::Dots), [
            (0, 0),
            (1, 7)
        ]);
        let filled = dots(1, 2, &data, 7, SparklineVariant::Filled);
        assert_eq!(
            filled,
            [(0, 0)]
                .into_iter()
                .chain((0..8).map(|y| (1, y)))
                .collect::<Vec<_>>()
        );
        assert_ne!(line, filled);
    }

    #[test]
    fn gaps_and_partial_samples_never_interpolate() {
        for variant in VARIANTS {
            for partial in [
                [(Some(0), true), (Some(7), false)],
                [(Some(0), false), (Some(7), true)],
                [(Some(0), true), (Some(7), true)],
            ] {
                if variant != SparklineVariant::Filled || partial[1].1 {
                    assert_eq!(dots(1, 2, &partial, 7, variant), [(0, 0), (1, 7)]);
                }
            }
            for gap_partial in [false, true] {
                let data = [
                    (Some(0), false),
                    (None, gap_partial),
                    (None, gap_partial),
                    (Some(7), true),
                ];
                assert_eq!(dots(2, 2, &data, 7, variant), [(0, 0), (3, 7)]);
            }
        }
        assert_eq!(
            dots(
                2,
                2,
                &[(Some(0), false), (None, false), (Some(7), false)],
                7,
                SparklineVariant::Line
            ),
            [(0, 0), (2, 7)]
        );
    }

    #[test]
    fn real_zero_empty_and_tiny_areas() {
        for variant in VARIANTS {
            assert_eq!(dots(1, 1, &[(Some(0), false); 2], 0, variant), [
                (0, 0),
                (1, 0)
            ]);
            assert_eq!(dots(1, 1, &[(Some(0), true), (None, true)], 1, variant), [
                (0, 0)
            ]);
            for (width, height) in [(0, 0), (0, 1), (1, 0), (1, 1), (2, 3)] {
                assert!(dots(width, height, &[], 0, variant).is_empty());
                assert!(dots(width, height, &[(None, true); 4], 1, variant).is_empty());
            }
        }
    }

    #[test]
    fn partial_flat_trace_has_noncolor_marker_and_shared_cells_are_conservative() {
        for variant in VARIANTS {
            for count in [0, 1] {
                let data = samples(&[
                    (Some(count), false),
                    (Some(count), false),
                    (Some(count), false),
                    (Some(count), true),
                    (None, true),
                ]);
                let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
                BrailleSparkline::new(&data)
                    .max(1)
                    .variant(variant)
                    .render(buffer.area, &mut buffer);
                if variant != SparklineVariant::Filled || count == 0 {
                    assert_eq!(buffer[(0, 0)].symbol(), buffer[(1, 0)].symbol());
                }
                assert!(!buffer[(0, 0)].modifier.contains(Modifier::UNDERLINED));
                assert!(buffer[(1, 0)].modifier.contains(Modifier::UNDERLINED));
                assert_eq!(buffer[(2, 0)].symbol(), " ");
                assert!(!buffer[(2, 0)].modifier.contains(Modifier::UNDERLINED));
            }
        }
    }

    #[test]
    fn auto_max_uses_only_visible_samples_and_can_override_fixed_max() {
        let data = samples(&[(Some(2), false), (Some(4), true), (Some(u64::MAX), false)]);
        for variant in VARIANTS {
            let mut automatic = Buffer::empty(Rect::new(0, 0, 1, 2));
            let mut fixed = automatic.clone();
            BrailleSparkline::new(&data)
                .max(100)
                .auto_max()
                .variant(variant)
                .render(automatic.area, &mut automatic);
            BrailleSparkline::new(&data)
                .max(4)
                .variant(variant)
                .render(fixed.area, &mut fixed);
            assert_eq!(automatic, fixed);
        }
    }

    #[test]
    fn rerender_clears_old_dots_and_partial_markers() {
        for variant in VARIANTS {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
            BrailleSparkline::new(&samples(&[(Some(1), true)]))
                .variant(variant)
                .render(buffer.area, &mut buffer);
            assert!(buffer[(0, 0)].modifier.contains(Modifier::UNDERLINED));
            BrailleSparkline::new(&[])
                .variant(variant)
                .render(buffer.area, &mut buffer);
            assert_eq!(buffer, Buffer::empty(buffer.area));
        }
    }

    #[test]
    fn clips_without_rescaling_or_writing_outside_area() {
        let data = samples(&[
            (Some(0), false),
            (Some(7), false),
            (None, true),
            (Some(3), true),
            (Some(6), false),
            (Some(1), false),
        ]);
        let area = Rect::new(5, 4, 3, 3);
        let style = Style::new()
            .fg(Color::Cyan)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD);
        for variant in VARIANTS {
            let widget = BrailleSparkline::new(&data)
                .max(7)
                .style(style)
                .variant(variant);
            let mut full = Buffer::empty(area);
            widget.render(area, &mut full);
            for bounds in [
                Rect::new(4, 3, 5, 5),
                Rect::new(6, 5, 3, 3),
                Rect::new(4, 3, 3, 3),
                Rect::new(20, 20, 1, 1),
            ] {
                let mut buffer = Buffer::empty(bounds);
                for cell in &mut buffer.content {
                    cell.set_symbol("!");
                }
                let before = buffer.clone();
                widget.render(area, &mut buffer);
                for y in bounds.y..bounds.bottom() {
                    for x in bounds.x..bounds.right() {
                        if area.contains((x, y).into()) {
                            assert_eq!(buffer[(x, y)], full[(x, y)]);
                            assert_eq!(buffer[(x, y)].fg, Color::Cyan);
                            assert_eq!(buffer[(x, y)].bg, Color::Black);
                            assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
                        } else {
                            assert_eq!(buffer[(x, y)], before[(x, y)]);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn line_matches_previous_canvas_rasterization() {
        // Check both slopes and midpoint ties against the former renderer.
        for height in 1..=4 {
            let top = height * 4 - 1;
            for a in 0..=top {
                for b in 0..=top {
                    let area = Rect::new(3, 2, 1, height);
                    let mut expected = Buffer::empty(area);
                    Canvas::default()
                        .marker(Marker::Braille)
                        .x_bounds([0.0, 1.0])
                        .y_bounds([0.0, f64::from(top)])
                        .paint(|ctx| {
                            ctx.draw(&Points {
                                coords: &[(0.0, f64::from(a)), (1.0, f64::from(b))],
                                color: Color::Reset,
                            });
                            ctx.draw(&Line::new(
                                0.0,
                                f64::from(a),
                                1.0,
                                f64::from(b),
                                Color::Reset,
                            ));
                        })
                        .render(area, &mut expected);
                    let mut actual = Buffer::empty(area);
                    BrailleSparkline::new(&samples(&[
                        (Some(u64::from(a)), false),
                        (Some(u64::from(b)), false),
                    ]))
                    .max(u64::from(top))
                    .render(area, &mut actual);
                    assert_eq!(actual, expected, "height={height}, a={a}, b={b}");
                }
            }
        }
    }
}
