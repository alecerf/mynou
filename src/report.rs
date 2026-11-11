//! The debug report: one tar file to send with a problem. It holds every log
//! file, the archive of the last days included, what Mynou showed of its
//! state when the report was made, and a summary of the version, the system
//! and the log. Secrets stay hidden: the log hides them before anything is
//! written, and the state goes through the same filter.

use crate::{engine::Engine, json, logs};
use std::{
    fs::File,
    io::Read,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Routes whose answers describe Mynou's state, as the interface shows it.
/// Secrets (`/api/secrets`) never go in.
const STATE: [(&str, &str); 14] = [
    ("status", "/api/status"),
    ("settings", "/api/settings"),
    ("profiles", "/api/profiles"),
    ("vpn", "/api/vpn"),
    ("update", "/api/update"),
    ("jobs", "/api/jobs?limit=200"),
    ("transfers", "/api/transfers"),
    ("library", "/api/library?limit=200"),
    ("series", "/api/series?limit=200"),
    ("indexers", "/api/indexers"),
    ("feeds", "/api/feeds"),
    ("irc", "/api/irc"),
    ("requesters", "/api/requesters"),
    ("stats", "/api/stats"),
];

pub enum Body {
    Bytes(Vec<u8>),
    /// An archive as it is on disk, read up to the size it had when the
    /// report began.
    File(File),
}

pub struct Member {
    pub name: String,
    pub size: u64,
    pub body: Body,
}

/// `mynou-report-2026-11-11`: the file's name without `.tar`, and the
/// folder it opens to.
pub fn name() -> String {
    format!("mynou-report-{}", crate::date::today())
}

fn text(name: String, text: String) -> Member {
    Member {
        name,
        size: text.len() as u64,
        body: Body::Bytes(text.into_bytes()),
    }
}

/// `value` with every secret hidden: each text goes through the log's filter,
/// and a text under a secret-looking name is replaced whole.
fn hide(value: &mut json::Value) {
    match value {
        json::Value::String(text) => *text = logs::redact(text),
        json::Value::Array(items) => items.iter_mut().for_each(hide),
        json::Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if logs::secret_key(key)
                    && matches!(field, json::Value::String(text) if !text.is_empty())
                {
                    *field = json::Value::from(logs::REDACTED);
                } else {
                    hide(field);
                }
            }
        }
        json::Value::Null | json::Value::Bool(_) | json::Value::Number(_) => {}
    }
}

/// `512 bytes`, `20 KiB`, `4.2 MiB`.
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{} KiB", bytes.div_ceil(1024)),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

/// The system Mynou runs on: `macOS 26.1 (25B78) on aarch64`.
fn system() -> String {
    let arch = std::env::consts::ARCH;
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
        .unwrap_or_default();
    let value = |key: &str| {
        let after = plist.split(&format!("<key>{key}</key>")).nth(1)?;
        let start = after.find("<string>")? + "<string>".len();
        let end = after[start..].find("</string>")? + start;
        Some(after[start..end].trim().to_owned())
    };
    if let (Some(version), Some(build)) = (value("ProductVersion"), value("ProductBuildVersion")) {
        return format!("macOS {version} ({build}) on {arch}");
    }
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let pretty = release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|name| name.trim_matches('"').to_owned());
    format!(
        "{} on {arch}",
        pretty.unwrap_or_else(|| std::env::consts::OS.to_owned())
    )
}

/// What the report holds, each size known before anything is sent.
pub fn members(engine: &Arc<Engine>) -> Vec<Member> {
    let root = name();
    let data = &engine.config.data_dir;
    let dir = logs::directory(data);
    let files = logs::files(&dir);
    let archive = logs::archive_usage(&dir);
    let number = |value: &json::Value, key: &str| {
        value.get(key).and_then(json::Value::as_f64).unwrap_or(0.0) as u64
    };
    let mut members = Vec::new();

    // The state first, so the log written while it is read goes in too.
    for (name, path) in STATE {
        let mut answer = json::Value::object();
        match crate::server::route(engine, "GET", path, &[]) {
            Ok((status, value)) => {
                answer.insert("status", json::Value::Number(f64::from(status)));
                answer.insert("data", value);
            }
            Err(e) => answer.insert("error", e),
        }
        hide(&mut answer);
        members.push(text(
            format!("{root}/state/{name}.json"),
            format!("{}\n", json::stringify(&answer)),
        ));
    }

    let mut live = 0;
    let mut waiting = 0;
    for (name, mut file, bytes) in files {
        if name.ends_with(".gz") {
            members.push(Member {
                name: format!("{root}/logs/{name}"),
                size: bytes,
                body: Body::File(file),
            });
            continue;
        }
        let mut records = Vec::new();
        if (&mut file).take(bytes).read_to_end(&mut records).is_err() {
            continue;
        }
        let name = if name.starts_with(logs::ARCHIVE) {
            waiting += 1;
            format!("{}/waiting-{waiting}.log.gz", logs::ARCHIVE)
        } else {
            live += bytes;
            format!("{name}.gz")
        };
        let packed = crate::gzip::compress(&records);
        members.push(Member {
            name: format!("{root}/logs/{name}"),
            size: packed.len() as u64,
            body: Body::Bytes(packed),
        });
    }

    let made = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = data.display().to_string();
    let folder = match folder.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => folder,
    };
    let since = archive
        .get("since")
        .and_then(json::Value::as_str)
        .map_or(String::new(), |day| format!(" since {day}"));
    let dropped = logs::dropped();
    let summary = format!(
        "Mynou debug report\n\
         \n\
         Made          {made} UTC\n\
         Mynou         {version} ({build} build)\n\
         System        {system}\n\
         Data folder   {folder}\n\
         Log           {live} of recent records{dropped}\n\
         Archive       {archived} (kept {keep} days, at most {most})\n\
         \n\
         What is inside\n\
         \n\
         state/        what Mynou showed when the report was made: status, settings,\n\
         \x20             jobs, transfers, library, series, sources (JSON)\n\
         logs/         the log: mynou*.log.gz holds the newest records, archive/ one\n\
         \x20             file per day (UTC). Read them with gunzip, or open them on a Mac.\n\
         \n\
         Tokens, keys and passwords are hidden.\n",
        made = crate::date::from_day((made / 86_400) as i64).unwrap_or_default()
            + &format!(
                " {:02}:{:02}:{:02}",
                made / 3600 % 24,
                made / 60 % 60,
                made % 60
            ),
        version = env!("CARGO_PKG_VERSION"),
        build = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        system = system(),
        live = size(live),
        dropped = if dropped > 0 {
            format!("; {dropped} records could not be written")
        } else {
            String::new()
        },
        archived = match number(&archive, "days") {
            0 => "nothing yet".to_owned(),
            1 => format!("1 day{since}, {}", size(number(&archive, "bytes"))),
            days => format!("{days} days{since}, {}", size(number(&archive, "bytes"))),
        },
        keep = logs::ARCHIVE_DAYS,
        most = size(logs::ARCHIVE_LIMIT),
    );
    members.insert(0, text(format!("{root}/README.txt"), summary));
    members
}

const BLOCK: u64 = 512;

fn padded(size: u64) -> u64 {
    size.div_ceil(BLOCK) * BLOCK
}

/// How long the tar file of `members` is.
pub fn length(members: &[Member]) -> u64 {
    members
        .iter()
        .map(|member| BLOCK + padded(member.size))
        .sum::<u64>()
        + 2 * BLOCK
}

/// A ustar header for a file of `size` bytes.
fn header(name: &str, size: u64, mtime: u64) -> [u8; 512] {
    let mut block = [0u8; 512];
    let octal = |block: &mut [u8; 512], at: usize, width: usize, value: u64| {
        let digits = format!("{value:0w$o}", w = width - 1);
        block[at..at + width - 1].copy_from_slice(&digits.as_bytes()[digits.len() - (width - 1)..]);
    };
    let name = name.as_bytes();
    let length = name.len().min(100);
    block[..length].copy_from_slice(&name[..length]);
    octal(&mut block, 100, 8, 0o644);
    octal(&mut block, 108, 8, 0);
    octal(&mut block, 116, 8, 0);
    octal(&mut block, 124, 12, size);
    octal(&mut block, 136, 12, mtime);
    block[156] = b'0';
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[148..156].copy_from_slice(b"        ");
    let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
    block[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
    block[154] = 0;
    block
}

/// Writes the tar file of `members` through `send`, exactly [`length`] bytes
/// long whatever happens to the files meanwhile.
pub fn write(
    members: Vec<Member>,
    send: &mut dyn FnMut(&[u8]) -> crate::Result<()>,
) -> crate::Result<()> {
    let mtime = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let zeros = [0u8; 64 * 1024];
    for member in members {
        send(&header(&member.name, member.size, mtime))?;
        let mut left = member.size;
        match member.body {
            Body::Bytes(bytes) => {
                send(&bytes)?;
                left = 0;
            }
            Body::File(file) => {
                let mut file = file.take(member.size);
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    let n = file.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    send(&chunk[..n])?;
                    left -= n as u64;
                }
            }
        }
        // A file that shrank meanwhile is filled out, so the length holds.
        let mut fill = left + padded(member.size) - member.size;
        while fill > 0 {
            let n = fill.min(zeros.len() as u64) as usize;
            send(&zeros[..n])?;
            fill -= n as u64;
        }
    }
    send(&zeros[..2 * BLOCK as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_keeps_its_shape_and_loses_its_secrets() {
        let mut value = json::parse(
            r#"{"plex":{"token":"plex-token-1234","token_set":true,"password_set":false},
                "sources":[{"url":"https://indexer.example/api?apikey=abcdef123456&t=search"}],
                "port":8787}"#,
        )
        .unwrap();
        hide(&mut value);
        let text = json::stringify(&value);
        assert!(!text.contains("plex-token-1234"), "{text}");
        assert!(!text.contains("abcdef123456"), "{text}");
        let plex = value.get("plex").unwrap();
        assert_eq!(
            plex.get("token").and_then(json::Value::as_str),
            Some(logs::REDACTED)
        );
        assert_eq!(plex.get("token_set"), Some(&json::Value::Bool(true)));
        assert_eq!(plex.get("password_set"), Some(&json::Value::Bool(false)));
        assert_eq!(
            value.get("port").and_then(json::Value::as_f64),
            Some(8787.0)
        );
        assert!(json::parse(&text).is_ok());
    }
}
