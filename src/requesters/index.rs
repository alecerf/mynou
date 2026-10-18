//! Disk-resident primary demand records and transactionally derived indexes.
use super::{Demand, State, valid_digest};
use crate::{
    Result,
    crypto::{Sha256, sha256},
    integrations::SweepCursor,
    inventory::{Change, Index, Key, MAX_PAGE_BYTES, MAX_PAGE_ROWS, MAX_VALUE_BYTES, Query},
    json,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const HEADER: Key = [0; 64];
pub(super) const FORMAT: &[u8] = b"MYNOURI1";
const DEMAND: u8 = 1;
const ACCOUNT: u8 = 2;
const QUEUE: u8 = 3;
const MEDIA: u8 = 4;
const LINKED: u8 = 5;
const IRC: u8 = 6;
const OPERATOR: u8 = 7;
const COUNT: u8 = 8;
const CHUNK: u8 = 9;
pub(super) const SOURCE: Key = key_const(10);
pub(super) const CURSOR: Key = key_const(11);
pub(super) const OPERATOR_COUNT: Key = key_const(12);
const POLL: u8 = 13;
const ORIGIN: u8 = 14;
const CLEAN: u8 = 15;
const PLEX: u8 = 16;
pub(super) const ADMISSION_CURSOR: Key = key_const(17);
pub(super) const OUTCOME_CURSOR: Key = key_const(18);
const SWEEP: u8 = 19;
const COLLECT: u8 = 20;
const SWEEP_FORMAT: &[u8; 8] = b"MYNOUS01";
const SWEEP_LEN: usize = 8 + 8 + SweepCursor::ENCODED_LEN + 32 + 8;
/// Index rows of stale origin generations examined per staged commit.
const COLLECT_ROWS: usize = 2 * MAX_PAGE_ROWS;
pub(super) fn account_cursor(account: &str, id: &str) -> Result<Key> {
    Ok(prefix(ACCOUNT, &[&short(account), &raw(id)?]))
}
pub(super) fn primary_cursor(id: &str) -> Result<Key> {
    Ok(prefix(DEMAND, &[&raw(id)?]))
}
pub(super) fn queue_cursor(state: &str, demand: &Demand) -> Result<Key> {
    Ok(prefix(
        QUEUE,
        &[
            &[state_code(state)?],
            &demand.request.season.to_be_bytes(),
            &demand.request.episode.to_be_bytes(),
            &raw(&demand.id)?,
        ],
    ))
}
const MAX_RECORD: usize = 16 * 1024 * 1024;

const fn key_const(table: u8) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    key
}
pub(super) fn prefix(table: u8, parts: &[&[u8]]) -> Key {
    let mut key = key_const(table);
    let mut at = 1;
    for part in parts {
        key[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    key
}
fn raw(id: &str) -> Result<[u8; 32]> {
    if !valid_digest(id) {
        return Err("Requester: invalid demand identity".into());
    }
    let mut value = [0; 32];
    for (i, byte) in value.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16)
            .map_err(|_| "Requester: invalid demand identity")?;
    }
    Ok(value)
}
fn short(value: &str) -> [u8; 16] {
    sha256(value.as_bytes())[..16]
        .try_into()
        .expect("fixed digest width")
}
fn job_raw(id: &str) -> Result<[u8; 16]> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Requester: invalid job identity".into());
    }
    let mut value = [0; 16];
    for (i, byte) in value.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16)
            .map_err(|_| "Requester: invalid job identity")?;
    }
    Ok(value)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(super) fn state_code(state: &str) -> Result<u8> {
    match state {
        "reserved" => Ok(0),
        "pending" => Ok(1),
        "conflict" => Ok(2),
        "active" => Ok(3),
        "ready" => Ok(4),
        "removed" => Ok(5),
        _ => Err("Requester: invalid demand state".into()),
    }
}
pub(super) fn counter(account: &str, state: u8) -> Key {
    prefix(COUNT, &[&short(account), &[state]])
}
pub(super) fn decode_count(bytes: Option<Vec<u8>>) -> Result<u64> {
    match bytes {
        None => Ok(0),
        Some(bytes) => Ok(u64::from_le_bytes(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| "Requester: invalid counter")?,
        )),
    }
}
pub(super) fn count(index: &mut Index, account: &str, state: u8) -> Result<u64> {
    decode_count(index.get(&counter(account, state))?)
}

/// Durable progress of one account's watchlist sweep. Origins seen by the
/// sweep are staged under `epoch`, which is published only when the sweep
/// reaches the end of a listing that did not change while it was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sweep {
    pub epoch: u64,
    pub cursor: SweepCursor,
    /// Chained digest of the origins in the order the sweep saw them.
    pub digest: [u8; 32],
    pub started_at: u64,
    /// A sweep resumed in another polling pass must confirm its full digest.
    pub multipass: bool,
    /// Completed sweep digest awaiting an independent complete traversal.
    pub verification: Option<[u8; 32]>,
}
impl Sweep {
    pub(crate) fn new(epoch: u64, started_at: u64) -> Self {
        Self {
            epoch,
            cursor: SweepCursor::start(),
            digest: [0; 32],
            started_at,
            multipass: false,
            verification: None,
        }
    }
    pub(crate) fn observe(&mut self, origin: &str) {
        let mut hash = Sha256::new();
        hash.update(&self.digest);
        hash.update(&(origin.len() as u64).to_be_bytes());
        hash.update(origin.as_bytes());
        self.digest = hash.finalize();
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(SWEEP_LEN);
        bytes.extend_from_slice(SWEEP_FORMAT);
        bytes.extend_from_slice(&self.epoch.to_be_bytes());
        bytes.extend_from_slice(&self.cursor.encode());
        bytes.extend_from_slice(&self.digest);
        bytes.extend_from_slice(&self.started_at.to_be_bytes());
        bytes.push(u8::from(self.multipass));
        bytes.push(u8::from(self.verification.is_some()));
        bytes.extend_from_slice(&self.verification.unwrap_or([0; 32]));
        bytes
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let invalid = || "Requester: invalid watchlist sweep".to_owned();
        if ![SWEEP_LEN, SWEEP_LEN + 34].contains(&bytes.len()) || &bytes[..8] != SWEEP_FORMAT {
            return Err(invalid());
        }
        let (multipass, verification) = if bytes.len() == SWEEP_LEN {
            // Existing cursors may already span several passes. Confirm them.
            (true, None)
        } else {
            let extra = &bytes[SWEEP_LEN..];
            if extra[0] > 1
                || extra[1] > 1
                || (extra[1] == 0 && extra[2..].iter().any(|byte| *byte != 0))
            {
                return Err(invalid());
            }
            (
                extra[0] == 1,
                (extra[1] == 1).then(|| extra[2..].try_into().expect("fixed digest width")),
            )
        };
        let cursor_end = 16 + SweepCursor::ENCODED_LEN;
        let sweep = Self {
            epoch: u64::from_be_bytes(bytes[8..16].try_into().map_err(|_| invalid())?),
            cursor: SweepCursor::decode(&bytes[16..cursor_end])?,
            digest: bytes[cursor_end..cursor_end + 32]
                .try_into()
                .map_err(|_| invalid())?,
            started_at: u64::from_be_bytes(
                bytes[cursor_end + 32..SWEEP_LEN]
                    .try_into()
                    .map_err(|_| invalid())?,
            ),
            multipass,
            verification,
        };
        if sweep.epoch == 0 {
            return Err(invalid());
        }
        Ok(sweep)
    }
}
/// What a staged commit does with the account's sweep.
pub(crate) enum Progress {
    /// Records how far the sweep got; a restarted sweep uses a new epoch.
    Continue(Sweep),
    /// The sweep completed consistently: its epoch becomes the published
    /// generation, which authorizes removing every origin it did not see.
    Publish(Sweep),
}
/// One bounded batch of a watchlist sweep.
pub(crate) struct Stage<'a> {
    pub account: &'a str,
    pub origins: &'a BTreeSet<String>,
    pub progress: Progress,
}
pub(super) fn sweep(index: &mut Index, account: &str) -> Result<Option<Sweep>> {
    index
        .get(&prefix(SWEEP, &[&short(account)]))?
        .map(|bytes| Sweep::decode(&bytes))
        .transpose()
}
fn published(index: &mut Index, account: &str) -> Result<Option<[u8; 8]>> {
    index
        .get(&prefix(POLL, &[&short(account)]))?
        .map(|epoch| {
            epoch
                .as_slice()
                .try_into()
                .map_err(|_| "Requester: invalid completed poll generation".to_owned())
        })
        .transpose()
}
/// Deletes origin rows of generations that neither the published poll nor the
/// running sweep uses, resuming a rotating cursor so that each commit examines
/// a bounded number of rows.
fn collect(
    index: &mut Index,
    account: &str,
    keep: &[[u8; 8]],
    changes: &mut BTreeMap<Key, Option<Vec<u8>>>,
) -> Result<()> {
    let account_key = short(account);
    let marker = prefix(COLLECT, &[&account_key]);
    let (start, end) = range(ORIGIN, &[&account_key]);
    let mut after = match index.get(&marker)? {
        None => None,
        Some(bytes) => {
            let key: Key = bytes
                .as_slice()
                .try_into()
                .map_err(|_| "Requester: invalid origin collection cursor")?;
            if key < start || key >= end {
                return Err("Requester: invalid origin collection cursor".into());
            }
            Some(key)
        }
    };
    let mut examined = 0;
    while examined < COLLECT_ROWS {
        let page = index.page(&Query {
            start,
            end: Some(end),
            after,
            limit: MAX_PAGE_ROWS.min(COLLECT_ROWS - examined),
            sequence: None,
        })?;
        examined += page.entries.len().max(1);
        for (key, _) in &page.entries {
            if !keep.iter().any(|epoch| key[17..25] == epoch[..]) {
                changes.entry(*key).or_insert(None);
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    changes.insert(marker, after.map(|key| key.to_vec()));
    Ok(())
}

/// Owned entries of one record. The primary manifest binds all checked chunks.
pub(super) fn entries(demand: &Demand) -> Result<BTreeMap<Key, Vec<u8>>> {
    Demand::from_json(&demand.to_json())?;
    let id = raw(&demand.id)?;
    let bytes = json::stringify(&demand.to_json()).into_bytes();
    if bytes.len() > MAX_RECORD {
        return Err("Requester: demand exceeds the historical 16 MiB record bound".into());
    }
    let mut entries = BTreeMap::new();
    let mut manifest = Vec::from(FORMAT);
    manifest.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    manifest.extend_from_slice(&sha256(&bytes));
    entries.insert(prefix(DEMAND, &[&id]), manifest);
    for (number, chunk) in bytes.chunks(MAX_VALUE_BYTES).enumerate() {
        entries.insert(
            prefix(CHUNK, &[&id, &(number as u32).to_be_bytes()]),
            chunk.to_vec(),
        );
    }
    entries.insert(
        prefix(ACCOUNT, &[&short(&demand.account_id), &id]),
        Vec::new(),
    );
    let state = state_code(&demand.state)?;
    if state != 5 {
        entries.insert(
            prefix(
                QUEUE,
                &[
                    &[state],
                    &demand.request.season.to_be_bytes(),
                    &demand.request.episode.to_be_bytes(),
                    &id,
                ],
            ),
            Vec::new(),
        );
    }
    if matches!(demand.state.as_str(), "reserved" | "active" | "ready") {
        entries.insert(
            prefix(MEDIA, &[&short(&demand.request.media_key()), &id]),
            Vec::new(),
        );
    }
    if let Some(job) = &demand.job_id {
        entries.insert(prefix(LINKED, &[&job_raw(job)?, &id]), Vec::new());
    }
    if demand
        .origins
        .iter()
        .any(|origin| origin.starts_with("irc:"))
    {
        entries.insert(prefix(IRC, &[&id]), Vec::new());
    }
    if demand.state != "removed"
        && demand
            .origins
            .iter()
            .any(|origin| !origin.starts_with("irc:"))
    {
        entries.insert(prefix(PLEX, &[&short(&demand.account_id), &id]), Vec::new());
    }
    Ok(entries)
}
pub(super) fn get(index: &mut Index, id: &str) -> Result<Option<Demand>> {
    let id_bytes = raw(id)?;
    let Some(manifest) = index.get(&prefix(DEMAND, &[&id_bytes]))? else {
        return Ok(None);
    };
    if manifest.len() != 48 || &manifest[..8] != FORMAT {
        return Err("Requester: invalid demand manifest".into());
    }
    let length = u64::from_le_bytes(
        manifest[8..16]
            .try_into()
            .map_err(|_| "Requester: invalid demand length")?,
    );
    if length == 0 || length > MAX_RECORD as u64 {
        return Err("Requester: invalid demand length".into());
    }
    let mut bytes = Vec::with_capacity(length as usize);
    for number in 0..(length as usize).div_ceil(MAX_VALUE_BYTES) {
        let chunk = index
            .get(&prefix(CHUNK, &[&id_bytes, &(number as u32).to_be_bytes()]))?
            .ok_or("Requester: missing demand chunk")?;
        let wanted = (length as usize - bytes.len()).min(MAX_VALUE_BYTES);
        if chunk.len() != wanted {
            return Err("Requester: invalid demand chunk length".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if sha256(&bytes).as_slice() != &manifest[16..] {
        return Err("Requester: demand checksum differs".into());
    }
    let demand = Demand::from_json(&json::parse(
        std::str::from_utf8(&bytes).map_err(|_| "Requester: invalid demand text")?,
    )?)?;
    if demand.id != id {
        return Err("Requester: demand identity differs from index".into());
    }
    Ok(Some(demand))
}

pub(super) struct DemandPage {
    pub demands: Vec<Demand>,
    pub next: Option<Key>,
}
pub(super) fn scan(
    index: &mut Index,
    start: Key,
    end: Key,
    after: Option<Key>,
    id_at: usize,
    limit: usize,
) -> Result<DemandPage> {
    let page = index.page(&Query {
        start,
        end: Some(end),
        after,
        limit,
        sequence: None,
    })?;
    let mut demands = Vec::new();
    let mut bytes = 0;
    let mut last = None;
    let mut more = page.next.is_some();
    for (key, _) in &page.entries {
        let id = hex(key
            .get(id_at..id_at + 32)
            .ok_or("Requester: invalid secondary key")?);
        let demand = get(index, &id)?.ok_or("Requester: secondary index has no demand")?;
        let size = json::stringify(&demand.to_json()).len();
        if !demands.is_empty() && bytes + size > MAX_PAGE_BYTES {
            more = true;
            break;
        }
        bytes += size;
        demands.push(demand);
        last = Some(*key);
    }
    Ok(DemandPage {
        demands,
        next: if more { last } else { None },
    })
}
pub(super) fn range(table: u8, parts: &[&[u8]]) -> (Key, Key) {
    let start = prefix(table, parts);
    let mut end = start;
    let width = 1 + parts.iter().map(|part| part.len()).sum::<usize>();
    for i in (0..width).rev() {
        if end[i] != 255 {
            end[i] += 1;
            end[i + 1..].fill(0);
            return (start, end);
        }
    }
    unreachable!("bounded table prefix always has a successor")
}
pub(super) fn all_page(index: &mut Index, after: Option<Key>, limit: usize) -> Result<DemandPage> {
    let (start, end) = range(DEMAND, &[]);
    scan(index, start, end, after, 1, limit)
}
pub(super) fn account_page(
    index: &mut Index,
    account: &str,
    after: Option<Key>,
    limit: usize,
) -> Result<DemandPage> {
    let (start, end) = range(ACCOUNT, &[&short(account)]);
    let page = scan(index, start, end, after, 17, limit)?;
    if page.demands.iter().any(|d| d.account_id != account) {
        return Err("Requester: account index identity differs".into());
    }
    Ok(page)
}
pub(super) fn queue_page(
    index: &mut Index,
    state: &str,
    after: Option<Key>,
    limit: usize,
) -> Result<DemandPage> {
    let code = state_code(state)?;
    let (start, end) = range(QUEUE, &[&[code]]);
    let page = scan(index, start, end, after, 10, limit)?;
    if page.demands.iter().any(|d| d.state != state) {
        return Err("Requester: queue state differs".into());
    }
    Ok(page)
}
pub(super) fn irc_page(index: &mut Index, after: Option<Key>, limit: usize) -> Result<DemandPage> {
    let (start, end) = range(IRC, &[]);
    scan(index, start, end, after, 1, limit)
}
pub(super) fn has_operator(index: &mut Index, id: &str) -> Result<bool> {
    Ok(index.get(&operator_key(id)?)?.is_some())
}
pub(super) fn operator_key(id: &str) -> Result<Key> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Requester: invalid operator identity".into());
    }
    Ok(prefix(OPERATOR, &[id.as_bytes()]))
}
pub(super) fn clean_key(account: &str) -> Key {
    prefix(CLEAN, &[&short(account)])
}
pub(super) fn poll_ready(index: &mut Index, account: &str) -> Result<bool> {
    let Some(epoch) = index.get(&prefix(POLL, &[&short(account)]))? else {
        return Ok(true);
    };
    let progress = index
        .get(&clean_key(account))?
        .ok_or("Requester: completed poll has no cleanup proof")?;
    if epoch.len() != 8
        || ![9, 73].contains(&progress.len())
        || progress[..8] != epoch
        || progress[8] > 1
    {
        return Err("Requester: invalid poll cleanup proof".into());
    }
    Ok(progress[8] == 1)
}
pub(super) fn cleanup_page(index: &mut Index, account: &str) -> Result<(DemandPage, Vec<u8>)> {
    let progress = index
        .get(&clean_key(account))?
        .ok_or("Requester: missing poll cleanup proof")?;
    if ![9, 73].contains(&progress.len()) || progress[8] != 0 {
        return Err("Requester: invalid unfinished cleanup proof".into());
    }
    let after = if progress.len() == 73 {
        Some(
            progress[9..]
                .try_into()
                .map_err(|_| "Requester: invalid cleanup cursor")?,
        )
    } else {
        None
    };
    let (start, end) = range(PLEX, &[&short(account)]);
    let page = scan(index, start, end, after, 17, MAX_PAGE_ROWS)?;
    if page.demands.iter().any(|d| d.account_id != account) {
        return Err("Requester: cleanup account identity differs".into());
    }
    Ok((page, progress[..8].to_vec()))
}

/// A complete source generation authorizes removals without rewriting all
/// retained demand records. Partial/failed upstream reads never publish it.
pub(super) fn effective(index: &mut Index, mut demand: Demand) -> Result<Demand> {
    let Some(epoch) = index.get(&prefix(POLL, &[&short(&demand.account_id)]))? else {
        return Ok(demand);
    };
    let epoch: [u8; 8] = epoch
        .as_slice()
        .try_into()
        .map_err(|_| "Requester: invalid completed poll generation")?;
    let mut retained = BTreeSet::new();
    for origin in &demand.origins {
        if origin.starts_with("irc:") {
            retained.insert(origin.clone());
            continue;
        }
        if let Some(known) = index.get(&prefix(
            ORIGIN,
            &[
                &short(&demand.account_id),
                &epoch,
                &sha256(origin.as_bytes()),
            ],
        ))? {
            if known != origin.as_bytes() {
                return Err("Requester: origin index identity differs".into());
            }
            retained.insert(origin.clone());
        }
    }
    demand.origins = retained;
    if demand.origins.is_empty() {
        demand.state = "removed".into();
        demand.outcome = "removed".into();
    }
    Ok(demand)
}

pub(super) fn legacy_page(
    state: &State,
    after: Option<Key>,
    limit: usize,
    irc: bool,
) -> Result<(Vec<Demand>, Option<Key>)> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return Err("Requester: invalid page size".into());
    }
    let mut records = Vec::new();
    let mut bytes = 0;
    let mut last = None;
    for demand in state.demands.values() {
        if irc
            && !demand
                .origins
                .iter()
                .any(|origin| origin.starts_with("irc:"))
        {
            continue;
        }
        let key = prefix(if irc { IRC } else { DEMAND }, &[&raw(&demand.id)?]);
        if after.is_some_and(|after| key <= after) {
            continue;
        }
        let size = json::stringify(&demand.to_json()).len();
        if records.len() == limit || (!records.is_empty() && bytes + size > MAX_PAGE_BYTES) {
            return Ok((records, last));
        }
        bytes += size;
        records.push(demand.clone());
        last = Some(key);
    }
    Ok((records, None))
}

/// One bounded patch, with all derived entries/counters and the metadata fence.
pub(super) fn commit(
    index: &mut Index,
    metadata: &State,
    demands: &[Demand],
    operators: &[String],
) -> Result<()> {
    commit_poll(index, metadata, demands, operators, None)
}
pub(super) fn commit_poll(
    index: &mut Index,
    metadata: &State,
    demands: &[Demand],
    operators: &[String],
    stage: Option<Stage<'_>>,
) -> Result<()> {
    if demands.len() > super::MAX_BATCH_ITEMS || operators.len() > MAX_PAGE_ROWS {
        return Err("Requester: patch row bound exceeded".into());
    }
    let mut hydrated = 0_usize;
    for demand in demands {
        let size = json::stringify(&demand.to_json()).len();
        if size > MAX_RECORD || hydrated.saturating_add(size) > MAX_RECORD {
            return Err(
                "Requester: patch byte bound exceeded; split the update into smaller batches"
                    .into(),
            );
        }
        hydrated = hydrated
            .checked_add(size)
            .ok_or("Requester: patch byte count overflow")?;
    }
    let mut changes = BTreeMap::new();
    let mut backlog_changes = Vec::new();
    let mut counters = BTreeMap::<Key, u64>::new();
    let mut ids = BTreeSet::new();
    for demand in demands {
        if !ids.insert(&demand.id) {
            return Err("Requester: duplicate patch demand".into());
        }
        let account = metadata
            .accounts
            .get(&demand.account_id)
            .ok_or("Requester: orphaned demand")?;
        if demand.revision > account.revision {
            return Err("Requester: demand policy revision exceeds account".into());
        }
        crate::inventory::backlog::maintain(
            index,
            &mut backlog_changes,
            &primary_cursor(&demand.id)?,
            matches!(demand.state.as_str(), "pending" | "reserved" | "conflict"),
            crate::store::now(),
        )?;
        let before = get(index, &demand.id)?;
        if let Some(before) = &before {
            for key in entries(before)?.into_keys() {
                changes.insert(key, None);
            }
        }
        for (key, value) in entries(demand)? {
            changes.insert(key, Some(value));
        }
        for (record, add) in [(before.as_ref(), false), (Some(demand), true)] {
            if let Some(record) = record {
                for account in [&record.account_id[..], ""] {
                    for state in [state_code(&record.state)?, 255] {
                        let key = counter(account, state);
                        if let std::collections::btree_map::Entry::Vacant(entry) =
                            counters.entry(key)
                        {
                            entry.insert(decode_count(index.get(&key)?)?);
                        }
                        let count = counters
                            .get_mut(&key)
                            .ok_or("Requester: missing staged counter")?;
                        *count = if add {
                            count.checked_add(1)
                        } else {
                            count.checked_sub(1)
                        }
                        .ok_or("Requester: counter overflow or underflow")?;
                    }
                }
            }
        }
    }
    for (key, count) in counters {
        changes.insert(key, Some(count.to_le_bytes().to_vec()));
    }
    let mut operator_count = decode_count(index.get(&OPERATOR_COUNT)?)?;
    let mut seen_operators = BTreeSet::new();
    for id in operators {
        if !seen_operators.insert(id) {
            return Err("Requester: duplicate operator patch".into());
        }
        if !has_operator(index, id)? {
            operator_count = operator_count
                .checked_add(1)
                .ok_or("Requester: operator counter overflow")?;
        }
        changes.insert(operator_key(id)?, Some(Vec::new()));
    }
    if !operators.is_empty() {
        changes.insert(OPERATOR_COUNT, Some(operator_count.to_le_bytes().to_vec()));
    }
    if let Some(Stage {
        account,
        origins,
        progress,
    }) = stage
    {
        if !poll_ready(index, account)? {
            return Err("Requester: previous complete poll cleanup is pending".into());
        }
        if origins.len() > super::MAX_BATCH_ITEMS {
            return Err("Requester: poll batch origin bound exceeded".into());
        }
        let record = metadata
            .accounts
            .get(account)
            .ok_or("Requester: poll account is missing")?;
        let previous = published(index, account)?;
        let (sweep, publish) = match &progress {
            Progress::Continue(sweep) => (sweep, false),
            Progress::Publish(sweep) => (sweep, true),
        };
        // Continuing sweeps stage above the published cursor; a publication
        // moves the cursor to the sweep in the same commit.
        if sweep.epoch <= previous.map_or(0, u64::from_le_bytes)
            || (publish && record.cursor != sweep.epoch)
            || (!publish && sweep.epoch <= record.cursor)
        {
            return Err("Requester: sweep generation does not advance".into());
        }
        let staged = sweep.epoch.to_le_bytes();
        // A running sweep also adds what it sees to the published generation:
        // an origin present now must not read as removed before publication.
        let mut generations = vec![staged];
        if !publish && let Some(previous) = previous {
            generations.push(previous);
        }
        for origin in origins {
            for epoch in &generations {
                changes.insert(
                    prefix(
                        ORIGIN,
                        &[&short(account), epoch, &sha256(origin.as_bytes())],
                    ),
                    Some(origin.as_bytes().to_vec()),
                );
            }
        }
        if publish {
            changes.insert(prefix(POLL, &[&short(account)]), Some(staged.to_vec()));
            let mut cleanup = staged.to_vec();
            cleanup.push(0);
            changes.insert(clean_key(account), Some(cleanup));
            changes.insert(prefix(SWEEP, &[&short(account)]), None);
        } else {
            changes.insert(prefix(SWEEP, &[&short(account)]), Some(sweep.encode()));
        }
        collect(index, account, &generations, &mut changes)?;
    }
    let mut header = metadata.clone();
    header.demands.clear();
    header.operator_jobs.clear();
    changes.insert(
        HEADER,
        Some(json::stringify(&header.to_json()).into_bytes()),
    );
    let mut changes: Vec<_> = changes
        .into_iter()
        .map(|(key, value)| match value {
            None => Change::Delete(key),
            Some(value) if value.len() <= crate::inventory::MAX_INLINE_BYTES => {
                Change::PutInline(key, value)
            }
            Some(value) => Change::Put(key, value),
        })
        .collect();
    changes.extend(backlog_changes);
    index.commit(changes)?;
    Ok(())
}

pub(super) fn prepare_backlog(index: &mut Index) -> Result<()> {
    crate::inventory::backlog::prepare(
        index,
        prefix(DEMAND, &[]),
        prefix(DEMAND + 1, &[]),
        |index, key, _| {
            let id = hex(&key[1..33]);
            let demand =
                get(index, &id)?.ok_or("Requester: missing demand during backlog upgrade")?;
            Ok((
                matches!(demand.state.as_str(), "pending" | "reserved" | "conflict"),
                None,
            ))
        },
    )
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    #[test]
    fn confirmation_progress_round_trips_and_legacy_progress_requires_confirmation() {
        let mut sweep = Sweep::new(7, 42);
        sweep.cursor.position = 200;
        sweep.cursor.anchor = Some([3; 32]);
        sweep.observe("movie:synthetic");
        let encoded = sweep.encode();
        let legacy = Sweep::decode(&encoded[..SWEEP_LEN]).unwrap();
        assert!(legacy.multipass);
        assert_eq!(legacy.verification, None);
        assert_eq!(legacy.cursor, sweep.cursor);
        assert_eq!(legacy.digest, sweep.digest);
        sweep.multipass = true;
        sweep.verification = Some([8; 32]);
        assert_eq!(Sweep::decode(&sweep.encode()).unwrap(), sweep);
    }

    #[test]
    fn malformed_confirmation_progress_is_rejected() {
        let encoded = Sweep::new(1, 42).encode();
        for at in [SWEEP_LEN, SWEEP_LEN + 1] {
            let mut corrupt = encoded.clone();
            corrupt[at] = 2;
            assert!(Sweep::decode(&corrupt).is_err());
        }
        let mut corrupt = encoded.clone();
        corrupt[SWEEP_LEN + 2] = 1;
        assert!(Sweep::decode(&corrupt).is_err());
        assert!(Sweep::decode(&encoded[..encoded.len() - 1]).is_err());
    }
}
