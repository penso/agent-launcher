use std::{
    fs::File,
    path::{Path, PathBuf},
};

/// Acquire before loading the backend registry or running any startup recovery.
/// The runtime retains this guard until all its background work has stopped.
pub struct RuntimeOwnership {
    _file: File,
    pub(crate) directory: PathBuf,
}

impl RuntimeOwnership {
    pub fn acquire(data_directory: &Path) -> crate::Result<Self> {
        let acquire = || -> std::io::Result<Self> {
            let directory = std::fs::canonicalize(data_directory)?;
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
            }
            let file = options.open(directory.join("runtime.lock"))?;
            file.try_lock().map_err(std::io::Error::from)?;
            Ok(Self {
                _file: file,
                directory,
            })
        };
        acquire().map_err(|error| agent_launcher_runner::Error::InvalidRequest(format!(
            "Runtime ownership unavailable; another launcher may be running for this repository: {error}"
        )).into())
    }
}
