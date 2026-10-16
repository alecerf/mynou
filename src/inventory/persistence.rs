use super::{
    Index, Key, MAX_CHANGES, MAX_VALUE_BYTES,
    tree::{self, Header, MAX_DIRTY_PAGES, Node, PAGE_SIZE, Tree, ValueRef},
};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    store::{private_options, reject_symlinks, sync_directory, sync_file},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(super) const MAX_WAL_BYTES: usize = 128 * 1024 * 1024;
const WAL_MAGIC: &[u8; 8] = b"MYNOUIW1";
const VALUE_MAGIC: &[u8; 8] = b"MYNOUIV1";
const ID_MAGIC: &[u8; 8] = b"MYNOUID1";
const SEGMENT_MAGIC: &[u8; 8] = b"MYNOUIS1";
/// Magic, store identity, segment number and their SHA-256.
pub(super) const SEGMENT_HEADER_BYTES: u64 = 80;
/// Magic, key and length before a value, its SHA-256 after.
pub(super) const ENVELOPE_BYTES: u64 = 108;

pub(super) struct Action {
    pub key: Key,
    pub data: Option<Vec<u8>>,
    /// Inline values live in the tree leaf and have no record file.
    pub inline: bool,
    pub existed: bool,
    /// Segment number and offset of a put written to the value log. Puts
    /// without it come only from WALs written before the log existed.
    pub segment: Option<(u64, u32)>,
}

impl Action {
    pub(super) fn reference(&self) -> Result<Option<ValueRef>> {
        self.data
            .as_ref()
            .map(|data| {
                if self.inline {
                    ValueRef::inline(data)
                } else if let Some((number, offset)) = self.segment {
                    Ok(ValueRef::segment(number, offset, data))
                } else {
                    Ok(ValueRef::file(data))
                }
            })
            .transpose()
    }
}
pub(super) struct Transaction {
    pub before: Header,
    pub after: Header,
    pub pages: BTreeMap<u64, Node>,
    pub actions: Vec<Action>,
}

impl Transaction {
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
            return Err("Inventory transaction exceeds its WAL byte budget".into());
        }
        let mut bytes = Vec::with_capacity(length + 48);
        bytes.extend_from_slice(WAL_MAGIC);
        bytes.extend_from_slice(&(length as u64).to_le_bytes());
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
            bytes.push(u8::from(action.existed));
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
        let digest = sha256(&bytes);
        bytes.extend_from_slice(&digest);
        Ok(bytes)
    }

    fn decode(bytes: &[u8], identity: [u8; 32]) -> Result<Self> {
        if bytes.len() < 48 || bytes.len() > MAX_WAL_BYTES + 48 || &bytes[..8] != WAL_MAGIC {
            return Err("Invalid published inventory WAL format or length".into());
        }
        let length = u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| "Invalid WAL length")?);
        if length.checked_add(48) != Some(bytes.len() as u64)
            || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
        {
            return Err("Corrupt or truncated published inventory WAL".into());
        }
        let mut reader = Reader {
            bytes: &bytes[16..bytes.len() - 32],
            position: 0,
        };
        let before = Header::decode(reader.take(PAGE_SIZE)?)?;
        let after = Header::decode(reader.take(PAGE_SIZE)?)?;
        if before.identity != identity
            || after.identity != identity
            || before.sequence.checked_add(1) != Some(after.sequence)
            || after.next < before.next
            || after.tail < before.tail
            || after.tail > before.head
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
        if count == 0 || count > MAX_CHANGES {
            return Err("Invalid inventory WAL action count".into());
        }
        let mut actions = Vec::new();
        let mut keys = BTreeSet::new();
        let mut expected = before.count;
        let mut segment_end = SEGMENT_HEADER_BYTES;
        for _ in 0..count {
            let key: Key = reader.take(64)?.try_into().map_err(|_| "Invalid WAL key")?;
            if !keys.insert(key) {
                return Err("Duplicate inventory WAL action".into());
            }
            let put = reader.byte()?;
            let existed = reader.byte()?;
            let length = reader.u32()? as usize;
            if put > 3
                || existed > 1
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
                let number = reader.u64()?;
                let offset = u64::from(reader.u32()?);
                if before.head.checked_add(1) != Some(after.head)
                    || number != before.head
                    || offset != segment_end
                {
                    return Err("Inventory WAL segment location mismatch".into());
                }
                segment_end = segment_end
                    .checked_add(ENVELOPE_BYTES + length as u64)
                    .filter(|end| *end <= u64::from(u32::MAX))
                    .ok_or("Inventory WAL segment overflow")?;
                Some((number, offset as u32))
            } else {
                None
            };
            let data = if put != 0 {
                Some(reader.take(length)?.to_vec())
            } else {
                None
            };
            expected = match (put != 0, existed) {
                (true, 0) => expected
                    .checked_add(1)
                    .ok_or("Inventory WAL count overflow")?,
                (false, 1) => expected
                    .checked_sub(1)
                    .ok_or("Inventory WAL count underflow")?,
                _ => expected,
            };
            actions.push(Action {
                key,
                data,
                inline: put == 2,
                existed: existed == 1,
                segment,
            });
        }
        if (segment_end != SEGMENT_HEADER_BYTES) != (after.head != before.head) {
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
        let length = file.metadata().map_err(|error| error.to_string())?.len();
        if length < tree::offset(self.before.next)? || length > tree::offset(self.after.next)? {
            return Err("Inventory file length disagrees with recovery".into());
        }
        let mut cache = tree::Cache::new();
        let mut overlay = Tree::new(file, self.after, &mut cache);
        overlay.dirty = self.pages.clone();
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

fn private_directory(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect inventory directory: {error}"))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("Inventory directory must be private and regular".into());
    }
    Ok(())
}

fn private_file(path: &Path, writable: bool) -> Result<File> {
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect inventory file: {error}"))?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
        return Err("Inventory files must be private regular files without hard links".into());
    }
    let file = private_options()
        .read(true)
        .write(writable)
        .open(path)
        .map_err(|error| format!("Cannot open inventory file: {error}"))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if opened.dev() != metadata.dev()
        || opened.ino() != metadata.ino()
        || !opened.is_file()
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
        .map_err(|error| format!("Cannot create inventory file: {error}"))
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot inspect inventory state: {error}")),
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
    clean_path(directory)?;
    reject_symlinks(directory)?;
    // create, not create_dir_all or chmod: existing/legacy directories are inert.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|error| format!("Inventory creation requires a new directory: {error}"))?;
    creation_checkpoint(1)?;
    let lock = new_file(&directory.join(".lock"))?;
    lock.try_lock()
        .map_err(|error| format!("Cannot lock inventory: {error}"))?;
    creation_checkpoint(2)?;
    let mut bytes = Vec::from(ID_MAGIC.as_slice());
    bytes.extend_from_slice(&identity);
    bytes.extend_from_slice(&sha256(&bytes));
    let mut id_file = new_file(&directory.join("identity.bin"))?;
    creation_checkpoint(6)?;
    id_file
        .write_all(&bytes)
        .and_then(|()| sync_file(&id_file))
        .map_err(|error| error.to_string())?;
    creation_checkpoint(3)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory.join("objects"))
        .map_err(|error| error.to_string())?;
    sync_directory(&directory.join("objects"))?;
    creation_checkpoint(4)?;
    let header = Header::empty(identity);
    let mut tree = new_file(&directory.join("tree.bin"))?;
    creation_checkpoint(7)?;
    tree.write_all(&header.encode())
        .and_then(|()| sync_file(&tree))
        .map_err(|error| error.to_string())?;
    creation_checkpoint(5)?;
    sync_file(&lock).map_err(|error| error.to_string())?;
    sync_directory(directory)?;
    sync_directory(
        directory
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    Ok(Index {
        directory: directory.to_owned(),
        _lock: lock,
        tree,
        header,
        cache: tree::Cache::new(),
        read_only: false,
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
    lock.try_lock().map_err(|error| {
        format!("Inventory storage is already open or cannot be locked: {error}")
    })?;
    private_directory(&directory.join("objects"))?;
    let mut tree = private_file(&directory.join("tree.bin"), !read_only)?;
    let published = directory.join("transaction.bin");
    let staging = directory.join("transaction.tmp");
    if present(&published)? {
        if read_only {
            return Err(
                "Inventory has a pending transaction; writable recovery is required".into(),
            );
        }
        let mut wal = private_file(&published, false)?;
        let bytes = read_bounded(&mut wal, MAX_WAL_BYTES + 48)?;
        let transaction = Transaction::decode(&bytes, identity)?;
        transaction.validate_overlay(&mut tree)?;
        preflight(directory, &transaction)?;
        apply(
            directory,
            &mut tree,
            &transaction,
            true,
            #[cfg(test)]
            None,
        )?;
        fs::remove_file(&published)
            .map_err(|error| format!("Cannot retire inventory WAL: {error}"))?;
        sync_directory(directory)?;
    }
    let header = Header::decode(&tree::read_page(&mut tree, 0)?)?;
    if header.identity != identity
        || tree.metadata().map_err(|error| error.to_string())?.len() != tree::offset(header.next)?
    {
        return Err("Inventory identity or file length mismatch".into());
    }
    // Verify the reachable root without walking the entire inventory at open.
    if header.root != 0 {
        Node::decode(
            &tree::read_page(&mut tree, header.root)?,
            header.root,
            header.level,
            header.next,
            identity,
        )?;
    }
    if present(&staging)? {
        if read_only {
            return Err("Inventory has unpublished staging; writable recovery is required".into());
        }
        private_file(&staging, false)?;
        fs::remove_file(&staging).map_err(|error| error.to_string())?;
        sync_directory(directory)?;
    }
    Ok(Index {
        directory: directory.to_owned(),
        _lock: lock,
        tree,
        header,
        cache: tree::Cache::new(),
        read_only,
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

fn read_bounded(file: &mut File, maximum: usize) -> Result<Vec<u8>> {
    if file.metadata().map_err(|error| error.to_string())?.len() > maximum as u64 {
        return Err("Inventory file exceeds its byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > maximum {
        return Err("Inventory file grew beyond its byte limit".into());
    }
    Ok(bytes)
}

fn value_path(directory: &Path, key: &Key) -> PathBuf {
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
    key: &Key,
    reference: ValueRef,
) -> Result<Vec<u8>> {
    if reference.inline {
        return Ok(reference.inline_data().to_vec());
    }
    if reference.segment {
        let (number, offset) = reference.location();
        if number < header.tail || number >= header.head {
            return Err("Inventory record names a segment outside the value log".into());
        }
        let mut segment = Segment::open(directory, header.identity, number)?;
        let (stored, data) = segment
            .envelope(u64::from(offset))?
            .ok_or("Inventory record lies past the end of its segment")?;
        if stored != *key
            || data.len() != reference.length as usize
            || sha256(&data)[..20] != reference.digest[12..]
        {
            return Err("Inventory record identity, length or checksum mismatch".into());
        }
        return Ok(data);
    }
    check_value_path(directory, key)?;
    let mut file = private_file(&value_path(directory, key), false)?;
    let bytes = read_bounded(&mut file, reference.length as usize + 108)?;
    let (stored, data) = open_envelope(&bytes)?;
    if stored != *key || data.len() != reference.length as usize || sha256(data) != reference.digest
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

/// Checks a complete envelope and returns its key and value.
fn open_envelope(bytes: &[u8]) -> Result<(Key, &[u8])> {
    if bytes.len() < ENVELOPE_BYTES as usize
        || &bytes[..8] != VALUE_MAGIC
        || u32::from_le_bytes(
            bytes[72..76]
                .try_into()
                .map_err(|_| "Invalid object length")?,
        ) as usize
            != bytes.len() - ENVELOPE_BYTES as usize
        || sha256(&bytes[..bytes.len() - 32]).as_slice() != &bytes[bytes.len() - 32..]
    {
        return Err("Inventory record identity, length or checksum mismatch".into());
    }
    let key = bytes[8..72].try_into().map_err(|_| "Invalid record key")?;
    Ok((key, &bytes[76..bytes.len() - 32]))
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

impl Segment {
    pub(super) fn open(directory: &Path, identity: [u8; 32], number: u64) -> Result<Self> {
        private_directory(&directory.join("segments"))?;
        let mut file = private_file(&segment_path(directory, number), false)?;
        let length = file.metadata().map_err(|error| error.to_string())?.len();
        let mut header = [0; SEGMENT_HEADER_BYTES as usize];
        file.read_exact(&mut header)
            .map_err(|error| format!("Cannot read inventory segment: {error}"))?;
        if header[..] != segment_header(identity, number)[..] {
            return Err("Inventory segment identity or checksum mismatch".into());
        }
        Ok(Self { file, length })
    }

    /// The envelope at `offset`, or `None` at the exact end of the segment.
    pub(super) fn envelope(&mut self, offset: u64) -> Result<Option<(Key, Vec<u8>)>> {
        if offset == self.length {
            return Ok(None);
        }
        let mut prefix = [0; 76];
        if offset < SEGMENT_HEADER_BYTES || offset > self.length - ENVELOPE_BYTES.min(self.length) {
            return Err("Inventory segment offset out of range".into());
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(&mut prefix))
            .map_err(|error| format!("Cannot read inventory segment: {error}"))?;
        let length = u32::from_le_bytes(
            prefix[72..76]
                .try_into()
                .map_err(|_| "Invalid object length")?,
        ) as u64;
        if length > MAX_VALUE_BYTES as u64 || offset + ENVELOPE_BYTES + length > self.length {
            return Err("Inventory segment envelope exceeds its segment".into());
        }
        let mut bytes = vec![0; (ENVELOPE_BYTES + length) as usize];
        bytes[..76].copy_from_slice(&prefix);
        self.file
            .read_exact(&mut bytes[76..])
            .map_err(|error| format!("Cannot read inventory segment: {error}"))?;
        let (key, data) = open_envelope(&bytes)?;
        Ok(Some((key, data.to_vec())))
    }
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
    for action in &transaction.actions {
        check_value_path(directory, &action.key)?;
    }
    Ok(())
}

fn check_segment_path(directory: &Path, number: u64) -> Result<()> {
    let path = segment_path(directory, number);
    if present(&path)? {
        private_file(&path, false)?;
    }
    Ok(())
}

pub(super) fn publish_and_apply(
    index: &mut Index,
    transaction: &Transaction,
    bytes: &[u8],
) -> Result<()> {
    transaction.validate_overlay(&mut index.tree)?;
    preflight(&index.directory, transaction)?;
    let staging = index.directory.join("transaction.tmp");
    let published = index.directory.join("transaction.bin");
    if present(&staging)? || present(&published)? {
        return Err("Inventory recovery artifacts already exist; reopen storage".into());
    }
    // Any error from creating the staging file onward has an uncertain outcome.
    index.poisoned = true;
    let mut wal = new_file(&staging)?;
    wal.write_all(bytes)
        .and_then(|()| sync_file(&wal))
        .map_err(|error| format!("Cannot stage inventory transaction: {error}"))?;
    #[cfg(test)]
    if index.fault == Some(Fault::Staged) {
        return Err("Injected interruption before WAL publication".into());
    }
    fs::rename(&staging, &published)
        .map_err(|error| format!("Cannot publish inventory WAL: {error}"))?;
    sync_directory(&index.directory)?;
    #[cfg(test)]
    if index.fault == Some(Fault::Published) {
        return Err("Injected interruption after WAL publication".into());
    }
    apply(
        &index.directory,
        &mut index.tree,
        transaction,
        false,
        #[cfg(test)]
        index.fault,
    )?;
    fs::remove_file(&published).map_err(|error| format!("Cannot retire inventory WAL: {error}"))?;
    #[cfg(test)]
    if index.fault == Some(Fault::Unlinked) {
        return Err("Injected interruption before WAL retirement sync".into());
    }
    sync_directory(&index.directory)?;
    index.poisoned = false;
    Ok(())
}

fn apply(
    directory: &Path,
    tree: &mut File,
    transaction: &Transaction,
    recovering: bool,
    #[cfg(test)] fault: Option<Fault>,
) -> Result<()> {
    let mut affected = BTreeSet::new();
    // Recovery cannot tell whether an interrupted apply made its new object
    // directories durable, so it always synchronizes `objects`.
    let mut created = recovering;
    write_segment(directory, transaction)?;
    for (number, action) in transaction.actions.iter().enumerate() {
        let path = value_path(directory, &action.key);
        let parent = path.parent().ok_or("Missing inventory object directory")?;
        // Only WALs from before the value log carry record-file puts.
        if let Some(data) = action
            .data
            .as_ref()
            .filter(|_| !action.inline && action.segment.is_none())
        {
            if !present(parent)? {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(parent)
                    .map_err(|error| {
                        format!("Cannot create inventory object directory: {error}")
                    })?;
                created = true;
            }
            private_directory(parent)?;
            let temporary = parent.join(format!(".record-{}.tmp", hex(&random_bytes::<16>()?)));
            let mut owns_temporary = false;
            let result = (|| -> Result<()> {
                let mut file = new_file(&temporary)?;
                owns_temporary = true;
                file.write_all(&envelope(&action.key, data))
                    .and_then(|()| sync_file(&file))
                    .map_err(|error| format!("Cannot persist inventory record: {error}"))?;
                fs::rename(&temporary, &path)
                    .map_err(|error| format!("Cannot publish inventory record: {error}"))?;
                Ok(())
            })();
            if result.is_err() && owns_temporary {
                let _ = fs::remove_file(&temporary);
            }
            result?;
            affected.insert(parent.to_owned());
        } else if present(&path)? {
            fs::remove_file(&path)
                .map_err(|error| format!("Cannot delete inventory record: {error}"))?;
            affected.insert(parent.to_owned());
        }
        #[cfg(test)]
        if number == 0 && fault == Some(Fault::FirstRecord) {
            return Err("Injected interruption during record apply".into());
        }
        #[cfg(not(test))]
        let _ = number;
    }
    for path in affected {
        sync_directory(&path)?;
    }
    if created {
        sync_directory(&directory.join("objects"))?;
    }
    for (number, (id, node)) in transaction.pages.iter().enumerate() {
        tree.seek(SeekFrom::Start(tree::offset(*id)?))
            .and_then(|_| tree.write_all(&node.encode().map_err(std::io::Error::other)?))
            .map_err(|error| format!("Cannot apply inventory page: {error}"))?;
        #[cfg(test)]
        if number == 0 && fault == Some(Fault::FirstPage) {
            return Err("Injected interruption during page apply".into());
        }
        #[cfg(not(test))]
        let _ = number;
    }
    tree.set_len(tree::offset(transaction.after.next)?)
        .map_err(|error| error.to_string())?;
    tree.seek(SeekFrom::Start(0))
        .and_then(|_| tree.write_all(&transaction.after.encode()))
        .and_then(|()| sync_file(tree))
        .map_err(|error| format!("Cannot persist inventory root: {error}"))?;
    // The durable root no longer references segments the cleaner emptied.
    if transaction.after.tail > transaction.before.tail {
        for number in transaction.before.tail..transaction.after.tail {
            let path = segment_path(directory, number);
            if present(&path)? {
                private_file(&path, false)?;
                fs::remove_file(&path)
                    .map_err(|error| format!("Cannot retire inventory segment: {error}"))?;
            }
        }
        sync_directory(&directory.join("segments"))?;
    }
    #[cfg(test)]
    if fault == Some(Fault::Applied) {
        return Err("Injected interruption after durable apply".into());
    }
    Ok(())
}

/// Writes the transaction's value-log segment, if it has one, under a
/// temporary name, flushes it once and publishes it at its number. Replay
/// rebuilds identical bytes from the WAL.
fn write_segment(directory: &Path, transaction: &Transaction) -> Result<()> {
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
        bytes.extend_from_slice(&envelope(&action.key, data));
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let segments = directory.join("segments");
    if !present(&segments)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&segments)
            .map_err(|error| format!("Cannot create inventory segments: {error}"))?;
        sync_directory(directory)?;
    }
    private_directory(&segments)?;
    let temporary = segments.join(format!(".segment-{}.tmp", hex(&random_bytes::<16>()?)));
    let mut owns_temporary = false;
    let result = (|| -> Result<()> {
        let mut file = new_file(&temporary)?;
        owns_temporary = true;
        file.write_all(&bytes)
            .and_then(|()| sync_file(&file))
            .map_err(|error| format!("Cannot persist inventory segment: {error}"))?;
        check_segment_path(directory, number)?;
        fs::rename(&temporary, segment_path(directory, number))
            .map_err(|error| format!("Cannot publish inventory segment: {error}"))?;
        sync_directory(&segments)
    })();
    if result.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    result
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
}
