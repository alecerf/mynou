//! Wanted titles: requests without an acceptable release yet. Loopback
//! indexer, feed, catalog and IRC fixtures only; nothing real is contacted.
use crate::irc_routing_support;
use crate::library_support::{self, Directory, Indexer, release};
use crate::series_support::Catalog;
use crate::transfer_support::{RecordingProxy, Seeder, Torrent};
use mynou::{
    config::{self, Config},
    date,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    store::{self, Job, Request},
    wanted,
};
use std::sync::{Arc, atomic::Ordering};

const HD: &str = r#"{"resolutions":[1080,720],"languages":["en"]}"#;
const RELEASE: &str = "Fixture.Movie.2024.1080p.WEB-DL.EN";
const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;

fn magnet(n: u8) -> String {
    format!("magnet:?xt=urn:btih:{}", format!("{n:02x}").repeat(20))
}

/// Downloads on, and one JSON indexer that its feed watch polls too.
fn configuration(indexer: &Indexer, max_attempts: u32) -> Value {
    let mut value = library_support::configuration(Some(indexer), HD);
    value.insert("max_attempts", max_attempts);
    let Value::Object(downloads) = value.get_mut("downloads").unwrap() else {
        panic!("downloads section")
    };
    downloads.insert("enabled".into(), true.into());
    let Value::Array(sources) = value.get_mut("indexers").unwrap() else {
        panic!("indexers")
    };
    sources[0].insert(
        "watch",
        json::parse(r#"{"enabled":true,"interval_secs":60}"#).unwrap(),
    );
    value
}

fn open(directory: &Directory, value: &Value) -> Arc<Engine> {
    Engine::open(config::from_json(value, &directory.0).unwrap()).unwrap()
}

fn submit(engine: &Engine, request: Request) -> String {
    engine.submit(request).unwrap().remove(0).id
}

fn job(engine: &Engine, id: &str) -> Job {
    lock(&engine.store).unwrap().get(id).unwrap().unwrap()
}

fn calls(indexer: &Indexer) -> usize {
    indexer.calls.load(Ordering::Relaxed)
}

/// Moves the next search of a wanted title, as time passing would.
fn due_at(engine: &Engine, id: &str, at: u64) {
    let mut wanted = job(engine, id);
    assert_eq!(wanted.state, "wanted");
    wanted.next_attempt_at = at;
    lock(&engine.store).unwrap().update(wanted).unwrap();
}

fn routed(report: &Value) -> u64 {
    report.get("sources").unwrap().as_array().unwrap()[0]
        .get("routed")
        .and_then(Value::as_u64)
        .unwrap()
}

fn assert_wanted(job: &Job, error: &str) {
    assert_eq!(
        (job.state.as_str(), job.attempts, job.last_error.as_deref()),
        ("wanted", 0, Some(error)),
        "{job:?}"
    );
    assert!(job.lease_id.is_none() && job.release.is_none());
}

#[test]
fn a_title_without_a_release_stays_wanted_across_a_restart_until_a_feed_offers_one() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let value = configuration(&indexer, 1);
    let engine = open(&directory, &value);
    // The first poll only learns what the feed holds already.
    assert_eq!(routed(&engine.feeds_poll(None, true).unwrap()), 0);
    let id = submit(&engine, library_support::movie("Fixture Movie"));
    let before = store::now();
    assert!(engine.tick().unwrap());
    let wanted = job(&engine, &id);
    assert_wanted(&wanted, integrations::NO_RELEASE);
    assert!(wanted.searched_at >= before);
    // The next search comes after the interval, six hours by default.
    assert!(wanted.next_attempt_at >= before + 6 * HOUR);
    assert!(wanted.next_attempt_at <= store::now() + 6 * HOUR);
    let searched = calls(&indexer);
    // No worker takes it again, and the wanted search waits for its time.
    assert!(!engine.tick().unwrap());
    assert!(engine.search_wanted().unwrap().is_none());
    drop(engine);

    let engine = open(&directory, &value);
    assert_eq!(job(&engine, &id), wanted);
    assert!(!engine.tick().unwrap());
    assert!(engine.search_wanted().unwrap().is_none());
    assert_eq!(calls(&indexer), searched);

    // Days later, the feed carries a release: it is taken at once.
    indexer.replace(Value::Array(vec![release(RELEASE, 5, &magnet(1))]));
    assert_eq!(routed(&engine.feeds_poll(None, true).unwrap()), 1);
    let fulfilled = job(&engine, &id);
    assert_eq!(fulfilled.state, "queued");
    assert_eq!(fulfilled.acquisition_url, Some(magnet(1)));
    assert_eq!(
        fulfilled.release.as_ref().map(|r| r.title.as_str()),
        Some(RELEASE)
    );
    assert_eq!(fulfilled.attempts, 0);
}

#[test]
fn an_irc_announcement_fulfils_a_wanted_title_after_a_restart() {
    let dir = Directory::new();
    let torrent = Torrent::single(
        &dir.0.join("source"),
        "Fixture.Movie.2024.1080p.mp4",
        include_bytes!("fixtures/demo.mp4").to_vec(),
    );
    let seed = Seeder::open(&dir.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let mut cfg = irc_routing_support::configuration(&dir.0.join("engine"), proxy.port);
    // A review rule leaves the request to the indexers, and there are none.
    cfg.irc.rules[0].action = "review".into();
    let engine = Engine::open(cfg.clone()).unwrap();
    let id = engine
        .submit(irc_routing_support::request(7))
        .unwrap()
        .remove(0)
        .id;
    assert!(engine.tick().unwrap());
    let wanted = job(&engine, &id);
    assert_wanted(&wanted, integrations::NO_INDEXER);
    // A search that could not ask anyone is tried again within the hour.
    assert!(wanted.next_attempt_at <= store::now() + HOUR);
    drop(engine);

    cfg.irc.rules[0].action = "grab".into();
    let engine = Engine::open(cfg).unwrap();
    assert_eq!(job(&engine, &id), wanted);
    assert!(!engine.tick().unwrap());
    irc_routing_support::receive(&engine, &torrent.id, 7);
    let report = engine.irc_route_pending().unwrap();
    assert_eq!(
        report.get("routed").and_then(Value::as_bool),
        Some(true),
        "{report:?}"
    );
    let fulfilled = job(&engine, &id);
    assert_eq!(fulfilled.state, "queued");
    assert_eq!(fulfilled.irc_origin.unwrap().torrent_id, torrent.id);
    assert!(fulfilled.acquisition_url.is_some() && fulfilled.last_error.is_none());
}

#[test]
fn the_wanted_search_takes_due_titles_one_at_a_time_the_longest_waiting_first() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let value = configuration(&indexer, 1);
    let engine = open(&directory, &value);
    let ids: Vec<String> = ["First Movie", "Second Movie", "Third Movie"]
        .into_iter()
        .map(|title| submit(&engine, library_support::movie(title)))
        .collect();
    for _ in &ids {
        assert!(engine.tick().unwrap());
    }
    for id in &ids {
        assert_wanted(&job(&engine, id), integrations::NO_RELEASE);
    }
    let now = store::now();
    due_at(&engine, &ids[0], now - 100);
    due_at(&engine, &ids[1], now - 200);
    let later = job(&engine, &ids[2]);

    // One title per turn, the one due first first; the third is not due.
    let first = engine.search_wanted().unwrap().unwrap();
    assert_eq!(
        (first.id.as_str(), first.state.as_str()),
        (ids[1].as_str(), "queued")
    );
    assert_eq!(job(&engine, &ids[0]).state, "wanted");
    assert_eq!(engine.search_wanted().unwrap().unwrap().id, ids[0]);
    assert!(engine.search_wanted().unwrap().is_none());

    // Workers search them like any request, and they are wanted again.
    let searched = calls(&indexer);
    assert!(engine.tick().unwrap());
    assert!(engine.tick().unwrap());
    assert!(!engine.tick().unwrap());
    assert_eq!(calls(&indexer), searched + 2);
    for id in &ids[..2] {
        let again = job(&engine, id);
        assert_wanted(&again, integrations::NO_RELEASE);
        assert!(again.next_attempt_at >= now + 6 * HOUR);
    }
    assert_eq!(job(&engine, &ids[2]), later);

    // An operator's retry searches a wanted title now.
    assert_eq!(engine.retry(&ids[2]).unwrap().state, "queued");
    assert!(engine.tick().unwrap());
    assert_eq!(calls(&indexer), searched + 3);
    assert_wanted(&job(&engine, &ids[2]), integrations::NO_RELEASE);

    // Nothing is searched while downloads are off.
    drop(engine);
    due_at(&open(&directory, &value), &ids[2], now - 300);
    let mut off = value.clone();
    off.get_mut("downloads").unwrap().insert("enabled", false);
    let engine = open(&directory, &off);
    assert!(engine.search_wanted().unwrap().is_none());
    assert_eq!(job(&engine, &ids[2]).state, "wanted");
}

#[test]
fn a_search_the_indexer_interval_refuses_leaves_the_title_wanted_for_the_hour() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let mut value = configuration(&indexer, 1);
    // At most one request a minute to this indexer.
    let Value::Array(sources) = value.get_mut("indexers").unwrap() else {
        panic!("indexers")
    };
    sources[0].insert("min_interval_ms", 60_000_u32);
    let engine = open(&directory, &value);
    let id = submit(&engine, library_support::movie("Fixture Movie"));
    assert!(engine.tick().unwrap());
    assert_wanted(&job(&engine, &id), integrations::NO_RELEASE);
    let searched = calls(&indexer);

    let now = store::now();
    due_at(&engine, &id, now - 1);
    assert_eq!(engine.search_wanted().unwrap().unwrap().id, id);
    assert!(engine.tick().unwrap());
    // The indexer's own bound holds: no request, no attempt spent.
    assert_eq!(calls(&indexer), searched);
    let refused = job(&engine, &id);
    assert_wanted(&refused, integrations::NO_INDEXER_ANSWER);
    assert!(refused.next_attempt_at > now && refused.next_attempt_at <= store::now() + HOUR);
}

/// TMDB details of film 7 with one release of `kind` on `day`.
fn film_details(day: &str, kind: u32) -> Value {
    json::parse(&format!(
        r#"{{"id":7,"title":"Fixture Movie","release_date":"{day}","runtime":120,"release_dates":{{"results":[{{"iso_3166_1":"US","release_dates":[{{"type":{kind},"release_date":"{day}T00:00:00.000Z"}}]}}]}}}}"#
    ))
    .unwrap()
}

fn with_catalog(config: &mut Config, catalog: &Catalog) {
    config.catalog.enabled = true;
    config.catalog.url = format!("{}/3", catalog.url);
    // As in series_support: a fixture-only Bearer value from the environment.
    config.catalog.token_env = "MYNOU_TEST_SECRET_A".into();
}

#[test]
fn a_film_is_not_searched_before_it_can_be_found() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let catalog = Catalog::open(Vec::new());
    let today = date::day(&date::today()).unwrap();
    let out = date::from_day(today + 30).unwrap();
    catalog.response("/3/movie/7", 200, film_details(&out, 4));
    let mut config = config::from_json(&configuration(&indexer, 1), &directory.0).unwrap();
    with_catalog(&mut config, &catalog);
    let engine = Engine::open(config).unwrap();
    assert_eq!(routed(&engine.feeds_poll(None, true).unwrap()), 0);
    let mut request = library_support::movie("Fixture Movie");
    request.tmdb_id = Some(7);
    let id = submit(&engine, request);

    assert!(engine.tick().unwrap());
    let waiting = job(&engine, &id);
    assert_eq!(
        (
            waiting.state.as_str(),
            waiting.attempts,
            waiting.last_error.as_deref()
        ),
        ("wanted", 0, None)
    );
    let availability = waiting.availability.clone().unwrap();
    assert_eq!(
        (availability.at, availability.basis.as_str()),
        (today as u64 * DAY + 30 * DAY, "digital")
    );
    assert!(wanted::not_yet_available(&waiting, store::now()));
    // Its dates are read again within a week.
    assert!(waiting.next_attempt_at <= store::now() + 7 * DAY);
    assert_eq!(calls(&indexer), 1, "only the feed baseline");

    // Neither a worker, the wanted search nor a feed takes it before its date.
    assert!(!engine.tick().unwrap());
    assert!(engine.search_wanted().unwrap().is_none());
    indexer.replace(Value::Array(vec![release(RELEASE, 5, &magnet(2))]));
    assert_eq!(routed(&engine.feeds_poll(None, true).unwrap()), 0);
    assert_eq!(job(&engine, &id), waiting);

    // Once it is out, a retry reads the dates again and searches.
    let past = date::from_day(today - 1).unwrap();
    catalog.response("/3/movie/7", 200, film_details(&past, 5));
    indexer.replace(Value::Array(Vec::new()));
    let polled = calls(&indexer);
    engine.retry(&id).unwrap();
    assert!(engine.tick().unwrap());
    assert_eq!(calls(&indexer), polled + 1);
    let searched = job(&engine, &id);
    assert_wanted(&searched, integrations::NO_RELEASE);
    assert!(!wanted::not_yet_available(&searched, store::now()));
    assert_eq!(
        searched.availability.unwrap().basis,
        "physical",
        "the dates were read again"
    );
}

#[test]
fn an_error_after_a_release_was_chosen_keeps_the_usual_retries() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(vec![release(
        RELEASE,
        5,
        "http://127.0.0.1:9/fixture.torrent",
    )]));
    let engine = open(&directory, &configuration(&indexer, 3));
    let id = submit(&engine, library_support::movie("Fixture Movie"));
    assert!(engine.tick().unwrap());
    let failed = job(&engine, &id);
    assert_eq!((failed.state.as_str(), failed.attempts), ("failed", 1));
    assert!(failed.next_attempt_at > 0, "{failed:?}");
    let error = failed.last_error.unwrap();
    assert!(!error.starts_with("Search: "), "{error}");
}

#[test]
fn requests_that_failed_for_good_without_a_release_become_wanted_once() {
    let directory = Directory::new();
    let indexer = Indexer::open(Value::Array(Vec::new()));
    let engine = open(&directory, &configuration(&indexer, 1));
    let none = submit(&engine, library_support::movie("First Movie"));
    let broken = submit(&engine, library_support::movie("Second Movie"));
    for (id, error) in [
        (&none, integrations::NO_RELEASE),
        (&broken, "Synthetic download failure"),
    ] {
        let mut failed = job(&engine, id);
        failed.state = "failed".into();
        failed.attempts = 1;
        failed.last_error = Some(error.into());
        failed.next_attempt_at = 0;
        lock(&engine.store).unwrap().update(failed).unwrap();
    }
    assert_eq!(engine.adopt_wanted().unwrap(), 1);
    let adopted = job(&engine, &none);
    assert_eq!(adopted.state, "wanted");
    assert!(adopted.next_attempt_at > 0);
    assert_eq!(job(&engine, &broken).state, "failed");
    // It runs once per store.
    let mut again = job(&engine, &broken);
    again.last_error = Some(integrations::NO_RELEASE.into());
    lock(&engine.store).unwrap().update(again).unwrap();
    assert_eq!(engine.adopt_wanted().unwrap(), 0);
    assert_eq!(job(&engine, &broken).state, "failed");
}
