//! Discovery matching, mapping, requests and storage.
use super::{Numbered, Run, map_files, normalize, numbered, season_query, store};
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
        map_files(&src, &layout(&[("Other/c.srt", 3), ("Other/a.mkv", 10)]), 0),
        Ok(vec![Some(5), Some(1)])
    );
    assert_eq!(
        map_files(&src, &layout(&[("Other/a.mkv", 11)]), 0),
        Err("layout_mismatch")
    );
    // Two equal lengths without a distinguishing name.
    assert_eq!(
        map_files(&src, &layout(&[("Other/x.txt", 3)]), 0),
        Err("ambiguous_layout")
    );
    // One source file can back only one candidate file.
    assert_eq!(
        map_files(
            &src,
            &layout(&[("Other/a.mkv", 10), ("Other/z.mkv", 10)]),
            0
        ),
        Err("layout_mismatch")
    );
    let nested = source(&[("Show/s1/e.mkv", 4), ("Show/s2/e.mkv", 4)]);
    assert_eq!(
        map_files(
            &nested,
            &layout(&[("Pack/s2/e.mkv", 4), ("Pack/s1/e.mkv", 4)]),
            0
        ),
        Ok(vec![Some(3), Some(1)])
    );
    assert_eq!(map_files(&src, &layout(&[]), 0), Err("layout_mismatch"));
}

#[test]
fn missing_small_files_are_downloaded_within_the_bound_but_never_video() {
    let src = source(&[("Show/a.mkv", 10)]);
    let candidate = layout(&[
        ("Other/a.mkv", 10),
        ("Other/a.nfo", 5),
        ("Other/Sample/a-sample.mkv", 3),
    ]);
    assert_eq!(
        map_files(&src, &candidate, 8),
        Ok(vec![Some(1), None, None])
    );
    assert_eq!(map_files(&src, &candidate, 7), Err("too_much_missing"));
    // Without the bound nothing is missing: the old exact matching.
    assert_eq!(map_files(&src, &candidate, 0), Err("too_much_missing"));
    // A missing video, even small, is never downloaded.
    assert_eq!(
        map_files(
            &src,
            &layout(&[("Other/a.mkv", 10), ("Other/b.mkv", 1)]),
            100
        ),
        Err("layout_mismatch")
    );
    // Something must be reused.
    assert_eq!(
        map_files(&src, &layout(&[("Other/a.nfo", 5)]), 100),
        Err("layout_mismatch")
    );
}

#[test]
fn episodes_and_season_packs_parse_with_the_episode_markers() {
    let episode = |series: &str, season, episode| {
        Some(Numbered {
            series: series.into(),
            season,
            episode,
        })
    };
    assert_eq!(
        numbered("Show.Name.S01E02.1080p.WEB-GRP.mkv"),
        episode("show name", 1, Some(2))
    );
    assert_eq!(
        numbered("Show Name 1x02 HDTV"),
        episode("show name", 1, Some(2))
    );
    // A year kept in one name and dropped in the other still matches.
    assert_eq!(
        numbered("Show.Name.2019.S01.1080p.WEB-GRP"),
        episode("show name", 1, None)
    );
    assert_eq!(
        numbered("Show Name Season 2 1080p"),
        episode("show name", 2, None)
    );
    assert_eq!(numbered("Série.Saison.3.FRENCH"), episode("serie", 3, None));
    for several in [
        "Show.S01E01E02.1080p",
        "Show.S01E01-E02.1080p",
        "Show.S01-S02.1080p",
        "Show.S01.S01E03",
        "S01E01.1080p",
        "Movie.2020.1080p.BluRay",
    ] {
        assert_eq!(numbered(several), None, "{several}");
    }
    assert_eq!(
        season_query("Show.Name.2019.S01.1080p.WEB-GRP").as_deref(),
        Some("Show Name 2019 S01")
    );
    assert_eq!(season_query("Show.Name.S01E02.1080p.WEB-GRP"), None);
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
fn configuration_requires_a_feed_or_a_torznab_or_json_search() {
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
    let mapped = parse(r#"{"enabled":true}"#, "json").unwrap();
    assert!(mapped.enabled && mapped.feed && mapped.search);
    for (bad, kind) in [
        (r#"{"enabled":true,"feed":false,"search":false}"#, "json"),
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
    assert!(!parse(r#"{"enabled":false}"#, "rss").unwrap().enabled);
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

#[test]
fn temporary_metainfo_http_failure_is_retryable() {
    use crate::requesters::engine::regression_tests::HttpNetwork;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    let url = "http://indexer.fixture/torrent".to_owned();
    let source = crate::config::Source {
        name: "Synthetic tracker".into(),
        kind: "torznab".into(),
        url: url.clone(),
        api_key_env: String::new(),
        options: crate::indexers::Options {
            protection: crate::source_network::Protection::Protected,
            network: Some(Arc::new(HttpNetwork(|_| (503, String::new())))),
            ..Default::default()
        },
    };
    let error = crate::integrations::indexer_metainfo_before(
        &source,
        &url,
        Instant::now() + Duration::from_secs(3),
    )
    .unwrap_err();
    assert_ne!(error, "Indexer: response rejected");
    assert_ne!(error, "Indexer: invalid download");
}

fn offers(count: usize) -> Vec<crate::integrations::Offer> {
    (0..count)
        .map(|n| crate::integrations::Offer {
            id: n.to_string(),
            title: "Synthetic show".into(),
            url: format!("http://127.0.0.1/torrent/{n}"),
            size: Some(u64::MAX),
            info_hash: None,
        })
        .collect()
}

#[test]
fn completion_search_resumes_after_budget_exhaustion_and_restart() {
    for already_fetched in [0, super::MAX_FETCHES] {
        let d = dir(&format!("pending-{already_fetched}"));
        let mut fetched = already_fetched;
        let (pending, stopped) = super::unfinished_offers(offers(9), |_| {
            if fetched == super::MAX_FETCHES {
                return Some(false);
            }
            fetched += 1;
            Some(true)
        });
        assert!(!stopped);
        assert_eq!(pending.len(), 9 - (super::MAX_FETCHES - already_fetched));
        let mut batch = store::Batch::default();
        batch.finish_search("tracker", "transfer", pending, 7);
        let mut s = store::DiscoveryStore::open(&d.0, false).unwrap();
        s.commit(batch).unwrap();
        drop(s);
        let mut s = store::DiscoveryStore::open(&d.0, false).unwrap();
        assert_eq!(s.searched("tracker", "transfer").unwrap(), None);
        let mut processed = Vec::new();
        for at in 8..=9 {
            let Some(pending) = s.pending("tracker", "transfer").unwrap() else {
                break;
            };
            fetched = 0;
            let (pending, stopped) = super::unfinished_offers(pending, |offer| {
                if fetched == super::MAX_FETCHES {
                    return Some(false);
                }
                fetched += 1;
                processed.push(offer.id.clone());
                Some(true)
            });
            assert!(!stopped);
            let mut batch = store::Batch::default();
            batch.finish_search("tracker", "transfer", pending, at);
            s.commit(batch).unwrap();
        }
        assert!(s.searched("tracker", "transfer").unwrap().is_some());
        assert!(s.pending("tracker", "transfer").unwrap().is_none());
        assert_eq!(processed.last().map(String::as_str), Some("8"));
        assert_eq!(processed.len(), 9 - (super::MAX_FETCHES - already_fetched));
    }
}

#[test]
fn completion_search_retains_the_failed_offer_and_unexamined_tail() {
    let (pending, stopped) =
        super::unfinished_offers(
            offers(4),
            |offer| {
                if offer.id == "1" { None } else { Some(true) }
            },
        );
    assert!(stopped);
    assert_eq!(
        pending
            .iter()
            .map(|offer| offer.id.as_str())
            .collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
}
