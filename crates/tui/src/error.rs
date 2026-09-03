use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("runtime failed: {0}")]
    Runtime(#[from] agent_launcher_runtime::Error),
}
