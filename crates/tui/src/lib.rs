mod activity;
mod app;
mod away;
mod catalog;
mod detail;
mod error;
mod event_loop;
mod format;
mod metrics;
mod mouse;
mod render;
mod rows;
mod status;
mod theme;
pub mod widgets;

pub use error::Error;
pub use event_loop::run;

/// Sizing of the inbox, activity panel, and detail view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LayoutMode {
    /// Keep the original centered, width- and height-capped inbox.
    Fixed,
    /// Use the available width and height while retaining padding and controls.
    #[default]
    Flexible,
}

impl LayoutMode {
    pub(crate) fn content_width(self, available: u16) -> u16 {
        match self {
            Self::Fixed => available.min(104),
            Self::Flexible => available,
        }
    }
}
