//! Bounded reads of operator-supplied files.
use aster_cli_kit::{Error, ErrorKind, Result};
use std::{io::Read, path::Path};

/// Read a regular file of at most `limit` bytes without following a final
/// symlink or blocking on a FIFO. A `secret` file must also be owned by this
/// user with no group or other access.
pub(crate) fn read_bounded(path: &Path, limit: usize, secret: bool) -> Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err(Error::usage(format!(
            "{} must be a regular file of at most {limit} bytes",
            path.display()
        )));
    }
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(Error::new(
                ErrorKind::Denied,
                format!(
                    "{} must be owned by this user with mode 0600",
                    path.display()
                ),
            )
            .with_hint("restrict the file to mode 0600 before retrying"));
        }
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(Error::usage(format!(
            "{} exceeds its size limit",
            path.display()
        )));
    }
    Ok(bytes)
}
