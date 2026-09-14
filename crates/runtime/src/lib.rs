//! Single-owner runtime state and command loop.

mod activity;
mod diagnostics;
mod error;
mod notification;
mod ownership;
mod prioritize;
mod prompts;
mod service;

pub use diagnostics::Diagnostics;
pub use error::{Error, Result};
pub use notification::{DesktopNotifier, NoopNotifier, NotifyRustNotifier};
pub use ownership::RuntimeOwnership;
pub use prompts::{PromptDocument, discover_prompt_profiles};
pub use service::{RuntimeHandle, RuntimeService};
