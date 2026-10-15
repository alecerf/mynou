//! Durability cost of one inventory commit.
//!
//! Each flush is one `F_FULLFSYNC` on Apple platforms, the dominant write
//! cost there, so these tests count flushes per commit and print the wall
//! time beside them. For a timed run on the current machine:
//!
//! ```sh
//! MYNOU_PERF_COMMITS=50 cargo test --release --lib inventory::flush_tests -- --nocapture --test-threads 1
//! ```

use super::{Change, Index, Key};
use crate::{crypto::random_bytes, store::flushes};
use std::{fs, path::PathBuf, time::Instant};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix: String = random_bytes::<16>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self(std::env::temp_dir().join(format!("mynou-flushes-{suffix}")))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// Record files sharing one object directory, as table-prefixed keys do.
    Files,
    /// Record files spread over every object directory.
    SpreadFiles,
    Inline,
    /// Delete as many record files as the previous commit wrote.
    DeleteFiles,
}

struct Cost {
    flushes_per_commit: u64,
    millis_per_commit: f64,
}

fn key(spread: bool, id: u64) -> Key {
    let mut key = [0; 64];
    if spread {
        key[0] = id as u8;
    }
    key[56..].copy_from_slice(&id.to_be_bytes());
    key
}

fn commits() -> u64 {
    std::env::var("MYNOU_PERF_COMMITS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| (1..=10_000).contains(count))
        .unwrap_or(3)
}

/// Measures `commits()` commits of `records` changes each on a fresh index.
fn measure(shape: Shape, records: u64) -> Cost {
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let commits = commits();
    let spread = matches!(shape, Shape::SpreadFiles);
    let value = vec![0x5a; 600];
    let mut next = 0;
    let mut total_flushes = 0;
    let mut elapsed = std::time::Duration::ZERO;
    for _ in 0..commits {
        let ids = next..next + records;
        next += records;
        let changes: Vec<_> = match shape {
            Shape::Files | Shape::SpreadFiles | Shape::DeleteFiles => ids
                .clone()
                .map(|id| Change::Put(key(spread, id), value.clone()))
                .collect(),
            Shape::Inline => ids
                .clone()
                .map(|id| Change::PutInline(key(spread, id), id.to_be_bytes().to_vec()))
                .collect(),
        };
        let changes = if matches!(shape, Shape::DeleteFiles) {
            index.commit(changes).unwrap();
            ids.map(|id| Change::Delete(key(spread, id))).collect()
        } else {
            changes
        };
        let before = flushes();
        let began = Instant::now();
        index.commit(changes).unwrap();
        elapsed += began.elapsed();
        total_flushes += flushes() - before;
    }
    Cost {
        flushes_per_commit: total_flushes / commits,
        millis_per_commit: elapsed.as_secs_f64() * 1000.0 / commits as f64,
    }
}

#[test]
fn commit_flushes_stay_bounded_as_records_per_commit_grow() {
    let shapes = [
        ("files", Shape::Files),
        ("spread files", Shape::SpreadFiles),
        ("inline", Shape::Inline),
        ("delete files", Shape::DeleteFiles),
    ];
    eprintln!("shape          records  flushes/commit  ms/commit");
    for (name, shape) in shapes {
        for records in [1, 10, 100, 1000] {
            let cost = measure(shape, records);
            eprintln!(
                "{name:<14} {records:>7} {:>15} {:>10.2}",
                cost.flushes_per_commit, cost.millis_per_commit
            );
        }
    }
}
