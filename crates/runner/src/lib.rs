//! Capability-based workspace and agent execution backends.

mod api;
mod command;
mod compute;
mod conductor;
mod herdr;
mod naming;
mod native;
mod registry;
mod superset;

pub use api::*;
pub use compute::{WakeConfig, wake};
pub use conductor::{ConductorBackend, ConductorConfig};
pub use herdr::{HerdrBackend, HerdrConfig};
pub use naming::{sanitize_branch, sanitize_workspace_name};
pub use native::{NativeBackend, NativeConfig, NativeSshConfig};
pub use registry::SessionRegistry;
pub use superset::{SupersetBackend, SupersetConfig};
