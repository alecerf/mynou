//! Mesures locales reproductibles : `cargo run --release --offline --example benchmark`.
//! Les résultats dépendent du CPU, du système de fichiers et de son cache.
use mynou::{crypto, json, media};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

fn object(values: impl IntoIterator<Item = (&'static str, json::Value)>) -> json::Value {
    json::Value::Object(
        values
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn n(value: f64) -> json::Value {
    json::Value::Number(value)
}

fn run() -> mynou::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/demo.mp4"));
    let iterations: u32 = std::env::args()
        .nth(2)
        .map(|n| n.parse())
        .transpose()
        .map_err(|_| "Le nombre d’itérations doit être un entier".to_string())?
        .unwrap_or(2_000);
    if iterations == 0 || iterations > 1_000_000 {
        return Err("Nombre d’itérations attendu entre 1 et 1000000".into());
    }
    let probe = media::analyze(&path)?;
    // Cache chaud explicitement : on mesure l’analyse, pas la latence d’un disque froid.
    for _ in 0..32 {
        black_box(media::analyze(black_box(&path))?);
    }
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(media::analyze(black_box(&path))?);
    }
    let media_elapsed = started.elapsed().as_secs_f64();
    let block: Vec<u8> = (0..1024 * 1024)
        .map(|i| ((i * 31 + 17) & 255) as u8)
        .collect();
    let hash_iterations = 32;
    black_box(crypto::sha256(black_box(&block)));
    let started = Instant::now();
    for _ in 0..hash_iterations {
        black_box(crypto::sha256(black_box(&block)));
    }
    let sha256_elapsed = started.elapsed().as_secs_f64();
    black_box(crypto::sha1(black_box(&block)));
    let started = Instant::now();
    for _ in 0..hash_iterations {
        black_box(crypto::sha1(black_box(&block)));
    }
    let sha1_elapsed = started.elapsed().as_secs_f64();
    let mib = f64::from(hash_iterations);
    let report = object([
        (
            "mynou_version",
            json::Value::String(env!("CARGO_PKG_VERSION").into()),
        ),
        (
            "architecture",
            json::Value::String(std::env::consts::ARCH.into()),
        ),
        ("system", json::Value::String(std::env::consts::OS.into())),
        (
            "logical_parallelism",
            n(std::thread::available_parallelism().map_or(1, usize::from) as f64),
        ),
        (
            "media",
            object([
                (
                    "path",
                    json::Value::String(path.to_string_lossy().into_owned()),
                ),
                ("size_bytes", n(probe.size_bytes as f64)),
                ("cache", json::Value::String("warm".into())),
                ("iterations", n(f64::from(iterations))),
                ("seconds", n(media_elapsed)),
                (
                    "microseconds_per_analysis",
                    n(media_elapsed * 1_000_000.0 / f64::from(iterations)),
                ),
                (
                    "analyses_per_second",
                    n(f64::from(iterations) / media_elapsed),
                ),
            ]),
        ),
        (
            "hashes",
            object([
                ("bytes_per_iteration", n(block.len() as f64)),
                ("iterations", n(f64::from(hash_iterations))),
                ("sha256_mib_per_second", n(mib / sha256_elapsed)),
                ("sha1_mib_per_second", n(mib / sha1_elapsed)),
            ]),
        ),
    ]);
    println!("{}", json::stringify(&report));
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Benchmark : {error}");
        std::process::exit(1);
    }
}
