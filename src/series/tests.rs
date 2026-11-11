//! Original storage, recovery and scale scenarios. CI only; all
//! catalogs here are synthetic and no network is used.
use super::{
    CalendarQuery, Episode, NumberingChoice, Plan, Record,
    storage::{Backend, SeriesStore, insert_many},
};
use crate::{
    crypto::random_bytes,
    date,
    inventory::{Fault, Index},
    json::{self, Value},
    numbering::{EpisodeNumber, SourceNumber},
    store::Request,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Instant,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix = super::hex(&random_bytes::<16>().unwrap());
        let path = std::env::temp_dir().join(format!("mynou-series-{suffix}"));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn contents(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            result.extend(contents(&path));
        } else {
            result.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
    result
}

fn identifier(n: u64) -> String {
    format!(
        "{:032x}",
        (u128::from(n) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835)
    )
}

fn request(n: u64, season: u32, episode: u32) -> Request {
    Request {
        kind: "series".into(),
        title: format!("Synthetic Series {n}"),
        year: 2020,
        season,
        episode,
        source_path: None,
        source_url: None,
        tmdb_id: Some(1000 + n),
        runtime_minutes: None,
        source_numbering: None,
        original_language: None,
    }
}

fn episode(n: u64, season: u32, number: u32, title_length: usize) -> Episode {
    let base = date::day("2024-01-01").unwrap();
    let offset = i64::from((season * 31 + number * 7) % 300);
    Episode {
        runtime_minutes: None,
        catalog_id: Some(n * 1_000_000 + u64::from(season) * 1_000 + u64::from(number)),
        season,
        episode: number,
        title: format!("Episode {season}x{number} ")
            .repeat(1 + title_length / 14)
            .chars()
            .take(title_length)
            .collect(),
        air_date: Some(date::from_day(base + offset).unwrap()),
    }
}

fn record(n: u64, seasons: u32, per_season: u32, title_length: usize) -> Record {
    let episodes: Vec<Episode> = (1..=seasons)
        .flat_map(|season| (1..=per_season).map(move |number| (season, number)))
        .map(|(season, number)| episode(n, season, number, title_length))
        .collect();
    let plan = Plan {
        request: request(n, 0, 0),
        episodes,
    };
    Record {
        id: identifier(n),
        anchors: super::numbering::initial_anchors(&plan),
        numbering: BTreeMap::new(),
        plan,
        monitored: true,
        include_specials: false,
        start_date: None,
        excluded: BTreeSet::new(),
        revision: 1,
        created_at: 10,
        updated_at: 10,
        checked_at: 10,
        next_check_at: 1_000 + n,
        last_error: None,
    }
}

fn ready(store: &mut SeriesStore) -> &mut Index {
    match &mut store.backend {
        Backend::Ready(index) => index,
        _ => panic!("the store has no index"),
    }
}

fn load_all(store: &mut SeriesStore) -> Vec<Record> {
    let mut records = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store.page(after.as_deref(), 37).unwrap();
        for summary in &page.series {
            let id = summary.get("id").and_then(Value::as_str).unwrap();
            records.push(store.get(id).unwrap().unwrap());
        }
        after = page.next;
        if after.is_none() {
            return records;
        }
    }
}

fn assert_consistent(store: &mut SeriesStore, expected: &[Record]) {
    let mut expected: Vec<Record> = expected.to_vec();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(load_all(store), expected);
    let counts = store.counts().unwrap();
    assert_eq!(counts.series, expected.len() as u64);
    assert_eq!(
        counts.monitored,
        expected.iter().filter(|record| record.monitored).count() as u64
    );
    for record in &expected {
        assert_eq!(store.get(&record.id).unwrap().as_ref(), Some(record));
    }
}

#[test]
fn empty_stores_create_nothing_and_the_first_write_publishes_the_index() {
    let directory = Directory::new();
    for read_only in [true, false] {
        let mut store = SeriesStore::open(&directory.0, read_only).unwrap();
        assert_eq!(store.counts().unwrap().series, 0);
        assert!(store.get(&identifier(1)).unwrap().is_none());
        assert_eq!(store.page(None, 5).unwrap().series.len(), 0);
        assert!(store.due(u64::MAX, 4).unwrap().is_empty());
        assert!(
            store
                .calendar(&CalendarQuery::new(Some("2024-01-01"), Some("2024-02-01")).unwrap())
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(contents(&directory.0).is_empty());
    }
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert!(store.save(record(1, 1, 2, 10)).is_err());
    drop(store);
    assert!(contents(&directory.0).is_empty());

    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(record(1, 1, 2, 10)).unwrap();
    assert!(directory.0.join("series.idx").is_dir());
    drop(store);
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert_eq!(store.counts().unwrap().series, 1);
    drop(store);

    // An interrupted first creation leaves only staging, replaced later.
    let directory = Directory::new();
    drop(Index::create(&directory.0.join("series.idx.creating")).unwrap());
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(record(2, 1, 2, 10)).unwrap();
    assert!(!directory.0.join("series.idx.creating").exists());
    assert_eq!(store.counts().unwrap().series, 1);
}

#[test]
fn inventories_above_every_former_limit_page_completely_and_survive_restart() {
    let directory = Directory::new();
    let scopes = 129;
    let records: Vec<Record> = (1..=scopes).map(|n| record(n, 2, 78, 300)).collect();
    let episodes: usize = records.iter().map(|r| r.plan.episodes.len()).sum();
    // More scopes and retained episodes than the old snapshot allowed.
    assert!(records.len() > 128);
    assert!(episodes > 20_000, "{episodes}");

    let started = Instant::now();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(records[0].clone()).unwrap();
    insert_many(ready(&mut store), &records[1..]).unwrap();
    assert_eq!(store.counts().unwrap().series, scopes);
    eprintln!(
        "series-above-limits scopes={scopes} episodes={episodes} load_ms={}",
        started.elapsed().as_millis()
    );
    drop(store);

    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    let mut expected = records.clone();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(load_all(&mut store), expected);
    for record in [&expected[0], &expected[64], &expected[128]] {
        assert_eq!(store.get(&record.id).unwrap().as_ref(), Some(record));
    }
    // Every summary is reachable page by page, in order, exactly once.
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = store.page(after.as_deref(), 33).unwrap();
        assert_eq!(page.total, scopes);
        seen.extend(
            page.series
                .iter()
                .map(|v| v.get("id").and_then(Value::as_str).unwrap().to_owned()),
        );
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let ids: Vec<String> = expected.iter().map(|r| r.id.clone()).collect();
    assert_eq!(seen, ids);
    // A well-formed cursor naming no series is stale and rejected explicitly.
    let unknown = "f".repeat(32);
    assert!(!ids.contains(&unknown));
    let error = store.page(Some(&unknown), 10).unwrap_err();
    assert!(error.contains("Stale series cursor"), "{error}");
    // Lookups and scheduling touch only their own entries.
    let wanted = &records[77];
    assert_eq!(
        store.by_scope(&wanted.plan.request).unwrap().as_ref(),
        Some(wanted)
    );
    assert_eq!(store.matching(&wanted.plan.request).unwrap().len(), 1);
    let mut title_only = wanted.plan.request.clone();
    title_only.tmdb_id = None;
    assert_eq!(store.matching(&title_only).unwrap().len(), 1);
    let due = store.due(u64::MAX, 4).unwrap();
    assert_eq!(
        due.iter().map(|d| d.next_check_at).collect::<Vec<_>>(),
        vec![1_001, 1_002, 1_003, 1_004]
    );
}

#[test]
fn overlapping_scopes_keep_one_identity_per_catalog_episode() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let whole = record(1, 2, 4, 12);
    store.save(whole.clone()).unwrap();
    // A season scope agreeing with the whole-series identities is accepted.
    let mut season = record(1, 0, 0, 0);
    season.id = identifier(50);
    season.plan.request = request(1, 2, 0);
    season.plan.episodes = whole
        .plan
        .episodes
        .iter()
        .filter(|e| e.season == 2)
        .cloned()
        .collect();
    season.anchors = super::numbering::initial_anchors(&season.plan);
    store.save(season.clone()).unwrap();
    // Closing checkpoints the saves, so the files settle before the
    // rejected writes below.
    drop(store);
    let mut store = SeriesStore::open(&directory.0, false).unwrap();

    let before = contents(&directory.0);
    // The same catalog identity under another number.
    let mut moved = season.clone();
    moved.id = identifier(51);
    moved.plan.request = request(1, 3, 0);
    let shared = *season.anchors.keys().next().unwrap();
    moved.plan.episodes = vec![Episode {
        runtime_minutes: None,
        catalog_id: Some(shared),
        season: 3,
        episode: 99,
        title: "Moved".into(),
        air_date: None,
    }];
    moved.anchors = BTreeMap::from([(
        shared,
        EpisodeNumber {
            season: 3,
            episode: 99,
        },
    )]);
    let error = store.save(moved).unwrap_err();
    assert!(error.contains("disagree"), "{error}");
    // Another identity on a retained number.
    let mut taken = season.clone();
    taken.id = identifier(52);
    let number = *season.anchors.values().next().unwrap();
    taken.plan.request = request(1, number.season, number.episode);
    taken.plan.episodes = vec![Episode {
        runtime_minutes: None,
        catalog_id: Some(999_999_999),
        season: number.season,
        episode: number.episode,
        title: "Taken".into(),
        air_date: None,
    }];
    taken.anchors = BTreeMap::from([(999_999_999, number)]);
    assert!(store.save(taken).unwrap_err().contains("disagree"));
    // The same canonical scope under another record identifier.
    let mut duplicate = whole.clone();
    duplicate.id = identifier(53);
    assert!(store.save(duplicate).unwrap_err().contains("Duplicate"));
    // A rewrite that changes a retained canonical number.
    let mut renumbered = whole.clone();
    let (id, number) = renumbered
        .anchors
        .iter()
        .next()
        .map(|(a, b)| (*a, *b))
        .unwrap();
    renumbered.anchors.insert(
        id,
        EpisodeNumber {
            season: number.season,
            episode: number.episode + 50,
        },
    );
    renumbered
        .plan
        .episodes
        .retain(|e| e.catalog_id != Some(id));
    assert!(store.save(renumbered).is_err());
    drop(store);
    assert_eq!(contents(&directory.0), before);

    // Catalog identities are scoped to their own series.
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let mut other = record(2, 1, 2, 5);
    let first = other.plan.episodes[0].clone();
    let mut reused = first.clone();
    reused.catalog_id = whole.plan.episodes[0].catalog_id;
    other.plan.episodes[0] = reused;
    other.anchors = super::numbering::initial_anchors(&other.plan);
    store.save(other).unwrap();
    assert_eq!(store.retained(Some(1001)).unwrap().len(), 2);
    assert_eq!(store.retained(Some(1002)).unwrap().len(), 1);
    assert_eq!(store.retained(Some(5)).unwrap().len(), 0);
}

#[test]
fn due_work_is_oldest_first_bounded_per_pass_and_never_starves_old_entries() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let total = 5_000u64;
    let records: Vec<Record> = (1..=total).map(|n| record(n, 1, 1, 4)).collect();
    store.save(records[0].clone()).unwrap();
    insert_many(ready(&mut store), &records[1..]).unwrap();
    // One pass reads one tree path plus its rows, not the inventory.
    let due = store.due(u64::MAX, 4).unwrap();
    assert_eq!(due.len(), 4);
    let reads = ready(&mut store).last_page_reads();
    assert!(
        reads <= 12,
        "a due pass read {reads} pages of {total} scopes"
    );
    // Not yet due entries are excluded.
    assert_eq!(store.due(1_002, 4).unwrap().len(), 2);

    // Serve oldest first and push each served scope to the back: every scope
    // is served once before any is served twice.
    let rotation = 40;
    let mut order = Vec::new();
    for pass in 0..rotation / 4 {
        for entry in store.due(u64::MAX, 4).unwrap() {
            let mut served = store.get(&entry.id).unwrap().unwrap();
            order.push(entry.id.clone());
            served.next_check_at = 1_000_000 + pass * 4 + order.len() as u64;
            store.save(served).unwrap();
        }
    }
    let expected: Vec<String> = (1..=rotation).map(identifier).collect();
    assert_eq!(order, expected);
    let unique: BTreeSet<_> = order.iter().collect();
    assert_eq!(unique.len(), rotation as usize);
    // The remaining old entries still come before every re-queued one.
    let next = store.due(u64::MAX, 4).unwrap();
    assert_eq!(next[0].next_check_at, 1_041);
    assert_eq!(store.counts().unwrap().monitored, total);
}

#[test]
fn calendar_windows_page_by_offset_and_by_cursor_beyond_the_old_offset_limit() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let day = |offset: i64| date::from_day(date::day("2025-03-01").unwrap() + offset).unwrap();
    let make = |n: u64, shows: u32| -> Record {
        let mut record = record(n, 1, shows, 8);
        for (index, episode) in record.plan.episodes.iter_mut().enumerate() {
            episode.air_date = Some(day((index as i64 + n as i64) % 30));
        }
        record
    };
    let records: Vec<Record> = (1..=300).map(|n| make(n, 71)).collect();
    store.save(records[0].clone()).unwrap();
    insert_many(ready(&mut store), &records[1..]).unwrap();
    let rows = 300 * 71;
    assert!(rows > 20_000);
    let mut query = CalendarQuery::new(Some(&day(0)), Some(&day(29))).unwrap();
    query.limit = 200;
    // Counting is bounded: a larger window reports no exact total.
    let first = store.calendar(&query).unwrap();
    assert_eq!(first.rows.len(), 200);
    assert_eq!(first.total, None);
    assert!(first.next.is_some());
    // A narrow window is counted exactly.
    let mut narrow = CalendarQuery::new(Some(&day(0)), Some(&day(0))).unwrap();
    narrow.limit = 200;
    let exact = store.calendar(&narrow).unwrap();
    let in_day: usize = records
        .iter()
        .flat_map(|r| &r.plan.episodes)
        .filter(|e| e.air_date.as_deref() == Some(&day(0)))
        .count();
    assert_eq!(exact.total, Some(in_day as u64));
    // The cursor walks the whole narrow window in key order, once per row.
    let mut collected = Vec::new();
    let mut cursor = None;
    loop {
        narrow.after = cursor.clone();
        narrow.limit = 50;
        let page = store.calendar(&narrow).unwrap();
        collected.extend(page.rows.iter().map(|row| {
            (
                row.episode.air_date.clone(),
                row.series.id.clone(),
                row.episode.season,
                row.episode.episode,
            )
        }));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(collected.len(), in_day);
    let mut sorted = collected.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(collected, sorted);
    // Pages deep inside the big window follow the cursor without scanning from the start.
    let mut query = CalendarQuery::new(Some(&day(0)), Some(&day(29))).unwrap();
    query.limit = 200;
    let mut cursor = None;
    for _ in 0..3 {
        query.after = cursor.clone();
        let page = store.calendar(&query).unwrap();
        assert_eq!(page.rows.len(), 200);
        cursor = page.next;
    }
    query.after = cursor;
    let deep = store.calendar(&query).unwrap();
    assert_eq!(deep.total, None);
    assert!(
        deep.rows.first().unwrap().episode.air_date >= first.rows.last().unwrap().episode.air_date
    );
    // A series filter reads only that scope's bounded plan.
    let mut scoped = CalendarQuery::new(Some(&day(0)), Some(&day(29))).unwrap();
    scoped.series_id = Some(identifier(17));
    scoped.limit = 30;
    let page = store.calendar(&scoped).unwrap();
    assert_eq!(page.total, Some(71));
    assert_eq!(page.rows.len(), 30);
    assert!(page.next.is_some());
}

#[test]
fn an_interrupted_write_never_discards_existing_records_at_any_commit_boundary() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstRecord,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let records: Vec<Record> = (1..=6).map(|n| record(n, 2, 6, 30)).collect();
        for record in &records {
            store.save(record.clone()).unwrap();
        }
        let mut changed = records[2].clone();
        changed.revision += 1;
        changed.monitored = false;
        changed.plan.episodes.pop();
        changed.plan.episodes[0].title = "Renamed".into();
        changed.anchors.retain(|id, _| {
            changed
                .plan
                .episodes
                .iter()
                .any(|e| e.catalog_id == Some(*id))
        });
        ready(&mut store).inject(Some(fault));
        assert!(store.save(changed.clone()).is_err(), "{fault:?}");
        // The uncertain store refuses reads and writes until reopened.
        assert!(store.get(&records[0].id).is_err());
        assert!(store.save(records[1].clone()).is_err());
        assert!(store.check_writable().is_err());
        drop(store);
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let mut expected = records.clone();
        if fault != Fault::Staged {
            expected[2] = changed;
        }
        assert_consistent(&mut store, &expected);
        // A later write works and keeps every earlier record.
        let mut next = expected[4].clone();
        next.revision += 1;
        store.save(next.clone()).unwrap();
        expected[4] = next;
        assert_consistent(&mut store, &expected);
    }
}

#[test]
fn a_read_only_index_and_unwritable_storage_refuse_writes_without_changes() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let records: Vec<Record> = (1..=3).map(|n| record(n, 1, 3, 10)).collect();
    for record in &records {
        store.save(record.clone()).unwrap();
    }
    drop(store);
    let before = contents(&directory.0);
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert!(
        store
            .save(records[0].clone())
            .unwrap_err()
            .contains("read-only")
    );
    assert!(
        store
            .configure(&records[0].id, Some(false), None, None, 5)
            .is_err()
    );
    assert_consistent(&mut store, &records);
    drop(store);
    assert_eq!(contents(&directory.0), before);
    // A full or failing disk surfaces as an error while records stay readable.
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    ready(&mut store).inject(Some(Fault::FirstRecord));
    let mut changed = records[1].clone();
    changed.revision += 1;
    assert!(store.save(changed).is_err());
    drop(store);
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    assert_eq!(store.counts().unwrap().series, 3);
}

/// The widest record the per-plan bounds allow: 2,000 unmapped dated episodes
/// and 2,000 retained identities, each with a numbering choice, spread over
/// `spread` seasons (so over at least `2 * spread` blocks).
fn widest(spread: u32) -> Record {
    let mut worst = record(7, 0, 0, 0);
    for index in 0..2_000u32 {
        let mut item = episode(7, 1 + index % spread, 1 + index / spread, 12);
        item.catalog_id = None;
        worst.plan.episodes.push(item);
    }
    worst.plan.episodes.sort_by_key(|e| (e.season, e.episode));
    for index in 0..2_000u32 {
        worst.anchors.insert(
            7_000_000_000 + u64::from(index),
            EpisodeNumber {
                season: 3_000 + index % spread,
                episode: 1 + index / spread,
            },
        );
    }
    for (index, (id, number)) in worst.anchors.clone().into_iter().enumerate() {
        worst.numbering.insert(
            id,
            NumberingChoice {
                catalog_id: id,
                catalog: number,
                source: SourceNumber::Absolute(index as u32 + 1),
            },
        );
    }
    worst.validate().unwrap();
    worst
}

fn save_widest(spread: u32) {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let worst = widest(spread);
    let started = Instant::now();
    store.save(worst.clone()).unwrap();
    eprintln!(
        "series-widest-save spread={spread} episodes=2000 anchors=2000 elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(store.get(&worst.id).unwrap().unwrap(), worst);
    // Re-dating every episode rewrites every calendar entry in one commit.
    let mut shifted = worst.clone();
    shifted.revision += 1;
    for (index, episode) in shifted.plan.episodes.iter_mut().enumerate() {
        episode.air_date =
            Some(date::from_day(date::day("2030-01-01").unwrap() + index as i64).unwrap());
    }
    let started = Instant::now();
    store.save(shifted.clone()).unwrap();
    eprintln!(
        "series-redate-all-save spread={spread} episodes=2000 elapsed_ms={}",
        started.elapsed().as_millis()
    );
    drop(store);
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert_consistent(&mut store, &[shifted]);
}

#[test]
fn the_widest_valid_plan_saves_in_one_transaction_and_round_trips() {
    // 2,000 episodes and 2,000 identities over 4,000 blocks (about 10,000
    // changes), then every date changed (about 6,000 changes), each one commit.
    save_widest(2_000);
}

#[test]
fn settings_changes_keep_counters_indexes_and_scope_lookups_exact() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let a = store
        .subscribe(record(1, 1, 3, 10).plan, false, true, true, 77)
        .unwrap();
    assert_eq!(a.revision, 1);
    assert!(a.start_date.is_some());
    // Subscribing again returns the stored scope without a write.
    let sequence = store.sequence().unwrap();
    let same = store
        .subscribe(record(1, 1, 3, 10).plan, true, false, false, 99)
        .unwrap();
    assert_eq!(same, a);
    assert_eq!(store.sequence().unwrap(), sequence);
    let b = store
        .subscribe(record(2, 1, 3, 10).plan, false, false, false, 78)
        .unwrap();
    assert_eq!(
        (
            store.counts().unwrap().series,
            store.counts().unwrap().monitored
        ),
        (2, 1)
    );
    let a = store
        .configure(&a.id, Some(false), Some(true), Some(None), 90)
        .unwrap();
    assert_eq!((a.revision, a.next_check_at, a.monitored), (2, 0, false));
    assert_eq!(store.counts().unwrap().monitored, 0);
    store.configure(&b.id, Some(true), None, None, 91).unwrap();
    assert_eq!(store.counts().unwrap().monitored, 1);
    assert_eq!(store.due(u64::MAX, 8).unwrap().len(), 1);
    let excluded = store.monitor_episode(&b.id, 1, 2, false, 92).unwrap();
    assert!(excluded.excluded.contains(&(1, 2)));
    assert!(store.monitor_episode(&b.id, 5, 5, false, 92).is_err());
    assert!(store.configure("zz", None, None, None, 1).is_err());
    assert!(
        store
            .configure(&identifier(999), None, None, None, 1)
            .is_err()
    );
    // Ambiguity is detectable without reading the catalog.
    let mut twin = record(3, 1, 2, 10);
    twin.plan.request.title = "Synthetic Series 1".into();
    twin.plan.request.year = 2020;
    twin.plan.request.tmdb_id = Some(5000);
    twin.anchors = super::numbering::initial_anchors(&twin.plan);
    store.save(twin).unwrap();
    let mut title_only = request(1, 0, 0);
    title_only.tmdb_id = None;
    assert_eq!(store.matching(&title_only).unwrap().len(), 2);
    title_only.year = 1999;
    assert!(store.matching(&title_only).unwrap().is_empty());
}

/// Measurement harness, run by hand: `MYNOU_SCALE_DIR=/path MYNOU_SCALE_SCOPES=N
/// MYNOU_SCALE_PHASE=build|read cargo test --release series_scale_report
/// -- --nocapture`; without MYNOU_SCALE_DIR the test does nothing. The phases
/// run in separate processes so the peak resident size of reading an existing
/// inventory is measured on its own.
#[test]
fn series_scale_report() {
    let Some(directory) = std::env::var_os("MYNOU_SCALE_DIR").map(PathBuf::from) else {
        return;
    };
    let scopes: u64 = std::env::var("MYNOU_SCALE_SCOPES")
        .expect("MYNOU_SCALE_SCOPES")
        .parse()
        .unwrap();
    let phase = std::env::var("MYNOU_SCALE_PHASE").expect("MYNOU_SCALE_PHASE");
    let episodes = scopes * 90;
    if phase == "build" {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let mut store = SeriesStore::open(&directory, false).unwrap();
        let started = Instant::now();
        store.save(record(1, 3, 30, 60)).unwrap();
        let mut batch = Vec::new();
        for n in 2..=scopes {
            batch.push(record(n, 3, 30, 60));
            if batch.len() == 100 {
                insert_many(ready(&mut store), &batch).unwrap();
                batch.clear();
            }
        }
        if !batch.is_empty() {
            insert_many(ready(&mut store), &batch).unwrap();
        }
        eprintln!(
            "series-scale-build scopes={scopes} episodes={episodes} elapsed_ms={} peak_rss_kib={}",
            started.elapsed().as_millis(),
            resident_kib()
        );
        return;
    }
    let started = Instant::now();
    let mut store = SeriesStore::open(&directory, false).unwrap();
    let open = started.elapsed();
    let started = Instant::now();
    let mut pages = 0;
    let mut after: Option<String> = None;
    loop {
        let page = store.page(after.as_deref(), 200).unwrap();
        pages += 1;
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let paged = started.elapsed();
    let started = Instant::now();
    let due = store.due(u64::MAX, 4).unwrap();
    let due_time = started.elapsed();
    let started = Instant::now();
    let mut changed = store.get(&due[0].id).unwrap().unwrap();
    changed.revision += 1;
    changed.next_check_at += 10_000_000;
    store.save(changed).unwrap();
    let save_time = started.elapsed();
    let mut query = CalendarQuery::new(Some("2024-01-01"), Some("2024-03-01")).unwrap();
    query.limit = 200;
    let started = Instant::now();
    let calendar = store.calendar(&query).unwrap();
    let calendar_time = started.elapsed();
    let started = Instant::now();
    let found = store.matching(&request(scopes / 2 + 1, 0, 0)).unwrap();
    let lookup_time = started.elapsed();
    assert_eq!(found.len(), 1);
    let size = fs::metadata(directory.join("series.idx/tree.bin"))
        .unwrap()
        .len();
    eprintln!(
        "series-scale-read scopes={scopes} episodes={episodes} open_ms={} summary_pages={pages} page_walk_ms={} due_pass_ms={} scope_lookup_ms={} one_save_ms={} calendar_page_ms={} calendar_total={:?} tree_mib={:.1} peak_rss_kib={}",
        open.as_millis(),
        paged.as_millis(),
        due_time.as_millis(),
        lookup_time.as_millis(),
        save_time.as_millis(),
        calendar_time.as_millis(),
        calendar.total,
        size as f64 / 1_048_576.0,
        resident_kib()
    );
}

fn resident_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("VmHWM:"))
                .and_then(|line| line.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

#[test]
fn series_due_pass_bounds_corrupt_prefix_and_resumes_after_restart() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    let mut records: Vec<_> = (1..=1201).map(|n| record(n, 1, 1, 1)).collect();
    for (offset, record) in records.iter_mut().enumerate() {
        record.next_check_at = offset as u64 + 1;
    }
    let healthy = records.last().unwrap().id.clone();
    let mut engine = Engine::open_for_management(config.clone()).unwrap();
    {
        let mut store = lock(&engine.series_store).unwrap();
        store.save(records[0].clone()).unwrap();
        insert_many(ready(&mut store), &records[1..]).unwrap();
        store.finish_due_pass(None, Some(123)).unwrap();
    }
    drop(engine);
    // More damaged scopes than both the former 128-scope inventory limit
    // and the unreadable-cache capacity. Each pass must still be bounded.
    for record in &records[..1200] {
        let mut key = [0; 64];
        key[0] = 0x10;
        for (slot, pair) in key[1..17]
            .iter_mut()
            .zip(record.id.as_bytes().as_chunks::<2>().0)
        {
            *slot = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        assert!(crate::inventory::damage_record(&config.store_dir.join("series.idx"), &key) > 0);
    }
    for turn in 0..3 {
        engine = Engine::open_for_management(config.clone()).unwrap();
        let before = ready(&mut lock(&engine.series_store).unwrap()).requests();
        assert!(engine.refresh_series_due().is_err());
        let mut series = lock(&engine.series_store).unwrap();
        assert_eq!(series.due_pass, [8, 400, 0]);
        let requests = ready(&mut series).requests() - before;
        assert!(
            requests <= 415,
            "{requests} requests in corrupt pass {turn}"
        );
        assert_eq!(
            series.due_cursor(u64::MAX).unwrap().unwrap().next_check_at,
            (turn + 1) * 400
        );
        // Newly added earlier-due scopes wait for the next sweep and do
        // not continually displace the older tail behind this checkpoint.
        let mut added = record(20_000 + turn, 1, 1, 1);
        added.next_check_at = 0;
        series.save(added).unwrap();
        drop(series);
        drop(engine);
    }
    // With no catalog credentials this produces an explicit local error;
    // it must nevertheless reach and reschedule the healthy tail scope.
    engine = Engine::open_for_management(config.clone()).unwrap();
    assert!(engine.refresh_series_due().is_err());
    {
        let mut series = lock(&engine.series_store).unwrap();
        assert_eq!(series.due_pass, [1, 1, 1]);
        assert!(series.get(&healthy).unwrap().unwrap().checked_at > 10);
        assert!(series.due_cursor(u64::MAX).unwrap().is_none());
    }
    drop(engine);
    // A new sweep wraps back to retry the original damaged prefix.
    engine = Engine::open_for_management(config).unwrap();
    assert!(engine.refresh_series_due().is_err());
    // New rows before the cursor get their own turn on wrap.
    assert!(lock(&engine.series_store).unwrap().due_pass[2] >= 3);
    // A cached unreadable prefix must consume the same budget. Otherwise a
    // second pass could walk an entire inventory without probing any record.
    lock(&engine.series_store)
        .unwrap()
        .save_due_cursor(None)
        .unwrap();
    let before = ready(&mut lock(&engine.series_store).unwrap()).requests();
    assert!(engine.refresh_series_due().is_err());
    let mut series = lock(&engine.series_store).unwrap();
    assert_eq!(series.due_pass, [8, 400, 0]);
    assert!(ready(&mut series).requests() - before <= 20);
    assert_eq!(
        series
            .backlog(crate::store::now())
            .unwrap()
            .get("last_completed_pass_at"),
        Some(&Value::from("123"))
    );
}

fn assert_backlog(store: &mut SeriesStore, now: u64, count: u64, oldest: Option<u64>) {
    let sequence = store.sequence().unwrap();
    let before = match &store.backend {
        Backend::Ready(index) => index.requests(),
        _ => 0,
    };
    let backlog = store.backlog(now).unwrap();
    assert_eq!(backlog.get("state").and_then(Value::as_str), Some("ready"));
    assert_eq!(backlog.get("due"), Some(&Value::from(count.to_string())));
    let value = |n: Option<u64>| n.map_or(Value::Null, |n| Value::from(n.to_string()));
    assert_eq!(backlog.get("oldest_due_at"), Some(&value(oldest)));
    assert_eq!(
        backlog.get("oldest_due_age_secs"),
        Some(&value(oldest.map(|at| now - at)))
    );
    if let Backend::Ready(index) = &store.backend {
        assert_eq!(index.requests() - before, 2, "status enumerated records");
        assert!(
            index.last_page_reads() <= 18,
            "status read {} pages",
            index.last_page_reads()
        );
    }
    assert_eq!(store.sequence().unwrap(), sequence, "status wrote storage");
}

#[test]
fn series_backlog_is_bounded_after_growth_removal_restart_and_clock_boundaries() {
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    assert_backlog(&mut store, 0, 0, None);
    let mut records: Vec<_> = (0..5000).map(|n| record(n, 1, 1, 4)).collect();
    // Multiple identities at exactly the same due time, including zero/MAX.
    for (n, row) in records.iter_mut().enumerate() {
        row.next_check_at = match n {
            0 => 0,
            4999 => u64::MAX,
            _ => 1000 + n as u64 / 2,
        };
    }
    store.save(records[0].clone()).unwrap();
    for chunk in records[1..].chunks(250) {
        insert_many(ready(&mut store), chunk).unwrap();
    }
    assert_backlog(&mut store, 0, 1, Some(0));
    assert_backlog(&mut store, 999, 1, Some(0));
    assert_backlog(&mut store, 1001, 4, Some(0));
    assert_backlog(&mut store, u64::MAX, 5000, Some(0));
    records[0].monitored = false;
    store.save(records[0].clone()).unwrap();
    assert_backlog(&mut store, 1001, 3, Some(1000));
    records[1].next_check_at = 5000;
    store.save(records[1].clone()).unwrap();
    assert_backlog(&mut store, 1001, 2, Some(1001));
    store.finish_due_pass(None, Some(1234)).unwrap();
    drop(store);
    let before = contents(&directory.0);
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert_backlog(&mut store, 1001, 2, Some(1001));
    assert_backlog(&mut store, 999, 0, None); // wall clock rollback
    assert_eq!(
        store.backlog(1001).unwrap().get("last_completed_pass_at"),
        Some(&Value::from("1234"))
    );
    drop(store);
    assert_eq!(contents(&directory.0), before);
}

#[test]
fn series_pass_freshness_advances_only_for_success_and_is_durable() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    let engine = Engine::open_for_management(config.clone()).unwrap();
    engine.refresh_series_due().unwrap(); // a completed empty pass is a success
    let read = |engine: &Engine| {
        engine
            .status()
            .unwrap()
            .get("inventory")
            .unwrap()
            .get("series")
            .unwrap()
            .get("last_completed_pass_at")
            .unwrap()
            .clone()
    };
    assert_ne!(read(&engine), Value::Null);
    {
        let mut store = lock(&engine.series_store).unwrap();
        // A fixed older success distinguishes subsequent calls in the same second.
        store.finish_due_pass(None, Some(123)).unwrap();
        let mut row = record(1, 1, 1, 1);
        row.next_check_at = 0;
        store.save(row).unwrap();
    }
    assert!(engine.refresh_series_due().is_err()); // no catalog credentials
    assert_eq!(read(&engine), Value::from("123"));
    engine.refresh_series_due_before(Instant::now()).unwrap();
    assert_eq!(read(&engine), Value::from("123"));
    engine
        .stopped
        .store(true, std::sync::atomic::Ordering::Release);
    engine.refresh_series_due().unwrap();
    assert_eq!(read(&engine), Value::from("123"));
    drop(engine);
    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(read(&engine), Value::from("123"));
}

#[test]
fn series_pass_checkpoint_and_freshness_recover_as_one_commit() {
    for fault in [
        Fault::Staged,
        Fault::Published,
        Fault::FirstPage,
        Fault::Applied,
        Fault::Unlinked,
    ] {
        let directory = Directory::new();
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let row = record(1, 1, 1, 1);
        store.save(row.clone()).unwrap();
        store.finish_due_pass(None, Some(10)).unwrap();
        let due = super::storage::Due {
            id: row.id,
            next_check_at: row.next_check_at,
        };
        ready(&mut store).inject(Some(fault));
        assert!(store.finish_due_pass(Some(&due), Some(20)).is_err());
        drop(store);
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let published = fault != Fault::Staged;
        assert_eq!(
            store.due_cursor(u64::MAX).unwrap(),
            published.then_some(due)
        );
        assert_eq!(
            store
                .backlog(u64::MAX)
                .unwrap()
                .get("last_completed_pass_at"),
            Some(&Value::from(if published { "20" } else { "10" }))
        );
    }
}

#[test]
fn series_backlog_status_is_explicit_and_redacted_when_unavailable() {
    use crate::engine::{Engine, lock};
    let directory = Directory::new();
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    let engine = Engine::open_for_management(config).unwrap();
    {
        let mut store = lock(&engine.series_store).unwrap();
        store.save(record(1, 1, 1, 1)).unwrap();
        ready(&mut store).inject(Some(Fault::Published));
        assert!(store.finish_due_pass(None, Some(10)).is_err());
    }
    let status = engine.status().unwrap();
    let backlog = status.get("inventory").unwrap().get("series").unwrap();
    assert_eq!(backlog.get("state"), Some(&Value::from("unavailable")));
    assert!(backlog.get("due").is_none());
    let report = json::stringify(backlog);
    assert!(!report.contains(directory.0.to_str().unwrap()));
    assert!(!report.contains("Synthetic"));
    assert!(
        backlog
            .get("error")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("restart with write access")
    );
    assert!(status.get("version").is_some());
}

#[test]
fn inventory_growth_report() {
    let Some(mut f) = crate::growth::Fixture::get("series") else {
        return;
    };
    let directory = f.directory.clone();
    let size = f.size;
    let mut store = f.measure("open_us", || SeriesStore::open(&directory, false).unwrap());
    if f.build {
        f.measure("build_us", || {
            store.save(record(1, 3, 30, 300)).unwrap();
            for start in (2..=size).step_by(20) {
                let rows: Vec<_> = (start..(start + 20).min(size + 1))
                    .map(|n| record(n, 3, 30, 300))
                    .collect();
                insert_many(ready(&mut store), &rows).unwrap();
            }
        });
    } else {
        assert_eq!(store.counts().unwrap().series, size);
        f.measure("page_us", || {
            assert_eq!(store.page(None, 200).unwrap().series.len(), 200);
        });
        f.measure("pass_us", || {
            assert_eq!(store.due(u64::MAX, 4).unwrap().len(), 4);
        });
        f.measure("update_us", || {
            let mut row = store.get(&identifier(size)).unwrap().unwrap();
            row.revision += 1;
            row.next_check_at += 1;
            store.save(row).unwrap();
        });
        let mut cursor = None;
        let mut count = 0;
        f.measure("walk_us", || {
            loop {
                let page = store.page(cursor.as_deref(), 200).unwrap();
                count += page.series.len() as u64;
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        });
        assert_eq!(count, size);
    }
    f.finish();
}

#[test]
fn episode_runtime_round_trips_and_old_plans_remain_readable() {
    let mut ep = episode(1, 1, 1, 0);
    ep.runtime_minutes = Some(47);
    assert_eq!(Episode::from_json(&ep.to_json()).unwrap(), ep);
    let mut old = ep.to_json();
    if let Value::Object(fields) = &mut old {
        fields.remove("runtime_minutes");
    }
    assert_eq!(Episode::from_json(&old).unwrap().runtime_minutes, None);
    let mut plan = record(1, 1, 1, 0);
    plan.plan.episodes[0].runtime_minutes = Some(47);
    assert_eq!(
        plan.episode_request(&plan.plan.episodes[0]).runtime_minutes,
        Some(47)
    );
}
