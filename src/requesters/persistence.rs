//! Checksummed atomic snapshots share the request journal's process lock.
use super::{Policy, Record, State};
use crate::{
    Result,
    config::Config,
    crypto::{random_bytes, sha256},
    json,
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs::{self},
    io::{Read, Write},
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 8] = b"MYNOUR01";
const MAX_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct RequesterStore {
    pub state: State,
    path: PathBuf,
    read_only: bool,
    poisoned: bool,
}
impl RequesterStore {
    pub(crate) fn open(directory: &Path, config: &Config, read_only: bool) -> Result<Self> {
        let path = directory.join("requesters.bin");
        reject_symlinks(&path)?;
        let state = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::empty(),
            Err(_) => return Err("Requester: cannot inspect snapshot".into()),
            Ok(m) => {
                if !m.is_file() || m.len() < 48 || m.len() > MAX_BYTES as u64 + 48 {
                    return Err("Requester: invalid snapshot file or size".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if m.nlink() != 1 {
                        return Err("Requester: snapshot cannot share hard links".into());
                    }
                }
                let mut bytes = Vec::new();
                private_options()
                    .read(true)
                    .open(&path)
                    .map_err(|_| "Requester: cannot open snapshot")?
                    .take(MAX_BYTES as u64 + 49)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "Requester: cannot read snapshot")?;
                if bytes.len() < 48
                    || &bytes[..8] != MAGIC
                    || u64::from_le_bytes(
                        bytes[8..16]
                            .try_into()
                            .map_err(|_| "Requester: invalid snapshot length")?,
                    ) != bytes.len() as u64 - 48
                    || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
                {
                    return Err("Requester: corrupt or unsupported snapshot".into());
                }
                State::from_json(&json::parse(
                    std::str::from_utf8(&bytes[16..bytes.len() - 32])
                        .map_err(|_| "Requester: snapshot is not UTF-8")?,
                )?)?
            }
        };
        let mut store = Self {
            state,
            path,
            read_only,
            poisoned: false,
        };
        let mut next = store.state.clone();
        for account in &config.requesters.accounts {
            if let Some(record) = next.accounts.get(&account.id) {
                if record.binding != account.binding() {
                    return Err("Requester: an existing account binding changed; use a new stable account ID".into());
                }
            } else {
                next.accounts.insert(
                    account.id.clone(),
                    Record {
                        id: account.id.clone(),
                        binding: account.binding(),
                        policy: Policy::initial(config),
                        revision: 1,
                        cursor: 0,
                        snapshot: None,
                        polled_at: 0,
                        last_error: None,
                    },
                );
            }
        }
        State::from_json(&next.to_json())?;
        if next != store.state {
            if read_only {
                store.state = next
            } else {
                store.save(next)?
            }
        }
        Ok(store)
    }
    pub(crate) fn save(&mut self, mut state: State) -> Result<()> {
        if self.read_only {
            return Err("Requester storage is read-only".into());
        }
        if self.poisoned {
            return Err(
                "Requester storage durability is uncertain; restart before changing demand".into(),
            );
        }
        state.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or("Requester: revision overflow")?;
        State::from_json(&state.to_json())?;
        let payload = json::stringify(&state.to_json()).into_bytes();
        if payload.len() > MAX_BYTES {
            return Err("Requester: snapshot exceeds 16 MiB".into());
        }
        let mut bytes = Vec::with_capacity(payload.len() + 48);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&sha256(&bytes));
        reject_symlinks(&self.path)?;
        let temp = self.path.with_file_name(format!(
            ".requesters-{}.tmp",
            super::digest(&random_bytes::<16>()?)
        ));
        let written = (|| -> Result<()> {
            let mut file = private_options()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "Requester: cannot create snapshot")?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| "Requester: cannot persist snapshot")?;
            fs::rename(&temp, &self.path).map_err(|_| "Requester: cannot replace snapshot")?;
            if let Err(e) = sync_directory(
                self.path
                    .parent()
                    .ok_or("Requester: missing storage directory")?,
            ) {
                self.poisoned = true;
                return Err(e);
            }
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written?;
        self.state = state;
        Ok(())
    }
}
