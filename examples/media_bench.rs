//! Media analysis timings, read counts and a comparison with ffprobe.
//!
//! ```sh
//! cargo run --release --example media_bench -- /private/tmp/mynou-media-bench/corpus
//! ```
//!
//! Arguments are media files or folders of them. `RUNS` (default 10) repeats
//! each analysis; `FFPROBE` names an ffprobe binary to time and compare with;
//! `ONCE=1` runs one pass per file only, for a first read after the cache is
//! dropped. Results are printed as text, and as JSON lines to `JSON` if set;
//! `DUMP=1` also prints each analysis to standard error.
use mynou::{json, media};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn files(arguments: impl Iterator<Item = String>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for argument in arguments {
        let path = PathBuf::from(argument);
        if path.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&path)
                .expect("read corpus folder")
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            entries.sort();
            result.extend(entries);
        } else {
            result.push(path);
        }
    }
    result
}

struct Timing {
    min: Duration,
    median: Duration,
    p95: Duration,
}

fn timing(mut samples: Vec<Duration>) -> Timing {
    samples.sort();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    Timing {
        min: samples[0],
        median: at(0.5),
        p95: at(0.95),
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Codec, width, height and frame rate.
type Video = (String, Option<u32>, Option<u32>, Option<f64>);
/// Codec, sample rate, channels and language.
type Audio = (String, Option<u32>, Option<u16>, Option<String>);

/// The fields both tools report, normalized for comparison.
#[derive(Debug, PartialEq)]
struct Summary {
    duration: Option<f64>,
    video: Vec<Video>,
    audio: Vec<Audio>,
}

fn from_mynou(file: &media::MediaFile) -> Summary {
    Summary {
        duration: file.duration_seconds,
        video: file
            .video_streams
            .iter()
            .map(|v| (v.codec.clone(), v.width, v.height, v.frame_rate))
            .collect(),
        audio: file
            .audio_streams
            .iter()
            .map(|a| {
                (
                    a.codec.clone(),
                    a.sample_rate,
                    a.channels,
                    a.language.clone(),
                )
            })
            .collect(),
    }
}

fn rate(text: Option<&str>) -> Option<f64> {
    let (n, d) = text?.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

fn from_ffprobe(value: &json::Value) -> Summary {
    let number = |v: Option<&json::Value>| {
        v.and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()))
    };
    let streams = value
        .get("streams")
        .and_then(|s| s.as_array())
        .unwrap_or(&[]);
    let of = |kind: &'static str| {
        streams
            .iter()
            .filter(move |s| s.get("codec_type").and_then(|t| t.as_str()) == Some(kind))
    };
    Summary {
        duration: number(value.get("format").and_then(|f| f.get("duration"))),
        video: of("video")
            .filter(|s| {
                // Cover art is a video stream to ffprobe, not to Mynou.
                s.get("disposition")
                    .and_then(|d| d.get("attached_pic"))
                    .and_then(|a| a.as_f64())
                    != Some(1.0)
            })
            .map(|s| {
                (
                    s.get("codec_name")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .into(),
                    number(s.get("width")).map(|n: f64| n as u32),
                    number(s.get("height")).map(|n: f64| n as u32),
                    rate(s.get("avg_frame_rate").and_then(|r| r.as_str()))
                        .or(rate(s.get("r_frame_rate").and_then(|r| r.as_str()))),
                )
            })
            .collect(),
        audio: of("audio")
            .map(|s| {
                (
                    s.get("codec_name")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .into(),
                    number(s.get("sample_rate")).map(|n: f64| n as u32),
                    number(s.get("channels")).map(|n: f64| n as u16),
                    s.get("tags")
                        .and_then(|t| t.get("language"))
                        .and_then(|l| l.as_str())
                        .filter(|l| *l != "und")
                        .map(str::to_owned),
                )
            })
            .collect(),
    }
}

/// Differences that matter to Mynou: stream counts, codecs, picture size,
/// frame rate within 0.5 %, audio layout and duration within one second.
fn differences(mynou: &Summary, ffprobe: &Summary) -> Vec<String> {
    let mut found = Vec::new();
    match (mynou.duration, ffprobe.duration) {
        (Some(a), Some(b)) if (a - b).abs() <= 1.0 => {}
        (a, b) => found.push(format!("duration {a:?} vs {b:?}")),
    }
    if mynou.video.len() != ffprobe.video.len() || mynou.audio.len() != ffprobe.audio.len() {
        found.push(format!(
            "streams {}v{}a vs {}v{}a",
            mynou.video.len(),
            mynou.audio.len(),
            ffprobe.video.len(),
            ffprobe.audio.len()
        ));
    }
    for (a, b) in mynou.video.iter().zip(&ffprobe.video) {
        if a.0 != b.0 || a.1 != b.1 || a.2 != b.2 {
            found.push(format!(
                "video {:?} vs {:?}",
                (&a.0, a.1, a.2),
                (&b.0, b.1, b.2)
            ));
        }
        match (a.3, b.3) {
            (Some(x), Some(y)) if (x - y).abs() <= y * 0.005 => {}
            (x, y) => found.push(format!("frame rate {x:?} vs {y:?}")),
        }
    }
    for (a, b) in mynou.audio.iter().zip(&ffprobe.audio) {
        if a != b {
            found.push(format!("audio {a:?} vs {b:?}"));
        }
    }
    found
}

fn ffprobe(binary: &str, path: &Path, verbose: bool) -> std::process::Output {
    Command::new(binary)
        .args(["-v", if verbose { "debug" } else { "error" }])
        .args(["-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .expect("run ffprobe")
}

/// ffprobe's own count from its I/O layer: "Statistics: N bytes read, M seeks".
fn ffprobe_io(stderr: &[u8]) -> Option<(u64, u64)> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| l.contains("bytes read,"))?;
    let words: Vec<_> = line.split_whitespace().collect();
    let at = words.iter().position(|w| *w == "bytes")?;
    Some((words[at - 1].parse().ok()?, words[at + 2].parse().ok()?))
}

fn main() {
    let runs = setting("RUNS", 10).max(1);
    let once = setting("ONCE", 0) == 1;
    let probe = std::env::var("FFPROBE").ok();
    let mut out = std::env::var("JSON").ok().map(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open JSON output")
    });
    let paths = files(std::env::args().skip(1));
    assert!(!paths.is_empty(), "usage: media_bench <file or folder>...");
    println!(
        "{:<28} {:>9} {:>9} {:>9} {:>7} {:>10} {:>7} | {:>9} {:>10} {:>6}  differences",
        "file",
        "MiB",
        "med ms",
        "p95 ms",
        "reads",
        "read KiB",
        "jumps",
        "ffp ms",
        "ffp KiB",
        "seeks"
    );
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        media::take_io_stats();
        let started = Instant::now();
        let first = media::analyze(&path);
        let first_time = started.elapsed();
        let io = media::take_io_stats();
        if setting("DUMP", 0) == 1 {
            match &first {
                Ok(file) => eprintln!("{name}: {}", json::stringify(&file.to_json())),
                Err(e) => eprintln!("{name}: error {e}"),
            }
        }
        let mut samples = vec![first_time];
        if !once {
            for _ in 1..runs {
                let started = Instant::now();
                let _ = std::hint::black_box(media::analyze(&path));
                samples.push(started.elapsed());
            }
        }
        let mynou = timing(samples);
        let mut probe_time = None;
        let mut probe_io = None;
        let mut diffs = Vec::new();
        if let Some(binary) = &probe {
            let mut samples = Vec::new();
            let mut output = None;
            for _ in 0..if once { 1 } else { runs } {
                let started = Instant::now();
                let result = ffprobe(binary, &path, false);
                samples.push(started.elapsed());
                output = Some(result);
            }
            probe_time = Some(timing(samples));
            probe_io = ffprobe_io(&ffprobe(binary, &path, true).stderr);
            let output = output.unwrap();
            let parsed = json::parse(&String::from_utf8_lossy(&output.stdout));
            match (&first, parsed) {
                (Ok(file), Ok(value)) => {
                    diffs = differences(&from_mynou(file), &from_ffprobe(&value))
                }
                (Err(e), _) => diffs.push(format!("Mynou error: {e}")),
                (_, Err(e)) => diffs.push(format!("ffprobe output: {e}")),
            }
        } else if let Err(e) = &first {
            diffs.push(format!("Mynou error: {e}"));
        }
        println!(
            "{:<28} {:>9.0} {:>9.2} {:>9.2} {:>7} {:>10.0} {:>7} | {:>9} {:>10} {:>6}  {}",
            name,
            size as f64 / 1_048_576.0,
            ms(mynou.median),
            ms(mynou.p95),
            io.reads,
            io.bytes as f64 / 1024.0,
            io.jumps,
            probe_time
                .as_ref()
                .map_or("-".into(), |t| format!("{:.1}", ms(t.median))),
            probe_io.map_or("-".into(), |(b, _)| format!("{:.0}", b as f64 / 1024.0)),
            probe_io.map_or("-".into(), |(_, s)| s.to_string()),
            if diffs.is_empty() {
                "none".into()
            } else {
                diffs.join("; ")
            },
        );
        if let Some(out) = &mut out {
            use std::io::Write;
            let mut row = json::Value::object();
            row.insert("file", name.as_str());
            row.insert("bytes", size.to_string());
            row.insert("runs", if once { 1 } else { runs as u32 });
            row.insert("mynou_first_ms", format!("{:.3}", ms(first_time)));
            row.insert("mynou_min_ms", format!("{:.3}", ms(mynou.min)));
            row.insert("mynou_median_ms", format!("{:.3}", ms(mynou.median)));
            row.insert("mynou_p95_ms", format!("{:.3}", ms(mynou.p95)));
            row.insert("mynou_reads", io.reads.to_string());
            row.insert("mynou_read_bytes", io.bytes.to_string());
            row.insert("mynou_jumps", io.jumps.to_string());
            if let Some(t) = &probe_time {
                row.insert("ffprobe_median_ms", format!("{:.3}", ms(t.median)));
                row.insert("ffprobe_p95_ms", format!("{:.3}", ms(t.p95)));
            }
            if let Some((bytes, seeks)) = probe_io {
                row.insert("ffprobe_read_bytes", bytes.to_string());
                row.insert("ffprobe_seeks", seeks.to_string());
            }
            row.insert(
                "differences",
                json::Value::Array(diffs.iter().map(|d| d.as_str().into()).collect()),
            );
            writeln!(out, "{}", json::stringify(&row)).expect("write JSON");
        }
    }
}
