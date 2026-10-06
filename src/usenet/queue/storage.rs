//! Checked private snapshots and immutable source blobs. No foreign-file cleanup.
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};
pub(super) const QUEUE: &[u8; 8] = b"MYNOUU01";
pub(super) const SOURCE: &[u8; 8] = b"MYNOUN01";
fn error() -> String {
    "Usenet queue: invalid, corrupt or inaccessible private storage".into()
}
pub(super) fn directory(path: &Path) -> Result<()> {
    reject_symlinks(path).map_err(|_| error())?;
    let m = fs::symlink_metadata(path).map_err(|_| error())?;
    if !m.is_dir() {
        return Err(error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(error());
        }
    }
    Ok(())
}
pub(super) fn create_directory(path: &Path, recursive: bool) -> Result<()> {
    reject_symlinks(path).map_err(|_| error())?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|_| error())?;
    directory(path)
}
pub(super) fn file(path: &Path, max: u64) -> Result<File> {
    reject_symlinks(path).map_err(|_| error())?;
    let m = fs::symlink_metadata(path).map_err(|_| error())?;
    if !m.is_file() || m.len() > max {
        return Err(error());
    }
    let f = private_options()
        .read(true)
        .open(path)
        .map_err(|_| error())?;
    let opened = f.metadata().map_err(|_| error())?;
    if !opened.is_file() || opened.len() != m.len() {
        return Err(error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1
            || opened.mode() & 0o077 != 0
            || opened.dev() != m.dev()
            || opened.ino() != m.ino()
        {
            return Err(error());
        }
    }
    Ok(f)
}
pub(super) fn read(path: &Path, magic: &[u8; 8], max: usize) -> Result<Vec<u8>> {
    let f = file(path, max as u64 + 48)?;
    let mut bytes = Vec::new();
    f.take(max as u64 + 49)
        .read_to_end(&mut bytes)
        .map_err(|_| error())?;
    if bytes.len() < 48
        || bytes.len() > max + 48
        || &bytes[..8] != magic
        || u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| error())?)
            != (bytes.len() - 48) as u64
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err(error());
    }
    Ok(bytes[16..bytes.len() - 32].to_vec())
}
pub(super) fn write(path: &Path, magic: &[u8; 8], payload: &[u8]) -> Result<()> {
    reject_symlinks(path).map_err(|_| error())?;
    if fs::symlink_metadata(path).is_ok() {
        file(path, 8 * 1024 * 1024 + 48)?;
    }
    let temp = path.with_file_name(format!(
        ".queue-{}.tmp",
        crate::requesters::digest(&random_bytes::<16>()?)
    ));
    let result = (|| -> Result<()> {
        let mut f = private_options()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| error())?;
        let mut bytes = magic.to_vec();
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(payload);
        let digest = sha256(&bytes);
        f.write_all(&bytes)
            .and_then(|()| f.write_all(&digest))
            .and_then(|()| f.sync_all())
            .map_err(|_| error())?;
        fs::rename(&temp, path).map_err(|_| error())?;
        sync_directory(path.parent().ok_or_else(error)?).map_err(|_| error())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
pub(super) fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(error()),
    }
}
