//! Native selection expansion is protected by the established API and form guards.
#[allow(dead_code)]
mod transfer_support;
mod web_support;
use mynou::{
    engine::lock,
    json::{self, Value},
    store::Request,
};
use std::fs;
use transfer_support::{BLOCK, Scratch, Torrent, engine_config, payload, wait};
use web_support::{Browser, Server, TOKEN};

#[test]
fn bearer_and_browser_expand_shared_interests_without_undoing_pause_or_modifying_source_metadata() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Controls",
        vec![
            ("first.mp4".into(), payload(BLOCK, 11)),
            ("second.mp4".into(), payload(BLOCK, 17)),
            ("third.mp4".into(), payload(BLOCK, 19)),
        ],
    );
    let metadata = fs::read(&torrent.path).unwrap();
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    // A retained mapped episode exercises native controls independently of catalog I/O.
    let request = Request {
        kind: "episode".into(),
        title: "Selection fixture".into(),
        year: 2026,
        season: 1,
        episode: 1,
        tmdb_id: Some(42),
        source_path: None,
        source_url: Some(torrent.path.to_str().unwrap().into()),
    };
    let job = lock(&server.engine.store)
        .unwrap()
        .submit_pack(request, "Controls/first.mp4".into())
        .unwrap();
    server.engine.tick().unwrap();
    server.engine.pause_transfer(&torrent.id).unwrap();
    wait(|| {
        server.engine.transfer(&torrent.id).unwrap().get("running") == Some(&Value::Bool(false))
    });
    let route = format!("/api/transfers/{}/selection", torrent.id);
    let auth = format!("Bearer {TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    assert_eq!(
        server
            .call("POST", &route, &[], r#"{"indices":[1]}"#)
            .status,
        401
    );
    let before = server.engine.transfer(&torrent.id).unwrap();
    for body in [
        r#"{"indices":[1,99]}"#,
        r#"{"indices":[1,1]}"#,
        r#"{"all":false}"#,
        r#"{"all":true,"indices":[1]}"#,
        r#"{"indices":[1],"unexpected":true}"#,
    ] {
        let response = server.call("POST", &route, &headers, body);
        assert_eq!(response.status, 400, "{}", response.body);
        assert_eq!(server.engine.transfer(&torrent.id).unwrap(), before);
    }
    let expanded = server.call("POST", &route, &headers, r#"{"indices":[1]}"#);
    assert_eq!(expanded.status, 200, "{}", expanded.body);
    let expanded = json::parse(&expanded.body).unwrap();
    assert_eq!(
        expanded.get("file_selection"),
        Some(&Value::Array(vec![
            "Controls/first.mp4".into(),
            "Controls/second.mp4".into()
        ]))
    );
    assert_eq!(expanded.get("user_paused"), Some(&Value::Bool(true)));
    let browser = Browser::login(&server);
    let detail_route = format!("/ui/transfers/{}", torrent.id);
    let page = browser.get(&server, &detail_route);
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Not selected"));
    assert!(page.body.contains("Include file"));
    page.no_secrets();
    let bad = browser.raw_post(
        &server,
        "/ui/transfers/selection",
        &format!("csrf=wrong&id={}&action=all", torrent.id),
    );
    assert_eq!(bad.status, 400);
    assert_eq!(
        server
            .engine
            .transfer(&torrent.id)
            .unwrap()
            .get("file_selection"),
        expanded.get("file_selection")
    );
    for fields in [
        vec![
            ("id", torrent.id.as_str()),
            ("action", "include"),
            ("index", "99"),
        ],
        vec![
            ("id", torrent.id.as_str()),
            ("action", "include"),
            ("index", "1.5"),
        ],
        vec![
            ("id", torrent.id.as_str()),
            ("action", "all"),
            ("index", "1"),
        ],
    ] {
        assert_eq!(
            browser
                .post(&server, "/ui/transfers/selection", &fields)
                .status,
            400
        );
        assert_eq!(
            server
                .engine
                .transfer(&torrent.id)
                .unwrap()
                .get("file_selection"),
            expanded.get("file_selection")
        );
    }
    let included = browser.post(
        &server,
        "/ui/transfers/selection",
        &[("id", &torrent.id), ("action", "include"), ("index", "2")],
    );
    assert_eq!(included.status, 303, "{}", included.body);
    assert_eq!(included.headers["location"], detail_route);
    let snapshot = server.engine.transfer(&torrent.id).unwrap();
    assert_eq!(
        snapshot
            .get("file_selection")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/transfers/selection",
                &[("id", &torrent.id), ("action", "all")]
            )
            .status,
        303
    );
    let snapshot = server.engine.transfer(&torrent.id).unwrap();
    assert_eq!(snapshot.get("file_selection"), Some(&Value::Null));
    assert_eq!(snapshot.get("user_paused"), Some(&Value::Bool(true)));
    assert_eq!(
        snapshot.get("downloaded_bytes"),
        Some(&Value::String("0".into()))
    );
    assert_eq!(fs::read(&torrent.path).unwrap(), metadata);
    let retained = lock(&server.engine.store).unwrap().get(&job.id).unwrap();
    assert_eq!(retained.pack_file.as_deref(), Some("Controls/first.mp4"));
    assert!(retained.imports.is_empty());
}
