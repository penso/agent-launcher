use ratatui::style::Color;

pub(crate) const BRAILLE_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) const AGENT_LOGO: &[&str] = &["▄▀█ █▀▀ █▀▀ █▄░█ ▀█▀", "█▀█ █▄█ ██▄ █░▀█ ░█░"];
pub(crate) const LAUNCHER_LOGO: &[&str] = &[
    "█░░ ▄▀█ █░█ █▄░█ █▀▀ █░█ █▀▀ █▀█",
    "█▄▄ █▀█ █▄█ █░▀█ █▄▄ █▀█ ██▄ █▀▄",
];

pub(crate) const fn bg() -> Color {
    Color::Rgb(10, 10, 10)
}

pub(crate) const fn element() -> Color {
    Color::Rgb(30, 30, 30)
}

pub(crate) const fn panel() -> Color {
    Color::Rgb(20, 20, 20)
}

pub(crate) const fn text() -> Color {
    Color::Rgb(238, 238, 238)
}

pub(crate) const fn muted() -> Color {
    Color::Rgb(128, 128, 128)
}

pub(crate) const fn primary() -> Color {
    Color::Rgb(250, 178, 131)
}

pub(crate) const fn secondary() -> Color {
    Color::Rgb(112, 112, 112)
}

pub(crate) const fn error() -> Color {
    Color::Rgb(224, 108, 117)
}

pub(crate) const fn border() -> Color {
    Color::Rgb(72, 72, 72)
}

pub(crate) const fn done() -> Color {
    Color::Rgb(0, 128, 128)
}
