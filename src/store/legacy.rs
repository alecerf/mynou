//! The request journal and snapshot of earlier versions (`journal.bin`,
//! `snapshot.bin`, formats 1 to 5), and the marker that replaces the snapshot
//! once the job index owns the data.
//!
//! The parser keeps every bound the old store applied (16 MiB snapshot, 16 MiB
//! records, 64 MiB journal, hash chain, checksums) but not the 10,000-job total,
//! which limited how many jobs the snapshot could hold and which the index no
//! longer has. Parsing never writes: an incomplete final journal record is
//! ignored (the preserved copy keeps its bytes), a complete corrupt record is an
//! error. The whole final state is validated before migration writes anything.
use super::{
    Event, Job, MAX_EVENTS, Result, fields, group_format, hex, integer, string, unhex_digest,
};
use crate::{
    crypto::sha256,
    json::{self, Value},
    store::{private_options, reject_symlinks},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

pub(super) const JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ01";
pub(super) const SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS01";
// Old readers must reject shared ownership instead of silently ignoring its owners.
pub(super) const SHARED_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ02";
pub(super) const SHARED_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS02";
pub(super) const GROUP_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ03";
pub(super) const GROUP_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS03";
pub(super) const REQUESTER_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ04";
pub(super) const REQUESTER_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS04";
pub(super) const IRC_JOURNAL_MAGIC: &[u8; 8] = b"MYNOUJ05";
pub(super) const IRC_SNAPSHOT_MAGIC: &[u8; 8] = b"MYNOUS05";
/// Snapshot-position marker of the index format. Earlier versions reject the
/// unknown magic instead of reading stale or empty data.
pub(super) const MARKER_MAGIC: &[u8; 8] = b"MYNOUS09";
const MARKER_DOMAIN: &[u8] = b"mynou-jobs-index/1\0";

pub(super) const MAX_RECORD: usize = 16 * 1024 * 1024;
pub(super) const MAX_SNAPSHOT: usize = 16 * 1024 * 1024;
pub(super) const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

pub(super) const SNAPSHOT: &str = "snapshot.bin";
pub(super) const JOURNAL: &str = "journal.bin";

/// The exact bytes of the legacy files, read within their historical bounds.
pub(super) struct Files {
    pub snapshot: Option<Vec<u8>>,
    pub journal: Vec<u8>,
    /// Digest binding both files, used to tie a staging index to its source.
    pub digest: [u8; 32],
}

pub(super) enum Inspection {
    /// No snapshot and an empty or absent journal.
    Missing,
    Legacy(Files),
    /// The index marker, binding the directory to one index identity.
    Marker { binding: String },
}

pub(super) fn digest_of(snapshot: Option<&[u8]>, journal: &[u8]) -> [u8; 32] {
    let mut input = b"mynou-jobs-legacy/1\0".to_vec();
    for (name, bytes) in [("snapshot", snapshot), ("journal", Some(journal))] {
        input.extend_from_slice(name.as_bytes());
        input.push(u8::from(bytes.is_some()));
        let bytes = bytes.unwrap_or(&[]);
        input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        input.extend_from_slice(&sha256(bytes));
    }
    sha256(&input)
}

pub(super) fn marker_bytes(identity: &[u8; 32]) -> Vec<u8> {
    let mut payload = Value::object();
    payload.insert("format", "mynou-jobs-index/1");
    payload.insert("binding", marker_binding(identity));
    let payload = json::stringify(&payload).into_bytes();
    let mut bytes = Vec::with_capacity(payload.len() + 48);
    bytes.extend_from_slice(MARKER_MAGIC);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&sha256(&bytes));
    bytes
}

pub(super) fn marker_binding(identity: &[u8; 32]) -> String {
    let mut input = MARKER_DOMAIN.to_vec();
    input.extend_from_slice(identity);
    hex(&sha256(&input))
}

/// Reads a regular, single-link file of at most `limit` bytes. `None` when it
/// does not exist. A group or world accessible file is repaired to 0600 only
/// when `repair` is set (a writable open), as the earlier store did.
pub(super) fn read_bounded(path: &Path, limit: u64, repair: bool) -> Result<Option<Vec<u8>>> {
    reject_symlinks(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Cannot inspect job storage".into()),
    };
    if !metadata.is_file() || metadata.len() > limit {
        return Err("Job storage file is not a regular file within its size bound".into());
    }
    let mut file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "Cannot open job storage".to_owned())?;
    let opened = file
        .metadata()
        .map_err(|_| "Cannot inspect job storage".to_owned())?;
    if opened.nlink() != 1 || opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err("Job storage files cannot share hard links".into());
    }
    if opened.permissions().mode() & 0o077 != 0 {
        if !repair {
            return Err("Job storage files must be private".into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "Cannot restrict job storage permissions".to_owned())?;
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read job storage".to_owned())?;
    if bytes.len() as u64 > limit {
        return Err("Job storage file grew beyond its bound".into());
    }
    Ok(Some(bytes))
}

fn snapshot_limit() -> u64 {
    MAX_SNAPSHOT as u64 + 48
}

/// Reads `snapshot.bin` and `journal.bin` without interpreting records.
pub(super) fn inspect(directory: &Path, repair: bool) -> Result<Inspection> {
    let snapshot = read_bounded(&directory.join(SNAPSHOT), snapshot_limit(), repair)?;
    if let Some(bytes) = &snapshot
        && bytes.starts_with(MARKER_MAGIC)
    {
        let binding = parse_marker(bytes)?;
        return Ok(Inspection::Marker { binding });
    }
    let journal = read_bounded(&directory.join(JOURNAL), MAX_JOURNAL_BYTES, repair)?
        .unwrap_or_default();
    if snapshot.is_none() && journal.is_empty() {
        return Ok(Inspection::Missing);
    }
    Ok(Inspection::Legacy(Files {
        digest: digest_of(snapshot.as_deref(), &journal),
        snapshot,
        journal,
    }))
}

fn parse_marker(bytes: &[u8]) -> Result<String> {
    let invalid = || "Invalid job index marker".to_owned();
    if bytes.len() < 48 || bytes.len() > 4096 {
        return Err(invalid());
    }
    let length = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| invalid())?);
    if length.checked_add(48) != Some(bytes.len() as u64)
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err(invalid());
    }
    let value = json::parse(std::str::from_utf8(&bytes[16..bytes.len() - 32]).map_err(|_| invalid())?)?;
    let map = fields(&value)?;
    if string(map, "format")? != "mynou-jobs-index/1" {
        return Err("Unsupported job index marker".into());
    }
    let binding = string(map, "binding")?;
    unhex_digest(&binding).map_err(|_| invalid())?;
    Ok(binding)
}

/// The final state a legacy snapshot and journal describe.
pub(super) struct State {
    pub jobs: BTreeMap<String, Job>,
    pub events: VecDeque<Event>,
    pub sequence: u64,
}

fn known_snapshot(magic: &[u8]) -> bool {
    [
        SNAPSHOT_MAGIC,
        SHARED_SNAPSHOT_MAGIC,
        GROUP_SNAPSHOT_MAGIC,
        REQUESTER_SNAPSHOT_MAGIC,
        IRC_SNAPSHOT_MAGIC,
    ]
    .iter()
    .any(|known| &known[..] == magic)
}

fn known_journal(magic: &[u8]) -> bool {
    [
        JOURNAL_MAGIC,
        SHARED_JOURNAL_MAGIC,
        GROUP_JOURNAL_MAGIC,
        REQUESTER_JOURNAL_MAGIC,
        IRC_JOURNAL_MAGIC,
    ]
    .iter()
    .any(|known| &known[..] == magic)
}

fn push_event(events: &mut VecDeque<Event>, mut event: Event) {
    event.message = super::bounded_message(&event.message);
    if events.len() == MAX_EVENTS {
        events.pop_front();
    }
    events.push_back(event);
}

/// Parses the snapshot and journal tail into the final state. Nothing is
/// validated beyond the file formats; see `validate`.
pub(super) fn parse(snapshot: Option<&[u8]>, journal: &[u8]) -> Result<State> {
    let mut state = State {
        jobs: BTreeMap::new(),
        events: VecDeque::new(),
        sequence: 0,
    };
    let mut chain = [0_u8; 32];
    if let Some(bytes) = snapshot {
        if !(48..=snapshot_limit() as usize).contains(&bytes.len()) {
            return Err("invalid snapshot size".to_owned());
        }
        if !known_snapshot(&bytes[..8]) {
            return Err("invalid snapshot format".to_owned());
        }
        let encoded_length =
            u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| "invalid length")?);
        if encoded_length > MAX_SNAPSHOT as u64
            || encoded_length.checked_add(48) != Some(bytes.len() as u64)
        {
            return Err("corrupt snapshot size".to_owned());
        }
        let payload_length = usize::try_from(encoded_length).map_err(|_| "snapshot too large")?;
        if sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..] {
            return Err("corrupt snapshot".to_owned());
        }
        let value = json::parse(
            std::str::from_utf8(&bytes[16..16 + payload_length])
                .map_err(|_| "snapshot is not UTF-8")?,
        )?;
        let map = fields(&value)?;
        state.sequence = integer(map, "sequence")?;
        chain = unhex_digest(&string(map, "chain")?)?;
        let job_values = match map.get("jobs") {
            Some(Value::Array(values)) => values,
            _ => return Err("invalid snapshot jobs".to_owned()),
        };
        let magic = &bytes[..8];
        for value in job_values {
            let job = Job::from_json(value)?;
            check_snapshot_format(&job, magic)?;
            if state.jobs.insert(job.id.clone(), job).is_some() {
                return Err("duplicate identities in snapshot".to_owned());
            }
        }
        let event_values = match map.get("events") {
            Some(Value::Array(values)) => values,
            _ => return Err("invalid snapshot events".to_owned()),
        };
        let retained = event_values.len() as u64;
        if retained < state.sequence.min(MAX_EVENTS as u64) || retained > state.sequence {
            return Err("missing snapshot events".to_owned());
        }
        let first = state.sequence - retained;
        for (index, value) in event_values.iter().enumerate() {
            let event = Event::from_json(value)?;
            if event.id != first + index as u64 + 1
                || !state.jobs.contains_key(&event.job_id)
                || event.id > state.sequence
            {
                return Err("invalid event sequence".to_owned());
            }
            push_event(&mut state.events, event);
        }
    }
    replay(&mut state, journal, chain)?;
    Ok(state)
}

fn check_snapshot_format(job: &Job, magic: &[u8]) -> Result<()> {
    if job.irc_origin.is_some() && magic != IRC_SNAPSHOT_MAGIC {
        return Err("IRC provenance requires snapshot format 5".into());
    }
    if job.requester.is_some() && magic != REQUESTER_SNAPSHOT_MAGIC && magic != IRC_SNAPSHOT_MAGIC {
        return Err("Requester provenance requires snapshot format 4".into());
    }
    if group_format(job)
        && magic != GROUP_SNAPSHOT_MAGIC
        && magic != REQUESTER_SNAPSHOT_MAGIC
        && magic != IRC_SNAPSHOT_MAGIC
    {
        return Err("Shared-group upgrades require snapshot format 3".into());
    }
    if job.shared_file.is_some() && magic == SNAPSHOT_MAGIC {
        return Err("Shared ownership requires snapshot format 2".into());
    }
    Ok(())
}

fn replay(state: &mut State, journal: &[u8], snapshot_chain: [u8; 32]) -> Result<()> {
    let snapshot_sequence = state.sequence;
    let length = journal.len() as u64;
    let mut offset = 0_u64;
    let mut previous: Option<(u64, [u8; 32])> = None;
    while offset < length {
        let remaining = length - offset;
        // An incomplete final record is an interrupted write: it never
        // acknowledged anything and is not part of the state.
        if remaining < 84 {
            break;
        }
        let start = offset as usize;
        let header = &journal[start..start + 84];
        if !known_journal(&header[..8]) {
            return Err(format!("corrupt journal at byte {offset}"));
        }
        if sha256(&header[..52]).as_slice() != &header[52..84] {
            return Err(format!("corrupt journal header at byte {offset}"));
        }
        let sequence = u64::from_le_bytes(header[8..16].try_into().map_err(|_| "invalid sequence")?);
        let payload_length =
            u32::from_le_bytes(header[16..20].try_into().map_err(|_| "invalid length")?) as usize;
        let chain: [u8; 32] = header[20..52].try_into().map_err(|_| "invalid chain")?;
        if payload_length > MAX_RECORD {
            return Err("journal transaction too large".to_owned());
        }
        let frame_length = 116 + payload_length as u64;
        if remaining < frame_length {
            break;
        }
        let frame = &journal[start..start + 84 + payload_length];
        let digest_bytes = &journal[start + 84 + payload_length..start + frame_length as usize];
        let digest: [u8; 32] = digest_bytes.try_into().map_err(|_| "invalid digest")?;
        if sha256(frame) != digest {
            return Err(format!("corrupt complete transaction at byte {offset}"));
        }
        if let Some((previous_sequence, previous_chain)) = previous {
            if previous_sequence.checked_add(1) != Some(sequence) || chain != previous_chain {
                return Err("invalid journal chain".to_owned());
            }
        } else if snapshot_sequence.checked_add(1) == Some(sequence) {
            if chain != snapshot_chain {
                return Err("journal chain differs from snapshot".to_owned());
            }
        } else if sequence == 0 || sequence > snapshot_sequence {
            return Err("invalid initial journal sequence".to_owned());
        }
        if sequence == snapshot_sequence && digest != snapshot_chain {
            return Err("journal differs from acknowledged snapshot".to_owned());
        }
        if sequence > snapshot_sequence {
            apply_record(state, &header[..8], &frame[84..], sequence)?;
            state.sequence = sequence;
        }
        previous = Some((sequence, digest));
        offset += frame_length;
    }
    if let Some((sequence, _)) = previous
        && sequence < snapshot_sequence
    {
        return Err("incomplete stale journal preceding snapshot".to_owned());
    }
    Ok(())
}

fn apply_record(state: &mut State, magic: &[u8], payload: &[u8], sequence: u64) -> Result<()> {
    let value = json::parse(std::str::from_utf8(payload).map_err(|_| "journal is not UTF-8")?)?;
    let map = fields(&value)?;
    let job = Job::from_json(super::field(map, "job")?)?;
    let requester_ok = magic == REQUESTER_JOURNAL_MAGIC || magic == IRC_JOURNAL_MAGIC;
    if job.irc_origin.is_some() && magic != IRC_JOURNAL_MAGIC {
        return Err("IRC provenance requires journal format 5".into());
    }
    if job.requester.is_some() && !requester_ok {
        return Err("Requester provenance requires journal format 4".into());
    }
    if group_format(&job) && magic != GROUP_JOURNAL_MAGIC && !requester_ok {
        return Err("Shared-group upgrades require journal format 3".into());
    }
    if job.shared_file.is_some() && magic == JOURNAL_MAGIC {
        return Err("Shared ownership requires journal format 2".into());
    }
    let event = Event::from_json(super::field(map, "event")?)?;
    if event.id != sequence
        || event.job_id != job.id
        || event.state != job.state
        || event.at != job.updated_at
    {
        return Err("inconsistent journal transaction".to_owned());
    }
    let mut jobs = vec![job];
    if map.contains_key("group_jobs") && !map.contains_key("group_action") {
        return Err("Group owners require a journal action".into());
    }
    let mut owners = None;
    if let Some(action) = map.get("group_action") {
        if (magic != GROUP_JOURNAL_MAGIC && !requester_ok) || map.contains_key("shared_owners") {
            return Err("Group action requires journal format 3".into());
        }
        super::groups::GroupAction::parse(action.as_str().ok_or("Invalid group action")?)?;
        owners = Some(
            map.get("group_jobs")
                .and_then(Value::as_array)
                .ok_or("Missing group transaction owners")?,
        );
    } else if let Some(shared) = map.get("shared_owners") {
        if magic != SHARED_JOURNAL_MAGIC && !requester_ok {
            return Err("Shared ownership requires journal format 2".into());
        }
        owners = Some(shared.as_array().ok_or("Invalid shared ownership journal array")?);
    } else if map.contains_key("group_jobs") {
        return Err("Group owners require a journal action".into());
    }
    if let Some(owners) = owners {
        if owners.is_empty() || owners.len() >= crate::pack::MAX_PACK_EPISODES {
            return Err("Invalid shared-group journal size".into());
        }
        for owner in owners {
            jobs.push(Job::from_json(owner)?);
        }
    }
    for job in jobs {
        state.jobs.insert(job.id.clone(), job);
    }
    push_event(&mut state.events, event);
    Ok(())
}

/// Checks the invariants the whole journal enforced, over the final state. An
/// invalid legacy store is refused and nothing is written.
pub(super) fn validate(state: &State) -> Result<()> {
    let mut keys = BTreeSet::new();
    for job in state.jobs.values() {
        if job.id.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err("Job identifiers must be lowercase hexadecimal".into());
        }
        if !keys.insert(job.key.as_str()) {
            return Err("duplicate request identity".to_owned());
        }
    }
    let roots = roots(&state.jobs)?;
    validate_shared_groups(&state.jobs, &roots)?;
    let mut pending = BTreeSet::new();
    let mut ready_parents = BTreeSet::new();
    let mut lookup = Lookup(&state.jobs);
    for job in state.jobs.values() {
        validate_lineage(&mut lookup, job)?;
        check_irc_ownership(&state.jobs, job)?;
        if super::layout::pending_upgrade(job) && !pending.insert(job.request.media_key()) {
            return Err("multiple pending upgrades in snapshot".to_owned());
        }
        if job.state == "ready"
            && let Some(parent) = &job.upgrade_parent
            && !ready_parents.insert(parent)
        {
            return Err("multiple ready children of the same upgrade parent".to_owned());
        }
    }
    let tips: BTreeSet<String> = library(&state.jobs, &roots)
        .into_iter()
        .map(|job| job.id.clone())
        .collect();
    for job in state.jobs.values().filter(|job| super::layout::pending_upgrade(job)) {
        if job
            .upgrade_parent
            .as_ref()
            .is_none_or(|parent| !tips.contains(parent))
        {
            return Err("pending upgrade has an obsolete parent in snapshot".to_owned());
        }
    }
    for event in &state.events {
        if !state.jobs.contains_key(&event.job_id) {
            return Err("invalid event sequence".to_owned());
        }
    }
    Ok(())
}

pub(super) struct Lookup<'a>(pub &'a BTreeMap<String, Job>);

impl super::JobLookup for Lookup<'_> {
    fn lookup(&mut self, id: &str) -> Result<Option<Job>> {
        Ok(self.0.get(id).cloned())
    }
}

use super::validate_lineage;

/// The first ancestor of every job, by an iterative walk that detects cycles.
pub(super) fn roots(jobs: &BTreeMap<String, Job>) -> Result<BTreeMap<String, String>> {
    let mut roots: BTreeMap<String, String> = BTreeMap::new();
    for job in jobs.values() {
        let mut path = Vec::new();
        let mut seen = BTreeSet::new();
        let mut cursor = job;
        let root = loop {
            if let Some(root) = roots.get(&cursor.id) {
                break root.clone();
            }
            if !seen.insert(cursor.id.as_str()) {
                return Err("cycle in upgrade lineage".into());
            }
            path.push(cursor.id.clone());
            match cursor.upgrade_parent.as_deref() {
                Some(id) => {
                    cursor = jobs
                        .get(id)
                        .ok_or("missing upgrade lineage reference")?
                }
                None => break cursor.id.clone(),
            }
        };
        for id in path {
            roots.insert(id, root.clone());
        }
    }
    Ok(roots)
}

/// Current library versions: a ready import that no ready child replaced, one
/// per media identity, represented by the newest lineage root.
pub(super) fn library<'a>(
    jobs: &'a BTreeMap<String, Job>,
    roots: &BTreeMap<String, String>,
) -> Vec<&'a Job> {
    let replaced: BTreeSet<&str> = jobs
        .values()
        .filter(|job| super::layout::imported(job))
        .filter_map(|job| job.upgrade_parent.as_deref())
        .collect();
    let mut current: BTreeMap<String, (&Job, &Job)> = BTreeMap::new();
    for job in jobs
        .values()
        .filter(|job| super::layout::imported(job) && !replaced.contains(job.id.as_str()))
    {
        let key = job.request.media_key();
        let root = roots
            .get(&job.id)
            .and_then(|id| jobs.get(id))
            .unwrap_or(job);
        match current.get(&key) {
            Some((previous, _))
                if (previous.created_at, &previous.id) >= (root.created_at, &root.id) => {}
            _ => {
                current.insert(key, (root, job));
            }
        }
    }
    let mut tips: Vec<_> = current.into_values().map(|(_, job)| job).collect();
    tips.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    tips
}

fn validate_shared_groups(
    jobs: &BTreeMap<String, Job>,
    roots: &BTreeMap<String, String>,
) -> Result<()> {
    use super::groups::GroupState;
    let mut groups: BTreeMap<String, GroupState<'_>> = BTreeMap::new();
    let mut physical = BTreeMap::new();
    let mut logical = BTreeMap::new();
    let mut lookup = Lookup(jobs);
    for job in jobs.values() {
        if let Some(file) = &job.shared_file {
            file.validate_job(job)?;
            let group = groups.entry(file.id()).or_insert_with(|| GroupState {
                file,
                owners: BTreeSet::new(),
                first: job,
                ready: 0,
                cancelled: 0,
            });
            let root = roots.get(&job.id).ok_or("Missing shared lineage root")?;
            let previous = logical.insert(job.request.media_key(), root.as_str());
            if group.file != file
                || !group.owners.insert(job.request.episode)
                || group.first.release != job.release
                || group.first.shared_upgrade != job.shared_upgrade
                || group.first.request.source_url != job.request.source_url
                || previous.is_some_and(|id| id != root)
            {
                return Err("Duplicate or conflicting shared owner in persistent storage".into());
            }
            if job.shared_upgrade.is_some() {
                super::groups::validate_group_lineage(&mut lookup, job)?;
            }
            group.ready += usize::from(job.state == "ready");
            group.cancelled += usize::from(job.state == "cancelled");
            let key = (&file.torrent_id, &file.file_path);
            if physical
                .insert(key, file)
                .is_some_and(|known| known != file)
            {
                return Err("Conflicting physical shared ownership in storage".into());
            }
        }
    }
    for (_, group) in groups {
        if group.first.shared_upgrade.is_some()
            && ((group.ready > 0 && group.ready != group.owners.len())
                || (group.cancelled > 0 && group.cancelled != group.owners.len()))
        {
            return Err("Partially promoted or cancelled shared replacement in storage".into());
        }
        if group
            .owners
            .into_iter()
            .ne(group.file.first_episode..=group.file.last_episode)
        {
            return Err("Incomplete shared ownership in persistent storage".into());
        }
    }
    if jobs
        .values()
        .any(|job| job.shared_file.is_none() && logical.contains_key(&job.request.media_key()))
    {
        return Err("Unrelated request collides with shared ownership in storage".into());
    }
    Ok(())
}

/// Ownership of one physical video by an IRC acquisition, a pack mapping or a
/// shared file, checked pairwise as the journal did.
fn check_irc_ownership(jobs: &BTreeMap<String, Job>, job: &Job) -> Result<()> {
    if job.irc_origin.is_none() && job.pack_file.is_none() {
        return Ok(());
    }
    for other in jobs.values().filter(|j| j.id != job.id) {
        if job.irc_origin.is_none() {
            if let Some(known) = &other.irc_origin
                && job.pack_file.as_ref() == Some(&known.file)
                && (job
                    .pack_origin
                    .as_ref()
                    .is_some_and(|p| known.torrent_aliases.contains(&p.torrent_id))
                    || job
                        .shared_file
                        .as_ref()
                        .is_some_and(|f| known.torrent_aliases.contains(&f.torrent_id))
                    || job
                        .download_id
                        .as_ref()
                        .is_some_and(|id| known.torrent_aliases.contains(id)))
            {
                return Err(
                    "IRC: pack mapping conflicts with retained announcement ownership".into(),
                );
            }
            continue;
        }
        let origin = job
            .irc_origin
            .as_ref()
            .ok_or("IRC: missing ownership origin")?;
        if let Some(known) = &other.irc_origin
            && known.file == origin.file
            && known
                .torrent_aliases
                .iter()
                .any(|id| origin.torrent_aliases.contains(id))
        {
            return Err("IRC: physical video already belongs to another acquisition".into());
        }
        if other.pack_file.as_ref() == Some(&origin.file)
            && (other
                .pack_origin
                .as_ref()
                .is_some_and(|p| origin.torrent_aliases.contains(&p.torrent_id))
                || other
                    .shared_file
                    .as_ref()
                    .is_some_and(|f| origin.torrent_aliases.contains(&f.torrent_id))
                || other
                    .download_id
                    .as_ref()
                    .is_some_and(|id| origin.torrent_aliases.contains(id)))
        {
            return Err("IRC: candidate conflicts with retained pack ownership".into());
        }
    }
    Ok(())
}

/// Frames in the old on-disk formats, for migration fixtures.
#[cfg(test)]
pub(super) mod fixture {
    use super::*;

    pub fn magics(jobs: &[Job], group: bool) -> (&'static [u8; 8], &'static [u8; 8]) {
        if jobs.iter().any(|j| j.irc_origin.is_some()) {
            (IRC_JOURNAL_MAGIC, IRC_SNAPSHOT_MAGIC)
        } else if jobs.iter().any(|j| j.requester.is_some()) {
            (REQUESTER_JOURNAL_MAGIC, REQUESTER_SNAPSHOT_MAGIC)
        } else if group || jobs.iter().any(group_format) {
            (GROUP_JOURNAL_MAGIC, GROUP_SNAPSHOT_MAGIC)
        } else if jobs.iter().any(|j| j.shared_file.is_some()) {
            (SHARED_JOURNAL_MAGIC, SHARED_SNAPSHOT_MAGIC)
        } else {
            (JOURNAL_MAGIC, SNAPSHOT_MAGIC)
        }
    }

    pub fn snapshot(
        jobs: &[Job],
        events: &[Event],
        sequence: u64,
        chain: [u8; 32],
        magic: &[u8; 8],
    ) -> Vec<u8> {
        let mut value = Value::object();
        value.insert("sequence", Value::String(sequence.to_string()));
        value.insert("chain", hex(&chain));
        value.insert(
            "jobs",
            Value::Array(jobs.iter().map(Job::to_json).collect()),
        );
        value.insert(
            "events",
            Value::Array(events.iter().map(Event::to_json).collect()),
        );
        let payload = json::stringify(&value).into_bytes();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(magic);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        let digest = sha256(&bytes);
        bytes.extend_from_slice(&digest);
        bytes
    }

    /// One journal frame and its digest (the next chain value).
    pub fn frame(
        jobs: &[Job],
        event: &Event,
        sequence: u64,
        chain: &[u8; 32],
        magic: &[u8; 8],
        action: Option<&str>,
    ) -> (Vec<u8>, [u8; 32]) {
        let mut value = Value::object();
        value.insert("job", jobs[0].to_json());
        value.insert("event", event.to_json());
        if let Some(action) = action {
            value.insert("group_action", action);
        }
        if jobs.len() > 1 {
            value.insert(
                if action.is_some() {
                    "group_jobs"
                } else {
                    "shared_owners"
                },
                Value::Array(jobs.iter().skip(1).map(Job::to_json).collect()),
            );
        }
        let payload = json::stringify(&value).into_bytes();
        let mut frame = Vec::new();
        frame.extend_from_slice(magic);
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(chain);
        frame.extend_from_slice(&sha256(&frame));
        frame.extend_from_slice(&payload);
        let digest = sha256(&frame);
        frame.extend_from_slice(&digest);
        (frame, digest)
    }
}
