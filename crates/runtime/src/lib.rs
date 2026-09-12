//! Single-owner runtime state and command loop.

mod diagnostics;
mod error;
mod notification;
mod service;

pub use diagnostics::Diagnostics;
pub use error::{Error, Result};
pub use notification::{DesktopNotifier, NoopNotifier, NotifyRustNotifier};
pub use service::{RuntimeHandle, RuntimeService};
