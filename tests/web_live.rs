//! Real native listener/session/projection fixtures; run by CI only.
#[allow(dead_code)]
mod library_support;
#[allow(dead_code)]
mod transfer_support;
mod web_support;

use library_support::{Directory, configuration, files, movie};
use mynou::{config, crypto::sha256, engine::lock, json};
use std::fs;
use transfer_support::{BLOCK, Scratch, Torrent, engine_config, payload};
use web_support::{Browser, Reply, Server, TOKEN};

fn server(directory: &Directory) -> Server {
    Server::open(config::from_json(&configuration(None, "{}"), &directory.0).unwrap())
}

fn live(server: &Server, browser: &Browser, route: &str) -> Reply {
    server.call(
        "GET",
        route,
        &[("Cookie", &browser.cookie), ("Origin", &server.origin())],
        "",
    )
}

fn assert_json(reply: &Reply, status: u16) {
    assert_eq!(reply.status, status);
    assert_eq!(reply.headers["cache-control"], "no-store");
    assert_eq!(reply.headers["x-content-type-options"], "nosniff");
    assert_eq!(reply.headers["x-frame-options"], "DENY");
    assert!(reply.headers["content-type"].starts_with("application/json"));
    assert!(!reply.headers.contains_key("location"));
    assert!(!reply.headers.contains_key("set-cookie"));
    assert!(reply.body.len() <= 48 * 1024);
    json::parse(&reply.body).unwrap();
    reply.no_secrets();
}

#[test]
fn live_routes_require_a_signed_in_exact_origin_browser_session() {
    let directory = Directory::new();
    let server = server(&directory);
    let route = format!("/ui/live/jobs?ids={}", "a".repeat(32));
    assert_json(&server.call("GET", &route, &[], ""), 401);
    assert_json(
        &server.call("GET", &route, &[("Authorization", &format!("Bearer {TOKEN}"))], ""),
        401,
    );
    let challenge = Browser::challenge(&server);
    assert_json(&live(&server, &challenge, &route), 401);
    let browser = Browser::login(&server);
    assert_json(
        &server.call("GET", &route, &[("Cookie", &browser.cookie)], ""),
        403,
    );
    for headers in [
        vec![("Cookie", browser.cookie.clone()), ("Origin", "https://foreign.invalid".into())],
        vec![("Cookie", browser.cookie.clone()), ("Origin", format!("https://{}", server.authority))],
        vec![("Cookie", browser.cookie.clone()), ("Origin", server.origin()), ("Sec-Fetch-Site", "cross-site".into())],
    ] {
        let headers: Vec<_> = headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
        assert_json(&server.call("GET", &route, &headers, ""), 403);
    }
    let referer = format!("{}/ui/jobs?q=ignored", server.origin());
    assert_json(
        &server.call("GET", &route, &[("Cookie", &browser.cookie), ("Referer", &referer)], ""),
        200,
    );
    assert_json(&live(&server, &browser, &route), 200);
    assert_eq!(
        server.call("GET", "/api/jobs", &[("Cookie", &browser.cookie)], "").status,
        401,
    );
    browser.post(&server, "/ui/logout", &[]);
    assert_json(&live(&server, &browser, &route), 401);
}

#[test]
fn live_queries_bound_identifiers_and_reject_ambiguity_without_mutations() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let before = files(&directory.0);
    let id = "a".repeat(32);
    for query in [
        String::new(),
        "ids=".into(),
        "ids=not-an-id".into(),
        format!("ids={id},{id}"),
        format!("ids={id},{}", id.to_ascii_uppercase()),
        format!("ids={id}&ids={id}"),
        format!("ids={id}&q=ignored"),
        format!("ids={id}&id={id}"),
        format!("ids={id}%00"),
        format!("ids={id},"),
        format!("ids={}", (0..51).map(|value| format!("{value:032x}")).collect::<Vec<_>>().join(",")),
        format!("ids={}", "a".repeat(4096)),
    ] {
        assert_json(&live(&server, &browser, &format!("/ui/live/jobs?{query}")), 400);
    }
    let ids = (0..50).map(|value| format!("{value:032x}")).collect::<Vec<_>>().join(",");
    let reply = live(&server, &browser, &format!("/ui/live/jobs?ids={ids}"));
    assert_json(&reply, 200);
    assert_eq!(json::parse(&reply.body).unwrap().get("entries").unwrap().as_array().unwrap().len(), 50);
    assert_json(&live(&server, &browser, "/ui/live/unknown?ids=anything"), 404);
    for method in ["POST", "PUT", "DELETE"] {
        assert_json(&server.call(method, &format!("/ui/live/jobs?ids={id}"), &[("Cookie", &browser.cookie)], ""), 405);
    }
    assert_eq!(files(&directory.0), before);
}

#[test]
fn job_projection_is_narrow_fresh_read_only_and_does_not_consume_flash_messages() {
    let directory = Directory::new();
    let server = server(&directory);
    let job = server.engine.submit(movie("Private title must stay private")).unwrap().remove(0);
    let mut changed = job.clone();
    changed.state = "downloading".into();
    changed.progress = 0.375;
    changed.attempts = 3;
    changed.last_error = Some("Private diagnostic must stay private".into());
    changed.acquisition_url = Some("https://private.invalid/file?token=synthetic-private-value".into());
    lock(&server.engine.store).unwrap().update(changed.clone()).unwrap();
    let browser = Browser::login(&server);
    let route = format!("/ui/live/jobs?ids={},{}", job.id.to_ascii_uppercase(), "f".repeat(32));
    let before = files(&directory.0);
    let reply = live(&server, &browser, &route);
    assert_json(&reply, 200);
    let value = json::parse(&reply.body).unwrap();
    let entries = value.get("entries").unwrap().as_array().unwrap();
    let entry = &entries[0];
    assert_eq!(entry.get("id").unwrap().as_str(), Some(job.id.as_str()));
    assert_eq!(entry.get("state").unwrap().as_str(), Some("downloading"));
    assert_eq!(entry.get("progress").unwrap().as_f64(), Some(0.375));
    assert_eq!(entry.get("attempts").unwrap().as_u64(), Some(3));
    assert_eq!(entry.as_object().unwrap().len(), 4);
    assert_eq!(entries[1].as_object().unwrap().len(), 2);
    assert_eq!(entries[1].get("missing").unwrap().as_bool(), Some(true));
    for forbidden in ["Private title", "Private diagnostic", "private.invalid", "synthetic-private-value", "files", "request", "csrf"] {
        assert!(!reply.body.contains(forbidden));
    }
    assert_eq!(files(&directory.0), before);
    changed.progress = 0.625;
    lock(&server.engine.store).unwrap().update(changed).unwrap();
    let newer = live(&server, &browser, &route);
    assert_eq!(json::parse(&newer.body).unwrap().get("entries").unwrap().as_array().unwrap()[0].get("progress").unwrap().as_f64(), Some(0.625));
    assert_eq!(browser.post(&server, "/ui/jobs/action", &[("id", &job.id), ("action", "cancel")]).status, 303);
    assert_json(&live(&server, &browser, &route), 200);
    let page = browser.get(&server, "/ui/jobs");
    assert!(page.body.contains("aria-label=\"Action results\""));
    assert!(!browser.get(&server, "/ui/jobs").body.contains("aria-label=\"Action results\""));
}

#[test]
fn transfer_projection_uses_exact_decimal_counters_and_retains_native_controls() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0, "private-name.bin", payload(BLOCK, 94));
    let metadata = fs::read(&torrent.path).unwrap();
    let server = Server::open(engine_config(&scratch.0.join("engine")));
    let mut request = movie("Private transfer title");
    request.kind = "file".into();
    request.source_url = Some(torrent.path.to_str().unwrap().into());
    server.engine.submit(request).unwrap();
    assert!(server.engine.tick().unwrap());
    let browser = Browser::login(&server);
    let before = files(&scratch.0);
    let snapshot = server.engine.transfer(&torrent.id).unwrap();
    let reply = live(&server, &browser, &format!("/ui/live/transfers?ids={}", torrent.id));
    assert_json(&reply, 200);
    let value = json::parse(&reply.body).unwrap();
    let entry = &value.get("entries").unwrap().as_array().unwrap()[0];
    assert_eq!(entry.as_object().unwrap().len(), 7);
    for key in ["downloaded_bytes", "uploaded_bytes", "verified_bytes", "seed_elapsed_secs"] {
        assert_eq!(entry.get(key).unwrap().as_str(), snapshot.get(key).unwrap().as_str());
    }
    assert!(!reply.body.contains("private-name"));
    assert!(!reply.body.contains("Private transfer"));
    assert_eq!(files(&scratch.0), before);
    let page = browser.get(&server, &format!("/ui/transfers/{}", torrent.id));
    assert!(page.body.contains("data-live=transfers"));
    assert!(page.body.contains("data-live-field=seed_elapsed_secs"));
    assert!(page.body.contains("name=download_limit_bps"));
    assert!(page.body.contains("action=\"/ui/transfers/policy\""));
    assert!(page.body.contains("action=\"/ui/transfers/selection\""));
    assert_eq!(fs::read(&torrent.path).unwrap(), metadata);
}

fn decode_hash(value: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = Vec::new();
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in value.bytes().take_while(|byte| *byte != b'=') {
        buffer = (buffer << 6) | u32::try_from(alphabet.iter().position(|candidate| *candidate == byte).unwrap()).unwrap();
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            result.push((buffer >> bits) as u8);
        }
    }
    result
}

#[test]
fn live_asset_is_content_pinned_and_only_progress_pages_opt_in() {
    let directory = Directory::new();
    let server = server(&directory);
    let browser = Browser::login(&server);
    let asset = server.call("GET", "/ui/live.js", &[], "");
    assert_eq!(asset.status, 200);
    assert!(asset.headers["content-type"].starts_with("text/javascript"));
    assert_eq!(asset.headers["cache-control"], "no-store");
    assert_eq!(asset.body, include_str!("../src/web/live.js"));
    let page = browser.get(&server, "/ui/jobs?q=keep-filter");
    let hash = page.body.split("integrity=\"sha256-").nth(1).unwrap().split('"').next().unwrap();
    assert_eq!(decode_hash(hash), sha256(asset.body.as_bytes()).to_vec());
    let policy = &page.headers["content-security-policy"];
    assert!(policy.contains(&format!("script-src 'sha256-{hash}'")));
    assert!(policy.contains("script-src-attr 'none'"));
    assert!(policy.contains("connect-src 'self'"));
    assert!(!policy.contains("unsafe-inline"));
    assert!(!policy.contains("unsafe-eval"));
    assert!(page.body.contains("data-live-toggle aria-pressed=false disabled"));
    assert!(page.body.contains("href=\"\" data-live-refresh"));
    assert!(page.body.contains("name=q"));
    assert!(page.body.contains("value=\"keep-filter\""));
    for route in ["/ui", "/ui/setup", "/ui/search", "/ui/library", "/ui/series"] {
        assert!(!browser.get(&server, route).body.contains("<script"));
    }
}
