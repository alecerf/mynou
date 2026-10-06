//! Checked private frame I/O. Atomic replacement under the workspace owner lock.
use crate::{
    Result,
    crypto::{Sha256, random_bytes},
    json::{self, Value},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};
pub(super) const DESCRIPTOR: &[u8; 8] = b"MYNOUW01";
pub(super) const RECEIPT: &[u8; 8] = b"MYNOUP01";
const MAX_HEADER: usize = 16 * 1024;
fn error() -> String {
    "Usenet workspace: invalid, corrupt or inaccessible private file".into()
}
pub(super) fn private_file(path: &Path, max: u64) -> Result<File> {
    reject_symlinks(path).map_err(|_| error())?;
    let m = fs::symlink_metadata(path).map_err(|_| error())?;
    if !m.is_file() || m.len() > max {
        return Err(error());
    }
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| error())?;
    let opened = file.metadata().map_err(|_| error())?;
    if !opened.is_file() || opened.len() != m.len() {
        return Err(error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.nlink() != 1
            || opened.mode() & 0o077 != 0
            || m.dev() != opened.dev()
            || m.ino() != opened.ino()
        {
            return Err(error());
        }
    }
    Ok(file)
}
pub(super) fn read_frame(
    path: &Path,
    magic: &[u8; 8],
    max_data: usize,
) -> Result<(Value, Vec<u8>)> {
    let mut file = private_file(path, (24 + MAX_HEADER + max_data + 32) as u64)?;
    let size = file.metadata().map_err(|_| error())?.len();
    let mut fixed = [0_u8; 24];
    file.read_exact(&mut fixed).map_err(|_| error())?;
    let hl = u64::from_le_bytes(fixed[8..16].try_into().map_err(|_| error())?);
    let dl = u64::from_le_bytes(fixed[16..24].try_into().map_err(|_| error())?);
    if &fixed[..8] != magic
        || hl == 0
        || hl > MAX_HEADER as u64
        || dl > max_data as u64
        || size != 56 + hl + dl
    {
        return Err(error());
    }
    let mut header = vec![0; hl as usize];
    let mut data = vec![0; dl as usize];
    let mut digest = [0; 32];
    file.read_exact(&mut header)
        .and_then(|()| file.read_exact(&mut data))
        .and_then(|()| file.read_exact(&mut digest))
        .map_err(|_| error())?;
    let mut hash = Sha256::new();
    hash.update(&fixed);
    hash.update(&header);
    hash.update(&data);
    if hash.finalize() != digest {
        return Err(error());
    }
    let header =
        json::parse(std::str::from_utf8(&header).map_err(|_| error())?).map_err(|_| error())?;
    Ok((header, data))
}
pub(super) fn write_frame(path: &Path, magic: &[u8; 8], header: &Value, data: &[u8]) -> Result<()> {
    reject_symlinks(path).map_err(|_| error())?;
    let header = json::stringify(header).into_bytes();
    if header.is_empty() || header.len() > MAX_HEADER {
        return Err("Usenet workspace: header limit exceeded".into());
    }
    let temp = path.with_file_name(format!(
        ".frame-{}.tmp",
        crate::requesters::digest(&random_bytes::<16>()?)
    ));
    let result = (|| -> Result<()> {
        let mut f = private_options()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|_| error())?;
        let mut fixed = magic.to_vec();
        fixed.extend_from_slice(&(header.len() as u64).to_le_bytes());
        fixed.extend_from_slice(&(data.len() as u64).to_le_bytes());
        let mut hash = Sha256::new();
        hash.update(&fixed);
        hash.update(&header);
        hash.update(data);
        f.write_all(&fixed)
            .and_then(|()| f.write_all(&header))
            .and_then(|()| f.write_all(data))
            .and_then(|()| f.write_all(&hash.finalize()))
            .and_then(|()| f.sync_all())
            .map_err(|_| error())?;
        fs::rename(&temp, path).map_err(|_| error())?;
        sync_directory(path.parent().ok_or("Usenet workspace: missing directory")?)
            .map_err(|_| "Usenet workspace: durability uncertain; reopen required")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
