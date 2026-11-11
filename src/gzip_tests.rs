use super::*;
/// Bytes that do not repeat: xorshift.
fn noise(length: usize, mut state: u64) -> Vec<u8> {
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn log_text(records: usize) -> Vec<u8> {
    let sources = ["torrent", "irc", "engine", "indexers", "vpn"];
    (0..records)
        .map(|i| {
            format!(
                "2026-11-{:02}T{:02}:{:02}:{:02}.{:03}Z INFO  {}: Film {} (20{:02}): downloading Film.{}.1080p.WEB-DL [job {}]\n",
                1 + i / 4000 % 28,
                i / 3600 % 24,
                i / 60 % 60,
                i % 60,
                i * 7 % 1000,
                sources[i % sources.len()],
                i % 97,
                i % 30,
                i % 89,
                i % 400
            )
        })
        .collect::<String>()
        .into_bytes()
}

#[test]
fn crc32_matches_the_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn every_kind_of_input_reads_back_with_gunzip() {
    let mut window = noise(WINDOW, 7);
    // The same bytes again, exactly as far back as DEFLATE reaches.
    window.extend_from_within(..WINDOW);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one byte", b"x".to_vec()),
        ("two bytes", b"xy".to_vec()),
        (
            "short text",
            b"Mynou keeps the log for thirty days.\n".to_vec(),
        ),
        ("one long run", vec![b'a'; 100_000]),
        ("noise", noise(150_000, 1)),
        ("farthest match", window),
        ("records", log_text(5_000)),
        ("every byte", (0..=255u8).cycle().take(70_000).collect()),
    ];
    for (name, data) in cases {
        let packed = compress(&data);
        assert_eq!(gunzip(&packed), data, "{name}");
    }
}

#[test]
fn log_records_shrink_and_members_follow_one_another() {
    let text = log_text(20_000);
    let packed = compress(&text);
    assert!(
        packed.len() * 6 < text.len(),
        "{} bytes became {}",
        text.len(),
        packed.len()
    );
    // An archive grows by members appended one after the other.
    let (first, second) = text.split_at(text.len() / 3);
    let mut joined = compress(first);
    joined.extend(compress(second));
    assert_eq!(gunzip(&joined), text);
}
