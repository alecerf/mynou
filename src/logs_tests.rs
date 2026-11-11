use super::*;
use std::os::unix::fs::PermissionsExt;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mynou-logs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn logs(&self) -> PathBuf {
        directory(&self.0)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sizes(dir: &Path) -> Vec<u64> {
    (0..=KEPT + 1)
        .filter_map(|index| fs::metadata(numbered(dir, index)).ok().map(|m| m.len()))
        .collect()
}

#[test]
fn the_log_rotates_within_its_bound_and_keeps_the_newest_records() {
    let dir = Directory::new();
    let mut writer = Writer::open(&dir.logs()).unwrap();
    let filler = "x".repeat(1_000);
    let mut written = 0_u64;
    let mut index = 0;
    while written < TOTAL_LIMIT * 3 / 2 {
        let record = line(Level::Info, "test", &format!("record {index:06} {filler}"));
        assert!(writer.write(&record));
        written += record.len() as u64;
        index += 1;
    }
    let sizes = sizes(&dir.logs());
    assert_eq!(sizes.len(), KEPT + 1, "{sizes:?}");
    assert!(sizes.iter().all(|&size| size <= FILE_LIMIT), "{sizes:?}");
    assert!(sizes.iter().sum::<u64>() <= TOTAL_LIMIT);
    assert!(!numbered(&dir.logs(), KEPT + 1).exists());
    let text = download(&dir.logs()).unwrap();
    assert!(text.contains(&format!("record {:06} ", index - 1)));
    assert!(!text.contains("record 000000 "));
    // Oldest first, one record per line.
    let numbers: Vec<u32> = text
        .lines()
        .map(|l| l.split("record ").nth(1).unwrap()[..6].parse().unwrap())
        .collect();
    assert!(numbers.windows(2).all(|pair| pair[1] == pair[0] + 1));
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.logs()), 0o700);
    assert_eq!(mode(&numbered(&dir.logs(), 0)), 0o600);
    assert_eq!(mode(&numbered(&dir.logs(), KEPT)), 0o600);
}

#[test]
fn a_write_failure_drops_the_record_instead_of_failing_the_caller() {
    let dir = Directory::new();
    let mut writer = Writer::open(&dir.logs()).unwrap();
    assert!(writer.write("one\n"));
    // The file cannot be opened: the record is dropped, and the next attempt
    // waits instead of retrying on every record.
    let current = numbered(&dir.logs(), 0);
    fs::remove_file(&current).unwrap();
    fs::create_dir(&current).unwrap();
    writer.file = None;
    assert!(!writer.write("two\n"));
    assert!(writer.retry_at.is_some());
    fs::remove_dir(&current).unwrap();
    assert!(!writer.write("three\n"));
    writer.retry_at = None;
    assert!(writer.write("four\n"));
    assert_eq!(fs::read_to_string(&current).unwrap(), "four\n");
    // A log folder removed by hand comes back with the next record once the
    // regular check notices.
    fs::remove_dir_all(dir.logs()).unwrap();
    writer.follow();
    assert!(writer.file.is_none());
    assert!(writer.write("five\n"));
    assert_eq!(fs::read_to_string(&current).unwrap(), "five\n");
    writer.follow();
    assert!(writer.file.is_some());
}

#[test]
fn records_are_one_bounded_line_with_time_level_and_source() {
    let record = line(Level::Warn, "irc", "first\nsecond\r\tthird");
    assert!(record.ends_with('\n'));
    assert_eq!(record.matches('\n').count(), 1);
    let (at, level, origin, message) = parse(record.trim_end()).unwrap();
    assert_eq!(at.len(), "2026-11-11T08:15:30.123Z".len());
    assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T');
    assert_eq!(level, Level::Warn);
    assert_eq!(origin, "irc");
    assert_eq!(message, "first second  third");
    let long = line(Level::Info, "test", &"é".repeat(LINE_LIMIT));
    assert!(long.len() <= LINE_LIMIT + 64);
    assert!(long.trim_end().ends_with('…'));
    assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        timestamp(UNIX_EPOCH + Duration::from_millis((365 * 86_400 + 3_661) * 1_000 + 500)),
        "1971-01-01T01:01:01.500Z"
    );
    assert_eq!(source("mynou::requesters::engine"), "requesters");
    assert_eq!(source("mynou::tunnel"), "tunnel");
    assert_eq!(source("mynou"), "mynou");
}

#[test]
fn known_secrets_and_secret_looking_values_are_redacted() {
    let known = vec![
        "s3cr3t-indexer-key-0042".to_owned(),
        "plex-owner-token-77".to_owned(),
        // Too short to hide on its own.
        "abc".to_owned(),
    ];
    let cases = [
        (
            "search failed with key s3cr3t-indexer-key-0042 and plex-owner-token-77",
            "search failed with key [redacted] and [redacted]",
        ),
        (
            "GET https://indexer.example/api?t=search&apikey=0123abcd&q=Film",
            "GET https://indexer.example/api?t=search&apikey=[redacted]&q=Film",
        ),
        (
            "https://tracker.example/rss?passkey=9f8e7d&api_key=1&token=xyz#top",
            "https://tracker.example/rss?passkey=[redacted]&api_key=[redacted]&token=[redacted]#top",
        ),
        (
            "http://127.0.0.1:32400/library/sections?X-Plex-Token=plexsecret",
            "http://127.0.0.1:32400/library/sections?X-Plex-Token=[redacted]",
        ),
        ("X-Plex-Token: plexsecret", "X-Plex-Token: [redacted]"),
        ("X-Api-Key: abcdef", "X-Api-Key: [redacted]"),
        (
            "Authorization: Basic dXNlcjpwYXNz",
            "Authorization: [redacted]",
        ),
        (
            "headers: Authorization: Bearer eyJhbGc.iOi\r\nHost: indexer.example",
            "headers: Authorization: [redacted]\r\nHost: indexer.example",
        ),
        ("sent bearer abc.def-ghi now", "sent bearer [redacted] now"),
        (
            "Cookie: mynou_session=abcdef; theme=dark",
            "Cookie: [redacted]",
        ),
        ("Set-Cookie: uid=1; pass=2", "Set-Cookie: [redacted]"),
        (
            "form body: username=me&password=hunter22",
            "form body: username=me&password=[redacted]",
        ),
        (
            r#"Plex answered {"id":1,"authToken":"tok123","code":"x"}"#,
            r#"Plex answered {"id":1,"authToken":"[redacted]","code":"x"}"#,
        ),
        ("PrivateKey = aGVsbG8gd29ybGQ=", "PrivateKey = [redacted]"),
        (
            "fetch https://user:pa55@feeds.example/rss failed",
            "fetch https://[redacted]@feeds.example/rss failed",
        ),
        (
            "announce https://tracker.example/announce/0123456789abcdef0123456789abcdef failed",
            "announce https://tracker.example/announce/[redacted] failed",
        ),
        (
            "https://tracker.example/a1B2c3D4e5F6g7H8i9/announce?info_hash=x",
            "https://tracker.example/[redacted]/announce?info_hash=x",
        ),
        ("IRC sent PASS hunter22", "IRC sent PASS [redacted]"),
        ("PASS hunter22", "PASS [redacted]"),
        (
            "AUTHENTICATE dXNlcgB1c2VyAGh1bnRlcjIy",
            "AUTHENTICATE [redacted]",
        ),
        ("AUTHENTICATE PLAIN", "AUTHENTICATE PLAIN"),
        ("AUTHENTICATE +", "AUTHENTICATE +"),
        (
            "PRIVMSG NickServ :IDENTIFY mynick hunter22",
            "PRIVMSG NickServ :IDENTIFY [redacted]",
        ),
        (
            "privmsg nickserv :identify hunter22",
            "privmsg nickserv :identify [redacted]",
        ),
        // Prose and names stay readable.
        (
            "Could not identify the release",
            "Could not identify the release",
        ),
        (
            "Plex token: missing; connect to Plex",
            "Plex token: missing; connect to Plex",
        ),
        (
            r#"{"password_env":"MYNOU_IRC_PASSWORD"}"#,
            r#"{"password_env":"MYNOU_IRC_PASSWORD"}"#,
        ),
        (
            "https://indexer.example/download/Film.2024.1080p.WEB-DL",
            "https://indexer.example/download/Film.2024.1080p.WEB-DL",
        ),
        ("abc is a short value", "abc is a short value"),
    ];
    for (input, expected) in cases {
        assert_eq!(redact_with(input, &known), expected, "{input}");
    }
    // A record is redacted before it is written.
    for (input, _) in cases {
        let record = line(Level::Error, "test", &redact_with(input, &known));
        for secret in [
            "s3cr3t-indexer",
            "plexsecret",
            "hunter22",
            "tok123",
            "pa55@",
        ] {
            assert!(!record.contains(secret), "{record}");
        }
    }
}

#[test]
fn launchd_output_is_folded_into_the_log_and_emptied() {
    let dir = Directory::new();
    let logs = dir.logs();
    let mut writer = Writer::open(&logs).unwrap();
    let output = logs.join(OUTPUT);
    let mut file = crate::store::private_options()
        .append(true)
        .create(true)
        .open(&output)
        .unwrap();
    file.write_all(
        b"mynou: Configuration: use schema 1\n\nthread 'main' has overflowed its stack\nAuthorization: Bearer leaked-token\n",
    )
    .unwrap();
    assert_eq!(writer.fold_output(), 3);
    assert_eq!(fs::metadata(&output).unwrap().len(), 0);
    let text = fs::read_to_string(numbered(&logs, 0)).unwrap();
    assert!(text.contains("ERROR output: mynou: Configuration: use schema 1\n"));
    assert!(text.contains("ERROR output: thread 'main' has overflowed its stack\n"));
    assert!(text.contains("ERROR output: Authorization: [redacted]\n"));
    assert!(!text.contains("leaked-token"));
    // launchd keeps appending through its descriptor: the next fold takes the
    // new lines only.
    file.write_all(b"abort\n").unwrap();
    assert_eq!(writer.fold_output(), 1);
    assert_eq!(writer.fold_output(), 0);
    let text = fs::read_to_string(numbered(&logs, 0)).unwrap();
    assert_eq!(text.matches("ERROR output: ").count(), 4);
    assert!(text.ends_with("ERROR output: abort\n"));
    // A flood keeps its end only, and says so.
    let flood = format!("{}\nlast line\n", "y".repeat(FOLD_LIMIT as usize * 2));
    file.write_all(flood.as_bytes()).unwrap();
    assert_eq!(writer.fold_output(), 1);
    let text = fs::read_to_string(numbered(&logs, 0)).unwrap();
    assert!(text.contains("earlier bytes of the service output were skipped"));
    assert!(text.ends_with("ERROR output: last line\n"));
    assert!(fs::metadata(numbered(&logs, 0)).unwrap().len() < FILE_LIMIT);
}

#[test]
fn reading_filters_by_level_newest_first() {
    let dir = Directory::new();
    let logs = dir.logs();
    prepare(&logs).unwrap();
    fs::write(
        numbered(&logs, 1),
        "2026-11-10T23:00:00.000Z ERROR irc: older error\n",
    )
    .unwrap();
    fs::write(
        numbered(&logs, 0),
        "2026-11-11T01:00:00.000Z INFO  engine: started\n\
         2026-11-11T01:00:01.000Z DEBUG indexers: detail\n\
         2026-11-11T01:00:02.000Z WARN  indexers: source unavailable: a: b\n\
         not a record\n\
         2026-11-11T01:00:03.000Z ERROR tunnel: gave up\n",
    )
    .unwrap();
    let field = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);
    let messages = |value: &Value| {
        field(value, "entries")
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                ["level", "source", "message"]
                    .map(|key| field(e, key).as_str().unwrap().to_owned())
                    .join(" ")
            })
            .collect::<Vec<_>>()
    };
    let warnings = tail(&logs, Level::Warn, 10).unwrap();
    assert_eq!(
        messages(&warnings),
        [
            "error tunnel gave up",
            "warn indexers source unavailable: a: b",
            "error irc older error"
        ]
    );
    assert_eq!(field(&warnings, "level").as_str(), Some("warn"));
    assert_eq!(field(&warnings, "files").as_array().unwrap().len(), 2);
    assert_eq!(
        field(&warnings, "limit_bytes").as_f64(),
        Some(TOTAL_LIMIT as f64)
    );
    assert_eq!(
        messages(&tail(&logs, Level::Debug, 2).unwrap()),
        [
            "error tunnel gave up",
            "warn indexers source unavailable: a: b"
        ]
    );
    assert_eq!(messages(&tail(&logs, Level::Error, 10).unwrap()).len(), 2);
    let info = tail(&logs, Level::Info, 10).unwrap();
    assert_eq!(
        field(&field(&info, "entries").as_array().unwrap()[0], "at").as_str(),
        Some("2026-11-11T01:00:03.000Z")
    );
    assert_eq!(query("").unwrap(), (Level::Info, DEFAULT_ENTRIES));
    assert_eq!(query("level=error&limit=5").unwrap(), (Level::Error, 5));
    assert!(query("level=trace").is_err());
    assert!(query("limit=0").is_err());
    assert!(query("limit=1001").is_err());
    assert!(query("since=1").is_err());
    // An empty directory reads as no record.
    let empty = Directory::new();
    let none = tail(&empty.logs(), Level::Debug, 10).unwrap();
    assert_eq!(field(&none, "entries").as_array().unwrap().len(), 0);
    assert_eq!(download(&empty.logs()).unwrap(), "");
}
