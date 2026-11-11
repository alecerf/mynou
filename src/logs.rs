//! The service log: one line per record, `<UTC time> <LEVEL> <source>: <message>`,
//! in `<data>/logs/mynou.log` and at most [`KEPT`] older files, each at most
//! [`FILE_LIMIT`] bytes. Secrets are hidden before a record is written or
//! echoed. A disk error drops the record and counts it; it never stops the
//! caller. Under launchd, the service's own stdout and stderr go to
//! [`OUTPUT`] beside it, which the log takes in and empties regularly.
//!
//! A file that rotation would drop goes to [`ARCHIVE`] instead, where the
//! `logs` thread compresses it into one gzip file per day (UTC), kept
//! [`ARCHIVE_DAYS`] days within [`ARCHIVE_LIMIT`]: enough to debug a problem
//! noticed late, and all of it goes into the debug report.
//!
//! Before [`init`], and when Mynou runs in a terminal, records also go to
//! stderr.
use crate::{Result, json::Value};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Records an error: something failed and needs attention.
#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => {
        $crate::logs::record($crate::logs::Level::Error, module_path!(), format_args!($($arg)+))
    };
}
/// Records a warning: something failed and Mynou tries again by itself.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => {
        $crate::logs::record($crate::logs::Level::Warn, module_path!(), format_args!($($arg)+))
    };
}
/// Records what Mynou did: a start, a download, an import.
#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => {
        $crate::logs::record($crate::logs::Level::Info, module_path!(), format_args!($($arg)+))
    };
}
/// Records a detail useful only to diagnose a problem.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => {
        $crate::logs::record($crate::logs::Level::Debug, module_path!(), format_args!($($arg)+))
    };
}

/// The most a log file holds before it is rotated.
pub const FILE_LIMIT: u64 = 1 << 20;
/// Older files kept beside `mynou.log`: `mynou.1.log` (newest) to `mynou.4.log`.
pub const KEPT: usize = 4;
/// The most the log holds on disk, all files together.
pub const TOTAL_LIMIT: u64 = FILE_LIMIT * (KEPT as u64 + 1);
/// The current file.
pub const CURRENT: &str = "mynou.log";
/// The folder of the log directory that holds the archives,
/// `mynou-2026-11-11.log.gz`, one per day.
pub const ARCHIVE: &str = "archive";
/// Days kept in the archive, today included.
pub const ARCHIVE_DAYS: i64 = 30;
/// The most the archive takes on disk; the oldest days make room.
pub const ARCHIVE_LIMIT: u64 = 100 << 20;
/// Files set aside by rotation and not compressed yet; past this many, the
/// oldest is dropped, as when nothing was archived.
const WAITING_LIMIT: usize = 16;
/// Where launchd writes the service's stdout and stderr.
pub const OUTPUT: &str = "output.log";
/// A record longer than this is cut.
const LINE_LIMIT: usize = 4_096;
/// The most of launchd's output taken in at once; anything older is skipped.
const FOLD_LIMIT: u64 = 256 * 1024;
/// How often launchd's output is taken in.
const FOLD_EVERY: Duration = Duration::from_secs(60);
/// After a disk error, records are dropped this long before the file is
/// opened again.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// The most entries one `GET /api/logs` returns, and their total text.
pub const MAX_ENTRIES: usize = 1_000;
const MAX_REPLY_BYTES: usize = 1 << 20;
/// The most bars of the timeline `GET /api/logs` draws the log with.
const TIMELINE_BARS: i64 = 60;
/// The minutes a bar can span: the narrowest that fits the log in
/// [`TIMELINE_BARS`] is used.
const TIMELINE_STEPS: [i64; 14] = [
    1, 2, 5, 10, 15, 30, 60, 120, 180, 360, 720, 1_440, 2_880, 10_080,
];
/// Replaces every hidden value.
pub const REDACTED: &str = "[redacted]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// The log directory of a data directory.
pub fn directory(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Creates the log directory, private to its owner, unless it exists.
pub fn prepare(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("Logs: {} is not a directory", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("Logs: cannot create {}: {e}", dir.display())),
        Err(e) => Err(format!("Logs: {}: {e}", dir.display())),
    }
}

fn numbered(dir: &Path, index: usize) -> PathBuf {
    if index == 0 {
        dir.join(CURRENT)
    } else {
        dir.join(format!("mynou.{index}.log"))
    }
}

/// Appends records to the current file and rotates it within the bound.
pub(crate) struct Writer {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
    retry_at: Option<Instant>,
}

impl Writer {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        prepare(dir)?;
        let mut writer = Self {
            dir: dir.to_owned(),
            file: None,
            size: 0,
            retry_at: None,
        };
        writer
            .reopen()
            .map_err(|e| format!("Logs: cannot open the log: {e}"))?;
        Ok(writer)
    }

    /// Opens the current file, and the directory again if it was removed.
    fn reopen(&mut self) -> std::io::Result<()> {
        prepare(&self.dir).map_err(std::io::Error::other)?;
        let file = crate::store::private_options()
            .append(true)
            .create(true)
            .open(numbered(&self.dir, 0))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        self.size = metadata.len();
        self.file = Some(file);
        Ok(())
    }

    /// Shifts every file one place older, dropping the oldest.
    fn rotate(&mut self) -> std::io::Result<()> {
        self.file = None;
        set_aside(&self.dir);
        for index in (0..KEPT).rev() {
            match fs::rename(numbered(&self.dir, index), numbered(&self.dir, index + 1)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        self.reopen()?;
        if let Some(archiver) = ARCHIVER.get() {
            archiver.unpark();
        }
        Ok(())
    }

    /// Writes one complete line; false when it was dropped.
    pub(crate) fn write(&mut self, line: &str) -> bool {
        if self.file.is_none() {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return false;
            }
            if self.reopen().is_err() {
                self.retry_at = Some(Instant::now() + RETRY_AFTER);
                return false;
            }
        }
        let result = (|| {
            if self.size + line.len() as u64 > FILE_LIMIT {
                self.rotate()?;
            }
            let file = self.file.as_mut().ok_or(std::io::ErrorKind::NotFound)?;
            file.write_all(line.as_bytes())?;
            self.size += line.len() as u64;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            self.file = None;
            self.retry_at = Some(Instant::now() + RETRY_AFTER);
        }
        result.is_ok()
    }

    /// Lets go of a current file that was removed or replaced by hand, so the
    /// next record goes to `mynou.log` again rather than to a deleted file.
    pub(crate) fn follow(&mut self) {
        let Some(open) = self.file.as_ref().map(File::metadata) else {
            return;
        };
        let same = match (fs::symlink_metadata(numbered(&self.dir, 0)), open) {
            (Ok(current), Ok(open)) => crate::store::same_inode(&current, &open),
            _ => false,
        };
        if !same {
            self.file = None;
            self.retry_at = None;
        }
    }

    /// Takes in what launchd wrote to [`OUTPUT`], as errors, and empties it.
    /// launchd appends, so writes after the truncation land at its new end.
    /// Returns the number of lines taken in.
    pub(crate) fn fold_output(&mut self) -> usize {
        let path = self.dir.join(OUTPUT);
        let Ok(mut file) = crate::store::private_options()
            .read(true)
            .write(true)
            .open(&path)
        else {
            return 0;
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return 0;
        }
        let mut bytes = Vec::new();
        let mut skipped = 0;
        let mut position = 0;
        // Output written while this reads is read too, a few times at most.
        for _ in 0..4 {
            let Ok(length) = file.metadata().map(|m| m.len()) else {
                return 0;
            };
            if length <= position {
                break;
            }
            let start = position.max(length.saturating_sub(FOLD_LIMIT));
            if start > position {
                skipped += start - position;
                bytes.clear();
            }
            if file.seek(SeekFrom::Start(start)).is_err()
                || (&mut file)
                    .take(length - start)
                    .read_to_end(&mut bytes)
                    .is_err()
            {
                return 0;
            }
            position = length;
        }
        if position == 0 || file.set_len(0).is_err() {
            return 0;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<&str> = text.lines().collect();
        if skipped > 0 {
            // The skip cut the first line.
            if !lines.is_empty() {
                lines.remove(0);
            }
            let note = line(
                Level::Warn,
                "output",
                &format!("{skipped} earlier bytes of the service output were skipped"),
            );
            if !self.write(&note) {
                count_dropped();
            }
            keep(&note);
        }
        let mut folded = 0;
        for text in lines.into_iter().filter(|l| !l.trim().is_empty()) {
            let record = line(Level::Error, "output", text);
            if self.write(&record) {
                folded += 1;
            } else {
                count_dropped();
            }
            keep(&record);
        }
        folded
    }
}

static LOGGER: Mutex<Option<Writer>> = Mutex::new(None);
/// Records also go to stderr: before `init`, and in a terminal.
static ECHO: AtomicBool = AtomicBool::new(true);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// The `logs` thread, woken when rotation sets a file aside.
static ARCHIVER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();
/// Held while the archive changes, and while the report takes its files.
static ARCHIVING: Mutex<()> = Mutex::new(());

fn logger() -> MutexGuard<'static, Option<Writer>> {
    LOGGER.lock().unwrap_or_else(|e| e.into_inner())
}

fn count_dropped() {
    DROPPED.fetch_add(1, Ordering::Relaxed);
}

/// Records the log could not write since this process started.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Whether records go to a file.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Writes records to the log of `data_dir` from now on, takes in launchd's
/// output, and logs panics. `echo` keeps records on stderr too, for a
/// terminal. Calling it again changes nothing.
pub fn init(data_dir: &Path, echo: bool) -> Result<()> {
    let mut logger = logger();
    if logger.is_some() {
        return Ok(());
    }
    let mut writer = Writer::open(&directory(data_dir))?;
    writer.fold_output();
    *logger = Some(writer);
    drop(logger);
    ECHO.store(echo, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
    install_panic_hook();
    let dir = directory(data_dir);
    let thread = std::thread::Builder::new()
        .name("logs".into())
        .spawn(move || {
            loop {
                // What an earlier run left waiting is archived first.
                archive(&dir);
                std::thread::park_timeout(FOLD_EVERY);
                fold_output();
            }
        })
        .map_err(|e| format!("Logs: cannot start: {e}"))?;
    let _ = ARCHIVER.set(thread.thread().clone());
    Ok(())
}

/// Takes in launchd's output now, so that a reader sees it.
pub fn fold_output() {
    if let Some(writer) = logger().as_mut() {
        writer.follow();
        writer.fold_output();
    }
}

/// Where a record comes from: Mynou's module, such as `irc` or `torrent`.
fn source(module: &str) -> &str {
    match module.split_once("::") {
        Some((_, rest)) => rest.split("::").next().unwrap_or(rest),
        None => module,
    }
}

/// Called by the `error!`, `warn!`, `info!` and `debug!` macros.
#[doc(hidden)]
pub fn record(level: Level, module: &str, args: fmt::Arguments<'_>) {
    let message = match args.as_str() {
        Some(text) => text.to_owned(),
        None => args.to_string(),
    };
    let line = line(level, source(module), &message);
    let active = active();
    if active {
        if !logger().as_mut().is_some_and(|w| w.write(&line)) {
            count_dropped();
        }
        keep(&line);
    }
    if level <= Level::Info && ECHO.load(Ordering::Acquire) {
        if active {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        } else {
            eprint!("{line}");
        }
    }
}

/// One complete record: redacted, on one line, bounded, with its newline.
pub(crate) fn line(level: Level, source: &str, message: &str) -> String {
    let mut text = String::with_capacity(message.len().min(LINE_LIMIT) + 48);
    text.push_str(&timestamp(SystemTime::now()));
    text.push(' ');
    text.push_str(level.tag());
    text.push(' ');
    text.push_str(source);
    text.push_str(": ");
    let start = text.len();
    for c in redact(message).chars() {
        let c = if c.is_control() { ' ' } else { c };
        if text.len() - start + c.len_utf8() > LINE_LIMIT {
            text.push('…');
            break;
        }
        text.push(c);
    }
    text.push('\n');
    text
}

/// `2026-11-11T08:15:30.123Z`.
fn timestamp(at: SystemTime) -> String {
    let elapsed = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let day = i64::try_from(seconds / 86_400)
        .ok()
        .and_then(|day| crate::date::from_day(day).ok())
        .unwrap_or_else(|| "1970-01-01".into());
    let time = seconds % 86_400;
    format!(
        "{day}T{:02}:{:02}:{:02}.{:03}Z",
        time / 3600,
        time / 60 % 60,
        time % 60,
        elapsed.subsec_millis()
    )
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".into());
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let line = line(
            Level::Error,
            "panic",
            &format!("thread '{name}' panicked{location}: {message}"),
        );
        // A panic while the log is held writes nowhere rather than wait.
        let written = LOGGER
            .try_lock()
            .ok()
            .and_then(|mut logger| logger.as_mut().map(|w| w.write(&line)))
            .unwrap_or(false);
        if !written {
            count_dropped();
        }
        if let Ok(mut recent) = RECENT.try_lock() {
            recent.push(&line);
        }
        // In a terminal, the usual report follows. Under launchd it would only
        // come back through the output file: it goes there only when the log
        // could not take it.
        if ECHO.load(Ordering::Acquire) || !written {
            previous(info);
        }
    }));
}

// Redaction ---------------------------------------------------------------

/// `text` without any secret Mynou knows (saved tokens, keys, passwords and
/// `MYNOU_` environment values) and without what looks like one: secret URL
/// parameters and fields, URL credentials and path keys, authorization and
/// cookie headers, IRC passwords.
pub fn redact(text: &str) -> String {
    redact_with(text, &crate::config::known_secret_values())
}

/// Shorter values would hide ordinary words; the patterns still apply.
const MIN_SECRET: usize = 6;

pub(crate) fn redact_with(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    let mut secrets: Vec<&str> = secrets
        .iter()
        .map(|s| s.trim())
        .filter(|s| s.len() >= MIN_SECRET)
        .collect();
    // A secret that contains another one goes first.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        if text.contains(secret) {
            text = text.replace(secret, REDACTED);
        }
    }
    let text = headers(&text);
    let text = irc(&text);
    let text = pairs(&text);
    urls(&text)
}

/// Finds `needle` (ASCII) in `haystack` from byte `from`, ignoring ASCII case.
fn find_ignoring_case(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle = needle.as_bytes();
    (from..=bytes.len().checked_sub(needle.len())?)
        .find(|&i| bytes[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn line_end(text: &str, from: usize) -> usize {
    text[from..]
        .find(['\r', '\n'])
        .map_or(text.len(), |offset| from + offset)
}

/// `Authorization`, `Proxy-Authorization`, `Cookie` and `Set-Cookie` header
/// values, and bearer tokens.
fn headers(text: &str) -> String {
    let mut text = text.to_owned();
    for name in ["authorization:", "cookie:"] {
        let mut from = 0;
        while let Some(at) = find_ignoring_case(&text, name, from) {
            let start = at + name.len();
            let end = line_end(&text, start);
            text.replace_range(start..end, &format!(" {REDACTED}"));
            from = start + REDACTED.len() + 1;
        }
    }
    let mut from = 0;
    while let Some(at) = find_ignoring_case(&text, "bearer ", from) {
        let start = at + "bearer ".len();
        let end = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';'))
            .map_or(text.len(), |offset| start + offset);
        if end > start && &text[start..end] != REDACTED {
            text.replace_range(start..end, REDACTED);
        }
        from = start;
    }
    text
}

/// SASL mechanisms and replies that `AUTHENTICATE` may show.
const MECHANISMS: [&str; 8] = [
    "PLAIN",
    "EXTERNAL",
    "SCRAM-SHA-1",
    "SCRAM-SHA-256",
    "SCRAM-SHA-512",
    "ECDSA-NIST256P-CHALLENGE",
    "+",
    "*",
];

/// IRC `PASS`, `AUTHENTICATE` (a mechanism name stays) and NickServ
/// `IDENTIFY` arguments, to the end of the line. `identify` in lower case
/// counts only as a message to a service (`:identify …`), not in prose.
fn irc(text: &str) -> String {
    let mut text = text.to_owned();
    for command in ["PASS ", "AUTHENTICATE ", "IDENTIFY "] {
        let any_case = command == "IDENTIFY ";
        let mut from = 0;
        loop {
            let found = if any_case {
                find_ignoring_case(&text, command, from)
            } else {
                text[from..].find(command).map(|offset| from + offset)
            };
            let Some(at) = found else { break };
            let start = at + command.len();
            let before = at.checked_sub(1).map(|i| text.as_bytes()[i]);
            let bounded = if &text[at..start] == command {
                matches!(before, None | Some(b' ' | b':' | b'\n' | b'\r'))
            } else {
                before == Some(b':')
            };
            let end = line_end(&text, start);
            let argument = text[start..end].trim();
            let mechanism = command == "AUTHENTICATE " && MECHANISMS.contains(&argument);
            if bounded && !argument.is_empty() && argument != REDACTED && !mechanism {
                text.replace_range(start..end, REDACTED);
            }
            from = start;
        }
    }
    text
}

/// Whether a field or parameter named `key` holds a secret.
pub(crate) fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key.ends_with("_env") {
        return false;
    }
    [
        "token",
        "apikey",
        "api_key",
        "api-key",
        "passkey",
        "authkey",
        "rsskey",
        "privatekey",
        "presharedkey",
        "_pass",
        "_secret",
        "session",
    ]
    .iter()
    .any(|suffix| key.ends_with(suffix))
        || key.contains("password")
        || matches!(key.as_str(), "key" | "pass" | "passwd" | "pwd" | "secret")
}

fn value_end(byte: u8, quote: Option<u8>) -> bool {
    match quote {
        Some(quote) => byte == quote,
        None => {
            byte.is_ascii_whitespace()
                || matches!(
                    byte,
                    b'&' | b'"' | b'\'' | b',' | b';' | b')' | b']' | b'}' | b'<' | b'>' | b'#'
                )
        }
    }
}

/// Values of secret fields: `apikey=…` in a URL or a form, `"token": "…"` in
/// JSON, `X-Plex-Token: …` in headers, `PrivateKey = …` in a profile.
fn pairs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let separator = bytes[i];
        if separator != b'=' && separator != b':' {
            i += 1;
            continue;
        }
        let mut end = i;
        while end > copied && bytes[end - 1] == b' ' {
            end -= 1;
        }
        let quoted = end > copied && matches!(bytes[end - 1], b'"' | b'\'');
        if quoted {
            end -= 1;
        }
        let mut start = end;
        while start > copied
            && (bytes[start - 1].is_ascii_alphanumeric() || matches!(bytes[start - 1], b'_' | b'-'))
        {
            start -= 1;
        }
        let key = &text[start..end];
        // `key: value` is prose unless it is a header, a JSON field or starts
        // a line.
        let field = separator == b'='
            || quoted
            || key.contains('-')
            || start == 0
            || matches!(bytes[start - 1], b'\n' | b'\r');
        if key.is_empty() || !field || !secret_key(key) || bytes.get(i + 1) == Some(&b'/') {
            i += 1;
            continue;
        }
        let mut value = i + 1;
        while value < bytes.len() && bytes[value] == b' ' {
            value += 1;
        }
        let quote = match bytes.get(value) {
            Some(&quote @ (b'"' | b'\'')) => {
                value += 1;
                Some(quote)
            }
            _ => None,
        };
        let mut stop = value;
        while stop < bytes.len() && !value_end(bytes[stop], quote) {
            stop += 1;
        }
        if stop > value && &text[value..stop] != REDACTED {
            out.push_str(&text[copied..value]);
            out.push_str(REDACTED);
            copied = stop;
        }
        i = stop.max(i + 1);
    }
    out.push_str(&text[copied..]);
    out
}

/// Whether a URL path segment looks like a key, such as a tracker passkey.
fn key_like(segment: &str) -> bool {
    segment.len() >= 16
        && segment.bytes().all(|b| b.is_ascii_alphanumeric())
        && segment.bytes().any(|b| b.is_ascii_digit())
        && segment.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Credentials before a URL's host, and path segments that look like keys.
fn urls(text: &str) -> String {
    let mut text = text.to_owned();
    let mut from = 0;
    while let Some(offset) = text[from..].find("://") {
        let authority = from + offset + 3;
        let end = text[authority..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .map_or(text.len(), |o| authority + o);
        let mut url = text[authority..end].to_owned();
        let host_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        if let Some(at) = url[..host_end].rfind('@') {
            url.replace_range(..at, REDACTED);
        }
        let host_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        if url[host_end..].starts_with('/') {
            let path_end = url[host_end..]
                .find(['?', '#'])
                .map_or(url.len(), |o| host_end + o);
            let path = url[host_end..path_end]
                .split('/')
                .map(|segment| if key_like(segment) { REDACTED } else { segment })
                .collect::<Vec<_>>()
                .join("/");
            url.replace_range(host_end..path_end, &path);
        }
        let length = url.len();
        text.replace_range(authority..end, &url);
        from = authority + length;
    }
    text
}

// Reading ----------------------------------------------------------------

/// The records shown by default.
pub const DEFAULT_ENTRIES: usize = 200;

/// What `GET /api/logs` asks for.
#[derive(Debug, PartialEq, Eq)]
pub struct Query {
    /// The least severe level shown: `info` by default.
    pub level: Level,
    /// How many records, newest first.
    pub limit: usize,
    /// Text the message or the source holds, in lower case; empty for any.
    pub text: String,
    /// The one source shown, such as `torrent`; empty for every source.
    pub source: String,
}

impl Query {
    /// Whether a record of `source` saying `message` is asked for, whatever
    /// its level.
    pub fn matches(&self, source: &str, message: &str) -> bool {
        (self.source.is_empty() || source == self.source)
            && (self.text.is_empty()
                || message.to_lowercase().contains(&self.text)
                || source.to_lowercase().contains(&self.text))
    }
}

/// `level`, `limit`, `q` (text to find) and `source` of `GET /api/logs`.
pub fn query(text: &str) -> Result<Query> {
    let mut query = Query {
        level: Level::Info,
        limit: DEFAULT_ENTRIES,
        text: String::new(),
        source: String::new(),
    };
    let decode = |value: &str, name: &str, most: usize| {
        crate::torrent::url_decode(value)
            .ok()
            .filter(|text| text.chars().count() <= most)
            .ok_or(format!(
                "{name} must be percent-encoded text of at most {most} characters"
            ))
    };
    for pair in text.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("level", value) => {
                query.level =
                    Level::parse(value).ok_or("level must be error, warn, info or debug")?;
            }
            ("limit", value) => {
                query.limit = value
                    .parse()
                    .ok()
                    .filter(|limit| (1..=MAX_ENTRIES).contains(limit))
                    .ok_or("limit must be a number from 1 to 1000")?;
            }
            ("q", value) => query.text = decode(value, "q", 200)?.trim().to_lowercase(),
            ("source", value) => query.source = decode(value, "source", 64)?.trim().to_owned(),
            _ => return Err("Logs accept only level, limit, q and source".into()),
        }
    }
    Ok(query)
}

/// The newest records the query asks for, newest first; how many records of
/// each level match it; every source in the log; the files and the archive.
pub fn tail(dir: &Path, query: &Query) -> Result<Value> {
    let limit = query.limit.clamp(1, MAX_ENTRIES);
    let mut entries = Vec::new();
    let mut bytes = 0;
    let mut files = Vec::new();
    let mut total = 0;
    let mut truncated = false;
    let mut counts = [0u64; 4];
    let mut sources: BTreeMap<String, u64> = BTreeMap::new();
    let mut newest = None;
    let mut oldest = None;
    // Records asked for, by level, in each minute; the minutes the log spans.
    let mut minutes: BTreeMap<i64, [u64; 4]> = BTreeMap::new();
    let mut span: Option<(i64, i64)> = None;
    let mut day = ([0u8; 10], 0i64);
    for index in 0..=KEPT {
        let path = numbered(dir, index);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        total += metadata.len();
        let mut file = Value::object();
        file.insert(
            "name",
            path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        );
        file.insert("bytes", Value::Number(metadata.len() as f64));
        files.push(file);
        let Some(text) = read(&path)? else { continue };
        let mut first = None;
        for raw in text.lines().rev() {
            let Some((at, level, source, message)) = parse(raw) else {
                continue;
            };
            if newest.is_none() {
                newest = Some(at.to_owned());
            }
            first = Some(at);
            match sources.get_mut(source) {
                Some(count) => *count += 1,
                None => {
                    sources.insert(source.to_owned(), 1);
                }
            }
            let at_minute = minute(at, &mut day);
            if let Some(m) = at_minute {
                span = Some(span.map_or((m, m), |(a, b)| (a.min(m), b.max(m))));
            }
            if !query.matches(source, message) {
                continue;
            }
            counts[level as usize] += 1;
            if let Some(m) = at_minute {
                minutes.entry(m).or_default()[level as usize] += 1;
            }
            if level > query.level || entries.len() >= limit || truncated {
                continue;
            }
            if bytes + message.len() > MAX_REPLY_BYTES {
                truncated = true;
                continue;
            }
            bytes += message.len();
            entries.push(entry(at, level, source, message));
        }
        if let Some(at) = first {
            oldest = Some(at.to_owned());
        }
    }
    let mut by_level = Value::object();
    for level in [Level::Error, Level::Warn, Level::Info, Level::Debug] {
        by_level.insert(level.name(), Value::Number(counts[level as usize] as f64));
    }
    let sources = sources
        .into_iter()
        .map(|(name, count)| {
            let mut source = Value::object();
            source.insert("name", name);
            source.insert("count", Value::Number(count as f64));
            source
        })
        .collect();
    let mut value = Value::object();
    value.insert("level", query.level.name());
    value.insert("entries", Value::Array(entries));
    value.insert("truncated", truncated);
    value.insert("counts", by_level);
    value.insert("sources", Value::Array(sources));
    // The span of the log read: the page drops the records it is then sent
    // that are not newer than the newest one read.
    value.insert("newest", newest.map_or(Value::Null, Value::from));
    value.insert("oldest", oldest.map_or(Value::Null, Value::from));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs() / 60).unwrap_or(0)
        });
    value.insert("timeline", timeline(span, &minutes, now));
    value.insert("files", Value::Array(files));
    value.insert("bytes", Value::Number(total as f64));
    value.insert("limit_bytes", Value::Number(TOTAL_LIMIT as f64));
    value.insert("dropped", Value::Number(dropped() as f64));
    value.insert("archive", archive_usage(dir));
    Ok(value)
}

/// The minute since the epoch of a record written at `2026-11-11T08:15:30.123Z`.
/// `day` holds the last date read and its day number: records come in runs
/// of one day.
fn minute(at: &str, day: &mut ([u8; 10], i64)) -> Option<i64> {
    let bytes = at.as_bytes();
    let number = |range: std::ops::Range<usize>| {
        let digits = bytes.get(range)?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    if bytes.get(10) != Some(&b'T') || bytes.get(13) != Some(&b':') {
        return None;
    }
    let (hour, min) = (number(11..13)?, number(14..16)?);
    if hour > 23 || min > 59 {
        return None;
    }
    if day.0 != bytes[..10] {
        let number = crate::date::day(at.get(..10)?).ok()?;
        day.0.copy_from_slice(&bytes[..10]);
        day.1 = number;
    }
    Some(day.1 * 1_440 + hour * 60 + min)
}

/// How the records asked for spread over time: `start` (UTC), `step` in
/// seconds, and for each bar the records of each level, most severe first.
/// The bars run to now, from the oldest record or [`TIMELINE_BARS`] steps
/// before now, whichever is earlier.
fn timeline(span: Option<(i64, i64)>, minutes: &BTreeMap<i64, [u64; 4]>, now: i64) -> Value {
    let Some((first, last)) = span else {
        return Value::Null;
    };
    // A clock set back leaves the newest record ahead of now.
    let last = last.max(now);
    let fits = |step: i64| last.div_euclid(step) - first.div_euclid(step) < TIMELINE_BARS;
    let step = TIMELINE_STEPS
        .into_iter()
        .find(|&step| fits(step))
        .unwrap_or_else(|| (last - first) / (TIMELINE_BARS - 1) + 1);
    // A young log still gets every bar: the timeline starts before it.
    let start = first
        .div_euclid(step)
        .min(last.div_euclid(step) - (TIMELINE_BARS - 1))
        * step;
    let len = usize::try_from(last.div_euclid(step) - start / step + 1).unwrap_or(0);
    let mut bars = vec![[0u64; 4]; len];
    for (&at, counts) in minutes.range(start..) {
        let Some(bar) = usize::try_from((at - start) / step)
            .ok()
            .and_then(|i| bars.get_mut(i))
        else {
            continue;
        };
        for (sum, count) in bar.iter_mut().zip(counts) {
            *sum += count;
        }
    }
    let Ok(date) = crate::date::from_day(start.div_euclid(1_440)) else {
        return Value::Null;
    };
    let time = start.rem_euclid(1_440);
    let mut value = Value::object();
    value.insert(
        "start",
        format!("{date}T{:02}:{:02}:00Z", time / 60, time % 60),
    );
    value.insert("step", Value::Number((step * 60) as f64));
    value.insert(
        "bars",
        Value::Array(
            bars.into_iter()
                .map(|bar| Value::Array(bar.into_iter().map(|n| Value::Number(n as f64)).collect()))
                .collect(),
        ),
    );
    value
}

/// What the log takes on disk, all files together.
pub fn size(dir: &Path) -> u64 {
    (0..=KEPT)
        .filter_map(|index| fs::symlink_metadata(numbered(dir, index)).ok())
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// A log file's text, or `None` when there is none. A file that grew past
/// its bound by hand is read up to it.
fn read(path: &Path) -> Result<Option<String>> {
    let file = match crate::store::private_options().read(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Logs: cannot read {}: {e}", path.display())),
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + LINE_LIMIT as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Logs: cannot read {}: {e}", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// One record as `GET /api/logs` and the `logs:LEVEL` topic show it.
fn entry(at: &str, level: Level, source: &str, message: &str) -> Value {
    let mut entry = Value::object();
    entry.insert("at", at);
    entry.insert("level", level.name());
    entry.insert("source", source);
    entry.insert("message", message);
    entry
}

// Following --------------------------------------------------------------

/// The most records kept in memory for the pages that follow the log, and
/// their total length: enough for a burst between two pushes.
const RECENT_LINES: usize = 1_000;
const RECENT_BYTES: usize = 512 * 1024;

/// The newest records, numbered in the order they were written.
struct Recent {
    lines: VecDeque<String>,
    bytes: usize,
    /// The number the next record takes.
    next: u64,
}

impl Recent {
    const fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            next: 0,
        }
    }

    fn push(&mut self, line: &str) {
        self.bytes += line.len();
        self.lines.push_back(line.to_owned());
        self.next += 1;
        while self.lines.len() > RECENT_LINES || self.bytes > RECENT_BYTES {
            let Some(old) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= old.len();
        }
    }

    /// The records numbered `cursor` and after, oldest first; the cursor
    /// after them; and how many of them had already left memory.
    fn since(&self, cursor: u64) -> (Vec<Value>, u64, u64) {
        let oldest = self.next - self.lines.len() as u64;
        let entries = self
            .lines
            .iter()
            .skip(usize::try_from(cursor.saturating_sub(oldest)).unwrap_or(usize::MAX))
            .filter_map(|line| parse(line.trim_end_matches('\n')))
            .map(|(at, level, source, message)| entry(at, level, source, message))
            .collect();
        (entries, self.next, oldest.saturating_sub(cursor))
    }
}

static RECENT: Mutex<Recent> = Mutex::new(Recent::new());

fn recent() -> MutexGuard<'static, Recent> {
    RECENT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps a record for the pages that follow the log.
fn keep(line: &str) {
    recent().push(line);
}

/// The number the next record takes: where a page starts following the log.
pub fn cursor() -> u64 {
    recent().next
}

/// The records written since `cursor`, oldest first; the cursor to follow on
/// from; and how many records went by too fast to be kept.
pub fn written_since(cursor: u64) -> (Vec<Value>, u64, u64) {
    recent().since(cursor)
}

// Archiving --------------------------------------------------------------

fn archiving() -> MutexGuard<'static, ()> {
    ARCHIVING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Files of `folder` named `prefix…suffix`, sorted by name.
fn named(folder: &Path, prefix: &str, suffix: &str) -> Vec<(String, PathBuf, u64)> {
    let Ok(read) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<_> = read
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            (name.starts_with(prefix) && name.ends_with(suffix) && metadata.is_file())
                .then(|| (name, entry.path(), metadata.len()))
        })
        .collect();
    found.sort();
    found
}

/// `mynou-2026-11-11.log.gz`: its day, when the name is one.
fn archive_day(name: &str) -> Option<&str> {
    let day = name.strip_prefix("mynou-")?.strip_suffix(".log.gz")?;
    let bytes = day.as_bytes();
    (bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit()))
    .then_some(day)
}

/// The archives, oldest day first: `(day, path, bytes)`.
fn archives(folder: &Path) -> Vec<(String, PathBuf, u64)> {
    named(folder, "mynou-", ".log.gz")
        .into_iter()
        .filter_map(|(name, path, bytes)| Some((archive_day(&name)?.to_owned(), path, bytes)))
        .collect()
}

/// Moves the oldest file, which rotation is about to drop, to the archive
/// folder, where the `logs` thread compresses it.
fn set_aside(dir: &Path) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let oldest = numbered(dir, KEPT);
    if !fs::symlink_metadata(&oldest).is_ok_and(|m| m.is_file()) {
        return;
    }
    let folder = dir.join(ARCHIVE);
    if prepare(&folder).is_err() {
        return;
    }
    let waiting = named(&folder, "waiting-", ".log");
    for (_, path, _) in waiting
        .iter()
        .take((waiting.len() + 1).saturating_sub(WAITING_LIMIT))
    {
        let _ = fs::remove_file(path);
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(
        "waiting-{nanos:024}-{:06}.log",
        NEXT.fetch_add(1, Ordering::Relaxed) % 1_000_000
    );
    let _ = fs::rename(&oldest, folder.join(name));
}

/// The day of a record, `2026-11-11`, from its time.
fn record_day(line: &str) -> Option<&str> {
    let day = line.get(..10)?;
    archive_day(&format!("mynou-{day}.log.gz"))
        .is_some()
        .then_some(day)
}

/// Compresses what rotation set aside into the archive of each day it
/// covers, oldest first, then keeps the archive within its bounds. A file
/// that cannot be archived waits for the next turn.
pub(crate) fn archive(dir: &Path) {
    let folder = dir.join(ARCHIVE);
    let _held = archiving();
    for (_, path, _) in named(&folder, "waiting-", ".log") {
        let Ok(Some(text)) = read(&path) else { break };
        let mut days: Vec<(String, String)> = Vec::new();
        for line in text.split_inclusive('\n') {
            let day = record_day(line)
                .map(str::to_owned)
                .or_else(|| days.last().map(|(day, _)| day.clone()))
                .unwrap_or_else(crate::date::today);
            match days.last_mut() {
                Some((last, records)) if *last == day => records.push_str(line),
                _ => days.push((day, line.to_owned())),
            }
        }
        let written = days.iter().all(|(day, records)| {
            crate::store::private_options()
                .append(true)
                .create(true)
                .open(folder.join(format!("mynou-{day}.log.gz")))
                .and_then(|mut file| {
                    file.write_all(&crate::gzip::compress(records.as_bytes()))?;
                    file.sync_data()
                })
                .is_ok()
        });
        if !written {
            break;
        }
        let _ = fs::remove_file(&path);
    }
    prune(&folder);
}

/// Drops the days older than [`ARCHIVE_DAYS`], then the oldest days while the
/// archive takes more than [`ARCHIVE_LIMIT`].
fn prune(folder: &Path) {
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    if let Ok(first) = crate::date::from_day(today as i64 - ARCHIVE_DAYS + 1) {
        prune_within(folder, &first, ARCHIVE_LIMIT);
    }
}

/// Drops the days before `first`, then the oldest while over `limit` bytes.
fn prune_within(folder: &Path, first: &str, limit: u64) {
    let mut kept = Vec::new();
    for (day, path, bytes) in archives(folder) {
        if day.as_str() < first {
            let _ = fs::remove_file(&path);
        } else {
            kept.push((path, bytes));
        }
    }
    let mut total: u64 = kept.iter().map(|(_, bytes)| bytes).sum();
    for (path, bytes) in kept {
        if total <= limit {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= bytes;
        }
    }
}

/// What the archive holds: `bytes`, `days` and the oldest day, `since`.
pub fn archive_usage(dir: &Path) -> Value {
    let archives = archives(&dir.join(ARCHIVE));
    let mut value = Value::object();
    value.insert(
        "bytes",
        Value::Number(archives.iter().map(|(_, _, bytes)| bytes).sum::<u64>() as f64),
    );
    value.insert("days", Value::Number(archives.len() as f64));
    value.insert(
        "since",
        archives
            .first()
            .map_or(Value::Null, |(day, _, _)| Value::from(day.as_str())),
    );
    value.insert("limit_bytes", Value::Number(ARCHIVE_LIMIT as f64));
    value.insert("limit_days", Value::Number(ARCHIVE_DAYS as f64));
    value
}

/// Every log file for the debug report, opened while the archive holds
/// still: `(name, file, bytes)`, archives first, then the newest records.
pub fn files(dir: &Path) -> Vec<(String, File, u64)> {
    fold_output();
    let _held = archiving();
    let open = |name: String, path: &Path| {
        let file = File::open(path).ok()?;
        let bytes = file.metadata().ok().filter(fs::Metadata::is_file)?.len();
        Some((name, file, bytes))
    };
    let mut found: Vec<_> = archives(&dir.join(ARCHIVE))
        .into_iter()
        .filter_map(|(day, path, _)| open(format!("{ARCHIVE}/mynou-{day}.log.gz"), &path))
        .collect();
    found.extend(
        named(&dir.join(ARCHIVE), "waiting-", ".log")
            .into_iter()
            .filter_map(|(name, path, _)| open(format!("{ARCHIVE}/{name}"), &path)),
    );
    found.extend((0..=KEPT).rev().filter_map(|index| {
        let path = numbered(dir, index);
        let name = path.file_name()?.to_str()?.to_owned();
        open(name, &path)
    }));
    found
}

/// `(time, level, source, message)` of one record.
fn parse(line: &str) -> Option<(&str, Level, &str, &str)> {
    let (at, rest) = line.split_once(' ')?;
    let (level, rest) = rest.trim_start().split_once(' ')?;
    let level = Level::parse(level)?;
    let (source, message) = rest.trim_start().split_once(": ")?;
    Some((at, level, source, message))
}

#[cfg(test)]
#[path = "logs_tests.rs"]
mod tests;
