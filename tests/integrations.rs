//! Integration contracts tested with synthetic local HTTP services.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mynou::config::{self, Config, Source};
use mynou::integrations;
use mynou::store::Request;

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
    let requests = integrations::watchlist(&config).unwrap();
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
    let requests = integrations::watchlist(&config).unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tmdb_id, Some(43));

    let ignored = Fixture::open(|_, _| {
        (200, r#"{"MediaContainer":{"size":1,"totalSize":2,"Metadata":[{"type":"movie","title":"Fixture","year":2024}]}}"#.into())
    });
    config.plex.watchlist_url = ignored.url.clone();
    assert!(
        integrations::watchlist(&config)
            .unwrap_err()
            .contains("pagination")
    );
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
    config.catalog.api_key_env = "MYNOU_TEST_ABSENT_CREDENTIAL_239104".into();
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
