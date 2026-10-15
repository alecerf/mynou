//! CI-only bounded, resumable requester watchlist sweeps on synthetic accounts.
mod library_support;
mod requester_support;
use library_support::Directory;
use mynou::{engine::Engine, json::Value};
use requester_support::*;

fn movies(range: std::ops::Range<u32>) -> Vec<Value> {
    range
        .map(|n| movie(n + 1, &format!("Synthetic Sweep Movie {n}")))
        .collect()
}

fn account<'a>(report: &'a Value, id: &str) -> &'a Value {
    report
        .get("accounts")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a.get("account_id").and_then(Value::as_str) == Some(id))
        .unwrap()
}

fn counter(report: &Value, id: &str, name: &str) -> u64 {
    account(report, id)
        .get("sweep")
        .unwrap()
        .get(name)
        .unwrap()
        .as_u64()
        .unwrap()
}

fn flag(report: &Value, id: &str, name: &str) -> bool {
    account(report, id)
        .get("sweep")
        .unwrap()
        .get(name)
        .unwrap()
        .as_bool()
        .unwrap()
}

/// Every demand of an account, read through continuation cursors.
fn all_demands(engine: &Engine, id: &str) -> Vec<Value> {
    let mut after: Option<String> = None;
    let mut out = Vec::new();
    loop {
        let page = engine.requester_page(id, after.as_deref(), 200).unwrap();
        out.extend(
            page.get("demands")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        match page.get("next").unwrap().as_str() {
            Some(next) => after = Some(next.into()),
            None => return out,
        }
    }
}

fn states(engine: &Engine, id: &str) -> (usize, usize) {
    let demands = all_demands(engine, id);
    let removed = demands
        .iter()
        .filter(|d| d.get("state").and_then(Value::as_str) == Some("removed"))
        .count();
    (demands.len(), removed)
}

fn progress(engine: &Engine, id: &str) -> Value {
    engine
        .requester(id, 0, 1)
        .unwrap()
        .get("sweep")
        .unwrap()
        .clone()
}

fn cursor(engine: &Engine, id: &str) -> String {
    engine
        .requester(id, 0, 1)
        .unwrap()
        .get("cursor")
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}

#[test]
fn large_watchlists_resume_across_passes_and_restarts_without_starving_other_accounts() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    // 1,700 movies exceed the former 512-request account poll limit.
    accounts.watchlist("alice", movies(0..1_700));
    accounts.watchlist("bob", movies(5_000..5_001));
    let cfg = accounts.config(&dir.0);
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    let report = engine.sync_requesters().unwrap();
    no_credentials(&report);
    // One pass reads a bounded number of pages and commits bounded batches.
    assert_eq!(counter(&report, "alice", "pages"), 8);
    assert_eq!(counter(&report, "alice", "requests"), 16);
    assert!(counter(&report, "alice", "rows") <= 8 * 200);
    assert!(!flag(&report, "alice", "published"));
    // The other account was not starved by the large one.
    assert!(flag(&report, "bob", "published"));
    assert_eq!(states(&engine, "bob"), (1, 0));
    let position: u64 = progress(&engine, "alice")
        .get("position")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(position, 200 + 7 * 199);
    assert_eq!(states(&engine, "alice"), (position as usize, 0));
    assert_eq!(cursor(&engine, "alice"), "0");
    drop(engine);

    // A restart resumes from the durable sweep instead of the first page.
    let engine = Engine::open_for_management(cfg).unwrap();
    assert_eq!(
        progress(&engine, "alice").get("position").unwrap().as_str(),
        Some(position.to_string().as_str())
    );
    let report = engine.sync_requesters().unwrap();
    assert_eq!(counter(&report, "alice", "pages"), 1);
    assert!(flag(&report, "alice", "published"));
    assert_eq!(cursor(&engine, "alice"), "1");
    assert_eq!(progress(&engine, "alice"), Value::Null);
    assert_eq!(states(&engine, "alice"), (1_700, 0));
}

fn position(engine: &Engine, id: &str) -> u64 {
    progress(engine, id)
        .get("position")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn removals_wait_for_a_complete_sweep_of_an_unchanged_listing() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    // Two passes per sweep: 8 pages, then the rest.
    accounts.watchlist("alice", movies(0..2_000));
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    assert!(!flag(
        &engine.sync_requesters().unwrap(),
        "alice",
        "published"
    ));
    assert!(flag(
        &engine.sync_requesters().unwrap(),
        "alice",
        "published"
    ));
    assert_eq!(cursor(&engine, "alice"), "1");
    assert_eq!(states(&engine, "alice"), (2_000, 0));

    // Half a sweep later the upstream fails: committed progress stays and the
    // sweep resumes at the same position once the upstream recovers.
    engine.sync_requesters().unwrap();
    let halfway = position(&engine, "alice");
    accounts.response("/alice/watchlist", 503, container(Vec::new()));
    let report = engine.sync_requesters().unwrap();
    assert_eq!(
        account(&report, "alice")
            .get("error")
            .and_then(Value::as_str),
        Some("Plex account poll failed")
    );
    assert_eq!(position(&engine, "alice"), halfway);

    // Item 500 leaves the list behind the cursor: the rest of the listing
    // shifts under the sweep, which therefore cannot prove the removal and
    // starts over with a new generation.
    let mut shorter = movies(0..2_000);
    shorter.remove(500);
    accounts.watchlist("alice", shorter);
    let report = engine.sync_requesters().unwrap();
    assert!(flag(&report, "alice", "restarted"));
    assert!(!flag(&report, "alice", "published"));
    assert_eq!(cursor(&engine, "alice"), "1");
    assert_eq!(states(&engine, "alice"), (2_000, 0));

    // Half of the next sweep proves nothing either.
    assert!(!flag(
        &engine.sync_requesters().unwrap(),
        "alice",
        "published"
    ));
    assert_eq!(states(&engine, "alice"), (2_000, 0));

    // A complete sweep of the unchanged list publishes and removes it.
    let report = engine.sync_requesters().unwrap();
    assert!(flag(&report, "alice", "published"));
    assert_eq!(states(&engine, "alice"), (2_000, 1));
    let removed = all_demands(&engine, "alice")
        .into_iter()
        .find(|d| d.get("state").and_then(Value::as_str) == Some("removed"))
        .unwrap();
    assert_eq!(
        removed
            .get("request")
            .unwrap()
            .get("title")
            .unwrap()
            .as_str(),
        Some("Synthetic Sweep Movie 500")
    );
}

#[test]
fn an_entry_that_cannot_be_resolved_keeps_the_entries_before_it() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    // The catalog is disabled, so the series entry cannot be resolved.
    let mut items = movies(0..2);
    items.push(show());
    items.extend(movies(2..3));
    accounts.watchlist("alice", items);
    let engine = Engine::open_for_management(accounts.config(&dir.0)).unwrap();
    // The second pass resumes at the failing entry: nothing is read twice.
    for rows in [2, 0] {
        let report = engine.sync_requesters().unwrap();
        assert_eq!(
            account(&report, "alice")
                .get("error")
                .and_then(Value::as_str),
            Some("Plex account poll failed")
        );
        assert_eq!(counter(&report, "alice", "rows"), rows);
        assert_eq!(position(&engine, "alice"), 2);
        assert_eq!(states(&engine, "alice"), (2, 0));
    }
}

#[test]
fn the_single_account_watchlist_resumes_its_sweep_across_passes_and_restarts() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    accounts.watchlist("alice", movies(0..1_700));
    let mut cfg = accounts.config(&dir.0);
    cfg.requesters.accounts.clear();
    cfg.plex.enabled = true;
    cfg.plex.url = accounts.url.clone();
    cfg.plex.watchlist_url = format!("{}/alice/watchlist", accounts.url);
    cfg.plex.token_override = Some(std::env::var("PATH").unwrap());
    let reads = || {
        accounts
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "/alice/watchlist")
            .count()
    };
    let engine = Engine::open_for_management(cfg.clone()).unwrap();
    assert_eq!(engine.sync().unwrap(), 200 + 7 * 199);
    assert_eq!(reads(), 8);
    drop(engine);
    let engine = Engine::open_for_management(cfg).unwrap();
    assert_eq!(engine.sync().unwrap(), 1_700 - (200 + 7 * 199));
    assert_eq!(reads(), 9);
    // The next sweep starts from the top; submissions are idempotent.
    assert_eq!(engine.sync().unwrap(), 0);
    assert_eq!(reads(), 17);
}
