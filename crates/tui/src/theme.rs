use ratatui::style::Color;

pub(crate) const BRAILLE_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) const AGENT_LOGO: &[&str] = &["▄▀█ █▀▀ █▀▀ █▄░█ ▀█▀", "█▀█ █▄█ ██▄ █░▀█ ░█░"];
pub(crate) const LAUNCHER_LOGO: &[&str] = &[
    "█░░ ▄▀█ █░█ █▄░█ █▀▀ █░█ █▀▀ █▀█",
    "█▄▄ █▀█ █▄█ █░▀█ █▄▄ █▀█ ██▄ █▀▄",
];

pub(crate) const fn bg() -> Color {
    Color::Rgb(40, 40, 40)
}

pub(crate) const fn element() -> Color {
    Color::Rgb(80, 73, 69)
}

pub(crate) const fn panel() -> Color {
    Color::Rgb(60, 56, 54)
}

pub(crate) const fn text() -> Color {
    Color::Rgb(235, 219, 178)
}

pub(crate) const fn muted() -> Color {
    Color::Rgb(168, 153, 132)
}

pub(crate) const fn primary() -> Color {
    Color::Rgb(250, 178, 131)
}

pub(crate) const fn secondary() -> Color {
    Color::Rgb(146, 131, 116)
}

pub(crate) const fn error() -> Color {
    Color::Rgb(224, 108, 117)
}

/// Agent status colors, from Herdr GPUI's gruvbox palette so a run reads the
/// same here as in Herdr's agent list.
pub(crate) const fn status_working() -> Color {
    Color::Rgb(250, 189, 47)
}

pub(crate) const fn status_idle() -> Color {
    Color::Rgb(184, 187, 38)
}

pub(crate) const fn status_blocked() -> Color {
    Color::Rgb(251, 73, 52)
}

pub(crate) const fn warning() -> Color {
    Color::Rgb(250, 189, 47)
}

pub(crate) const fn border() -> Color {
    Color::Rgb(102, 92, 84)
}

pub(crate) const fn done() -> Color {
    Color::Rgb(142, 192, 124)
}
