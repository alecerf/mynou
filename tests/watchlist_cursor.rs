//! A watchlist with no supported entries still advances durably.
mod library_support;
mod requester_support;

use library_support::Directory;
use mynou::{engine::Engine, json::Value};
use requester_support::{Accounts, movie};

#[test]
fn a_single_account_sweep_resumes_before_any_job_was_submitted() {
    let dir = Directory::new();
    let accounts = Accounts::open();
    let mut items: Vec<_> = (0..1_699)
        .map(|id| {
            let mut item = Value::object();
            item.insert("type", "artist");
            item.insert("ratingKey", id.to_string());
            item
        })
        .collect();
    items.push(movie(42, "Synthetic tail movie"));
    accounts.watchlist("alice", items);
    let mut config = accounts.config(&dir.0);
    config.requesters.accounts.clear();
    config.plex.enabled = true;
    config.plex.url = accounts.url.clone();
    config.plex.watchlist_url = format!("{}/alice/watchlist", accounts.url);
    config.plex.token_override = Some(std::env::var("PATH").unwrap());
    let reads = || {
        accounts
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "/alice/watchlist")
            .count()
    };
    let engine = Engine::open_for_management(config.clone()).unwrap();
    assert_eq!(engine.sync().unwrap(), 0);
    assert_eq!(reads(), 8);
    assert_eq!(engine.store.lock().unwrap().counts().unwrap().jobs, 0);
    drop(engine);

    let engine = Engine::open_for_management(config).unwrap();
    assert_eq!(engine.sync().unwrap(), 1);
    assert_eq!(
        reads(),
        9,
        "resume at the saved position, without rereading eight pages"
    );
    assert_eq!(engine.store.lock().unwrap().counts().unwrap().jobs, 1);
    assert_eq!(engine.sync().unwrap(), 0);
    assert_eq!(reads(), 17, "the next sweep starts at the top");
}
