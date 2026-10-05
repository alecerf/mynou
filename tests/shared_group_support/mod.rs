//! Original canonical owners and distinct synthetic shared videos, used only in CI.
#![allow(dead_code)]
use crate::{
    library_support::Directory,
    series_support::{Catalog, episode, id, request},
    transfer_support::{BLOCK, Torrent, payload},
};
use mynou::{
    config::Config,
    engine::{Engine, lock},
    json::Value,
    library::GroupRequest,
    pack::SharedFileRequest,
    store::{Job, Store},
};
use std::{fs, path::Path};

pub struct Fixture {
    pub catalog: Catalog,
    pub cfg: Config,
    pub original: Torrent,
    pub replacement: Torrent,
    pub count: u32,
}
impl Fixture {
    pub fn new(directory: &Directory, count: u32) -> Self {
        let catalog = Catalog::open(
            (1..=count)
                .map(|number| episode(1, number, Some("2024-01-01"), "Fixture episode"))
                .collect(),
        );
        let mut cfg = catalog.config(&directory.0.join("engine"));
        let profile = cfg.selection.episode_profile.clone();
        cfg.selection
            .profiles
            .get_mut(&profile)
            .unwrap()
            .resolutions = vec![2160, 1080, 720, 480];
        Self {
            original: video(directory, "original", 0),
            replacement: video(directory, "replacement", 1),
            catalog,
            cfg,
            count,
        }
    }
    pub fn parents(&self, engine: &Engine) -> Vec<Job> {
        let series = id(&engine
            .track_series_with_policy(&request(), false, false, false)
            .unwrap())
        .to_owned();
        let mut query = SharedFileRequest {
            source_url: self.original.path.to_str().unwrap().into(),
            file_path: "Pack/shared.mp4".into(),
            season: 1,
            episodes: (1..=self.count).collect(),
            apply: false,
            plan_id: None,
        };
        let preview = engine.shared_file(&series, &query).unwrap();
        query.apply = true;
        query.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
        let created = engine.shared_file(&series, &query).unwrap();
        let owners = ids(&created);
        let binding = lock(&engine.store)
            .unwrap()
            .get(&owners[0])
            .unwrap()
            .shared_file
            .unwrap();
        fs::create_dir_all(Path::new(&binding.import_path).parent().unwrap()).unwrap();
        fs::write(&binding.import_path, &self.original.files[0].1).unwrap();
        let mut store = lock(&engine.store).unwrap();
        for owner in &owners {
            let mut job = store.get(owner).unwrap();
            job.state = "ready".into();
            job.progress = 1.0;
            job.imports = vec![binding.import_path.clone()];
            store.update(job).unwrap();
        }
        store.shared_group(&owners[0]).unwrap()
    }
    pub fn replacement_query(&self) -> GroupRequest {
        GroupRequest {
            action: "replace".into(),
            release_title: "Fixture.Series.S01.1080p.WEB-DL".into(),
            source_url: Some(self.replacement.path.to_str().unwrap().into()),
            file_path: Some("Pack/shared.mp4".into()),
            apply: false,
            plan_id: None,
        }
    }
}
pub fn video(directory: &Directory, tag: &str, marker: u8) -> Torrent {
    let mut bytes = include_bytes!("../../examples/demo.mp4").to_vec();
    if marker != 0 {
        bytes.extend_from_slice(&[
            0, 0, 0, 16, b'f', b'r', b'e', b'e', marker, 0, 0, 0, 0, 0, 0, 0,
        ]);
    }
    Torrent::multiple(
        &directory.0.join(tag),
        "Pack",
        vec![
            ("shared.mp4".into(), bytes),
            ("boundary.bin".into(), payload(BLOCK * 2, marker + 17)),
            ("untouched.txt".into(), payload(BLOCK * 8, marker + 31)),
        ],
    )
}
pub fn baseline_query() -> GroupRequest {
    GroupRequest {
        action: "baseline".into(),
        release_title: "Fixture.Series.S01.720p.WEB-DL".into(),
        source_url: None,
        file_path: None,
        apply: false,
        plan_id: None,
    }
}
pub fn guarded(engine: &Engine, owner: &str, query: &GroupRequest) -> GroupRequest {
    let preview = engine.library_group(owner, query).unwrap();
    let mut apply = query.clone();
    apply.apply = true;
    apply.plan_id = Some(preview.get("plan_id").unwrap().as_str().unwrap().into());
    apply
}
pub fn apply(engine: &Engine, owner: &str, query: &GroupRequest) -> Value {
    engine
        .library_group(owner, &guarded(engine, owner, query))
        .unwrap()
}
pub fn baseline(engine: &Engine, owner: &str) {
    apply(engine, owner, &baseline_query());
}
pub fn ids(report: &Value) -> Vec<String> {
    report
        .get("jobs")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|job| id(job).into())
        .collect()
}
pub fn confirm(store: &mut Store, owner: &str, bytes: &[u8]) {
    let mut job = store.get(owner).unwrap();
    let destination = job.shared_file.as_ref().unwrap().import_path.clone();
    if !Path::new(&destination).exists() {
        fs::create_dir_all(Path::new(&destination).parent().unwrap()).unwrap();
        fs::write(&destination, bytes).unwrap();
    }
    job.state = "ready".into();
    job.progress = 1.0;
    job.imports = vec![destination];
    job.next_attempt_at = 0;
    job.last_error = None;
    store.update(job).unwrap();
}
pub fn leaves(paths: &[&str]) -> Value {
    let entries = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let mut entry = Value::object();
            entry.insert("parentIndex", 1_u32);
            entry.insert("index", index as u32 + 1);
            let mut part = Value::object();
            part.insert("file", (*path).to_owned());
            let mut media = Value::object();
            media.insert("Part", Value::Array(vec![part]));
            entry.insert("Media", Value::Array(vec![media]));
            entry
        })
        .collect();
    let mut container = Value::object();
    container.insert("Metadata", Value::Array(entries));
    let mut response = Value::object();
    response.insert("MediaContainer", container);
    response
}
