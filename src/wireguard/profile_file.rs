//! Read-only private profile loading. No configuration writes or networking.

use super::{MAX_PROFILE_BYTES, Profile, ProfileError, parse};
use std::{
    fmt, fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path},
};

/// Deliberately retains neither a path, OS error text nor profile input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportError {
    Io,
    UnsafeFile,
    Changed,
    ProfileLimit,
    Encoding,
    Parse(ProfileError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => f.write_str("cannot read WireGuard profile file"),
            Self::UnsafeFile => f.write_str("WireGuard profile needs a private regular file without links and a symlink-free path"),
            Self::Changed => f.write_str("WireGuard profile file changed during import"),
            Self::ProfileLimit => f.write_str("WireGuard profile file exceeds size limit"),
            Self::Encoding => f.write_str("WireGuard profile file is not UTF-8"),
            Self::Parse(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ImportError {}

fn inspect(path: &Path) -> Result<fs::Metadata, ImportError> {
    // Keep `..` from hiding a symlink component in ancestor traversal.
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(ImportError::UnsafeFile);
    }
    crate::store::reject_symlinks(path).map_err(|_| ImportError::UnsafeFile)?;
    fs::symlink_metadata(path).map_err(|_| ImportError::Io)
}

fn private(metadata: &fs::Metadata) -> Result<(), ImportError> {
    if !metadata.is_file()
        || metadata.nlink() != 1
        || !matches!(metadata.permissions().mode() & 0o7777, 0o400 | 0o600)
    {
        return Err(ImportError::UnsafeFile);
    }
    Ok(())
}

fn unchanged(before: &fs::Metadata, after: &fs::Metadata) -> Result<(), ImportError> {
    private(after)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.mode() != after.mode()
        || before.uid() != after.uid()
        || before.gid() != after.gid()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(ImportError::Changed);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Checkpoint {
    Inspected,
    Opened,
    Read,
}

/// Reads at most MAX_PROFILE_BYTES + 1 bytes through a checked file descriptor.
/// Requires mode 0400/0600 and no symbolic/hard links. Never changes the file.
/// This is not an atomic snapshot against a hostile process with write access;
/// keep the file and its parent directories under trusted control.
pub fn read_private_file(path: &Path) -> Result<Profile, ImportError> {
    read_checked(path, |_| {})
}

fn read_checked(
    path: &Path,
    mut checkpoint: impl FnMut(Checkpoint),
) -> Result<Profile, ImportError> {
    let before = inspect(path)?;
    private(&before)?;
    if before.len() > MAX_PROFILE_BYTES as u64 {
        return Err(ImportError::ProfileLimit);
    }
    checkpoint(Checkpoint::Inspected);
    // The repository flags refuse a final symlink and avoid blocking on a FIFO
    // substituted after inspection. Verify descriptor identity before reading.
    let file = crate::store::private_options()
        .read(true)
        .open(path)
        .map_err(|_| ImportError::Io)?;
    unchanged(&before, &file.metadata().map_err(|_| ImportError::Io)?)?;
    unchanged(&before, &inspect(path)?)?;
    checkpoint(Checkpoint::Opened);
    let mut bytes = Vec::with_capacity(MAX_PROFILE_BYTES + 1);
    (&file)
        .take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ImportError::Io)?;
    checkpoint(Checkpoint::Read);
    unchanged(&before, &file.metadata().map_err(|_| ImportError::Io)?)?;
    unchanged(&before, &inspect(path)?)?;
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(ImportError::ProfileLimit);
    }
    let input = std::str::from_utf8(&bytes).map_err(|_| ImportError::Encoding)?;
    parse(input).map_err(ImportError::Parse)
}

#[cfg(test)]
#[path = "profile_file_tests.rs"]
mod tests;
