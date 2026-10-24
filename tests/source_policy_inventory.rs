//! Large retained policy fixture is a separate harness so it does not extend
//! the existing library harness's finite growing-inventory test budget.
mod library_support;
use library_support::Directory;
use mynou::{
    config,
    crypto::sha256,
    engine::Engine,
    indexers::ControlRequest,
    inventory::{Change, Index, Key},
    json::{self, Value},
};

fn key(table: u8, id: Option<&str>) -> Key {
    let mut key = [0; 64];
    key[0] = table;
    if let Some(id) = id {
        key[1..33].copy_from_slice(&sha256(id.as_bytes()));
    }
    key
}
fn config(directory: &Directory, id: &str) -> config::Config {
    let mut value = config::default_json();
    value.get_mut("downloads").unwrap().insert("enabled", false);
    let mut source = Value::object();
    source.insert("id", id);
    source.insert("name", "synthetic-retained-policy");
    source.insert("kind", "json");
    source.insert("url", "http://127.0.0.1:9/feed");
    value.insert("indexers", Value::Array(vec![source]));
    config::from_json(&value, &directory.0).unwrap()
}

#[test]
fn retired_source_growth_above_both_old_limits_preserves_late_control_and_restart() {
    let directory = Directory::new();
    let first = config(&directory, "first");
    drop(Engine::open_for_management(first.clone()).unwrap());
    // Seed valid retired policies through the public index API. Capture the
    // actual persisted binding; this fixture neither copies its algorithm nor
    // reaches private SourceStore internals.
    let mut index = Index::open(&first.store_dir.join("indexers.idx"), false).unwrap();
    let first_record = index.get(&key(0x10, Some("first"))).unwrap().unwrap();
    let template = json::parse(std::str::from_utf8(&first_record).unwrap()).unwrap();
    let mut bytes = first_record.len();
    for start in (1..=10_001_u64).step_by(200) {
        let mut changes = Vec::new();
        let end = (start + 200).min(10_002);
        for n in start..end {
            let id = format!("source-{n:057x}");
            let mut row = template.clone();
            row.insert("id", id.clone());
            row.insert("at", u64::MAX.to_string());
            let encoded = json::stringify(&row).into_bytes();
            bytes += encoded.len();
            changes.push(Change::Put(key(0x10, Some(&id)), encoded));
        }
        changes.push(Change::PutInline(key(1, None), end.to_le_bytes().to_vec()));
        index.commit(changes).unwrap();
    }
    assert!(bytes > 2 * 1024 * 1024);
    assert_eq!(index.len().unwrap(), 10_002 + 3);
    drop(index);
    let id = format!("source-{:057x}", 10_001);
    let late = config(&directory, &id);
    let engine = Engine::open_for_management(late.clone()).unwrap();
    let mut pause = ControlRequest {
        action: "pause".into(),
        apply: false,
        plan_id: None,
    };
    let preview = engine.indexer_control(&id, &pause).unwrap();
    pause.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    pause.apply = true;
    assert_eq!(
        engine.indexer_control(&id, &pause).unwrap().get("enabled"),
        Some(&Value::Bool(false))
    );
    assert!(engine.indexer_control(&id, &pause).is_err());
    drop(engine);
    let readonly = Engine::open_for_preview(config(&directory, &id)).unwrap();
    let report = readonly.indexers().unwrap();
    let row = &report.get("sources").unwrap().as_array().unwrap()[0];
    assert_eq!(row.get("enabled"), Some(&Value::Bool(false)));
    assert_eq!(row.get("policy_revision").unwrap().as_str(), Some("2"));
    drop(readonly);
    let mut index = Index::open(&late.store_dir.join("indexers.idx"), true).unwrap();
    assert_eq!(index.len().unwrap(), 10_002 + 3);
    assert_eq!(
        index.get(&key(0x10, Some("first"))).unwrap().unwrap(),
        first_record
    );
}
