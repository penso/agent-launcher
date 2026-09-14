//! Capability-based workspace and agent execution backends.

mod api;
mod command;
mod compute;
mod conductor;
mod herdr;
mod naming;
mod native;
mod private;
mod registry;
mod superset;

pub use api::*;
pub use compute::{WakeConfig, wake};
pub use conductor::{ConductorBackend, ConductorConfig};
pub use herdr::{HerdrBackend, HerdrConfig, verify_private_herdr_transport};
pub use naming::{sanitize_branch, sanitize_workspace_name};
pub use native::{NativeBackend, NativeConfig, NativeSshConfig};
pub use private::{prepare_private_checkout, verify_private_checkout};
pub use registry::SessionRegistry;
pub use superset::{SupersetBackend, SupersetConfig};
