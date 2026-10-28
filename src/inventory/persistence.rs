use super::{
    Index, Key, MAX_CHANGES, MAX_VALUE_BYTES,
    checksum::xxh64,
    tree::{self, Header, MAX_DIRTY_PAGES, Node, PAGE_SIZE, Tree, ValueRef},
};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    store::{
        STORAGE_FULL, before_private_open, io_text, private_options, reject_symlinks,
        sync_directory, sync_file, try_lock_file,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(super) const MAX_WAL_BYTES: usize = 128 * 1024 * 1024;
/// Error of a change whose WAL, with its tree pages, exceeds `MAX_WAL_BYTES`.
pub(super) const WAL_BUDGET: &str = "Inventory transaction exceeds its WAL byte budget";
const WAL_MAGIC: &[u8; 8] = b"MYNOUIW1";
const VALUE_MAGIC: &[u8; 8] = b"MYNOUIV1";
const FAST_VALUE_MAGIC: &[u8; 8] = b"MYNOUIV2";
const ID_MAGIC: &[u8; 8] = b"MYNOUID1";
const SEGMENT_MAGIC: &[u8; 8] = b"MYNOUIS1";
/// Magic, store identity, segment number and their SHA-256.
pub(super) const SEGMENT_HEADER_BYTES: u64 = 80;
/// Magic, key and length before a value, its 32-byte checksum field after.
pub(super) const ENVELOPE_BYTES: u64 = 108;
/// Segment `head` takes appends until it reaches this size.
pub(super) const SEGMENT_TARGET: u64 = 64 * 1024 * 1024;

const REDO: &str = "redo.bin";
const SLOT_MAGIC: &[u8; 8] = b"MYNOUIJ1";
const RECORD_MAGIC: &[u8; 8] = b"MYNOUIR1";
/// Two alternating checkpoint slots precede the records.
const SLOT_BYTES: u64 = 8192;
const RECORDS: u64 = 2 * SLOT_BYTES;
/// Records start on this boundary, so a damaged record never hides a later
/// valid one from the check that follows the last record.
const RECORD_ALIGN: u64 = 4096;
/// Magic, generation and body length before a record body, XXH64 after.
const RECORD_FRAMING: u64 = 32;
/// Redo bytes after which the next commit checkpoints first.
pub(super) const CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct Action {
    pub key: Key,
    pub data: Option<Vec<u8>>,
    /// Inline values live in the tree leaf and have no record file.
    pub inline: bool,
    pub existed: bool,
    /// Segment number and offset of a put written to the value log. Puts
    /// without it come only from WALs written before the log existed.
    pub segment: Option<(u64, u32)>,
    /// The key may have a legacy record file to inspect and remove. Staging
    /// knows from the replaced reference; a recovered WAL assumes it may.
    pub replaces_file: bool,
    /// The envelope carries XXH64. Only WALs from earlier releases have
    /// SHA-256 envelopes.
    pub fast: bool,
}

impl Action {
    /// Only WALs from before the value log carry record-file puts.
    fn writes_file(&self) -> bool {
        self.data.is_some() && !self.inline && self.segment.is_none()
    }
    pub(super) fn reference(&self) -> Result<Option<ValueRef>> {
        self.data
            .as_ref()
            .map(|data| {
                if self.inline {
                    ValueRef::inline(data)
                } else if let Some((number, offset)) = self.segment {
                    Ok(if self.fast {
                        ValueRef::logged(
                            number,
                            offset,
                            data.len() as u32,
                            fast_envelope_checksum(&self.key, data),
                        )
                    } else {
                        ValueRef::segment(number, offset, data)
                    })
                } else {
                    Ok(ValueRef::file(data))
                }
            })
            .transpose()
    }
    fn envelope(&self, data: &[u8]) -> Vec<u8> {
        if self.fast {
            fast_envelope(&self.key, data)
        } else {
            envelope(&self.key, data)
        }
    }
}
pub(super) struct Transaction {
    pub before: Header,
    pub after: Header,
    pub pages: BTreeMap<u64, Node>,
    pub actions: Vec<Action>,
}

/// Which writer produced a transaction body: the per-commit WAL of earlier
/// releases, or a record of the redo log.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Wal,
    Redo,
}

#[cfg(test)]
thread_local! { pub(crate) static SEGMENT_TARGET_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) }; }

/// Segment and offset where a commit's first new envelope goes: the end of
/// segment `head` while it is below the target and not `sealed`, else a new
/// segment, whose header the commit writes first (offset zero).
pub(super) fn log_target(header: &Header, sealed: bool) -> Result<(u64, u64)> {
    #[cfg(test)]
    let target = SEGMENT_TARGET_OVERRIDE
        .with(|value| value.get())
        .unwrap_or(SEGMENT_TARGET);
    #[cfg(not(test))]
    let target = SEGMENT_TARGET;
    if header.head_length == 0 {
        Ok((header.head, 0))
    } else if !sealed && header.head_length < target {
        Ok((header.head, header.head_length))
    } else {
        Ok((
            header
                .head
                .checked_add(1)
                .ok_or("Inventory segment number overflow")?,
            0,
        ))
    }
}

impl Transaction {
    /// The body shared by WAL files and redo records, without framing.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let length = self.actions.iter().try_fold(
            2 * PAGE_SIZE + 8 + self.pages.len() * (PAGE_SIZE + 8),
            |length, action| {
                length
                    .checked_add(
                        70 + action.data.as_ref().map_or(0, Vec::len)
                            + if action.segment.is_some() && !action.inline {
                                12
                            } else {
                                0
                            },
                    )
                    .ok_or("Inventory WAL length overflow")
            },
        )?;
        if length > MAX_WAL_BYTES {
            return Err(WAL_BUDGET.into());
        }
        let mut bytes = Vec::with_capacity(length + 48);
        bytes.extend_from_slice(&self.before.encode());
        bytes.extend_from_slice(&self.after.encode());
        bytes.extend_from_slice(&(self.pages.len() as u32).to_le_bytes());
        for (id, node) in &self.pages {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&node.encode()?);
        }
        bytes.extend_from_slice(&(self.actions.len() as u32).to_le_bytes());
        for action in &self.actions {
            bytes.extend_from_slice(&action.key);
            bytes.push(match (&action.data, action.inline, action.segment) {
                (None, _, _) => 0,
                (Some(_), true, _) => 2,
                (Some(_), false, None) => 1,
                (Some(_), false, Some(_)) => 3,
            });
            bytes.push(u8::from(action.existed) | u8::from(action.replaces_file) << 1);
            bytes.extend_from_slice(
                &(action.data.as_ref().map_or(0, Vec::len) as u32).to_le_bytes(),
            );
            if let Some((number, offset)) = action.segment.filter(|_| !action.inline) {
                bytes.extend_from_slice(&number.to_le_bytes());
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
            if let Some(data) = &action.data {
                bytes.extend_from_slice(data);
            }
        }
        Ok(bytes)
    }

    /// A per-commit WAL written by an earlier release.
    fn decode_wal(bytes: &[u8], identity: [u8; 32]) -> Result<Self> {
        if bytes.len() < 48 || bytes.len() > MAX_WAL_BYTES + 48 || &bytes[..8] != WAL_MAGIC {
            return Err("Invalid published inventory WAL format or length".into());
        }
        let length = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| "Invalid WAL length")?);
        if length.checked_add(48) != Some(bytes.len() as u64)
            || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
        {
            return Err("Corrupt or truncated published inventory WAL".into());
        }
        Self::decode(&bytes[16..bytes.len() - 32], identity, Origin::Wal)
    }

    fn decode(body: &[u8], identity: [u8; 32], origin: Origin) -> Result<Self> {
        let mut reader = Reader {
            bytes: body,
            position: 0,
        };
        let before = Header::decode(reader.take(PAGE_SIZE)?)?;
        let after = Header::decode(reader.take(PAGE_SIZE)?)?;
        if before.identity != identity
            || after.identity != identity
            || before.sequence.checked_add(1) != Some(after.sequence)
            || after.next < before.next
            || after.tail < before.tail
            // A commit may seal and clean the segment that took appends.
            || after.tail > before.head + u64::from(before.head_length > 0)
            || after.tail.saturating_sub(before.tail) > super::MAX_RETIRED_SEGMENTS
            || !(after.head == before.head || before.head.checked_add(1) == Some(after.head))
        {
            return Err("Inventory WAL identity, sequence or allocation mismatch".into());
        }
        let count = reader.u32()? as usize;
        if count > MAX_DIRTY_PAGES {
            return Err("Too many inventory WAL pages".into());
        }
        let mut pages = BTreeMap::new();
        for _ in 0..count {
            let id = reader.u64()?;
            let page = reader.take(PAGE_SIZE)?;
            if id == 0 || id >= after.next || page[16] > 63 {
                return Err("Invalid inventory WAL page reference".into());
            }
            let node = Node::decode(page, id, page[16], after.next, identity)?;
            if pages.insert(id, node).is_some() {
                return Err("Duplicate inventory WAL page".into());
            }
        }
        let count = reader.u32()? as usize;
        if count > MAX_CHANGES {
            return Err("Invalid inventory WAL action count".into());
        }
        // Range-count upgrades only replace branch metadata. They leave the
        // root, allocation, records and value log unchanged, and advance the
        // sequence once like every other recoverable transaction.
        if count == 0 {
            let mut unchanged = after;
            unchanged.sequence = before.sequence;
            if unchanged != before || pages.is_empty() || pages.values().any(|node| node.level == 0)
            {
                return Err("Invalid inventory metadata-only WAL".into());
            }
        }
        // A WAL wrote each commit's values to a new segment `head`. A redo
        // record either appends to segment `head` or starts the next one;
        // its first envelope says which.
        let mut target: Option<(u64, u64)> =
            (origin == Origin::Wal).then_some((before.head, SEGMENT_HEADER_BYTES));
        let fresh = before.head + u64::from(before.head_length > 0);
        let mut actions = Vec::new();
        let mut keys = BTreeSet::new();
        let mut expected = before.count;
        let mut segment_end = 0;
        for _ in 0..count {
            let key: Key = reader.take(64)?.try_into().map_err(|_| "Invalid WAL key")?;
            if !keys.insert(key) {
                return Err("Duplicate inventory WAL action".into());
            }
            let put = reader.byte()?;
            let flags = reader.byte()?;
            let length = reader.u32()? as usize;
            if put > 3
                || flags > if origin == Origin::Redo { 3 } else { 1 }
                || length
                    > if put == 2 {
                        tree::MAX_INLINE_BYTES
                    } else {
                        MAX_VALUE_BYTES
                    }
                || (put == 0 && length != 0)
            {
                return Err("Invalid inventory WAL value or action".into());
            }
            let segment = if put == 3 {
                let segment = reader.u64()?;
                let offset = u64::from(reader.u32()?);
                let (number, first) = *target.get_or_insert((segment, offset));
                if segment_end == 0 {
                    segment_end = first;
                }
                let placed = match origin {
                    Origin::Wal => before.head.checked_add(1) == Some(after.head),
                    Origin::Redo => {
                        (number == before.head
                            && before.head_length > 0
                            && first == before.head_length)
                            || (number == fresh && first == SEGMENT_HEADER_BYTES)
                    }
                };
                if !placed || segment != number || offset != segment_end {
                    return Err("Inventory WAL segment location mismatch".into());
                }
                segment_end = segment_end
                    .checked_add(ENVELOPE_BYTES + length as u64)
                    .filter(|end| *end <= u64::from(u32::MAX))
                    .ok_or("Inventory WAL segment overflow")?;
                Some((segment, offset as u32))
            } else {
                None
            };
            let data = if put != 0 {
                Some(reader.take(length)?.to_vec())
            } else {
                None
            };
            let existed = flags & 1 == 1;
            expected = match (put != 0, existed) {
                (true, false) => expected
                    .checked_add(1)
                    .ok_or("Inventory WAL count overflow")?,
                (false, true) => expected
                    .checked_sub(1)
                    .ok_or("Inventory WAL count underflow")?,
                _ => expected,
            };
            actions.push(Action {
                key,
                data,
                inline: put == 2,
                existed,
                segment,
                replaces_file: origin == Origin::Wal || flags & 2 == 2,
                fast: origin == Origin::Redo,
            });
        }
        let log_agrees = match (origin, target) {
            (Origin::Wal, _) => (segment_end != 0) == (after.head != before.head),
            (Origin::Redo, Some((number, _))) => {
                after.head == number && after.head_length == segment_end
            }
            // Nothing appended: segment `head` is unchanged, or sealed.
            (Origin::Redo, None) => {
                (after.head == before.head && after.head_length == before.head_length)
                    || (after.head == fresh && after.head_length == 0 && fresh != before.head)
            }
        };
        if !log_agrees {
            return Err("Inventory WAL segment and value log disagree".into());
        }
        if expected != after.count || reader.position != reader.bytes.len() {
            return Err("Inventory WAL count or trailing-data mismatch".into());
        }
        Ok(Self {
            before,
            after,
            pages,
            actions,
        })
    }

    fn validate_overlay(&self, file: &mut File) -> Result<()> {
        let bytes = tree::read_page(file, 0)?;
        if let Ok(actual) = Header::decode(&bytes)
            && actual != self.before
            && actual != self.after
        {
            return Err("Stale inventory WAL disagrees with the live root".into());
        }
        let length = file.metadata().map_err(|error| io_text(&error))?.len();
        if length < tree::offset(self.before.next)? || length > tree::offset(self.after.next)? {
            return Err("Inventory file length disagrees with recovery".into());
        }
        let mut cache = tree::Cache::new();
        let mut overlay = Tree::new(file, self.after, &mut cache);
        overlay.dirty = self.pages.clone();
        if self.actions.is_empty() {
            overlay.validate_count_upgrade()?;
        }
        for action in &self.actions {
            if overlay.find(&action.key)? != action.reference()? {
                return Err("Inventory WAL records disagree with its tree pages".into());
            }
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("Truncated inventory WAL payload")?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| "Invalid WAL integer")?,
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| "Invalid WAL integer")?,
        ))
    }
}

/// The redo log: the durable checkpoint and the records committed after it.
/// Tree pages and segments written since the checkpoint are not yet flushed;
/// the records can rebuild them.
pub(super) struct Redo {
    file: File,
    generation: u64,
    /// The root as of the last checkpoint, which the files fully contain.
    slot: Header,
    /// Where the next record starts.
    end: u64,
    /// Segments opened for writing; the active one stays open.
    writers: BTreeMap<u64, File>,
    /// Segments written since the checkpoint.
    dirty: BTreeSet<u64>,
    /// Directories whose entries changed since the checkpoint.
    directories: BTreeSet<PathBuf>,
}

impl Redo {
    fn new(file: File, generation: u64, slot: Header) -> Self {
        Self {
            file,
            generation,
            slot,
            end: RECORDS,
            writers: BTreeMap::new(),
            dirty: BTreeSet::new(),
            directories: BTreeSet::new(),
        }
    }

    /// True when records await a checkpoint.
    pub(super) fn pending(&self) -> bool {
        self.end > RECORDS
    }

    /// The segment file to write, created when missing. A segment written
    /// from its start is emptied first: anything already there came from a
    /// change that never became durable, or is rebuilt by the same replay.
    fn segment(&mut self, directory: &Path, number: u64, fresh: bool) -> Result<&File> {
        if !self.writers.contains_key(&number) {
            let segments = directory.join("segments");
            if !present(&segments)? {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&segments)
                    .map_err(|error| {
                        format!("Cannot create inventory segments: {}", io_text(&error))
                    })?;
                self.directories.insert(directory.to_owned());
            }
            private_directory(&segments)?;
            let path = segment_path(directory, number);
            let file = if present(&path)? {
                private_file(&path, true)?
            } else {
                self.directories.insert(segments);
                new_file(&path)?
            };
            self.writers.insert(number, file);
        }
        let file = self
            .writers
            .get(&number)
            .ok_or("Inventory segment writer lost an entry")?;
        if fresh && file.metadata().map_err(|error| io_text(&error))?.len() > 0 {
            file.set_len(0).map_err(|error| io_text(&error))?;
        }
        self.dirty.insert(number);
        Ok(file)
    }
}

pub(super) const CREATION_REDO_BYTES: usize = SLOT_BYTES as usize + 24 + PAGE_SIZE + 8;

/// The redo file a new store starts with: an empty first slot and the
/// creation checkpoint in the second.
pub(super) fn creation_redo(identity: [u8; 32]) -> Vec<u8> {
    let mut bytes = vec![0; SLOT_BYTES as usize];
    bytes.extend_from_slice(&slot_bytes(1, 0, &Header::empty(identity)));
    bytes
}

fn slot_bytes(generation: u64, retired_from: u64, header: &Header) -> Vec<u8> {
    let mut bytes = Vec::from(SLOT_MAGIC.as_slice());
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&retired_from.to_le_bytes());
    bytes.extend_from_slice(&header.encode());
    bytes.extend_from_slice(&xxh64(&bytes).to_le_bytes());
    bytes
}

/// The newest valid checkpoint slot: generation, the tail of the checkpoint
/// before it (whose retired segments may remain) and the root.
fn read_slots(file: &File, identity: [u8; 32]) -> Result<(u64, u64, Header)> {
    let length = 24 + PAGE_SIZE + 8;
    let mut best: Option<(u64, u64, Header)> = None;
    for slot in 0..2 {
        let mut bytes = vec![0; length];
        if file.read_exact_at(&mut bytes, slot * SLOT_BYTES).is_err() {
            continue;
        }
        let body = length - 8;
        if &bytes[..8] != SLOT_MAGIC || xxh64(&bytes[..body]).to_le_bytes() != bytes[body..] {
            continue;
        }
        let generation = u64::from_le_bytes(bytes[8..16].try_into().unwrap_or_default());
        let retired = u64::from_le_bytes(bytes[16..24].try_into().unwrap_or_default());
        let header = Header::decode(&bytes[24..body])?;
        if generation % 2 != slot || header.identity != identity || retired > header.tail {
            return Err("Inventory redo checkpoint names another slot or store".into());
        }
        if best.is_none_or(|(newest, _, _)| generation > newest) {
            best = Some((generation, retired, header));
        }
    }
    best.ok_or_else(|| "Inventory redo log has no valid checkpoint".into())
}

fn record_bytes(generation: u64, body: &[u8]) -> Vec<u8> {
    let framed = RECORD_FRAMING as usize + body.len();
    let mut bytes = Vec::with_capacity(framed.next_multiple_of(RECORD_ALIGN as usize));
    bytes.extend_from_slice(RECORD_MAGIC);
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(&xxh64(&bytes).to_le_bytes());
    bytes.resize(framed.next_multiple_of(RECORD_ALIGN as usize), 0);
    bytes
}

/// The intact record at `offset`, if one of this generation starts there,
/// and its aligned size. A record whose checksum holds but whose content is
/// invalid is corruption, not an interrupted write.
fn read_record(
    file: &File,
    offset: u64,
    length: u64,
    generation: u64,
    identity: [u8; 32],
) -> Result<Option<(Transaction, u64)>> {
    let mut head = [0; 24];
    if offset + 24 > length || file.read_exact_at(&mut head, offset).is_err() {
        return Ok(None);
    }
    let body = u64::from_le_bytes(head[16..24].try_into().unwrap_or_default());
    if &head[..8] != RECORD_MAGIC
        || head[8..16] != generation.to_le_bytes()
        || body > MAX_WAL_BYTES as u64
        || offset + RECORD_FRAMING + body > length
    {
        return Ok(None);
    }
    let mut bytes = vec![0; (RECORD_FRAMING + body) as usize];
    if file.read_exact_at(&mut bytes, offset).is_err() {
        return Ok(None);
    }
    let end = bytes.len() - 8;
    if xxh64(&bytes[..end]).to_le_bytes() != bytes[end..] {
        return Ok(None);
    }
    let transaction = Transaction::decode(&bytes[24..end], identity, Origin::Redo)
        .map_err(|error| format!("Inventory redo record is invalid: {error}"))?;
    Ok(Some((
        transaction,
        (RECORD_FRAMING + body).next_multiple_of(RECORD_ALIGN),
    )))
}

/// Every record after the checkpoint, in order, and whether bytes of this
/// generation follow the last one (an interrupted write).
fn read_records(file: &File, generation: u64, slot: Header) -> Result<(Vec<Transaction>, bool)> {
    let length = file.metadata().map_err(|error| io_text(&error))?.len();
    let identity = slot.identity;
    let mut records = Vec::new();
    let mut state = slot;
    let mut offset = RECORDS;
    while let Some((transaction, size)) = read_record(file, offset, length, generation, identity)? {
        if transaction.before != state {
            return Err("Inventory redo record does not follow its predecessor".into());
        }
        state = transaction.after;
        records.push(transaction);
        offset += size;
    }
    let mut head = [0; 16];
    let torn = file.read_exact_at(&mut head, offset).is_ok()
        && &head[..8] == RECORD_MAGIC
        && head[8..] == generation.to_le_bytes();
    // A record is written only after the previous one is durable, so an
    // intact record past the first unreadable one means damage, not a crash.
    let mut probe = offset + RECORD_ALIGN;
    while probe + 24 <= length {
        if read_record(file, probe, length, generation, identity)?.is_some() {
            return Err("Inventory redo log is damaged before its last record".into());
        }
        probe += RECORD_ALIGN;
    }
    Ok((records, torn))
}

fn private_directory(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect inventory directory: {}", io_text(&error)))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("Inventory directory must be private and regular".into());
    }
    Ok(())
}

fn private_file(path: &Path, writable: bool) -> Result<File> {
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect inventory file: {}", io_text(&error)))?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
        return Err("Inventory files must be private regular files without hard links".into());
    }
    before_private_open(path);
    let file = private_options()
        .read(true)
        .write(writable)
        .open(path)
        .map_err(|error| format!("Cannot open inventory file: {}", io_text(&error)))?;
    let opened = file.metadata().map_err(|error| io_text(&error))?;
    if !opened.is_file() {
        return Err("Inventory files must be private regular files without hard links".into());
    }
    if opened.dev() != metadata.dev()
        || opened.ino() != metadata.ino()
        || opened.nlink() != 1
        || opened.permissions().mode() & 0o077 != 0
    {
        return Err("Inventory file changed while opening".into());
    }
    Ok(file)
}

fn new_file(path: &Path) -> Result<File> {
    reject_symlinks(path)?;
    private_options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Cannot create inventory file: {}", io_text(&error)))
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "Cannot inspect inventory state: {}",
            io_text(&error)
        )),
    }
}

#[cfg(test)]
thread_local! { pub(super) static CREATE_STOP: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) }; }
fn creation_checkpoint(stage: u8) -> Result<()> {
    #[cfg(test)]
    if CREATE_STOP.with(|s| s.get()) == Some(stage) {
        return Err("Inventory creation interrupted by fixture".into());
    }
    let _ = stage;
    Ok(())
}
pub(super) fn create(directory: &Path) -> Result<Index> {
    create_bound(directory, random_bytes::<32>()?)
}
pub(super) fn create_bound(directory: &Path, identity: [u8; 32]) -> Result<Index> {
    create_bound_checked(directory, identity).map_err(|error| explain_open(directory, error))
}
fn create_bound_checked(directory: &Path, identity: [u8; 32]) -> Result<Index> {
    clean_path(directory)?;
    reject_symlinks(directory)?;
    // create, not create_dir_all or chmod: existing/legacy directories are inert.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|error| {
            format!(
                "Inventory creation requires a new directory: {}",
                io_text(&error)
            )
        })?;
    creation_checkpoint(1)?;
    let lock = new_file(&directory.join(".lock"))?;
    try_lock_file(&lock).map_err(|error| format!("Cannot lock inventory: {error}"))?;
    creation_checkpoint(2)?;
    let mut bytes = Vec::from(ID_MAGIC.as_slice());
    bytes.extend_from_slice(&identity);
    bytes.extend_from_slice(&sha256(&bytes));
    let mut id_file = new_file(&directory.join("identity.bin"))?;
    creation_checkpoint(6)?;
    id_file
        .write_all(&bytes)
        .and_then(|()| sync_file(&id_file))
        .map_err(|error| io_text(&error))?;
    creation_checkpoint(3)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory.join("objects"))
        .map_err(|error| io_text(&error))?;
    sync_directory(&directory.join("objects"))?;
    creation_checkpoint(4)?;
    let header = Header::empty(identity);
    // The redo log precedes the tree: a directory without the tree is an
    // interrupted creation, never a store from before the redo log.
    let redo = new_file(&directory.join(REDO))?;
    redo.write_all_at(&creation_redo(identity)[SLOT_BYTES as usize..], SLOT_BYTES)
        .and_then(|()| sync_file(&redo))
        .map_err(|error| io_text(&error))?;
    creation_checkpoint(8)?;
    let mut tree = new_file(&directory.join("tree.bin"))?;
    creation_checkpoint(7)?;
    tree.write_all(&header.encode())
        .and_then(|()| sync_file(&tree))
        .map_err(|error| io_text(&error))?;
    creation_checkpoint(5)?;
    sync_file(&lock).map_err(|error| io_text(&error))?;
    sync_directory(directory)?;
    sync_directory(
        directory
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    Ok(Index {
        directory: directory.to_owned(),
        name: super::store_name(directory),
        _lock: lock,
        tree,
        header,
        cache: tree::Cache::new(),
        segments: Segments::default(),
        redo: Some(Box::new(Redo::new(redo, 1, header))),
        read_only: false,
        interrupted: false,
        poisoned: false,
        #[cfg(test)]
        fault: None,
        #[cfg(test)]
        last_page_reads: 0,
        #[cfg(test)]
        requests: 0,
        #[cfg(test)]
        last_staged_nodes: 0,
        #[cfg(test)]
        legacy_records: false,
    })
}

fn clean_path(path: &Path) -> Result<()> {
    if path.file_name().is_none()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Inventory requires a clean directory path".into());
    }
    Ok(())
}

pub(super) fn open(
    directory: &Path,
    read_only: bool,
    expected_identity: Option<[u8; 32]>,
) -> Result<Index> {
    open_checked(directory, read_only, expected_identity)
        .map_err(|error| explain_open(directory, error))
}

fn open_checked(
    directory: &Path,
    read_only: bool,
    expected_identity: Option<[u8; 32]>,
) -> Result<Index> {
    clean_path(directory)?;
    private_directory(directory)?;
    // No new files, permission edits or legacy-format fallbacks during open.
    let mut id_file = private_file(&directory.join("identity.bin"), false)?;
    let identity = read_bounded(&mut id_file, 72)?;
    if identity.len() != 72
        || &identity[..8] != ID_MAGIC
        || sha256(&identity[..40]).as_slice() != &identity[40..]
    {
        return Err("Invalid inventory identity format or checksum".into());
    }
    let identity: [u8; 32] = identity[8..40]
        .try_into()
        .map_err(|_| "Invalid inventory identity")?;
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err("Inventory marker names a different index".into());
    }
    let lock = private_file(&directory.join(".lock"), false)?;
    try_lock_file(&lock).map_err(|error| {
        format!("Inventory storage is already open or cannot be locked: {error}")
    })?;
    private_directory(&directory.join("objects"))?;
    let mut tree = private_file(&directory.join("tree.bin"), !read_only)?;
    let published = directory.join("transaction.bin");
    let staging = directory.join("transaction.tmp");
    let redo_path = directory.join(REDO);
    // A WAL is only ever left by a release from before the redo log.
    let legacy = present(&published)? || Header::is_legacy(&tree::read_page(&mut tree, 0)?);
    let mut replay = Replay::default();
    let (header, redo) = if present(&redo_path)? {
        if present(&published)? || present(&staging)? {
            return Err("Inventory has both a redo log and an earlier WAL".into());
        }
        let file = private_file(&redo_path, !read_only)?;
        let (redo, state, pending) = recover(directory, &mut tree, file, identity, read_only)?;
        replay = pending;
        (state, (!read_only).then(|| Box::new(redo)))
    } else {
        if present(&published)? {
            if read_only {
                return Err(
                    "Inventory has a pending transaction; writable recovery is required".into(),
                );
            }
            let mut wal = private_file(&published, false)?;
            let bytes = read_bounded(&mut wal, MAX_WAL_BYTES + 48)?;
            let transaction = Transaction::decode_wal(&bytes, identity)?;
            transaction.validate_overlay(&mut tree)?;
            preflight(directory, &transaction)?;
            apply(
                directory,
                &mut tree,
                &transaction,
                true,
                #[cfg(test)]
                None,
                #[cfg(not(test))]
                (),
            )?;
            fs::remove_file(&published)
                .map_err(|error| format!("Cannot retire inventory WAL: {}", io_text(&error)))?;
            sync_directory(directory)?;
        }
        if !legacy {
            return Err("Inventory redo log is missing".into());
        }
        let header = Header::decode(&tree::read_page(&mut tree, 0)?)?;
        if present(&staging)? {
            if read_only {
                return Err(
                    "Inventory has unpublished staging; writable recovery is required".into(),
                );
            }
            private_file(&staging, false)?;
            fs::remove_file(&staging).map_err(|error| io_text(&error))?;
            sync_directory(directory)?;
        }
        let redo = if read_only {
            None
        } else {
            Some(Box::new(adopt_redo(directory, &mut tree, header)?))
        };
        (header, redo)
    };
    // Pages a read-only open replayed need not have reached the tree file.
    if replay.pages.is_empty() {
        let root = Header::decode(&tree::read_page(&mut tree, 0)?)?;
        if root != header
            || tree.metadata().map_err(|error| io_text(&error))?.len() != tree::offset(header.next)?
        {
            return Err("Inventory identity or file length mismatch".into());
        }
    }
    if header.identity != identity {
        return Err("Inventory identity or file length mismatch".into());
    }
    // Verify the reachable root without walking the entire inventory at open.
    if header.root != 0 && !replay.pages.contains_key(&header.root) {
        Node::decode(
            &tree::read_page(&mut tree, header.root)?,
            header.root,
            header.level,
            header.next,
            identity,
        )?;
    }
    Ok(Index {
        directory: directory.to_owned(),
        name: super::store_name(directory),
        _lock: lock,
        tree,
        header,
        cache: tree::Cache::pinned(std::mem::take(&mut replay.pages)),
        segments: Segments {
            pending: std::mem::take(&mut replay.segments),
            ..Segments::default()
        },
        redo,
        read_only,
        interrupted: replay.interrupted,
        poisoned: false,
        #[cfg(test)]
        fault: None,
        #[cfg(test)]
        last_page_reads: 0,
        #[cfg(test)]
        requests: 0,
        #[cfg(test)]
        last_staged_nodes: 0,
        #[cfg(test)]
        legacy_records: false,
    })
}

/// Gives a store from before the redo log its first checkpoint. The log is
/// durable before the root changes format, and the new root format makes
/// earlier releases refuse the store before any record exists.
fn adopt_redo(directory: &Path, tree: &mut File, header: Header) -> Result<Redo> {
    let file = new_file(&directory.join(REDO))?;
    file.write_all_at(&slot_bytes(1, header.tail, &header), SLOT_BYTES)
        .and_then(|()| sync_file(&file))
        .map_err(|error| format!("Cannot create inventory redo log: {}", io_text(&error)))?;
    sync_directory(directory)?;
    tree.write_all_at(&header.encode(), 0)
        .and_then(|()| sync_file(tree))
        .map_err(|error| format!("Cannot persist inventory root: {}", io_text(&error)))?;
    Ok(Redo::new(file, 1, header))
}

/// Replays records after the checkpoint and checkpoints again. Read-only
/// opens refuse pending records, as they refuse a pending WAL.
/// What a read-only open takes from records it may not apply: the pages and
/// segment bytes they wrote, which the files may lack after a crash.
#[derive(Default)]
struct Replay {
    pages: HashMap<u64, std::sync::Arc<Node>>,
    /// Per segment, where the pending bytes start and the bytes.
    segments: HashMap<u64, (u64, Vec<u8>)>,
    /// Any record or interrupted record write follows the checkpoint.
    interrupted: bool,
}

impl Replay {
    fn new(records: &[Transaction]) -> Result<Self> {
        let mut replay = Self::default();
        for transaction in records {
            if transaction.actions.iter().any(Action::writes_file) {
                return Err(
                    "Inventory has a pending record file; writable recovery is required".into(),
                );
            }
            for (id, node) in &transaction.pages {
                replay.pages.insert(*id, std::sync::Arc::new(node.clone()));
            }
            let Some((number, start, bytes)) = record_segment(transaction)? else {
                continue;
            };
            match replay.segments.get_mut(&number) {
                Some((first, pending)) if *first + pending.len() as u64 == start => {
                    pending.extend_from_slice(&bytes);
                }
                _ => {
                    replay.segments.insert(number, (start, bytes));
                }
            }
        }
        Ok(replay)
    }
}

/// Returns the open log, the state the records reach and, for a read-only
/// open, what they wrote. A writable open applies and checkpoints them.
fn recover(
    directory: &Path,
    tree: &mut File,
    file: File,
    identity: [u8; 32],
    read_only: bool,
) -> Result<(Redo, Header, Replay)> {
    let (generation, retired, slot) = read_slots(&file, identity)?;
    let (records, torn) = read_records(&file, generation, slot)?;
    // The live root is the checkpoint or a state a record reaches; a torn
    // root implies a record being applied. Anything else is a stale log.
    match Header::decode(&tree::read_page(tree, 0)?) {
        Ok(root) => {
            if root != slot && !records.iter().any(|record| record.after == root) {
                return Err("Stale inventory redo log disagrees with the live root".into());
            }
        }
        Err(error) if records.is_empty() => return Err(error),
        Err(_) => {}
    }
    if read_only {
        let state = records.last().map_or(slot, |record| record.after);
        let mut replay = Replay::new(&records)?;
        replay.interrupted = !records.is_empty() || torn;
        return Ok((Redo::new(file, generation, slot), state, replay));
    }
    let mut redo = Redo::new(file, generation, slot);
    let mut state = slot;
    // Every path a replay could write or remove is inspected first.
    for transaction in &records {
        preflight_redo(directory, transaction)?;
    }
    for transaction in &records {
        apply_logged(
            directory,
            tree,
            &mut redo,
            &mut Segments::default(),
            transaction,
            #[cfg(test)]
            None,
            #[cfg(not(test))]
            (),
        )?;
        state = transaction.after;
    }
    if !records.is_empty() || torn {
        tree.set_len(tree::offset(state.next)?)
            .map_err(|error| io_text(&error))?;
        checkpoint(directory, tree, &mut redo, &state)?;
    } else {
        remove_segments(directory, retired..slot.tail)?;
    }
    drop_unlogged(directory, &state)?;
    Ok((redo, state, Replay::default()))
}

/// Removes appends of changes that never became durable: bytes past the
/// active segment's length and a segment the next commit would create.
fn drop_unlogged(directory: &Path, header: &Header) -> Result<()> {
    let head = segment_path(directory, header.head);
    if header.head_length > 0 {
        let file = private_file(&head, true)?;
        if file.metadata().map_err(|error| io_text(&error))?.len() > header.head_length {
            file.set_len(header.head_length)
                .map_err(|error| io_text(&error))?;
        }
    } else {
        remove_segments(directory, header.head..header.head + 1)?;
    }
    remove_segments(directory, header.head + 1..header.head + 2)
}

/// Removes segments the durable root no longer references, if present.
/// A crash may leave some; the next checkpoint slot names them again.
fn remove_segments(directory: &Path, numbers: std::ops::Range<u64>) -> Result<()> {
    for number in numbers {
        let path = segment_path(directory, number);
        if present(&path)? {
            private_file(&path, false)?;
            fs::remove_file(&path)
                .map_err(|error| format!("Cannot retire inventory segment: {}", io_text(&error)))?;
        }
    }
    Ok(())
}

/// Makes every change since the last checkpoint durable in the tree and the
/// segments, then records `header` as the new checkpoint and starts the redo
/// log over. Segments the cleaner retired are removed afterwards.
fn checkpoint(directory: &Path, tree: &File, redo: &mut Redo, header: &Header) -> Result<()> {
    for number in &redo.dirty {
        if let Some(file) = redo.writers.get(number) {
            sync_file(file)
                .map_err(|error| format!("Cannot flush inventory segment: {}", io_text(&error)))?;
        }
    }
    for path in &redo.directories {
        sync_directory(path)?;
    }
    sync_file(tree)
        .map_err(|error| format!("Cannot persist inventory root: {}", io_text(&error)))?;
    let generation = redo
        .generation
        .checked_add(1)
        .ok_or("Inventory redo generation overflow")?;
    redo.file
        .write_all_at(
            &slot_bytes(generation, redo.slot.tail, header),
            generation % 2 * SLOT_BYTES,
        )
        .and_then(|()| sync_file(&redo.file))
        .map_err(|error| format!("Cannot checkpoint inventory: {}", io_text(&error)))?;
    let retired = redo.slot.tail..header.tail;
    redo.generation = generation;
    redo.slot = *header;
    redo.end = RECORDS;
    redo.dirty.clear();
    redo.directories.clear();
    redo.writers.retain(|number, _| *number == header.head);
    // Old records would only lengthen the damage probe of the next open.
    redo.file
        .set_len(RECORDS)
        .map_err(|error| format!("Cannot truncate inventory redo log: {}", io_text(&error)))?;
    remove_segments(directory, retired)
}

fn read_bounded(file: &mut File, maximum: usize) -> Result<Vec<u8>> {
    if file.metadata().map_err(|error| io_text(&error))?.len() > maximum as u64 {
        return Err("Inventory file exceeds its byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_text(&error))?;
    if bytes.len() > maximum {
        return Err("Inventory file grew beyond its byte limit".into());
    }
    Ok(bytes)
}

pub(super) fn value_path(directory: &Path, key: &Key) -> PathBuf {
    let name: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    directory.join("objects").join(&name[..2]).join(&name[2..])
}

pub(super) fn check_value_path(directory: &Path, key: &Key) -> Result<()> {
    let path = value_path(directory, key);
    private_directory(&directory.join("objects"))?;
    let parent = path.parent().ok_or("Missing inventory object directory")?;
    if present(parent)? {
        private_directory(parent)?;
    }
    reject_symlinks(&path)?;
    if present(&path)? {
        private_file(&path, false)?;
    }
    Ok(())
}

pub(super) fn read_value(
    directory: &Path,
    header: &Header,
    segments: &mut Segments,
    key: &Key,
    reference: ValueRef,
) -> Result<Vec<u8>> {
    if reference.inline {
        return Ok(reference.inline_data().to_vec());
    }
    if reference.segment {
        let (number, offset) = reference.location();
        if number < header.tail
            || number > header.head
            || (number == header.head && header.head_length == 0)
        {
            return Err("Inventory record names a segment outside the value log".into());
        }
        let bytes = segments.read_envelope(
            directory,
            header.identity,
            number,
            u64::from(offset),
            reference.length,
        )?;
        let (stored, data, checksum) = open_envelope(&bytes)?;
        let bound = if reference.fast {
            checksum == Some(reference.checksum())
        } else {
            checksum.is_none() && sha256(data)[..20] == reference.digest[12..]
        };
        if stored != *key || data.len() != reference.length as usize || !bound {
            return Err("Inventory record identity, length or checksum mismatch".into());
        }
        return Ok(data.to_vec());
    }
    check_value_path(directory, key)?;
    let mut file = private_file(&value_path(directory, key), false)?;
    let bytes = read_bounded(&mut file, reference.length as usize + 108)?;
    let (stored, data, checksum) = open_envelope(&bytes)?;
    if stored != *key
        || data.len() != reference.length as usize
        || checksum.is_some()
        || sha256(data) != reference.digest
    {
        return Err("Inventory record identity, length or checksum mismatch".into());
    }
    Ok(data.to_vec())
}

fn envelope(key: &Key, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() + ENVELOPE_BYTES as usize);
    bytes.extend_from_slice(VALUE_MAGIC);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    bytes.extend_from_slice(&sha256(&bytes));
    bytes
}

/// An envelope whose checksum field holds its XXH64 and then zeros.
fn fast_envelope(key: &Key, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() + ENVELOPE_BYTES as usize);
    bytes.extend_from_slice(FAST_VALUE_MAGIC);
    bytes.extend_from_slice(key);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    bytes.extend_from_slice(&xxh64(&bytes).to_le_bytes());
    bytes.resize(bytes.len() + 24, 0);
    bytes
}

fn fast_envelope_checksum(key: &Key, data: &[u8]) -> u64 {
    let envelope = fast_envelope(key, data);
    let end = envelope.len() - 32;
    u64::from_le_bytes(envelope[end..end + 8].try_into().unwrap_or_default())
}

/// Checks a complete envelope and returns its key, value and, for an XXH64
/// envelope, its checksum.
fn open_envelope(bytes: &[u8]) -> Result<(Key, &[u8], Option<u64>)> {
    let invalid = || "Inventory record identity, length or checksum mismatch".to_owned();
    if bytes.len() < ENVELOPE_BYTES as usize
        || u32::from_le_bytes(
            bytes[72..76]
                .try_into()
                .map_err(|_| "Invalid object length")?,
        ) as usize
            != bytes.len() - ENVELOPE_BYTES as usize
    {
        return Err(invalid());
    }
    let end = bytes.len() - 32;
    let checksum = if &bytes[..8] == FAST_VALUE_MAGIC {
        let checksum = xxh64(&bytes[..end]);
        if checksum.to_le_bytes() != bytes[end..end + 8]
            || bytes[end + 8..].iter().any(|byte| *byte != 0)
        {
            return Err(invalid());
        }
        Some(checksum)
    } else if &bytes[..8] == VALUE_MAGIC && sha256(&bytes[..end]).as_slice() == &bytes[end..] {
        None
    } else {
        return Err(invalid());
    };
    let key = bytes[8..72].try_into().map_err(|_| "Invalid record key")?;
    Ok((key, &bytes[76..end], checksum))
}

fn segment_path(directory: &Path, number: u64) -> PathBuf {
    directory.join("segments").join(format!("{number:016x}"))
}

fn segment_header(identity: [u8; 32], number: u64) -> Vec<u8> {
    let mut bytes = Vec::from(SEGMENT_MAGIC.as_slice());
    bytes.extend_from_slice(&identity);
    bytes.extend_from_slice(&number.to_le_bytes());
    bytes.extend_from_slice(&sha256(&bytes));
    bytes
}

/// One open, identity-checked value-log segment.
pub(super) struct Segment {
    file: File,
    pub length: u64,
}

/// Segments kept open for reads, each inspected once when first opened.
/// Envelopes are never rewritten in place, so a descriptor stays valid until
/// its segment leaves the log; the owner holds the store lock throughout.
#[derive(Default)]
pub(super) struct Segments {
    open: HashMap<u64, Segment>,
    order: VecDeque<u64>,
    /// Bytes of records a read-only open replayed, by segment: where they
    /// start and the bytes. Reads past that start never touch the file.
    pending: HashMap<u64, (u64, Vec<u8>)>,
}
const OPEN_SEGMENTS: usize = 64;

impl Segments {
    fn get(&mut self, directory: &Path, identity: [u8; 32], number: u64) -> Result<&Segment> {
        if !self.open.contains_key(&number) {
            let segment = Segment::open(directory, identity, number)?;
            while self.open.len() >= OPEN_SEGMENTS {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.open.remove(&oldest);
            }
            self.open.insert(number, segment);
            self.order.push_back(number);
        }
        self.open
            .get(&number)
            .ok_or_else(|| "Inventory segment cache lost an entry".into())
    }

    /// The envelope of a `length`-byte value at `offset` of segment `number`.
    fn read_envelope(
        &mut self,
        directory: &Path,
        identity: [u8; 32],
        number: u64,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>> {
        if let Some((start, bytes)) = self.pending.get(&number)
            && offset >= *start
        {
            let from = (offset - start) as usize;
            let size = (ENVELOPE_BYTES + u64::from(length)) as usize;
            return bytes
                .get(from..from + size)
                .filter(|_| offset >= SEGMENT_HEADER_BYTES && length as usize <= MAX_VALUE_BYTES)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "Inventory record lies past the end of its segment".into());
        }
        self.get(directory, identity, number)?
            .read_envelope(offset, length)
    }

    /// Records that a commit appended to an open segment.
    fn extend(&mut self, number: u64, length: u64) {
        if let Some(segment) = self.open.get_mut(&number) {
            segment.length = segment.length.max(length);
        }
    }

    /// Closes segments that a durable commit retired from the log.
    pub(super) fn retire(&mut self, retired: std::ops::Range<u64>) {
        self.open.retain(|number, _| !retired.contains(number));
        self.order.retain(|number| !retired.contains(number));
    }
}

impl Segment {
    pub(super) fn open(directory: &Path, identity: [u8; 32], number: u64) -> Result<Self> {
        private_directory(&directory.join("segments"))?;
        let mut file = private_file(&segment_path(directory, number), false)?;
        let length = file.metadata().map_err(|error| io_text(&error))?.len();
        let mut header = [0; SEGMENT_HEADER_BYTES as usize];
        file.read_exact(&mut header)
            .map_err(|error| format!("Cannot read inventory segment: {}", io_text(&error)))?;
        if header[..] != segment_header(identity, number)[..] {
            return Err("Inventory segment identity or checksum mismatch".into());
        }
        Ok(Self { file, length })
    }

    /// The complete envelope of a `length`-byte value at `offset`, in one read.
    fn read_envelope(&self, offset: u64, length: u32) -> Result<Vec<u8>> {
        let size = ENVELOPE_BYTES + u64::from(length);
        if offset < SEGMENT_HEADER_BYTES
            || length as usize > MAX_VALUE_BYTES
            || offset.checked_add(size).is_none_or(|end| end > self.length)
        {
            return Err("Inventory record lies past the end of its segment".into());
        }
        let mut bytes = vec![0; size as usize];
        self.file
            .read_exact_at(&mut bytes, offset)
            .map_err(|error| format!("Cannot read inventory segment: {}", io_text(&error)))?;
        Ok(bytes)
    }

    /// The envelope at `offset` with its reference in segment `number`, or
    /// `None` at the exact end of the segment.
    pub(super) fn envelope(
        &mut self,
        number: u64,
        offset: u64,
    ) -> Result<Option<(Key, Vec<u8>, ValueRef)>> {
        if offset == self.length {
            return Ok(None);
        }
        let mut prefix = [0; 76];
        if offset < SEGMENT_HEADER_BYTES || offset > self.length - ENVELOPE_BYTES.min(self.length) {
            return Err("Inventory segment offset out of range".into());
        }
        self.file
            .read_exact_at(&mut prefix, offset)
            .map_err(|error| format!("Cannot read inventory segment: {}", io_text(&error)))?;
        let length = u32::from_le_bytes(
            prefix[72..76]
                .try_into()
                .map_err(|_| "Invalid object length")?,
        ) as u64;
        if length > MAX_VALUE_BYTES as u64 || offset + ENVELOPE_BYTES + length > self.length {
            return Err("Inventory segment envelope exceeds its segment".into());
        }
        let bytes = self.read_envelope(offset, length as u32)?;
        let (key, data, checksum) = open_envelope(&bytes)?;
        let reference = match checksum {
            Some(checksum) => ValueRef::logged(number, offset as u32, length as u32, checksum),
            None => ValueRef::segment(number, offset as u32, data),
        };
        Ok(Some((key, data.to_vec(), reference)))
    }
}

/// Inspects the segments and record files a redo replay writes or removes.
fn preflight_redo(directory: &Path, transaction: &Transaction) -> Result<()> {
    private_directory(directory)?;
    let segments = directory.join("segments");
    if present(&segments)? {
        private_directory(&segments)?;
    }
    if let Some((number, _)) = record_target(transaction) {
        check_segment_path(directory, number)?;
    }
    for number in transaction.before.tail..transaction.after.tail {
        check_segment_path(directory, number)?;
    }
    preflight_files(directory, transaction)
}

/// Record-file checks for the keys a transaction writes or may remove.
fn preflight_files(directory: &Path, transaction: &Transaction) -> Result<()> {
    for action in &transaction.actions {
        if action.replaces_file || action.writes_file() {
            check_value_path(directory, &action.key)?;
        }
    }
    Ok(())
}

fn preflight(directory: &Path, transaction: &Transaction) -> Result<()> {
    private_directory(directory)?;
    // Inspect all segment paths before publishing a WAL or applying any redo.
    // Recovery may replace a torn private file, but never an unsafe entry.
    let segments = directory.join("segments");
    if present(&segments)? {
        private_directory(&segments)?;
    }
    if transaction
        .after
        .tail
        .saturating_sub(transaction.before.tail)
        > super::MAX_RETIRED_SEGMENTS
    {
        return Err("Inventory transaction exceeds its segment retirement budget".into());
    }
    if transaction.after.head != transaction.before.head {
        check_segment_path(directory, transaction.before.head)?;
    }
    for number in transaction.before.tail..transaction.after.tail {
        check_segment_path(directory, number)?;
    }
    preflight_files(directory, transaction)
}

fn check_segment_path(directory: &Path, number: u64) -> Result<()> {
    let path = segment_path(directory, number);
    if present(&path)? {
        private_file(&path, false)?;
    }
    Ok(())
}

/// Why a commit failed and whether its WAL was already published, which
/// makes its outcome uncertain until recovery replays or discards it.
pub(super) struct Failure {
    published: bool,
    error: String,
}

impl Failure {
    fn before(error: String) -> Self {
        Self {
            published: false,
            error,
        }
    }
    fn after(error: String) -> Self {
        Self {
            published: true,
            error,
        }
    }

    /// An English error that names the store and the operator action.
    pub(super) fn explain(self, name: &str) -> String {
        let full = self.error.contains(STORAGE_FULL);
        let recover = super::RECOVER;
        match (self.published, full) {
            (false, true) => format!(
                "{name}: the disk is full, so the change was not saved and existing records are \
                 unchanged; free disk space, then restart Mynou"
            ),
            (false, false) => format!(
                "{name}: the change was not saved and existing records are unchanged ({}); \
                 restart Mynou before retrying",
                self.error
            ),
            (true, true) => format!(
                "{name}: the disk filled up while a change was being saved, so its outcome is \
                 uncertain; {recover} once disk space is free"
            ),
            (true, false) => format!(
                "{name}: the outcome of a change is uncertain ({}); {recover}",
                self.error
            ),
        }
    }
}

/// Explains an open whose recovery ran out of space: the WAL is kept, so a
/// restart after freeing space completes it. Other errors are unchanged.
pub(super) fn explain_open(directory: &Path, error: String) -> String {
    if error.contains(STORAGE_FULL) {
        format!(
            "{}: the disk is full, so recovery could not finish; {} once disk space is free",
            super::store_name(directory),
            super::RECOVER
        )
    } else {
        error
    }
}

#[cfg(test)]
fn inject(fault: Option<Fault>, stage: Stage) -> std::io::Result<()> {
    if fault == Some(Fault::Full(stage)) {
        Err(std::io::ErrorKind::StorageFull.into())
    } else {
        Ok(())
    }
}
#[cfg(not(test))]
fn inject(_: (), _: Stage) -> std::io::Result<()> {
    Ok(())
}

/// Commits with one flush: the record is durable once the redo log is,
/// and applying it to the tree and segments needs no flush of its own.
/// A full redo log is checkpointed before the record is written.
pub(super) fn commit(
    index: &mut Index,
    transaction: &Transaction,
    body: &[u8],
) -> std::result::Result<(), Failure> {
    #[cfg(test)]
    let fault = index.fault;
    #[cfg(not(test))]
    let fault = ();
    transaction
        .validate_overlay(&mut index.tree)
        .map_err(Failure::before)?;
    let segments = index.directory.join("segments");
    if present(&segments).map_err(Failure::before)? {
        private_directory(&segments).map_err(Failure::before)?;
    }
    preflight_files(&index.directory, transaction).map_err(Failure::before)?;
    let Some(redo) = index.redo.as_mut() else {
        return Err(Failure::before(
            "Inventory is open without its redo log".into(),
        ));
    };
    let record = record_bytes(redo.generation, body);
    if redo.pending() && redo.end - RECORDS + record.len() as u64 > CHECKPOINT_BYTES {
        // Until the slot is durable the previous checkpoint and records hold.
        index.poisoned = true;
        checkpoint(&index.directory, &index.tree, redo, &index.header).map_err(Failure::before)?;
        index.poisoned = false;
    }
    // From the first record byte onward, storage needs a reopen: recovery
    // replays an intact record and ignores an interrupted one.
    index.poisoned = true;
    #[cfg(test)]
    if fault == Some(Fault::Staged) {
        return Err(Failure::before(
            "Injected interruption before the redo record was written".into(),
        ));
    }
    inject(fault, Stage::Wal)
        .and_then(|()| redo.file.write_all_at(&record, redo.end))
        .map_err(|error| {
            Failure::before(format!(
                "Cannot write inventory redo record: {}",
                io_text(&error)
            ))
        })?;
    if let Err(error) = sync_file(&redo.file) {
        // The record may or may not have reached the disk. Removing it
        // leaves the change unsaved unless the machine stops first.
        let _ = redo.file.set_len(redo.end);
        return Err(Failure::after(format!(
            "Cannot flush inventory redo log: {}",
            io_text(&error)
        )));
    }
    inject(fault, Stage::Directory).map_err(|error| {
        Failure::after(format!(
            "Cannot apply inventory change: {}",
            io_text(&error)
        ))
    })?;
    #[cfg(test)]
    if fault == Some(Fault::Published) {
        return Err(Failure::after(
            "Injected interruption after the redo record became durable".into(),
        ));
    }
    apply_logged(
        &index.directory,
        &mut index.tree,
        redo,
        &mut index.segments,
        transaction,
        fault,
    )
    .map_err(Failure::after)?;
    #[cfg(test)]
    if fault == Some(Fault::Unlinked) {
        return Err(Failure::after(
            "Injected interruption after the change was applied".into(),
        ));
    }
    redo.end += record.len() as u64;
    index.poisoned = false;
    Ok(())
}

/// Checkpoints pending records, as a clean close does.
pub(super) fn close(index: &mut Index) -> Result<()> {
    if let Some(redo) = index.redo.as_mut()
        && redo.pending()
    {
        checkpoint(&index.directory, &index.tree, redo, &index.header)?;
    }
    Ok(())
}

/// Writes a recorded change to the segments, record files and tree without
/// flushing; the next checkpoint does. Replaying a record writes the same
/// bytes again, so it is idempotent.
fn apply_logged(
    directory: &Path,
    tree: &mut File,
    redo: &mut Redo,
    segments: &mut Segments,
    transaction: &Transaction,
    #[cfg(test)] fault: Option<Fault>,
    #[cfg(not(test))] fault: (),
) -> Result<()> {
    if let Some((number, start, bytes)) = record_segment(transaction)? {
        let file = redo.segment(directory, number, start == 0)?;
        inject(fault, Stage::Objects)
            .and_then(|()| file.write_all_at(&bytes, start))
            .map_err(|error| format!("Cannot persist inventory segment: {}", io_text(&error)))?;
        segments.extend(number, start + bytes.len() as u64);
    }
    apply_files_and_pages(directory, tree, redo, transaction, fault)
}

/// The bytes a record appends to its segment, with the segment and where
/// they start; a new segment's bytes begin with its header.
fn record_segment(transaction: &Transaction) -> Result<Option<(u64, u64, Vec<u8>)>> {
    let Some((number, start)) = record_target(transaction) else {
        return Ok(None);
    };
    let mut bytes = if start == 0 {
        segment_header(transaction.after.identity, number)
    } else {
        Vec::new()
    };
    for action in &transaction.actions {
        let (Some((segment, offset)), Some(data)) = (action.segment, action.data.as_ref()) else {
            continue;
        };
        if segment != number || u64::from(offset) != start + bytes.len() as u64 {
            return Err("Inventory segment layout disagrees with its record".into());
        }
        bytes.extend_from_slice(&action.envelope(data));
    }
    Ok(Some((number, start, bytes)))
}

/// The segment a record appends to and where its bytes start: zero for a
/// new segment, whose header comes first.
fn record_target(transaction: &Transaction) -> Option<(u64, u64)> {
    transaction.actions.iter().find_map(|action| {
        action
            .segment
            .filter(|_| action.data.is_some())
            .map(|(number, offset)| {
                let offset = u64::from(offset);
                (
                    number,
                    if offset == SEGMENT_HEADER_BYTES {
                        0
                    } else {
                        offset
                    },
                )
            })
    })
}

fn apply_files_and_pages(
    directory: &Path,
    tree: &mut File,
    redo: &mut Redo,
    transaction: &Transaction,
    #[cfg(test)] fault: Option<Fault>,
    #[cfg(not(test))] fault: (),
) -> Result<()> {
    #[cfg(test)]
    if fault == Some(Fault::FirstRecord) {
        return Err("Injected interruption during record apply".into());
    }
    for action in &transaction.actions {
        let path = value_path(directory, &action.key);
        let parent = path.parent().ok_or("Missing inventory object directory")?;
        if let Some(data) = action.data.as_ref().filter(|_| action.writes_file()) {
            if write_record_file(directory, &path, &action.key, data, fault)? {
                redo.directories.insert(directory.join("objects"));
            }
            redo.directories.insert(parent.to_owned());
        } else if action.replaces_file && present(&path)? {
            private_file(&path, false)?;
            fs::remove_file(&path)
                .map_err(|error| format!("Cannot delete inventory record: {}", io_text(&error)))?;
            redo.directories.insert(parent.to_owned());
        }
    }
    for (number, (id, node)) in transaction.pages.iter().enumerate() {
        inject(fault, Stage::Tree)
            .and_then(|()| {
                tree.write_all_at(
                    &node.encode().map_err(std::io::Error::other)?,
                    tree::offset(*id).map_err(std::io::Error::other)?,
                )
            })
            .map_err(|error| format!("Cannot apply inventory page: {}", io_text(&error)))?;
        #[cfg(test)]
        if number == 0 && fault == Some(Fault::FirstPage) {
            return Err("Injected interruption during page apply".into());
        }
        #[cfg(not(test))]
        let _ = number;
    }
    let length = tree::offset(transaction.after.next)?;
    if tree.metadata().map_err(|error| io_text(&error))?.len() < length {
        tree.set_len(length).map_err(|error| io_text(&error))?;
    }
    tree.write_all_at(&transaction.after.encode(), 0)
        .map_err(|error| format!("Cannot persist inventory root: {}", io_text(&error)))?;
    #[cfg(test)]
    if fault == Some(Fault::Applied) {
        return Err("Injected interruption after durable apply".into());
    }
    Ok(())
}

/// Writes a legacy record file under a temporary name, flushes it and
/// publishes it at its key. Returns whether its object directory is new.
fn write_record_file(
    directory: &Path,
    path: &Path,
    key: &Key,
    data: &[u8],
    #[cfg(test)] fault: Option<Fault>,
    #[cfg(not(test))] fault: (),
) -> Result<bool> {
    let parent = path.parent().ok_or("Missing inventory object directory")?;
    let mut created = false;
    if !present(parent)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                format!(
                    "Cannot create inventory object directory: {}",
                    io_text(&error)
                )
            })?;
        created = true;
    }
    private_directory(parent)?;
    let _ = directory;
    let temporary = parent.join(format!(".record-{}.tmp", hex(&random_bytes::<16>()?)));
    let mut owns_temporary = false;
    let result = (|| -> Result<()> {
        let mut file = new_file(&temporary)?;
        owns_temporary = true;
        inject(fault, Stage::Objects)
            .and_then(|()| file.write_all(&envelope(key, data)))
            .and_then(|()| sync_file(&file))
            .map_err(|error| format!("Cannot persist inventory record: {}", io_text(&error)))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Cannot publish inventory record: {}", io_text(&error)))?;
        Ok(())
    })();
    if result.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| created)
}

/// Applies a WAL left by an earlier release, flushing as that release did.
fn apply(
    directory: &Path,
    tree: &mut File,
    transaction: &Transaction,
    recovering: bool,
    #[cfg(test)] fault: Option<Fault>,
    #[cfg(not(test))] fault: (),
) -> Result<()> {
    let mut affected = BTreeSet::new();
    // Recovery cannot tell whether an interrupted apply made its new object
    // directories durable, so it always synchronizes `objects`.
    let mut created = recovering;
    write_segment(directory, transaction, fault)?;
    for action in &transaction.actions {
        let path = value_path(directory, &action.key);
        let parent = path.parent().ok_or("Missing inventory object directory")?;
        if let Some(data) = action.data.as_ref().filter(|_| action.writes_file()) {
            created |= write_record_file(directory, &path, &action.key, data, fault)?;
            affected.insert(parent.to_owned());
        } else if action.replaces_file && present(&path)? {
            fs::remove_file(&path)
                .map_err(|error| format!("Cannot delete inventory record: {}", io_text(&error)))?;
            affected.insert(parent.to_owned());
        }
    }
    for path in affected {
        sync_directory(&path)?;
    }
    if created {
        sync_directory(&directory.join("objects"))?;
    }
    for (id, node) in &transaction.pages {
        tree.seek(SeekFrom::Start(tree::offset(*id)?))
            .and_then(|_| inject(fault, Stage::Tree))
            .and_then(|()| tree.write_all(&node.encode().map_err(std::io::Error::other)?))
            .map_err(|error| format!("Cannot apply inventory page: {}", io_text(&error)))?;
    }
    tree.set_len(tree::offset(transaction.after.next)?)
        .map_err(|error| io_text(&error))?;
    tree.seek(SeekFrom::Start(0))
        .and_then(|_| tree.write_all(&transaction.after.encode()))
        .and_then(|()| sync_file(tree))
        .map_err(|error| format!("Cannot persist inventory root: {}", io_text(&error)))?;
    // The durable root no longer references segments the cleaner emptied.
    if transaction.after.tail > transaction.before.tail {
        remove_segments(directory, transaction.before.tail..transaction.after.tail)?;
        sync_directory(&directory.join("segments"))?;
    }
    Ok(())
}

/// Writes the segment of a WAL left by an earlier release under a temporary
/// name, flushes it once and publishes it at its number.
fn write_segment(
    directory: &Path,
    transaction: &Transaction,
    #[cfg(test)] fault: Option<Fault>,
    #[cfg(not(test))] fault: (),
) -> Result<()> {
    let number = transaction.before.head;
    let mut bytes = Vec::new();
    for action in &transaction.actions {
        let (Some((segment, offset)), Some(data)) = (action.segment, action.data.as_ref()) else {
            continue;
        };
        if bytes.is_empty() {
            bytes = segment_header(transaction.after.identity, number);
        }
        if segment != number || offset as usize != bytes.len() {
            return Err("Inventory segment layout disagrees with its WAL".into());
        }
        bytes.extend_from_slice(&action.envelope(data));
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let segments = directory.join("segments");
    if !present(&segments)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&segments)
            .map_err(|error| format!("Cannot create inventory segments: {}", io_text(&error)))?;
        sync_directory(directory)?;
    }
    private_directory(&segments)?;
    let temporary = segments.join(format!(".segment-{}.tmp", hex(&random_bytes::<16>()?)));
    let mut owns_temporary = false;
    let result = (|| -> Result<()> {
        let mut file = new_file(&temporary)?;
        owns_temporary = true;
        inject(fault, Stage::Objects)
            .and_then(|()| file.write_all(&bytes))
            .and_then(|()| sync_file(&file))
            .map_err(|error| format!("Cannot persist inventory segment: {}", io_text(&error)))?;
        check_segment_path(directory, number)?;
        fs::rename(&temporary, segment_path(directory, number))
            .map_err(|error| format!("Cannot publish inventory segment: {}", io_text(&error)))?;
        sync_directory(&segments)
    })();
    if result.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Test fixture: the body of every redo record after the checkpoint.
#[cfg(test)]
pub(crate) fn redo_bodies(directory: &Path) -> Vec<Vec<u8>> {
    let bytes = fs::read(directory.join(REDO)).unwrap();
    let mut bodies = Vec::new();
    let mut offset = RECORDS as usize;
    while offset + 24 <= bytes.len() && &bytes[offset..offset + 8] == RECORD_MAGIC {
        let length =
            u64::from_le_bytes(bytes[offset + 16..offset + 24].try_into().unwrap()) as usize;
        bodies.push(bytes[offset + 24..offset + 24 + length].to_vec());
        offset += (RECORD_FRAMING as usize + length).next_multiple_of(RECORD_ALIGN as usize);
    }
    bodies
}

/// Test fixture: replaces the redo record at `index` with a correctly framed
/// and checksummed record carrying `body`, keeping its generation.
#[cfg(test)]
pub(crate) fn rewrite_redo_body(directory: &Path, index: usize, body: &[u8]) {
    let path = directory.join(REDO);
    let mut bytes = fs::read(&path).unwrap();
    let mut offset = RECORDS as usize;
    for _ in 0..index {
        let length =
            u64::from_le_bytes(bytes[offset + 16..offset + 24].try_into().unwrap()) as usize;
        offset += (RECORD_FRAMING as usize + length).next_multiple_of(RECORD_ALIGN as usize);
    }
    let generation = u64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().unwrap());
    let record = record_bytes(generation, body);
    let old = u64::from_le_bytes(bytes[offset + 16..offset + 24].try_into().unwrap()) as usize;
    let old = (RECORD_FRAMING as usize + old).next_multiple_of(RECORD_ALIGN as usize);
    assert_eq!(record.len(), old, "fixture bodies keep their aligned size");
    bytes[offset..offset + old].copy_from_slice(&record);
    fs::write(&path, bytes).unwrap();
}

/// Test fixture: flips one byte inside every stored envelope of `key` (in any
/// segment or legacy record file) and returns how many it damaged.
#[cfg(test)]
pub(crate) fn damage_record(directory: &Path, key: &Key) -> usize {
    let mut damaged = 0;
    let legacy = value_path(directory, key);
    if let Ok(mut bytes) = fs::read(&legacy) {
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x55;
        fs::write(&legacy, bytes).unwrap();
        damaged += 1;
    }
    let Ok(entries) = fs::read_dir(directory.join("segments")) else {
        return damaged;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        let mut bytes = fs::read(&path).unwrap();
        let mut offset = SEGMENT_HEADER_BYTES as usize;
        while offset + ENVELOPE_BYTES as usize <= bytes.len() {
            let length =
                u32::from_le_bytes(bytes[offset + 72..offset + 76].try_into().unwrap()) as usize;
            if bytes[offset + 8..offset + 72] == key[..] {
                bytes[offset + 76 + length / 2] ^= 0x55;
                damaged += 1;
            }
            offset += ENVELOPE_BYTES as usize + length;
        }
        fs::write(&path, bytes).unwrap();
    }
    damaged
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Fault {
    Staged,
    Published,
    FirstRecord,
    FirstPage,
    Applied,
    Unlinked,
    /// The storage device reports no space at this stage.
    Full(Stage),
}

/// Commit stages where tests inject a full disk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Stage {
    /// Writing the redo record, before it can be durable.
    Wal,
    /// Writing the value-log segment or a legacy record file.
    Objects,
    /// Rewriting tree pages in place.
    Tree,
    /// Right after the redo record is durable, before it is applied.
    Directory,
}
