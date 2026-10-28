//! Durability cost of one inventory commit.
//!
//! Each flush is one `F_FULLFSYNC` on Apple platforms, the dominant write
//! cost there, so these tests count flushes per commit and print the wall
//! time beside them. A commit flushes only its redo record; checkpoints,
//! amortized over many commits, flush the files the records describe. For a
//! timed run on the current machine:
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

#[derive(Clone, Copy, Debug)]
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
    flushes: Vec<u64>,
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
    let mut measured_flushes = Vec::new();
    let mut elapsed = std::time::Duration::ZERO;
    for commit in 0..commits {
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
        let count = flushes() - before;
        // Record files written before the value log flush themselves;
        // averaging must never conceal a commit that exceeds its own bound.
        let limit = 1;
        assert!(
            count <= limit,
            "{shape:?}, {records} records, commit {commit}: {count} flushes exceed {limit}"
        );
        measured_flushes.push(count);
    }
    Cost {
        flushes: measured_flushes,
        millis_per_commit: elapsed.as_secs_f64() * 1000.0 / commits as f64,
    }
}

#[test]
fn commit_flushes_stay_constant_as_records_per_commit_grow() {
    let shapes = [
        ("files", Shape::Files),
        ("spread files", Shape::SpreadFiles),
        ("inline", Shape::Inline),
        ("delete files", Shape::DeleteFiles),
    ];
    eprintln!("shape          records  first  steady min/max  mean flushes/commit  ms/commit");
    for (name, shape) in shapes {
        let mut counts = Vec::new();
        for records in [1, 10, 100, 1000] {
            let cost = measure(shape, records);
            let steady = match (
                cost.flushes[1..].iter().min(),
                cost.flushes[1..].iter().max(),
            ) {
                (Some(min), Some(max)) => format!("{min}/{max}"),
                _ => "-".into(),
            };
            let mean = cost.flushes.iter().sum::<u64>() as f64 / cost.flushes.len() as f64;
            eprintln!(
                "{name:<14} {records:>7} {:>6} {steady:>15} {mean:>20.3} {:>10.2}",
                cost.flushes[0], cost.millis_per_commit
            );
            counts.push(cost.flushes);
        }
        // Compare each commit separately across record counts, including
        // the initial directory creation: more records never add a flush.
        assert!(
            counts.iter().all(|count| *count == counts[0]),
            "{name}: {counts:?}"
        );
    }
}

#[test]
fn a_cleaning_commit_adds_one_flush_for_retired_segments() {
    let slack = Slack::set(u64::MAX / 4);
    let directory = Directory::new();
    let mut index = Index::create(&directory.0).unwrap();
    let value = vec![0x33; 600];
    for _ in 0..3 {
        index
            .commit(
                (0..200)
                    .map(|id| Change::Put(key(false, id), value.clone()))
                    .collect(),
            )
            .unwrap();
    }
    drop(slack);
    let _slack = Slack::set(0);
    let before = flushes();
    let tail = index.header.tail;
    index
        .commit(vec![Change::Put(key(false, 0), value.clone())])
        .unwrap();
    assert!(index.header.tail > tail);
    assert_eq!(
        flushes() - before,
        1,
        "retired segments wait for the checkpoint"
    );
    // The checkpoint flushes each segment written since the last one (here
    // the sealed segment and its successor), the directories whose entries
    // changed, the tree and the redo slot.
    let before = flushes();
    index.checkpoint();
    assert!(flushes() - before <= 6, "{} flushes", flushes() - before);
}

#[test]
fn a_checkpoint_costs_the_same_after_one_commit_or_a_thousand() {
    let mut costs = Vec::new();
    for commits in [1, 1000] {
        let directory = Directory::new();
        let mut index = Index::create(&directory.0).unwrap();
        index
            .commit(vec![Change::Put(key(false, 0), vec![1; 600])])
            .unwrap();
        index.checkpoint();
        for id in 0..commits {
            index
                .commit(vec![Change::Put(key(false, id), vec![2; 600])])
                .unwrap();
        }
        let before = flushes();
        index.checkpoint();
        costs.push(flushes() - before);
    }
    assert_eq!(costs[0], costs[1], "{costs:?}");
}

/// Restores the production cleaning threshold when a test ends.
struct Slack;
impl Slack {
    fn set(bytes: u64) -> Self {
        super::CLEAN_SLACK_OVERRIDE.with(|value| value.set(Some(bytes)));
        Self
    }
}
impl Drop for Slack {
    fn drop(&mut self) {
        super::CLEAN_SLACK_OVERRIDE.with(|value| value.set(None));
    }
}
