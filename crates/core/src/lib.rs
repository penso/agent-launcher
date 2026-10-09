//! Shared domain types for agent-launcher.

/// The release version (`YYYYMMDD.N`) when built by the release workflow,
/// otherwise the crate version.
pub const VERSION: &str = match option_env!("AGENT_LAUNCHER_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

mod activity;
mod away;
mod config;
mod event;
mod issue;
mod repository;
mod runtime;
mod security;
mod workspace;

pub use activity::*;
pub use away::*;
pub use config::*;
pub use event::*;
pub use issue::*;
pub use repository::*;
pub use runtime::*;
pub use security::*;
pub use workspace::*;
