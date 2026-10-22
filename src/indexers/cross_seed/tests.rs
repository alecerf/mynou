//! Discovery matching, mapping, requests and storage.
use super::{Run, map_files, normalize, store};
use crate::{
    json::{self, Value},
    torrent::{CandidateLayout, ReuseSource},
};
use std::path::PathBuf;

fn source(files: &[(&str, u64)]) -> ReuseSource {
    ReuseSource {
        id: "a".repeat(40),
        name: "Show".into(),
        files: files
            .iter()
            .enumerate()
            // Odd original indices: padding files leave gaps in real metadata.
            .map(|(i, (path, length))| (2 * i + 1, PathBuf::from(path), *length))
            .collect(),
        total: files.iter().map(|f| f.1).sum(),
        ready: true,
        paused: false,
    }
}
fn layout(files: &[(&str, u64)]) -> CandidateLayout {
    CandidateLayout {
        id: "b".repeat(40),
        files: files
            .iter()
            .map(|(path, length)| (PathBuf::from(path), *length))
            .collect(),
    }
}

#[test]
fn names_compare_without_case_separators_or_video_extension() {
    assert_eq!(
        normalize("Some.Movie.2020.1080p.BluRay.x264-GRP.mkv"),
        "some movie 2020 1080p bluray x264 grp"
    );
    assert_eq!(
        normalize(" Some Movie 2020 1080p BluRay x264-GRP "),
        normalize("some_movie_2020_1080p_bluray_x264_grp")
    );
    assert_eq!(normalize("Pack.S01.nfo"), "pack s01 nfo");
    assert_eq!(normalize("Épisode.S01E02"), "épisode s01e02");
    assert_eq!(normalize("..."), "");
}

#[test]
fn mapping_uses_lengths_then_names_then_paths_and_refuses_guesses() {
    let src = source(&[("Show/a.mkv", 10), ("Show/b.nfo", 3), ("Show/c.srt", 3)]);
    // Renamed root, reordered files and a subset of the source.
    assert_eq!(
        map_files(&src, &layout(&[("Other/c.srt", 3), ("Other/a.mkv", 10)])),
        Ok(vec![5, 1])
    );
    assert_eq!(
        map_files(&src, &layout(&[("Other/a.mkv", 11)])),
        Err("layout_mismatch")
    );
    // Two equal lengths without a distinguishing name.
    assert_eq!(
        map_files(&src, &layout(&[("Other/x.txt", 3)])),
        Err("ambiguous_layout")
    );
    // One source file can back only one candidate file.
    assert_eq!(
        map_files(&src, &layout(&[("Other/a.mkv", 10), ("Other/z.mkv", 10)])),
        Err("layout_mismatch")
    );
    let nested = source(&[("Show/s1/e.mkv", 4), ("Show/s2/e.mkv", 4)]);
    assert_eq!(
        map_files(
            &nested,
            &layout(&[("Pack/s2/e.mkv", 4), ("Pack/s1/e.mkv", 4)])
        ),
        Ok(vec![3, 1])
    );
    assert_eq!(map_files(&src, &layout(&[])), Err("layout_mismatch"));
}

#[test]
fn run_requests_are_strict() {
    let parse = |text: &str| Run::from_json(&json::parse(text).unwrap());
    let run = parse(&format!(
        r#"{{"dry_run":true,"force":true,"indexer":"tracker-a","transfer":"{}"}}"#,
        "A".repeat(40)
    ))
    .unwrap();
    assert!(run.dry_run && run.force);
    assert_eq!(run.indexer.as_deref(), Some("tracker-a"));
    assert_eq!(run.transfer, Some("a".repeat(40)));
    assert!(parse("{}").unwrap().transfer.is_none());
    for bad in [
        r#"{"dry_run":"yes"}"#,
        r#"{"transfer":"abc"}"#,
        r#"{"indexer":""}"#,
        r#"{"unknown":true}"#,
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn configuration_requires_a_feed_or_torznab_search() {
    let parse = |section: &str, kind: &str| {
        let value = json::parse(&format!(
            r#"{{"kind":"{kind}","url":"http://127.0.0.1/api","cross_seed":{section}}}"#
        ))
        .unwrap();
        crate::indexers::Options::from_json(&value, "http://127.0.0.1/api").map(|o| o.cross_seed)
    };
    let torznab = parse(r#"{"enabled":true}"#, "torznab").unwrap();
    assert!(torznab.enabled && torznab.feed && torznab.search);
    assert_eq!(
        (torznab.interval_secs, torznab.search_interval_secs),
        (3_600, None)
    );
    let rss = parse(r#"{"enabled":true}"#, "rss").unwrap();
    assert!(rss.feed && !rss.search);
    let custom = parse(
        r#"{"enabled":true,"feed":false,"interval_secs":60,"search_interval_secs":86400}"#,
        "torznab",
    )
    .unwrap();
    assert_eq!(
        (custom.feed, custom.search_interval_secs),
        (false, Some(86_400))
    );
    for (bad, kind) in [
        (r#"{"enabled":true}"#, "json"),
        (r#"{"enabled":true,"search":true}"#, "rss"),
        (r#"{"enabled":true,"feed":false,"search":false}"#, "torznab"),
        (r#"{"interval_secs":59}"#, "torznab"),
        (r#"{"search_interval_secs":60}"#, "torznab"),
        (r#"{"enabled":"yes"}"#, "torznab"),
        (r#"{"cooldown":1}"#, "torznab"),
    ] {
        assert!(parse(bad, kind).is_err(), "{bad} {kind}");
    }
    // Disabled sections are validated but never require a capable source.
    assert!(!parse(r#"{"enabled":false}"#, "json").unwrap().enabled);
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn dir(name: &str) -> Dir {
    let path = std::env::temp_dir().join(format!(
        "mynou-cross-seed-discovery-{name}-{}-{}",
        std::process::id(),
        crate::store::now()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir(&path).unwrap();
    let path = path.canonicalize().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    Dir(path)
}
fn outcome(n: u64, reason: &str) -> store::Outcome {
    store::Outcome {
        at: n,
        indexer: "tracker-a".into(),
        trigger: "feed".into(),
        result: "rejected".into(),
        reason: reason.into(),
        source_transfer_id: Some("a".repeat(40)),
        candidate_id: None,
    }
}

#[test]
fn storage_is_created_on_first_write_bounded_and_durable() {
    let d = dir("storage");
    let mut s = store::DiscoveryStore::open(&d.0, true).unwrap();
    assert!(s.seen("tracker-a").unwrap().is_empty());
    assert!(s.commit(store::Batch::default()).is_ok());
    let mut batch = store::Batch::default();
    batch.seen.insert("x".into(), vec!["1".into()]);
    assert!(s.commit(batch).is_err(), "read-only stores never write");
    assert!(!d.0.join("cross-seed.idx").exists());

    let mut s = store::DiscoveryStore::open(&d.0, false).unwrap();
    let mut batch = store::Batch::default();
    batch.seen.insert(
        "tracker-a".into(),
        (0..store::MAX_SEEN + 5)
            .map(|n| format!("{n:032x}"))
            .collect(),
    );
    batch
        .searched
        .push(("tracker-a".into(), "a".repeat(40), 77));
    batch.added.push(store::Provenance {
        candidate_id: "b".repeat(40),
        source_transfer_id: "a".repeat(40),
        indexer: "tracker-a".into(),
        trigger: "completion".into(),
        at: 78,
    });
    batch.outcomes = (0..store::MAX_RECENT as u64 + 3)
        .map(|n| {
            outcome(
                n,
                if n % 2 == 0 {
                    "layout_mismatch"
                } else {
                    "unsupported_metadata"
                },
            )
        })
        .collect();
    s.commit(batch).unwrap();
    drop(s);

    let mut s = store::DiscoveryStore::open(&d.0, false).unwrap();
    let seen = s.seen("tracker-a").unwrap();
    assert_eq!(seen.len(), store::MAX_SEEN);
    assert_eq!(seen.front().unwrap(), &format!("{:032x}", 5));
    assert!(s.seen("tracker-b").unwrap().is_empty());
    assert_eq!(s.searched("tracker-a", &"a".repeat(40)).unwrap(), Some(77));
    assert_eq!(s.searched("tracker-b", &"a".repeat(40)).unwrap(), None);
    let (added, total) = s.added_page(50).unwrap();
    assert_eq!((added.len(), total), (1, 1));
    assert_eq!(added[0].trigger, "completion");
    let recent = s.recent().unwrap();
    assert_eq!(recent.len(), store::MAX_RECENT);
    assert_eq!(recent[0].at, 3);
    let stats = s.stats().unwrap();
    let rejected = stats.get("rejected").unwrap();
    assert_eq!(
        rejected.get("layout_mismatch").and_then(Value::as_str),
        Some("52")
    );
    assert_eq!(
        rejected.get("unsupported_metadata").and_then(Value::as_str),
        Some("51")
    );
    // Re-adding remembered identities keeps them once and in place.
    let mut batch = store::Batch::default();
    batch
        .seen
        .insert("tracker-a".into(), vec![format!("{:032x}", 5)]);
    s.commit(batch).unwrap();
    assert_eq!(s.seen("tracker-a").unwrap().len(), store::MAX_SEEN);
}
