//! Imports go where Plex already keeps the title, under names Plex reads
//! cleanly, with the release's subtitles, and only when the file lasts about
//! as long as the title. Plex is a synthetic local service; torrents come
//! from local peers.
use crate::library_support::{demo_lasting, run_until};
use crate::transfer_support::{Scratch, Seeder, Torrent, engine_config};
use mynou::config::{Config, PathMapping};
use mynou::engine::{Engine, public_job};
use mynou::json::{self, Value};
use mynou::store::{Job, Request};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type Listings = Arc<Mutex<BTreeMap<String, Vec<Value>>>>;

/// Plex answering every listing route from a fixed list of items; any
/// other route (a section refresh) gets an empty listing.
struct Plex {
    url: String,
    listings: Listings,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Plex {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let listings = Listings::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let (thread_listings, thread_stopped) = (listings.clone(), stopped.clone());
        let thread = thread::spawn(move || {
            while !thread_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("Plex fixture accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    assert!(header.len() < 16_384 && Instant::now() < deadline);
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                assert!(header.contains("X-Plex-Token: import-layout-plex-token"));
                let route = header.split_whitespace().nth(1).unwrap();
                let path = route.split('?').next().unwrap();
                // Every listing fits in its first page.
                let items = thread_listings
                    .lock()
                    .unwrap()
                    .get(path)
                    .cloned()
                    .unwrap_or_default();
                let mut container = Value::object();
                container.insert("size", Value::Number(items.len() as f64));
                container.insert("Metadata", Value::Array(items));
                let mut body = Value::object();
                body.insert("MediaContainer", container);
                let body = json::stringify(&body);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            url,
            listings,
            stopped,
            thread: Some(thread),
        }
    }

    fn list(&self, route: &str, items: Vec<Value>) {
        self.listings.lock().unwrap().insert(route.into(), items);
    }

    /// An engine whose Plex sees the films under `/plex/movies` and the
    /// series under `/plex/tv`.
    fn engine_config(&self, root: &Path) -> Config {
        let mut cfg = engine_config(root);
        cfg.plex.enabled = true;
        cfg.plex.url = self.url.clone();
        cfg.plex.token_override = Some("import-layout-plex-token".into());
        cfg.plex.path_mappings = vec![
            PathMapping {
                mynou_prefix: cfg.movies_root.to_str().unwrap().into(),
                plex_prefix: "/plex/movies".into(),
            },
            PathMapping {
                mynou_prefix: cfg.series_root.to_str().unwrap().into(),
                plex_prefix: "/plex/tv".into(),
            },
        ];
        cfg
    }
}

impl Drop for Plex {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn media(files: &[&str]) -> Value {
    let parts = files
        .iter()
        .map(|file| {
            let mut part = Value::object();
            part.insert("file", *file);
            part
        })
        .collect();
    let mut media = Value::object();
    media.insert("Part", Value::Array(parts));
    Value::Array(vec![media])
}

/// A film or series as a Plex section lists it.
fn item(title: &str, year: u32, tmdb: u64, key: &str, files: &[&str]) -> Value {
    let mut guid = Value::object();
    guid.insert("id", format!("tmdb://{tmdb}"));
    let mut item = Value::object();
    item.insert("title", title);
    item.insert("year", year);
    item.insert("ratingKey", key);
    item.insert("Guid", Value::Array(vec![guid]));
    if !files.is_empty() {
        item.insert("Media", media(files));
    }
    item
}

fn leaf(season: u32, episode: u32, file: &str) -> Value {
    let mut leaf = Value::object();
    leaf.insert("parentIndex", season);
    leaf.insert("index", episode);
    leaf.insert("Media", media(&[file]));
    leaf
}

fn request(kind: &str, title: &str, year: u32, tmdb: Option<u64>) -> Request {
    Request {
        kind: kind.into(),
        title: title.into(),
        year,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        runtime_minutes: None,
        source_numbering: None,
        original_language: None,
        tmdb_id: tmdb,
    }
}

/// A local file to import, written with `bytes`.
fn source(scratch: &Scratch, name: &str, bytes: &[u8]) -> PathBuf {
    let directory = scratch.0.join("sources");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn from_path(mut request: Request, path: &Path) -> Request {
    request.source_path = Some(path.to_str().unwrap().into());
    request
}

/// Runs `request` until its file is in the library.
fn import(engine: &Arc<Engine>, request: Request) -> Job {
    let id = engine.submit(request).unwrap().remove(0).id;
    run_until(engine, &id, "imported")
}

/// The import notes, as the API shows them.
fn notes(job: &Job) -> Vec<String> {
    public_job(job)
        .get("import_notes")
        .and_then(Value::as_array)
        .map(|notes| {
            notes
                .iter()
                .map(|note| note.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn regular_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let kind = fs::symlink_metadata(&path).unwrap().file_type();
        if kind.is_dir() {
            found.extend(regular_files(&path));
        } else if kind.is_file() {
            found.push(path);
        }
    }
    found
}

const DEMO: &[u8] = include_bytes!("fixtures/demo.mp4");

#[test]
fn episodes_join_the_season_folder_plex_already_uses() {
    let scratch = Scratch::new();
    let plex = Plex::open();
    let cfg = plex.engine_config(&scratch.0.join("engine"));
    let show = cfg.series_root.join("Show (2019)");
    for folder in ["Season 2", "Specials"] {
        fs::create_dir_all(show.join(folder)).unwrap();
    }
    plex.list(
        "/library/sections/2/all",
        vec![item("Show", 2019, 99, "7", &[])],
    );
    plex.list(
        "/library/metadata/7/allLeaves",
        vec![
            leaf(2, 1, "/plex/tv/Show (2019)/Season 2/Show - S02E01.mkv"),
            leaf(0, 1, "/plex/tv/Show (2019)/Specials/Show - S00E01.mkv"),
        ],
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    let new_show = cfg.series_root.join("New Show");
    for (title, tmdb, season, number, expected) in [
        (
            "Show",
            Some(99),
            2,
            2,
            show.join("Season 2/Show - S02E02.mp4"),
        ),
        (
            "Show",
            Some(99),
            0,
            2,
            show.join("Specials/Show - S00E02.mp4"),
        ),
        (
            "New Show",
            None,
            2,
            1,
            new_show.join("Season 02/New Show - S02E01.mp4"),
        ),
        (
            "New Show",
            None,
            0,
            1,
            new_show.join("Season 00/New Show - S00E01.mp4"),
        ),
    ] {
        let mut episode = request("episode", title, 2019, tmdb);
        episode.season = season;
        episode.episode = number;
        let path = source(&scratch, &format!("{title} {season} {number}.mp4"), DEMO);
        let job = import(&engine, from_path(episode, &path));
        assert_eq!(
            job.imports,
            [expected.to_str().unwrap()],
            "{:?}",
            notes(&job)
        );
        assert_eq!(fs::read(&expected).unwrap(), DEMO);
    }
    // No second folder for a season the series already has.
    assert_eq!(names(&show), ["Season 2", "Specials"]);
}

#[test]
fn films_join_their_plex_folder_and_new_names_use_dashes() {
    let scratch = Scratch::new();
    let plex = Plex::open();
    let cfg = plex.engine_config(&scratch.0.join("engine"));
    let movies = cfg.movies_root.clone();
    let mission = movies.join("Mission - Impossible (1996)");
    let amelie = movies.join("Le Fabuleux Destin d'Amélie Poulain (2001)");
    let legacy = movies.join("Alien_ Romulus (2024)");
    for folder in [&mission, &amelie, &legacy] {
        fs::create_dir_all(folder).unwrap();
    }
    fs::write(
        legacy.join("Alien_ Romulus (2024).mkv"),
        b"imported earlier",
    )
    .unwrap();
    plex.list(
        "/library/sections/1/all",
        vec![
            item(
                "Mission: Impossible",
                1996,
                954,
                "1",
                &["/plex/movies/Mission - Impossible (1996)/Mission.Impossible.1996.720p.mkv"],
            ),
            item(
                "Le Fabuleux Destin d'Amélie Poulain",
                2001,
                194,
                "2",
                &["/plex/movies/Le Fabuleux Destin d'Amélie Poulain (2001)/Amelie.2001.mkv"],
            ),
        ],
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    for (title, year, tmdb, expected, note) in [
        (
            "Mission: Impossible",
            1996,
            Some(954),
            mission.join("Mission - Impossible (1996).mp4"),
            Some("plex_folder: "),
        ),
        (
            "Amélie",
            2001,
            Some(194),
            amelie.join("Amélie (2001).mp4"),
            Some("plex_folder: "),
        ),
        (
            "Dune: Part Two",
            2024,
            None,
            movies.join("Dune - Part Two (2024)/Dune - Part Two (2024).mp4"),
            None,
        ),
        (
            "Alien: Romulus",
            2024,
            None,
            legacy.join("Alien - Romulus (2024).mp4"),
            Some("folder_kept: "),
        ),
    ] {
        let path = source(&scratch, &format!("{}.mp4", title.replace(':', "")), DEMO);
        let job = import(
            &engine,
            from_path(request("movie", title, year, tmdb), &path),
        );
        assert_eq!(
            job.imports,
            [expected.to_str().unwrap()],
            "{:?}",
            notes(&job)
        );
        let folder_notes: Vec<_> = notes(&job)
            .into_iter()
            .filter(|note| note.starts_with("plex_folder") || note.starts_with("folder_"))
            .collect();
        match note {
            Some(code) => assert!(folder_notes[0].starts_with(code), "{folder_notes:?}"),
            None => assert!(folder_notes.is_empty(), "{folder_notes:?}"),
        }
    }
    // The title Mynou named with `_` kept its folder; no second one appeared.
    assert!(!movies.join("Alien - Romulus (2024)").exists());
    assert_eq!(
        fs::read(legacy.join("Alien_ Romulus (2024).mkv")).unwrap(),
        b"imported earlier"
    );
}

#[test]
fn unusable_plex_folders_fall_back_to_mynous_layout_with_a_reason() {
    let scratch = Scratch::new();
    let plex = Plex::open();
    let cfg = plex.engine_config(&scratch.0.join("engine"));
    let movies = cfg.movies_root.clone();
    let outside = scratch.0.join("outside");
    fs::create_dir_all(outside.join("Outside Film (2020)")).unwrap();
    fs::create_dir_all(outside.join("linked")).unwrap();
    fs::create_dir_all(movies.join("Twice A (2020)")).unwrap();
    fs::create_dir_all(movies.join("Twice B (2020)")).unwrap();
    symlink(outside.join("linked"), movies.join("Linked (2020)")).unwrap();
    let outside_file = outside.join("Outside Film (2020)/Outside.Film.mkv");
    plex.list(
        "/library/sections/1/all",
        vec![
            item(
                "Outside Film",
                2020,
                1,
                "1",
                &[outside_file.to_str().unwrap()],
            ),
            item(
                "Symlinked Film",
                2020,
                2,
                "2",
                &["/plex/movies/Linked (2020)/Linked.mkv"],
            ),
            item(
                "Twice",
                2020,
                3,
                "3",
                &["/plex/movies/Twice A (2020)/Twice.mkv"],
            ),
            item(
                "Twice",
                2020,
                3,
                "4",
                &["/plex/movies/Twice B (2020)/Twice.mkv"],
            ),
        ],
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    for (title, tmdb, reason) in [
        ("Outside Film", 1, "is outside"),
        ("Symlinked Film", 2, "symbolic link"),
        ("Twice", 3, "2 folders"),
    ] {
        let path = source(&scratch, &format!("{title}.mp4"), DEMO);
        let job = import(
            &engine,
            from_path(request("movie", title, 2020, Some(tmdb)), &path),
        );
        let expected = movies.join(format!("{title} (2020)/{title} (2020).mp4"));
        assert_eq!(job.imports, [expected.to_str().unwrap()]);
        let notes = notes(&job);
        assert!(
            notes
                .iter()
                .any(|note| note.starts_with("folder_fallback: ") && note.contains(reason)),
            "{title}: {notes:?}"
        );
    }
    // Nothing was written outside the library, through the link or not.
    assert!(regular_files(&outside).is_empty());
    assert_eq!(names(&movies.join("Twice A (2020)")), Vec::<String>::new());
}

#[test]
fn a_release_brings_its_subtitles_under_plex_names() {
    let scratch = Scratch::new();
    let french = b"1\n00:00:01,000 --> 00:00:02,000\nBonjour\n".to_vec();
    let forced = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n".to_vec();
    let torrent = Torrent::multiple(
        &scratch.0.join("metadata"),
        "Movie.2026.1080p",
        vec![
            (PathBuf::from("Movie.mkv"), DEMO.to_vec()),
            (PathBuf::from("Movie.fr.srt"), french.clone()),
            (PathBuf::from("Movie.en.forced.srt"), forced.clone()),
            (PathBuf::from("Movie.nfo"), b"release notes".to_vec()),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let cfg = engine_config(&scratch.0.join("engine"));
    let engine = Engine::open(cfg.clone()).unwrap();
    let mut movie = request("movie", "Movie", 2026, None);
    movie.source_url = Some(torrent.magnet(seed.client.listen_port()));
    let id = engine.submit(movie).unwrap().remove(0).id;
    let ready = run_until(&engine, &id, "ready");
    let folder = cfg.movies_root.join("Movie (2026)");
    assert_eq!(
        ready.imports,
        [folder.join("Movie (2026).mkv").to_str().unwrap()]
    );
    assert_eq!(
        ready.subtitles,
        [
            folder.join("Movie (2026).fr.srt").to_str().unwrap(),
            folder.join("Movie (2026).en.forced.srt").to_str().unwrap(),
        ]
    );
    assert_eq!(
        names(&folder),
        [
            "Movie (2026).en.forced.srt",
            "Movie (2026).fr.srt",
            "Movie (2026).mkv"
        ]
    );
    assert_eq!(
        fs::read(folder.join("Movie (2026).fr.srt")).unwrap(),
        french
    );
    assert_eq!(
        fs::read(folder.join("Movie (2026).en.forced.srt")).unwrap(),
        forced
    );
    assert!(public_job(&ready).get("subtitles").is_some());
    // The payload is copied, never linked: it keeps seeding.
    for file in &ready.files {
        assert_eq!(fs::metadata(file).unwrap().nlink(), 1, "{file}");
    }
}

#[test]
fn an_import_never_overwrites_a_subtitle() {
    let scratch = Scratch::new();
    let cfg = engine_config(&scratch.0.join("engine"));
    let folder = cfg.movies_root.join("Kept (2026)");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("Kept (2026).fr.srt"), b"chosen by hand").unwrap();
    let video = source(&scratch, "Kept.mp4", DEMO);
    source(&scratch, "Kept.fr.srt", b"from the release");
    source(&scratch, "Kept.en.srt", b"english");
    source(&scratch, "Other.de.srt", b"another video's");
    let engine = Engine::open(cfg).unwrap();
    let job = import(
        &engine,
        from_path(request("movie", "Kept", 2026, None), &video),
    );
    assert_eq!(
        job.subtitles,
        [folder.join("Kept (2026).en.srt").to_str().unwrap()]
    );
    assert_eq!(
        fs::read(folder.join("Kept (2026).fr.srt")).unwrap(),
        b"chosen by hand"
    );
    assert!(
        notes(&job)
            .iter()
            .any(|note| note.starts_with("subtitle_kept: ")),
        "{:?}",
        notes(&job)
    );
    assert_eq!(
        names(&folder),
        [
            "Kept (2026).en.srt",
            "Kept (2026).fr.srt",
            "Kept (2026).mp4"
        ]
    );
}

#[test]
fn a_file_that_cannot_be_the_film_is_refused_before_anything_is_written() {
    let scratch = Scratch::new();
    let mut cfg = engine_config(&scratch.0.join("engine"));
    // Retries are allowed: a refused file must still not be tried again.
    cfg.max_attempts = 5;
    let engine = Engine::open(cfg.clone()).unwrap();
    let short = demo_lasting(22);
    let path = source(&scratch, "short.mp4", &short);
    let mut film = from_path(request("movie", "Long Film", 2020, None), &path);
    film.runtime_minutes = Some(120);
    let id = engine.submit(film).unwrap().remove(0).id;
    let failed = run_until(&engine, &id, "failed");
    let error = failed.last_error.clone().unwrap();
    assert!(error.starts_with("runtime_mismatch: "), "{error}");
    assert!(
        error.contains("22 min") && error.contains("120 min"),
        "{error}"
    );
    assert_eq!(failed.next_attempt_at, 0);
    assert!(failed.imports.is_empty());
    assert!(!cfg.movies_root.join("Long Film (2020)").exists());
    assert_eq!(fs::read(&path).unwrap(), short);
    assert_eq!(
        public_job(&failed)
            .get("last_error")
            .and_then(Value::as_str),
        Some(error.as_str())
    );

    // An extended cut is within the tolerance.
    let path = source(&scratch, "extended.mp4", &demo_lasting(150));
    let mut film = from_path(request("movie", "Extended Film", 2020, None), &path);
    film.runtime_minutes = Some(120);
    let job = import(&engine, film);
    assert_eq!(job.imports.len(), 1);
    assert!(
        notes(&job)
            .iter()
            .any(|note| note == "runtime_checked: the file lasts 150 min, the film runs 120 min"),
        "{:?}",
        notes(&job)
    );

    // Without a known runtime, the file is imported and the job says so.
    let path = source(&scratch, "unknown.mp4", DEMO);
    let job = import(
        &engine,
        from_path(request("movie", "Unknown Film", 2020, None), &path),
    );
    assert_eq!(job.imports.len(), 1);
    assert!(
        notes(&job)
            .iter()
            .any(|note| note.starts_with("runtime_check_skipped: no runtime is known")),
        "{:?}",
        notes(&job)
    );
}
