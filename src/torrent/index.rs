//! Bounded transfer catalog. Immutable snapshots are published before the index
//! transaction that references them; unreferenced snapshots are safe after a crash.
use super::*;
use crate::{
    crypto::sha256,
    inventory::{Change, Index, Key, MAX_PAGE_ROWS, Query},
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
const FORMAT: &[u8] = b"mynou-transfers/2";
const SNAPSHOT_LIMIT: usize = 64 * 1024 * 1024;

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

/// Deletes every secondary key a record holds, before it is replaced or forgotten.
fn release_keys(old: &Record, changes: &mut BTreeMap<Key, Option<Vec<u8>>>) -> Result<()> {
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
    Ok(())
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
        let mut index = match fs::symlink_metadata(&path) {
            Ok(_) => Index::open(&path, false)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create(&config.state_dir)?
            }
            Err(_) => return Err("Cannot inspect transfer index".into()),
        };
        if index.get(&HEADER)?.as_deref() != Some(FORMAT) {
            return Err("Unsupported transfer catalog format".into());
        }
        crate::inventory::backlog::prepare(&mut index, single(PRIMARY), single(PRIMARY + 1))?;
        Ok(Self {
            index,
            directory: config.state_dir.clone(),
            session: crate::crypto::random_bytes()?,
            _ownership: ownership,
        })
    }
    pub(super) fn backlog(&mut self, at: u64) -> Result<crate::json::Value> {
        crate::inventory::backlog::read(&mut self.index, "transfer_queue_scan", at)
    }
    pub(super) fn finish_backlog_pass(&mut self, at: u64) -> Result<()> {
        crate::inventory::backlog::finish(&mut self.index, at)
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
            release_keys(old, &mut changes)?;
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
        let mut changes: Vec<_> = changes
            .into_iter()
            .map(|(k, v)| match v {
                Some(v) if v.len() <= 32 => Change::PutInline(k, v),
                Some(v) => Change::Put(k, v),
                None => Change::Delete(k),
            })
            .collect();
        crate::inventory::backlog::maintain(
            &mut self.index,
            &mut changes,
            &primary,
            record.queued(),
            crate::store::now(),
        )?;
        self.index.commit(changes)?;
        if let Some(old_digest) = old_digest {
            let path = snapshots.join(hex(&old_digest));
            fs::remove_file(path).map_err(|_| "Cannot remove superseded transfer snapshot")?;
            crate::store::sync_directory(&snapshots)
                .map_err(|_| "Cannot synchronize transfer snapshots")?;
        }
        Ok(())
    }
    /// Forgets a transfer: its record, every secondary key and its snapshot.
    /// A transfer already forgotten is not an error.
    pub fn remove(&mut self, id: &str) -> Result<()> {
        let primary = id_key(PRIMARY, id)?;
        let Some(old) = self.get(id)? else {
            return Ok(());
        };
        let old_digest = self.index.get(&primary)?;
        let mut changes = BTreeMap::new();
        release_keys(&old, &mut changes)?;
        let mut counts = self.counts()?;
        counts.listing = counts
            .listing
            .checked_add(1)
            .ok_or("Transfer listing sequence exhausted")?;
        counts.count = counts
            .count
            .checked_sub(1)
            .ok_or("Invalid transfer count")?;
        counts.downloaded = counts
            .downloaded
            .checked_sub(u128::from(old.control.downloaded_bytes))
            .ok_or("Invalid downloaded aggregate")?;
        counts.uploaded = counts
            .uploaded
            .checked_sub(u128::from(old.control.uploaded_bytes))
            .ok_or("Invalid uploaded aggregate")?;
        changes.insert(primary, None);
        changes.insert(COUNTS, Some(counts.encode()));
        let mut changes: Vec<_> = changes
            .into_iter()
            .map(|(k, v)| match v {
                Some(v) if v.len() <= 32 => Change::PutInline(k, v),
                Some(v) => Change::Put(k, v),
                None => Change::Delete(k),
            })
            .collect();
        crate::inventory::backlog::maintain(
            &mut self.index,
            &mut changes,
            &primary,
            false,
            crate::store::now(),
        )?;
        self.index.commit(changes)?;
        if let Some(old_digest) = old_digest {
            let snapshots = self.directory.join("transfer-snapshots");
            match fs::remove_file(snapshots.join(hex(&old_digest))) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("Cannot remove the transfer snapshot".into()),
            }
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
    /// The indexed wire hash that `matches`. An encrypted handshake names its
    /// torrent only through a hash of it, so every torrent is checked.
    pub fn wire_matching(
        &mut self,
        matches: impl Fn(&[u8; 20]) -> bool,
    ) -> Result<Option<[u8; 20]>> {
        let mut after = None;
        loop {
            let page = self.index.page(&Query {
                start: single(WIRE),
                end: Some(single(WIRE + 1)),
                after,
                limit: MAX_PAGE_ROWS,
                ..Query::default()
            })?;
            for (key, _) in &page.entries {
                if let Ok(wire) = <[u8; 20]>::try_from(&key[2..22])
                    && key[1] == 20
                    && matches(&wire)
                {
                    return Ok(Some(wire));
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(None),
            }
        }
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
}

/// Builds an empty catalog under a temporary name, then publishes it.
fn create(directory: &Path) -> Result<Index> {
    let staging = directory.join("transfers.idx.creating");
    crate::store::reject_symlinks(&staging)?;
    if fs::symlink_metadata(&staging).is_ok() {
        // Only an interrupted creation leaves it, and it held no transfer.
        fs::remove_dir_all(&staging).map_err(|_| "Cannot remove an unfinished transfer index")?;
    }
    let mut index = Index::create(&staging)?;
    index.commit(vec![
        Change::PutInline(HEADER, FORMAT.to_vec()),
        Change::Put(COUNTS, Counts::default().encode()),
    ])?;
    crate::inventory::backlog::prepare(&mut index, single(PRIMARY), single(PRIMARY + 1))?;
    drop(index);
    let path = directory.join("transfers.idx");
    fs::rename(&staging, &path).map_err(|_| "Cannot publish the transfer index")?;
    crate::store::sync_directory(directory).map_err(|_| "Cannot synchronize the transfer index")?;
    Index::open(&path, false)
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
                max_seeding: 0,
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
        /// Records the transfer of `source` in the index, as adding it does.
        fn add(&self, index: &mut TransferIndex, id: &str) {
            index.save(&load_job(&self.config, id).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn transfer_backlog_follows_pause_failure_completion_and_restart() {
        use crate::json::Value as Json;
        let mut fixture = Fixture::new();
        let first = fixture.source(1, Some(1));
        let second = fixture.source(2, Some(2));
        let mut index = TransferIndex::open(&mut fixture.config).unwrap();
        fixture.add(&mut index, &first);
        fixture.add(&mut index, &second);
        assert_eq!(
            index.backlog(u64::MAX).unwrap().get("due"),
            Some(&Json::from("2"))
        );
        let mut record = index.get(&first).unwrap().unwrap();
        record.paused = true;
        index.save_record(&record).unwrap();
        assert_eq!(
            index.backlog(u64::MAX).unwrap().get("due"),
            Some(&Json::from("1"))
        );
        record.paused = false;
        index.save_record(&record).unwrap();
        index.finish_backlog_pass(100).unwrap();
        drop(index);
        let mut index = TransferIndex::open(&mut fixture.config).unwrap();
        let value = index.backlog(u64::MAX).unwrap();
        assert_eq!(value.get("due"), Some(&Json::from("2")));
        assert_eq!(
            value.get("last_completed_pass_at"),
            Some(&Json::from("100"))
        );
        index.mark_failed(&first, "synthetic failure").unwrap();
        let mut other = index.get(&format!("{:040x}", 2)).unwrap().unwrap();
        other.status.selected_ready = true;
        index.save_record(&other).unwrap();
        let value = index.backlog(u64::MAX).unwrap();
        assert_eq!(value.get("due"), Some(&Json::from("0")));
        assert_eq!(value.get("oldest_due_at"), Some(&Json::Null));
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
            fixture.add(&mut index, &id);
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
        fixture.add(&mut index, &id);
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
            let id = fixture.source(1, Some(1));
            let mut index = TransferIndex::open(&mut fixture.config).unwrap();
            fixture.add(&mut index, &id);
            drop(index);
            let tree = fixture.config.state_dir.join("transfers.idx/tree.bin");
            if missing {
                fs::remove_file(&tree).unwrap();
            } else {
                fs::write(&tree, []).unwrap();
            }
            assert!(TransferIndex::open(&mut fixture.config).is_err());
            if missing {
                assert!(!tree.exists());
            } else {
                assert!(fs::read(tree).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn foreign_catalog_directories_are_errors_and_kept() {
        let mut fixture = Fixture::new();
        atomic_write(
            &fixture.config.state_dir.join("transfers.idx/foreign"),
            b"preserve",
        )
        .unwrap();
        assert!(TransferIndex::open(&mut fixture.config).is_err());
        assert_eq!(
            checked_read(&fixture.config.state_dir.join("transfers.idx/foreign"), 8).unwrap(),
            b"preserve"
        );
        // An interrupted creation leaves only staging, which held nothing.
        let mut other = Fixture::new();
        drop(Index::create(&other.config.state_dir.join("transfers.idx.creating")).unwrap());
        let mut index = TransferIndex::open(&mut other.config).unwrap();
        assert_eq!(index.aggregates().unwrap().0, 0);
        assert!(
            !other
                .config
                .state_dir
                .join("transfers.idx.creating")
                .exists()
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
        ];
        for (name, skip, opened) in cases {
            for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
                let fixture = Fixture::new();
                let mut config = fixture.config.clone();
                drop(TransferIndex::open(&mut config).unwrap());
                let error = swapped(name, skip, swap, move || {
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
    #[test]
    fn full_disk_at_every_creation_flush_restarts_cleanly() {
        for boundary in 0..1000 {
            let mut fixture = Fixture::new();
            let result = crate::store::with_full_disk_after(boundary, || {
                TransferIndex::open(&mut fixture.config)
            });
            let error = match result {
                Ok(value) => {
                    drop(value);
                    assert!(boundary > 3);
                    return;
                }
                Err(error) => error,
            };
            assert!(
                error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
                "boundary {boundary}: {error}"
            );
            assert!(!error.contains(fixture.root.to_str().unwrap()), "{error}");
            let mut index = TransferIndex::open(&mut fixture.config).unwrap();
            assert_eq!(index.aggregates().unwrap().0, 0);
        }
        panic!("creation fixture exceeded its durability boundary budget");
    }
}

#[cfg(test)]
mod growth_report {
    use super::*;
    #[test]
    fn inventory_growth_report() {
        let Some(mut f) = crate::growth::Fixture::get("transfers") else {
            return;
        };
        let directory = f.directory.clone();
        let size = f.size;
        let mut config = DownloadConfig {
            data_dir: directory.join("data"),
            state_dir: directory.join("state"),
            listen_port: 0,
            seed: false,
            dht: false,
            pex: false,
            max_active: 1,
            max_seeding: 0,
            max_peers: 1,
        };
        mkdir_private(&config.state_dir).unwrap();
        mkdir_private(&config.data_dir).unwrap();
        let mut index = f.measure("open_us", || TransferIndex::open(&mut config).unwrap());
        if f.build {
            f.measure("build_us", || {
                for n in 1..=size {
                    let id = format!("{n:040x}");
                    let record = Record {
                        status: DownloadStatus {
                            id: id.clone(),
                            progress: 0.0,
                            ready: false,
                            selected_ready: false,
                            files: Vec::new(),
                            available_files: Vec::new(),
                            message: "synthetic".into(),
                        },
                        control: TorrentControl::new(id, n).unwrap(),
                        paused: false,
                        failed: false,
                        v1: Some(decode_hex(&format!("{n:040x}")).unwrap()),
                        v2: None,
                        cross_seed: false,
                        current: true,
                        session: index.session,
                    };
                    index.save_record(&record).unwrap();
                }
            });
        } else {
            assert_eq!(index.aggregates().unwrap().0, size);
            f.measure("page_us", || {
                let (ids, _, _, _) = index.page(None, 200).unwrap();
                assert_eq!(ids.len(), 200);
                for id in ids {
                    index.get(&id).unwrap().unwrap();
                }
            });
            f.measure("pass_us", || {
                assert_eq!(index.queue(64).unwrap().len(), 64);
            });
            f.measure("update_us", || {
                let mut row = index.get(&format!("{:040x}", size)).unwrap().unwrap();
                row.control.priority += 1;
                index.save_record(&row).unwrap();
            });
            let mut after = None;
            let mut count = 0;
            f.measure("walk_us", || {
                loop {
                    let (ids, next, _, _) = index.page(after.as_deref(), 200).unwrap();
                    count += ids.len() as u64;
                    for id in ids {
                        index.get(&id).unwrap().unwrap();
                    }
                    after = next;
                    if after.is_none() {
                        break;
                    }
                }
            });
            assert_eq!(count, size);
        }
        f.finish();
    }
}
