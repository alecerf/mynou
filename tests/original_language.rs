//! The `original` profile language, resolved per title from TMDB's original
//! language, with a synthetic indexer and catalog.
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

/// One local service: `/indexer` answers the releases, `/3/…` is TMDB.
struct Fixture {
    url: String,
    catalog_calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const RELEASES: &str = r#"[
    {"title":"Fixture.Film.2024.1080p.FRENCH","seeders":90,"url":"magnet:?xt=urn:btih:french"},
    {"title":"Fixture.Film.2024.1080p.MULTi.VFF","seeders":10,"url":"magnet:?xt=urn:btih:multi"},
    {"title":"Fixture.Film.2024.1080p","seeders":50,"url":"magnet:?xt=urn:btih:untagged"}
]"#;

fn catalog(path: &str) -> (u16, String) {
    let film = |id: u32, language: &str| {
        format!(
            r#"{{"id":{id},"title":"Fixture Film","release_date":"2024-03-01","runtime":100,"original_language":"{language}"}}"#
        )
    };
    match path.split('?').next().unwrap_or(path) {
        "/3/movie/101" => (200, film(101, "en")),
        "/3/movie/202" => (200, film(202, "fr")),
        "/3/movie/404" => (500, "{}".into()),
        "/3/search/movie" => (
            200,
            r#"{"results":[{"id":101,"title":"Fixture Film","release_date":"2024-03-01"}]}"#.into(),
        ),
        "/3/tv/303" => (
            200,
            r#"{"id":303,"name":"Fixture Show","first_air_date":"2020-01-01","original_language":"ja",
                "seasons":[{"season_number":1,"air_date":"2020-01-01"}]}"#
                .into(),
        ),
        "/3/tv/303/season/1" => (
            200,
            r#"{"episodes":[{"episode_number":1,"air_date":"2020-01-01","runtime":24}]}"#.into(),
        ),
        _ => panic!("unexpected catalog path {path}"),
    }
}

impl Fixture {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let catalog_calls = Arc::new(AtomicUsize::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread_calls = Arc::clone(&catalog_calls);
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
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let (status, body) = if path.starts_with("/indexer") {
                    (200, RELEASES.to_owned())
                } else {
                    thread_calls.fetch_add(1, Ordering::Relaxed);
                    catalog(&path)
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            catalog_calls,
            stop,
            thread: Some(thread),
        }
    }

    fn catalog_calls(&self) -> usize {
        self.catalog_calls.load(Ordering::Relaxed)
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

/// A configuration whose films and series need their original audio.
fn configured(fixture: &Fixture, languages: &str) -> Config {
    let mut value = config::default_json();
    value.insert(
        "selection",
        json::parse(&format!(
            r#"{{"movie_profile":"original","episode_profile":"original","profiles":{{"original":{{"resolutions":[1080],"languages":{languages},"language_mode":"require"}}}}}}"#
        ))
        .unwrap(),
    );
    let mut config = config::from_json(&value, Path::new(".")).unwrap();
    config.sources.push(Source {
        options: Default::default(),
        name: "fixture".into(),
        kind: "json".into(),
        url: format!("{}/indexer", fixture.url),
        api_key_env: "MYNOU_TEST_ABSENT_ORIGINAL_KEY_640213".into(),
    });
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", fixture.url);
    config.catalog.token_env = "MYNOU_TEST_SECRET_A".into();
    config
}

fn film(tmdb_id: Option<u64>) -> Request {
    Request {
        kind: "movie".into(),
        title: "Fixture Film".into(),
        year: 2024,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        runtime_minutes: None,
        source_numbering: None,
        original_language: None,
        tmdb_id,
    }
}

/// Release titles of a report list, with their reasons.
fn listed(report: &Value, key: &str) -> Vec<(String, Vec<String>)> {
    let mut found: Vec<_> = report
        .get(key)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| {
            let reasons = candidate
                .get("assessment")
                .and_then(|a| a.get("reasons"))
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .map(|reason| reason.as_str().unwrap().to_owned())
                .collect();
            (
                candidate.get("title").unwrap().as_str().unwrap().to_owned(),
                reasons,
            )
        })
        .collect();
    found.sort();
    found
}

fn titles(report: &Value, key: &str) -> Vec<String> {
    listed(report, key)
        .into_iter()
        .map(|(title, _)| title)
        .collect()
}

fn explanation(report: &Value, title: &str) -> String {
    ["accepted", "rejected"]
        .iter()
        .flat_map(|key| report.get(key).unwrap().as_array().unwrap())
        .find(|c| c.get("title").and_then(Value::as_str) == Some(title))
        .and_then(|c| c.get("assessment")?.get("original")?.get("explanation"))
        .and_then(Value::as_str)
        .unwrap()
        .to_owned()
}

fn language(report: &Value) -> Option<&str> {
    report
        .get("original_language")
        .unwrap()
        .get("language")
        .unwrap()
        .as_str()
}

#[test]
fn an_english_film_refuses_a_french_dub_alone_and_takes_multi_or_untagged() {
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["original"]"#);
    let report = integrations::search_report(&config, &film(Some(101))).unwrap();
    assert_eq!(language(&report), Some("en"));
    assert_eq!(
        report
            .get("original_language")
            .unwrap()
            .get("name")
            .and_then(Value::as_str),
        Some("English")
    );
    assert_eq!(
        titles(&report, "accepted"),
        [
            "Fixture.Film.2024.1080p",
            "Fixture.Film.2024.1080p.MULTi.VFF"
        ]
    );
    assert_eq!(
        listed(&report, "rejected"),
        [(
            "Fixture.Film.2024.1080p.FRENCH".to_owned(),
            vec!["Original language: English — FR only, no original audio".to_owned()]
        )]
    );
    assert_eq!(
        explanation(&report, "Fixture.Film.2024.1080p.MULTi.VFF"),
        "Original language: English — MULTi, with the original audio"
    );
    // The French release has the most seeders; it is still never chosen.
    let selected = integrations::select_release(&config, &film(Some(101))).unwrap();
    assert_ne!(selected.title, "Fixture.Film.2024.1080p.FRENCH");
    assert!(selected.assessment.original.unwrap().audio);
}

#[test]
fn a_french_film_takes_french_or_multi_and_refuses_untagged() {
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["original"]"#);
    let report = integrations::search_report(&config, &film(Some(202))).unwrap();
    assert_eq!(language(&report), Some("fr"));
    assert_eq!(
        titles(&report, "accepted"),
        [
            "Fixture.Film.2024.1080p.FRENCH",
            "Fixture.Film.2024.1080p.MULTi.VFF"
        ]
    );
    assert_eq!(
        listed(&report, "rejected"),
        [(
            "Fixture.Film.2024.1080p".to_owned(),
            vec!["Original language: French — no language tag, no sign of French audio".to_owned()]
        )]
    );
    assert_eq!(
        explanation(&report, "Fixture.Film.2024.1080p.FRENCH"),
        "Original language: French — FR, the original audio"
    );
    let selected = integrations::select_release(&config, &film(Some(202))).unwrap();
    assert_eq!(selected.title, "Fixture.Film.2024.1080p.FRENCH");
}

#[test]
fn an_unknown_original_language_never_lets_a_french_dub_alone_pass() {
    let fixture = Fixture::open();
    let mut config = configured(&fixture, r#"["original"]"#);
    // TMDB failing, then off: only VO, MULTi and VOSTFR count.
    for enabled in [true, false] {
        config.catalog.enabled = enabled;
        let report = integrations::search_report(&config, &film(Some(404))).unwrap();
        assert_eq!(language(&report), None);
        assert_eq!(
            report.get("original_language").unwrap().get("name"),
            Some(&Value::Null)
        );
        assert_eq!(
            titles(&report, "accepted"),
            ["Fixture.Film.2024.1080p.MULTi.VFF"]
        );
        assert_eq!(
            listed(&report, "rejected"),
            [
                (
                    "Fixture.Film.2024.1080p".to_owned(),
                    vec![
                        "Original language unknown — no language tag; only VO, MULTi or VOSTFR count"
                            .to_owned()
                    ]
                ),
                (
                    "Fixture.Film.2024.1080p.FRENCH".to_owned(),
                    vec![
                        "Original language unknown — FR only; only VO, MULTi or VOSTFR count"
                            .to_owned()
                    ]
                )
            ]
        );
    }
}

#[test]
fn a_known_or_chosen_original_language_is_not_looked_up_again() {
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["original"]"#);
    // A title chosen on the Quality page, without its TMDB identity.
    let mut chosen = film(None);
    chosen.original_language = Some("fr".into());
    let report = integrations::search_report(&config, &chosen).unwrap();
    assert_eq!(language(&report), Some("fr"));
    assert!(titles(&report, "accepted").contains(&"Fixture.Film.2024.1080p.FRENCH".to_owned()));
    assert_eq!(fixture.catalog_calls(), 0);
    // A typed title is found on TMDB by its name.
    let report = integrations::search_report(&config, &film(None)).unwrap();
    assert_eq!(language(&report), Some("en"));
    assert!(fixture.catalog_calls() > 0);
    // A profile without `original` does not ask TMDB for it (the runtime is
    // known here), and says nothing of it.
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["vff"]"#);
    let mut timed = film(Some(101));
    timed.runtime_minutes = Some(100);
    let report = integrations::search_report(&config, &timed).unwrap();
    assert_eq!(report.get("original_language"), Some(&Value::Null));
    assert_eq!(fixture.catalog_calls(), 0);
}

#[test]
fn new_titles_keep_the_original_language_tmdb_gives() {
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["vff","original"]"#);
    let films = integrations::expand(&config, &film(None)).unwrap();
    assert_eq!(films.len(), 1);
    assert_eq!(films[0].tmdb_id, Some(101));
    assert_eq!(films[0].original_language.as_deref(), Some("en"));
    let mut series = film(Some(303));
    series.kind = "series".into();
    series.title = "Fixture Show".into();
    series.year = 2020;
    let episodes = integrations::expand(&config, &series).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].original_language.as_deref(), Some("ja"));
    // The request keeps it in its saved form; older forms have none.
    let saved = Request::from_json(&episodes[0].to_json()).unwrap();
    assert_eq!(saved, episodes[0]);

    // A job created before is filled once, when its profile asks for it.
    let mut older = film(Some(202));
    integrations::learn_original_language(&config, &mut older);
    assert_eq!(older.original_language.as_deref(), Some("fr"));
    let mut unknown = film(Some(404));
    integrations::learn_original_language(&config, &mut unknown);
    assert_eq!(unknown.original_language, None);
    let calls = fixture.catalog_calls();
    let plain = configured(&fixture, r#"["vff"]"#);
    let mut untouched = film(Some(101));
    integrations::learn_original_language(&plain, &mut untouched);
    assert_eq!(untouched.original_language, None);
    assert_eq!(fixture.catalog_calls(), calls);
}

#[test]
fn vff_and_the_original_audio_keep_both_required() {
    let fixture = Fixture::open();
    let config = configured(&fixture, r#"["vff","original"]"#);
    // An English film needs MULTi with VFF; for a French film, FRENCH holds
    // the original audio but does not say VFF.
    let english = integrations::search_report(&config, &film(Some(101))).unwrap();
    assert_eq!(
        titles(&english, "accepted"),
        ["Fixture.Film.2024.1080p.MULTi.VFF"]
    );
    let french = integrations::search_report(&config, &film(Some(202))).unwrap();
    assert_eq!(
        titles(&french, "accepted"),
        ["Fixture.Film.2024.1080p.MULTi.VFF"]
    );
    assert_eq!(
        listed(&french, "rejected")
            .into_iter()
            .find(|(title, _)| title == "Fixture.Film.2024.1080p.FRENCH")
            .unwrap()
            .1,
        ["The release lacks some of the profile's languages"]
    );
}
