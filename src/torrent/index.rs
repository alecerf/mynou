//! Bounded transfer catalog. Immutable snapshots are published before the index
//! transaction that references them; unreferenced snapshots are safe after a crash.
use super::*;
use crate::{
    crypto::sha256,
    inventory::{Change, Index, Key, Query},
};

const HEADER: Key = [0; 64];
const COUNTS: Key = single(1);
const PRIMARY: u8 = 16;
const ALIAS: u8 = 17;
const QUEUE: u8 = 18;
const ORDER: u8 = 19;
const SEED: u8 = 20;
const WIRE: u8 = 21;
const ALL_ORDERED: u8 = 22;
const DISCOVERY: u8 = 23;
const FORMAT: &[u8] = b"mynou-transfers/1";
const BUILDING: &[u8] = b"mynou-transfers/1-building";
const SENTINEL: &str = "0000000000000000000000000000000000000000.torrent";
const SNAPSHOT_LIMIT: usize = 64 * 1024 * 1024;
#[cfg(test)]
thread_local! {static MIGRATION_STOP:std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}

const fn single(table: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key
}
fn id_key(table: u8, id: &str) -> Result<Key> {
    if ![40, 64].contains(&id.len()) || id != id.to_ascii_lowercase() {
        return Err("Invalid indexed torrent identity".into());
    }
    let mut key = single(table);
    key[1] = id.len() as u8;
    for (at, pair) in id.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        key[2 + at] = u8::from_str_radix(
            std::str::from_utf8(pair).map_err(|_| "Invalid torrent identity")?,
            16,
        )
        .map_err(|_| "Invalid torrent identity")?;
    }
    Ok(key)
}
fn key_id(key: &Key, offset: usize) -> Result<String> {
    let size = usize::from(key[offset]);
    if ![40, 64].contains(&size) {
        return Err("Invalid indexed identity width".into());
    }
    Ok(hex(&key[offset + 1..offset + 1 + size / 2]))
}
fn alias_key(bytes: &[u8]) -> Key {
    let mut key = single(ALIAS);
    key[1] = bytes.len() as u8;
    key[2..2 + bytes.len()].copy_from_slice(bytes);
    key
}
fn queue_key(record: &Record) -> Result<Key> {
    let mut key = single(QUEUE);
    key[1..5].copy_from_slice(&(!(record.control.priority as u32 ^ 0x8000_0000)).to_be_bytes());
    key[5..13].copy_from_slice(&record.control.queue_order.to_be_bytes());
    let id = id_key(PRIMARY, &record.status.id)?;
    key[13..46].copy_from_slice(&id[1..34]);
    Ok(key)
}
fn order_key(order: u64) -> Key {
    let mut key = single(ORDER);
    key[1..9].copy_from_slice(&order.to_be_bytes());
    key
}

pub(super) struct Record {
    pub status: DownloadStatus,
    pub control: TorrentControl,
    pub paused: bool,
    pub failed: bool,
    pub v1: Option<[u8; 20]>,
    pub v2: Option<[u8; 32]>,
    pub cross_seed: bool,
    pub current: bool,
    session: [u8; 16],
}
impl Record {
    fn queued(&self) -> bool {
        !self.paused
            && !self.failed
            && !self.control.user_paused
            && !self.control.seed_limited
            && !self.status.selected_ready
            && !self.status.ready
    }
    fn encode(&self) -> Result<Vec<u8>> {
        let mut status = BTreeMap::new();
        status.insert(
            b"id".to_vec(),
            Value::Bytes(self.status.id.as_bytes().to_vec()),
        );
        status.insert(
            b"progress".to_vec(),
            Value::Bytes(self.status.progress.to_bits().to_le_bytes().to_vec()),
        );
        status.insert(b"ready".to_vec(), Value::Int(i64::from(self.status.ready)));
        status.insert(
            b"selected_ready".to_vec(),
            Value::Int(i64::from(self.status.selected_ready)),
        );
        status.insert(
            b"message".to_vec(),
            Value::Bytes(self.status.message.as_bytes().to_vec()),
        );
        for (name, paths) in [
            ("files", &self.status.files),
            ("available_files", &self.status.available_files),
        ] {
            let paths = paths
                .iter()
                .map(|p| {
                    p.to_str()
                        .map(|p| Value::Bytes(p.as_bytes().to_vec()))
                        .ok_or_else(|| "Non-UTF8 indexed transfer path".to_owned())
                })
                .collect::<Result<Vec<_>>>()?;
            status.insert(name.as_bytes().to_vec(), Value::List(paths));
        }
        let fields = BTreeMap::from([
            (b"status".to_vec(), Value::Dict(status)),
            (b"control".to_vec(), Value::Bytes(self.control.encode()?)),
            (b"paused".to_vec(), Value::Int(i64::from(self.paused))),
            (b"failed".to_vec(), Value::Int(i64::from(self.failed))),
            (b"seed".to_vec(), Value::Int(i64::from(self.cross_seed))),
            (
                b"v1".to_vec(),
                Value::Bytes(self.v1.map_or_else(Vec::new, |h| h.to_vec())),
            ),
            (
                b"v2".to_vec(),
                Value::Bytes(self.v2.map_or_else(Vec::new, |h| h.to_vec())),
            ),
            (b"session".to_vec(), Value::Bytes(self.session.to_vec())),
        ]);
        let encoded = crate::bencode::encode(&Value::Dict(fields));
        if encoded.len() > SNAPSHOT_LIMIT {
            return Err("Indexed transfer snapshot exceeds limit".into());
        }
        Ok(encoded)
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let value = crate::bencode::parse_bounded(bytes, SNAPSHOT_LIMIT)?;
        let fields = value
            .as_dict()
            .filter(|f| f.len() == 8)
            .ok_or("Invalid transfer snapshot")?;
        let get = |name: &[u8]| {
            fields
                .get(name)
                .ok_or_else(|| "Missing transfer snapshot field".to_owned())
        };
        let binary = |name: &[u8]| {
            get(name)?
                .as_bytes()
                .ok_or_else(|| "Invalid transfer snapshot bytes".to_owned())
        };
        let flag = |name: &[u8]| match get(name)? {
            Value::Int(0) => Ok(false),
            Value::Int(1) => Ok(true),
            _ => Err("Invalid transfer snapshot flag".to_owned()),
        };
        let status = get(b"status")?
            .as_dict()
            .filter(|s| s.len() == 7)
            .ok_or("Invalid transfer status")?;
        let field = |name: &str| {
            status
                .get(name.as_bytes())
                .ok_or_else(|| "Missing transfer status field".to_owned())
        };
        let string = |name: &str| {
            std::str::from_utf8(field(name)?.as_bytes().ok_or("Invalid status text")?)
                .map(str::to_owned)
                .map_err(|_| "Invalid status UTF-8".to_owned())
        };
        let boolean = |name: &str| match field(name)? {
            Value::Int(0) => Ok(false),
            Value::Int(1) => Ok(true),
            _ => Err("Invalid status flag".to_owned()),
        };
        let paths = |name: &str| match field(name)? {
            Value::List(paths) => paths
                .iter()
                .map(|v| {
                    std::str::from_utf8(v.as_bytes().ok_or("Invalid status path")?)
                        .map(PathBuf::from)
                        .map_err(|_| "Invalid status path UTF-8".to_owned())
                })
                .collect::<Result<Vec<_>>>(),
            _ => Err("Invalid status paths".to_owned()),
        };
        let control = TorrentControl::decode(binary(b"control")?)?;
        let id = string("id")?;
        id_key(PRIMARY, &id)?;
        if control.id != id {
            return Err("Transfer snapshot identity mismatch".into());
        }
        let progress = f64::from_bits(u64::from_le_bytes(
            field("progress")?
                .as_bytes()
                .ok_or("Invalid progress bytes")?
                .try_into()
                .map_err(|_| "Invalid progress width")?,
        ));
        if !progress.is_finite() || !(0.0..=1.0).contains(&progress) {
            return Err("Invalid transfer progress".into());
        }
        let v1 = match binary(b"v1")? {
            [] => None,
            bytes => Some(bytes.try_into().map_err(|_| "Invalid transfer v1 alias")?),
        };
        let v2 = match binary(b"v2")? {
            [] => None,
            bytes => Some(bytes.try_into().map_err(|_| "Invalid transfer v2 alias")?),
        };
        Ok(Self {
            status: DownloadStatus {
                id,
                progress,
                ready: boolean("ready")?,
                selected_ready: boolean("selected_ready")?,
                files: paths("files")?,
                available_files: paths("available_files")?,
                message: string("message")?,
            },
            control,
            paused: flag(b"paused")?,
            failed: flag(b"failed")?,
            cross_seed: flag(b"seed")?,
            v1,
            v2,
            current: false,
            session: binary(b"session")?
                .try_into()
                .map_err(|_| "Invalid transfer session")?,
        })
    }
}

#[derive(Default)]
struct Counts {
    count: u64,
    downloaded: u128,
    uploaded: u128,
    order: u64,
    listing: u64,
}
impl Counts {
    fn encode(&self) -> Vec<u8> {
        [
            self.count.to_le_bytes().as_slice(),
            self.downloaded.to_le_bytes().as_slice(),
            self.uploaded.to_le_bytes().as_slice(),
            self.order.to_le_bytes().as_slice(),
            self.listing.to_le_bytes().as_slice(),
        ]
        .concat()
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 56 {
            return Err("Invalid transfer catalog counters".into());
        }
        Ok(Self {
            count: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            downloaded: u128::from_le_bytes(bytes[8..24].try_into().unwrap()),
            uploaded: u128::from_le_bytes(bytes[24..40].try_into().unwrap()),
            order: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            listing: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
        })
    }
}

pub(super) struct TransferIndex {
    index: Index,
    directory: PathBuf,
    session: [u8; 16],
    _ownership: File,
}
impl TransferIndex {
    /// Index size and state for diagnostics, from the durable root only.
    pub(super) fn storage(&self) -> crate::json::Value {
        crate::inventory::report("index", Some(&self.index))
    }
    pub fn open(config: &mut DownloadConfig) -> Result<Self> {
        let ownership = state_lock(&config.state_dir)?;
        let path = config.state_dir.join("transfers.idx");
        let marker = config.state_dir.join(SENTINEL);
        let mut marker_bytes = match fs::symlink_metadata(&marker) {
            Ok(_) => checked_read(&marker, 72)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&path).is_ok() {
                    return Err(
                        "Transfer index marker is missing; restore matching store backup".into(),
                    );
                }
                for suffix in ["source", "seed", "control", "paused"] {
                    let reserved = config
                        .state_dir
                        .join(format!("0000000000000000000000000000000000000000.{suffix}"));
                    if fs::symlink_metadata(&reserved).is_ok() {
                        return Err(
                            "Reserved transfer migration identity conflicts with legacy state"
                                .into(),
                        );
                    }
                }
                let mut bytes = b"MYNOUT01".to_vec();
                bytes.extend_from_slice(&crate::crypto::random_bytes::<32>()?);
                bytes.extend_from_slice(&sha256(&bytes));
                atomic_write(&marker, &bytes)?;
                bytes
            }
            Err(_) => return Err("Cannot inspect transfer format marker".into()),
        };
        if marker_bytes.len() != 72
            || !matches!(&marker_bytes[..8], b"MYNOUT01" | b"MYNOUT02")
            || sha256(&marker_bytes[..40]).as_slice() != &marker_bytes[40..]
        {
            return Err("Transfer migration marker or reserved identity conflict".into());
        }
        let published = &marker_bytes[..8] == b"MYNOUT02";
        if published && fs::symlink_metadata(&path).is_err() {
            return Err(
                "Published transfer index is missing; restore matching store backup".into(),
            );
        }
        let identity = marker_bytes[8..40]
            .try_into()
            .map_err(|_| "Invalid transfer identity marker")?;
        let mut index = if published {
            Index::open_bound(&path, false, identity)?
        } else {
            open_bound_catalog(&path, identity)?
        };
        if index.is_empty()? {
            if published {
                return Err(
                    "Published transfer catalog is empty; restore matching store backup".into(),
                );
            }
            index.commit(vec![
                Change::Put(HEADER, BUILDING.to_vec()),
                Change::Put(COUNTS, Counts::default().encode()),
            ])?;
        }
        let mut store = Self {
            index,
            directory: config.state_dir.clone(),
            session: crate::crypto::random_bytes()?,
            _ownership: ownership,
        };
        match store.index.get(&HEADER)?.as_deref() {
            Some(FORMAT) => {}
            Some(BUILDING) if !published => store.migrate(config)?,
            _ => return Err("Unsupported transfer catalog format".into()),
        }
        if &marker_bytes[..8] == b"MYNOUT01" {
            marker_bytes[..8].copy_from_slice(b"MYNOUT02");
            let digest = sha256(&marker_bytes[..40]);
            marker_bytes[40..].copy_from_slice(&digest);
            atomic_write(&marker, &marker_bytes)?;
        }
        Ok(store)
    }
    fn counts(&mut self) -> Result<Counts> {
        Counts::decode(
            &self
                .index
                .get(&COUNTS)?
                .ok_or("Missing transfer catalog counters")?,
        )
    }
    pub fn next_order(&mut self) -> Result<u64> {
        let mut counts = self.counts()?;
        counts.order = counts
            .order
            .checked_add(1)
            .ok_or("Transfer FIFO exhausted")?;
        self.index
            .commit(vec![Change::Put(COUNTS, counts.encode())])?;
        Ok(counts.order)
    }
    pub fn aggregates(&mut self) -> Result<(u64, u64, u64)> {
        let c = self.counts()?;
        Ok((
            c.count,
            c.downloaded.min(u64::MAX as u128) as u64,
            c.uploaded.min(u64::MAX as u128) as u64,
        ))
    }
    pub fn get(&mut self, id: &str) -> Result<Option<Record>> {
        let Some(digest) = self.index.get(&id_key(PRIMARY, id)?)? else {
            return Ok(None);
        };
        let digest: [u8; 32] = digest
            .as_slice()
            .try_into()
            .map_err(|_| "Invalid transfer snapshot reference")?;
        check_private_directory(&self.directory.join("transfer-snapshots"))?;
        let bytes = checked_read(
            &self.directory.join("transfer-snapshots").join(hex(&digest)),
            SNAPSHOT_LIMIT,
        )?;
        if sha256(&bytes) != digest {
            return Err("Transfer snapshot checksum mismatch".into());
        }
        let mut record = Record::decode(&bytes)?;
        record.current = record.session == self.session;
        if record.status.id != id {
            return Err("Transfer snapshot lookup identity mismatch".into());
        }
        Ok(Some(record))
    }
    pub fn alias(&mut self, v1: Option<[u8; 20]>, v2: Option<[u8; 32]>) -> Result<Option<String>> {
        let mut found = None;
        for hash in [v1.map(|v| v.to_vec()), v2.map(|v| v.to_vec())]
            .into_iter()
            .flatten()
        {
            if let Some(id) = self.index.get(&alias_key(&hash))? {
                let id = String::from_utf8(id).map_err(|_| "Invalid transfer alias target")?;
                id_key(PRIMARY, &id)?;
                if found.as_ref().is_some_and(|old| old != &id) {
                    return Err("Conflicting indexed transfer aliases".into());
                }
                found = Some(id);
            }
        }
        Ok(found)
    }
    pub fn save(&mut self, job: &Job) -> Result<()> {
        let record = Record {
            status: job.status.clone(),
            control: snapshot_control(job),
            paused: job.paused,
            failed: job.failed,
            v1: job.source.v1,
            v2: job.source.v2,
            cross_seed: job.cross_seed.is_some(),
            current: true,
            session: self.session,
        };
        self.save_record(&record)
    }
    fn save_record(&mut self, record: &Record) -> Result<()> {
        let id = &record.status.id;
        let primary = id_key(PRIMARY, id)?;
        let old = self.get(id)?;
        let old_digest = self.index.get(&primary)?;
        let encoded = record.encode()?;
        let digest = sha256(&encoded);
        if self.index.get(&primary)?.as_deref() == Some(digest.as_slice()) {
            return Ok(());
        }
        if record.control.queue_order == 0 {
            return Err("Invalid transfer FIFO order".into());
        }
        let mut changes = BTreeMap::new();
        if let Some(old) = &old {
            for hash in [old.v1.map(|v| v.to_vec()), old.v2.map(|v| v.to_vec())]
                .into_iter()
                .flatten()
            {
                changes.insert(alias_key(&hash), None);
            }
            if old.queued() {
                changes.insert(queue_key(old)?, None);
            }
            changes.insert(id_key(SEED, &old.status.id)?, None);
            let mut all = queue_key(old)?;
            all[0] = ALL_ORDERED;
            changes.insert(all, None);
            for wire in [old.v1, old.v2.map(|h| h[..20].try_into().unwrap())]
                .into_iter()
                .flatten()
            {
                let mut key = alias_key(&wire);
                key[0] = WIRE;
                changes.insert(key, None);
            }
            changes.insert(order_key(old.control.queue_order), None);
        }
        let order = order_key(record.control.queue_order);
        if self
            .index
            .get(&order)?
            .is_some_and(|owner| owner != id.as_bytes())
        {
            return Err("Duplicate transfer FIFO order".into());
        }
        changes.insert(order, Some(id.as_bytes().to_vec()));
        for hash in [record.v1.map(|v| v.to_vec()), record.v2.map(|v| v.to_vec())]
            .into_iter()
            .flatten()
        {
            let key = alias_key(&hash);
            if self
                .index
                .get(&key)?
                .is_some_and(|owner| owner != id.as_bytes())
            {
                return Err("Duplicate transfer alias".into());
            }
            changes.insert(key, Some(id.as_bytes().to_vec()));
        }
        if record.queued() {
            changes.insert(queue_key(record)?, Some(Vec::new()));
        }
        if record.status.ready
            && !record.paused
            && !record.failed
            && !record.control.user_paused
            && !record.control.seed_limited
        {
            changes.insert(id_key(SEED, id)?, Some(Vec::new()));
        }
        let mut all = queue_key(record)?;
        all[0] = ALL_ORDERED;
        changes.insert(all, Some(Vec::new()));
        for wire in [record.v1, record.v2.map(|h| h[..20].try_into().unwrap())]
            .into_iter()
            .flatten()
        {
            let mut key = alias_key(&wire);
            key[0] = WIRE;
            if self
                .index
                .get(&key)?
                .is_some_and(|owner| owner != id.as_bytes())
            {
                return Err("Conflicting transfer wire identity".into());
            }
            changes.insert(key, Some(id.as_bytes().to_vec()));
        }
        let snapshots = self.directory.join("transfer-snapshots");
        ensure_private_directory(&snapshots)?;
        let path = snapshots.join(hex(&digest));
        if path.exists() {
            if checked_read(&path, SNAPSHOT_LIMIT)? != encoded {
                return Err("Transfer snapshot content conflict".into());
            }
        } else {
            atomic_write(&path, &encoded)?;
        }
        let mut counts = self.counts()?;
        if old.as_ref().is_none_or(|old| {
            old.control.priority != record.control.priority
                || old.control.queue_order != record.control.queue_order
        }) {
            counts.listing = counts
                .listing
                .checked_add(1)
                .ok_or("Transfer listing sequence exhausted")?;
        }
        if let Some(old) = old {
            counts.downloaded = counts
                .downloaded
                .checked_sub(u128::from(old.control.downloaded_bytes))
                .ok_or("Invalid downloaded aggregate")?;
            counts.uploaded = counts
                .uploaded
                .checked_sub(u128::from(old.control.uploaded_bytes))
                .ok_or("Invalid uploaded aggregate")?;
        } else {
            counts.count = counts
                .count
                .checked_add(1)
                .ok_or("Transfer count exhausted")?;
        }
        counts.downloaded += u128::from(record.control.downloaded_bytes);
        counts.uploaded += u128::from(record.control.uploaded_bytes);
        counts.order = counts.order.max(record.control.queue_order);
        changes.insert(primary, Some(digest.to_vec()));
        changes.insert(COUNTS, Some(counts.encode()));
        self.index.commit(
            changes
                .into_iter()
                .map(|(k, v)| match v {
                    Some(v) if v.len() <= 32 => Change::PutInline(k, v),
                    Some(v) => Change::Put(k, v),
                    None => Change::Delete(k),
                })
                .collect(),
        )?;
        if let Some(old_digest) = old_digest {
            let path = snapshots.join(hex(&old_digest));
            fs::remove_file(path).map_err(|_| "Cannot remove superseded transfer snapshot")?;
            crate::store::sync_directory(&snapshots)
                .map_err(|_| "Cannot synchronize transfer snapshots")?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn queue(&mut self, limit: usize) -> Result<Vec<String>> {
        let page = self.index.page(&Query {
            start: single(QUEUE),
            end: Some(single(QUEUE + 1)),
            limit,
            ..Query::default()
        })?;
        page.entries
            .into_iter()
            .map(|(key, _)| key_id(&key, 13))
            .collect()
    }
    pub fn revision(&self) -> Result<u64> {
        self.index.sequence()
    }
    pub fn mark_failed(&mut self, id: &str, message: &str) -> Result<()> {
        let mut record = self.get(id)?.ok_or("Missing failed transfer")?;
        record.failed = true;
        record.status.ready = false;
        record.status.selected_ready = false;
        record.status.available_files.clear();
        record.status.message = message.chars().take(16_384).collect();
        record.current = true;
        record.session = self.session;
        self.save_record(&record)
    }
    pub fn queue_page(
        &mut self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>)> {
        let after = after.map(decode_hex::<64>).transpose()?;
        if after.as_ref().is_some_and(|key| key[0] != QUEUE) {
            return Err("Invalid queue cursor".into());
        }
        let page = self.index.page(&Query {
            start: single(QUEUE),
            end: Some(single(QUEUE + 1)),
            after,
            limit,
            ..Query::default()
        })?;
        Ok((
            page.entries
                .iter()
                .map(|(key, _)| key_id(key, 13))
                .collect::<Result<Vec<_>>>()?,
            page.next.map(|key| hex(&key)),
        ))
    }
    pub fn primary_page(
        &mut self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>, u64, u64)> {
        let after = after.map(|id| id_key(PRIMARY, id)).transpose()?;
        let page = self.index.page(&Query {
            start: single(PRIMARY),
            end: Some(single(PRIMARY + 1)),
            after,
            limit,
            ..Query::default()
        })?;
        let ids = page
            .entries
            .iter()
            .map(|(key, _)| key_id(key, 1))
            .collect::<Result<Vec<_>>>()?;
        let next = page.next.map(|key| key_id(&key, 1)).transpose()?;
        Ok((ids, next, self.counts()?.count, page.sequence))
    }
    pub fn wire_alias(&mut self, wire: [u8; 20]) -> Result<Option<String>> {
        let mut key = alias_key(&wire);
        key[0] = WIRE;
        self.index
            .get(&key)?
            .map(|id| String::from_utf8(id).map_err(|_| "Invalid transfer wire owner".to_owned()))
            .transpose()
    }
    pub fn seeds_page(
        &mut self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>)> {
        let after = after.map(|id| id_key(SEED, id)).transpose()?;
        let page = self.index.page(&Query {
            start: single(SEED),
            end: Some(single(SEED + 1)),
            after,
            limit,
            ..Query::default()
        })?;
        Ok((
            page.entries
                .iter()
                .map(|(k, _)| key_id(k, 1))
                .collect::<Result<Vec<_>>>()?,
            page.next.map(|k| key_id(&k, 1)).transpose()?,
        ))
    }
    pub fn page(
        &mut self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>, u64, u64)> {
        let after = after.map(decode_hex::<64>).transpose()?;
        if after.as_ref().is_some_and(|k| k[0] != ALL_ORDERED) {
            return Err("Invalid transfer page cursor".into());
        }
        let page = self.index.page(&Query {
            start: single(ALL_ORDERED),
            end: Some(single(ALL_ORDERED + 1)),
            after,
            limit,
            ..Query::default()
        })?;
        let ids = page
            .entries
            .iter()
            .map(|(key, _)| key_id(key, 13))
            .collect::<Result<Vec<_>>>()?;
        let counts = self.counts()?;
        Ok((
            ids,
            page.next.map(|key| hex(&key)),
            counts.count,
            counts.listing,
        ))
    }
    fn migrate(&mut self, config: &DownloadConfig) -> Result<()> {
        let backup = self.directory.join(".transfers-legacy");
        let resumed = fs::symlink_metadata(&backup).is_ok();
        ensure_private_directory(&backup)?;
        let mut legacy = 0_u64;
        // Freeze and verify each original independently. This is migration-only
        // enumeration; normal opens read only the root, marker and queue window.
        let mut max_order = self.counts()?.order;
        for entry in
            fs::read_dir(&self.directory).map_err(|_| "Cannot enumerate legacy transfer state")?
        {
            let entry = entry.map_err(|_| "Cannot read legacy transfer entry")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == SENTINEL {
                continue;
            }
            let Some((id, suffix)) = name.rsplit_once('.') else {
                continue;
            };
            if !["torrent", "source", "seed", "control", "paused"].contains(&suffix)
                || id_key(PRIMARY, id).is_err()
            {
                continue;
            }
            let bytes = checked_read_with_permissions(&entry.path(), SNAPSHOT_LIMIT, false)?;
            legacy += 1;
            let saved = backup.join(name);
            // A prior imported row may have materialized a previously absent
            // compatibility control. It is derived state, not legacy backup.
            if suffix == "control"
                && !saved.exists()
                && let Some(imported) = self.get(id)?
            {
                if imported.control.encode()? != bytes {
                    return Err("Migrated transfer controls changed before publication".into());
                }
                continue;
            }
            if saved.exists() {
                if checked_read(&saved, SNAPSHOT_LIMIT)? != bytes {
                    return Err(
                        "Legacy transfer changed during migration; restore matching backup".into(),
                    );
                }
            } else {
                atomic_write(&saved, &bytes)?;
            }
            if suffix == "control" {
                max_order = max_order.max(TorrentControl::decode(&bytes)?.queue_order);
            }
        }
        // A first start finds no legacy file: creating the catalog is not a migration.
        let progress =
            crate::progress::Tracker::start_if(legacy > 0, &self.directory, "transfers", None);
        if resumed {
            progress.resumed();
        }
        progress.stage("preserving", 0);
        let mut counts = self.counts()?;
        counts.order = max_order;
        self.index
            .commit(vec![Change::Put(COUNTS, counts.encode())])?;
        // Disk-stage identities in bounded batches. Filename discovery order
        // must not change the legacy lexicographic FIFO assignment.
        let mut pending = BTreeMap::new();
        for entry in
            fs::read_dir(&backup).map_err(|_| "Cannot enumerate transfer migration backup")?
        {
            let entry = entry.map_err(|_| "Cannot read transfer migration backup")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((id, suffix)) = name.rsplit_once('.') else {
                continue;
            };
            if !["torrent", "source", "seed"].contains(&suffix) {
                continue;
            }
            let raw = id_key(PRIMARY, id)?;
            let mut staged = single(DISCOVERY);
            staged[1..33].copy_from_slice(&raw[2..34]);
            staged[33] = raw[1];
            pending.insert(staged, Change::PutInline(staged, Vec::new()));
            if pending.len() >= 64 {
                self.index
                    .commit(std::mem::take(&mut pending).into_values().collect())?;
            }
        }
        if !pending.is_empty() {
            self.index.commit(pending.into_values().collect())?;
        }
        let mut done = 0_u64;
        progress.stage("copying", 0);
        loop {
            let page = self.index.page(&Query {
                start: single(DISCOVERY),
                end: Some(single(DISCOVERY + 1)),
                limit: 64,
                ..Query::default()
            })?;
            if page.entries.is_empty() {
                break;
            }
            for (staged, _) in page.entries {
                let size = usize::from(staged[33]);
                if ![40, 64].contains(&size) {
                    return Err("Invalid migration identity width".into());
                }
                let id = hex(&staged[1..1 + size / 2]);
                if self.get(&id)?.is_none() {
                    let mut job = super::load_job(config, &id)?;
                    if !self.directory.join(format!("{id}.control")).exists() {
                        job.control.queue_order = self.next_order()?;
                    }
                    self.save(&job)?;
                }
                let control_path = self.directory.join(format!("{id}.control"));
                if !control_path.exists() {
                    let imported = self.get(&id)?.ok_or("Missing migrated transfer")?;
                    atomic_write(&control_path, &imported.control.encode()?)?;
                }
                #[cfg(test)]
                if MIGRATION_STOP.with(|stop| stop.get()) {
                    return Err("Transfer migration interrupted by fixture".into());
                }
                self.index.commit(vec![Change::Delete(staged)])?;
                done += 1;
                progress.stage("copying", done);
            }
        }
        progress.stage("promoting", done);
        self.index
            .commit(vec![Change::Put(HEADER, FORMAT.to_vec())])?;
        progress.total(done);
        progress.finish();
        Ok(())
    }
}

fn checked_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    checked_read_with_permissions(path, limit, true)
}
fn checked_read_with_permissions(path: &Path, limit: usize, private: bool) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "Cannot inspect private transfer state")?;
    private_state_file(&metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if private && metadata.mode() & 0o077 != 0 {
            return Err("Transfer state must have private permissions".into());
        }
    }
    if metadata.len() > limit as u64 {
        return Err("Transfer state exceeds limit".into());
    }
    crate::store::before_private_open(path);
    let file = crate::store::private_options()
        .read(true)
        .open(path)
        .map_err(|_| "Cannot open private transfer state")?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect opened transfer state")?;
    private_state_file(&opened)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err("Transfer state changed during opening".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read private transfer state")?;
    if bytes.len() > limit || bytes.len() as u64 != metadata.len() {
        return Err("Transfer state changed or exceeds limit".into());
    }
    Ok(bytes)
}

fn check_private_directory(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "Cannot inspect private transfer directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Invalid private transfer directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err("Transfer directory must be private".into());
        }
    }
    Ok(())
}
fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => check_private_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => mkdir_private(path),
        Err(_) => Err("Cannot inspect private transfer directory".into()),
    }
}

fn optional_read(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(_) => checked_read(path, limit).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Cannot inspect transfer creation artifact".into()),
    }
}
fn check_creation(path: &Path, identity: [u8; 32]) -> Result<()> {
    ensure_private_directory(path)?;
    let mut expected = b"MYNOUID1".to_vec();
    expected.extend_from_slice(&identity);
    expected.extend_from_slice(&sha256(&expected));
    let identity_bytes = optional_read(&path.join("identity.bin"), 72)?;
    if identity_bytes
        .as_ref()
        .is_some_and(|bytes| !expected.starts_with(bytes))
    {
        return Err("Foreign transfer creation identity".into());
    }
    let complete = identity_bytes.as_deref() == Some(expected.as_slice());
    for entry in fs::read_dir(path).map_err(|_| "Cannot enumerate transfer creation artifacts")? {
        let entry = entry.map_err(|_| "Cannot inspect transfer creation artifact")?;
        match entry.file_name().to_str() {
            Some(".lock") => {
                if !checked_read(&entry.path(), 0)?.is_empty() {
                    return Err("Invalid transfer creation lock".into());
                }
            }
            Some("identity.bin") => {}
            Some("tree.bin") => {
                let bytes = checked_read(&entry.path(), 4095)?;
                if !complete || !Index::valid_creation_prefix(identity, &bytes) {
                    return Err("Foreign transfer creation tree".into());
                }
            }
            Some("objects") => {
                ensure_private_directory(&entry.path())?;
                if !complete
                    || fs::read_dir(entry.path())
                        .map_err(|_| "Cannot inspect initial transfer objects")?
                        .next()
                        .is_some()
                {
                    return Err("Foreign populated transfer creation objects".into());
                }
            }
            _ => return Err("Unknown transfer creation artifact".into()),
        }
    }
    Ok(())
}
fn open_bound_catalog(path: &Path, identity: [u8; 32]) -> Result<Index> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Index::create_bound(path, identity);
        }
        Err(_) => return Err("Cannot inspect transfer index".into()),
        Ok(_) => {}
    }
    ensure_private_directory(path)?;
    match fs::symlink_metadata(path.join("tree.bin")) {
        Ok(metadata) if metadata.len() >= 4096 => return Index::open_bound(path, false, identity),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err("Cannot inspect transfer creation tree".into());
        }
        _ => {}
    }
    check_creation(path, identity)?;
    let lock_path = path.join(".lock");
    let lock = if optional_read(&lock_path, 0)?.is_some() {
        let inspected =
            fs::symlink_metadata(&lock_path).map_err(|_| "Cannot inspect initial transfer lock")?;
        private_state_file(&inspected)?;
        crate::store::before_private_open(&lock_path);
        let lock = crate::store::private_options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|_| "Cannot open initial transfer lock")?;
        let opened = lock
            .metadata()
            .map_err(|_| "Cannot inspect initial transfer lock")?;
        private_state_file(&opened)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if opened.dev() != inspected.dev() || opened.ino() != inspected.ino() {
                return Err("Initial transfer lock changed during opening".into());
            }
        }
        lock
    } else {
        crate::store::private_options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|_| "Cannot create initial transfer lock")?
    };
    crate::store::try_lock_file(&lock).map_err(|_| "Transfer index creation is already owned")?;
    check_creation(path, identity)?;
    let parent = path.parent().ok_or("Transfer index has no parent")?;
    let preserved = parent.join(format!(
        "transfers.idx.incomplete-{}",
        hex(&crate::crypto::random_bytes::<16>()?)
    ));
    fs::rename(path, &preserved).map_err(|_| "Cannot preserve interrupted transfer index")?;
    crate::store::sync_directory(parent)
        .map_err(|_| "Cannot synchronize transfer creation recovery")?;
    let index = Index::create_bound(path, identity);
    drop(lock);
    index
}

fn state_lock(directory: &Path) -> Result<File> {
    let path = directory.join(".transfers.lock");
    let inspected = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            checked_read(&path, 0)?;
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("Cannot inspect transfer ownership lock".into()),
    };
    crate::store::before_private_open(&path);
    let file = crate::store::private_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| "Cannot open transfer ownership lock")?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect opened transfer ownership lock")?;
    private_state_file(&opened)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.mode() & 0o077 != 0
            || inspected.is_some_and(|old| old.dev() != opened.dev() || old.ino() != opened.ino())
        {
            return Err("Invalid or changed transfer ownership lock".into());
        }
    }
    if opened.len() != 0 {
        return Err("Invalid transfer ownership lock contents".into());
    }
    crate::store::try_lock_file(&file).map_err(|_| "Native transfer state is already owned")?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        config: DownloadConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mynou-transfer-index-{}",
                hex(&crate::crypto::random_bytes::<16>().unwrap())
            ));
            let config = DownloadConfig {
                data_dir: root.join("data"),
                state_dir: root.join("state"),
                listen_port: 0,
                seed: false,
                dht: false,
                pex: false,
                max_active: 1,
                max_peers: 1,
            };
            mkdir_private(&config.state_dir).unwrap();
            mkdir_private(&config.data_dir).unwrap();
            Self { root, config }
        }
        fn source(&self, n: u64, order: Option<u64>) -> String {
            let id = format!("{n:040x}");
            atomic_write(
                &self.config.state_dir.join(format!("{id}.source")),
                format!("magnet:?xt=urn:btih:{id}").as_bytes(),
            )
            .unwrap();
            if let Some(order) = order {
                let control = TorrentControl::new(id.clone(), order).unwrap();
                atomic_write(
                    &self.config.state_dir.join(format!("{id}.control")),
                    &control.encode().unwrap(),
                )
                .unwrap();
            }
            id
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn migration_assigns_missing_fifo_after_existing_and_preserves_backup() {
        let mut fixture = Fixture::new();
        let first = fixture.source(1, Some(900));
        let second = fixture.source(2, None);
        atomic_write(
            &fixture.config.state_dir.join(format!("{first}.paused")),
            b"paused\n",
        )
        .unwrap();
        let mut index = TransferIndex::open(&mut fixture.config).unwrap();
        assert!(index.get(&first).unwrap().unwrap().paused);
        assert_eq!(index.get(&first).unwrap().unwrap().control.queue_order, 900);
        assert_eq!(
            index.get(&second).unwrap().unwrap().control.queue_order,
            901
        );
        assert_eq!(index.queue(200).unwrap(), vec![second]);
        assert_eq!(index.aggregates().unwrap(), (2, 0, 0));
        let marker = checked_read(&fixture.config.state_dir.join(SENTINEL), 72).unwrap();
        assert!(
            Meta::parse(&marker).is_err(),
            "legacy torrent readers must reject marker"
        );
        assert_eq!(
            checked_read(
                &fixture
                    .config
                    .state_dir
                    .join(".transfers-legacy")
                    .join(format!("{first}.paused")),
                8
            )
            .unwrap(),
            b"paused\n"
        );
    }

    #[test]
    fn every_interrupted_initial_creation_resumes_with_bound_identity() {
        for stage in 1..=7 {
            let mut fixture = Fixture::new();
            fixture.source(1, Some(1));
            Index::inject_creation(Some(stage));
            assert!(TransferIndex::open(&mut fixture.config).is_err());
            Index::inject_creation(None);
            let mut index = TransferIndex::open(&mut fixture.config).unwrap();
            assert_eq!(index.aggregates().unwrap().0, 1, "stage {stage}");
        }
    }

    #[test]
    fn published_snapshot_save_recovers_without_dangling_reference() {
        for fault in [
            crate::inventory::Fault::Staged,
            crate::inventory::Fault::Published,
            crate::inventory::Fault::FirstPage,
            crate::inventory::Fault::Applied,
        ] {
            let mut fixture = Fixture::new();
            let id = fixture.source(1, Some(1));
            let mut index = TransferIndex::open(&mut fixture.config).unwrap();
            let job = load_job(&fixture.config, &id).unwrap();
            job.counters.downloaded.store(42, Ordering::Relaxed);
            index.index.inject(Some(fault));
            assert!(index.save(&job).is_err());
            assert!(index.get(&id).is_err());
            drop(index);
            let mut reopened = TransferIndex::open(&mut fixture.config).unwrap();
            let record = reopened.get(&id).unwrap().unwrap();
            assert!([0, 42].contains(&record.control.downloaded_bytes));
            assert_eq!(
                reopened.aggregates().unwrap().1,
                record.control.downloaded_bytes
            );
        }
    }

    #[test]
    fn snapshots_replace_without_retaining_update_history_and_listing_is_stable() {
        let mut fixture = Fixture::new();
        let id = fixture.source(1, Some(1));
        let mut index = TransferIndex::open(&mut fixture.config).unwrap();
        let job = load_job(&fixture.config, &id).unwrap();
        let before = index.page(None, 200).unwrap().3;
        for count in 1..=4 {
            job.counters.downloaded.store(count, Ordering::Relaxed);
            index.save(&job).unwrap();
        }
        assert_eq!(index.page(None, 200).unwrap().3, before);
        assert_eq!(
            fs::read_dir(fixture.config.state_dir.join("transfer-snapshots"))
                .unwrap()
                .count(),
            1
        );
        assert!(index.get(&id).unwrap().unwrap().current);
        let sequence = index.index.sequence().unwrap();
        index.save(&job).unwrap();
        assert_eq!(index.index.sequence().unwrap(), sequence);
        drop(index);
        let mut reopened = TransferIndex::open(&mut fixture.config).unwrap();
        assert!(!reopened.get(&id).unwrap().unwrap().current);
    }

    #[test]
    fn interrupted_import_resumes_and_existing_owner_preserves_marker() {
        let mut fixture = Fixture::new();
        for n in 1..=3 {
            fixture.source(n, None);
        }
        MIGRATION_STOP.with(|stop| stop.set(true));
        assert!(TransferIndex::open(&mut fixture.config).is_err());
        MIGRATION_STOP.with(|stop| stop.set(false));
        let mut index = TransferIndex::open(&mut fixture.config).unwrap();
        assert_eq!(index.aggregates().unwrap().0, 3);
        let marker = checked_read(&fixture.config.state_dir.join(SENTINEL), 72).unwrap();
        assert!(TransferIndex::open(&mut fixture.config).is_err());
        assert_eq!(
            checked_read(&fixture.config.state_dir.join(SENTINEL), 72).unwrap(),
            marker
        );
        drop(index);
        fs::rename(
            fixture.config.state_dir.join("transfers.idx"),
            fixture.config.state_dir.join("saved-index"),
        )
        .unwrap();
        assert!(
            TransferIndex::open(&mut fixture.config).is_err(),
            "ready marker must not rebuild missing committed index"
        );
    }

    #[test]
    fn large_status_snapshots_use_the_storage_parser_bound() {
        let fixture = Fixture::new();
        let id = fixture.source(1, Some(1));
        let mut job = load_job(&fixture.config, &id).unwrap();
        // A long configured absolute root repeats for every metadata path.
        // The metadata itself can be small while a status exceeds 16 MiB.
        let root = "segment/".repeat(112);
        job.status.files = (0..20_000)
            .map(|n| PathBuf::from(format!("/{root}file-{n}")))
            .collect();
        let record = Record {
            status: job.status.clone(),
            control: snapshot_control(&job),
            paused: false,
            failed: false,
            v1: job.source.v1,
            v2: job.source.v2,
            cross_seed: false,
            current: true,
            session: [0; 16],
        };
        let encoded = record.encode().unwrap();
        assert!(encoded.len() > 16 * 1024 * 1024);
        assert_eq!(
            Record::decode(&encoded).unwrap().status.files,
            job.status.files
        );
        assert!(Record::decode(&vec![0; SNAPSHOT_LIMIT + 1]).is_err());
    }

    #[test]
    fn published_catalog_damage_never_restarts_initial_creation() {
        for missing in [true, false] {
            let mut fixture = Fixture::new();
            fixture.source(1, Some(1));
            drop(TransferIndex::open(&mut fixture.config).unwrap());
            let marker_path = fixture.config.state_dir.join(SENTINEL);
            let marker = fs::read(&marker_path).unwrap();
            let catalog = fixture.config.state_dir.join("transfers.idx");
            let tree = catalog.join("tree.bin");
            for object in fs::read_dir(catalog.join("objects")).unwrap() {
                let object = object.unwrap();
                if object.file_type().unwrap().is_dir() {
                    fs::remove_dir_all(object.path()).unwrap();
                } else {
                    fs::remove_file(object.path()).unwrap();
                }
            }
            if missing {
                fs::remove_file(&tree).unwrap();
            } else {
                fs::write(&tree, []).unwrap();
            }
            assert!(TransferIndex::open(&mut fixture.config).is_err());
            assert_eq!(fs::read(&marker_path).unwrap(), marker);
            assert_eq!(fs::read_dir(catalog.join("objects")).unwrap().count(), 0);
            if missing {
                assert!(!tree.exists());
            } else {
                assert!(fs::read(tree).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn marker_loss_and_foreign_creation_artifacts_are_explicit_errors() {
        let mut fixture = Fixture::new();
        let index = TransferIndex::open(&mut fixture.config).unwrap();
        drop(index);
        fs::remove_file(fixture.config.state_dir.join(SENTINEL)).unwrap();
        assert!(TransferIndex::open(&mut fixture.config).is_err());
        let mut other = Fixture::new();
        Index::inject_creation(Some(1));
        assert!(TransferIndex::open(&mut other.config).is_err());
        Index::inject_creation(None);
        atomic_write(
            &other.config.state_dir.join("transfers.idx/foreign"),
            b"preserve",
        )
        .unwrap();
        assert!(TransferIndex::open(&mut other.config).is_err());
        assert_eq!(
            checked_read(&other.config.state_dir.join("transfers.idx/foreign"), 8).unwrap(),
            b"preserve"
        );
    }

    /// The ownership lock and persisted transfer state, swapped after
    /// inspection, are refused at open time without blocking.
    #[test]
    fn swapped_transfer_state_is_refused_at_open_without_blocking() {
        use crate::store::open_tests::{Swap, swapped};
        // The lock is read empty (first open), then opened for ownership.
        let cases = [
            (".transfers.lock", 0, "Cannot open private transfer state"),
            (".transfers.lock", 1, "Cannot open transfer ownership lock"),
            ("source", 0, "Cannot open private transfer state"),
        ];
        for (name, skip, opened) in cases {
            for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
                let fixture = Fixture::new();
                let id = fixture.source(1, Some(1));
                let mut config = fixture.config.clone();
                if name == ".transfers.lock" {
                    drop(TransferIndex::open(&mut config).unwrap());
                }
                let name = if name == "source" {
                    format!("{id}.source")
                } else {
                    name.to_owned()
                };
                let error = swapped(&name, skip, swap, move || {
                    TransferIndex::open(&mut config).map(drop)
                });
                match swap {
                    Swap::Symlink => assert_eq!(error, opened, "{name} {skip}"),
                    Swap::Fifo => assert!(
                        error == opened
                            || error == "Special state files are not allowed"
                            || error == "Invalid or changed transfer ownership lock",
                        "{name} {skip}: {error}"
                    ),
                    Swap::Replaced if skip == 1 => {
                        assert_eq!(error, "Invalid or changed transfer ownership lock")
                    }
                    Swap::Replaced => assert_eq!(
                        error, "Transfer state changed during opening",
                        "{name} {skip}"
                    ),
                }
            }
        }
    }
}
