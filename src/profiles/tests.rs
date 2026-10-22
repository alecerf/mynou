//! Synthetic per-title profile fixtures; validation runs in CI.
use super::*;
use crate::{config, crypto::random_bytes, requesters::Policy, store::Request};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix = crate::requesters::digest(&random_bytes::<16>().unwrap());
        let path = std::env::temp_dir().join(format!("mynou-title-profiles-{suffix}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Global `hd` (1080p), with `uhd` (2160p) and `sd` (720p) available per title.
fn config(directory: &Directory) -> Config {
    let mut value = config::default_json();
    value.get_mut("downloads").unwrap().insert("enabled", false);
    value.insert(
        "selection",
        json::parse(
            r#"{"movie_profile":"hd","episode_profile":"hd","profiles":{
                "hd":{"resolutions":[1080]},
                "uhd":{"resolutions":[2160],"cutoff_resolution":2160},
                "sd":{"resolutions":[720]}}}"#,
        )
        .unwrap(),
    );
    config::from_json(&value, &directory.0).unwrap()
}

fn movie(tmdb_id: u64) -> Title {
    Title {
        kind: Kind::Movie,
        tmdb_id,
    }
}
fn series(tmdb_id: u64) -> Title {
    Title {
        kind: Kind::Series,
        tmdb_id,
    }
}

fn binding(title: Title, profile: &str) -> (Title, Option<Binding>) {
    (
        title,
        Some(Binding {
            title,
            profile: profile.into(),
            origin: "operator".into(),
            updated_at: 1,
        }),
    )
}

fn request(kind: &str, title: &str, tmdb_id: Option<u64>, episode: u32) -> Request {
    Request {
        kind: kind.into(),
        title: title.into(),
        year: 2026,
        season: u32::from(episode != 0),
        episode,
        source_path: None,
        source_url: None,
        source_numbering: None,
        tmdb_id,
    }
}

fn control_json(text: &str) -> ControlRequest {
    ControlRequest::from_json(&json::parse(text).unwrap()).unwrap()
}

#[test]
fn bindings_survive_reopening_and_page_in_kind_then_identity_order() {
    let directory = Directory::new();
    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    assert_eq!(store.get(&movie(1)).unwrap(), None);
    assert_eq!(store.sequence().unwrap(), 0);
    assert!(!directory.0.join(INDEX).exists(), "reads create nothing");
    store
        .commit(&[
            binding(series(9), "sd"),
            binding(movie(2), "uhd"),
            binding(movie(1), "hd"),
            binding(series(3), "uhd"),
        ])
        .unwrap();
    drop(store);

    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    assert_eq!(store.count().unwrap(), 4);
    assert_eq!(store.get(&movie(2)).unwrap().unwrap().profile, "uhd");
    let (first, next) = store.page(None, 3).unwrap();
    let order: Vec<_> = first.iter().map(|b| b.title).collect();
    assert_eq!(order, [movie(1), movie(2), series(3)]);
    assert_eq!(next, Some(series(3)));
    let (rest, next) = store.page(next.as_ref(), 3).unwrap();
    assert_eq!(
        rest.iter().map(|b| b.title).collect::<Vec<_>>(),
        [series(9)]
    );
    assert_eq!(next, None);

    store.commit(&[(movie(2), None)]).unwrap();
    store.commit(&[(movie(404), None)]).unwrap();
    assert_eq!(store.get(&movie(2)).unwrap(), None);
    assert_eq!(store.count().unwrap(), 3);
    drop(store);

    let mut read_only = TitleProfiles::open(&directory.0, true).unwrap();
    assert_eq!(read_only.get(&series(9)).unwrap().unwrap().profile, "sd");
    assert!(read_only.commit(&[binding(movie(5), "hd")]).is_err());
    assert_eq!(read_only.get(&movie(5)).unwrap(), None);
}

#[test]
fn a_read_only_store_that_was_never_written_creates_and_accepts_nothing() {
    let directory = Directory::new();
    let mut store = TitleProfiles::open(&directory.0, true).unwrap();
    assert!(store.commit(&[binding(movie(1), "hd")]).is_err());
    assert_eq!(store.page(None, 10).unwrap(), (Vec::new(), None));
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
}

#[test]
fn an_unfinished_creation_is_preserved_rather_than_reused_or_deleted() {
    let directory = Directory::new();
    fs::create_dir(directory.0.join(STAGING)).unwrap();
    fs::write(directory.0.join(STAGING).join("partial"), b"left behind").unwrap();
    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    store.commit(&[binding(movie(1), "uhd")]).unwrap();
    let preserved: Vec<_> = fs::read_dir(&directory.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("profiles.idx.abandoned-"))
        .collect();
    assert_eq!(preserved.len(), 1);
    assert_eq!(
        fs::read(directory.0.join(&preserved[0]).join("partial")).unwrap(),
        b"left behind"
    );
    assert!(!directory.0.join(STAGING).exists());
    assert_eq!(store.get(&movie(1)).unwrap().unwrap().profile, "uhd");
}

#[test]
fn title_bindings_take_precedence_over_requester_captures_and_global_profiles() {
    let directory = Directory::new();
    let config = config(&directory);
    let mut policy = Policy::initial(&config);
    policy.movie_profile = "sd".into();
    let capture = Capture::new(&config, &policy, "movie").unwrap();
    let resolved = |kind, capture, binding| {
        resolve(&config, kind, capture, binding)
            .map(|(name, profile)| (name.to_owned(), profile.resolutions.clone()))
    };
    assert_eq!(
        resolved("movie", None, None).unwrap(),
        ("hd".into(), vec![1080])
    );
    assert_eq!(
        resolved("movie", Some(&capture), None).unwrap(),
        ("sd".into(), vec![720])
    );
    assert_eq!(
        resolved("movie", Some(&capture), Some("uhd")).unwrap(),
        ("uhd".into(), vec![2160])
    );
    assert_eq!(
        resolved("series", None, Some("uhd")).unwrap(),
        ("uhd".into(), vec![2160])
    );
    // A binding never falls back to a default when its profile was removed.
    assert_eq!(
        resolved("episode", None, Some("removed")).unwrap_err(),
        "Selection: unknown profile removed"
    );

    // The title binding overrides even a capture of the same name.
    let mut edited = config.clone();
    edited.selection.profiles.insert(
        "sd".into(),
        crate::selection::Profile::from_json(&json::parse(r#"{"resolutions":[480]}"#).unwrap())
            .unwrap(),
    );
    let (_, profile) = resolve(&edited, "movie", Some(&capture), Some("sd")).unwrap();
    assert_eq!(profile.resolutions, [480]);
    edited.selection.profiles.remove("sd");
    assert!(resolve(&edited, "movie", Some(&capture), Some("sd")).is_err());

    let mut bound = config.clone();
    bind(&mut bound, "episode", "uhd");
    assert_eq!(bound.selection.episode_profile, "uhd");
    assert_eq!(bound.selection.movie_profile, "hd");
}

#[test]
fn bound_requester_jobs_use_current_profiles_and_keep_captured_destinations() {
    let directory = Directory::new();
    let original = config(&directory);
    let capture = Capture::new(&original, &Policy::initial(&original), "movie").unwrap();
    let mut edited = original.clone();
    edited.movies_root = directory.0.join("new-movies");
    edited.selection.profiles.insert(
        "hd".into(),
        Profile::from_json(&json::parse(r#"{"resolutions":[480]}"#).unwrap()).unwrap(),
    );
    let engine = Engine::open_for_management(edited.clone()).unwrap();
    let mut job = lock(&engine.store)
        .unwrap()
        .submit(request("movie", "Captured Movie", Some(42), 0))
        .unwrap();
    job.requester = Some(crate::requesters::Provenance {
        account_id: "fixture".into(),
        demand_id: "fixture-demand".into(),
        policy_revision: 1,
        capture: capture.clone(),
    });
    let effective = engine.configuration_for(&job).unwrap();
    assert_eq!(
        effective.selection.profile("movie").unwrap().1.resolutions,
        [1080]
    );
    lock(&engine.title_profiles)
        .unwrap()
        .commit(&[binding(movie(42), "hd")])
        .unwrap();
    let effective = engine.configuration_for(&job).unwrap();
    assert_eq!(
        effective.selection.profile("movie").unwrap().1.resolutions,
        [480]
    );
    assert_eq!(effective.movies_root, PathBuf::from(&capture.movies_root));
    drop(engine);

    edited.selection.profiles.remove("hd");
    edited.selection.movie_profile = "uhd".into();
    edited.selection.episode_profile = "uhd".into();
    let engine = Engine::open_for_management(edited).unwrap();
    let effective = engine.configuration_for(&job).unwrap();
    assert_eq!(
        effective.selection.profile("movie").unwrap_err(),
        "Selection: unknown profile hd"
    );
    assert_eq!(effective.movies_root, PathBuf::from(&capture.movies_root));
}

#[test]
fn requests_bind_movies_by_identity_and_episodes_or_packs_by_their_series() {
    assert_eq!(
        Title::of(&request("movie", "A", Some(7), 0)),
        Some(movie(7))
    );
    assert_eq!(
        Title::of(&request("episode", "S", Some(7), 2)),
        Some(series(7))
    );
    assert_eq!(
        Title::of(&request("series", "S", Some(7), 0)),
        Some(series(7))
    );
    assert_eq!(Title::of(&request("movie", "Untracked", None, 0)), None);
    assert_eq!(Title::of(&request("file", "Manual", Some(7), 0)), None);
}

#[test]
fn reviewed_changes_preview_without_writing_and_apply_only_an_unchanged_plan() {
    let directory = Directory::new();
    let config = config(&directory);
    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    let preview = control(
        &config,
        &mut store,
        &control_json(r#"{"action":"set","kind":"movie","tmdb_id":42,"profile":"uhd"}"#),
    )
    .unwrap();
    assert_eq!(preview.get("bind").and_then(Value::as_u64), Some(1));
    assert_eq!(preview.get("applied"), Some(&Value::Bool(false)));
    assert_eq!(store.get(&movie(42)).unwrap(), None);
    assert!(
        !directory.0.join(INDEX).exists(),
        "a preview writes nothing"
    );
    let plan = preview.get("plan_id").and_then(Value::as_str).unwrap();

    let stale = format!(
        r#"{{"action":"set","kind":"movie","tmdb_id":42,"profile":"uhd","apply":true,"plan_id":"{}"}}"#,
        "0".repeat(64)
    );
    assert!(control(&config, &mut store, &control_json(&stale)).is_err());
    assert_eq!(store.get(&movie(42)).unwrap(), None);

    let apply = format!(
        r#"{{"action":"set","kind":"movie","tmdb_id":42,"profile":"uhd","apply":true,"plan_id":"{plan}"}}"#
    );
    let applied = control(&config, &mut store, &control_json(&apply)).unwrap();
    assert_eq!(applied.get("applied"), Some(&Value::Bool(true)));
    let stored = store.get(&movie(42)).unwrap().unwrap();
    assert_eq!(
        (stored.profile.as_str(), stored.origin.as_str()),
        ("uhd", "operator")
    );
    // The same guard cannot be replayed after the store changed.
    assert!(control(&config, &mut store, &control_json(&apply)).is_err());

    // A configuration edit to the named profiles also invalidates a reviewed plan.
    let clear = control(
        &config,
        &mut store,
        &control_json(r#"{"action":"clear","kind":"movie","tmdb_id":42}"#),
    )
    .unwrap();
    let clear_plan = clear.get("plan_id").and_then(Value::as_str).unwrap();
    let mut edited = config.clone();
    edited.selection.profiles.remove("sd");
    let clear_apply = format!(
        r#"{{"action":"clear","kind":"movie","tmdb_id":42,"apply":true,"plan_id":"{clear_plan}"}}"#
    );
    assert!(control(&edited, &mut store, &control_json(&clear_apply)).is_err());
    control(&config, &mut store, &control_json(&clear_apply)).unwrap();
    assert_eq!(store.get(&movie(42)).unwrap(), None);

    let unknown =
        control_json(r#"{"action":"set","kind":"series","tmdb_id":7,"profile":"absent"}"#);
    assert_eq!(
        control(&config, &mut store, &unknown).unwrap_err(),
        "Title profile: the selected profile is not configured"
    );
    for invalid in [
        r#"{"action":"set","kind":"movie","tmdb_id":42}"#,
        r#"{"action":"clear","kind":"movie","tmdb_id":42,"profile":"hd"}"#,
        r#"{"action":"set","kind":"episode","tmdb_id":42,"profile":"hd"}"#,
        r#"{"action":"set","kind":"movie","tmdb_id":0,"profile":"hd"}"#,
        r#"{"action":"set","kind":"movie","tmdb_id":42,"profile":"hd","apply":true}"#,
        r#"{"action":"import","kind":"movie","tmdb_id":42}"#,
        r#"{"action":"set","kind":"movie","tmdb_id":42,"profile":"hd","extra":1}"#,
    ] {
        assert!(
            ControlRequest::from_json(&json::parse(invalid).unwrap()).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn bindings_stored_by_the_removed_import_stay_usable_and_import_is_refused() {
    let directory = Directory::new();
    let config = config(&directory);
    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    let (title, stored) = binding(movie(1), "uhd");
    let mut stored = stored.unwrap();
    stored.origin = "migration".into();
    store.commit(&[(title, Some(stored))]).unwrap();
    drop(store);
    let mut store = TitleProfiles::open(&directory.0, true).unwrap();
    let kept = store.get(&movie(1)).unwrap().unwrap();
    assert_eq!(
        (kept.profile.as_str(), kept.origin.as_str()),
        ("uhd", "migration")
    );
    let listed = list(&config, &mut store, None, 10).unwrap();
    let row = &listed.get("bindings").and_then(Value::as_array).unwrap()[0];
    assert_eq!(row.get("resolved"), Some(&Value::Bool(true)));
    drop(store);
    // An explicit operator change replaces it like any other binding.
    let mut store = TitleProfiles::open(&directory.0, false).unwrap();
    let preview = control(
        &config,
        &mut store,
        &control_json(r#"{"action":"set","kind":"movie","tmdb_id":1,"profile":"hd"}"#),
    )
    .unwrap();
    assert_eq!(preview.get("rebind").and_then(Value::as_u64), Some(1));
    for removed in [
        r#"{"action":"import","import":{"profiles":{"A":"hd"},"titles":[]}}"#,
        r#"{"action":"set","kind":"movie","tmdb_id":1,"profile":"hd","import":{}}"#,
    ] {
        assert!(
            ControlRequest::from_json(&json::parse(removed).unwrap()).is_err(),
            "{removed}"
        );
    }
}

#[test]
fn engine_selection_follows_bindings_for_movies_series_episodes_and_restarts() {
    let directory = Directory::new();
    let config = config(&directory);
    let first_movie = request("movie", "First Movie", Some(11), 0);
    let second_movie = request("movie", "Second Movie", Some(12), 0);
    let first_series = request("episode", "First Series", Some(21), 1);
    let second_series = request("episode", "Second Series", Some(22), 3);
    let untracked = request("movie", "Untracked Movie", None, 0);
    let engine = Engine::open_for_management(config.clone()).unwrap();
    let jobs: Vec<_> = [
        &first_movie,
        &second_movie,
        &first_series,
        &second_series,
        &untracked,
    ]
    .into_iter()
    .map(|r| lock(&engine.store).unwrap().submit(r.clone()).unwrap())
    .collect();
    let profile = |engine: &Engine, job: &crate::store::Job| {
        let config = engine.configuration_for(job).unwrap();
        let (name, _) = config.selection.profile(&job.request.kind).unwrap();
        name.to_owned()
    };
    assert!(jobs.iter().all(|job| profile(&engine, job) == "hd"));

    let mut store = lock(&engine.title_profiles).unwrap();
    store
        .commit(&[
            binding(movie(11), "uhd"),
            binding(movie(12), "sd"),
            binding(series(21), "uhd"),
            binding(series(22), "sd"),
        ])
        .unwrap();
    drop(store);
    let expected = ["uhd", "sd", "uhd", "sd", "hd"];
    for (job, name) in jobs.iter().zip(expected) {
        assert_eq!(profile(&engine, job), name, "{}", job.request.title);
    }
    let pack = request("series", "First Series", Some(21), 0);
    let pack_config = engine.configuration_for_request(&pack).unwrap();
    assert_eq!(pack_config.selection.profile("episode").unwrap().0, "uhd");
    drop(engine);

    let engine = Engine::open_for_management(config.clone()).unwrap();
    for (job, name) in jobs.iter().zip(expected) {
        assert_eq!(profile(&engine, job), name, "after restart");
    }
    drop(engine);

    // A removed profile leaves the bound title unresolved instead of using `hd`.
    let mut edited = config.clone();
    edited.selection.profiles.remove("sd");
    let engine = Engine::open_for_management(edited).unwrap();
    let config = engine.configuration_for(&jobs[1]).unwrap();
    assert_eq!(
        config.selection.profile("movie").unwrap_err(),
        "Selection: unknown profile sd"
    );
    assert_eq!(profile(&engine, &jobs[0]), "uhd");
}
