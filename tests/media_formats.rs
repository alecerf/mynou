use mynou::media::analyze;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str, data: &[u8]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mynou-media-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).expect("test directory");
        let path = dir.join(name);
        fs::write(&path, data).expect("fixture");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut bytes = kind.to_vec();
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        bytes.push(0);
    }
    bytes
}
fn riff(kind: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&((children.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(children);
    bytes
}
fn wave_format() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&8_000_u32.to_le_bytes());
    fmt.extend_from_slice(&16_000_u32.to_le_bytes());
    fmt.extend_from_slice(&2_u16.to_le_bytes());
    fmt.extend_from_slice(&16_u16.to_le_bytes());
    fmt
}

#[test]
fn bundled_mp4_is_analyzed_without_ffprobe() {
    let media = analyze(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo.mp4"))
        .expect("example MP4");
    assert_eq!(media.container, "mp4");
    assert_eq!(media.title, "Mynou Demo!");
    assert_eq!(media.title_source, "metadata");
    assert_eq!(media.date.as_deref(), Some("2026-10-02"));
    assert_eq!(media.duration_seconds, Some(2.0));
    assert_eq!(media.size_bytes, 125_671);
    let video = &media.video_streams[0];
    assert_eq!(video.codec, "mpeg4");
    assert_eq!(
        (video.width, video.height, video.frame_rate),
        (Some(640), Some(360), Some(24.0))
    );
    let audio = &media.audio_streams[0];
    assert_eq!(audio.codec, "aac");
    assert_eq!((audio.sample_rate, audio.channels), (Some(48_000), Some(1)));
    assert!(matches!(media.to_json(), mynou::json::Value::Object(_)));
}

#[test]
fn mp4_extended_mdat_skips_four_gibibytes() {
    let source =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo.mp4")).expect("example");
    let mut at = 0;
    let mut ftyp = Vec::new();
    let mut moov = Vec::new();
    while at < source.len() {
        let size = u32::from_be_bytes(source[at..at + 4].try_into().unwrap()) as usize;
        match &source[at + 4..at + 8] {
            b"ftyp" => ftyp.extend_from_slice(&source[at..at + size]),
            b"moov" => moov.extend_from_slice(&source[at..at + size]),
            _ => {}
        }
        at += size;
    }
    assert!(!moov.is_empty());
    let fixture = Fixture::new("sparse.mp4", &[]);
    let mut file = File::create(fixture.path()).expect("sparse fixture");
    file.write_all(&ftyp).unwrap();
    file.write_all(&1_u32.to_be_bytes()).unwrap();
    file.write_all(b"mdat").unwrap();
    let media_size = 4_u64 * 1024 * 1024 * 1024 + 16;
    file.write_all(&media_size.to_be_bytes()).unwrap();
    file.seek(SeekFrom::Start(ftyp.len() as u64 + media_size))
        .unwrap();
    file.write_all(&moov).unwrap();
    drop(file);
    let result = analyze(fixture.path()).expect("mdat must be skipped");
    assert!(result.size_bytes > 4_u64 * 1024 * 1024 * 1024);
    assert_eq!(result.title, "Mynou Demo!");
    assert_eq!(result.video_streams[0].width, Some(640));
}

#[test]
fn malformed_mp4_lengths_are_rejected() {
    let cases = [
        [
            1_u32.to_be_bytes().as_slice(),
            b"ftyp",
            &9_u64.to_be_bytes(),
        ]
        .concat(),
        [100_u32.to_be_bytes().as_slice(), b"ftyp", b"isom"].concat(),
        [
            16_u32.to_be_bytes().as_slice(),
            b"moov",
            &100_u32.to_be_bytes(),
            b"trak",
        ]
        .concat(),
        [
            1_u32.to_be_bytes().as_slice(),
            b"moov",
            &u64::MAX.to_be_bytes(),
        ]
        .concat(),
    ];
    for data in cases {
        let fixture = Fixture::new("bad.mp4", &data);
        assert!(analyze(fixture.path()).is_err());
    }
}

#[test]
fn wave_metadata_and_unknown_fields() {
    let info = [
        b"INFO".as_slice(),
        &riff_chunk(b"INAM", b"Local title\0"),
        &riff_chunk(b"ICRD", b"2026-10-03\0"),
    ]
    .concat();
    let children = [
        riff_chunk(b"fmt ", &wave_format()),
        riff_chunk(b"LIST", &info),
        riff_chunk(b"data", &vec![0; 16_000]),
    ]
    .concat();
    let fixture = Fixture::new("test.wav", &riff(b"WAVE", &children));
    let media = analyze(fixture.path()).expect("WAV");
    assert_eq!(media.title, "Local title");
    assert_eq!(media.date.as_deref(), Some("2026-10-03"));
    assert_eq!(media.duration_seconds, Some(1.0));
    assert_eq!(media.bit_rate, Some(128_000));
    assert_eq!(media.audio_streams[0].codec, "pcm_s16le");
    let fixture = Fixture::new(
        "file_name.wav",
        &riff(b"WAVE", &riff_chunk(b"fmt ", &wave_format())),
    );
    let empty = analyze(fixture.path()).unwrap();
    assert_eq!(empty.title, "file name");
    assert_eq!(empty.date, None);
    assert_eq!(empty.duration_seconds, None);
}

#[test]
fn rf64_ds64_and_truncated_padding() {
    let data = vec![0; 16_000];
    let mut wave_data = b"data".to_vec();
    wave_data.extend_from_slice(&u32::MAX.to_le_bytes());
    wave_data.extend_from_slice(&data);
    let fmt = riff_chunk(b"fmt ", &wave_format());
    let length = 12 + 36 + fmt.len() + wave_data.len();
    let mut ds64 = Vec::new();
    ds64.extend_from_slice(&((length - 8) as u64).to_le_bytes());
    ds64.extend_from_slice(&(data.len() as u64).to_le_bytes());
    ds64.extend_from_slice(&8_000_u64.to_le_bytes());
    ds64.extend_from_slice(&0_u32.to_le_bytes());
    let content = [
        b"RF64".as_slice(),
        &u32::MAX.to_le_bytes(),
        b"WAVE",
        &riff_chunk(b"ds64", &ds64),
        &fmt,
        &wave_data,
    ]
    .concat();
    let fixture = Fixture::new("large.wav", &content);
    assert_eq!(analyze(fixture.path()).unwrap().duration_seconds, Some(1.0));
    let mut odd = riff_chunk(b"JUNK", b"x");
    odd.pop();
    let fixture = Fixture::new("bad.wav", &riff(b"WAVE", &odd));
    assert!(analyze(fixture.path()).is_err());
}

fn flac_block(kind: u8, data: &[u8], last: bool) -> Vec<u8> {
    let size = (data.len() as u32).to_be_bytes();
    [
        vec![
            kind | if last { 0x80 } else { 0 },
            size[1],
            size[2],
            size[3],
        ],
        data.to_vec(),
    ]
    .concat()
}
#[test]
fn flac_streaminfo_and_comments() {
    let mut stream = vec![0; 34];
    let packed = (48_000_u64 << 44) | (1_u64 << 41) | (15_u64 << 36) | 96_000;
    stream[10..18].copy_from_slice(&packed.to_be_bytes());
    let mut comments = vec![0, 0, 0, 0];
    comments.extend_from_slice(&2_u32.to_le_bytes());
    for item in ["TITLE=Dependency-free Rust", "DATE=2026"] {
        comments.extend_from_slice(&(item.len() as u32).to_le_bytes());
        comments.extend_from_slice(item.as_bytes());
    }
    let bytes = [
        b"fLaC".to_vec(),
        flac_block(0, &stream, false),
        flac_block(4, &comments, true),
    ]
    .concat();
    let fixture = Fixture::new("test.flac", &bytes);
    let media = analyze(fixture.path()).unwrap();
    assert_eq!(media.title, "Dependency-free Rust");
    assert_eq!(media.duration_seconds, Some(2.0));
    assert_eq!(
        (
            media.audio_streams[0].sample_rate,
            media.audio_streams[0].channels
        ),
        (Some(48_000), Some(2))
    );
    let mut truncated = bytes.clone();
    truncated.pop();
    assert!(analyze(Fixture::new("bad.flac", &truncated).path()).is_err());
}

fn element(id: u32, data: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let start = id_bytes.iter().position(|byte| *byte != 0).unwrap();
    let mut result = id_bytes[start..].to_vec();
    let mut width = 1;
    while data.len() as u64 >= (1_u64 << (width * 7)) - 1 {
        width += 1;
    }
    let size = data.len() as u64 | (1_u64 << (width * 7));
    let bytes = size.to_be_bytes();
    result.extend_from_slice(&bytes[8 - width..]);
    result.extend_from_slice(data);
    result
}
fn ebml_fixture(doctype: &str, unknown_segment: bool) -> Vec<u8> {
    let ebml = element(0x1a45dfa3, &element(0x4282, doctype.as_bytes()));
    let info = element(
        0x1549a966,
        &[
            element(0x2ad7b1, &[0x0f, 0x42, 0x40]),
            element(0x4489, &2_000_f64.to_be_bytes()),
            element(0x7ba9, "EBML Demo".as_bytes()),
        ]
        .concat(),
    );
    let video = element(
        0xae,
        &[
            element(0x83, &[1]),
            element(0x86, b"V_VP9"),
            element(0x23e383, &40_000_000_u64.to_be_bytes()),
            element(
                0xe0,
                &[
                    element(0xb0, &1_280_u16.to_be_bytes()),
                    element(0xba, &720_u16.to_be_bytes()),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let audio = element(
        0xae,
        &[
            element(0x83, &[2]),
            element(0x86, b"A_OPUS"),
            element(0x22b59c, b"eng"),
            element(
                0xe1,
                &[
                    element(0xb5, &48_000_f64.to_be_bytes()),
                    element(0x9f, &[2]),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let payload = [info, element(0x1654ae6b, &[video, audio].concat())].concat();
    let segment = if unknown_segment {
        [vec![0x18, 0x53, 0x80, 0x67, 0xff], payload].concat()
    } else {
        element(0x18538067, &payload)
    };
    [ebml, segment].concat()
}
#[test]
fn matroska_webm_known_and_unknown_segment_sizes() {
    for (doctype, unknown) in [("matroska", false), ("webm", true)] {
        let fixture = Fixture::new("test.mkv", &ebml_fixture(doctype, unknown));
        let media = analyze(fixture.path()).unwrap();
        assert_eq!(media.container, doctype);
        assert_eq!(media.title, "EBML Demo");
        assert_eq!(media.duration_seconds, Some(2.0));
        assert_eq!(
            (
                media.video_streams[0].width,
                media.video_streams[0].height,
                media.video_streams[0].frame_rate
            ),
            (Some(1_280), Some(720), Some(25.0))
        );
        assert_eq!(media.audio_streams[0].codec, "opus");
        assert_eq!(media.audio_streams[0].language.as_deref(), Some("eng"));
        assert_eq!(media.audio_streams[0].sample_rate, Some(48_000));
    }
    let fixture = Fixture::new("bad.mkv", &ebml_fixture("other", false));
    assert!(analyze(fixture.path()).is_err());
    let fixture = Fixture::new("bad.mkv", &[0x1a, 0x45, 0xdf, 0xa3, 0]);
    assert!(analyze(fixture.path()).is_err());
}

#[test]
fn bounded_parsers_do_not_panic_on_mutated_headers() {
    let original =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/demo.mp4")).unwrap();
    let fixture = Fixture::new("mutated.mp4", &[]);
    let mut state = 0x9e37_79b9_u64;
    for turn in 0..512 {
        let len = match turn % 4 {
            0 => turn,
            1 => 16,
            2 => 64,
            _ => original.len(),
        };
        let mut bytes = original[..len.min(original.len())].to_vec();
        for _ in 0..8 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if !bytes.is_empty() {
                let at = state as usize % bytes.len();
                bytes[at] = (state >> 32) as u8;
            }
        }
        fs::write(fixture.path(), bytes).unwrap();
        let _ = analyze(fixture.path());
    }
}

#[test]
fn mp3_id3_utf8_and_xing_duration() {
    fn safe_integer(size: usize) -> [u8; 4] {
        [
            ((size >> 21) & 127) as u8,
            ((size >> 14) & 127) as u8,
            ((size >> 7) & 127) as u8,
            (size & 127) as u8,
        ]
    }
    let mut tags = Vec::new();
    for (kind, value) in [(b"TIT2", "MP3 Demo – UTF-8"), (b"TDRC", "2026-10-03")] {
        tags.extend_from_slice(kind);
        tags.extend_from_slice(&safe_integer(value.len() + 1));
        tags.extend_from_slice(&[0, 0, 3]);
        tags.extend_from_slice(value.as_bytes());
    }
    let mut data = [
        b"ID3".as_slice(),
        &[4, 0, 0],
        &safe_integer(tags.len()),
        &tags,
    ]
    .concat();
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
    frame[36..40].copy_from_slice(b"Xing");
    frame[40..44].copy_from_slice(&1_u32.to_be_bytes());
    frame[44..48].copy_from_slice(&100_u32.to_be_bytes());
    data.extend_from_slice(&frame);
    data.extend_from_slice(&frame);
    let fixture = Fixture::new("test.mp3", &data);
    let media = analyze(fixture.path()).unwrap();
    assert_eq!(media.title, "MP3 Demo – UTF-8");
    assert_eq!(media.date.as_deref(), Some("2026-10-03"));
    assert_eq!(media.audio_streams[0].codec, "mp3");
    assert_eq!(
        (
            media.audio_streams[0].sample_rate,
            media.audio_streams[0].channels
        ),
        (Some(44_100), Some(2))
    );
    assert!((media.duration_seconds.unwrap() - 100.0 * 1_152.0 / 44_100.0).abs() < 1e-9);
    let fixture = Fixture::new("bad.mp3", b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f");
    assert!(analyze(fixture.path()).is_err());
}

#[test]
fn avi_streams_and_negative_bitmap_height() {
    let mut avih = vec![0; 56];
    avih[..4].copy_from_slice(&40_000_u32.to_le_bytes());
    avih[16..20].copy_from_slice(&50_u32.to_le_bytes());
    let mut strh = vec![0; 56];
    strh[..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(b"XVID");
    strh[20..24].copy_from_slice(&1_u32.to_le_bytes());
    strh[24..28].copy_from_slice(&25_u32.to_le_bytes());
    let mut bitmap = vec![0; 40];
    bitmap[..4].copy_from_slice(&40_u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&640_i32.to_le_bytes());
    bitmap[8..12].copy_from_slice(&(-360_i32).to_le_bytes());
    bitmap[16..20].copy_from_slice(b"XVID");
    let video = riff_chunk(
        b"LIST",
        &[
            b"strl".as_slice(),
            &riff_chunk(b"strh", &strh),
            &riff_chunk(b"strf", &bitmap),
        ]
        .concat(),
    );
    strh[..4].copy_from_slice(b"auds");
    let audio = riff_chunk(
        b"LIST",
        &[
            b"strl".as_slice(),
            &riff_chunk(b"strh", &strh),
            &riff_chunk(b"strf", &wave_format()),
        ]
        .concat(),
    );
    let hdrl = riff_chunk(
        b"LIST",
        &[
            b"hdrl".as_slice(),
            &riff_chunk(b"avih", &avih),
            &video,
            &audio,
        ]
        .concat(),
    );
    let fixture = Fixture::new("test.avi", &riff(b"AVI ", &hdrl));
    let media = analyze(fixture.path()).unwrap();
    assert_eq!(media.container, "avi");
    assert_eq!(media.duration_seconds, Some(2.0));
    assert_eq!(media.video_streams[0].codec, "mpeg4");
    assert_eq!(
        (
            media.video_streams[0].width,
            media.video_streams[0].height,
            media.video_streams[0].frame_rate
        ),
        (Some(640), Some(360), Some(25.0))
    );
    assert_eq!(media.audio_streams[0].index, 1);
    assert_eq!(media.audio_streams[0].codec, "pcm_s16le");
}
