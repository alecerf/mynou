//! Series, episode and numbering records stored in the disk-resident index.
//!
//! A scope is a header record plus small blocks that hold the episodes,
//! retained numbering identities and numbering choices of 64 consecutive
//! episode numbers of one season, so one record never grows with the number
//! of series and a plan stays within its per-plan bounds. Secondary entries in
//! the same index serve scheduling, calendar windows and identity lookups
//! without enumerating the inventory. A save is one index transaction.
use super::{
    CalendarQuery, Episode, NumberingChoice, Plan, Record, flag, integer,
    migration::{self, MigrationReport},
    numbering::{catalog_id, initial_anchors},
    small, text,
};
use crate::{
    Result,
    crypto::{random_bytes, sha256},
    date,
    inventory::{Change, Index, Key, MAX_PAGE_ROWS, Page, Query},
    json::{self, Value},
    numbering::EpisodeNumber,
    selection::tokens,
    store::Request,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    rc::Rc,
};

/// Episodes sharing one block record: `episode / BLOCK` within a season.
pub(super) const BLOCK: u32 = 64;
const FORMAT: &[u8] = b"mynou-series/1";
/// Rows examined to report an exact calendar total. Beyond it the page is
/// still correct and a cursor continues it; only the count is withheld.
const COUNT_SCAN_ROWS: u64 = 20_000;

const T_META: u8 = 0x01;
const T_MIGRATION: u8 = 0x02;
const T_HEADER: u8 = 0x10;
const T_BLOCK: u8 = 0x11;
const T_CALENDAR: u8 = 0x20;
const T_DUE: u8 = 0x30;
const T_CANONICAL: u8 = 0x40;
const T_TMDB: u8 = 0x41;
const T_TITLE: u8 = 0x42;
const T_IDENTITY: u8 = 0x50;
const T_OWNER: u8 = 0x51;

pub(super) enum Backend {
    /// Nothing is stored yet; the index is created by the first write.
    Absent,
    /// A legacy snapshot exists but this open cannot migrate it (read-only).
    Unmigrated,
    Ready(Box<Index>),
}

/// The series catalog. The parent request journal owns the directory lock.
pub struct SeriesStore {
    directory: PathBuf,
    pub(super) backend: Backend,
    read_only: bool,
    poisoned: bool,
    pub(crate) migration: Option<MigrationReport>,
    pub(crate) due_pass: [usize; 3],
}

#[derive(Debug)]
pub struct SummaryPage {
    pub series: Vec<Value>,
    /// Exclusive cursor for the next page, absent after the last one.
    pub next: Option<String>,
    pub total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub series: u64,
    pub monitored: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub id: String,
    pub next_check_at: u64,
}

pub struct CalendarRow {
    /// Series scope without its episode list; numbering holds this row's choice.
    pub series: Rc<Record>,
    pub episode: Episode,
}

pub struct CalendarPage {
    pub rows: Vec<CalendarRow>,
    /// Exact when the whole window was counted within `COUNT_SCAN_ROWS`.
    pub total: Option<u64>,
    pub next: Option<String>,
}

// ---------------------------------------------------------------------------
// Keys

fn key(parts: &[&[u8]]) -> Key {
    let mut key = [0; 64];
    let mut at = 0;
    for part in parts {
        key[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    key
}

fn id_bytes(id: &str) -> Result<[u8; 16]> {
    let bytes = id.as_bytes();
    if bytes.len() != 32 || !bytes.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("Invalid series identifier".into());
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

fn id_text(bytes: &[u8]) -> String {
    super::hex(bytes)
}

fn header_key(id: &[u8; 16]) -> Key {
    key(&[&[T_HEADER], id])
}
fn block_key(id: &[u8; 16], season: u32, episode: u32) -> Key {
    key(&[
        &[T_BLOCK],
        id,
        &season.to_be_bytes(),
        &(episode / BLOCK).to_be_bytes(),
    ])
}
fn calendar_key(date: &str, id: &[u8; 16], season: u32, episode: u32) -> Key {
    key(&[
        &[T_CALENDAR],
        date.as_bytes(),
        id,
        &season.to_be_bytes(),
        &episode.to_be_bytes(),
    ])
}
fn due_key(next: u64, id: &[u8; 16]) -> Key {
    key(&[&[T_DUE], &next.to_be_bytes(), id])
}
fn canonical_key(request: &Request) -> Key {
    key(&[&[T_CANONICAL], &sha256(request.canonical_key().as_bytes())])
}
fn tmdb_key(tmdb: u64, season: u32, episode: u32, id: &[u8; 16]) -> Key {
    key(&[
        &[T_TMDB],
        &tmdb.to_be_bytes(),
        &season.to_be_bytes(),
        &episode.to_be_bytes(),
        id,
    ])
}
fn title_digest(title: &str) -> [u8; 16] {
    let digest = sha256(tokens(title).join("\u{1f}").as_bytes());
    digest[..16].try_into().expect("sixteen digest bytes")
}
fn title_key(title: &str, season: u32, episode: u32, id: &[u8; 16]) -> Key {
    key(&[
        &[T_TITLE],
        &title_digest(title),
        &season.to_be_bytes(),
        &episode.to_be_bytes(),
        id,
    ])
}
fn identity_key(tmdb: u64, catalog: u64) -> Key {
    key(&[&[T_IDENTITY], &tmdb.to_be_bytes(), &catalog.to_be_bytes()])
}
fn owner_key(tmdb: u64, number: EpisodeNumber) -> Key {
    key(&[
        &[T_OWNER],
        &tmdb.to_be_bytes(),
        &number.season.to_be_bytes(),
        &number.episode.to_be_bytes(),
    ])
}
const FORMAT_KEY: [u8; 64] = {
    let mut key = [0; 64];
    key[0] = T_META;
    key
};
const COUNTS_KEY: [u8; 64] = {
    let mut key = [0; 64];
    key[0] = T_META;
    key[1] = 1;
    key
};
const DUE_CURSOR_KEY: Key = {
    let mut key = [0; 64];
    key[0] = T_META;
    key[1] = 2;
    key
};
pub(super) const MIGRATION_SOURCE_KEY: [u8; 64] = {
    let mut key = [0; 64];
    key[0] = T_MIGRATION;
    key
};
pub(super) const MIGRATION_CURSOR_KEY: [u8; 64] = {
    let mut key = [0; 64];
    key[0] = T_MIGRATION;
    key[1] = 1;
    key
};

/// `[start, end)` covering every key that begins with `prefix`.
fn prefix_range(prefix: &[u8]) -> (Key, Option<Key>) {
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

fn scan(index: &mut Index, prefix: &[u8], after: Option<Key>, limit: usize) -> Result<Page> {
    let (start, end) = prefix_range(prefix);
    index.page(&Query {
        start,
        end,
        after,
        limit,
        sequence: None,
    })
}

// ---------------------------------------------------------------------------
// Record encoding

/// The persisted scope without its episodes, anchors and numbering choices.
pub(super) struct Head {
    pub id: String,
    pub request: Request,
    pub monitored: bool,
    pub include_specials: bool,
    pub start_date: Option<String>,
    pub excluded: BTreeSet<(u32, u32)>,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub checked_at: u64,
    pub next_check_at: u64,
    pub last_error: Option<String>,
    pub episodes: u32,
    pub undated: u32,
    pub unmapped: u32,
    pub anchors: u32,
    pub choices: u32,
}

impl Head {
    fn of(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            request: record.plan.request.clone(),
            monitored: record.monitored,
            include_specials: record.include_specials,
            start_date: record.start_date.clone(),
            excluded: record.excluded.clone(),
            revision: record.revision,
            created_at: record.created_at,
            updated_at: record.updated_at,
            checked_at: record.checked_at,
            next_check_at: record.next_check_at,
            last_error: record.last_error.clone(),
            episodes: record.plan.episodes.len() as u32,
            undated: record
                .plan
                .episodes
                .iter()
                .filter(|episode| episode.air_date.is_none())
                .count() as u32,
            unmapped: record
                .plan
                .episodes
                .iter()
                .filter(|episode| episode.catalog_id.is_none())
                .count() as u32,
            anchors: record.anchors.len() as u32,
            choices: record.numbering.len() as u32,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut value = Value::object();
        value.insert("v", 1_u32);
        value.insert("id", self.id.clone());
        value.insert("request", self.request.to_json());
        value.insert("monitored", self.monitored);
        value.insert("include_specials", self.include_specials);
        value.insert(
            "start_date",
            self.start_date.clone().map_or(Value::Null, Value::String),
        );
        value.insert(
            "excluded",
            Value::Array(
                self.excluded
                    .iter()
                    .map(|(season, episode)| {
                        Value::Array(vec![(*season).into(), (*episode).into()])
                    })
                    .collect(),
            ),
        );
        for (name, number) in [
            ("revision", self.revision),
            ("created_at", self.created_at),
            ("updated_at", self.updated_at),
            ("checked_at", self.checked_at),
            ("next_check_at", self.next_check_at),
        ] {
            value.insert(name, number.to_string());
        }
        value.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::String),
        );
        for (name, number) in [
            ("episodes", self.episodes),
            ("undated", self.undated),
            ("unmapped", self.unmapped),
            ("anchors", self.anchors),
            ("choices", self.choices),
        ] {
            value.insert(name, number);
        }
        json::stringify(&value).into_bytes()
    }

    fn decode(bytes: &[u8], expected: &str) -> Result<Self> {
        let value = json::parse(std::str::from_utf8(bytes).map_err(|_| "Invalid series header")?)?;
        super::fields(
            &value,
            &[
                "v",
                "id",
                "request",
                "monitored",
                "include_specials",
                "start_date",
                "excluded",
                "revision",
                "created_at",
                "updated_at",
                "checked_at",
                "next_check_at",
                "last_error",
                "episodes",
                "undated",
                "unmapped",
                "anchors",
                "choices",
            ],
        )?;
        if value.get("v").and_then(Value::as_u64) != Some(1) || text(&value, "id")? != expected {
            return Err("Series header version or identity mismatch".into());
        }
        let optional = |name| -> Result<Option<String>> {
            match value.get(name) {
                Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.clone())),
                _ => Err("Invalid optional series field".into()),
            }
        };
        let mut excluded = BTreeSet::new();
        let entries = value
            .get("excluded")
            .and_then(Value::as_array)
            .ok_or("Missing episode exclusions")?;
        if entries.len() > super::MAX_EPISODES {
            return Err("Too many episode exclusions".into());
        }
        for entry in entries {
            let pair = entry.as_array().ok_or("Invalid episode exclusion")?;
            let number = |value: &Value| value.as_u64().and_then(|n| u32::try_from(n).ok());
            let (Some(season), Some(episode)) =
                (pair.first().and_then(number), pair.get(1).and_then(number))
            else {
                return Err("Invalid episode exclusion".into());
            };
            if pair.len() != 2 || !excluded.insert((season, episode)) {
                return Err("Invalid episode exclusion".into());
            }
        }
        let head = Self {
            id: expected.to_owned(),
            request: Request::from_json(value.get("request").ok_or("Missing series identity")?)?,
            monitored: flag(&value, "monitored")?,
            include_specials: flag(&value, "include_specials")?,
            start_date: optional("start_date")?,
            excluded,
            revision: integer(&value, "revision")?,
            created_at: integer(&value, "created_at")?,
            updated_at: integer(&value, "updated_at")?,
            checked_at: integer(&value, "checked_at")?,
            next_check_at: integer(&value, "next_check_at")?,
            last_error: optional("last_error")?,
            episodes: small(&value, "episodes")?,
            undated: small(&value, "undated")?,
            unmapped: small(&value, "unmapped")?,
            anchors: small(&value, "anchors")?,
            choices: small(&value, "choices")?,
        };
        if [head.episodes, head.anchors, head.choices]
            .iter()
            .any(|count| *count as usize > super::MAX_EPISODES)
            || head.undated > head.episodes
            || head.unmapped > head.episodes
        {
            return Err("Invalid series header counts".into());
        }
        Ok(head)
    }

    /// A record carrying the scope's settings and no catalog content.
    pub(super) fn shell(&self) -> Record {
        Record {
            id: self.id.clone(),
            plan: Plan {
                request: self.request.clone(),
                episodes: Vec::new(),
            },
            monitored: self.monitored,
            include_specials: self.include_specials,
            start_date: self.start_date.clone(),
            excluded: self.excluded.clone(),
            revision: self.revision,
            created_at: self.created_at,
            updated_at: self.updated_at,
            checked_at: self.checked_at,
            next_check_at: self.next_check_at,
            last_error: self.last_error.clone(),
            anchors: BTreeMap::new(),
            numbering: BTreeMap::new(),
        }
    }

    fn summary(&self) -> Value {
        use crate::integrations::report_text;
        let mut value = Value::object();
        value.insert("id", self.id.clone());
        let mut request = self.request.to_json();
        request.insert("title", report_text(&self.request.title, 4096));
        value.insert("request", request);
        value.insert("monitored", self.monitored);
        value.insert("include_specials", self.include_specials);
        value.insert(
            "start_date",
            self.start_date.clone().map_or(Value::Null, Value::String),
        );
        value.insert("episode_count", self.episodes);
        value.insert("undated_count", self.undated);
        value.insert("unmapped_count", self.unmapped);
        value.insert("checked_at", self.checked_at.to_string());
        value.insert("next_check_at", self.next_check_at.to_string());
        value.insert(
            "last_error",
            self.last_error
                .as_ref()
                .map_or(Value::Null, |text| report_text(text, 2048).into()),
        );
        value
    }
}

#[derive(Default)]
struct Block {
    episodes: Vec<Episode>,
    anchors: Vec<(u64, EpisodeNumber)>,
    choices: Vec<NumberingChoice>,
}

impl Block {
    fn encode(mut self) -> Vec<u8> {
        self.episodes.sort_by_key(|episode| episode.episode);
        self.anchors.sort_by_key(|(_, number)| number.episode);
        self.choices.sort_by_key(|choice| choice.catalog_id);
        let mut value = Value::object();
        value.insert("v", 1_u32);
        value.insert(
            "episodes",
            Value::Array(self.episodes.iter().map(Episode::to_json).collect()),
        );
        value.insert(
            "anchors",
            Value::Array(
                self.anchors
                    .iter()
                    .map(|(id, number)| {
                        let mut value = number.to_json();
                        value.insert("catalog_id", Value::Number(*id as f64));
                        value
                    })
                    .collect(),
            ),
        );
        value.insert(
            "numbering",
            Value::Array(self.choices.iter().map(|choice| choice.to_json()).collect()),
        );
        json::stringify(&value).into_bytes()
    }

    fn decode(bytes: &[u8], season: u32, block: u32) -> Result<Self> {
        let value = json::parse(std::str::from_utf8(bytes).map_err(|_| "Invalid series block")?)?;
        super::fields(&value, &["v", "episodes", "anchors", "numbering"])?;
        if value.get("v").and_then(Value::as_u64) != Some(1) {
            return Err("Unsupported series block version".into());
        }
        let list = |name: &str| -> Result<&[Value]> {
            let entries = value
                .get(name)
                .and_then(Value::as_array)
                .ok_or("Invalid series block")?;
            if entries.len() > BLOCK as usize * 2 {
                return Err("Oversized series block".into());
            }
            Ok(entries)
        };
        let mut decoded = Self::default();
        for entry in list("episodes")? {
            decoded.episodes.push(Episode::from_json(entry)?);
        }
        for entry in list("anchors")? {
            super::fields(entry, &["catalog_id", "season", "episode"])?;
            let number = EpisodeNumber {
                season: small(entry, "season")?,
                episode: small(entry, "episode")?,
            };
            number.validate()?;
            decoded.anchors.push((catalog_id(entry)?, number));
        }
        for entry in list("numbering")? {
            decoded.choices.push(NumberingChoice::from_json(entry)?);
        }
        let inside =
            |number: EpisodeNumber| number.season == season && number.episode / BLOCK == block;
        let ordered = decoded
            .episodes
            .windows(2)
            .all(|pair| pair[0].episode < pair[1].episode)
            && decoded
                .anchors
                .windows(2)
                .all(|pair| pair[0].1.episode < pair[1].1.episode)
            && decoded
                .choices
                .windows(2)
                .all(|pair| pair[0].catalog_id < pair[1].catalog_id);
        if !ordered
            || decoded.episodes.iter().any(|e| {
                !inside(EpisodeNumber {
                    season: e.season,
                    episode: e.episode,
                })
            })
            || decoded.anchors.iter().any(|(_, number)| !inside(*number))
        {
            return Err("Series block content disagrees with its key".into());
        }
        Ok(decoded)
    }
}

/// How a key is stored: a record file, or a tiny value inline in the tree.
#[derive(Clone, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Inline(Vec<u8>),
}

impl Entry {
    fn change(self, key: Key) -> Change {
        match self {
            Self::File(data) => Change::Put(key, data),
            Self::Inline(data) => Change::PutInline(key, data),
        }
    }
}

fn tmdb(record: &Record) -> Result<u64> {
    record
        .plan
        .request
        .tmdb_id
        .ok_or_else(|| "Series scopes require a catalog identity".into())
}

/// Every key one record owns, with its desired content. Shared identity
/// entries are insert-only and handled separately.
fn owned(record: &Record) -> Result<BTreeMap<Key, Entry>> {
    let id = id_bytes(&record.id)?;
    let series = tmdb(record)?;
    let mut entries = BTreeMap::new();
    entries.insert(header_key(&id), Entry::File(Head::of(record).encode()));
    let mut blocks: BTreeMap<(u32, u32), Block> = BTreeMap::new();
    for episode in &record.plan.episodes {
        blocks
            .entry((episode.season, episode.episode / BLOCK))
            .or_default()
            .episodes
            .push(episode.clone());
        if let Some(date) = &episode.air_date {
            entries.insert(
                calendar_key(date, &id, episode.season, episode.episode),
                Entry::Inline(Vec::new()),
            );
        }
    }
    for (catalog, number) in &record.anchors {
        blocks
            .entry((number.season, number.episode / BLOCK))
            .or_default()
            .anchors
            .push((*catalog, *number));
    }
    for choice in record.numbering.values() {
        let number = record
            .anchors
            .get(&choice.catalog_id)
            .ok_or("Numbering choice requires a retained catalog identity")?;
        blocks
            .entry((number.season, number.episode / BLOCK))
            .or_default()
            .choices
            .push(*choice);
    }
    for ((season, block), content) in blocks {
        entries.insert(
            block_key(&id, season, block * BLOCK),
            Entry::File(content.encode()),
        );
    }
    if record.monitored {
        entries.insert(
            due_key(record.next_check_at, &id),
            Entry::Inline(Vec::new()),
        );
    }
    entries.insert(
        canonical_key(&record.plan.request),
        Entry::Inline(id.to_vec()),
    );
    entries.insert(
        tmdb_key(
            series,
            record.plan.request.season,
            record.plan.request.episode,
            &id,
        ),
        Entry::Inline(Vec::new()),
    );
    entries.insert(
        title_key(
            &record.plan.request.title,
            record.plan.request.season,
            record.plan.request.episode,
            &id,
        ),
        Entry::Inline(Vec::new()),
    );
    Ok(entries)
}

fn number_bytes(number: EpisodeNumber) -> Vec<u8> {
    let mut bytes = number.season.to_le_bytes().to_vec();
    bytes.extend_from_slice(&number.episode.to_le_bytes());
    bytes
}

fn counts(index: &mut Index) -> Result<Counts> {
    let bytes = index
        .get(&COUNTS_KEY)?
        .ok_or("Series index is missing its counters")?;
    let [series, monitored] = [0, 8].map(|at| {
        bytes
            .get(at..at + 8)
            .and_then(|slice| <[u8; 8]>::try_from(slice).ok())
            .map(u64::from_le_bytes)
    });
    match (series, monitored, bytes.len()) {
        (Some(series), Some(monitored), 16) if monitored <= series => {
            Ok(Counts { series, monitored })
        }
        _ => Err("Invalid series index counters".into()),
    }
}

fn counts_change(counts: Counts) -> Change {
    let mut bytes = counts.series.to_le_bytes().to_vec();
    bytes.extend_from_slice(&counts.monitored.to_le_bytes());
    Change::PutInline(COUNTS_KEY, bytes)
}

/// Writes the format marker and zero counters into an empty index.
pub(super) fn initialize(index: &mut Index) -> Result<()> {
    index.commit(vec![
        Change::PutInline(FORMAT_KEY, FORMAT.to_vec()),
        counts_change(Counts {
            series: 0,
            monitored: 0,
        }),
    ])?;
    Ok(())
}

/// Confirms the format marker of an opened index. An index that was created
/// but never initialized (interrupted creation) is completed when writable.
pub(super) fn check_format(index: &mut Index, read_only: bool) -> Result<()> {
    match index.get(&FORMAT_KEY)? {
        Some(value) if value == FORMAT => counts(index).map(|_| ()),
        Some(_) => Err("Unsupported series index format".into()),
        None if index.is_empty()? && !read_only => initialize(index),
        None => Err("Series index is missing its format marker".into()),
    }
}

pub(super) fn load_head(index: &mut Index, id: &str) -> Result<Option<Head>> {
    let id_key = header_key(&id_bytes(id)?);
    index
        .get(&id_key)?
        .map(|bytes| Head::decode(&bytes, id))
        .transpose()
}

pub(super) fn load_record(index: &mut Index, id: &str) -> Result<Option<Record>> {
    let raw = id_bytes(id)?;
    let Some(head) = load_head(index, id)? else {
        return Ok(None);
    };
    let mut record = head.shell();
    let mut after = None;
    loop {
        let page = scan(
            index,
            &[&[T_BLOCK][..], &raw].concat(),
            after,
            MAX_PAGE_ROWS,
        )?;
        for (found, bytes) in &page.entries {
            let season = u32::from_be_bytes(found[17..21].try_into().expect("four bytes"));
            let block = u32::from_be_bytes(found[21..25].try_into().expect("four bytes"));
            let content = Block::decode(bytes, season, block)?;
            record.plan.episodes.extend(content.episodes);
            for (catalog, number) in content.anchors {
                record.anchors.insert(catalog, number);
            }
            for choice in content.choices {
                record.numbering.insert(choice.catalog_id, choice);
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    if record.plan.episodes.len() != head.episodes as usize
        || record.anchors.len() != head.anchors as usize
        || record.numbering.len() != head.choices as usize
    {
        return Err("Series records disagree with their header".into());
    }
    record.validate()?;
    Ok(Some(record))
}

/// Commits one record and its index entries atomically, enforcing the
/// scope and identity invariants. `extra` rides in the same transaction.
pub(super) fn save_record(index: &mut Index, record: &Record, extra: Vec<Change>) -> Result<()> {
    record.validate()?;
    let id = id_bytes(&record.id)?;
    let series = tmdb(record)?;
    let previous = load_record(index, &record.id)?;
    let wanted = owned(record)?;
    let current = previous
        .as_ref()
        .map(owned)
        .transpose()?
        .unwrap_or_default();
    let mut changes = Vec::new();
    for (key, entry) in &wanted {
        if current.get(key) != Some(entry) {
            changes.push(entry.clone().change(*key));
        }
    }
    for key in current.keys() {
        if !wanted.contains_key(key) {
            changes.push(Change::Delete(*key));
        }
    }
    if let Some(holder) = index.get(&canonical_key(&record.plan.request))?
        && holder != id
    {
        return Err("Duplicate persistent series scope".into());
    }
    // Catalog identities and canonical numbers are one-to-one across every
    // scope of a series, including scopes that overlap this one.
    for (catalog, number) in &record.anchors {
        if previous
            .as_ref()
            .is_some_and(|old| old.anchors.get(catalog) == Some(number))
        {
            continue;
        }
        let conflict = || "Overlapping series scopes disagree on retained episode identities";
        let identity = identity_key(series, *catalog);
        match index.get(&identity)? {
            Some(known) if known != number_bytes(*number) => return Err(conflict().into()),
            Some(_) => {}
            None => changes.push(Change::PutInline(identity, number_bytes(*number))),
        }
        let owner = owner_key(series, *number);
        match index.get(&owner)? {
            Some(known) if known != catalog.to_le_bytes() => return Err(conflict().into()),
            Some(_) => {}
            None => changes.push(Change::PutInline(owner, catalog.to_le_bytes().to_vec())),
        }
    }
    let before = counts(index)?;
    let mut after = before;
    match &previous {
        None => after.series += 1,
        Some(old) if old.monitored => {
            after.monitored = after
                .monitored
                .checked_sub(1)
                .ok_or("Series index counters disagree with its records")?;
        }
        Some(_) => {}
    }
    if record.monitored {
        after.monitored += 1;
    }
    if after != before {
        changes.push(counts_change(after));
    }
    changes.extend(extra);
    if changes.is_empty() {
        return Ok(());
    }
    index.commit(changes)?;
    Ok(())
}

/// Confirms every entry a record owns is present with its exact content.
pub(super) fn verify_record(index: &mut Index, record: &Record) -> Result<()> {
    for (key, entry) in owned(record)? {
        let stored = index.get(&key)?.ok_or("Series index entry is missing")?;
        let (Entry::File(data) | Entry::Inline(data)) = entry;
        if stored != data {
            return Err("Series index entry differs from its record".into());
        }
    }
    Ok(())
}

pub(super) fn counts_of(index: &mut Index) -> Result<Counts> {
    counts(index)
}

pub(super) fn cursor_of(index: &mut Index) -> Result<Option<String>> {
    Ok(index
        .get(&MIGRATION_CURSOR_KEY)?
        .map(|bytes| id_text(&bytes)))
}

pub(super) fn source_of(index: &mut Index) -> Result<Option<Vec<u8>>> {
    index.get(&MIGRATION_SOURCE_KEY)
}

pub(super) fn migration_changes(
    source: Option<&[u8; 32]>,
    cursor: Option<&str>,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    if let Some(source) = source {
        changes.push(Change::PutInline(MIGRATION_SOURCE_KEY, source.to_vec()));
    }
    if let Some(cursor) = cursor {
        changes.push(Change::PutInline(
            MIGRATION_CURSOR_KEY,
            id_bytes(cursor)?.to_vec(),
        ));
    }
    Ok(changes)
}

/// Retires the resumable cursor once a migration is verified.
pub(super) fn clear_cursor() -> Change {
    Change::Delete(MIGRATION_CURSOR_KEY)
}

// ---------------------------------------------------------------------------
// The store

impl SeriesStore {
    pub(crate) fn open(directory: &Path, read_only: bool) -> Result<Self> {
        migration::open(directory, read_only)
    }

    pub(super) fn from_parts(
        directory: &Path,
        backend: Backend,
        read_only: bool,
        migration: Option<MigrationReport>,
    ) -> Self {
        Self {
            directory: directory.to_owned(),
            backend,
            read_only,
            poisoned: false,
            migration,
            due_pass: [0; 3],
        }
    }

    pub(crate) fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err("Series storage is read-only".into());
        }
        if self.poisoned {
            return Err(
                "Series storage durability is uncertain; restart before further changes".into(),
            );
        }
        Ok(())
    }

    /// `None` when nothing has ever been stored.
    fn reader(&mut self) -> Result<Option<&mut Index>> {
        match &mut self.backend {
            Backend::Absent => Ok(None),
            Backend::Unmigrated => Err(
                "Series storage has not been migrated; start the service or any command with write access once"
                    .into(),
            ),
            Backend::Ready(index) => Ok(Some(index)),
        }
    }

    fn writer(&mut self) -> Result<&mut Index> {
        self.check_writable()?;
        if matches!(self.backend, Backend::Absent) {
            let index = migration::create_fresh(&self.directory)?;
            self.backend = Backend::Ready(Box::new(index));
        }
        match &mut self.backend {
            Backend::Ready(index) => Ok(index),
            _ => Err("Series storage has not been migrated".into()),
        }
    }

    fn guarded<T>(&mut self, work: impl FnOnce(&mut Index) -> Result<T>) -> Result<T> {
        let index = self.writer()?;
        let result = work(index);
        if result.is_err() && !index.is_usable() {
            self.poisoned = true;
        }
        result
    }

    /// Identifies the stored state for stale-plan fences; changes on every commit.
    pub(crate) fn sequence(&mut self) -> Result<u64> {
        self.reader()?.map_or(Ok(0), |index| index.sequence())
    }

    /// Index size and state for diagnostics, from the durable root only.
    pub(crate) fn storage(&self) -> Value {
        let mut value = match &self.backend {
            Backend::Absent => crate::inventory::report("empty", None),
            Backend::Unmigrated => crate::inventory::report("needs_migration", None),
            Backend::Ready(index) => crate::inventory::report("index", Some(index)),
        };
        value.insert("migrated_at_start", self.migration.is_some());
        value
    }

    /// Storage state for status reports; never fails on an unreadable store.
    pub fn describe(&mut self) -> Value {
        let state = match &self.backend {
            Backend::Absent => "empty",
            Backend::Unmigrated => "needs_migration",
            Backend::Ready(_) => "index",
        };
        let mut value = Value::object();
        value.insert("state", state);
        let mut due = Value::object();
        for (name, count) in ["pages", "examined", "attempted"]
            .into_iter()
            .zip(self.due_pass)
        {
            due.insert(name, Value::Number(count as f64));
        }
        value.insert("due_pass", due);
        match self.counts() {
            Ok(counts) => {
                value.insert("series", Value::Number(counts.series as f64));
                value.insert("monitored", Value::Number(counts.monitored as f64));
            }
            Err(_) => {
                value.insert("series", Value::Null);
                value.insert("monitored", Value::Null);
            }
        }
        value.insert(
            "migration",
            self.migration.as_ref().map_or(Value::Null, |report| {
                let mut value = Value::object();
                value.insert("records", Value::Number(report.records as f64));
                value.insert("episodes", Value::Number(report.episodes as f64));
                value.insert("resumed", report.resumed);
                value.insert("preserved_snapshot", report.backup.clone());
                value
            }),
        );
        value
    }

    pub fn counts(&mut self) -> Result<Counts> {
        match self.reader()? {
            None => Ok(Counts {
                series: 0,
                monitored: 0,
            }),
            Some(index) => counts(index),
        }
    }

    pub fn get(&mut self, id: &str) -> Result<Option<Record>> {
        id_bytes(id)?;
        match self.reader()? {
            None => Ok(None),
            Some(index) => load_record(index, id),
        }
    }

    /// Summaries ordered by identifier, after the exclusive `after` cursor.
    pub fn page(&mut self, after: Option<&str>, limit: usize) -> Result<SummaryPage> {
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err("Series page limit must be 1 to 200".into());
        }
        let after = after
            .map(|id| id_bytes(id).map(|id| header_key(&id)))
            .transpose()?;
        let Some(index) = self.reader()? else {
            return Ok(SummaryPage {
                series: Vec::new(),
                next: None,
                total: 0,
            });
        };
        let total = counts(index)?.series;
        let page = scan(index, &[T_HEADER], after, limit)?;
        let mut series = Vec::new();
        for (found, bytes) in &page.entries {
            let id = id_text(&found[1..17]);
            series.push(Head::decode(bytes, &id)?.summary());
        }
        Ok(SummaryPage {
            series,
            next: page.next.map(|found| id_text(&found[1..17])),
            total,
        })
    }

    /// The scope with this exact canonical identity, if any.
    pub(crate) fn by_scope(&mut self, request: &Request) -> Result<Option<Record>> {
        let Some(index) = self.reader()? else {
            return Ok(None);
        };
        let Some(holder) = index.get(&canonical_key(request))? else {
            return Ok(None);
        };
        let id = id_text(&holder);
        match load_record(index, &id)? {
            Some(record) if record.plan.request.canonical_key() == request.canonical_key() => {
                Ok(Some(record))
            }
            _ => Err("Series scope index disagrees with its records".into()),
        }
    }

    /// Scopes a series request refers to: at most two are returned, so a
    /// caller can reject ambiguity without reading the whole catalog.
    pub(crate) fn matching(&mut self, request: &Request) -> Result<Vec<Record>> {
        let prefix: Vec<u8> = match request.tmdb_id {
            Some(id) => [
                &[T_TMDB][..],
                &id.to_be_bytes(),
                &request.season.to_be_bytes(),
                &request.episode.to_be_bytes(),
            ]
            .concat(),
            None => [
                &[T_TITLE][..],
                &title_digest(&request.title),
                &request.season.to_be_bytes(),
                &request.episode.to_be_bytes(),
            ]
            .concat(),
        };
        let Some(index) = self.reader()? else {
            return Ok(Vec::new());
        };
        let page = scan(index, &prefix, None, MAX_PAGE_ROWS)?;
        if page.next.is_some() {
            return Err("Series identity is ambiguous; specify a TMDB ID".into());
        }
        let mut found = Vec::new();
        for (entry, _) in page.entries {
            let id = id_text(&entry[prefix.len()..prefix.len() + 16]);
            let record = load_record(index, &id)?.ok_or("Series lookup index is stale")?;
            let known = &record.plan.request;
            let hit = known.season == request.season
                && known.episode == request.episode
                && match request.tmdb_id {
                    Some(id) => known.tmdb_id == Some(id),
                    None => {
                        tokens(&known.title) == tokens(&request.title)
                            && (request.year == 0 || request.year == known.year)
                    }
                };
            if hit {
                found.push(record);
                if found.len() > 1 {
                    break;
                }
            }
        }
        Ok(found)
    }

    /// Scopes of one catalog series, at most two (callers reject ambiguity).
    pub(crate) fn retained(&mut self, tmdb: Option<u64>) -> Result<Vec<Record>> {
        let Some(tmdb) = tmdb else {
            return Ok(Vec::new());
        };
        let Some(index) = self.reader()? else {
            return Ok(Vec::new());
        };
        let prefix = [&[T_TMDB][..], &tmdb.to_be_bytes()].concat();
        let page = scan(index, &prefix, None, 3)?;
        let mut found = Vec::new();
        for (entry, _) in page.entries {
            let id = id_text(&entry[prefix.len() + 8..prefix.len() + 24]);
            found.push(load_record(index, &id)?.ok_or("Series lookup index is stale")?);
        }
        Ok(found)
    }

    /// Monitored scopes whose check is due, oldest first. Work is one tree
    /// path plus at most `limit` rows however many scopes exist.
    #[cfg(test)]
    pub(crate) fn due(&mut self, now: u64, limit: usize) -> Result<Vec<Due>> {
        self.due_after(now, None, limit)
    }

    /// Durable position in the due index. A position whose timestamp is now
    /// in the future (clock rollback) restarts the due range conservatively.
    pub(crate) fn due_cursor(&mut self, now: u64) -> Result<Option<Due>> {
        let Some(index) = self.reader()? else {
            return Ok(None);
        };
        index
            .get(&DUE_CURSOR_KEY)?
            .map(|bytes| {
                if bytes.len() != 24 {
                    return Err("Invalid series scheduling cursor".into());
                }
                let next_check_at = u64::from_be_bytes(bytes[..8].try_into().expect("eight bytes"));
                Ok((next_check_at <= now).then(|| Due {
                    id: id_text(&bytes[8..]),
                    next_check_at,
                }))
            })
            .transpose()
            .map(Option::flatten)
    }

    /// An interrupted pass replays its last uncheckpointed rows. Refreshes
    /// retain their revision fences and already-rescheduled scopes leave the
    /// old due range, so checkpoints never grant acquisition authority.
    pub(crate) fn save_due_cursor(&mut self, due: Option<&Due>) -> Result<()> {
        self.check_writable()?;
        let bytes = due
            .map(|due| {
                let mut bytes = due.next_check_at.to_be_bytes().to_vec();
                bytes.extend_from_slice(&id_bytes(&due.id)?);
                Ok::<_, String>(bytes)
            })
            .transpose()?;
        let previous = match self.reader()? {
            Some(index) => index.get(&DUE_CURSOR_KEY)?,
            None => None,
        };
        if previous != bytes {
            self.guarded(|index| {
                index.commit(vec![match bytes {
                    Some(bytes) => Change::PutInline(DUE_CURSOR_KEY, bytes),
                    None => Change::Delete(DUE_CURSOR_KEY),
                }])?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// The same order continued after an earlier entry (`after` is exclusive).
    pub(crate) fn due_after(
        &mut self,
        now: u64,
        after: Option<&Due>,
        limit: usize,
    ) -> Result<Vec<Due>> {
        let after = after
            .map(|due| Ok::<_, String>(due_key(due.next_check_at, &id_bytes(&due.id)?)))
            .transpose()?;
        let Some(index) = self.reader()? else {
            return Ok(Vec::new());
        };
        let start = key(&[&[T_DUE]]);
        let end = match now.checked_add(1) {
            Some(next) => key(&[&[T_DUE], &next.to_be_bytes()]),
            None => prefix_range(&[T_DUE]).1.ok_or("Invalid due range")?,
        };
        let page = index.page(&Query {
            start,
            end: Some(end),
            after,
            limit: limit.clamp(1, MAX_PAGE_ROWS),
            sequence: None,
        })?;
        page.entries
            .iter()
            .map(|(found, _)| {
                Ok(Due {
                    next_check_at: u64::from_be_bytes(
                        found[1..9].try_into().map_err(|_| "Invalid due key")?,
                    ),
                    id: id_text(&found[9..25]),
                })
            })
            .collect()
    }

    pub fn subscribe(
        &mut self,
        plan: Plan,
        include_specials: bool,
        future_only: bool,
        monitored: bool,
        now: u64,
    ) -> Result<Record> {
        plan.validate()?;
        if let Some(record) = self.by_scope(&plan.request)? {
            return Ok(record);
        }
        let id = id_text(&random_bytes::<16>()?);
        if self.get(&id)?.is_some() {
            return Err("Cannot allocate a unique series identifier".into());
        }
        let record = Record {
            id,
            anchors: initial_anchors(&plan),
            numbering: BTreeMap::new(),
            plan,
            monitored,
            include_specials,
            start_date: future_only.then(date::today),
            excluded: BTreeSet::new(),
            revision: 1,
            created_at: now,
            updated_at: now,
            checked_at: now,
            next_check_at: 0,
            last_error: None,
        };
        self.save(record.clone())?;
        Ok(record)
    }

    pub fn configure(
        &mut self,
        id: &str,
        monitored: Option<bool>,
        include_specials: Option<bool>,
        start_date: Option<Option<String>>,
        now: u64,
    ) -> Result<Record> {
        self.check_writable()?;
        let mut record = self.get(id)?.ok_or("Unknown monitored series")?;
        if let Some(enabled) = monitored {
            record.monitored = enabled;
        }
        if let Some(enabled) = include_specials {
            record.include_specials = enabled;
        }
        if let Some(start) = start_date {
            if let Some(date) = &start {
                date::day(date)?;
            }
            record.start_date = start;
        }
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or("Series revision overflow")?;
        record.updated_at = now;
        record.next_check_at = 0;
        self.save(record.clone())?;
        Ok(record)
    }

    pub fn monitor_episode(
        &mut self,
        id: &str,
        season: u32,
        episode: u32,
        enabled: bool,
        now: u64,
    ) -> Result<Record> {
        self.check_writable()?;
        let mut record = self.get(id)?.ok_or("Unknown monitored series")?;
        if !record
            .plan
            .episodes
            .iter()
            .any(|item| item.season == season && item.episode == episode)
        {
            return Err("Unknown catalog episode".into());
        }
        if enabled {
            record.excluded.remove(&(season, episode));
        } else {
            record.excluded.insert((season, episode));
        }
        record.revision = record
            .revision
            .checked_add(1)
            .ok_or("Series revision overflow")?;
        record.updated_at = now;
        record.next_check_at = 0;
        self.save(record.clone())?;
        Ok(record)
    }

    pub(crate) fn save(&mut self, mut record: Record) -> Result<()> {
        self.check_writable()?;
        record.plan.episodes.sort_by_key(|e| (e.season, e.episode));
        record.validate()?;
        self.guarded(|index| save_record(index, &record, Vec::new()))
    }

    /// One calendar page over the date index; see `CalendarQuery`.
    pub(crate) fn calendar(&mut self, query: &CalendarQuery) -> Result<CalendarPage> {
        let Some(index) = self.reader()? else {
            return Ok(CalendarPage {
                rows: Vec::new(),
                total: Some(0),
                next: None,
            });
        };
        if let Some(id) = &query.series_id {
            return scoped_calendar(index, query, id);
        }
        let after = query.after.as_deref().map(parse_cursor).transpose()?;
        let start = key(&[&[T_CALENDAR], query.from.as_bytes()]);
        let end = prefix_range(&[&[T_CALENDAR][..], query.to.as_bytes()].concat()).1;
        if after.is_some_and(|after| after < start || end.is_some_and(|end| after >= end)) {
            return Err("Calendar cursor is outside the requested window".into());
        }
        let mut skip = if after.is_some() { 0 } else { query.offset };
        let mut cursor = after;
        let mut selected = Vec::new();
        let mut counted = 0_u64;
        let mut more = false;
        let mut complete = false;
        loop {
            let page = index.page(&Query {
                start,
                end,
                after: cursor,
                limit: MAX_PAGE_ROWS,
                sequence: None,
            })?;
            for (found, _) in &page.entries {
                counted += 1;
                if skip > 0 {
                    skip -= 1;
                } else if selected.len() < query.limit {
                    selected.push(*found);
                } else {
                    more = true;
                }
            }
            cursor = page.next;
            if cursor.is_none() {
                complete = true;
                break;
            }
            let enough = selected.len() == query.limit;
            // A cursor request stops once it knows a row follows; an offset
            // request also counts the window, within a fixed scan budget.
            if enough
                && (if after.is_some() {
                    more
                } else {
                    counted >= COUNT_SCAN_ROWS
                })
            {
                more = true;
                break;
            }
        }
        let next = match selected.last() {
            Some(last) if more => Some(cursor_text(last)?),
            _ => None,
        };
        let total = (complete && after.is_none()).then_some(counted);
        let mut heads: HashMap<[u8; 16], Rc<Record>> = HashMap::new();
        let mut blocks: HashMap<Key, Block> = HashMap::new();
        let mut rows = Vec::new();
        for found in selected {
            let (day, id, season, episode) = parse_calendar_key(&found)?;
            let shell = match heads.get(&id) {
                Some(shell) => shell.clone(),
                None => {
                    let head = load_head(index, &id_text(&id))?
                        .ok_or("Series calendar index disagrees with its records")?;
                    let shell = Rc::new(head.shell());
                    heads.insert(id, shell.clone());
                    shell
                }
            };
            let block = block_key(&id, season, episode);
            if let std::collections::hash_map::Entry::Vacant(slot) = blocks.entry(block) {
                let bytes = index
                    .get(&block)?
                    .ok_or("Series calendar index disagrees with its records")?;
                slot.insert(Block::decode(&bytes, season, episode / BLOCK)?);
            }
            let content = &blocks[&block];
            let known = content
                .episodes
                .iter()
                .find(|item| item.episode == episode && item.air_date.as_deref() == Some(&day))
                .ok_or("Series calendar index disagrees with its records")?;
            let mut series = (*shell).clone();
            if let Some(choice) = known.catalog_id.and_then(|id| {
                content
                    .choices
                    .iter()
                    .find(|choice| choice.catalog_id == id)
            }) {
                series.numbering.insert(choice.catalog_id, *choice);
            }
            rows.push(CalendarRow {
                series: Rc::new(series),
                episode: known.clone(),
            });
        }
        Ok(CalendarPage { rows, total, next })
    }
}

fn cursor_text(found: &Key) -> Result<String> {
    let (day, id, season, episode) = parse_calendar_key(found)?;
    Ok(format!("{day}.{}.{season}.{episode}", id_text(&id)))
}

fn parse_cursor(text: &str) -> Result<Key> {
    let invalid = || "Invalid calendar cursor".to_owned();
    let parts: Vec<_> = text.split('.').collect();
    let [day, id, season, episode] = parts[..] else {
        return Err(invalid());
    };
    date::day(day)?;
    let number = |text: &str| -> Result<u32> {
        if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        text.parse().map_err(|_| invalid())
    };
    Ok(calendar_key(
        day,
        &id_bytes(id)?,
        number(season)?,
        number(episode)?,
    ))
}

fn parse_calendar_key(found: &Key) -> Result<(String, [u8; 16], u32, u32)> {
    let day = std::str::from_utf8(&found[1..11]).map_err(|_| "Invalid calendar key")?;
    let id: [u8; 16] = found[11..27]
        .try_into()
        .map_err(|_| "Invalid calendar key")?;
    let season = u32::from_be_bytes(
        found[27..31]
            .try_into()
            .map_err(|_| "Invalid calendar key")?,
    );
    let episode = u32::from_be_bytes(
        found[31..35]
            .try_into()
            .map_err(|_| "Invalid calendar key")?,
    );
    Ok((day.to_owned(), id, season, episode))
}

/// A calendar restricted to one scope reads that scope's bounded plan.
fn scoped_calendar(index: &mut Index, query: &CalendarQuery, id: &str) -> Result<CalendarPage> {
    let Some(record) = load_record(index, id)? else {
        return Ok(CalendarPage {
            rows: Vec::new(),
            total: Some(0),
            next: None,
        });
    };
    let after = query.after.as_deref().map(parse_cursor).transpose()?;
    let mut window: Vec<&Episode> = record
        .plan
        .episodes
        .iter()
        .filter(|episode| {
            episode
                .air_date
                .as_ref()
                .is_some_and(|date| date >= &query.from && date <= &query.to)
        })
        .collect();
    window.sort_by_key(|episode| (episode.air_date.clone(), episode.season, episode.episode));
    let raw = id_bytes(id)?;
    let positioned: Vec<&Episode> = match after {
        Some(after) => window
            .iter()
            .copied()
            .filter(|episode| {
                episode.air_date.as_deref().is_some_and(|date| {
                    calendar_key(date, &raw, episode.season, episode.episode) > after
                })
            })
            .collect(),
        None => window.iter().copied().skip(query.offset).collect(),
    };
    let more = positioned.len() > query.limit;
    let shown: Vec<&Episode> = positioned.into_iter().take(query.limit).collect();
    let next = match shown.last().filter(|_| more) {
        Some(episode) => episode
            .air_date
            .as_deref()
            .map(|date| cursor_text(&calendar_key(date, &raw, episode.season, episode.episode)))
            .transpose()?,
        None => None,
    };
    let shared = Rc::new({
        let mut shell = record.clone();
        shell.plan.episodes = Vec::new();
        shell
    });
    Ok(CalendarPage {
        total: after.is_none().then_some(window.len() as u64),
        next,
        rows: shown
            .into_iter()
            .map(|episode| CalendarRow {
                series: shared.clone(),
                episode: episode.clone(),
            })
            .collect(),
    })
}

/// Test-only: overwrites the counters, as a damaged index could present them.
#[cfg(test)]
pub(super) fn forge_counts(index: &mut Index, series: u64, monitored: u64) -> Result<()> {
    index.commit(vec![counts_change(Counts { series, monitored })])?;
    Ok(())
}

/// Test-only bulk loader: commits many brand-new records per transaction.
/// Production code always goes through `save_record`.
#[cfg(test)]
pub(super) fn insert_many(index: &mut Index, records: &[Record]) -> Result<()> {
    let mut changes: Vec<Change> = Vec::new();
    let mut shared: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
    let mut before = counts(index)?;
    for record in records {
        record.validate()?;
        let series = tmdb(record)?;
        let mut pending = Vec::new();
        for (key, entry) in owned(record)? {
            pending.push(entry.change(key));
        }
        let mut local = BTreeMap::new();
        for (catalog, number) in &record.anchors {
            local.insert(identity_key(series, *catalog), number_bytes(*number));
            local.insert(owner_key(series, *number), catalog.to_le_bytes().to_vec());
        }
        if !changes.is_empty()
            && changes.len() + shared.len() + pending.len() + local.len() + 1
                > crate::inventory::MAX_CHANGES
        {
            flush(index, &mut changes, &mut shared, before)?;
        }
        changes.extend(pending);
        shared.extend(local);
        before.series += 1;
        before.monitored += u64::from(record.monitored);
    }
    flush(index, &mut changes, &mut shared, before)
}

#[cfg(test)]
fn flush(
    index: &mut Index,
    changes: &mut Vec<Change>,
    shared: &mut BTreeMap<Key, Vec<u8>>,
    after: Counts,
) -> Result<()> {
    for (key, value) in std::mem::take(shared) {
        if index.get(&key)?.is_none() {
            changes.push(Change::PutInline(key, value));
        }
    }
    changes.push(counts_change(after));
    index.commit(std::mem::take(changes))?;
    Ok(())
}
