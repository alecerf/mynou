//! A data directory as `serve` sees it: settings in the job store.
#![allow(dead_code)]
use std::path::Path;

/// Saves the settings `serve` reads from `directory`; Mynou keeps them in its
/// job store, not in a file.
pub fn save_settings(directory: &Path, document: &mynou::json::Value) {
    mynou::store::Store::open(&mynou::config::store_dir(directory))
        .unwrap()
        .save_settings(document)
        .unwrap();
}
