//! A data directory as commands see it: settings in the job store and the
//! address a running service records.
#![allow(dead_code)]
use std::path::Path;

/// Saves the settings that commands read from `directory` while no service
/// runs there; Mynou keeps them in its job store, not in a file.
pub fn save_settings(directory: &Path, document: &mynou::json::Value) {
    mynou::store::Store::open(&mynou::config::store_dir(directory))
        .unwrap()
        .save_settings(document)
        .unwrap();
}

/// Records a running API where commands for `directory` find it, as `serve` does.
pub fn serve_here(directory: &Path, address: &str) {
    std::fs::create_dir_all(directory.join("state")).unwrap();
    std::fs::write(directory.join("state/endpoint"), address).unwrap();
}
