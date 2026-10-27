//! Integration contracts tested with synthetic local HTTP services.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mynou::config::{self, Config, Source};
use mynou::integrations::{self, SweepCursor};
use mynou::store::Request;
use std::sync::Mutex;

struct Fixture {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn open(handler: impl Fn(&str, &[String]) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut headers = Vec::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push(line.trim().into());
                }
                let (status, body) = handler(&path, &headers);
                write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn config() -> Config {
    config::from_json(&config::default_json(), Path::new(".")).unwrap()
}

/// Reads a whole watchlist sweep page by page, as the service does in passes.
fn sweep(config: &Config) -> Result<(Vec<Request>, SweepCursor), String> {
    let mut cursor = SweepCursor::start();
    let mut requests = Vec::new();
    loop {
        let page = integrations::watchlist_page(config, &cursor)?;
        requests.extend(page.entries.into_iter().map(|(_, request)| request));
        if page.complete {
            return Ok((requests, page.next));
        }
        assert!(
            page.next.position > cursor.position,
            "sweep made no progress"
        );
        cursor = page.next;
    }
}

/// A Plex listing that honors `X-Plex-Container-Start` and serves at most
/// `page` items per response, whatever size was requested.
fn listing(path: &str, items: &[String], page: usize) -> String {
    let start = path
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("X-Plex-Container-Start="))
        .map_or(0, |start| start.parse::<usize>().unwrap());
    let size = path
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("X-Plex-Container-Size="))
        .map_or(page, |size| size.parse::<usize>().unwrap())
        .min(page);
    let end = items.len().min(start + size);
    let slice = &items[start.min(end)..end];
    format!(
        r#"{{"MediaContainer":{{"size":{},"totalSize":{},"Metadata":[{}]}}}}"#,
        slice.len(),
        items.len(),
        slice.join(",")
    )
}

fn watchlist_movie(id: usize) -> String {
    format!(
        r#"{{"type":"movie","title":"Synthetic {id}","year":2024,"ratingKey":"{id}","Guid":[{{"id":"tmdb://{}"}}]}}"#,
        id + 1
    )
}

fn movie() -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Movie".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: Some(42),
    }
}

#[test]
fn json_search_ignores_wrong_identity_and_chooses_viable_seeders() {
    let fixture = Fixture::open(|path, _| {
        assert!(path.contains("q=Fixture%20Movie"));
        (200, r#"{"results":[
            {"title":"Fixture.Movie.2023.1080p","seeders":999,"url":"magnet:?xt=urn:btih:wrong"},
            {"title":"Fixture.Movie.The.Sequel.2024","seeders":999,"url":"magnet:?xt=urn:btih:sequel"},
            {"title":"Fixture.Movie.2024.1080p","seeders":0,"url":"magnet:?xt=urn:btih:dead"},
            {"title":"Fixture.Movie.2024.720p","seeders":7,"download_url":"magnet:?xt=urn:btih:chosen"},
            {"title":"Fixture.Movie.2024.1080p","seeders":3,"url":"magnet:?xt=urn:btih:less"}] }"#.into())
    });
    let mut config = config();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "json".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into(),
    });
    assert_eq!(
        integrations::search(&config, &movie()).unwrap(),
        "magnet:?xt=urn:btih:chosen"
    );
}

#[test]
fn plex_watchlist_deduplicates_and_verifies_playable_library_entry() {
    let fixture = Fixture::open(|path, headers| {
        assert!(
            headers
                .iter()
                .any(|h| h.eq_ignore_ascii_case("X-Plex-Token: fixture-secret"))
        );
        let body = if path.starts_with("/watchlist") {
            r#"{"MediaContainer":{"size":2,"totalSize":2,"Metadata":[
                {"type":"movie","title":"Fixture Movie","year":2024,"Guid":[{"id":"tmdb://42"}]},
                {"type":"movie","title":"Fixture Movie","year":2024,"Guid":[{"id":"tmdb://42"}]}]}}"#
        } else if path.starts_with("/library/sections/1/all") {
            r#"{"MediaContainer":{"size":2,"Metadata":[
                {"title":"Fixture Movie","year":2024,"Guid":[{"id":"tmdb://999"}],"Media":[{"Part":[{"file":"wrong.mp4"}]}]},
                {"title":"Fixture Movie","year":2024,"Guid":[{"id":"tmdb://42"}],"Media":[{"Part":[{"file":"fixture.mp4"}]}]}]}}"#
        } else if path.starts_with("/library/sections/1/refresh") {
            ""
        } else {
            panic!("unexpected path: {path}");
        };
        (200, body.into())
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.url = fixture.url.clone();
    config.plex.watchlist_url = format!("{}/watchlist", fixture.url);
    config.plex.token_override = Some("fixture-secret".into());
    let (requests, end) = sweep(&config).unwrap();
    assert!(end.consistent);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tmdb_id, Some(42));
    integrations::refresh(&config, &requests[0]).unwrap();
    assert!(integrations::available(&config, &requests[0]).unwrap());
}

#[test]
fn tmdb_expands_only_aired_episodes_and_rejects_disabled_catalog() {
    let fixture = Fixture::open(|path, headers| {
        assert!(
            headers
                .iter()
                .any(|h| h.starts_with("Authorization: Bearer "))
        );
        let body = if path.starts_with("/3/tv/42/season/1") {
            r#"{"episodes":[{"episode_number":1,"air_date":"2020-01-01"},{"episode_number":2,"air_date":"2200-01-01"},
                {"episode_number":3,"air_date":null},{"episode_number":4,"air_date":"2020-02-30"}]}"#
        } else if path.starts_with("/3/tv/42") {
            r#"{"first_air_date":"2020-01-01","seasons":[{"season_number":0},{"season_number":1,"air_date":"2020-01-01"},
                {"season_number":2,"air_date":"2200-01-01"}]}"#
        } else {
            panic!("unexpected path: {path}");
        };
        (200, body.into())
    });
    let mut config = config();
    let mut series = movie();
    series.kind = "series".into();
    series.year = 2020;
    assert!(integrations::expand(&config, &series).is_err());
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    // PATH exists in the build environment; fixtures need neither global
    // environment changes nor real credentials.
    config.catalog.token_env = "PATH".into();
    let episodes = integrations::expand(&config, &series).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!((episodes[0].season, episodes[0].episode), (1, 1));
    assert_eq!(episodes[0].tmdb_id, Some(42));
}

#[test]
fn malformed_indexer_and_plex_responses_do_not_report_ready() {
    let fixture = Fixture::open(|_, _| (200, "<!DOCTYPE rss><rss/>".into()));
    let mut config = config();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "rss".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into(),
    });
    assert!(integrations::search(&config, &movie()).is_err());
    config.plex.enabled = true;
    config.plex.url = fixture.url.clone();
    config.plex.token_override = Some("fixture".into());
    assert!(integrations::available(&config, &movie()).is_err());
}

#[test]
fn plex_pagination_fetches_remaining_items_and_rejects_ignored_offset() {
    let fixture = Fixture::open(|path, _| {
        let second = path.contains("X-Plex-Container-Start=1");
        let id = if second { 43 } else { 42 };
        (
            200,
            format!(
                r#"{{"MediaContainer":{{"size":1,"totalSize":2,"Metadata":[{{"type":"movie","title":"Fixture {id}","year":2024,"Guid":[{{"id":"tmdb://{id}"}}]}}]}}}}"#
            ),
        )
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.watchlist_url = format!("{}/watchlist", fixture.url);
    config.plex.token_override = Some("fixture".into());
    let (requests, end) = sweep(&config).unwrap();
    assert!(end.consistent);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tmdb_id, Some(43));

    let ignored = Fixture::open(|_, _| {
        (200, r#"{"MediaContainer":{"size":1,"totalSize":2,"Metadata":[{"type":"movie","title":"Fixture","year":2024}]}}"#.into())
    });
    config.plex.watchlist_url = ignored.url.clone();
    assert!(sweep(&config).unwrap_err().contains("pagination"));
}

#[test]
fn plex_episode_readiness_requires_matching_show_season_episode_and_part() {
    let indexed = Arc::new(AtomicBool::new(false));
    let server_indexed = indexed.clone();
    let fixture = Fixture::open(move |path, _| {
        if path.starts_with("/library/sections/2/all") {
            (200, r#"{"MediaContainer":{"size":1,"Metadata":[{"title":"Fixture Movie","year":2024,"ratingKey":"17","Guid":[{"id":"tmdb://42"}]}]}}"#.into())
        } else if path.starts_with("/library/metadata/17/allLeaves") {
            let part = if server_indexed.load(Ordering::Acquire) {
                r#", "Media":[{"Part":[{"file":"fixture-s02e03.mp4"}]}]"#
            } else {
                ""
            };
            (
                200,
                format!(
                    r#"{{"MediaContainer":{{"size":3,"Metadata":[
                {{"parentIndex":1,"index":3,"Media":[{{"Part":[{{"file":"wrong-season.mp4"}}]}}]}},
                {{"parentIndex":2,"index":4,"Media":[{{"Part":[{{"file":"wrong-episode.mp4"}}]}}]}},
                {{"parentIndex":2,"index":3{part}}}]}}}}"#
                ),
            )
        } else {
            panic!("unexpected path: {path}");
        }
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.url = fixture.url.clone();
    config.plex.token_override = Some("fixture".into());
    let mut episode = movie();
    episode.kind = "episode".into();
    episode.season = 2;
    episode.episode = 3;
    assert!(!integrations::available(&config, &episode).unwrap());
    indexed.store(true, Ordering::Release);
    assert!(integrations::available(&config, &episode).unwrap());
}

#[test]
fn torznab_network_feed_selects_enclosure_without_including_details_page() {
    let fixture = Fixture::open(|path, _| {
        assert!(path.contains("t=movie"));
        (
            200,
            r#"<rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><item>
            <title>Fixture.Movie.2024.1080p</title><link>https://example.invalid/details/1</link>
            <enclosure url="/download?id=1&amp;token=fixture" type="application/x-bittorrent"/>
            <torznab:attr name="seeders" value="3"/></item></channel></rss>"#
                .into(),
        )
    });
    let mut config = config();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "torznab".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into(),
    });
    assert_eq!(
        integrations::search(&config, &movie()).unwrap(),
        format!("{}/download?id=1&token=fixture", fixture.url)
    );
}

#[test]
fn catalog_ambiguous_titles_require_explicit_identity() {
    let fixture = Fixture::open(|path, _| {
        assert!(path.starts_with("/3/search/tv"));
        (
            200,
            r#"{"results":[{"id":42,"name":"Fixture Movie","first_air_date":"2024-01-01"},
            {"id":43,"name":"Fixture Movie","first_air_date":"2024-08-02"}]}"#
                .into(),
        )
    });
    let mut config = config();
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    config.catalog.token_env = "PATH".into();
    let mut series = movie();
    series.kind = "series".into();
    series.tmdb_id = None;
    assert!(
        integrations::expand(&config, &series)
            .unwrap_err()
            .contains("ambiguous")
    );
}

#[test]
fn catalog_http_cache_reuses_seasons_and_invalidates_source_and_credentials() {
    let details_count = Arc::new(AtomicUsize::new(0));
    let season_count = Arc::new(AtomicUsize::new(0));
    let server_details = details_count.clone();
    let server_seasons = season_count.clone();
    let fixture = Fixture::open(move |path, headers| {
        assert!(
            headers
                .iter()
                .any(|h| h.starts_with("Authorization: Bearer "))
        );
        if path.contains("/tv/42/season/1") {
            server_seasons.fetch_add(1, Ordering::Relaxed);
            (
                200,
                r#"{"episodes":[{"episode_number":1,"air_date":"2020-01-01"}]}"#.into(),
            )
        } else if path.contains("/tv/42") {
            server_details.fetch_add(1, Ordering::Relaxed);
            (200, r#"{"first_air_date":"2020-01-01","seasons":[{"season_number":1,"air_date":"2020-01-01"}]}"#.into())
        } else {
            panic!("unexpected path: {path}");
        }
    });
    let mut config = config();
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    config.catalog.token_env = "PATH".into();
    let mut series = movie();
    series.kind = "series".into();
    series.year = 2020;
    assert_eq!(integrations::expand(&config, &series).unwrap().len(), 1);
    assert_eq!(integrations::expand(&config, &series).unwrap().len(), 1);
    assert_eq!(details_count.load(Ordering::Relaxed), 1);
    assert_eq!(season_count.load(Ordering::Relaxed), 1);

    config.catalog.url = format!("{}/changed-source", fixture.url);
    integrations::expand(&config, &series).unwrap();
    assert_eq!(details_count.load(Ordering::Relaxed), 2);
    assert_eq!(season_count.load(Ordering::Relaxed), 2);
    assert_ne!(
        std::env::var("PATH").unwrap(),
        std::env::var("HOME").unwrap()
    );
    config.catalog.token_env = "HOME".into();
    integrations::expand(&config, &series).unwrap();
    assert_eq!(details_count.load(Ordering::Relaxed), 3);
    assert_eq!(season_count.load(Ordering::Relaxed), 3);

    config.catalog.token_env = "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into();
    assert!(integrations::expand(&config, &series).is_err());
    assert_eq!(details_count.load(Ordering::Relaxed), 3);
}

#[test]
fn catalog_http_errors_and_malformed_json_are_never_cached() {
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = requests.clone();
    let fixture = Fixture::open(move |_, _| {
        let count = server_requests.fetch_add(1, Ordering::Relaxed);
        if count < 2 {
            (503, "{}".into())
        } else {
            (200, "{invalid".into())
        }
    });
    let mut config = config();
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    config.catalog.token_env = "PATH".into();
    let mut series = movie();
    series.kind = "series".into();
    for _ in 0..4 {
        assert!(integrations::expand(&config, &series).is_err());
    }
    assert_eq!(requests.load(Ordering::Relaxed), 4);
}

#[test]
fn watchlist_sweep_reaches_every_item_beyond_former_item_and_page_limits() {
    // 100,100 items served 100 per response: more items than the former
    // 100,000-item total and more pages than the former 1,000-page total.
    let items: Arc<Vec<String>> = Arc::new((0..100_100).map(watchlist_movie).collect());
    let served = Arc::new(AtomicUsize::new(0));
    let largest = Arc::new(AtomicUsize::new(0));
    let (listing_items, requests, widest) = (items.clone(), served.clone(), largest.clone());
    let fixture = Fixture::open(move |path, _| {
        requests.fetch_add(1, Ordering::AcqRel);
        let body = listing(path, &listing_items, 100);
        widest.fetch_max(body.matches(r#""type":"movie""#).count(), Ordering::AcqRel);
        (200, body)
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.watchlist_url = format!("{}/watchlist", fixture.url);
    config.plex.token_override = Some("fixture".into());
    let (requests, end) = sweep(&config).unwrap();
    assert!(end.consistent);
    assert_eq!(end.position, 100_100);
    assert_eq!(requests.len(), 100_100);
    let ids: std::collections::BTreeSet<_> = requests.iter().map(|r| r.tmdb_id).collect();
    assert_eq!(ids.len(), 100_100);
    // Every response held one page; each later page re-reads one anchor item.
    assert_eq!(largest.load(Ordering::Acquire), 100);
    let pages = served.load(Ordering::Acquire);
    assert!((1_001..=1_012).contains(&pages), "{pages} requests");
}

#[test]
fn watchlist_sweeps_resume_after_errors_and_flag_changed_or_shrinking_listings() {
    let items = Arc::new(Mutex::new((0..10).map(watchlist_movie).collect::<Vec<_>>()));
    let failing = Arc::new(AtomicBool::new(false));
    let (served, fail) = (items.clone(), failing.clone());
    let fixture = Fixture::open(move |path, _| {
        if fail.load(Ordering::Acquire) {
            return (503, String::new());
        }
        (200, listing(path, &served.lock().unwrap(), 4))
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.watchlist_url = format!("{}/watchlist", fixture.url);
    config.plex.token_override = Some("fixture".into());
    let read = |cursor: &SweepCursor| integrations::watchlist_page(&config, cursor);

    // An upstream error leaves the cursor usable: the sweep resumes where it
    // stopped and still sees every item exactly once.
    let first = read(&SweepCursor::start()).unwrap();
    assert_eq!((first.entries.len(), first.complete), (4, false));
    failing.store(true, Ordering::Release);
    assert!(read(&first.next).is_err());
    failing.store(false, Ordering::Release);
    let mut cursor = first.next.clone();
    let mut seen: Vec<_> = first.entries.iter().map(|(_, r)| r.tmdb_id).collect();
    loop {
        let page = read(&cursor).unwrap();
        seen.extend(page.entries.iter().map(|(_, r)| r.tmdb_id));
        cursor = page.next;
        if page.complete {
            break;
        }
    }
    assert!(cursor.consistent);
    assert_eq!(seen, (1..=10).map(Some).collect::<Vec<_>>());
    // Each entry carries the cursor that resumes right after it.
    assert_eq!(first.entries[1].0.position, 2);
    let resumed = read(&first.entries[1].0).unwrap();
    assert_eq!(resumed.entries[0].1.tmdb_id, Some(3));

    // An item added before the cursor shifts the listing: the sweep still
    // reads on, but may not prove anything about removals.
    let first = read(&SweepCursor::start()).unwrap();
    items.lock().unwrap().insert(0, watchlist_movie(99));
    let mut cursor = first.next;
    let end = loop {
        let page = read(&cursor).unwrap();
        cursor = page.next;
        if page.complete {
            break cursor;
        }
    };
    assert!(!end.consistent);

    // A listing that shrank below the cursor ends the sweep inconsistently.
    let first = read(&SweepCursor::start()).unwrap();
    items.lock().unwrap().truncate(2);
    let page = read(&first.next).unwrap();
    assert!(page.complete && !page.next.consistent);

    // An unchanged listing ends consistently.
    let (all, end) = sweep(&config).unwrap();
    assert_eq!(all.len(), 2);
    assert!(end.consistent);
}

#[test]
fn plex_availability_scans_listings_beyond_the_former_item_limit_one_page_at_a_time() {
    let matching = r#"{"title":"Fixture Movie","year":2024,"Guid":[{"id":"tmdb://42"}],"Media":[{"Part":[{"file":"/media/fixture.mp4"}]}]}"#;
    let mut items: Vec<String> = (0..100_000)
        .map(|n| {
            format!(
                r#"{{"title":"Fixture Movie","year":2024,"Guid":[{{"id":"tmdb://{}"}}]}}"#,
                n + 100
            )
        })
        .collect();
    items.push(matching.into());
    let items = Arc::new(Mutex::new(items));
    let served = Arc::new(AtomicUsize::new(0));
    let (listing_items, requests) = (items.clone(), served.clone());
    let fixture = Fixture::open(move |path, _| {
        assert!(path.starts_with("/library/sections/1/all"));
        requests.fetch_add(1, Ordering::AcqRel);
        (200, listing(path, &listing_items.lock().unwrap(), 200))
    });
    let mut config = config();
    config.plex.enabled = true;
    config.plex.url = fixture.url.clone();
    config.plex.token_override = Some("fixture".into());
    // The only playable copy is item 100,001: every page is read in turn.
    assert!(integrations::available(&config, &movie()).unwrap());
    assert_eq!(served.load(Ordering::Acquire), 501);
    let mut absent = movie();
    absent.tmdb_id = Some(7);
    served.store(0, Ordering::Release);
    assert!(!integrations::available(&config, &absent).unwrap());
    assert_eq!(served.load(Ordering::Acquire), 501);
    // A match on the first page stops reading at once.
    let mut listing_now = items.lock().unwrap();
    let last = listing_now.pop().unwrap();
    listing_now.insert(0, last);
    drop(listing_now);
    served.store(0, Ordering::Release);
    assert!(integrations::available(&config, &movie()).unwrap());
    assert_eq!(served.load(Ordering::Acquire), 1);
}

#[test]
fn catalog_search_lists_titles_years_and_synopses_without_invalid_entries() {
    let fixture = Fixture::open(|path, _| {
        assert!(path.starts_with("/3/search/tv?"), "{path}");
        assert!(path.contains("query=Fixture%20Show"), "{path}");
        assert!(path.contains("first_air_date_year=2024"), "{path}");
        (
            200,
            r#"{"results":[
            {"id":42,"name":"Fixture Show","original_name":"Spectacle","first_air_date":"2024-01-01","overview":"A show.","poster_path":"/abc123.jpg"},
            {"id":0,"name":"No identity"},
            {"id":43,"name":"","first_air_date":"2024-08-02"},
            {"id":44,"name":"Undated Show","original_name":"Undated Show","first_air_date":""}]}"#
                .into(),
        )
    });
    let mut config = config();
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    config.catalog.token_env = "PATH".into();
    let report = integrations::catalog_search(&config, "series", " Fixture Show ", 2024).unwrap();
    let results = report.get("results").unwrap().as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("tmdb_id").unwrap().as_u64(), Some(42));
    assert_eq!(results[0].get("kind").unwrap().as_str(), Some("series"));
    assert_eq!(
        results[0].get("title").unwrap().as_str(),
        Some("Fixture Show")
    );
    assert_eq!(
        results[0].get("original_title").unwrap().as_str(),
        Some("Spectacle")
    );
    assert_eq!(results[0].get("year").unwrap().as_u64(), Some(2024));
    assert_eq!(
        results[0].get("overview").unwrap().as_str(),
        Some("A show.")
    );
    assert_eq!(results[1].get("tmdb_id").unwrap().as_u64(), Some(44));
    assert_eq!(
        results[0].get("poster").unwrap().as_str(),
        Some("/abc123.jpg")
    );
    assert!(results[1].get("poster").is_none());
    assert!(results[1].get("original_title").is_none());
    assert!(results[1].get("year").unwrap().as_u64().is_none());
    for (kind, text) in [("episode", "Fixture"), ("movie", ""), ("movie", "a\u{7}b")] {
        assert!(integrations::catalog_search(&config, kind, text, 0).is_err());
    }
    config.catalog.enabled = false;
    assert!(integrations::catalog_search(&config, "movie", "Fixture", 0).is_err());
}

#[test]
fn a_chosen_release_is_found_again_by_its_candidate_id_even_when_rejected() {
    let fixture = Fixture::open(|_, _| {
        (
            200,
            r#"{"results":[
            {"title":"Fixture.Movie.2024.720p","seeders":7,"url":"magnet:?xt=urn:btih:chosen"},
            {"title":"Fixture.Movie.2024.1080p","seeders":0,"url":"magnet:?xt=urn:btih:dead"}] }"#
                .into(),
        )
    });
    let mut config = config();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "json".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into(),
    });
    let report = integrations::search_report(&config, &movie()).unwrap();
    let id = |list: &str| {
        report.get(list).unwrap().as_array().unwrap()[0]
            .get("id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let (accepted, ok) =
        integrations::pick_release_before(&config, &movie(), &id("accepted"), deadline).unwrap();
    assert!(ok);
    assert_eq!(accepted.url, "magnet:?xt=urn:btih:chosen");
    let (rejected, ok) =
        integrations::pick_release_before(&config, &movie(), &id("rejected"), deadline).unwrap();
    assert!(!ok);
    assert_eq!(rejected.url, "magnet:?xt=urn:btih:dead");
    assert_eq!(rejected.title, "Fixture.Movie.2024.1080p");
    for (id, error) in [
        ("0".repeat(64), "no longer offered"),
        ("../x".into(), "invalid candidate"),
    ] {
        let Err(refused) = integrations::pick_release_before(&config, &movie(), &id, deadline)
        else {
            panic!("{id} was accepted");
        };
        assert!(refused.contains(error), "{refused}");
    }
}

#[test]
fn catalog_posters_become_data_urls_only_for_jpeg_or_png_images() {
    use std::io::Read;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/t/p/w342", listener.local_addr().unwrap());
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut head = [0; 2048];
            let n = stream.read(&mut head).unwrap();
            let line = String::from_utf8_lossy(&head[..n]).to_string();
            count.fetch_add(1, Ordering::SeqCst);
            let body: &[u8] = if line.contains("/poster.jpg") {
                &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3]
            } else {
                b"<html>not an image</html>"
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let mut config = config();
    config.catalog.enabled = true;
    let poster = integrations::catalog_poster_from(&config, &base, "/poster.jpg").unwrap();
    assert_eq!(
        poster.get("data_url").unwrap().as_str(),
        Some("data:image/jpeg;base64,/9j/4AECAw==")
    );
    // A second read comes from memory.
    integrations::catalog_poster_from(&config, &base, "/poster.jpg").unwrap();
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert!(
        integrations::catalog_poster_from(&config, &base, "/page.jpg")
            .unwrap_err()
            .contains("not a JPEG or PNG")
    );
    for path in [
        "poster.jpg",
        "/../x.jpg",
        "/a.gif",
        "/a.jpg?x=1",
        "/",
        "/.jpg",
    ] {
        assert!(
            integrations::catalog_poster_from(&config, &base, path)
                .unwrap_err()
                .contains("invalid poster path"),
            "{path}"
        );
    }
    config.catalog.enabled = false;
    assert!(integrations::catalog_poster_from(&config, &base, "/poster.jpg").is_err());
}
