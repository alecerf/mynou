//! Browser form journeys against the real native listener. CI execution only.
mod library_support;
mod web_support;
use library_support::{Directory, Indexer, configuration, local_import, movie, release};
use mynou::{config, engine::lock, json::Value};
use std::fs;
use web_support::{Browser, Server, TOKEN, fields};

fn server(directory: &Directory) -> Server {
    Server::open(config::from_json(&configuration(None, "{}"), &directory.0).unwrap())
}

#[test]
fn browser_sign_in_sign_out_and_api_authentication_are_separate() {
    let directory = Directory::new();
    let server = server(&directory);
    let root = server.call("GET", "/", &[], "");
    assert_eq!(root.status, 303);
    assert_eq!(root.headers["location"], "/ui");
    assert_eq!(
        server.call("GET", "/ui/jobs", &[], "").headers["location"],
        "/ui/login"
    );
    let browser = Browser::login(&server);
    let page = browser.get(&server, "/ui/jobs");
    assert_eq!(page.status, 200);
    assert!(page.headers["content-type"].starts_with("text/html"));
    assert_eq!(page.headers["cache-control"], "no-store");
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert!(page.headers["content-security-policy"].contains("default-src 'none'"));
    assert_eq!(
        server
            .call("GET", "/api/jobs", &[("Cookie", &browser.cookie)], "")
            .status,
        401
    );
    assert_eq!(
        server
            .call(
                "GET",
                "/api/jobs",
                &[("Authorization", &format!("Bearer {TOKEN}"))],
                ""
            )
            .status,
        200
    );
    assert_eq!(
        server
            .call(
                "GET",
                "/ui/jobs",
                &[("Authorization", &format!("Bearer {TOKEN}"))],
                ""
            )
            .status,
        303
    );
    let logout = browser.post(&server, "/ui/logout", &[]);
    assert_eq!(logout.status, 303);
    assert!(logout.headers["set-cookie"].contains("Max-Age=0"));
    assert_eq!(browser.get(&server, "/ui/jobs").status, 303);
}

#[test]
fn login_rejects_wrong_token_missing_origin_and_login_csrf() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::challenge(&server);
    let wrong = browser.raw_post(
        &server,
        "/ui/login",
        &fields(&[("csrf", &browser.csrf), ("token", "wrong-token")]),
    );
    assert_eq!(wrong.status, 401);
    wrong.no_secrets();
    let values = fields(&[("csrf", &browser.csrf), ("token", TOKEN)]);
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/login",
                &[
                    ("Cookie", &browser.cookie),
                    ("Content-Type", "application/x-www-form-urlencoded")
                ],
                &values
            )
            .status,
        403
    );
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/login",
                &[
                    ("Origin", &server.origin()),
                    ("Content-Type", "application/x-www-form-urlencoded")
                ],
                &values
            )
            .status,
        403
    );
    assert_eq!(
        browser
            .raw_post(
                &server,
                "/ui/login",
                &fields(&[("csrf", "wrong"), ("token", TOKEN)])
            )
            .status,
        403
    );
    assert_eq!(browser.get(&server, "/ui/jobs").status, 303);
}

#[test]
fn same_origin_csrf_and_cookie_ambiguity_cannot_mutate_a_job() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let job = server
        .engine
        .submit(movie("Origin Fixture"))
        .unwrap()
        .remove(0);
    let body = fields(&[
        ("csrf", &browser.csrf),
        ("action", "cancel"),
        ("id", &job.id),
    ]);
    let origin = server.origin();
    for extra in [
        vec![("Origin", "http://attacker.invalid")],
        vec![
            ("Origin", origin.as_str()),
            ("Sec-Fetch-Site", "cross-site"),
        ],
    ] {
        let mut headers = vec![
            ("Cookie", browser.cookie.as_str()),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ];
        headers.extend(extra);
        assert_eq!(
            server
                .call("POST", "/ui/jobs/action", &headers, &body)
                .status,
            403
        );
    }
    assert_eq!(
        browser
            .raw_post(
                &server,
                "/ui/jobs/action",
                &fields(&[("csrf", "invalid"), ("action", "cancel"), ("id", &job.id)])
            )
            .status,
        403
    );
    let duplicate_cookie = format!("{}; {}", browser.cookie, browser.cookie);
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/jobs/action",
                &[
                    ("Cookie", &duplicate_cookie),
                    ("Origin", &server.origin()),
                    ("Content-Type", "application/x-www-form-urlencoded")
                ],
                &body
            )
            .status,
        400
    );
    assert_eq!(
        lock(&server.engine.store)
            .unwrap()
            .get(&job.id)
            .unwrap()
            .state,
        "queued"
    );
}

#[test]
fn browser_sessions_are_host_bound_and_https_sessions_require_the_same_origin() {
    let directory = Directory::new();
    let server = server(&directory);
    let challenge = Browser::challenge(&server);
    let origin = format!("https://{}", server.authority);
    let login = server.call(
        "POST",
        "/ui/login",
        &[
            ("Cookie", &challenge.cookie),
            ("Origin", &origin),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
        &fields(&[("csrf", &challenge.csrf), ("token", TOKEN)]),
    );
    assert_eq!(login.status, 303);
    assert!(login.headers["set-cookie"].contains("; Secure"));
    assert!(login.headers["set-cookie"].contains("HttpOnly; SameSite=Strict"));
    let cookie = login.headers["set-cookie"].split(';').next().unwrap();
    let page = server.call("GET", "/ui", &[("Cookie", cookie)], "");
    let csrf = web_support::csrf(&page.body);
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/logout",
                &[
                    ("Cookie", cookie),
                    ("Origin", &server.origin()),
                    ("Content-Type", "application/x-www-form-urlencoded")
                ],
                &fields(&[("csrf", &csrf)])
            )
            .status,
        403
    );
    assert_eq!(
        server
            .call(
                "GET",
                "/ui/jobs",
                &[("Cookie", cookie), ("Host", "attacker.invalid")],
                ""
            )
            .status,
        303
    );
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/logout",
                &[
                    ("Cookie", cookie),
                    ("Origin", &origin),
                    ("Content-Type", "application/x-www-form-urlencoded")
                ],
                &fields(&[("csrf", &csrf)])
            )
            .status,
        303
    );
}

#[test]
fn browser_forms_submit_deduplicate_cancel_and_retry_requests() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let values = [
        ("kind", "movie"),
        ("title", "Form Movie"),
        ("year", "2024"),
        ("source_kind", "auto"),
    ];
    for _ in 0..2 {
        assert_eq!(browser.post(&server, "/ui/requests", &values).status, 303);
    }
    let jobs = lock(&server.engine.store).unwrap().list();
    assert_eq!(jobs.len(), 1);
    let id = &jobs[0].id;
    assert!(
        browser
            .get(&server, "/ui/jobs")
            .body
            .contains("Existing requests are reused")
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/jobs/action",
                &[("id", id), ("action", "cancel")]
            )
            .status,
        303
    );
    assert_eq!(
        lock(&server.engine.store).unwrap().get(id).unwrap().state,
        "cancelled"
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/jobs/action",
                &[("id", id), ("action", "retry")]
            )
            .status,
        303
    );
    assert_eq!(
        lock(&server.engine.store).unwrap().get(id).unwrap().state,
        "queued"
    );
    let detail = browser.get(&server, &format!("/ui/jobs/{id}"));
    assert!(detail.body.contains("request cancelled"));
    detail.no_secrets();
}

#[test]
fn bulk_controls_prevalidate_every_identifier_and_report_partial_failures() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let a = server.engine.submit(movie("Bulk A")).unwrap().remove(0);
    let b = server.engine.submit(movie("Bulk B")).unwrap().remove(0);
    for values in [
        vec![
            ("action", "cancel"),
            ("id", a.id.as_str()),
            ("id", "invalid"),
        ],
        vec![("action", "delete"), ("id", a.id.as_str())],
        vec![
            ("action", "cancel"),
            ("id", a.id.as_str()),
            ("id", a.id.as_str()),
        ],
    ] {
        assert_eq!(
            browser.post(&server, "/ui/jobs/action", &values).status,
            400
        );
        assert_eq!(
            lock(&server.engine.store)
                .unwrap()
                .get(&a.id)
                .unwrap()
                .state,
            "queued"
        );
    }
    let missing = "f".repeat(32);
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/jobs/action",
                &[
                    ("action", "cancel"),
                    ("id", &a.id),
                    ("id", &missing),
                    ("id", &b.id)
                ]
            )
            .status,
        303
    );
    assert_eq!(
        lock(&server.engine.store)
            .unwrap()
            .get(&a.id)
            .unwrap()
            .state,
        "cancelled"
    );
    assert_eq!(
        lock(&server.engine.store)
            .unwrap()
            .get(&b.id)
            .unwrap()
            .state,
        "cancelled"
    );
    let result = browser.get(&server, "/ui/jobs");
    assert!(result.body.contains("2 of 3 action(s) succeeded"));
    assert!(result.body.contains("unknown job"));
    result.no_secrets();
    assert!(
        !browser
            .get(&server, "/ui/jobs")
            .body
            .contains("2 of 3 action(s) succeeded")
    );
}

#[test]
fn malformed_forms_and_navigation_have_no_side_effects() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let prefix = format!("csrf={}&", browser.csrf);
    for suffix in [
        "kind=movie&title=a&title=b",
        "kind=movie&title=%FF",
        "kind=movie&title=a&year=10000",
        "kind=movie&title=a&redirect=evil",
        "kind=movie&title=a&source_kind=auto&source_value=secret",
    ] {
        assert_eq!(
            browser
                .raw_post(&server, "/ui/requests", &format!("{prefix}{suffix}"))
                .status,
            400
        );
    }
    assert_eq!(
        browser
            .raw_post(&server, "/ui/requests", &"a".repeat(65_537))
            .status,
        400
    );
    assert_eq!(
        browser
            .raw_post(&server, "/ui/requests?redirect=evil", &prefix)
            .status,
        400
    );
    for route in [
        "/ui/jobs?page=0",
        "/ui/jobs?page=100001",
        "/ui/jobs?page=1&page=2",
        "/ui/jobs?redirect=evil",
        "/ui/jobs?q=%00",
        "/ui/jobs?state=unknown",
        "/ui/login?token=secret",
    ] {
        assert_eq!(browser.get(&server, route).status, 400, "{route}");
    }
    assert_eq!(
        server
            .call(
                "POST",
                "/ui/requests",
                &[
                    ("Cookie", &browser.cookie),
                    ("Origin", &server.origin()),
                    ("Content-Type", "application/json")
                ],
                "{}"
            )
            .status,
        415
    );
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
}

#[test]
fn job_pages_escape_titles_paths_and_errors_without_exposing_urls() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let mut request = movie("<script>alert('x')</script> & Movie");
    request.source_url =
        Some("http://127.0.0.1:1/file?token=library-download-fixture-secret".into());
    let mut job = server.engine.submit(request).unwrap().remove(0);
    job.last_error = Some(
        "failed https://provider.invalid/torrent?apikey=library-indexer-fixture-secret".into(),
    );
    job.files = vec!["/source/<img onerror='alert(1)'>.mp4".into()];
    lock(&server.engine.store)
        .unwrap()
        .update(job.clone())
        .unwrap();
    for route in ["/ui/jobs".into(), format!("/ui/jobs/{}", job.id)] {
        let response = browser.get(&server, &route);
        assert_eq!(response.status, 200);
        assert!(response.body.contains("&lt;script&gt;"));
        assert!(!response.body.contains("<script>"));
        response.no_secrets();
    }
    let detail = browser.get(&server, &format!("/ui/jobs/{}", job.id));
    assert!(detail.body.contains("[redacted]"));
    assert!(detail.body.contains("&lt;img"));
    assert!(!detail.body.contains("<img"));
}

#[test]
fn filtered_job_pages_are_bounded_and_keep_pagination_filters() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    for n in 0..55 {
        server
            .engine
            .submit(movie(&format!("Page Fixture {n:03}")))
            .unwrap();
    }
    let first = browser.get(&server, "/ui/jobs?q=Page+Fixture&state=queued");
    assert_eq!(first.status, 200);
    assert_eq!(first.body.matches("type=checkbox").count(), 50);
    assert!(first.body.contains("55 entries · Page 1 of 2"));
    assert!(
        first
            .body
            .contains("q=Page%20Fixture&amp;state=queued&amp;page=2")
    );
    let second = browser.get(&server, "/ui/jobs?q=Page+Fixture&state=queued&page=2");
    assert_eq!(second.body.matches("type=checkbox").count(), 5);
    let filtered = browser.get(&server, "/ui/jobs?q=Page+Fixture+054&state=queued");
    assert_eq!(filtered.body.matches("type=checkbox").count(), 1);
    assert!(filtered.body.contains("1 entries · Page 1 of 1"));
}

#[test]
fn search_previews_show_decisions_preserve_identity_and_never_write_jobs() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![
        release(
            "Fixture.Movie.2024.1080p.WEB-DL.EN",
            4,
            "http://127.0.0.1:1/download?token=library-download-fixture-secret",
        ),
        release(
            "Other.Movie.2024.1080p.WEB-DL.EN",
            100,
            "http://127.0.0.1:1/other?token=library-download-fixture-secret",
        ),
    ]));
    let server = Server::open(
        config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap(),
    );
    let browser = Browser::login(&server);
    let preview = browser.post(
        &server,
        "/ui/search",
        &[
            ("kind", "movie"),
            ("title", "Fixture Movie"),
            ("year", "2024"),
            ("source_kind", "auto"),
        ],
    );
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert!(preview.body.contains("Automatically selected"));
    assert!(preview.body.contains("Fixture.Movie.2024.1080p.WEB-DL.EN"));
    assert!(preview.body.contains("value=\"Fixture Movie\""));
    assert!(preview.body.contains("value=2024"));
    preview.no_secrets();
    assert!(lock(&server.engine.store).unwrap().list().is_empty());
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/requests",
                &[
                    ("kind", "movie"),
                    ("title", "Fixture Movie"),
                    ("year", "2024"),
                    ("source_kind", "auto")
                ]
            )
            .status,
        303
    );
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 1);
}

#[test]
fn library_forms_monitor_record_baseline_and_preview_upgrades_with_file_retention() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        "Fixture.Movie.2024.1080p.WEB-DL.EN",
        4,
        "http://127.0.0.1:1/download?token=library-download-fixture-secret",
    )]));
    let server = Server::open(
        config::from_json(
            &configuration(Some(&indexer), r#"{"resolutions":[1080,720]}"#),
            &directory.0,
        )
        .unwrap(),
    );
    let job = local_import(&server.engine, &directory.0, "Fixture Movie");
    let before = fs::read(&job.imports[0]).unwrap();
    let browser = Browser::login(&server);
    assert!(
        browser
            .get(&server, "/ui/library")
            .body
            .contains("Needs baseline")
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/library/action",
                &[("id", &job.id), ("action", "unmonitor")]
            )
            .status,
        303
    );
    assert!(
        !lock(&server.engine.store)
            .unwrap()
            .get(&job.id)
            .unwrap()
            .monitored
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/library/action",
                &[("id", &job.id), ("action", "monitor")]
            )
            .status,
        303
    );
    assert_eq!(
        browser
            .post(
                &server,
                "/ui/library/baseline",
                &[
                    ("id", &job.id),
                    ("release_title", "Fixture.Movie.2024.720p.WEB-DL.EN")
                ]
            )
            .status,
        303
    );
    let snapshot = lock(&server.engine.store).unwrap().get(&job.id).unwrap();
    assert!(snapshot.release.is_some());
    let preview = browser.post(&server, "/ui/upgrades", &[("action", "preview")]);
    assert_eq!(preview.status, 200);
    assert!(preview.body.contains("upgrade available"));
    preview.no_secrets();
    assert_eq!(
        lock(&server.engine.store).unwrap().get(&job.id).unwrap(),
        snapshot
    );
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 1);
    let applied = browser.post(&server, "/ui/upgrades", &[("action", "apply")]);
    assert_eq!(applied.status, 200);
    applied.no_secrets();
    assert_eq!(lock(&server.engine.store).unwrap().list().len(), 2);
    assert_eq!(fs::read(&job.imports[0]).unwrap(), before);
    assert!(
        browser
            .get(&server, "/ui/library")
            .body
            .contains("View pending upgrade")
    );
}

#[test]
fn sessions_end_on_restart_and_pages_offer_keyboard_navigation_without_scripts() {
    let directory = Directory::new();
    let mut cfg = config::from_json(&configuration(None, "{}"), &directory.0).unwrap();
    let server = Server::open(cfg.clone());
    cfg.listen = server.authority.clone();
    let browser = Browser::login(&server);
    for route in [
        "/ui",
        "/ui/jobs",
        "/ui/library",
        "/ui/search",
        "/ui/transfers",
    ] {
        let response = browser.get(&server, route);
        assert_eq!(response.status, 200);
        assert!(response.body.contains("<html lang=en>"));
        assert!(response.body.contains("Skip to content"));
        assert!(response.body.contains("<main id=main>"));
        assert!(!response.body.contains("<script"));
        assert!(response.body.contains("href=/ui/style.css"));
        response.no_secrets();
    }
    let css = server.call("GET", "/ui/style.css", &[], "");
    assert_eq!(css.status, 200);
    assert!(css.headers["content-type"].starts_with("text/css"));
    assert!(css.body.contains(":focus-visible"));
    assert!(css.body.contains("@media"));
    assert_eq!(browser.get(&server, "/ui/unknown").status, 404);
    assert_eq!(
        server
            .call("DELETE", "/ui/jobs", &[("Cookie", &browser.cookie)], "")
            .status,
        405
    );
    drop(server);
    let restarted = Server::open(cfg);
    assert_eq!(browser.get(&restarted, "/ui/jobs").status, 303);
}
