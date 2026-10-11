//! Original storage, migration, recovery and scale scenarios. CI only; all
//! catalogs here are synthetic and no network is used.
use super::{
    CalendarQuery, Episode, NumberingChoice, Plan, Record,
    legacy::MAX_LEGACY_SNAPSHOT,
    migration::{self, Stage},
    storage::{Backend, SeriesStore, insert_many, verify_record},
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
    save_widest(100);
}

#[test]
#[ignore = "worst block spread; run with --ignored"]
fn the_widest_possible_block_spread_still_fits_one_transaction() {
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
/// -- --ignored --nocapture`. The phases run in separate processes so the
/// peak resident size of reading an existing inventory is measured on its own.
#[test]
#[ignore = "measurement run; see the doc comment"]
fn series_scale_report() {
    let directory = PathBuf::from(std::env::var("MYNOU_SCALE_DIR").expect("MYNOU_SCALE_DIR"));
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
