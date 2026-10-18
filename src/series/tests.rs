//! Original storage, migration, recovery and scale scenarios. CI only; all
//! catalogs here are synthetic and no network is used.
use super::{
    CalendarQuery, Episode, NumberingChoice, Plan, Record,
    legacy::MAX_LEGACY_SNAPSHOT,
    migration::{self, Stage},
    storage::{Backend, SeriesStore, forge_counts, insert_many, verify_record},
};
use crate::{
    crypto::{random_bytes, sha256},
    date,
    inventory::{Fault, Index},
    json::{self, Value},
    numbering::{EpisodeNumber, SourceNumber},
    store::Request,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
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
        source_numbering: None,
    }
}

fn episode(n: u64, season: u32, number: u32, title_length: usize) -> Episode {
    let base = date::day("2024-01-01").unwrap();
    let offset = i64::from((season * 31 + number * 7) % 300);
    Episode {
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

fn legacy_bytes(records: &[Record], schema: u32) -> Vec<u8> {
    let payload = Value::Array(
        records
            .iter()
            .map(|record| {
                let mut value = record.to_json();
                if schema == 1
                    && let Value::Object(fields) = &mut value
                {
                    fields.remove("anchors");
                    fields.remove("numbering");
                }
                value
            })
            .collect(),
    );
    let mut envelope = Value::object();
    envelope.insert("schema_version", schema);
    envelope.insert(
        "digest",
        super::hex(&sha256(json::stringify(&payload).as_bytes())),
    );
    envelope.insert("records", payload);
    json::stringify(&envelope).into_bytes()
}

fn write_legacy(directory: &Path, records: &[Record], schema: u32) -> Vec<u8> {
    let bytes = legacy_bytes(records, schema);
    let path = directory.join("series.json");
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    bytes
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
        verify_record(ready(store), record).unwrap();
    }
}

fn fail_at(stage: Stage) -> impl FnMut(Stage) -> crate::Result<()> {
    move |reached| {
        if reached == stage {
            Err(format!("Injected interruption at {reached:?}"))
        } else {
            Ok(())
        }
    }
}

fn marker_schema(directory: &Path) -> Option<u64> {
    let text = fs::read_to_string(directory.join("series.json")).ok()?;
    json::parse(&text)
        .ok()?
        .get("schema_version")
        .and_then(Value::as_u64)
}

fn sample() -> Vec<Record> {
    let mut records = vec![
        record(1, 2, 5, 30),
        record(2, 1, 70, 20),
        record(3, 3, 3, 40),
    ];
    // An overlapping season scope of series 1 sharing its retained identities.
    let mut scope = record(1, 0, 0, 0);
    scope.id = identifier(40);
    scope.plan.request = request(1, 2, 0);
    scope.plan.episodes = records[0]
        .plan
        .episodes
        .iter()
        .filter(|episode| episode.season == 2)
        .cloned()
        .collect();
    scope.anchors = super::numbering::initial_anchors(&scope.plan);
    scope.monitored = false;
    let choice = NumberingChoice {
        catalog_id: records[2].plan.episodes[1].catalog_id.unwrap(),
        catalog: EpisodeNumber {
            season: 1,
            episode: 2,
        },
        source: SourceNumber::Absolute(2),
    };
    records[2].numbering.insert(choice.catalog_id, choice);
    records[1].excluded.insert((1, 4));
    records[1].start_date = Some("2024-03-01".into());
    records[1].last_error = Some("Catalog unavailable".into());
    records.push(scope);
    records
}

#[test]
fn schema_one_and_two_snapshots_migrate_exactly_and_the_original_is_preserved() {
    for schema in [1, 2] {
        let directory = Directory::new();
        let mut records = sample();
        let original = write_legacy(&directory.0, &records, schema);
        if schema == 1 {
            // Schema 1 predates numbering history: identities are inferred.
            for record in &mut records {
                record.numbering.clear();
                record.anchors = super::numbering::initial_anchors(&record.plan);
            }
        }
        // Read-only opening neither migrates nor creates anything.
        let before = contents(&directory.0);
        let mut preview = SeriesStore::open(&directory.0, true).unwrap();
        assert!(
            preview
                .get(&records[0].id)
                .unwrap_err()
                .contains("not been migrated")
        );
        assert!(preview.page(None, 10).is_err());
        assert!(preview.save(records[0].clone()).is_err());
        assert_eq!(
            preview.describe().get("state"),
            Some(&Value::from("needs_migration"))
        );
        drop(preview);
        assert_eq!(contents(&directory.0), before);

        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let report = store.migration.clone().unwrap();
        assert_eq!(
            (report.records, report.resumed),
            (records.len() as u64, false)
        );
        // Schema 1 carries no anchors: they are inferred, then stored.
        assert_consistent(&mut store, &records);
        assert_eq!(
            fs::read(directory.0.join(&report.backup)).unwrap(),
            original
        );
        assert_eq!(marker_schema(&directory.0), Some(3));
        assert!(!directory.0.join("series.idx.migrating").exists());
        let described = store.describe();
        assert_eq!(
            described.get("migration").and_then(|m| m.get("records")),
            Some(&Value::Number(records.len() as f64))
        );
        drop(store);

        // Reopening neither migrates again nor changes any byte.
        let before = contents(&directory.0);
        let mut again = SeriesStore::open(&directory.0, true).unwrap();
        assert!(again.migration.is_none());
        assert_consistent(&mut again, &records);
        drop(again);
        assert_eq!(contents(&directory.0), before);
    }
}

#[test]
fn migration_resumes_after_an_interruption_at_every_stage() {
    let records = sample();
    let stages = [
        Stage::BackupWritten,
        Stage::StagingCreated,
        Stage::Record(1),
        Stage::Record(2),
        Stage::Record(records.len() as u64),
        Stage::Verified,
        Stage::Promoted,
        Stage::MarkerWritten,
    ];
    for stage in stages {
        let directory = Directory::new();
        let original = write_legacy(&directory.0, &records, 2);
        let error = migration::open_with(&directory.0, false, &mut fail_at(stage))
            .err()
            .unwrap();
        assert!(error.contains("Injected"), "{stage:?}: {error}");
        // Until the marker, older binaries still find the exact legacy file.
        if stage != Stage::MarkerWritten {
            assert_eq!(fs::read(directory.0.join("series.json")).unwrap(), original);
        } else {
            assert_eq!(marker_schema(&directory.0), Some(3));
        }
        // A read-only look at any interrupted state changes nothing.
        let before = contents(&directory.0);
        if let Ok(mut preview) = SeriesStore::open(&directory.0, true) {
            let _ = preview.page(None, 10);
        }
        assert_eq!(contents(&directory.0), before, "{stage:?}");

        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        assert_consistent(&mut store, &records);
        assert_eq!(marker_schema(&directory.0), Some(3));
        let backup = format!("series.legacy-{}.json", super::hex(&sha256(&original)[..8]));
        assert_eq!(
            fs::read(directory.0.join(backup)).unwrap(),
            original,
            "{stage:?}"
        );
        if matches!(stage, Stage::Record(_) | Stage::Verified) {
            assert!(store.migration.as_ref().unwrap().resumed, "{stage:?}");
        }
        drop(store);
        // Idempotent: another open changes nothing.
        let before = contents(&directory.0);
        drop(SeriesStore::open(&directory.0, false).unwrap());
        assert_eq!(contents(&directory.0), before);
    }
}

/// Applies an edit to a legacy snapshot and seals it with a matching checksum.
fn resealed(bytes: &[u8], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut envelope = json::parse(std::str::from_utf8(bytes).unwrap()).unwrap();
    edit(&mut envelope);
    let digest = super::hex(&sha256(
        json::stringify(envelope.get("records").unwrap()).as_bytes(),
    ));
    envelope.insert("digest", digest);
    json::stringify(&envelope).into_bytes()
}

#[test]
fn invalid_legacy_snapshots_are_refused_without_any_write() {
    let valid = sample();
    let good = legacy_bytes(&valid, 2);
    // (name, bytes, the file is already refused when only inspected read-only)
    let mut cases: Vec<(&str, Vec<u8>, bool)> = Vec::new();
    cases.push((
        "checksum",
        String::from_utf8(good.clone())
            .unwrap()
            .replace("Synthetic Series 2", "Altered Series 2")
            .into_bytes(),
        true,
    ));
    cases.push((
        "schema",
        resealed(&good, |envelope| envelope.insert("schema_version", 9_u32)),
        true,
    ));
    let mut disagreeing = valid.clone();
    let id = *disagreeing[3].anchors.keys().next().unwrap();
    disagreeing[3].plan.episodes.clear();
    disagreeing[3].anchors = BTreeMap::from([(
        id,
        EpisodeNumber {
            season: 9,
            episode: 9,
        },
    )]);
    cases.push(("identity", legacy_bytes(&disagreeing, 2), false));
    let mut duplicate = valid.clone();
    duplicate[3].plan.request = duplicate[0].plan.request.clone();
    duplicate[3].plan.episodes.clear();
    duplicate[3].anchors.clear();
    cases.push(("duplicate scope", legacy_bytes(&duplicate, 2), false));
    for mode in [
        "missing",
        "unknown field",
        "duplicate anchor",
        "orphan choice",
    ] {
        cases.push((
            mode,
            resealed(&good, |envelope| {
                let Some(Value::Array(records)) = envelope.get_mut("records") else {
                    panic!("records");
                };
                match mode {
                    "missing" => {
                        if let Value::Object(fields) = &mut records[0] {
                            fields.remove("anchors");
                        }
                    }
                    "unknown field" => records[0].insert("surprise", true),
                    "duplicate anchor" => {
                        let anchor =
                            records[0].get("anchors").unwrap().as_array().unwrap()[0].clone();
                        records[0].insert("anchors", Value::Array(vec![anchor.clone(), anchor]));
                    }
                    _ => records[0].insert(
                        "numbering",
                        Value::Array(vec![
                            NumberingChoice {
                                catalog_id: 999,
                                catalog: EpisodeNumber {
                                    season: 1,
                                    episode: 1,
                                },
                                source: SourceNumber::Absolute(13),
                            }
                            .to_json(),
                        ]),
                    ),
                }
            }),
            false,
        ));
    }
    for (name, bytes, refused_read_only) in cases {
        let directory = Directory::new();
        let path = directory.0.join("series.json");
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before = contents(&directory.0);
        assert!(SeriesStore::open(&directory.0, false).is_err(), "{name}");
        assert_eq!(
            SeriesStore::open(&directory.0, true).is_err(),
            refused_read_only,
            "{name}"
        );
        assert_eq!(contents(&directory.0), before, "{name}");
    }
}

#[test]
fn conflicting_or_missing_state_is_refused_never_overwritten() {
    let records = sample();
    // A restored legacy snapshot that is not the one the index came from.
    let directory = Directory::new();
    write_legacy(&directory.0, &records, 2);
    drop(SeriesStore::open(&directory.0, false).unwrap());
    let other = write_legacy(&directory.0, &records[..2], 2);
    let before = contents(&directory.0);
    let error = SeriesStore::open(&directory.0, false).err().unwrap();
    assert!(error.contains("differs"), "{error}");
    assert_eq!(contents(&directory.0), before);
    assert_eq!(fs::read(directory.0.join("series.json")).unwrap(), other);

    // The marker without its index.
    let directory = Directory::new();
    write_legacy(&directory.0, &records, 2);
    drop(SeriesStore::open(&directory.0, false).unwrap());
    fs::remove_dir_all(directory.0.join("series.idx")).unwrap();
    let error = SeriesStore::open(&directory.0, false).err().unwrap();
    assert!(error.contains("index is missing"), "{error}");
    assert!(SeriesStore::open(&directory.0, true).is_err());

    // A marker bound to a different index.
    let first = Directory::new();
    let second = Directory::new();
    write_legacy(&first.0, &records, 2);
    write_legacy(&second.0, &records[..1], 2);
    drop(SeriesStore::open(&first.0, false).unwrap());
    drop(SeriesStore::open(&second.0, false).unwrap());
    fs::copy(first.0.join("series.json"), second.0.join("series.json")).unwrap();
    assert!(
        SeriesStore::open(&second.0, false)
            .err()
            .unwrap()
            .contains("different series index")
    );

    // A preserved snapshot name that holds other bytes is never replaced.
    let directory = Directory::new();
    let original = write_legacy(&directory.0, &records, 2);
    let name = format!("series.legacy-{}.json", super::hex(&sha256(&original)[..8]));
    fs::write(directory.0.join(&name), b"someone else's data").unwrap();
    fs::set_permissions(directory.0.join(&name), fs::Permissions::from_mode(0o600)).unwrap();
    let before = contents(&directory.0);
    assert!(
        SeriesStore::open(&directory.0, false)
            .err()
            .unwrap()
            .contains("refusing to overwrite")
    );
    assert_eq!(contents(&directory.0), before);

    // Staging built from other bytes is derived data and is rebuilt.
    let directory = Directory::new();
    write_legacy(&directory.0, &records[..2], 2);
    migration::open_with(&directory.0, false, &mut fail_at(Stage::Record(1)))
        .err()
        .unwrap();
    write_legacy(&directory.0, &records, 2);
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    assert!(!store.migration.as_ref().unwrap().resumed);
    assert_consistent(&mut store, &records);
}

#[test]
fn empty_stores_create_nothing_and_the_first_write_publishes_index_and_marker() {
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
    assert_eq!(marker_schema(&directory.0), Some(3));
    drop(store);
    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    assert_eq!(store.counts().unwrap().series, 1);
    drop(store);

    // An interrupted first creation leaves only derived staging, replaced later.
    let directory = Directory::new();
    drop(Index::create(&directory.0.join("series.idx.migrating")).unwrap());
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(record(2, 1, 2, 10)).unwrap();
    assert!(!directory.0.join("series.idx.migrating").exists());
    // A published index whose marker was never written is completed on open.
    drop(store);
    fs::remove_file(directory.0.join("series.json")).unwrap();
    let store = SeriesStore::open(&directory.0, false).unwrap();
    assert_eq!(marker_schema(&directory.0), Some(3));
    drop(store);
}

#[test]
fn inventories_above_every_former_limit_page_completely_and_survive_restart() {
    let directory = Directory::new();
    let scopes = 129;
    let records: Vec<Record> = (1..=scopes).map(|n| record(n, 2, 78, 300)).collect();
    let episodes: usize = records.iter().map(|r| r.plan.episodes.len()).sum();
    let snapshot = legacy_bytes(&records, 2).len();
    // More scopes, retained episodes and bytes than the old snapshot allowed.
    assert!(records.len() > 128);
    assert!(episodes > 20_000, "{episodes}");
    assert!(snapshot > MAX_LEGACY_SNAPSHOT, "{snapshot}");

    let started = Instant::now();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(records[0].clone()).unwrap();
    insert_many(ready(&mut store), &records[1..]).unwrap();
    assert_eq!(store.counts().unwrap().series, scopes);
    eprintln!(
        "series-above-limits scopes={scopes} episodes={episodes} snapshot_bytes={snapshot} load_ms={}",
        started.elapsed().as_millis()
    );
    drop(store);

    let mut store = SeriesStore::open(&directory.0, true).unwrap();
    let mut expected = records.clone();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(load_all(&mut store), expected);
    for record in [&expected[0], &expected[64], &expected[128]] {
        verify_record(ready(&mut store), record).unwrap();
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

    let before = contents(&directory.0);
    // The same catalog identity under another number.
    let mut moved = season.clone();
    moved.id = identifier(51);
    moved.plan.request = request(1, 3, 0);
    let shared = *season.anchors.keys().next().unwrap();
    moved.plan.episodes = vec![Episode {
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
fn empty_and_old_limit_snapshots_migrate_and_counters_never_underflow() {
    // An empty snapshot is a valid snapshot.
    let directory = Directory::new();
    let original = write_legacy(&directory.0, &[], 2);
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let report = store.migration.clone().unwrap();
    assert_eq!((report.records, report.episodes), (0, 0));
    assert_eq!(store.counts().unwrap().series, 0);
    assert_eq!(
        fs::read(directory.0.join(&report.backup)).unwrap(),
        original
    );
    assert_eq!(marker_schema(&directory.0), Some(3));
    store.save(record(1, 1, 2, 10)).unwrap();
    drop(store);

    // A snapshot at the old limits: 128 scopes, just under 20,000 episodes and
    // just under 8 MiB, the largest file the old store could have written.
    let directory = Directory::new();
    let records: Vec<Record> = (1..=128).map(|n| record(n, 2, 78, 300)).collect();
    let bytes = write_legacy(&directory.0, &records, 2);
    let episodes: usize = records.iter().map(|r| r.plan.episodes.len()).sum();
    assert_eq!(episodes, 19_968);
    assert!(bytes.len() > 7 * 1024 * 1024 && bytes.len() <= MAX_LEGACY_SNAPSHOT);
    let started = Instant::now();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    eprintln!(
        "series-migration-at-old-limits scopes=128 episodes={episodes} snapshot_bytes={} elapsed_ms={}",
        bytes.len(),
        started.elapsed().as_millis()
    );
    let mut expected = records.clone();
    expected.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(load_all(&mut store), expected);
    assert_eq!(store.counts().unwrap().series, 128);
    verify_record(ready(&mut store), &expected[127]).unwrap();
    drop(store);

    // Counters that disagree with the records are an error, not a panic or wrap.
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    store.save(record(1, 1, 2, 10)).unwrap();
    forge_counts(ready(&mut store), 1, 0).unwrap();
    let mut changed = record(1, 1, 2, 10);
    changed.monitored = false;
    changed.revision = 2;
    assert!(
        store
            .save(changed)
            .unwrap_err()
            .contains("counters disagree")
    );
    assert_eq!(store.counts().unwrap().monitored, 0);
}

#[test]
fn a_preserved_copy_must_be_a_private_regular_file_of_the_exact_size() {
    use std::os::unix::fs::symlink;
    let records = sample();
    let original = legacy_bytes(&records, 2);
    let name = format!("series.legacy-{}.json", super::hex(&sha256(&original)[..8]));
    for case in ["public", "hard link", "symlink", "oversized", "directory"] {
        let directory = Directory::new();
        write_legacy(&directory.0, &records, 2);
        let backup = directory.0.join(&name);
        match case {
            "public" => {
                fs::write(&backup, &original).unwrap();
                fs::set_permissions(&backup, fs::Permissions::from_mode(0o644)).unwrap();
            }
            "hard link" => {
                fs::write(&backup, &original).unwrap();
                fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
                fs::hard_link(&backup, directory.0.join("alias")).unwrap();
            }
            "symlink" => {
                fs::write(directory.0.join("target"), &original).unwrap();
                symlink(directory.0.join("target"), &backup).unwrap();
            }
            "oversized" => {
                fs::write(&backup, vec![b'x'; original.len() + 1]).unwrap();
                fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
            }
            _ => fs::create_dir(&backup).unwrap(),
        }
        let before = contents(&directory.0);
        assert!(SeriesStore::open(&directory.0, false).is_err(), "{case}");
        assert_eq!(contents(&directory.0), before, "{case}");
    }
    // The same bytes in a private file are accepted and reused.
    let directory = Directory::new();
    write_legacy(&directory.0, &records, 2);
    let backup = directory.0.join(&name);
    fs::write(&backup, &original).unwrap();
    fs::set_permissions(&backup, fs::Permissions::from_mode(0o600)).unwrap();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    assert_consistent(&mut store, &records);
}

/// The legacy snapshot, swapped after inspection, is refused at open time
/// without blocking, including for another regular file with the same bytes.
#[test]
fn a_swapped_legacy_snapshot_is_refused_at_open_without_blocking() {
    use crate::store::open_tests::{Swap, swapped};
    for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
        let dir = Directory::new();
        write_legacy(&dir.0, &sample(), 2);
        let path = dir.0.join("series.json");
        let error = swapped("series.json", 0, swap, move || {
            super::legacy::inspect(&path, false).map(drop)
        });
        match swap {
            Swap::Symlink => assert!(error.starts_with("Cannot open series snapshot"), "{error}"),
            Swap::Fifo => assert_eq!(error, "Invalid or oversized series snapshot"),
            Swap::Replaced => assert_eq!(error, "Series snapshot changed while opening"),
        }
    }
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

#[test]
fn full_disk_at_every_migration_flush_preserves_legacy_and_restarts() {
    for boundary in 0..1000 {
        let directory = Directory::new();
        let records = sample();
        let original = write_legacy(&directory.0, &records, 2);
        let result =
            crate::store::with_full_disk_after(boundary, || SeriesStore::open(&directory.0, false));
        let error = match result {
            Ok(value) => {
                drop(value);
                assert!(boundary > 10);
                return;
            }
            Err(error) => error,
        };
        assert!(
            error.starts_with("Series storage: "),
            "boundary {boundary}: {error}"
        );
        assert!(
            error.contains("disk") || error.contains(crate::store::STORAGE_FULL),
            "boundary {boundary}: {error}"
        );
        assert!(!error.contains(directory.0.to_str().unwrap()), "{error}");
        // The original remains at its live name until marker publication;
        // afterwards the exact private backup must already exist.
        let live = fs::read(directory.0.join("series.json")).unwrap();
        if live != original {
            assert!(
                fs::read_dir(&directory.0).unwrap().any(|entry| {
                    let path = entry.unwrap().path();
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".legacy-")
                        && fs::read(path).is_ok_and(|bytes| bytes == original)
                }),
                "boundary {boundary}: original snapshot lost"
            );
        }
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        assert_consistent(&mut store, &records);
    }
    panic!("migration fixture exceeded its durability boundary budget");
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
fn series_backlog_survives_snapshot_and_legacy_branch_upgrades() {
    for schema in [1, 2] {
        let directory = Directory::new();
        let records = sample();
        let original = write_legacy(&directory.0, &records, schema);
        let before = contents(&directory.0);
        let mut preview = SeriesStore::open(&directory.0, true).unwrap();
        assert!(preview.backlog(u64::MAX).is_err());
        drop(preview);
        assert_eq!(contents(&directory.0), before);
        let mut store = SeriesStore::open(&directory.0, false).unwrap();
        let monitored: Vec<_> = records.iter().filter(|r| r.monitored).collect();
        assert_backlog(
            &mut store,
            u64::MAX,
            monitored.len() as u64,
            monitored.iter().map(|r| r.next_check_at).min(),
        );
        assert_eq!(
            store.backlog(10000).unwrap().get("last_completed_pass_at"),
            Some(&Value::Null)
        );
        assert_eq!(
            fs::read(directory.0.join(&store.migration.as_ref().unwrap().backup)).unwrap(),
            original
        );
    }
    let directory = Directory::new();
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    let records: Vec<_> = (1..400).map(|n| record(n, 1, 1, 4)).collect();
    store.save(records[0].clone()).unwrap();
    insert_many(ready(&mut store), &records[1..]).unwrap();
    crate::inventory::legacy_range_index(ready(&mut store));
    drop(store);
    let before = contents(&directory.0);
    let mut preview = SeriesStore::open(&directory.0, true).unwrap();
    assert!(
        preview
            .backlog(u64::MAX)
            .unwrap_err()
            .contains("explicit writable upgrade")
    );
    drop(preview);
    assert_eq!(contents(&directory.0), before);
    // Fail the first upgrade flush, then resume via normal writable startup.
    assert!(
        crate::store::with_full_disk_after(0, || SeriesStore::open(&directory.0, false)).is_err()
    );
    let mut store = SeriesStore::open(&directory.0, false).unwrap();
    assert_backlog(&mut store, u64::MAX, 399, Some(1001));
    for row in &records {
        assert_eq!(store.get(&row.id).unwrap().unwrap(), *row);
    }
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
#[ignore = "opt-in inventory_growth.py measurements"]
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
