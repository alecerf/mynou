//! Runtime availability transitions, validated only by GitHub Actions.
mod library_support;
mod requester_support;

use library_support::{Directory, Indexer, library_ids, local_import, release};
use mynou::{
    config,
    engine::{Engine, lock},
    json::Value,
};
use requester_support::*;
use std::{fs, path::Path};

fn remove_or_replace(path: &Path, source: &Path, case: &str) {
    match case {
        "valid" => {}
        "missing" => fs::remove_file(path).unwrap(),
        "directory" => {
            fs::remove_file(path).unwrap();
            fs::create_dir(path).unwrap();
        }
        "symlink_file" => {
            fs::remove_file(path).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(source, path).unwrap();
        }
        "symlink_parent" => {
            let parent = path.parent().unwrap();
            let moved = parent.with_extension("moved");
            fs::rename(parent, &moved).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(moved, parent).unwrap();
        }
        _ => panic!("unknown fixture case"),
    }
}

#[test]
fn restarted_imports_fail_safely_with_plex_disabled_or_stale_indexed_paths() {
    for plex in [false, true] {
        for case in ["missing", "directory", "symlink_file", "symlink_parent"] {
            let directory = Directory::new();
            let accounts = Accounts::open();
            let mut cfg = accounts.config(&directory.0);
            cfg.requesters.accounts.clear();
            cfg.max_attempts = 1;
            let source = directory.0.join("source.mp4");
            let bytes = include_bytes!("../examples/demo.mp4");
            fs::write(&source, bytes).unwrap();
            let engine = Engine::open(cfg.clone()).unwrap();
            let mut req = request(7, "Fixture Movie");
            req.source_path = Some(source.to_str().unwrap().into());
            let id = engine.submit(req).unwrap().remove(0).id;
            assert!(engine.tick().unwrap());
            let imported = lock(&engine.store).unwrap().get(&id).unwrap();
            assert_eq!(imported.state, "imported");
            drop(engine);
            remove_or_replace(Path::new(&imported.imports[0]), &source, case);
            cfg.plex.enabled = plex;
            cfg.plex.url = accounts.url.clone();
            cfg.plex.token_env = "PATH".into();
            let mut indexed = movie(7, "Fixture Movie");
            let mut part = Value::object();
            part.insert("file", imported.imports[0].clone());
            let mut media = Value::object();
            media.insert("Part", Value::Array(vec![part]));
            indexed.insert("Media", Value::Array(vec![media]));
            accounts.response("/library/sections/1/all", 200, container(vec![indexed]));
            accounts.response("/library/sections/1/refresh", 200, Value::object());
            let engine = Engine::open(cfg.clone()).unwrap();
            assert!(engine.tick().unwrap());
            let failed = lock(&engine.store).unwrap().get(&id).unwrap();
            assert_eq!(failed.state, "failed", "{case}, Plex {plex}");
            assert_eq!(failed.imports, imported.imports);
            assert_eq!(failed.files, imported.files);
            assert_eq!(failed.next_attempt_at, 0);
            let error = failed.last_error.as_ref().unwrap();
            assert!(error.contains("restore"));
            assert!(!error.contains(directory.0.to_str().unwrap()));
            assert_eq!(fs::read(&source).unwrap(), bytes);
            assert!(accounts.calls.lock().unwrap().is_empty());
            assert!(library_ids(&engine).is_empty());
            // Retry preserves the retained record and does not copy/reacquire.
            engine.retry(&id).unwrap();
            assert!(engine.tick().unwrap());
            let failed_again = lock(&engine.store).unwrap().get(&id).unwrap();
            assert_eq!(failed_again.state, "failed");
            assert_eq!(failed_again.imports, imported.imports);
            drop(engine);
            cfg.plex.enabled = false;
            let engine = Engine::open(cfg).unwrap();
            if case == "symlink_parent" {
                let parent = Path::new(&imported.imports[0]).parent().unwrap();
                fs::remove_file(parent).unwrap();
                fs::rename(parent.with_extension("moved"), parent).unwrap();
            } else {
                let path = Path::new(&imported.imports[0]);
                if case == "directory" {
                    fs::remove_dir(path).unwrap();
                } else if case == "symlink_file" {
                    fs::remove_file(path).unwrap();
                }
                fs::write(path, bytes).unwrap();
            }
            engine.retry(&id).unwrap();
            assert!(engine.tick().unwrap());
            assert_eq!(
                lock(&engine.store).unwrap().get(&id).unwrap().state,
                "ready"
            );
        }
    }
}

#[test]
fn captured_ready_reuse_requires_files_but_pending_sharing_remains_compatible() {
    for case in [
        "valid",
        "pending",
        "missing",
        "directory",
        "symlink_file",
        "symlink_parent",
        "foreign",
        "parent",
    ] {
        let directory = Directory::new();
        let accounts = Accounts::open();
        let cfg = accounts.config(&directory.0);
        accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
        let engine = Engine::open_for_management(cfg.clone()).unwrap();
        enable(&engine, "alice");
        engine.sync_requesters().unwrap();
        let initial = job(&engine, "alice");
        let source = directory.0.join("source.mp4");
        fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
        if matches!(case, "foreign" | "parent") {
            let root = Path::new(&initial.requester.as_ref().unwrap().capture.movies_root);
            fs::create_dir_all(root).unwrap();
            let outside = root.parent().unwrap().join("outside.mp4");
            fs::write(&outside, include_bytes!("../examples/demo.mp4")).unwrap();
            let mut completed = initial.clone();
            completed.state = "ready".into();
            completed.progress = 1.0;
            completed.imports = vec![if case == "foreign" {
                outside.to_str().unwrap().into()
            } else {
                root.join("../outside.mp4").to_str().unwrap().into()
            }];
            lock(&engine.store).unwrap().update(completed).unwrap();
        } else if case != "pending" {
            let completed = ready(&engine, &initial.id);
            remove_or_replace(Path::new(&completed.imports[0]), &source, case);
        }
        drop(engine);
        // Journal recovery must allow a previously-ready import to be missing.
        let engine = Engine::open_for_management(cfg).unwrap();
        enable(&engine, "bob");
        accounts.watchlist("bob", vec![movie(7, "Fixture Movie")]);
        engine.sync_requesters().unwrap();
        let reused = demand(&engine, "bob");
        let expected = match case {
            "valid" => "ready",
            "pending" => "active",
            _ => "conflict",
        };
        assert_eq!(
            reused.get("state").and_then(Value::as_str),
            Some(expected),
            "{case}"
        );
        assert_eq!(
            reused.get("outcome").and_then(Value::as_str),
            Some(expected),
            "{case}"
        );
        if matches!(case, "valid" | "pending") {
            assert_eq!(job(&engine, "bob").id, initial.id);
        } else {
            assert_eq!(reused.get("job_id"), Some(&Value::Null));
            assert_eq!(reused.get("admitted_at"), Some(&Value::Null));
        }
        assert_eq!(lock(&engine.store).unwrap().list().len(), 1);
    }
}

#[test]
fn resumed_operator_imports_cannot_escape_the_configured_destination() {
    for case in ["foreign", "parent", "relative", "changed_root"] {
        let directory = Directory::new();
        let mut cfg = config::from_json(&config::default_json(), &directory.0).unwrap();
        cfg.downloads_enabled = false;
        cfg.max_attempts = 1;
        let source = directory.0.join("source.mp4");
        fs::write(&source, include_bytes!("../examples/demo.mp4")).unwrap();
        let engine = Engine::open_for_management(cfg.clone()).unwrap();
        let mut req = request(7, "Fixture Movie");
        req.source_path = Some(source.to_str().unwrap().into());
        let id = engine.submit(req).unwrap().remove(0).id;
        engine.tick().unwrap();
        let mut imported = lock(&engine.store).unwrap().get(&id).unwrap();
        if case == "changed_root" {
            cfg.movies_root = directory.0.join("different-library");
        } else {
            imported.imports = vec![match case {
                "foreign" => source.to_str().unwrap().into(),
                "parent" => cfg
                    .movies_root
                    .join("../source.mp4")
                    .to_str()
                    .unwrap()
                    .into(),
                "relative" => "source.mp4".into(),
                _ => unreachable!(),
            }];
            lock(&engine.store)
                .unwrap()
                .update(imported.clone())
                .unwrap();
        }
        drop(engine);
        let engine = Engine::open_for_management(cfg).unwrap();
        engine.tick().unwrap();
        let failed = lock(&engine.store).unwrap().get(&id).unwrap();
        assert_eq!(failed.state, "failed", "{case}");
        assert_eq!(failed.imports, imported.imports);
        assert!(library_ids(&engine).is_empty());
    }
}

#[test]
fn file_removed_during_plex_confirmation_never_produces_ready_outcomes() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let mut cfg = accounts.config(&directory.0);
    cfg.max_attempts = 1;
    cfg.plex.enabled = true;
    cfg.plex.url = accounts.url.clone();
    cfg.plex.token_env = "PATH".into();
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    enable(&engine, "alice");
    engine.sync_requesters().unwrap();
    let mut imported = job(&engine, "alice");
    let path = cfg.movies_root.join("retained-fixture.mp4");
    fs::create_dir_all(&cfg.movies_root).unwrap();
    fs::write(&path, include_bytes!("../examples/demo.mp4")).unwrap();
    imported.imports = vec![path.to_str().unwrap().into()];
    imported.state = "imported".into();
    lock(&engine.store)
        .unwrap()
        .update(imported.clone())
        .unwrap();
    let mut indexed = movie(7, "Fixture Movie");
    let mut part = Value::object();
    part.insert("file", imported.imports[0].clone());
    let mut media = Value::object();
    media.insert("Part", Value::Array(vec![part]));
    indexed.insert("Media", Value::Array(vec![media]));
    accounts.response("/library/sections/1/all", 200, container(vec![indexed]));
    accounts.response("/library/sections/1/refresh", 200, Value::object());
    *accounts.remove_on_request.lock().unwrap() = Some(("/library/sections/1/all".into(), path));
    assert!(engine.tick().unwrap());
    let failed = lock(&engine.store).unwrap().get(&imported.id).unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.imports, imported.imports);
    assert_eq!(
        demand(&engine, "alice")
            .get("outcome")
            .and_then(Value::as_str),
        Some("failed")
    );
    assert!(library_ids(&engine).is_empty());
    assert!(
        lock(&engine.store)
            .unwrap()
            .events(&imported.id)
            .iter()
            .all(|event| event.state != "ready")
    );
}

#[test]
fn an_unsafe_retained_upgrade_does_not_replace_its_valid_parent() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        "Fixture.Movie.2024.1080p.WEB-DL.EN",
        2,
        "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
    )]));
    let cfg = library_support::config(
        &directory.0,
        Some(&indexer),
        r#"{"resolutions":[1080,720],"languages":["en"]}"#,
    );
    let engine = Engine::open(cfg.clone()).unwrap();
    let parent = local_import(&engine, &directory.0, "Fixture Movie");
    engine
        .set_baseline(&parent.id, "Fixture.Movie.2024.720p.WEB-DL.EN")
        .unwrap();
    engine.check_upgrades(true).unwrap();
    let mut child = lock(&engine.store)
        .unwrap()
        .list()
        .into_iter()
        .find(|job| job.upgrade_parent.as_deref() == Some(parent.id.as_str()))
        .unwrap();
    let path = cfg.movies_root.join("missing-upgrade.mp4");
    child.imports = vec![path.to_str().unwrap().into()];
    child.state = "imported".into();
    lock(&engine.store).unwrap().update(child.clone()).unwrap();
    drop(engine);
    let engine = Engine::open(cfg).unwrap();
    engine.tick().unwrap();
    assert_eq!(
        lock(&engine.store).unwrap().get(&child.id).unwrap().state,
        "failed"
    );
    assert_eq!(library_ids(&engine), [parent.id]);
    assert_eq!(
        fs::read(&parent.imports[0]).unwrap(),
        include_bytes!("../examples/demo.mp4")
    );
}
