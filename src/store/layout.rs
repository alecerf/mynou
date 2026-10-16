//! Keys and per-job secondary entries of the job index (`jobs.idx`).
//!
//! A job is one record plus small inline entries in the same ordered index:
//! creation order, canonical identity, media identity, the claim queue, shared
//! ownership, transfer ownership, lineage and requester/IRC ownership. Every
//! entry is derived from the job itself, so one commit replaces the job and
//! its entries together. No table is enumerated to answer a routine question.
use super::{Job, Result};
use crate::{
    crypto::sha256,
    inventory::{Change, Key},
    json,
};
use std::collections::BTreeMap;

pub(super) const FORMAT: &[u8] = b"mynou-jobs/1";

pub(super) const T_META: u8 = 0x01;
pub(super) const T_MIGRATION: u8 = 0x02;
pub(super) const T_JOB: u8 = 0x10;
pub(super) const T_EVENTS: u8 = 0x11;
pub(super) const T_ALL: u8 = 0x12;
pub(super) const T_KEY: u8 = 0x20;
pub(super) const T_MEDIA: u8 = 0x21;
pub(super) const T_QUEUE: u8 = 0x22;
pub(super) const T_WAIT: u8 = 0x23;
pub(super) const T_GROUP: u8 = 0x24;
pub(super) const T_PHYSICAL: u8 = 0x25;
pub(super) const T_SHARED_MEDIA: u8 = 0x26;
pub(super) const T_PENDING: u8 = 0x27;
pub(super) const T_CANDIDATE: u8 = 0x28;
pub(super) const T_LIBRARY: u8 = 0x29;
pub(super) const T_ROOT: u8 = 0x2a;
pub(super) const T_TRANSFER: u8 = 0x2b;
pub(super) const T_RETIRE: u8 = 0x2c;
pub(super) const T_REQUESTER: u8 = 0x2d;
pub(super) const T_IRC: u8 = 0x2e;
pub(super) const T_PACKREF: u8 = 0x2f;
pub(super) const T_IDENTITY: u8 = 0x30;
pub(super) const T_CHILD: u8 = 0x31;
pub(super) const T_TIP: u8 = 0x32;
pub(super) const T_SEQUENCE: u8 = 0x33;
pub(super) const T_DEMAND: u8 = 0x34;
pub(super) const T_ACQUISITION: u8 = 0x35;
pub(super) const T_IRC_ORIGIN: u8 = 0x36;
pub(super) const T_MEDIA_TIP: u8 = 0x37;
pub(super) const T_JOB_CHUNK: u8 = 0x38;
pub(super) const T_MONITOR_DUE: u8 = 0x39;
pub(super) const T_MONITOR_REF: u8 = 0x3a;
pub(super) const T_MONITOR_SHARED: u8 = 0x3b;
pub(super) const T_MONITOR_UNCHECKED: u8 = 0x3c;
pub(super) const T_IMPORTED_CHILD: u8 = 0x3d;

pub(super) const FORMAT_KEY: Key = single(T_META, 0);
pub(super) const COUNTS_KEY: Key = single(T_META, 1);
pub(super) const LIBRARY_COUNT_KEY: Key = single(T_META, 2);
pub(super) const MONITOR_COUNTS_KEY: Key = single(T_META, 3);
pub(super) const MONITOR_CURSOR_KEY: Key = single(T_META, 4);
pub(super) const CHILD_CURSOR_KEY: Key = single(T_META, 5);
/// Resume point of the single-account Plex watchlist sweep.
pub(super) const WATCHLIST_CURSOR_KEY: Key = single(T_META, 6);
pub(super) const CLAIM_CURSOR_KEY: Key = single(T_META, 7);
pub(super) const REQUESTER_SWEEP_CURSOR_KEY: Key = single(T_META, 8);
pub(super) const MIGRATION_SOURCE_KEY: Key = single(T_MIGRATION, 0);
pub(super) const MIGRATION_CURSOR_KEY: Key = single(T_MIGRATION, 1);

const fn single(table: u8, sub: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key[1] = sub;
    key
}

pub(super) fn key(parts: &[&[u8]]) -> Key {
    let mut key = [0; 64];
    let mut at = 0;
    for part in parts {
        // Every caller passes fixed-width parts that fit 64 bytes; a longer
        // sequence is cut instead of panicking, and tests cover each width.
        let end = (at + part.len()).min(64);
        key[at..end].copy_from_slice(&part[..end - at]);
        at = end;
    }
    key
}

/// `[start, end)` covering every key that begins with `prefix`.
pub(super) fn prefix_range(prefix: &[u8]) -> (Key, Option<Key>) {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last != 0xff {
            *last += 1;
            return (key(&[prefix]), Some(key(&[&end])));
        }
        end.pop();
    }
    (key(&[prefix]), None)
}

pub(super) fn id_bytes(id: &str) -> Result<[u8; 16]> {
    let bytes = id.as_bytes();
    if bytes.len() != 32 || !bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("Invalid job identifier".into());
    }
    let nibble = |byte: u8| {
        if byte <= b'9' {
            byte - b'0'
        } else {
            byte - b'a' + 10
        }
    };
    let mut out = [0; 16];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks(2)) {
        *slot = nibble(pair[0]) << 4 | nibble(pair[1]);
    }
    Ok(out)
}

pub(super) fn id_text(bytes: &[u8]) -> String {
    super::hex(bytes)
}

/// A hex digest field (job key, media key) as raw bytes.
pub(super) fn digest_bytes(text: &str) -> Result<[u8; 32]> {
    super::unhex_digest(text)
}

/// Sixteen bytes identifying an arbitrary text, bound to its purpose.
pub(super) fn short(domain: &str, parts: &[&str]) -> [u8; 16] {
    let mut input = Vec::new();
    input.extend_from_slice(domain.as_bytes());
    for part in parts {
        input.push(0);
        input.extend_from_slice(&(part.len() as u64).to_le_bytes());
        input.extend_from_slice(part.as_bytes());
    }
    let digest = sha256(&input);
    let mut out = [0; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

pub(super) fn job_key(id: &[u8; 16]) -> Key {
    key(&[&[T_JOB], id])
}
pub(super) fn events_key(id: &[u8; 16], sequence: u64) -> Key {
    key(&[&[T_EVENTS], id, &sequence.to_be_bytes()])
}
pub(super) fn media_tip_key(media: &[u8; 32]) -> Key {
    key(&[&[T_MEDIA_TIP], media])
}
/// Newest first: the creation time is stored inverted.
pub(super) fn all_key(created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_ALL], &(!created).to_be_bytes(), id])
}
pub(super) fn canonical_key(digest: &[u8; 32]) -> Key {
    key(&[&[T_KEY], digest])
}
pub(super) fn media_key(media: &[u8; 32], created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_MEDIA], media, &created.to_be_bytes(), id])
}
pub(super) fn queue_key(created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_QUEUE], &created.to_be_bytes(), id])
}
pub(super) fn wait_key(wake: u64, created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_WAIT], &wake.to_be_bytes(), &created.to_be_bytes(), id])
}
pub(super) fn group_key(group: &[u8; 16], episode: u32) -> Key {
    key(&[&[T_GROUP], group, &episode.to_be_bytes()])
}
pub(super) fn physical_key(torrent: &str, path: &str) -> Key {
    key(&[&[T_PHYSICAL], &short("physical", &[torrent, path])])
}
pub(super) fn shared_media_key(media: &[u8; 32]) -> Key {
    key(&[&[T_SHARED_MEDIA], media])
}
pub(super) fn pending_key(media: &[u8; 32], id: &[u8; 16]) -> Key {
    key(&[&[T_PENDING], media, id])
}
pub(super) fn candidate_key(media: &[u8; 32], root_created: u64, root: &[u8; 16]) -> Key {
    key(&[&[T_CANDIDATE], media, &root_created.to_be_bytes(), root])
}
pub(super) fn library_key(created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_LIBRARY], &(!created).to_be_bytes(), id])
}
pub(super) fn root_key(id: &[u8; 16]) -> Key {
    key(&[&[T_ROOT], id])
}
pub(super) fn transfer_key(transfer: &str, id: &[u8; 16]) -> Key {
    key(&[&[T_TRANSFER], &short("transfer", &[transfer]), id])
}
pub(super) fn retire_key(id: &[u8; 16]) -> Key {
    key(&[&[T_RETIRE], id])
}
pub(super) fn requester_key(created: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_REQUESTER], &created.to_be_bytes(), id])
}
pub(super) fn irc_prefix(file: &str, alias: &str) -> Vec<u8> {
    [
        &[T_IRC][..],
        &short("irc-file", &[file]),
        &short("irc-alias", &[alias]),
    ]
    .concat()
}
pub(super) fn irc_key(file: &str, alias: &str, id: &[u8; 16]) -> Key {
    key(&[&irc_prefix(file, alias), id])
}
pub(super) fn packref_prefix(path: &str, kind: &str, value: &str) -> Vec<u8> {
    [
        &[T_PACKREF][..],
        &short("pack-file", &[path]),
        &short("pack-ref", &[kind, value]),
    ]
    .concat()
}
pub(super) fn packref_key(path: &str, kind: &str, value: &str, id: &[u8; 16]) -> Key {
    key(&[&packref_prefix(path, kind, value), id])
}
pub(super) fn identity_prefix(kind: &str, tmdb: Option<u64>, season: u32, episode: u32) -> Vec<u8> {
    let tmdb = tmdb.map_or_else(|| "-".to_owned(), |id| id.to_string());
    [
        &[T_IDENTITY][..],
        &short(
            "identity",
            &[kind, &tmdb, &season.to_string(), &episode.to_string()],
        ),
    ]
    .concat()
}
pub(super) fn child_key(parent: &[u8; 16], child: &[u8; 16]) -> Key {
    key(&[&[T_CHILD], parent, child])
}
/// Present only for a ready import, so "does the parent have one" is a
/// single-row read regardless of how many other children it ever had.
pub(super) fn imported_child_key(parent: &[u8; 16], child: &[u8; 16]) -> Key {
    key(&[&[T_IMPORTED_CHILD], parent, child])
}
pub(super) fn tip_key(id: &[u8; 16]) -> Key {
    key(&[&[T_TIP], id])
}
pub(super) fn sequence_key(sequence: u64) -> Key {
    key(&[&[T_SEQUENCE], &sequence.to_be_bytes()])
}
pub(super) fn acquisition_prefix(url: &str) -> Vec<u8> {
    [&[T_ACQUISITION][..], &short("acquisition", &[url])].concat()
}
pub(super) fn acquisition_key(url: &str, id: &[u8; 16]) -> Key {
    key(&[&acquisition_prefix(url), id])
}
pub(super) fn demand_key(demand: &[u8; 32], id: &[u8; 16]) -> Key {
    key(&[&[T_DEMAND], demand, id])
}

/// Every transfer identity a job depends on.
pub(super) fn transfers(job: &Job) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    if let Some(id) = &job.download_id {
        found.push(id);
    }
    if let Some(file) = &job.shared_file {
        found.push(&file.torrent_id);
    }
    if let Some(origin) = &job.pack_origin {
        found.push(&origin.torrent_id);
    }
    if let Some(origin) = &job.irc_origin {
        found.push(&origin.torrent_id);
        for alias in &origin.torrent_aliases {
            found.push(alias);
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Torrent identities a pack-mapped job offers to the pack-file reference table.
pub(super) fn pack_transfers(job: &Job) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    if let Some(origin) = &job.pack_origin {
        found.push(&origin.torrent_id);
    }
    if let Some(file) = &job.shared_file {
        found.push(&file.torrent_id);
    }
    if let Some(id) = &job.download_id {
        found.push(id);
    }
    found.sort_unstable();
    found.dedup();
    found
}

pub(super) fn terminal(state: &str) -> bool {
    matches!(state, "ready" | "failed" | "cancelled")
}

/// A job the claim queue may hand out at some time.
pub(super) fn open(job: &Job) -> bool {
    !matches!(job.state.as_str(), "ready" | "cancelled" | "staged")
        && (job.state != "failed" || job.next_attempt_at > 0)
}

/// The first instant at which an open job may be claimed.
pub(super) fn wake(job: &Job) -> u64 {
    if job.lease_id.is_some() {
        job.next_attempt_at.max(job.lease_until)
    } else {
        job.next_attempt_at
    }
}

pub(super) fn pending_upgrade(job: &Job) -> bool {
    job.upgrade_parent.is_some()
        && !matches!(job.state.as_str(), "ready" | "cancelled")
        && (job.shared_upgrade.is_some() || job.state != "failed" || job.next_attempt_at > 0)
}

/// Whether a ready import can be the library's current version of its media.
pub(super) fn imported(job: &Job) -> bool {
    job.state == "ready" && !job.imports.is_empty()
}

/// How a key is stored: a record file, or a tiny value inline in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    File(Vec<u8>),
    Inline(Vec<u8>),
}

impl Entry {
    pub(super) fn change(self, key: Key) -> Change {
        match self {
            Self::File(data) => Change::Put(key, data),
            Self::Inline(data) => Change::PutInline(key, data),
        }
    }
    pub(super) fn bytes(&self) -> &[u8] {
        match self {
            Self::File(data) | Self::Inline(data) => data,
        }
    }
}

/// A job's identity roots: the first ancestor of its upgrade lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Root {
    pub id: [u8; 16],
    pub created_at: u64,
}

impl Root {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut bytes = self.created_at.to_be_bytes().to_vec();
        bytes.extend_from_slice(&self.id);
        bytes
    }
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 24 {
            return Err("Invalid job lineage entry".into());
        }
        let mut created = [0; 8];
        created.copy_from_slice(&bytes[..8]);
        let mut id = [0; 16];
        id.copy_from_slice(&bytes[8..]);
        Ok(Self {
            id,
            created_at: u64::from_be_bytes(created),
        })
    }
}

/// The stored form of a job.
pub(super) fn encode_job(job: &Job) -> Vec<u8> {
    json::stringify(&job.to_json()).into_bytes()
}

pub(super) fn decode_job(bytes: &[u8], expected: &str) -> Result<Job> {
    let value = json::parse(std::str::from_utf8(bytes).map_err(|_| "Invalid job record")?)?;
    let job = Job::from_json(&value)?;
    if job.id != expected {
        return Err("Job record identity differs from its key".into());
    }
    Ok(job)
}

/// Digest of the canonical form of a shared-file binding.
pub(super) fn file_digest(file: &crate::pack::SharedFile) -> [u8; 32] {
    let mut value = json::Value::object();
    value.insert("torrent_id", file.torrent_id.clone());
    value.insert("file_path", file.file_path.clone());
    value.insert("tmdb_id", json::Value::Number(file.tmdb_id as f64));
    value.insert("title", file.title.clone());
    value.insert("year", file.year);
    value.insert("season", file.season);
    value.insert("first_episode", file.first_episode);
    value.insert("last_episode", file.last_episode);
    value.insert("import_path", file.import_path.clone());
    sha256(json::stringify(&value).as_bytes())
}

/// The entries a job owns for the given lineage root. `queued` says whether
/// the job currently waits in the due queue instead of the wait list.
pub(super) fn owned(job: &Job, root: &Root) -> Result<BTreeMap<Key, Entry>> {
    let id = id_bytes(&job.id)?;
    let canonical = digest_bytes(&job.key)?;
    let media = digest_bytes(&job.request.media_key())?;
    let mut entries = BTreeMap::new();
    let mut inline = |key: Key, value: &[u8]| {
        entries.insert(key, Entry::Inline(value.to_vec()));
    };
    inline(all_key(job.created_at, &id), &[]);
    inline(canonical_key(&canonical), &id);
    inline(media_key(&media, job.created_at, &id), &[]);
    inline(root_key(&id), &root.encode());
    if open(job) {
        inline(wait_key(wake(job), job.created_at, &id), &[]);
    }
    if let Some(file) = &job.shared_file {
        let group = short("group", &[&file.id()]);
        inline(group_key(&group, job.request.episode), &id);
        inline(
            physical_key(&file.torrent_id, &file.file_path),
            &file_digest(file),
        );
        if job.shared_upgrade.is_none() {
            inline(shared_media_key(&media), &id);
        }
    }
    if pending_upgrade(job) {
        inline(pending_key(&media, &id), &[]);
    }
    if let Some(parent) = &job.upgrade_parent {
        let parent = id_bytes(parent)?;
        inline(child_key(&parent, &id), &[u8::from(imported(job))]);
        if imported(job) {
            inline(imported_child_key(&parent, &id), &[]);
        }
    }
    for transfer in transfers(job) {
        inline(transfer_key(transfer, &id), &[]);
    }
    if let Some(url) = &job.acquisition_url {
        inline(acquisition_key(url, &id), &[]);
    }
    if !job.retiring_transfers.is_empty() {
        inline(retire_key(&id), &[]);
    }
    if job.requester.is_some() && !matches!(job.state.as_str(), "ready" | "cancelled") {
        inline(requester_key(job.created_at, &id), &[]);
    }
    if let Some(origin) = &job.irc_origin {
        inline(key(&[&[T_IRC_ORIGIN], &id]), &[]);
        for alias in &origin.torrent_aliases {
            inline(irc_key(&origin.file, alias, &id), &[]);
        }
    }
    if let Some(path) = &job.pack_file {
        for transfer in pack_transfers(job) {
            inline(packref_key(path, "t", transfer, &id), &[]);
        }
        if let Some(url) = &job.request.source_url {
            inline(packref_key(path, "u", url, &id), &[]);
        }
    }
    if let Some(provenance) = &job.requester {
        let demand = digest_bytes(&provenance.demand_id)?;
        inline(demand_key(&demand, &id), &[]);
    }
    let request = &job.request;
    if request.tmdb_id.is_some() {
        inline(
            key(&[
                &identity_prefix(
                    &request.kind,
                    request.tmdb_id,
                    request.season,
                    request.episode,
                ),
                &id,
            ]),
            &[],
        );
    }
    let encoded = encode_job(job);
    if encoded.len() > super::legacy::MAX_RECORD {
        return Err("A job record exceeds the 16 MiB record limit".into());
    }
    if encoded.len() <= crate::inventory::MAX_VALUE_BYTES {
        entries.insert(job_key(&id), Entry::File(encoded));
    } else {
        let mut manifest = json::Value::object();
        let chunks = encoded.chunks(crate::inventory::MAX_VALUE_BYTES);
        manifest.insert("job_chunks", chunks.len() as u32);
        manifest.insert("job_bytes", encoded.len() as u32);
        manifest.insert("job_digest", super::hex(&sha256(&encoded)));
        entries.insert(
            job_key(&id),
            Entry::File(json::stringify(&manifest).into_bytes()),
        );
        for (n, chunk) in chunks.enumerate() {
            entries.insert(
                key(&[&[T_JOB_CHUNK], &id, &(n as u32).to_be_bytes()]),
                Entry::File(chunk.to_vec()),
            );
        }
    }
    Ok(entries)
}
