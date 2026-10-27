//! A data directory as `serve` sees it: settings in the job store.
#![allow(dead_code)]
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

/// Saves the settings `serve` reads from `directory`; Mynou keeps them in its
/// job store, not in a file. It also records the explicit choice of the
/// machine's own connection, which `serve` waits for before any torrent work.
pub fn save_settings(directory: &Path, document: &mynou::json::Value) {
    let store = mynou::config::store_dir(directory);
    mynou::store::Store::open(&store)
        .unwrap()
        .save_settings(document)
        .unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(store.join("vpn.conf"))
        .and_then(|mut file| file.write_all(b"# mynou-vpn-v1 disabled\n"))
        .unwrap();
}
