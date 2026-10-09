//! CI-only real local native peers, captured routes and exact Plex confirmation.
mod automatic_pack_support;
mod library_support;
mod requester_support;
#[allow(dead_code)]
mod transfer_support;
use automatic_pack_support::{Provider, release};
use library_support::{Directory, run_until};
use mynou::{
    engine::{Engine, lock},
    json::Value,
};
use requester_support::*;
use std::fs;

#[test]
fn existing_plex_media_under_the_mapped_destination_needs_no_native_acquisition() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let mut cfg = accounts.config(&directory.0);
    cfg.plex.enabled = true;
    cfg.plex.url = accounts.url.clone();
    cfg.plex.token_env = "PATH".into();
    cfg.plex.path_mappings = vec![mynou::config::PathMapping {
        mynou_prefix: cfg.requesters.destinations[0]
            .movies_root
            .to_string_lossy()
            .into_owned(),
        plex_prefix: "/plex/family".into(),
    }];
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    let mut present = movie(7, "Fixture Movie");
    let mut media = Value::object();
    let mut part = Value::object();
    part.insert("file", "/plex/family/Fixture Movie/Feature.mp4");
    media.insert("Part", Value::Array(vec![part]));
    present.insert("Media", Value::Array(vec![media]));
    accounts.response("/library/sections/1/all", 200, container(vec![present]));
    let engine = Engine::open_for_management(cfg).unwrap();
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    p.destination = "family".into();
    apply(&engine, "alice", policy_query(p));
    engine.sync_requesters().unwrap();
    assert!(engine.tick().unwrap());
    let fulfilled = job(&engine, "alice");
    assert_eq!(fulfilled.state, "ready");
    assert!(fulfilled.download_id.is_none());
    assert!(fulfilled.imports.is_empty());
    assert_eq!(
        demand(&engine, "alice")
            .get("state")
            .and_then(Value::as_str),
        Some("ready")
    );
}
use transfer_support::{Seeder, Torrent};

#[test]
fn opt_in_precedes_native_acquisition_and_captured_route_survives_edit_restart_and_confirmation() {
    let directory = Directory::new();
    let accounts = Accounts::open();
    let provider = Provider::open();
    let bytes = include_bytes!("../examples/demo.mp4").to_vec();
    let torrent = Torrent::single(
        &directory.0.join("source"),
        "Fixture.Movie.1080p.mp4",
        bytes.clone(),
    );
    let seed = Seeder::open(&directory.0.join("seeder"), &[&torrent]);
    provider.releases(vec![release(
        "Fixture Movie 2024 1080p WEB-DL",
        &torrent.magnet(seed.client.listen_port()),
        8,
    )]);
    let mut cfg = accounts.config(&directory.0.join("engine"));
    cfg.downloads_enabled = true;
    cfg.sources = vec![provider.source("json")];
    cfg.plex.enabled = true;
    cfg.plex.url = accounts.url.clone();
    cfg.plex.token_env = "PATH".into();
    cfg.plex.token_override = None;
    accounts.watchlist("alice", vec![movie(7, "Fixture Movie")]);
    accounts.response("/library/sections/1/refresh", 200, Value::object());
    let mut old = movie(7, "Fixture Movie");
    let mut media = Value::object();
    let mut part = Value::object();
    part.insert("file", "/old-unrelated/Fixture Movie.mp4");
    media.insert("Part", Value::Array(vec![part]));
    old.insert("Media", Value::Array(vec![media]));
    accounts.response("/library/sections/1/all", 200, container(vec![old.clone()]));
    let engine = Engine::open(cfg.clone()).unwrap();
    engine.sync_requesters().unwrap();
    assert!(!engine.tick().unwrap());
    assert!(provider.calls.lock().unwrap().is_empty());
    assert!(lock(&engine.store).unwrap().list().is_empty());
    assert!(!cfg.downloads.data_dir.join(&torrent.id).exists());
    let mut p = policy(&engine, "alice");
    p.enabled = true;
    p.destination = "family".into();
    apply(&engine, "alice", policy_query(p));
    let acquired = job(&engine, "alice");
    let captured = acquired.requester.clone();
    let mut p = policy(&engine, "alice");
    p.destination = "default".into();
    apply(&engine, "alice", policy_query(p));
    drop(engine);
    let mut changed = cfg.clone();
    changed
        .selection
        .profiles
        .get_mut("any")
        .unwrap()
        .blocked_terms = vec!["fixture".into()];
    changed.requesters.destinations[0].movies_root = directory.0.join("changed-route");
    let engine = Engine::open(changed.clone()).unwrap();
    let scanning = run_until(&engine, &acquired.id, "scanning");
    assert!(
        scanning.imports[0].starts_with(
            &cfg.requesters.destinations[0]
                .movies_root
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(fs::read(&scanning.imports[0]).unwrap(), bytes);
    assert_eq!(scanning.requester, captured);
    drop(engine);
    let engine = Engine::open(changed).unwrap();
    assert_eq!(
        lock(&engine.store)
            .unwrap()
            .get(&acquired.id)
            .unwrap()
            .requester,
        captured
    );
    let mut media = Value::object();
    let mut part = Value::object();
    part.insert("file", scanning.imports[0].clone());
    media.insert("Part", Value::Array(vec![part]));
    old.insert("Media", Value::Array(vec![media]));
    accounts.response("/library/sections/1/all", 200, container(vec![old]));
    let ready_job = run_until(&engine, &acquired.id, "ready");
    assert_eq!(ready_job.imports, scanning.imports);
    assert_eq!(ready_job.requester, captured);
    assert_eq!(fs::read(&ready_job.imports[0]).unwrap(), bytes);
    let notifications = engine.requester("alice", 0, 100).unwrap();
    assert!(
        notifications
            .get("notifications")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.get("outcome").and_then(Value::as_str) == Some("ready"))
    );
    no_credentials(&notifications);
}
