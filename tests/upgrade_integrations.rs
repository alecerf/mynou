//! Upgrade decisions and Plex confirmation use synthetic local HTTP services.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mynou::config::{self, Config, PathMapping, Source};
use mynou::integrations;
use mynou::json::{self, Value};
use mynou::store::{RecordedRelease, Request};

struct Fixture {
    url: String,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn open(handler: impl Fn(&str, &[String]) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_calls = Arc::clone(&calls);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
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
                thread_calls.fetch_add(1, Ordering::Relaxed);
                let (status, body) = handler(&path, &headers);
                if let Err(error) = write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ) {
                    // Deadline tests intentionally close the client before a
                    // slow service can send its response.
                    assert!(matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                }
            }
        });
        Self {
            url,
            calls,
            stop,
            thread: Some(thread),
        }
    }

    fn mutable_library(items: Vec<Value>) -> (Self, Arc<Mutex<Vec<Value>>>) {
        let items = Arc::new(Mutex::new(items));
        let server_items = Arc::clone(&items);
        let fixture = Self::open(move |path, headers| {
            assert!(path.starts_with("/library/sections/1/all"));
            assert!(path.contains("includeGuids=1"));
            assert!(headers.iter().any(|header| {
                header.eq_ignore_ascii_case("X-Plex-Token: fixture-upgrade-secret")
            }));
            (200, container(server_items.lock().unwrap().clone(), None))
        });
        (fixture, items)
    }

    fn plex_config(&self) -> Config {
        let mut config = config::from_json(&config::default_json(), Path::new(".")).unwrap();
        config.plex.enabled = true;
        config.plex.url = self.url.clone();
        config.plex.token_override = Some("fixture-upgrade-secret".into());
        config
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

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

fn container(items: Vec<Value>, total: Option<usize>) -> String {
    let mut inner = object([
        ("size", Value::Number(items.len() as f64)),
        ("Metadata", Value::Array(items)),
    ]);
    if let Some(total) = total {
        inner.insert("totalSize", Value::Number(total as f64));
    }
    json::stringify(&object([("MediaContainer", inner)]))
}

fn media(paths: &[&str]) -> Value {
    Value::Array(vec![object([(
        "Part",
        Value::Array(
            paths
                .iter()
                .map(|path| object([("file", (*path).into())]))
                .collect(),
        ),
    )])])
}

fn movie_entry(paths: &[&str], id: u64) -> Value {
    object([
        ("title", "Fixture Movie".into()),
        ("year", Value::Number(2024.0)),
        (
            "Guid",
            Value::Array(vec![object([("id", format!("tmdb://{id}").into())])]),
        ),
        ("Media", media(paths)),
    ])
}

fn episode_entry(season: u32, episode: u32, path: &str) -> Value {
    object([
        ("parentIndex", Value::Number(f64::from(season))),
        ("index", Value::Number(f64::from(episode))),
        ("Media", media(&[path])),
    ])
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

fn imports(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).into()).collect()
}

#[test]
fn movie_upgrade_does_not_confirm_the_existing_playable_copy() {
    let (fixture, items) = Fixture::mutable_library(vec![movie_entry(&["/library/old.mp4"], 42)]);
    let config = fixture.plex_config();
    let new_import = imports(&["/library/new.mp4"]);
    assert!(integrations::available(&config, &movie()).unwrap());
    assert!(!integrations::available_import(&config, &movie(), &new_import).unwrap());
    items
        .lock()
        .unwrap()
        .push(movie_entry(&["/library/new.mp4"], 42));
    assert!(integrations::available_import(&config, &movie(), &new_import).unwrap());
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 3);
}

#[test]
fn every_import_must_be_indexed_under_the_requested_identity() {
    let (fixture, items) = Fixture::mutable_library(vec![
        movie_entry(&["/library/new-part-one.mp4"], 42),
        movie_entry(&["/library/new-part-two.mp4"], 999),
    ]);
    let config = fixture.plex_config();
    let new_imports = imports(&["/library/new-part-one.mp4", "/library/new-part-two.mp4"]);
    assert!(!integrations::available_import(&config, &movie(), &new_imports).unwrap());
    *items.lock().unwrap() = vec![movie_entry(
        &["/library/new-part-one.mp4", "/library/new-part-two.mp4"],
        42,
    )];
    assert!(integrations::available_import(&config, &movie(), &new_imports).unwrap());
}

#[test]
fn episode_upgrade_requires_the_new_part_in_the_correct_season_and_episode() {
    let indexed = Arc::new(AtomicBool::new(false));
    let server_indexed = Arc::clone(&indexed);
    let fixture = Fixture::open(move |path, _| {
        if path.starts_with("/library/sections/2/all") {
            let mut show = movie_entry(&[], 42);
            show.insert("ratingKey", "17");
            (200, container(vec![show], None))
        } else {
            assert!(path.starts_with("/library/metadata/17/allLeaves"));
            let mut episodes = vec![
                episode_entry(1, 3, "/library/new-s02e03.mp4"),
                episode_entry(2, 4, "/library/new-s02e03.mp4"),
                episode_entry(2, 3, "/library/old-s02e03.mp4"),
            ];
            if server_indexed.load(Ordering::Acquire) {
                episodes.push(episode_entry(2, 3, "/library/new-s02e03.mp4"));
            }
            (200, container(episodes, None))
        }
    });
    let config = fixture.plex_config();
    let mut episode = movie();
    episode.kind = "episode".into();
    episode.season = 2;
    episode.episode = 3;
    let new_import = imports(&["/library/new-s02e03.mp4"]);
    assert!(!integrations::available_import(&config, &episode, &new_import).unwrap());
    indexed.store(true, Ordering::Release);
    assert!(integrations::available_import(&config, &episode, &new_import).unwrap());
}

#[test]
fn longest_mapping_uses_components_and_does_not_match_a_similar_directory_name() {
    let (fixture, items) =
        Fixture::mutable_library(vec![movie_entry(&["/plex/all/movies/Fixture/new.mp4"], 42)]);
    let mut config = fixture.plex_config();
    config.plex.path_mappings = vec![
        PathMapping {
            mynou_prefix: "/mynou".into(),
            plex_prefix: "/plex/all".into(),
        },
        PathMapping {
            mynou_prefix: "/mynou/movies".into(),
            plex_prefix: "/plex/films".into(),
        },
    ];
    let new_import = imports(&["/mynou/movies/Fixture/new.mp4"]);
    assert!(!integrations::available_import(&config, &movie(), &new_import).unwrap());
    *items.lock().unwrap() = vec![movie_entry(&["/plex/films/Fixture/new.mp4"], 42)];
    assert!(integrations::available_import(&config, &movie(), &new_import).unwrap());

    let similar_directory = imports(&["/mynou/movies-extra/Fixture/new.mp4"]);
    *items.lock().unwrap() = vec![movie_entry(&["/plex/films-extra/Fixture/new.mp4"], 42)];
    assert!(!integrations::available_import(&config, &movie(), &similar_directory).unwrap());
    *items.lock().unwrap() = vec![movie_entry(&["/plex/all/movies-extra/Fixture/new.mp4"], 42)];
    assert!(integrations::available_import(&config, &movie(), &similar_directory).unwrap());
}

#[test]
fn root_mapping_preserves_the_entire_local_suffix() {
    let (fixture, _) =
        Fixture::mutable_library(vec![movie_entry(&["/plex/library/mynou/new.mp4"], 42)]);
    let mut config = fixture.plex_config();
    config.plex.path_mappings = vec![PathMapping {
        mynou_prefix: "/".into(),
        plex_prefix: "/plex/library".into(),
    }];
    assert!(
        integrations::available_import(&config, &movie(), &imports(&["/mynou/new.mp4"])).unwrap()
    );
}

#[test]
fn absent_mapping_requires_the_exact_absolute_path_and_never_a_basename_or_suffix() {
    let (fixture, items) = Fixture::mutable_library(Vec::new());
    let config = fixture.plex_config();
    let new_import = imports(&["/library/new.mp4"]);
    for path in [
        "new.mp4",
        "/new.mp4",
        "/other/library/new.mp4",
        "/library-old/new.mp4",
    ] {
        *items.lock().unwrap() = vec![movie_entry(&[path], 42)];
        assert!(!integrations::available_import(&config, &movie(), &new_import).unwrap());
    }
    *items.lock().unwrap() = vec![movie_entry(&["/library/new.mp4"], 42)];
    assert!(integrations::available_import(&config, &movie(), &new_import).unwrap());
}

#[test]
fn malformed_paths_cannot_confirm_an_upgrade_or_leak_into_errors() {
    let (fixture, items) = Fixture::mutable_library(Vec::new());
    let config = fixture.plex_config();
    let new_import = imports(&["/library/new.mp4"]);
    for path in [
        "library/new.mp4",
        "/library/../library/new.mp4",
        "/library/./new.mp4",
        "/library//new.mp4",
        "/library/new.mp4/",
        "/library\\new.mp4",
        "/library/secret-token\nnew.mp4",
    ] {
        *items.lock().unwrap() = vec![movie_entry(&[path], 42)];
        assert!(!integrations::available_import(&config, &movie(), &new_import).unwrap());
        let error =
            integrations::available_import(&config, &movie(), &imports(&[path])).unwrap_err();
        assert_eq!(error, "Plex: invalid imported file path");
        assert!(!error.contains("secret-token"));
    }
    assert!(integrations::available_import(&config, &movie(), &imports(&["/"])).is_err());
}

#[test]
fn disabled_plex_confirms_only_a_nonempty_import_list() {
    let config = config::from_json(&config::default_json(), Path::new(".")).unwrap();
    assert!(!integrations::available_import(&config, &movie(), &[]).unwrap());
    assert!(
        integrations::available_import(&config, &movie(), &imports(&["/library/new.mp4"])).unwrap()
    );
}

#[test]
fn structured_selection_agrees_with_search_and_the_redacted_public_preview() {
    let fixture = Fixture::open(|_, _| {
        (
            200,
            r#"[
                {"title":"Fixture.Movie.2024.720p","seeders":900,"url":"https://private.invalid/low?apikey=must-stay-private"},
                {"title":"Fixture.Movie.2024.1080p","seeders":2,"url":"https://private.invalid/high?apikey=must-stay-private"}
            ]"#
                .into(),
        )
    });
    let mut value = config::default_json();
    value.insert(
        "selection",
        json::parse(
            r#"{"movie_profile":"cinema","episode_profile":"cinema","profiles":{"cinema":{"resolutions":[1080,720]}}}"#,
        )
        .unwrap(),
    );
    let mut config = config::from_json(&value, Path::new(".")).unwrap();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "json".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_UPGRADE_ABSENT_KEY_8754381".into(),
    });
    let selected = integrations::select_release(&config, &movie()).unwrap();
    let report = integrations::search_report(&config, &movie()).unwrap();
    assert_eq!(
        selected.url,
        integrations::search(&config, &movie()).unwrap()
    );
    assert_eq!(selected.title, "Fixture.Movie.2024.1080p");
    assert_eq!(selected.profile, "cinema");
    assert!(selected.assessment.accepted);
    assert_eq!(
        report.get("selected_candidate_id").and_then(Value::as_str),
        Some(selected.id.as_str())
    );
    let chosen = report
        .get("accepted")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(selected.id.as_str()))
        .unwrap();
    assert_eq!(
        chosen.get("assessment"),
        Some(&selected.assessment.to_json())
    );
    let rendered = json::stringify(&report);
    for secret in ["https://", "private.invalid", "must-stay-private", "apikey"] {
        assert!(!rendered.contains(secret));
    }

    let before = fixture.calls.load(Ordering::Relaxed);
    for manual_file in [false, true] {
        let mut manual = movie();
        if manual_file {
            manual.source_path = Some("/private/manual.mp4".into());
        } else {
            manual.source_url =
                Some("https://private.invalid/manual?apikey=must-stay-private".into());
        }
        let error = match integrations::select_release(&config, &manual) {
            Ok(_) => panic!("manual sources must not acquire a release assessment"),
            Err(error) => error,
        };
        assert!(!error.contains("private.invalid"));
        assert_eq!(
            integrations::search_report(&config, &manual)
                .unwrap()
                .get("manual_override")
                .and_then(Value::as_bool),
            Some(true)
        );
    }
    assert_eq!(fixture.calls.load(Ordering::Relaxed), before);
}

#[test]
fn structured_selection_cannot_return_a_rejected_or_missing_candidate() {
    for body in [
        "[]",
        r#"[{"title":"Fixture.Movie.2023.1080p","seeders":100,"url":"https://private.invalid/wrong?apikey=must-stay-private"}]"#,
    ] {
        let body = body.to_owned();
        let fixture = Fixture::open(move |_, _| (200, body.clone()));
        let mut config = config::from_json(&config::default_json(), Path::new(".")).unwrap();
        config.sources.push(Source {
            options: Default::default(),
            name: "fixture".into(),
            kind: "json".into(),
            url: fixture.url.clone(),
            api_key_env: "MYNOU_UPGRADE_ABSENT_KEY_8754381".into(),
        });
        let error = match integrations::select_release(&config, &movie()) {
            Ok(_) => panic!("only an accepted candidate can be selected"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "Search: no release satisfies the identity, seed count and selection profile"
        );
        assert_eq!(integrations::search(&config, &movie()).unwrap_err(), error);
    }
}

#[test]
fn unrecordable_titles_do_not_win_acquisition_and_remain_explained_in_the_preview() {
    let releases = Value::Array(vec![
        object([
            ("title", "Fixture.Movie.2024.1080p\0".into()),
            ("seeders", Value::Number(900.0)),
            ("url", "magnet:?xt=urn:btih:invalid-nul".into()),
        ]),
        object([
            (
                "title",
                format!("Fixture.Movie.2024.1080p.{}", "x".repeat(2048)).into(),
            ),
            ("seeders", Value::Number(999.0)),
            ("url", "magnet:?xt=urn:btih:too-long".into()),
        ]),
        object([
            ("title", "Fixture.Movie.2024.720p".into()),
            ("seeders", Value::Number(1.0)),
            ("url", "magnet:?xt=urn:btih:valid".into()),
        ]),
    ]);
    let body = json::stringify(&releases);
    let fixture = Fixture::open(move |_, _| (200, body.clone()));
    let mut config = config::from_json(&config::default_json(), Path::new(".")).unwrap();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "json".into(),
        url: fixture.url.clone(),
        api_key_env: "MYNOU_UPGRADE_ABSENT_KEY_8754381".into(),
    });
    let selected = integrations::select_release(&config, &movie()).unwrap();
    assert_eq!(selected.title, "Fixture.Movie.2024.720p");
    assert_eq!(selected.url, "magnet:?xt=urn:btih:valid");
    assert_eq!(
        selected.url,
        integrations::search(&config, &movie()).unwrap()
    );
    RecordedRelease {
        title: selected.title,
        profile: selected.profile,
    }
    .validate()
    .unwrap();
    let report = integrations::search_report(&config, &movie()).unwrap();
    let rejected = report.get("rejected").unwrap().as_array().unwrap();
    assert_eq!(rejected.len(), 1);
    assert!(
        json::stringify(rejected[0].get("assessment").unwrap())
            .contains("invalid for acquisition provenance")
    );
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(2)
    );
    assert!(!json::stringify(&report).contains("magnet:"));
}

#[test]
fn a_shared_deadline_discards_earlier_candidates_when_a_later_indexer_is_slow() {
    let deadline = Instant::now() + Duration::from_secs(1);
    let fast = Fixture::open(|_, _| {
        (
            200,
            r#"[{"title":"Fixture.Movie.2024.720p","seeders":1,"url":"magnet:?xt=urn:btih:earlier"}]"#
                .into(),
        )
    });
    let slow = Fixture::open(move |_, _| {
        thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(20),
        );
        (
            200,
            r#"[{"title":"Fixture.Movie.2024.1080p","seeders":10,"url":"magnet:?xt=urn:btih:later"}]"#
                .into(),
        )
    });
    let mut config = config::from_json(&config::default_json(), Path::new(".")).unwrap();
    for fixture in [&fast, &slow] {
        config.sources.push(Source {
            options: Default::default(),
            name: "fixture".into(),
            kind: "json".into(),
            url: fixture.url.clone(),
            api_key_env: "MYNOU_UPGRADE_ABSENT_KEY_8754381".into(),
        });
    }
    let error = match integrations::select_release_before(&config, &movie(), deadline) {
        Ok(_) => panic!("a deadline must not allow selection from an incomplete search"),
        Err(error) => error,
    };
    assert_eq!(error, "Search: indexer search exceeded its time budget");
    assert_eq!(fast.calls.load(Ordering::Relaxed), 1);
    assert_eq!(slow.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn import_confirmation_keeps_pagination_and_rejects_an_ignored_offset() {
    let fixture = Fixture::open(|path, _| {
        let path = if path.contains("X-Plex-Container-Start=1") {
            "/library/new.mp4"
        } else {
            "/library/old.mp4"
        };
        (200, container(vec![movie_entry(&[path], 42)], Some(2)))
    });
    let config = fixture.plex_config();
    assert!(
        integrations::available_import(&config, &movie(), &imports(&["/library/new.mp4"])).unwrap()
    );
    assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);

    let ignored = Fixture::open(|_, _| {
        (
            200,
            container(vec![movie_entry(&["/library/old.mp4"], 42)], Some(2)),
        )
    });
    let config = ignored.plex_config();
    assert!(
        integrations::available_import(&config, &movie(), &imports(&["/library/new.mp4"]))
            .unwrap_err()
            .contains("pagination")
    );
}

#[test]
fn invalid_plex_metadata_and_http_failures_do_not_expose_service_secrets() {
    let fixture = Fixture::open(|_, _| {
        let mut show = movie_entry(&[], 42);
        show.insert(
            "ratingKey",
            "https://private.invalid?token=must-stay-private",
        );
        (200, container(vec![show], None))
    });
    let config = fixture.plex_config();
    let mut episode = movie();
    episode.kind = "episode".into();
    episode.season = 2;
    episode.episode = 3;
    assert_eq!(
        integrations::available_import(&config, &episode, &imports(&["/library/new.mp4"]))
            .unwrap_err(),
        "Plex: invalid ratingKey"
    );

    let fixture = Fixture::open(|_, _| (403, "must-stay-private".into()));
    assert_eq!(
        integrations::available_import(
            &fixture.plex_config(),
            &movie(),
            &imports(&["/library/new.mp4"]),
        )
        .unwrap_err(),
        "Plex: HTTP response 403"
    );
}
