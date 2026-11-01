//! Mutation fuzzing of the parsers that read untrusted bytes, with the
//! standard library only.
//!
//! ```sh
//! cargo run --release --example parser_fuzz -- 200000
//! cargo run --release --example parser_fuzz -- 1000000 tls packets
//! ```
//!
//! The first argument is the number of inputs per target (default 100,000;
//! media targets run a fiftieth of it, since each input is written to a file).
//! Further arguments keep only the targets they name. `SEED` changes the
//! deterministic input sequence. Release builds abort on a panic, which
//! would stop the whole service: before aborting, the panic hook writes the
//! input to `target/parser-fuzz/crash-TARGET.bin` and prints how to replay it
//! (`REPLAY=path`, with that target named).
use std::{cell::RefCell, path::PathBuf, sync::Mutex, time::Instant};

/// The input being parsed, for the panic hook.
static CURRENT: Mutex<(String, Vec<u8>)> = Mutex::new((String::new(), Vec::new()));

thread_local! {
    static SCRATCH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Bytes that parsers treat specially: lengths, delimiters, bencode and JSON.
const INTERESTING: [u8; 16] = [
    0, 1, 0x7f, 0x80, 0xff, b'0', b'9', b'e', b'i', b'l', b'd', b':', b'"', b'<', b'{', b'[',
];

fn mutate(rng: &mut Rng, seed: &[u8], seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..1 + rng.below(8) {
        let len = v.len();
        match rng.below(9) {
            0 if len > 0 => {
                let i = rng.below(len);
                v[i] = rng.next() as u8;
            }
            1 if len > 0 => {
                let i = rng.below(len);
                v.remove(i);
            }
            2 => {
                let i = rng.below(len + 1);
                v.insert(i, rng.next() as u8);
            }
            3 if len > 0 => {
                v.truncate(rng.below(len));
            }
            4 if len > 1 => {
                // Repeat a slice, which grows nesting and counts.
                let (a, b) = (rng.below(len), rng.below(len));
                let chunk = v[a.min(b)..a.max(b)].to_vec();
                let at = rng.below(len + 1);
                v.splice(at..at, chunk);
            }
            5 if len > 0 => {
                let i = rng.below(len);
                v[i] = INTERESTING[rng.below(INTERESTING.len())];
            }
            6 if len > 1 => {
                // A big-endian length field set to an edge value.
                let i = rng.below(len - 1);
                let value: u16 = [0, 1, 0x7fff, 0x8000, 0xffff][rng.below(5)];
                v[i..i + 2].copy_from_slice(&value.to_be_bytes());
            }
            7 if !seeds.is_empty() => {
                // Splice in part of another seed.
                let other = &seeds[rng.below(seeds.len())];
                if !other.is_empty() {
                    let (a, b) = (rng.below(other.len()), rng.below(other.len()));
                    let at = rng.below(len + 1);
                    v.splice(at..at, other[a.min(b)..a.max(b)].iter().copied());
                }
            }
            _ if len > 0 => {
                let i = rng.below(len);
                v[i] ^= 1 << rng.below(8);
            }
            _ => {}
        }
    }
    v.truncate(1 << 20);
    v
}

fn crash_path(target: &str) -> PathBuf {
    PathBuf::from("target/parser-fuzz").join(format!("crash-{target}.bin"))
}

/// Parses `data` as a file of `extension`, for the media analyzers.
fn media(extension: &str, data: &[u8]) {
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let directory = scratch.get_or_insert_with(|| {
            let directory = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join(format!("mynou-parser-fuzz-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&directory);
            directory
        });
        let path = directory.join(format!("input.{extension}"));
        if std::fs::write(&path, data).is_ok() {
            let _ = mynou::media::analyze(&path);
        }
    });
}

fn text(data: &[u8], parse: impl Fn(&str)) {
    if let Ok(text) = std::str::from_utf8(data) {
        parse(text);
    }
}

type Target = (&'static str, Vec<Vec<u8>>, fn(&[u8]), usize);

fn bencode_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(format!("{}:", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
}

fn torrent() -> Vec<u8> {
    let mut info = b"d6:lengthi40000e4:name9:movie.mkv12:piece lengthi16384e6:pieces".to_vec();
    bencode_string(&mut info, &[0xab; 60]);
    info.extend_from_slice(b"e");
    let mut out = b"d8:announce".to_vec();
    bencode_string(&mut out, b"udp://tracker.example:6969/announce");
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(&info);
    out.extend_from_slice(b"e");
    out
}

fn tracker_reply() -> Vec<u8> {
    let mut out = b"d8:completei3e10:incompletei1e8:intervali1800e5:peers".to_vec();
    bencode_string(
        &mut out,
        &[192, 0, 2, 1, 0x1a, 0xe1, 198, 51, 100, 2, 0x1a, 0xe2],
    );
    out.extend_from_slice(b"6:peers6");
    bencode_string(
        &mut out,
        &[
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x1a, 0xe1,
        ],
    );
    out.extend_from_slice(b"e");
    out
}

/// A ServerHello record: TLS 1.3 with a key share, or TLS 1.2 without.
fn server_hello(tls13: bool) -> Vec<u8> {
    let mut body = vec![3, 3];
    body.extend_from_slice(&[0x5a; 32]);
    body.push(32);
    body.extend_from_slice(&[7; 32]);
    body.extend_from_slice(&[0x13, 0x03, 0]);
    let mut extensions = if tls13 {
        vec![0x00, 0x2b, 0, 2, 3, 4, 0x00, 0x33, 0, 36, 0, 0x1d, 0, 32]
    } else {
        vec![0xff, 0x01, 0, 1, 0, 0x00, 0x0b, 0, 2, 1, 0]
    };
    if tls13 {
        extensions.extend_from_slice(&[0x42; 32]);
    } else {
        body.splice(body.len() - 3..body.len() - 1, [0xc0, 0x2f]);
    }
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);
    let mut message = vec![2, 0, (body.len() >> 8) as u8, body.len() as u8];
    message.extend_from_slice(&body);
    let mut record = vec![0x16, 3, 3];
    record.extend_from_slice(&(message.len() as u16).to_be_bytes());
    record.extend_from_slice(&message);
    record
}

/// A TLS 1.2 Certificate body: one list of certificates.
fn legacy_certificates(der: &[u8]) -> Vec<u8> {
    let mut out = ((der.len() + 3) as u32).to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&(der.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(der);
    out
}

fn certificate_message(der: &[u8]) -> Vec<u8> {
    // TLS 1.3 Certificate body: context, list length, one entry with no extensions.
    let entry_len = 3 + der.len() + 2;
    let mut out = vec![0];
    out.extend_from_slice(&(entry_len as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&(der.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(der);
    out.extend_from_slice(&[0, 0]);
    out
}

fn der_from_pem(pem: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut bits, mut count) = (0_u32, 0);
    for byte in pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .flat_map(str::bytes)
    {
        let Some(value) = TABLE.iter().position(|&b| b == byte) else {
            continue;
        };
        bits = bits << 6 | value as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out
}

/// uTP packets for a connection that expects sequence 42 and has sent 100
/// to 103, each preceded by its length: an ack, data in and out of order,
/// a FIN and a reset.
fn utp_packets() -> Vec<u8> {
    let packet = |kind: u8, seq: u16, ack: u16, payload: &[u8]| {
        let mut out = vec![kind << 4 | 1, 0, 0, 8];
        out.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 0, 4, 0, 0]);
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ack.to_be_bytes());
        out.extend_from_slice(payload);
        out
    };
    let mut out = Vec::new();
    for packet in [
        packet(2, 9, 101, b""),
        packet(0, 43, 101, b"second"),
        packet(0, 42, 102, b"first"),
        packet(2, 9, 102, b""),
        packet(1, 44, 103, b""),
        packet(3, 0, 0, b""),
    ] {
        out.push(packet.len() as u8);
        out.extend(packet);
    }
    out
}

fn dht_messages() -> Vec<Vec<u8>> {
    let id = "1:ad2:id20:abcdefghij0123456789";
    vec![
        format!("d{id}e1:q4:ping1:t2:aa1:y1:qe").into_bytes(),
        format!("d{id}9:info_hash20:mnopqrstuvwxyz123456e1:q9:get_peers1:t2:aa1:y1:qe").into_bytes(),
        format!("d{id}9:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:abcdefghe1:q13:announce_peer1:t2:aa1:y1:qe").into_bytes(),
        b"d1:rd2:id20:abcdefghij01234567895:nodes26:abcdefghij0123456789\xc6\x33\x64\x07\x1a\xe1e1:t2:aa1:y1:re".to_vec(),
    ]
}

fn packets() -> Vec<Vec<u8>> {
    let mut udp = vec![
        0x45, 0, 0, 37, 0, 1, 0, 0, 64, 17, 0, 0, 198, 51, 100, 7, 10, 2, 0, 2,
    ];
    udp.extend_from_slice(&[
        0x00, 0x35, 0xc0, 0x01, 0, 17, 0, 0, b'p', b'i', b'n', b'g', b'!', 0, 0, 0, 0,
    ]);
    let mut syn = vec![
        0x1a, 0xe1, 0xc0, 0x02, 0, 0, 0, 1, 0, 0, 0, 0, 0x80, 0x02, 0xff, 0xff, 0, 0, 0, 0,
    ];
    syn.extend_from_slice(&[2, 4, 0x05, 0xb4, 4, 2, 1, 3, 3, 7, 0, 0, 0, 0]);
    let mut dns = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
    dns.extend_from_slice(b"\x07example\x03com\x00\x00\x01\x00\x01");
    dns.extend_from_slice(&[
        0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60, 0, 6, 3, b'w', b'w', b'w', 0xc0, 0x0c,
    ]);
    dns.extend_from_slice(&[0xc0, 0x29, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
    let datagram = udp[20..].to_vec();
    vec![udp, datagram, syn, dns]
}

fn mkv() -> Vec<u8> {
    vec![
        0x1a, 0x45, 0xdf, 0xa3, 0x9f, 0x42, 0x86, 0x81, 1, 0x42, 0xf7, 0x81, 1, 0x42, 0xf2, 0x81,
        4, 0x42, 0xf3, 0x81, 8, 0x42, 0x82, 0x88, b'm', b'a', b't', b'r', b'o', b's', b'k', b'a',
        0x42, 0x87, 0x81, 4, 0x42, 0x85, 0x81, 2, 0x18, 0x53, 0x80, 0x67, 0x01, 0, 0, 0, 0, 0, 0,
        0x30, 0x15, 0x49, 0xa9, 0x66, 0x8c, 0x2a, 0xd7, 0xb1, 0x83, 0x0f, 0x42, 0x40, 0x44, 0x89,
        0x84, 0x46, 0x1c, 0x40, 0x00, 0x16, 0x54, 0xae, 0x6b, 0x95, 0xae, 0x93, 0xd7, 0x81, 1,
        0x83, 0x81, 1, 0x86, 0x85, b'V', b'_', b'H', b'E', b'V', 0xe0, 0x86, 0xb0, 0x82, 0x0f, 0,
        0xba, 0x81, 0x80,
    ]
}

fn targets() -> Vec<Target> {
    let der = der_from_pem(include_str!("../src/pki/fixtures/amazon-root-ca-1.pem"));
    vec![
        ("bencode", vec![torrent(), b"li1ei-2e3:abcd1:xle1:yi0eee".to_vec()], |d| {
            let _ = mynou::bencode::parse(d);
            let _ = mynou::bencode::parse_prefix(d);
            let _ = mynou::bencode::parse_info_raw(d);
        }, 1),
        ("metainfo", vec![torrent()], mynou::fuzzing::metainfo, 1),
        ("tracker", vec![tracker_reply()], mynou::fuzzing::tracker, 1),
        ("json", vec![format!(r#"{{"a":[1,2.5e10,-0.0,"x{0}u00e9{0}ud83d{0}ude00",true,null,{{"b":{{}}}}],"c":"{0}n"}}"#, '\\').into_bytes()],
            |d| text(d, |t| { let _ = mynou::json::parse(t); }), 1),
        ("xml", vec![br#"<?xml version="1.0"?><rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><item><title>A &amp; B 2019 1080p</title><enclosure url="http://x/a.torrent" length="1"/><torznab:attr name="seeders" value="5"/></item></channel></rss>"#.to_vec()],
            mynou::fuzzing::xml, 1),
        ("url", vec![b"https://user:pw@[::1]:8080/a//b?x=1#f".to_vec(), b"http://example.com:65535/%20".to_vec()],
            |d| text(d, |t| { let _ = mynou::net::parse_url(t); }), 1),
        ("irc", vec![b"@time=2020 :nick!user@host PRIVMSG #chan :hello world".to_vec(), b":srv 001 me :Welcome".to_vec()],
            |d| text(d, |t| { let _ = mynou::irc::protocol::Message::parse(t); }), 1),
        ("wireguard-profile", vec![b"[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.2.0.2/32, fd00::2/128\nDNS = 10.2.0.1\n\n[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0, ::/0\nEndpoint = 1.2.3.4:51820\n".to_vec()],
            |d| text(d, |t| { let _ = mynou::wireguard::profile::parse(t); }), 1),
        ("websocket", vec![vec![0x81, 0x85, 1, 2, 3, 4, b'h' ^ 1, b'e' ^ 2, b'l' ^ 3, b'l' ^ 4, b'o' ^ 1], vec![0x82, 0xfe, 0, 4, 1, 2, 3, 4, 0, 0, 0, 0]],
            |d| {
                let mut buffer = d.to_vec();
                for _ in 0..4 {
                    if !matches!(mynou::ws::decode(&mut buffer), Ok(Some(_))) { break; }
                }
            }, 1),
        ("x509", vec![der.clone()], |d| { let _ = mynou::pki::parse_certificate(d); }, 1),
        ("tls",
            vec![
                server_hello(true),
                server_hello(true)[5..].to_vec(),
                server_hello(false)[5..].to_vec(),
                certificate_message(&der),
                legacy_certificates(&der),
            ],
            mynou::fuzzing::tls, 1),
        ("packets", packets(), mynou::fuzzing::packets, 1),
        ("utp", vec![utp_packets()], mynou::fuzzing::utp, 1),
        ("dht", dht_messages(), mynou::fuzzing::dht, 1),
        ("mp4", vec![include_bytes!("../tests/fixtures/demo.mp4").to_vec()], |d| media("mp4", d), 50),
        ("mkv", vec![mkv()], |d| media("mkv", d), 50),
        ("avi", vec![[&b"RIFF\x40\x00\x00\x00AVI LIST\x20\x00\x00\x00hdrlavih\x38\x00\x00\x00"[..], &[0; 60]].concat()],
            |d| media("avi", d), 50),
    ]
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let iterations: usize = arguments
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(100_000);
    let only: Vec<String> = arguments.collect();
    let seed: u64 = std::env::var("SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (target, input) = &*CURRENT.lock().unwrap_or_else(|e| e.into_inner());
        let path = crash_path(target);
        let _ = std::fs::create_dir_all("target/parser-fuzz");
        let _ = std::fs::write(&path, input);
        eprintln!(
            "parser_fuzz: {target} panicked on a {}-byte input saved to {}; replay with REPLAY={} cargo run --example parser_fuzz -- 1 {target}",
            input.len(),
            path.display(),
            path.display()
        );
        default_hook(info);
    }));
    let replay = std::env::var_os("REPLAY")
        .map(|path| std::fs::read(path).expect("REPLAY names a readable file"));
    for (name, seeds, parse, divisor) in targets() {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        if let Some(input) = &replay {
            *CURRENT.lock().unwrap() = (name.into(), input.clone());
            parse(input);
            eprintln!("{name}: replayed without a panic");
            continue;
        }
        let started = Instant::now();
        let mut rng = Rng(seed
            ^ name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
                (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
            }));
        let count = (iterations / divisor).max(1);
        for seed in &seeds {
            *CURRENT.lock().unwrap() = (name.into(), seed.clone());
            parse(seed);
        }
        for _ in 0..count {
            let base = rng.below(seeds.len());
            let input = mutate(&mut rng, &seeds[base], &seeds);
            CURRENT.lock().unwrap().1.clone_from(&input);
            parse(&input);
        }
        eprintln!(
            "{name}: {count} inputs in {:.1} s, no panic",
            started.elapsed().as_secs_f64()
        );
    }
    SCRATCH.with(|scratch| {
        if let Some(directory) = scratch.borrow().as_ref() {
            let _ = std::fs::remove_dir_all(directory);
        }
    });
}
