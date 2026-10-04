//! Native browser controls with original synthetic torrent metadata and local peers.
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use mynou::{engine::lock, json::Value, store::Request};
use std::fs;
use transfer_support::{BLOCK, Scratch, Torrent, engine_config, payload};
use web_support::{Browser, Server};

fn request(path: &str) -> Request {
    Request {
        kind: "file".into(),
        title: "Web transfer fixture".into(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: Some(path.into()),
        source_numbering: None,
        tmdb_id: None,
    }
}

#[test]
fn browser_transfer_controls_change_native_state_and_keep_sources() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "web-controls.bin", payload(BLOCK, 94));
    let metadata = fs::read(&torrent.path).unwrap();
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    let job = server
        .engine
        .submit(request(torrent.path.to_str().unwrap()))
        .unwrap()
        .remove(0);
    assert!(server.engine.tick().unwrap());
    let browser = Browser::login(&server);
    let paused = browser.post(
        &server,
        "/ui/transfers/action",
        &[("id", &torrent.id), ("action", "pause")],
    );
    assert_eq!(paused.status, 303, "{}", paused.body);
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("user_paused"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/priority",
                &[("id", &torrent.id), ("priority", "20")]
            )
            .status,
        303
    );
    assert_eq!(
        server.engine.transfer(&torrent.id).unwrap().get("priority"),
        Some(&Value::Number(20.0))
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/files",
                &[("id", &torrent.id), ("index", "0"), ("priority", "high")]
            )
            .status,
        303
    );
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("files")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("priority"),
        Some(&Value::String("high".into()))
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/policy",
                &[
                    ("id", &torrent.id),
                    ("action", "save"),
                    ("download_limit_bps", "65536"),
                    ("upload_limit_bps", "32768"),
                    ("seed_ratio_milli", "1500"),
                    ("seed_time_secs", "3600")
                ]
            )
            .status,
        303
    );
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("policy")
            .unwrap()
            .get("seed_ratio_milli"),
        Some(&Value::Number(1500.0))
    );
    let detail = browser.get(&server, &format!("/ui/transfers/{}", torrent.id));
    assert_eq!(detail.status, 200);
    assert!(detail.body.contains("web-controls.bin"));
    assert!(detail.body.contains(&format!("/ui/jobs/{}", job.id)));
    assert!(detail.body.contains("Files and selection"));
    assert!(detail.body.contains("value=\"1500\""));
    detail.no_secrets();
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/policy",
                &[("id", &torrent.id), ("action", "reset")]
            )
            .status,
        303
    );
    assert_eq!(
        server.engine.transfer(&torrent.id).unwrap().get("policy"),
        Some(&Value::Null)
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/action",
                &[("id", &torrent.id), ("action", "resume")]
            )
            .status,
        303
    );
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("user_paused"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        lock(&server.engine.store)
            .unwrap()
            .get(&job.id)
            .unwrap()
            .state,
        "downloading"
    );
    assert_eq!(fs::read(&torrent.path).unwrap(), metadata);
    browser.get(&server, "/ui/transfers").no_secrets();
}

#[test]
fn invalid_transfer_forms_preserve_priority_policy_and_file_choices() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "web-invalid.bin", payload(BLOCK, 95));
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    server
        .engine
        .submit(request(torrent.path.to_str().unwrap()))
        .unwrap();
    server.engine.tick().unwrap();
    server.engine.pause_transfer(&torrent.id).unwrap();
    let browser = Browser::login(&server);
    let before = server.engine.transfer(&torrent.id).unwrap();
    let cases = [
        (
            "/ui/transfers/priority",
            vec![("id", torrent.id.as_str()), ("priority", "1001")],
        ),
        (
            "/ui/transfers/priority",
            vec![("id", torrent.id.as_str()), ("priority", "1.5")],
        ),
        (
            "/ui/transfers/files",
            vec![
                ("id", torrent.id.as_str()),
                ("index", "0"),
                ("priority", "skip"),
            ],
        ),
        (
            "/ui/transfers/files",
            vec![
                ("id", torrent.id.as_str()),
                ("index", "999"),
                ("priority", "high"),
            ],
        ),
        (
            "/ui/transfers/policy",
            vec![
                ("id", torrent.id.as_str()),
                ("action", "save"),
                ("download_limit_bps", "1073741825"),
            ],
        ),
        (
            "/ui/transfers/policy",
            vec![
                ("id", torrent.id.as_str()),
                ("action", "save"),
                ("seed_ratio_milli", "0"),
            ],
        ),
        (
            "/ui/transfers/policy",
            vec![
                ("id", torrent.id.as_str()),
                ("action", "save"),
                ("seed_time_secs", "315360001"),
            ],
        ),
    ];
    for (route, fields) in cases {
        let response = browser.post(&server, route, &fields);
        assert_eq!(response.status, 400, "{route}: {}", response.body);
        response.no_secrets();
        let after = server.engine.transfer(&torrent.id).unwrap();
        for key in ["user_paused", "priority", "policy", "files"] {
            assert_eq!(after.get(key), before.get(key), "{key} changed for {route}");
        }
    }
}

#[test]
fn bulk_transfer_validation_and_unknown_entries_report_independent_results() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "web-bulk.bin", payload(BLOCK, 96));
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    server
        .engine
        .submit(request(torrent.path.to_str().unwrap()))
        .unwrap();
    server.engine.tick().unwrap();
    server.engine.pause_transfer(&torrent.id).unwrap();
    let browser = Browser::login(&server);
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/action",
                &[("action", "resume"), ("id", &torrent.id), ("id", "invalid")]
            )
            .status,
        400
    );
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("user_paused"),
        Some(&Value::Bool(true))
    );
    let missing = "f".repeat(40);
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/action",
                &[("action", "resume"), ("id", &torrent.id), ("id", &missing)]
            )
            .status,
        303
    );
    let page = browser.get(&server, "/ui/transfers");
    assert_eq!(page.status, 200);
    assert!(page.body.contains("1 of 2 action(s) succeeded"));
    assert!(page.body.contains("Unknown download"));
    page.no_secrets();
}
