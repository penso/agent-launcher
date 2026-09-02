use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("terminal I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
