//! Release selection uses synthetic indexers; no public torrent is acquired.
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mynou::config::{self, Config, Source};
use mynou::integrations;
use mynou::json::{self, Value};
use mynou::store::Request;

struct Indexer {
    url: String,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Indexer {
    fn open(status: u16, body: String) -> Self {
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
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                thread_calls.fetch_add(1, Ordering::Relaxed);
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            calls,
            stop,
            thread: Some(thread),
        }
    }

    fn releases(body: &str) -> Self {
        Self::open(200, body.into())
    }

    fn source(&self, name: &str) -> Source {
        Source {
            options: Default::default(),
            name: name.into(),
            kind: "json".into(),
            url: self.url.clone(),
            api_key_env: "MYNOU_TEST_ABSENT_SELECTION_KEY_981546".into(),
        }
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn configured(profile: &str) -> Config {
    let mut value = config::default_json();
    let mut profiles = Value::object();
    profiles.insert("cinema", json::parse(profile).unwrap());
    let mut selection = Value::object();
    selection.insert("movie_profile", "cinema");
    selection.insert("episode_profile", "cinema");
    selection.insert("profiles", profiles);
    value.insert("selection", selection);
    config::from_json(&value, Path::new(".")).unwrap()
}

fn movie(title: &str) -> Request {
    Request {
        kind: "movie".into(),
        title: title.into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id: None,
    }
}

fn entries<'a>(report: &'a Value, key: &str) -> &'a [Value] {
    report.get(key).unwrap().as_array().unwrap()
}

fn selected_title(report: &Value) -> Option<&str> {
    let id = report.get("selected_candidate_id")?.as_str()?;
    entries(report, "accepted")
        .iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(id))?
        .get("title")?
        .as_str()
}

#[test]
fn configured_resolution_beats_seed_count_in_actual_search() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.720p","seeders":900,"url":"magnet:?xt=urn:btih:seeded"},
            {"title":"Fixture.Movie.2024.1080p","seeders":2,"url":"magnet:?xt=urn:btih:preferred"}]"#,
    );
    let mut config = configured(r#"{"resolutions":[1080,720]}"#);
    config.sources.push(indexer.source("fixture"));
    let request = movie("Fixture Movie");
    assert_eq!(
        integrations::search(&config, &request).unwrap(),
        "magnet:?xt=urn:btih:preferred"
    );
    let report = integrations::search_report(&config, &request).unwrap();
    assert_eq!(selected_title(&report), Some("Fixture.Movie.2024.1080p"));
    assert_eq!(entries(&report, "accepted").len(), 2);
}

#[test]
fn strict_language_does_not_infer_english_from_the_movie_title() {
    let indexer = Indexer::releases(
        r#"[{"title":"English.Patient.2024.1080p","seeders":500,"url":"magnet:?xt=urn:btih:unknown"},
            {"title":"English.Patient.2024.1080p.FRENCH","seeders":600,"url":"magnet:?xt=urn:btih:wrong"},
            {"title":"English.Patient.2024.1080p.ENGLISH","seeders":2,"url":"magnet:?xt=urn:btih:chosen"}]"#,
    );
    let mut config = configured(r#"{"languages":["en"]}"#);
    config.sources.push(indexer.source("fixture"));
    let report = integrations::search_report(&config, &movie("English Patient")).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert_eq!(entries(&report, "rejected").len(), 2);
    assert_eq!(
        selected_title(&report),
        Some("English.Patient.2024.1080p.ENGLISH")
    );
}

#[test]
fn title_prefix_year_episode_and_seed_threshold_have_separate_reasons() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.Sequel.2024.1080p","seeders":10,"url":"magnet:?xt=urn:btih:sequel"},
            {"title":"Fixture.Movie.2023.1080p","seeders":10,"url":"magnet:?xt=urn:btih:year"},
            {"title":"Fixture.Movie.2024.S01E01.1080p","seeders":10,"url":"magnet:?xt=urn:btih:episode"},
            {"title":"Fixture.Movie.2024.1080p","seeders":0,"url":"magnet:?xt=urn:btih:dead"}]"#,
    );
    let mut config = configured("{}");
    config.sources.push(indexer.source("fixture"));
    let report = integrations::search_report(&config, &movie("Fixture Movie")).unwrap();
    assert!(entries(&report, "accepted").is_empty());
    assert_eq!(entries(&report, "rejected").len(), 4);
    assert_eq!(report.get("selected_candidate_id"), Some(&Value::Null));
    let reasons = json::stringify(report.get("rejected").unwrap());
    assert!(reasons.contains("title, year or episode"));
    assert!(reasons.contains("minimum seeders"));
    assert!(integrations::search(&config, &movie("Fixture Movie")).is_err());
}

#[test]
fn accent_variants_and_x_episode_markers_preserve_existing_identity_support() {
    let indexer = Indexer::releases(
        r#"[{"title":"Cafe.Fixture.2024.1080p","seeders":1,"url":"magnet:?xt=urn:btih:accent"},
            {"title":"Fixture.Series.2x03.1080p","seeders":1,"url":"magnet:?xt=urn:btih:episode"}]"#,
    );
    let mut config = configured(r#"{"resolutions":[1080]}"#);
    config.sources.push(indexer.source("fixture"));
    assert_eq!(
        integrations::search(&config, &movie("Café Fixture")).unwrap(),
        "magnet:?xt=urn:btih:accent"
    );
    let mut episode = movie("Fixture Series");
    episode.kind = "episode".into();
    episode.season = 2;
    episode.episode = 3;
    assert_eq!(
        integrations::search(&config, &episode).unwrap(),
        "magnet:?xt=urn:btih:episode"
    );
}

#[test]
fn video_dimensions_are_not_misclassified_as_episode_identity() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.1920x1080.WEB-DL","seeders":1,"url":"magnet:?xt=urn:btih:dimensions"},
            {"title":"Fixture.Movie.2024.2x03.1080p","seeders":500,"url":"magnet:?xt=urn:btih:episode"}]"#,
    );
    let mut config = configured(r#"{"resolutions":[1080]}"#);
    config.sources.push(indexer.source("fixture"));
    let mut request = movie("Fixture Movie");
    assert_eq!(
        integrations::search(&config, &request).unwrap(),
        "magnet:?xt=urn:btih:dimensions"
    );
    request.kind = "episode".into();
    request.season = 2;
    request.episode = 3;
    assert_eq!(
        integrations::search(&config, &request).unwrap(),
        "magnet:?xt=urn:btih:episode"
    );
}

#[test]
fn score_threshold_and_blocked_terms_reject_high_seed_releases() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.1080p","seeders":999,"url":"magnet:?xt=urn:btih:score"},
            {"title":"Fixture.Movie.2024.1080p.PROPER.CAM","seeders":999,"url":"magnet:?xt=urn:btih:blocked"},
            {"title":"Fixture.Movie.2024.1080p.PROPER","seeders":1,"url":"magnet:?xt=urn:btih:chosen"}]"#,
    );
    let mut config = configured(
        r#"{"blocked_terms":["cam"],"minimum_score":20,"score_rules":[{"terms":["proper"],"score":20}]}"#,
    );
    config.sources.push(indexer.source("fixture"));
    let report = integrations::search_report(&config, &movie("Fixture Movie")).unwrap();
    assert_eq!(entries(&report, "accepted").len(), 1);
    assert_eq!(entries(&report, "rejected").len(), 2);
    assert_eq!(
        integrations::search(&config, &movie("Fixture Movie")).unwrap(),
        "magnet:?xt=urn:btih:chosen"
    );
}

#[test]
fn custom_score_precedes_resolution_and_seed_preferences() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.1080p","seeders":500,"url":"magnet:?xt=urn:btih:quality"},
            {"title":"Fixture.Movie.2024.720p.PROPER","seeders":1,"url":"magnet:?xt=urn:btih:chosen"}]"#,
    );
    let mut config =
        configured(r#"{"resolutions":[1080,720],"score_rules":[{"terms":["proper"],"score":10}]}"#);
    config.sources.push(indexer.source("fixture"));
    assert_eq!(
        integrations::search(&config, &movie("Fixture Movie")).unwrap(),
        "magnet:?xt=urn:btih:chosen"
    );
}

#[test]
fn repeated_reports_and_tie_breaks_are_deterministic_and_redacted() {
    let indexer = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.1080p","seeders":5,"url":"https://private.example/z?apikey=never-leak-token"},
            {"title":"Fixture.Movie.2024.1080p","seeders":5,"url":"https://private.example/a?apikey=never-leak-token"}]"#,
    );
    let mut config = configured("{}");
    config
        .sources
        .push(indexer.source("https://secret.example?token=secret-name"));
    let request = movie("Fixture Movie");
    let first = integrations::search_report(&config, &request).unwrap();
    let second = integrations::search_report(&config, &request).unwrap();
    assert_eq!(first, second);
    let rendered = json::stringify(&first);
    for forbidden in [
        "https://",
        "never-leak-token",
        "secret-name",
        "private.example",
    ] {
        assert!(!rendered.contains(forbidden));
    }
    let id = first
        .get("selected_candidate_id")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(id.len(), 64);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    // Equal rank and seeders fall back to the opaque candidate ID.
    let smallest = entries(&first, "accepted")
        .iter()
        .filter_map(|candidate| candidate.get("id").and_then(Value::as_str))
        .min()
        .unwrap();
    assert_eq!(id, smallest);
    let chosen = integrations::search(&config, &request).unwrap();
    assert!(
        [
            "https://private.example/a?apikey=never-leak-token",
            "https://private.example/z?apikey=never-leak-token"
        ]
        .contains(&chosen.as_str())
    );
    assert_eq!(integrations::search(&config, &request).unwrap(), chosen);
}

#[test]
fn usable_empty_indexer_returns_empty_preview_without_search_success() {
    let indexer = Indexer::releases("[]");
    let mut config = configured("{}");
    config.sources.push(indexer.source("empty"));
    let report = integrations::search_report(&config, &movie("Fixture Movie")).unwrap();
    assert!(entries(&report, "accepted").is_empty());
    assert!(entries(&report, "rejected").is_empty());
    assert_eq!(report.get("selected_candidate_id"), Some(&Value::Null));
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(0)
    );
    assert!(integrations::search(&config, &movie("Fixture Movie")).is_err());
}

#[test]
fn failed_indexers_do_not_block_a_usable_source_or_leak_response_bodies() {
    let broken = Indexer::open(
        403,
        "https://private.example?apikey=never-leak-token".into(),
    );
    let working = Indexer::releases(
        r#"[{"title":"Fixture.Movie.2024.1080p","seeders":1,"url":"magnet:?xt=urn:btih:chosen"}]"#,
    );
    let mut config = configured("{}");
    config.sources.push(broken.source("broken"));
    config.sources.push(working.source("working"));
    let report = integrations::search_report(&config, &movie("Fixture Movie")).unwrap();
    assert_eq!(
        report
            .get("indexers")
            .unwrap()
            .get("failed")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(!json::stringify(&report).contains("never-leak-token"));
    config.sources.pop();
    assert_eq!(
        integrations::search_report(&config, &movie("Fixture Movie")).unwrap_err(),
        "Search: no indexer returned a usable response"
    );
}

fn page(config: &Config, request: &Request, query: &str) -> Value {
    integrations::search_report_page(
        config,
        request,
        &integrations::SearchPage::parse(query).unwrap(),
    )
    .unwrap()
}

fn ids(report: &Value) -> Vec<String> {
    ["accepted", "rejected"]
        .iter()
        .flat_map(|key| entries(report, key))
        .map(|candidate| candidate.get("id").unwrap().as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn cursor_pages_reach_every_candidate_beyond_the_former_report_limit() {
    // 1,001 accepted plus 200 rejected candidates exceed the former 1,000-entry report.
    let releases = (0..1_201)
        .map(|number| {
            let mut release = Value::object();
            release.insert(
                "title",
                if number < 1_001 {
                    "Fixture.Movie.2024.1080p"
                } else {
                    "Other.Movie.2024.1080p"
                },
            );
            release.insert("seeders", Value::Number((number % 977 + 1) as f64));
            release.insert("url", format!("magnet:?xt=urn:btih:fixture{number}"));
            release
        })
        .collect();
    let indexer = Indexer::open(200, json::stringify(&Value::Array(releases)));
    let mut config = configured("{}");
    config.sources.push(indexer.source("fixture"));
    let request = movie("Fixture Movie");
    let first = integrations::search_report(&config, &request).unwrap();
    assert_eq!(first.get("truncated"), None);
    assert_eq!(
        first.get("candidate_count").and_then(Value::as_u64),
        Some(1_201)
    );
    assert_eq!(
        first.get("reported_count").and_then(Value::as_u64),
        Some(100)
    );
    let selected = first.get("selected_candidate_id").unwrap().clone();
    assert_eq!(
        entries(&first, "accepted")[0].get("id"),
        Some(&selected),
        "the first candidate of the first page is the selection"
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut order = Vec::new();
    let mut query = "limit=200".to_owned();
    let mut pages = 0;
    loop {
        let report = page(&config, &request, &query);
        pages += 1;
        assert_eq!(report.get("selected_candidate_id"), Some(&selected));
        assert_eq!(
            report.get("candidate_count").and_then(Value::as_u64),
            Some(1_201)
        );
        let page_ids = ids(&report);
        assert_eq!(
            report.get("reported_count").and_then(Value::as_u64),
            Some(page_ids.len() as u64)
        );
        assert!(page_ids.len() <= 200);
        for candidate in entries(&report, "accepted")
            .iter()
            .chain(entries(&report, "rejected"))
        {
            order.push((
                candidate
                    .get("assessment")
                    .and_then(|a| a.get("accepted"))
                    .and_then(Value::as_bool)
                    .unwrap(),
                candidate.get("seeders").and_then(Value::as_u64).unwrap(),
            ));
        }
        for id in page_ids {
            assert!(seen.insert(id), "a candidate repeated across pages");
        }
        match report.get("next") {
            Some(Value::String(next)) => query = format!("limit=200&after={next}"),
            Some(Value::Null) => break,
            other => panic!("unexpected next cursor {other:?}"),
        }
    }
    assert_eq!(pages, 7);
    assert_eq!(seen.len(), 1_201);
    // Accepted candidates first, each group by descending seeders.
    assert!(order.windows(2).all(|pair| {
        let ((accepted_a, seeders_a), (accepted_b, seeders_b)) = (pair[0], pair[1]);
        (accepted_a && !accepted_b) || (accepted_a == accepted_b && seeders_a >= seeders_b)
    }));
    assert_eq!(
        order.iter().filter(|(accepted, _)| *accepted).count(),
        1_001
    );
}

#[test]
fn malformed_cursors_and_page_limits_are_rejected() {
    for query in [
        "limit=0",
        "limit=201",
        "limit=01",
        "limit=x",
        "after=zz",
        "after=",
        "limit=5&limit=6",
        "offset=3",
        "after",
    ] {
        assert!(
            integrations::SearchPage::parse(query).is_err(),
            "{query} must be refused"
        );
    }
    let valid = "0".repeat(122);
    assert_eq!(
        integrations::SearchPage::parse(&format!("after={valid}&limit=200"))
            .unwrap()
            .limit,
        200
    );
    assert_eq!(
        integrations::SearchPage::parse(&format!("after={}", "0".repeat(120))).unwrap_err(),
        "Search: invalid candidate cursor"
    );
    assert_eq!(
        integrations::SearchPage::parse(&format!("after=02{}", "0".repeat(120))).unwrap_err(),
        "Search: invalid candidate cursor"
    );
    assert_eq!(
        integrations::SearchPage::parse(&format!("after={}", "A".repeat(122))).unwrap_err(),
        "Search: invalid candidate cursor"
    );
    assert_eq!(integrations::SearchPage::parse("").unwrap().limit, 100);
}

#[test]
fn combined_candidates_beyond_former_totals_are_streamed_not_refused() {
    // Three responses of about 8 MiB exceed the former 16 MiB candidate budget.
    let url = format!("https://private.example/{}", "x".repeat(7_950));
    let releases = (0..1_000)
        .map(|number| {
            let mut release = Value::object();
            release.insert("title", "Fixture.Movie.2024.1080p");
            release.insert("seeders", Value::Number((number + 1) as f64));
            release.insert("url", url.clone());
            release
        })
        .collect();
    let body = json::stringify(&Value::Array(releases));
    assert!(body.len() < 8 * 1024 * 1024);
    let indexer = Indexer::open(200, body);
    let mut config = configured("{}");
    for name in ["first", "second", "third"] {
        config.sources.push(indexer.source(name));
    }
    let report = integrations::search_report(&config, &movie("Fixture Movie")).unwrap();
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(3_000)
    );
    assert_eq!(entries(&report, "accepted").len(), 100);
    assert_eq!(
        entries(&report, "accepted")[0]
            .get("seeders")
            .and_then(Value::as_u64),
        Some(1_000)
    );
    assert!(report.get("next").unwrap().as_str().is_some());
    assert!(!json::stringify(&report).contains("private.example"));
    assert_eq!(
        integrations::search(&config, &movie("Fixture Movie")).unwrap(),
        url
    );

    // More results than the former 100,000-candidate total, across responses.
    let small = (0..40_000)
        .map(|number| {
            let mut release = Value::object();
            release.insert("title", "Fixture.Movie.2024.1080p");
            release.insert("seeders", Value::Number((number % 5_000 + 1) as f64));
            release.insert("url", format!("magnet:?xt=urn:btih:f{number}"));
            release
        })
        .collect();
    let indexer = Indexer::open(200, json::stringify(&Value::Array(small)));
    let mut config = configured("{}");
    for name in ["first", "second", "third"] {
        config.sources.push(indexer.source(name));
    }
    let report = page(&config, &movie("Fixture Movie"), "limit=200");
    assert_eq!(
        report.get("candidate_count").and_then(Value::as_u64),
        Some(120_000)
    );
    assert_eq!(entries(&report, "accepted").len(), 200);
    let next = report.get("next").unwrap().as_str().unwrap().to_owned();
    let second = page(&config, &movie("Fixture Movie"), &format!("after={next}"));
    assert_eq!(entries(&second, "accepted").len(), 100);
    let first_ids = ids(&report);
    assert!(ids(&second).iter().all(|id| !first_ids.contains(id)));
    assert_eq!(
        second.get("selected_candidate_id"),
        report.get("selected_candidate_id")
    );
}

#[test]
fn manual_source_override_is_previewed_without_contacting_indexers() {
    let indexer = Indexer::open(500, "unexpected request".into());
    let mut config = configured(r#"{"languages":["en"]}"#);
    config.sources.push(indexer.source("must-not-contact"));
    let mut request = movie("Fixture Movie");
    request.source_url = Some("https://private.example/file?apikey=never-leak-token".into());
    let report = integrations::search_report(&config, &request).unwrap();
    assert_eq!(
        report.get("manual_override").and_then(Value::as_bool),
        Some(true)
    );
    assert!(!json::stringify(&report).contains("never-leak-token"));
    assert_eq!(
        integrations::search(&config, &request).unwrap(),
        request.source_url.unwrap()
    );
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn manual_file_preview_does_not_search_or_expose_a_private_path() {
    let indexer = Indexer::open(500, "unexpected request".into());
    let mut config = configured("{}");
    config.sources.push(indexer.source("must-not-contact"));
    let mut request = movie("Fixture Movie");
    request.source_path = Some("/private/library/secret-file.mp4".into());
    let report = integrations::search_report(&config, &request).unwrap();
    assert_eq!(
        report.get("manual_override").and_then(Value::as_bool),
        Some(true)
    );
    assert!(!json::stringify(&report).contains("secret-file"));
    assert_eq!(indexer.calls.load(Ordering::Relaxed), 0);
}
