//! Guided setup uses real authenticated HTTP; fixtures execute only in CI.
mod library_support;
mod web_support;

use library_support::{Directory, Indexer, configuration, files};
use mynou::{config, indexers::Authentication, json::Value};
use std::sync::atomic::Ordering;
use web_support::{Browser, Server, TOKEN};

fn state(page: &str, id: &str, expected: &str) {
    assert!(
        page.contains(&format!("data-setup=\"{id}\" data-status=\"{expected}\"")),
        "Unexpected setup state for {id}"
    );
}

#[test]
fn setup_requires_a_browser_session_and_preserves_security_headers() {
    let directory = Directory::new();
    let cfg = config::from_json(&configuration(None, "{}"), &directory.0).unwrap();
    let server = Server::open(cfg);
    let bearer = format!("Bearer {TOKEN}");
    for headers in [vec![], vec![("Authorization", bearer.as_str())]] {
        let page = server.call("GET", "/ui/setup", &headers, "");
        assert_eq!(page.status, 303);
        assert_eq!(page.headers["location"], "/ui/login");
        page.no_secrets();
    }
    let browser = Browser::login(&server);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    page.no_secrets();
    assert_eq!(page.headers["cache-control"], "no-store");
    assert_eq!(page.headers["referrer-policy"], "same-origin");
    assert_eq!(page.headers["x-frame-options"], "DENY");
    let csp = &page.headers["content-security-policy"];
    assert!(csp.contains("default-src 'none'"));
    assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
    assert!(page.body.contains("<h1>Set up Mynou</h1>"));
    assert!(page.body.contains("href=/ui/setup aria-current=page"));
    assert!(page.body.contains("href=#main"));
    assert_eq!(page.body.matches("<h1>").count(), 1);
    assert!(page.body.contains("Preview a request"));
    let overview = browser.get(&server, "/ui");
    assert!(overview.body.contains("Check setup"));
    let logout = browser.post(&server, "/ui/logout", &[]);
    assert_eq!(logout.status, 303);
    assert_eq!(
        browser.get(&server, "/ui/setup").headers["location"],
        "/ui/login"
    );
}

#[test]
fn empty_installation_gives_missing_and_optional_states_without_writes() {
    let directory = Directory::new();
    let cfg = config::from_json(&configuration(None, "{}"), &directory.0).unwrap();
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    state(&page.body, "folders", "configured");
    state(&page.body, "sources", "missing");
    state(&page.body, "downloads", "missing");
    state(&page.body, "catalog", "optional");
    state(&page.body, "plex", "optional");
    assert!(page.body.contains("Both are currently disabled"));
    assert!(
        page.body
            .contains("Connections, credentials and folder access have not been tested")
    );
    assert_eq!(files(&directory.0), before);
    assert!(server.engine.store.lock().unwrap().list().is_empty());
    page.no_secrets();
    assert!(!page.body.contains(directory.0.to_str().unwrap()));
}

#[test]
fn configured_source_without_matching_route_does_not_claim_connection_success() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let cfg = config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap();
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    for _ in 0..2 {
        let page = browser.get(&server, "/ui/setup");
        assert_eq!(page.status, 200);
        state(&page.body, "sources", "configured");
        state(&page.body, "downloads", "missing");
        assert!(page.body.contains("Review sources and diagnostics"));
        assert!(page.body.contains("href=\"/ui/indexers\""));
        assert!(!page.body.contains(&indexer.url));
        page.no_secrets();
    }
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
    assert!(server.engine.store.lock().unwrap().list().is_empty());
}

#[test]
fn malformed_optional_settings_and_missing_authentication_stay_private() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let mut cfg = config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap();
    cfg.sources[0].name = "setup-private-alias-3274".into();
    cfg.sources[0].options.authentication = Authentication::Bearer {
        token_env: "MYNOU_SETUP_ABSENT_CREDENTIAL_3274".into(),
    };
    cfg.catalog.enabled = true;
    cfg.catalog.url = "not-a-url-setup-private-address".into();
    cfg.plex.enabled = true;
    cfg.plex.url = format!("{}/plex", indexer.url);
    cfg.plex.watchlist_url = format!("{}/watchlist", indexer.url);
    cfg.plex.token_override = Some("fixture\nprivate-header".into());
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    state(&page.body, "sources", "missing");
    state(&page.body, "catalog", "attention");
    state(&page.body, "plex", "attention");
    for forbidden in [
        "setup-private-alias-3274",
        "MYNOU_SETUP_ABSENT_CREDENTIAL_3274",
        "not-a-url-setup-private-address",
        "fixture\nprivate-header",
        indexer.url.as_str(),
        directory.0.to_str().unwrap(),
    ] {
        assert!(
            !page.body.contains(forbidden),
            "Private setup input was rendered"
        );
    }
    page.no_secrets();
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
}

#[test]
fn setup_rejects_query_actions_and_post_actions_without_side_effects() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let cfg = config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap();
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    for query in [
        "?probe=yes",
        "?apply=yes",
        "?token=setup-fixture-query-value",
    ] {
        let page = browser.get(&server, &format!("/ui/setup{query}"));
        assert_eq!(page.status, 400);
        page.no_secrets();
        assert!(!page.body.contains("setup-fixture-query-value"));
    }
    let page = browser.post(&server, "/ui/setup", &[]);
    assert!((400..500).contains(&page.status));
    page.no_secrets();
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
    assert!(server.engine.store.lock().unwrap().list().is_empty());
}

#[test]
fn disabled_sources_offer_review_without_starting_a_probe() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let mut cfg = config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap();
    cfg.sources[0].options.enabled = false;
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    state(&page.body, "sources", "attention");
    assert!(page.body.contains("0 enabled"));
    assert!(page.body.contains("Review sources and diagnostics"));
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
    page.no_secrets();
}

#[test]
fn newznab_source_requires_its_native_usenet_route_even_with_torrents_enabled() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let mut value = configuration(Some(&indexer), "{}");
    let Value::Array(sources) = value.get_mut("indexers").unwrap() else {
        panic!("source fixture");
    };
    sources[0].insert("id", "setup-newznab");
    sources[0].insert("kind", "newznab");
    let mut policy = Value::object();
    policy.insert("server_id", "setup-provider");
    sources[0].insert("usenet", policy);
    value.insert(
        "usenet",
        mynou::json::parse(
            r#"{"servers":[{"id":"setup-provider","host":"127.0.0.1","port":1,"tls":false}]}"#,
        )
        .unwrap(),
    );
    let mut cfg = config::from_json(&value, &directory.0).unwrap();
    cfg.downloads_enabled = true;
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    state(&page.body, "sources", "configured");
    state(&page.body, "downloads", "attention");
    assert!(
        page.body
            .contains("do not match an enabled native download route")
    );
    assert!(page.body.contains("href=\"/ui/usenet\""));
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
    page.no_secrets();
}

#[test]
fn present_plex_and_catalog_settings_stay_unverified_and_redacted() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![]));
    let mut cfg = config::from_json(&configuration(Some(&indexer), "{}"), &directory.0).unwrap();
    cfg.catalog.enabled = true;
    cfg.catalog.url = format!("{}/catalog", indexer.url);
    cfg.catalog.token_env = "PATH".into();
    cfg.catalog.api_key_env = "MYNOU_SETUP_ABSENT_CREDENTIAL_3274".into();
    cfg.plex.enabled = true;
    cfg.plex.url = format!("{}/plex", indexer.url);
    cfg.plex.watchlist_url = format!("{}/watchlist", indexer.url);
    cfg.plex.token_override = Some("setup-credential-value-3274".into());
    let server = Server::open(cfg);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let page = browser.get(&server, "/ui/setup");
    assert_eq!(page.status, 200);
    state(&page.body, "catalog", "configured");
    state(&page.body, "plex", "configured");
    assert!(page.body.contains("No Plex connection is tested here"));
    assert!(!page.body.contains("setup-credential-value-3274"));
    assert!(!page.body.contains(&indexer.url));
    assert!(!page.body.contains(&std::env::var("PATH").unwrap()));
    assert_eq!(indexer.calls.load(Ordering::Acquire), 0);
    assert_eq!(files(&directory.0), before);
    page.no_secrets();
}
