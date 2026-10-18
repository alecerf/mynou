use super::{Key, MAX_VALUE_BYTES, Query};
use crate::{Result, crypto::sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{Read, Seek, SeekFrom},
};

pub(super) const PAGE_SIZE: usize = 4096;
const FANOUT: usize = 32;
/// Operation bound on the distinct pages one transaction may rewrite.
pub(super) const MAX_DIRTY_PAGES: usize = 16_384;
const NODE_MAGIC: &[u8; 8] = b"MYNOUIN1";
const COUNTED_MAGIC: &[u8; 8] = b"MYNOUIN2";
const HEADER_MAGIC: &[u8; 8] = b"MYNOUI01";

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Header {
    pub identity: [u8; 32],
    pub sequence: u64,
    pub root: u64,
    pub next: u64,
    pub count: u64,
    pub level: u8,
    /// Value log: segments `tail..head` exist; `tail_offset` is how far the
    /// cleaner has read into `tail`; byte totals count whole segment files and
    /// the envelopes still referenced by the tree.
    pub head: u64,
    pub tail: u64,
    pub tail_offset: u64,
    pub log_bytes: u64,
    pub live_bytes: u64,
}

impl Header {
    pub fn empty(identity: [u8; 32]) -> Self {
        Self {
            identity,
            sequence: 0,
            root: 0,
            next: 1,
            count: 0,
            level: 0,
            head: 0,
            tail: 0,
            tail_offset: 0,
            log_bytes: 0,
            live_bytes: 0,
        }
    }
    pub fn encode(self) -> [u8; PAGE_SIZE] {
        let mut bytes = [0; PAGE_SIZE];
        bytes[..8].copy_from_slice(HEADER_MAGIC);
        for (offset, value) in [
            (8, self.sequence),
            (16, self.root),
            (24, self.next),
            (32, self.count),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes[40] = self.level;
        bytes[41..73].copy_from_slice(&self.identity);
        for (offset, value) in [
            (73, self.head),
            (81, self.tail),
            (89, self.tail_offset),
            (97, self.log_bytes),
            (105, self.live_bytes),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        seal(&mut bytes);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        check(bytes, HEADER_MAGIC)?;
        if bytes[113..PAGE_SIZE - 32].iter().any(|byte| *byte != 0) {
            return Err("Invalid inventory header padding".into());
        }
        let header = Self {
            identity: bytes[41..73]
                .try_into()
                .map_err(|_| "Invalid inventory identity")?,
            sequence: integer(bytes, 8)?,
            root: integer(bytes, 16)?,
            next: integer(bytes, 24)?,
            count: integer(bytes, 32)?,
            level: bytes[40],
            head: integer(bytes, 73)?,
            tail: integer(bytes, 81)?,
            tail_offset: integer(bytes, 89)?,
            log_bytes: integer(bytes, 97)?,
            live_bytes: integer(bytes, 105)?,
        };
        if header.tail > header.head
            || header.live_bytes > header.log_bytes
            || (header.tail == header.head && (header.tail_offset != 0 || header.log_bytes != 0))
            || header.head.saturating_sub(header.tail)
                > header.log_bytes
                    / (super::persistence::SEGMENT_HEADER_BYTES
                        + super::persistence::ENVELOPE_BYTES)
            || (header.tail_offset != 0
                && (header.tail_offset < super::persistence::SEGMENT_HEADER_BYTES
                    || header.tail_offset > u64::from(u32::MAX)
                    || header.tail_offset > header.log_bytes))
        {
            return Err("Invalid inventory value log metadata".into());
        }
        if header.next == 0
            || header.root >= header.next
            || header.level > 63
            || (header.count == 0) != (header.root == 0)
            || (header.root == 0 && header.level != 0)
        {
            return Err("Invalid inventory root metadata".into());
        }
        offset(header.next)?;
        Ok(header)
    }
}

/// A leaf reference names a legacy record file (`length` bytes whose SHA-256
/// is `digest`), an envelope in a value-log segment (segment number, offset
/// and the first 20 bytes of the SHA-256 in `digest`), or carries at most 32
/// bytes inline in the tree page itself, zero-padded in the digest field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ValueRef {
    pub length: u32,
    pub digest: [u8; 32],
    pub inline: bool,
    pub segment: bool,
}

const INLINE_FLAG: u32 = 0x8000_0000;
const SEGMENT_FLAG: u32 = 0x4000_0000;
pub(super) const MAX_INLINE_BYTES: usize = 32;

impl ValueRef {
    pub fn file(data: &[u8]) -> Self {
        Self {
            length: data.len() as u32,
            digest: sha256(data),
            inline: false,
            segment: false,
        }
    }
    pub fn segment(number: u64, offset: u32, data: &[u8]) -> Self {
        let mut digest = [0; 32];
        digest[..8].copy_from_slice(&number.to_le_bytes());
        digest[8..12].copy_from_slice(&offset.to_le_bytes());
        digest[12..].copy_from_slice(&sha256(data)[..20]);
        Self {
            length: data.len() as u32,
            digest,
            inline: false,
            segment: true,
        }
    }
    /// Segment number and envelope offset of a segment reference.
    pub fn location(&self) -> (u64, u32) {
        (
            u64::from_le_bytes(self.digest[..8].try_into().unwrap_or_default()),
            u32::from_le_bytes(self.digest[8..12].try_into().unwrap_or_default()),
        )
    }
    pub fn inline(data: &[u8]) -> Result<Self> {
        if data.len() > MAX_INLINE_BYTES {
            return Err("Inline inventory value exceeds 32 bytes".into());
        }
        let mut digest = [0; 32];
        digest[..data.len()].copy_from_slice(data);
        Ok(Self {
            length: data.len() as u32,
            digest,
            inline: true,
            segment: false,
        })
    }
    pub fn inline_data(&self) -> &[u8] {
        &self.digest[..self.length as usize]
    }
    fn stored_length(self) -> u32 {
        if self.inline {
            self.length | INLINE_FLAG
        } else if self.segment {
            self.length | SEGMENT_FLAG
        } else {
            self.length
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Link {
    pub key: Key,
    pub page: u64,
    /// None in legacy branches, until explicitly upgraded. Never guess zero.
    pub count: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) enum Items {
    Leaf(Vec<(Key, ValueRef)>),
    Branch(Vec<Link>),
}

#[derive(Clone, Debug)]
pub(super) struct Node {
    pub identity: [u8; 32],
    pub id: u64,
    pub level: u8,
    pub items: Items,
}

impl Node {
    pub(super) fn count(&self) -> Result<Option<u64>> {
        match &self.items {
            Items::Leaf(items) => Ok(Some(items.len() as u64)),
            Items::Branch(items) => {
                let mut total = 0_u64;
                let mut complete = true;
                for link in items {
                    let Some(count) = link.count else {
                        complete = false;
                        continue;
                    };
                    if count == 0 {
                        return Err("Empty inventory subtree count".into());
                    }
                    total = total
                        .checked_add(count)
                        .ok_or("Inventory subtree count overflow")?;
                }
                Ok(complete.then_some(total))
            }
        }
    }
    fn len(&self) -> usize {
        match &self.items {
            Items::Leaf(items) => items.len(),
            Items::Branch(items) => items.len(),
        }
    }
    fn first(&self) -> Result<Key> {
        match &self.items {
            Items::Leaf(items) => items.first().map(|(key, _)| *key),
            Items::Branch(items) => items.first().map(|link| link.key),
        }
        .ok_or_else(|| "Empty inventory node".into())
    }
    fn last(&self) -> Result<Key> {
        match &self.items {
            Items::Leaf(items) => items.last().map(|(key, _)| *key),
            Items::Branch(items) => items.last().map(|link| link.key),
        }
        .ok_or_else(|| "Empty inventory node".into())
    }
    pub fn encode(&self) -> Result<[u8; PAGE_SIZE]> {
        if self.len() == 0 || self.len() > FANOUT {
            return Err("Invalid inventory node size".into());
        }
        let mut bytes = [0; PAGE_SIZE];
        let counted = matches!(&self.items, Items::Branch(items) if items.iter().any(|link| link.count.is_some()));
        bytes[..8].copy_from_slice(if counted { COUNTED_MAGIC } else { NODE_MAGIC });
        self.count()?;
        bytes[8..16].copy_from_slice(&self.id.to_le_bytes());
        bytes[16] = self.level;
        bytes[17..19].copy_from_slice(&(self.len() as u16).to_le_bytes());
        bytes[24..56].copy_from_slice(&self.identity);
        let mut cursor = 64;
        match &self.items {
            Items::Leaf(items) => {
                for (key, value) in items {
                    bytes[cursor..cursor + 64].copy_from_slice(key);
                    bytes[cursor + 64..cursor + 68]
                        .copy_from_slice(&value.stored_length().to_le_bytes());
                    bytes[cursor + 68..cursor + 100].copy_from_slice(&value.digest);
                    cursor += 100;
                }
            }
            Items::Branch(items) => {
                for link in items {
                    bytes[cursor..cursor + 64].copy_from_slice(&link.key);
                    bytes[cursor + 64..cursor + 72].copy_from_slice(&link.page.to_le_bytes());
                    if counted {
                        bytes[cursor + 72..cursor + 80]
                            .copy_from_slice(&link.count.unwrap_or(0).to_le_bytes());
                    }
                    cursor += if counted { 80 } else { 72 };
                }
            }
        }
        seal(&mut bytes);
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8], id: u64, level: u8, next: u64, identity: [u8; 32]) -> Result<Self> {
        let counted = bytes.get(..8) == Some(COUNTED_MAGIC.as_slice());
        check(bytes, if counted { COUNTED_MAGIC } else { NODE_MAGIC })?;
        if counted && level == 0 {
            return Err("Invalid counted inventory leaf".into());
        }
        let count = u16::from_le_bytes(bytes[17..19].try_into().map_err(|_| "Invalid node count")?)
            as usize;
        if integer(bytes, 8)? != id
            || bytes[16] != level
            || count == 0
            || count > FANOUT
            || bytes[19..24]
                .iter()
                .chain(bytes[56..64].iter())
                .any(|byte| *byte != 0)
            || bytes[24..56] != identity
        {
            return Err("Invalid inventory node identity, level or count".into());
        }
        let mut cursor = 64;
        let mut previous = None;
        let mut leaves = Vec::new();
        let mut branches = Vec::new();
        for _ in 0..count {
            let key: Key = bytes[cursor..cursor + 64]
                .try_into()
                .map_err(|_| "Invalid inventory key")?;
            if previous.is_some_and(|old| old >= key) {
                return Err("Unordered inventory node".into());
            }
            previous = Some(key);
            if level == 0 {
                let stored = u32::from_le_bytes(
                    bytes[cursor + 64..cursor + 68]
                        .try_into()
                        .map_err(|_| "Invalid value length")?,
                );
                let inline = stored & INLINE_FLAG != 0;
                let segment = stored & SEGMENT_FLAG != 0;
                let length = stored & !(INLINE_FLAG | SEGMENT_FLAG);
                let digest: [u8; 32] = bytes[cursor + 68..cursor + 100]
                    .try_into()
                    .map_err(|_| "Invalid value digest")?;
                if length as usize
                    > if inline {
                        MAX_INLINE_BYTES
                    } else {
                        MAX_VALUE_BYTES
                    }
                    || (inline && segment)
                    || (inline && digest[length as usize..].iter().any(|byte| *byte != 0))
                {
                    return Err("Oversized or malformed inventory value reference".into());
                }
                leaves.push((
                    key,
                    ValueRef {
                        length,
                        digest,
                        inline,
                        segment,
                    },
                ));
                cursor += 100;
            } else {
                let page = integer(bytes, cursor + 64)?;
                if page == 0 || page >= next || page == id {
                    return Err("Invalid inventory child reference".into());
                }
                let count = if counted {
                    integer(bytes, cursor + 72)?
                } else {
                    0
                };
                branches.push(Link {
                    key,
                    page,
                    count: (count != 0).then_some(count),
                });
                cursor += if counted { 80 } else { 72 };
            }
        }
        if bytes[cursor..PAGE_SIZE - 32].iter().any(|byte| *byte != 0) {
            return Err("Invalid inventory node padding".into());
        }
        let node = Self {
            identity,
            id,
            level,
            items: if level == 0 {
                Items::Leaf(leaves)
            } else {
                Items::Branch(branches)
            },
        };
        node.count()?;
        Ok(node)
    }
}

#[derive(Clone, Copy, Default)]
struct Bounds {
    upper: Option<Key>,
    lower: Option<Key>,
    count: Option<u64>,
}

/// Decoded pages that were verified against their checksum and are still
/// current. Cleared whenever a commit may have changed a page; at most
/// `CACHE_NODES` entries (about 8 MiB) are kept.
pub(super) type Cache = HashMap<u64, Node>;
pub(super) const CACHE_NODES: usize = 2048;

pub(super) struct Tree<'a> {
    pub file: &'a mut File,
    pub header: Header,
    pub dirty: BTreeMap<u64, Node>,
    cache: &'a mut Cache,
    #[cfg(test)]
    pub reads: usize,
}

impl<'a> Tree<'a> {
    pub fn new(file: &'a mut File, header: Header, cache: &'a mut Cache) -> Self {
        Self {
            file,
            header,
            dirty: BTreeMap::new(),
            cache,
            #[cfg(test)]
            reads: 0,
        }
    }
    fn node(&mut self, id: u64, level: u8, bounds: Bounds) -> Result<Node> {
        if id == 0 || id >= self.header.next || level > 63 {
            return Err("Invalid inventory tree reference".into());
        }
        let node = if let Some(node) = self.dirty.get(&id).or_else(|| self.cache.get(&id)) {
            node.clone()
        } else {
            #[cfg(test)]
            {
                self.reads += 1;
            }
            let bytes = read_page(self.file, id)?;
            let node = Node::decode(&bytes, id, level, self.header.next, self.header.identity)?;
            if self.cache.len() >= CACHE_NODES {
                self.cache.clear();
            }
            self.cache.insert(id, node.clone());
            node
        };
        if node.level != level
            || bounds
                .count
                .is_some_and(|count| node.count().ok() != Some(Some(count)))
            || bounds
                .upper
                .is_some_and(|key| node.last().ok() != Some(key))
            || bounds
                .lower
                .is_some_and(|key| node.first().is_ok_and(|first| first <= key))
        {
            return Err("Inventory child disagrees with its parent".into());
        }
        Ok(node)
    }
    pub fn find(&mut self, key: &Key) -> Result<Option<ValueRef>> {
        if self.header.root == 0 {
            return Ok(None);
        }
        let mut id = self.header.root;
        let mut bounds = Bounds::default();
        for level in (0..=self.header.level).rev() {
            let node = self.node(id, level, bounds)?;
            match node.items {
                Items::Leaf(items) => {
                    return Ok(items
                        .binary_search_by_key(key, |(key, _)| *key)
                        .ok()
                        .map(|index| items[index].1));
                }
                Items::Branch(items) => {
                    let index = items.partition_point(|link| link.key < *key);
                    let Some(link) = items.get(index) else {
                        return Ok(None);
                    };
                    id = link.page;
                    bounds = Bounds {
                        count: link.count,
                        upper: Some(link.key),
                        lower: if index == 0 {
                            bounds.lower
                        } else {
                            Some(items[index - 1].key)
                        },
                    };
                }
            }
        }
        Err("Invalid inventory traversal".into())
    }
    /// Count keys strictly below a boundary, following one tree path.
    fn rank(&mut self, key: &Key) -> Result<u64> {
        let mut id = self.header.root;
        let mut bounds = Bounds {
            count: Some(self.header.count),
            ..Bounds::default()
        };
        let mut rank = 0_u64;
        for level in (0..=self.header.level).rev() {
            let node = self.node(id, level, bounds)?;
            match node.items {
                Items::Leaf(items) => {
                    return rank
                        .checked_add(items.partition_point(|(found, _)| found < key) as u64)
                        .ok_or_else(|| "Inventory rank overflow".into());
                }
                Items::Branch(items) => {
                    let mut next = None;
                    let mut lower = bounds.lower;
                    for child in items {
                        if child.key < *key {
                            rank = rank
                                .checked_add(child.count.ok_or(
                                    "Inventory range counts need an explicit writable upgrade",
                                )?)
                                .filter(|rank| *rank <= self.header.count)
                                .ok_or("Inventory rank overflow")?;
                        } else {
                            next = Some((
                                child.page,
                                Bounds {
                                    upper: Some(child.key),
                                    lower,
                                    count: child.count,
                                },
                            ));
                            break;
                        }
                        lower = Some(child.key);
                    }
                    let Some((page, child_bounds)) = next else {
                        return Ok(rank);
                    };
                    id = page;
                    bounds = child_bounds;
                }
            }
        }
        Err("Invalid inventory rank traversal".into())
    }

    pub(super) fn summary(&mut self, start: Key, end: Option<Key>) -> Result<super::RangeSummary> {
        if end.is_some_and(|end| end < start) {
            return Err("Invalid inventory summary range".into());
        }
        if self.header.root == 0 || end == Some(start) {
            return Ok(super::RangeSummary {
                count: 0,
                first: None,
            });
        }
        let root = self.node(self.header.root, self.header.level, Bounds::default())?;
        if root.count()?.is_none() {
            return Err("Inventory range counts need an explicit writable upgrade".into());
        }
        if root.count()? != Some(self.header.count) {
            return Err("Inventory subtree count disagrees with the root".into());
        }
        let lower = self.rank(&start)?;
        let upper = end.map_or(Ok(self.header.count), |end| self.rank(&end))?;
        let count = upper
            .checked_sub(lower)
            .ok_or("Invalid inventory range ranks")?;
        let first = if count == 0 {
            None
        } else {
            Some(
                self.range(
                    &Query {
                        start,
                        end,
                        limit: 1,
                        ..Query::default()
                    },
                    1,
                )?
                .first()
                .ok_or("Inventory range count disagrees with its first key")?
                .0,
            )
        };
        Ok(super::RangeSummary { count, first })
    }

    /// Only missing summaries are visited. Partial parents are saved too, so
    /// restart skips completed subtrees. At most budget + tree height branch
    /// pages are dirtied; each has at most FANOUT children.
    pub(super) fn prepare_counts(&mut self, budget: usize) -> Result<bool> {
        if self.header.root == 0 {
            return Ok(true);
        }
        let count = self.upgrade_counts(
            self.header.root,
            self.header.level,
            Bounds::default(),
            budget,
        )?;
        if count.is_some_and(|count| count != self.header.count) {
            return Err("Inventory subtree count disagrees with the root".into());
        }
        Ok(count.is_some())
    }

    /// A metadata-only redo may add counts, but may not change any branch
    /// identity, key or child pointer. A torn page is restored from the
    /// checksummed WAL as with record transactions.
    pub(super) fn validate_count_upgrade(&mut self) -> Result<()> {
        for (id, node) in &self.dirty {
            let Items::Branch(new) = &node.items else {
                return Err("Invalid inventory count-upgrade page".into());
            };
            let bytes = read_page(self.file, *id)?;
            if let Ok(old) = Node::decode(
                &bytes,
                *id,
                node.level,
                self.header.next,
                self.header.identity,
            ) {
                let Items::Branch(old) = old.items else {
                    return Err("Inventory count upgrade changed a leaf".into());
                };
                if old.len() != new.len()
                    || old.iter().zip(new).any(|(old, new)| {
                        old.key != new.key
                            || old.page != new.page
                            || old.count.is_some_and(|count| Some(count) != new.count)
                    })
                {
                    return Err("Inventory count upgrade changed existing entries".into());
                }
            }
        }
        let upgraded = self.dirty.values().cloned().collect::<Vec<_>>();
        for node in upgraded {
            let Items::Branch(links) = node.items else {
                unreachable!("checked above")
            };
            let mut lower = None;
            for child in links {
                if child.count.is_some() {
                    self.node(
                        child.page,
                        node.level - 1,
                        Bounds {
                            upper: Some(child.key),
                            lower,
                            count: child.count,
                        },
                    )?;
                }
                lower = Some(child.key);
            }
        }
        if self.header.root != 0 {
            let root = self.node(self.header.root, self.header.level, Bounds::default())?;
            if root
                .count()?
                .is_some_and(|count| count != self.header.count)
            {
                return Err("Inventory subtree count disagrees with the root".into());
            }
        }
        Ok(())
    }

    fn upgrade_counts(
        &mut self,
        id: u64,
        level: u8,
        bounds: Bounds,
        budget: usize,
    ) -> Result<Option<u64>> {
        let mut node = self.node(id, level, bounds)?;
        if let Some(count) = node.count()? {
            return Ok(Some(count));
        }
        let Items::Branch(items) = &mut node.items else {
            unreachable!("leaves have counts")
        };
        let mut changed = false;
        let mut lower = bounds.lower;
        for child in items {
            if child.count.is_none() {
                if self.dirty.len() >= budget {
                    break;
                }
                child.count = self.upgrade_counts(
                    child.page,
                    level - 1,
                    Bounds {
                        upper: Some(child.key),
                        lower,
                        count: None,
                    },
                    budget,
                )?;
                // A partially upgraded child is also durable progress, even
                // when its total is not yet known.
                changed |= child.count.is_some();
            }
            lower = Some(child.key);
        }
        let count = node.count()?;
        if changed {
            self.dirty.insert(id, node);
        }
        Ok(count)
    }
    pub fn range(&mut self, query: &Query, limit: usize) -> Result<Vec<(Key, ValueRef)>> {
        let mut rows = Vec::new();
        if self.header.root != 0 {
            self.collect(
                Link {
                    key: [255; 64],
                    page: self.header.root,
                    count: Some(self.header.count),
                },
                self.header.level,
                Bounds::default(),
                query,
                limit,
                &mut rows,
            )?;
        }
        Ok(rows)
    }
    fn collect(
        &mut self,
        link: Link,
        level: u8,
        bounds: Bounds,
        query: &Query,
        limit: usize,
        rows: &mut Vec<(Key, ValueRef)>,
    ) -> Result<()> {
        let node = self.node(link.page, level, bounds)?;
        match node.items {
            Items::Leaf(items) => {
                for (key, value) in items {
                    if key < query.start || query.after.is_some_and(|after| key <= after) {
                        continue;
                    }
                    if query.end.is_some_and(|end| key >= end) {
                        break;
                    }
                    rows.push((key, value));
                    if rows.len() == limit {
                        break;
                    }
                }
            }
            Items::Branch(items) => {
                let mut lower = bounds.lower;
                for child in items {
                    if query
                        .end
                        .is_some_and(|end| lower.is_some_and(|previous| previous >= end))
                    {
                        break;
                    }
                    if child.key >= query.start
                        && !query.after.is_some_and(|after| child.key <= after)
                    {
                        self.collect(
                            child,
                            level - 1,
                            Bounds {
                                count: child.count,
                                upper: Some(child.key),
                                lower,
                            },
                            query,
                            limit,
                            rows,
                        )?;
                        if rows.len() == limit {
                            break;
                        }
                    }
                    lower = Some(child.key);
                }
            }
        }
        Ok(())
    }
    fn allocate(&mut self) -> Result<u64> {
        let id = self.header.next;
        self.header.next = id
            .checked_add(1)
            .ok_or("Inventory page identity overflow")?;
        offset(self.header.next)?;
        Ok(id)
    }
    pub fn change(&mut self, key: &Key, value: Option<ValueRef>, existed: bool) -> Result<()> {
        let links = if self.header.root == 0 {
            if let Some(value) = value {
                let id = self.allocate()?;
                self.dirty.insert(
                    id,
                    Node {
                        identity: self.header.identity,
                        id,
                        level: 0,
                        items: Items::Leaf(vec![(*key, value)]),
                    },
                );
                vec![Link {
                    key: *key,
                    page: id,
                    count: Some(1),
                }]
            } else {
                Vec::new()
            }
        } else {
            self.edit(
                self.header.root,
                self.header.level,
                Bounds::default(),
                key,
                value,
            )?
        };
        match links.as_slice() {
            [] => {
                self.header.root = 0;
                self.header.level = 0;
            }
            [link] => {
                self.header.root = link.page;
                while self.header.level > 0 {
                    let node = self.node(self.header.root, self.header.level, Bounds::default())?;
                    let Items::Branch(items) = node.items else {
                        return Err("Invalid inventory root kind".into());
                    };
                    if items.len() != 1 {
                        break;
                    }
                    self.header.root = items[0].page;
                    self.header.level -= 1;
                }
            }
            _ => {
                let level = self
                    .header
                    .level
                    .checked_add(1)
                    .filter(|level| *level <= 63)
                    .ok_or("Inventory tree level overflow")?;
                let id = self.allocate()?;
                self.dirty.insert(
                    id,
                    Node {
                        identity: self.header.identity,
                        id,
                        level,
                        items: Items::Branch(links),
                    },
                );
                self.header.root = id;
                self.header.level = level;
            }
        }
        if self.dirty.len() > MAX_DIRTY_PAGES {
            return Err("Inventory transaction exceeds its page budget".into());
        }
        self.header.count = match (value.is_some(), existed) {
            (true, false) => self
                .header
                .count
                .checked_add(1)
                .ok_or("Inventory record count overflow")?,
            (false, true) => self
                .header
                .count
                .checked_sub(1)
                .ok_or("Inventory record count underflow")?,
            _ => self.header.count,
        };
        Ok(())
    }
    fn edit(
        &mut self,
        id: u64,
        level: u8,
        bounds: Bounds,
        key: &Key,
        value: Option<ValueRef>,
    ) -> Result<Vec<Link>> {
        let mut node = self.node(id, level, bounds)?;
        match &mut node.items {
            Items::Leaf(items) => match (items.binary_search_by_key(key, |(key, _)| *key), value) {
                (Ok(index), Some(value)) => items[index].1 = value,
                (Ok(index), None) => {
                    items.remove(index);
                }
                (Err(index), Some(value)) => items.insert(index, (*key, value)),
                (Err(_), None) => {}
            },
            Items::Branch(items) => {
                let index = items
                    .partition_point(|link| link.key < *key)
                    .min(items.len() - 1);
                let child = items[index];
                let links = self.edit(
                    child.page,
                    level - 1,
                    Bounds {
                        count: child.count,
                        upper: Some(child.key),
                        lower: if index == 0 {
                            bounds.lower
                        } else {
                            Some(items[index - 1].key)
                        },
                    },
                    key,
                    value,
                )?;
                items.splice(index..=index, links);
            }
        }
        if node.len() == 0 {
            self.dirty.remove(&id);
            return Ok(Vec::new());
        }
        let mut links = Vec::new();
        if node.len() > FANOUT {
            let new_id = self.allocate()?;
            let midpoint = node.len() / 2;
            let right = Node {
                identity: self.header.identity,
                id: new_id,
                level,
                items: match &mut node.items {
                    Items::Leaf(items) => Items::Leaf(items.split_off(midpoint)),
                    Items::Branch(items) => Items::Branch(items.split_off(midpoint)),
                },
            };
            links.push(Link {
                key: right.last()?,
                page: new_id,
                count: right.count()?,
            });
            self.dirty.insert(new_id, right);
        }
        links.insert(
            0,
            Link {
                key: node.last()?,
                page: id,
                count: node.count()?,
            },
        );
        self.dirty.insert(id, node);
        Ok(links)
    }
}

pub(super) fn offset(id: u64) -> Result<u64> {
    id.checked_mul(PAGE_SIZE as u64)
        .filter(|offset| *offset <= i64::MAX as u64)
        .ok_or_else(|| "Inventory file offset overflow".into())
}
pub(super) fn read_page(file: &mut File, id: u64) -> Result<[u8; PAGE_SIZE]> {
    let mut bytes = [0; PAGE_SIZE];
    file.seek(SeekFrom::Start(offset(id)?))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|error| format!("Cannot read inventory page: {error}"))?;
    Ok(bytes)
}
fn integer(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .map_err(|_| "Invalid inventory integer")?,
    ))
}
fn seal(bytes: &mut [u8; PAGE_SIZE]) {
    let digest = sha256(&bytes[..PAGE_SIZE - 32]);
    bytes[PAGE_SIZE - 32..].copy_from_slice(&digest);
}
fn check(bytes: &[u8], magic: &[u8; 8]) -> Result<()> {
    if bytes.len() != PAGE_SIZE
        || &bytes[..8] != magic
        || sha256(&bytes[..PAGE_SIZE - 32]).as_slice() != &bytes[PAGE_SIZE - 32..]
    {
        return Err("Invalid inventory page format or checksum".into());
    }
    Ok(())
}
