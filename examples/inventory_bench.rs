//! Inventory index workloads from docs/inventory-storage.md#benchmark.
//!
//! Uses only the index's public API, single-threaded, on a fresh store:
//!
//! ```sh
//! cargo run --release --example inventory_bench -- /private/tmp/inventory-bench
//! ```
//!
//! `N` (records, default 100000), `VALUE` (bytes, default 2048), `BATCH`
//! (records per load commit, default 1000), `SINGLE` (one-record commits,
//! default 200) and `READS` (default 100000) resize the run. `PHASE=reads`
//! loops on point reads for `READS` operations, for a sampling profiler.
use mynou::inventory::{Change, Index, Key, Query};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// xorshift64*: a fixed, dependency-free sequence for repeatable runs.
struct Rng(u64);
impl Rng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % bound as u64) as usize
    }
}

fn key(id: u64) -> Key {
    let mut key = [0; 64];
    key[..8].copy_from_slice(&id.to_be_bytes());
    key[8..16].copy_from_slice(&id.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    key
}

fn value(id: u64, version: u64, length: usize) -> Vec<u8> {
    let seed = id ^ version.rotate_left(32);
    (0..length)
        .map(|i| (seed >> (i % 8 * 8)) as u8 ^ i as u8)
        .collect()
}

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn directory_bytes(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => directory_bytes(&entry.path()),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

fn report(phase: &str, result: String) {
    println!("{phase:<24} {result}");
}

fn per_op(elapsed: Duration, operations: usize) -> String {
    format!(
        "{:>9.2} µs/op",
        elapsed.as_secs_f64() * 1e6 / operations as f64
    )
}

fn rate(elapsed: Duration, records: usize) -> String {
    format!("{:>9.0} rec/s", records as f64 / elapsed.as_secs_f64())
}

fn mib(path: &Path) -> String {
    format!("{:>9.1} MiB", directory_bytes(path) as f64 / 1_048_576.0)
}

fn point_reads(
    index: &mut Index,
    rng: &mut Rng,
    records: usize,
    reads: usize,
) -> Result<String, String> {
    let start = Instant::now();
    for _ in 0..reads {
        let id = rng.below(records) as u64;
        if index.get(&key(id))?.is_none() {
            return Err(format!("record {id} is missing"));
        }
    }
    Ok(per_op(start.elapsed(), reads))
}

fn main() -> Result<(), String> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: inventory_bench EMPTY_OR_MISSING_DIRECTORY")?,
    );
    if root.exists()
        && std::fs::read_dir(&root)
            .map_err(|e| e.to_string())?
            .next()
            .is_some()
    {
        return Err("the benchmark directory must be empty or missing".into());
    }
    let _ = std::fs::remove_dir(&root);
    let records = setting("N", 100_000);
    let length = setting("VALUE", 2048);
    let batch = setting("BATCH", 1000);
    let inline = length <= 24;
    let profile = std::env::var("PHASE").is_ok_and(|phase| phase == "reads");
    let put = |id: u64, version: u64| {
        let data = value(id, version, length);
        if inline {
            Change::PutInline(key(id), data)
        } else {
            Change::Put(key(id), data)
        }
    };
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut order: Vec<u64> = (0..records as u64).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i + 1));
    }

    let mut index = Index::create(&root)?;
    let start = Instant::now();
    for chunk in order.chunks(batch) {
        index.commit(chunk.iter().map(|&id| put(id, 0)).collect())?;
    }
    report(
        "load",
        format!(
            "{}   ({records} × {length} B, {batch} per commit)",
            rate(start.elapsed(), records)
        ),
    );
    report("size after load", mib(&root));

    let reads = setting("READS", 100_000);
    if profile {
        report(
            "point reads",
            point_reads(&mut index, &mut rng, records, reads)?,
        );
        return Ok(());
    }
    if inline {
        report(
            "point reads",
            point_reads(&mut index, &mut rng, records, reads)?,
        );
        drop(index);
        return std::fs::remove_dir_all(&root).map_err(|e| e.to_string());
    }

    let singles = setting("SINGLE", 200);
    let mut samples = Vec::with_capacity(singles);
    for round in 0..singles {
        let id = rng.below(records) as u64;
        let start = Instant::now();
        index.commit(vec![put(id, 1 + round as u64)])?;
        samples.push(start.elapsed());
    }
    samples.sort();
    let milliseconds = |sample: Duration| sample.as_secs_f64() * 1e3;
    report(
        "one-record commit",
        format!(
            "{:>9.2} ms mean   p50 {:.2}   p99 {:.2}",
            milliseconds(samples.iter().sum::<Duration>()) / singles as f64,
            milliseconds(samples[singles / 2]),
            milliseconds(samples[singles * 99 / 100]),
        ),
    );

    let updates = records.min(10_000);
    let start = Instant::now();
    for _ in 0..updates / 100 {
        let mut ids = std::collections::BTreeSet::new();
        while ids.len() < 100.min(records) {
            ids.insert(rng.below(records) as u64);
        }
        index.commit(ids.into_iter().map(|id| put(id, 7)).collect())?;
    }
    report("updates, 100 per commit", rate(start.elapsed(), updates));

    report(
        "point reads",
        point_reads(&mut index, &mut rng, records, reads)?,
    );

    let start = Instant::now();
    for _ in 0..1000 {
        // Between two existing keys, so each lookup walks a random path.
        let mut missing = key(rng.below(records) as u64);
        missing[16] = 1;
        if index.get(&missing)?.is_some() {
            return Err("a missing key was found".into());
        }
    }
    report("missing-key reads", per_op(start.elapsed(), 1000));

    let start = Instant::now();
    drop(index);
    let mut index = Index::open(&root, false)?;
    report(
        "close and reopen",
        format!("{:>9.2} ms", milliseconds(start.elapsed())),
    );

    let start = Instant::now();
    let mut query = Query {
        limit: 200,
        ..Query::default()
    };
    let mut rows = 0;
    loop {
        let page = index.page(&query)?;
        rows += page.entries.len();
        match page.next {
            Some(next) => query.after = Some(next),
            None => break,
        }
    }
    if rows != records {
        return Err(format!("the scan returned {rows} of {records} records"));
    }
    report(
        "full scan, 200 per page",
        format!("{:>9.0} ms", milliseconds(start.elapsed())),
    );

    let start = Instant::now();
    for _ in 0..1000 {
        let first = rng.below(records) as u64;
        index.range_summary(key(first), Some(key(first + records as u64 / 10)))?;
    }
    report("count of a 10% range", per_op(start.elapsed(), 1000));

    let start = Instant::now();
    let deletes = &order[..records / 10];
    for chunk in deletes.chunks(batch) {
        index.commit(chunk.iter().map(|&id| Change::Delete(key(id))).collect())?;
    }
    report("deletes, 10%", rate(start.elapsed(), deletes.len()));
    report("size at end", mib(&root));
    drop(index);
    std::fs::remove_dir_all(&root).map_err(|e| e.to_string())
}
