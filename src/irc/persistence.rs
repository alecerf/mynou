//! Private checked snapshots retain deduplication identities without pruning.
use super::{Settings, State, digest};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    json,
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 8] = b"MYNOUI01";
const ROUTE_MAGIC: &[u8; 8] = b"MYNOUI02";
const ADMISSION_MAGIC: &[u8; 8] = b"MYNOUI03";
const DELIVERY_MAGIC: &[u8; 8] = b"MYNOUI04";
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) struct AnnouncementStore {
    pub state: State,
    path: PathBuf,
    read_only: bool,
    poisoned: bool,
    dirty: bool,
}
impl AnnouncementStore {
    pub(crate) fn open(
        directory: &Path,
        settings: &Settings,
        notifications: &crate::notifications::Settings,
        read_only: bool,
    ) -> Result<Self> {
        let path = directory.join("announcements.bin");
        reject_symlinks(&path)?;
        let state = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(_) => return Err("IRC: cannot inspect history".into()),
            Ok(m) => {
                if !m.is_file() || m.len() < 48 || m.len() > MAX_BYTES as u64 + 48 {
                    return Err("IRC: invalid history file or size".into());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if m.nlink() != 1 || m.mode() & 0o077 != 0 {
                        return Err(
                            "IRC: history requires a private file with one hard link".into()
                        );
                    }
                }
                let mut bytes = Vec::new();
                private_options()
                    .read(true)
                    .open(&path)
                    .map_err(|_| "IRC: cannot open history")?
                    .take(MAX_BYTES as u64 + 49)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "IRC: cannot read history")?;
                if bytes.len() < 48
                    || (&bytes[..8] != MAGIC
                        && &bytes[..8] != ROUTE_MAGIC
                        && &bytes[..8] != ADMISSION_MAGIC
                        && &bytes[..8] != DELIVERY_MAGIC)
                    || u64::from_le_bytes(
                        bytes[8..16]
                            .try_into()
                            .map_err(|_| "IRC: invalid history length")?,
                    ) != bytes.len() as u64 - 48
                    || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
                {
                    return Err("IRC: corrupt or unsupported history".into());
                }
                let state = State::from_json(&json::parse(
                    std::str::from_utf8(&bytes[16..bytes.len() - 32])
                        .map_err(|_| "IRC: history is not UTF-8")?,
                )?)?;
                if state.records.values().any(|r| r.route.is_some()) && &bytes[..8] == MAGIC {
                    return Err("IRC: acquisition reservations require history format 2".into());
                }
                if state.records.values().any(|r| r.admission.is_some())
                    && (&bytes[..8] == MAGIC || &bytes[..8] == ROUTE_MAGIC)
                {
                    return Err("IRC: requester admissions require history format 3".into());
                }
                if !state.delivery.is_empty() && &bytes[..8] != DELIVERY_MAGIC {
                    return Err("IRC: notification events require history format 4".into());
                }
                state
            }
        };
        let mut next = state.clone();
        for source in &settings.sources {
            if let Some(b) = next.bindings.get(&source.id) {
                if *b != source.binding() {
                    return Err(
                        "IRC: an existing source binding changed; use a new stable source ID"
                            .into(),
                    );
                }
            } else {
                next.bindings.insert(source.id.clone(), source.binding());
            }
        }
        next.delivery.configure(notifications, "irc")?;
        State::from_json(&next.to_json())?;
        let dirty = next != state;
        Ok(Self {
            state: next,
            path,
            read_only,
            poisoned: false,
            dirty,
        })
    }
    pub(crate) fn initialize(&mut self) -> Result<()> {
        if self.dirty && !self.read_only {
            self.save(self.state.clone())?;
        }
        Ok(())
    }
    pub(crate) fn save(&mut self, mut next: State) -> Result<()> {
        if self.read_only {
            return Err("IRC: history is read-only".into());
        }
        if self.poisoned {
            return Err(
                "IRC: history durability is uncertain; restart before making decisions".into(),
            );
        }
        next.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or("IRC: history revision overflow")?;
        for r in next.records.values() {
            let outcome = if r.admission.as_ref().is_some_and(|a| a.phase == "aborted") {
                "aborted"
            } else if let Some(route) = &r.route {
                match route.phase.as_str() {
                    "reserved" => "reserved",
                    "routed" => "routed",
                    _ => "route_aborted",
                }
            } else {
                &r.decision
            };
            let old = self.state.records.get(&r.id);
            let previous = old.map(|r| {
                if r.admission.as_ref().is_some_and(|a| a.phase == "aborted") {
                    "aborted"
                } else if let Some(route) = &r.route {
                    match route.phase.as_str() {
                        "reserved" => "reserved",
                        "routed" => "routed",
                        _ => "route_aborted",
                    }
                } else {
                    &r.decision
                }
            });
            if previous != Some(outcome) {
                next.delivery.enqueue(crate::notifications::Signal {
                    kind: "irc".into(),
                    scope: r.source_id.clone(),
                    subject: r.id.clone(),
                    emission: r.revision,
                    outcome: outcome.into(),
                    at: crate::store::now().max(r.first_seen),
                })?;
            }
        }
        State::from_json(&next.to_json())?;
        let payload = json::stringify(&next.to_json()).into_bytes();
        if payload.len() > MAX_BYTES {
            return Err("IRC: history exceeds 8 MiB".into());
        }
        let mut bytes = Vec::with_capacity(payload.len() + 48);
        bytes.extend_from_slice(if !next.delivery.is_empty() {
            DELIVERY_MAGIC
        } else if next.records.values().any(|r| r.admission.is_some()) {
            ADMISSION_MAGIC
        } else if next.records.values().any(|r| r.route.is_some()) {
            ROUTE_MAGIC
        } else {
            MAGIC
        });
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&sha256(&bytes));
        reject_symlinks(&self.path)?;
        let temp = self.path.with_file_name(format!(
            ".announcements-{}.tmp",
            digest(&random_bytes::<16>()?)
        ));
        let result = (|| -> Result<()> {
            let mut file = private_options()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|_| "IRC: cannot create history snapshot")?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| "IRC: cannot persist history")?;
            fs::rename(&temp, &self.path).map_err(|_| "IRC: cannot replace history")?;
            if let Err(e) =
                sync_directory(self.path.parent().ok_or("IRC: missing history directory")?)
            {
                self.poisoned = true;
                return Err(e);
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
        self.state = next;
        self.dirty = false;
        Ok(())
    }
}
