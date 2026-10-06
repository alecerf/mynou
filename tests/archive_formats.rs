//! Original synthetic archives and independent raw-DEFLATE golden streams.
mod archive_support;
mod library_support;
use archive_support::{Fixture, archive, hex, original_payload, reference_crc, stored_zip};
use library_support::Directory;
use mynou::{
    archive::{Limits, Method, Zip, deflate},
    crypto, json,
};
use std::{
    fs,
    io::{self, Cursor, Read, Write},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

fn decode(bytes: &[u8], bound: u64) -> mynou::Result<(Vec<u8>, deflate::Decoded)> {
    let mut out = Vec::new();
    let result = deflate::decode(
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        &mut out,
        bound,
        16_384,
        &AtomicBool::new(true),
    )?;
    Ok((out, result))
}
fn parsed(bytes: &[u8]) -> Zip {
    Zip::read(&mut Cursor::new(bytes), Limits::default()).unwrap()
}
fn extract(bytes: &[u8], index: usize) -> (Vec<u8>, mynou::archive::VerifiedEntry) {
    let zip = parsed(bytes);
    let mut out = Vec::new();
    let proof = zip
        .extract(
            &mut Cursor::new(bytes),
            index,
            &mut out,
            &AtomicBool::new(true),
        )
        .unwrap();
    (out, proof)
}
fn directory_start(bytes: &[u8]) -> usize {
    let i = bytes.len() - 6;
    u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize
}
fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    count: usize,
}
impl Bits {
    fn field(&mut self, value: u32, width: u8) {
        for bit in 0..width {
            if self.count % 8 == 0 {
                self.bytes.push(0);
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= (((value >> bit) & 1) as u8) << (self.count % 8);
            self.count += 1;
        }
    }
    fn code(&mut self, value: u32, width: u8) {
        self.field(value.reverse_bits() >> (32 - width), width);
    }
    fn fixed(&mut self, symbol: u16) {
        match symbol {
            0..=143 => self.code(0x30 + u32::from(symbol), 8),
            144..=255 => self.code(0x190 + u32::from(symbol - 144), 9),
            256..=279 => self.code(u32::from(symbol - 256), 7),
            280..=287 => self.code(0xc0 + u32::from(symbol - 280), 8),
            _ => panic!("Invalid fixture symbol"),
        }
    }
    fn dynamic_header(&mut self, code_lengths: [u8; 4]) {
        self.field(1, 1);
        self.field(2, 2);
        self.field(0, 5);
        self.field(0, 5);
        self.field(0, 4);
        for length in code_lengths {
            self.field(u32::from(length), 3);
        }
    }
}

#[test]
fn independent_fixed_and_dynamic_streams_preserve_every_byte_and_checksums() {
    let expected = original_payload();
    for (text, block) in [
        (include_str!("archive_support/fixed.hex"), 1),
        (include_str!("archive_support/dynamic.hex"), 2),
    ] {
        let bytes = hex(text);
        assert_eq!((bytes[0] >> 1) & 3, block);
        let (actual, proof) = decode(&bytes, expected.len() as u64).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(proof.bytes, expected.len() as u64);
        assert_eq!(proof.crc32, reference_crc(&expected));
        assert_eq!(proof.sha256, crypto::sha256(&expected));
    }
}

#[test]
fn maximum_distance_and_overlapping_copy_survive_window_wraparound() {
    let mut bits = Bits::default();
    bits.field(1, 1);
    bits.field(1, 2);
    let mut expected: Vec<_> = (0..32_768).map(|i| (i % 251) as u8).collect();
    for &byte in &expected {
        bits.fixed(u16::from(byte));
    }
    bits.fixed(285);
    bits.code(29, 5);
    bits.field(8191, 13);
    expected.extend_from_within(..258);
    bits.fixed(285);
    bits.code(0, 5);
    expected.extend(std::iter::repeat_n(*expected.last().unwrap(), 258));
    bits.fixed(256);
    assert_eq!(
        decode(&bits.bytes, expected.len() as u64).unwrap().0,
        expected
    );
}

#[test]
fn stored_and_empty_blocks_support_exact_bounded_input_without_overread() {
    let payload = b"Original stored bytes";
    let mut bytes = vec![1];
    bytes.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(!(payload.len() as u16)).to_le_bytes());
    bytes.extend_from_slice(payload);
    assert_eq!(decode(&bytes, payload.len() as u64).unwrap().0, payload);
    let declared = bytes.len();
    bytes.extend_from_slice(b"outside the declared payload");
    let mut reader = Cursor::new(bytes);
    let mut out = Vec::new();
    deflate::decode(
        &mut reader,
        declared as u64,
        &mut out,
        payload.len() as u64,
        1,
        &AtomicBool::new(true),
    )
    .unwrap();
    assert_eq!(reader.position(), declared as u64);
    assert_eq!(out, payload);
    let empty = [0, 0, 0, 255, 255, 1, 0, 0, 255, 255];
    assert!(
        deflate::decode(
            &mut Cursor::new(empty),
            10,
            &mut Vec::new(),
            0,
            1,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("block count")
    );
    assert!(decode(&empty, 0).unwrap().0.is_empty());
}

#[test]
fn malformed_and_truncated_deflate_never_returns_a_proof() {
    let bytes = hex(include_str!("archive_support/dynamic.hex"));
    let prefixes = (0..64)
        .chain((64..bytes.len()).step_by(31))
        .chain(bytes.len() - 32..bytes.len());
    for i in prefixes {
        assert!(decode(&bytes[..i], 200_000).is_err(), "prefix {i}");
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(decode(&extra, 200_000).unwrap_err().contains("trailing"));
    assert!(decode(&[7], 100).unwrap_err().contains("Reserved"));
    assert!(
        decode(&[1, 1, 0, 0, 0, b'x'], 100)
            .unwrap_err()
            .contains("inconsistent")
    );
    for reserved in [286, 287] {
        let mut bits = Bits::default();
        bits.field(1, 1);
        bits.field(1, 2);
        bits.fixed(reserved);
        assert!(decode(&bits.bytes, 100).unwrap_err().contains("Reserved"));
    }
    for distance in [0, 30, 31] {
        let mut bits = Bits::default();
        bits.field(1, 1);
        bits.field(1, 2);
        bits.fixed(257);
        bits.code(distance, 5);
        assert!(decode(&bits.bytes, 100).is_err());
    }
}

#[test]
fn dynamic_alphabet_oversubscription_incompleteness_and_repeats_are_rejected() {
    for (lengths, message) in [
        ([1, 1, 1, 0], "Oversubscribed"),
        ([2, 2, 0, 0], "Incomplete"),
        ([0; 4], "Empty"),
    ] {
        let mut bits = Bits::default();
        bits.dynamic_header(lengths);
        assert!(decode(&bits.bytes, 10).unwrap_err().contains(message));
    }
    let mut repeat = Bits::default();
    repeat.dynamic_header([1, 0, 1, 0]);
    repeat.field(0, 1);
    assert!(
        decode(&repeat.bytes, 10)
            .unwrap_err()
            .contains("no previous")
    );
    let mut overflow = Bits::default();
    overflow.dynamic_header([0, 0, 1, 1]);
    for _ in 0..2 {
        overflow.field(1, 1);
        overflow.field(127, 7);
    }
    assert!(
        decode(&overflow.bytes, 10)
            .unwrap_err()
            .contains("exceeds its alphabet")
    );
    let mut missing = Bits::default();
    missing.dynamic_header([0, 0, 1, 1]);
    missing.field(1, 1);
    missing.field(127, 7);
    missing.field(1, 1);
    missing.field(109, 7);
    assert!(
        decode(&missing.bytes, 10)
            .unwrap_err()
            .contains("no end-of-block")
    );
    let mut reserved = Bits::default();
    reserved.field(1, 1);
    reserved.field(2, 2);
    reserved.field(31, 5);
    assert!(
        decode(&reserved.bytes, 10)
            .unwrap_err()
            .contains("alphabet size")
    );
}

#[test]
fn literal_only_dynamic_block_can_omit_the_distance_alphabet() {
    let mut bits = Bits::default();
    bits.field(1, 1);
    bits.field(2, 2);
    bits.field(0, 5);
    bits.field(0, 5);
    bits.field(14, 4);
    let order = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1];
    for index in order {
        bits.field(u32::from(index == 0 || index == 1), 3);
    }
    for index in 0..258 {
        bits.field(u32::from(index == 65 || index == 256), 1);
    }
    bits.field(0, 1);
    bits.field(1, 1);
    assert_eq!(decode(&bits.bytes, 1).unwrap().0, b"A");
}

#[test]
fn decoded_output_limit_is_enforced_before_writing_excess_bytes() {
    let bytes = hex(include_str!("archive_support/fixed.hex"));
    let mut out = Vec::new();
    assert!(
        deflate::decode(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &mut out,
            80_000,
            100,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("byte limit")
    );
    assert_eq!(out.len(), 65_536);
    assert_eq!(out, original_payload()[..65_536]);
    for (input, output, blocks) in [
        (0, 10, 1),
        (u64::MAX, 10, 1),
        (1, u64::MAX, 1),
        (1, 10, 0),
        (1, 10, 65_537),
    ] {
        assert!(
            deflate::decode(
                &mut io::empty(),
                input,
                &mut Vec::new(),
                output,
                blocks,
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }
}

struct CancelSink<'a> {
    flag: &'a AtomicBool,
    bytes: Vec<u8>,
}
impl Write for CancelSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        self.flag.store(false, Ordering::Release);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn cancellation_at_a_streaming_write_fences_the_final_result() {
    let bytes = hex(include_str!("archive_support/dynamic.hex"));
    let flag = AtomicBool::new(true);
    let mut sink = CancelSink {
        flag: &flag,
        bytes: Vec::new(),
    };
    assert!(
        deflate::decode(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &mut sink,
            200_000,
            100,
            &flag
        )
        .unwrap_err()
        .contains("cancelled")
    );
    assert_eq!(sink.bytes.len(), 65_536);
    assert_eq!(sink.bytes, original_payload()[..65_536]);
    let zip_bytes = stored_zip("movie.mp4", &original_payload());
    let zip = parsed(&zip_bytes);
    flag.store(true, Ordering::Release);
    sink.bytes.clear();
    assert!(
        zip.extract(&mut Cursor::new(zip_bytes), 0, &mut sink, &flag)
            .unwrap_err()
            .contains("cancelled")
    );
    assert_eq!(sink.bytes.len(), 65_536);
    flag.store(true, Ordering::Release);
    sink.bytes.clear();
    // The final write is also fenced, including a stream with only one byte.
    let mut small = Bits::default();
    small.field(1, 1);
    small.field(1, 2);
    small.fixed(u16::from(b'A'));
    small.fixed(256);
    assert!(
        deflate::decode(
            &mut Cursor::new(&small.bytes),
            small.bytes.len() as u64,
            &mut sink,
            1,
            1,
            &flag
        )
        .unwrap_err()
        .contains("cancelled")
    );
    assert_eq!(sink.bytes, b"A");
}

#[test]
fn stored_media_and_independent_deflate_zip_entries_verify_size_crc_and_sha() {
    let media = include_bytes!("../examples/demo.mp4");
    let bytes = stored_zip("Original Movie (2026)/Original.Movie.2026.1080p.mp4", media);
    let zip = parsed(&bytes);
    assert_eq!(zip.entries()[0].method(), Method::Stored);
    assert_eq!(zip.declared_bytes(), media.len() as u64);
    let (actual, proof) = extract(&bytes, 0);
    assert_eq!(actual, media);
    assert_eq!(proof.bytes(), media.len() as u64);
    assert_eq!(proof.crc32(), reference_crc(media));
    assert_eq!(*proof.sha256(), crypto::sha256(media));
    let expected = original_payload();
    for text in [
        include_str!("archive_support/fixed.hex"),
        include_str!("archive_support/dynamic.hex"),
    ] {
        let compressed = hex(text);
        let mut fixture = Fixture::stored("original.bin", &expected);
        fixture.deflate = Some(&compressed);
        let bytes = archive(&[fixture]);
        assert_eq!(parsed(&bytes).entries()[0].method(), Method::Deflate);
        assert_eq!(extract(&bytes, 0).0, expected);
    }
}

#[test]
fn signed_and_unsigned_zip_descriptors_are_checked_before_decode() {
    for signed in [false, true] {
        let payload = b"Original descriptor fixture";
        let mut fixture = Fixture::stored("movie.mp4", payload);
        fixture.descriptor = Some(signed);
        let mut bytes = archive(&[fixture]);
        assert_eq!(extract(&bytes, 0).0, payload);
        let start = directory_start(&bytes) - if signed { 16 } else { 12 };
        bytes[start + if signed { 4 } else { 0 }] ^= 1;
        assert!(
            Zip::read(&mut Cursor::new(bytes), Limits::default())
                .unwrap_err()
                .contains("descriptor disagrees")
        );
    }
}

#[test]
fn payload_crc_failure_does_not_turn_metadata_into_verified_content() {
    let mut bytes = stored_zip("original.bin", b"Original payload");
    let zip = parsed(&bytes);
    assert_eq!(zip.report().get("content_verified"), Some(&false.into()));
    bytes[30 + "original.bin".len()] ^= 1;
    assert!(
        zip.extract(
            &mut Cursor::new(bytes),
            0,
            &mut Vec::new(),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("CRC verification")
    );
}

#[test]
fn every_truncated_zip_prefix_is_rejected_without_panicking() {
    let bytes = archive(&[
        Fixture::stored("first.mp4", b"first"),
        Fixture::stored("second.mp4", b"second"),
    ]);
    for index in 0..bytes.len() {
        assert!(
            Zip::read(&mut Cursor::new(&bytes[..index]), Limits::default()).is_err(),
            "prefix {index}"
        );
    }
}

#[test]
fn unsafe_and_ambiguous_zip_names_are_rejected() {
    for name in [
        "../movie.mp4",
        "/movie.mp4",
        "a/../movie.mp4",
        "a//movie.mp4",
        "a/./movie.mp4",
        "C:/movie.mp4",
        "a\\movie.mp4",
        "a/movie.mp4 ",
        "a/movie.mp4.",
        "a\0.mp4",
        "CON.mp4",
        "a/COM9.mkv",
        "a/LPT1.txt",
    ] {
        assert!(
            Zip::read(&mut Cursor::new(stored_zip(name, b"x")), Limits::default()).is_err(),
            "{name:?}"
        );
    }
    for names in [
        ["Movie.mp4", "movie.mp4"],
        ["movie", "movie/part.mp4"],
        ["movie/", "MOVIE"],
    ] {
        let bytes = archive(&[
            Fixture::stored(names[0], b""),
            Fixture::stored(names[1], b""),
        ]);
        assert!(Zip::read(&mut Cursor::new(bytes), Limits::default()).is_err());
    }
    let deep = "a/".repeat(33) + "movie.mp4";
    assert!(Zip::read(&mut Cursor::new(stored_zip(&deep, b"x")), Limits::default()).is_err());
}

#[test]
fn explicit_utf8_and_ascii_names_are_supported_without_legacy_encoding_guesses() {
    let bytes = stored_zip("Original Café/movie.mp4", b"x");
    assert_eq!(
        parsed(&bytes).entries()[0].name(),
        "Original Café/movie.mp4"
    );
    let mut legacy = bytes;
    let start = directory_start(&legacy);
    put16(&mut legacy, 6, 0);
    put16(&mut legacy, start + 8, 0);
    assert!(
        Zip::read(&mut Cursor::new(legacy), Limits::default())
            .unwrap_err()
            .contains("ASCII")
    );
    let mut invalid = stored_zip("original.mp4", b"x");
    let start = directory_start(&invalid);
    invalid[30] = 255;
    invalid[start + 46] = 255;
    assert!(
        Zip::read(&mut Cursor::new(invalid), Limits::default())
            .unwrap_err()
            .contains("UTF-8")
    );
}

#[test]
fn links_devices_encryption_zip64_and_alternate_names_fail_closed() {
    for kind in [0o120777u32, 0o020600, 0o060600, 0o010600] {
        let mut fixture = Fixture::stored("movie.mp4", b"x");
        fixture.external = kind << 16;
        assert!(Zip::read(&mut Cursor::new(archive(&[fixture])), Limits::default()).is_err());
    }
    for id in [1u16, 0x000d, 0x0017, 0x0018, 0x7075, 0x9901] {
        let mut extra = id.to_le_bytes().to_vec();
        extra.extend_from_slice(&[0, 0]);
        let mut fixture = Fixture::stored("movie.mp4", b"x");
        fixture.extra = &extra;
        assert!(Zip::read(&mut Cursor::new(archive(&[fixture])), Limits::default()).is_err());
    }
    let original = stored_zip("movie.mp4", b"x");
    for flags in [1, 0x0040, 0x2000, 0x0010, 0x0020, 0x1000] {
        let mut bytes = original.clone();
        let start = directory_start(&bytes);
        put16(&mut bytes, 6, flags);
        put16(&mut bytes, start + 8, flags);
        assert!(Zip::read(&mut Cursor::new(bytes), Limits::default()).is_err());
    }
    for field in [20, 24, 42] {
        let mut bytes = original.clone();
        let start = directory_start(&bytes);
        put32(&mut bytes, start + field, u32::MAX);
        assert!(
            Zip::read(&mut Cursor::new(bytes), Limits::default())
                .unwrap_err()
                .contains("ZIP64")
        );
    }
}

#[test]
fn conflicting_headers_hidden_data_extra_fields_and_overlapping_entries_are_rejected() {
    let original = stored_zip("movie.mp4", b"x");
    for offset in [4, 6, 8, 10, 14, 18, 22, 26, 30] {
        let mut bytes = original.clone();
        bytes[offset] ^= 1;
        assert!(
            Zip::read(&mut Cursor::new(bytes), Limits::default()).is_err(),
            "offset {offset}"
        );
    }
    let mut trailing = original.clone();
    trailing.push(0);
    assert!(Zip::read(&mut Cursor::new(trailing), Limits::default()).is_err());
    let mut prefix = original.clone();
    prefix.insert(0, 0);
    let end = prefix.len() - 22;
    let start = directory_start(&original) + 1;
    put32(&mut prefix, end + 16, start as u32);
    put32(&mut prefix, start + 42, 1);
    assert!(
        Zip::read(&mut Cursor::new(prefix), Limits::default())
            .unwrap_err()
            .contains("hidden data")
    );
    let mut multi = archive(&[
        Fixture::stored("a.mp4", b"a"),
        Fixture::stored("b.mp4", b"b"),
    ]);
    let start = directory_start(&multi);
    let second = start + 46 + "a.mp4".len();
    put32(&mut multi, second + 42, 0);
    // Renaming the second central entry to the first also exposes path aliasing.
    assert!(Zip::read(&mut Cursor::new(multi), Limits::default()).is_err());
    for extra in [
        &[2, 0, 1][..],
        &[2, 0, 2, 0, 0][..],
        &[2, 0, 0, 0, 2, 0, 0, 0][..],
    ] {
        let mut fixture = Fixture::stored("movie.mp4", b"x");
        fixture.extra = extra;
        assert!(Zip::read(&mut Cursor::new(archive(&[fixture])), Limits::default()).is_err());
    }
}

#[test]
fn entry_total_count_and_compression_ratio_limits_apply_before_decoding() {
    let original = stored_zip("movie.mp4", b"1234567890");
    let small = Limits {
        max_entry_bytes: 9,
        ..Limits::default()
    };
    assert!(Zip::read(&mut Cursor::new(&original), small).is_err());
    let multi = archive(&[
        Fixture::stored("a.mp4", b"123456"),
        Fixture::stored("b.mp4", b"123456"),
    ]);
    let total = Limits {
        max_entry_bytes: 10,
        max_total_bytes: 11,
        ..Limits::default()
    };
    assert!(
        Zip::read(&mut Cursor::new(&multi), total)
            .unwrap_err()
            .contains("total byte")
    );
    let count = Limits {
        max_entries: 1,
        ..Limits::default()
    };
    assert!(Zip::read(&mut Cursor::new(multi), count).is_err());
    let expected = original_payload();
    let compressed = hex(include_str!("archive_support/dynamic.hex"));
    let mut fixture = Fixture::stored("original.bin", &expected);
    fixture.deflate = Some(&compressed);
    let ratio = Limits {
        max_ratio: 2,
        ..Limits::default()
    };
    assert!(
        Zip::read(&mut Cursor::new(archive(&[fixture])), ratio)
            .unwrap_err()
            .contains("ratio")
    );
    assert!(
        Limits {
            max_ratio: 0,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            max_total_bytes: u64::MAX,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn structural_changes_after_inspection_and_invalid_indices_write_nothing() {
    let mut bytes = stored_zip("movie.mp4", b"original");
    let zip = parsed(&bytes);
    let start = directory_start(&bytes);
    bytes[30] = b'M';
    bytes[start + 46] = b'M';
    let mut sink = Vec::new();
    assert!(
        zip.extract(
            &mut Cursor::new(&bytes),
            0,
            &mut sink,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("changed after inspection")
    );
    assert!(sink.is_empty());
    assert!(
        zip.extract(
            &mut Cursor::new(&bytes),
            1,
            &mut sink,
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert!(
        zip.extract(
            &mut Cursor::new(&bytes),
            0,
            &mut sink,
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .contains("cancelled")
    );
    assert!(sink.is_empty());
}

#[test]
fn empty_archives_and_directories_are_inspectable_without_payload_publication() {
    assert!(parsed(&archive(&[])).entries().is_empty());
    let mut directory = Fixture::stored("original/", b"");
    directory.external = (0o040700 << 16) | 0x10;
    let bytes = archive(&[directory, Fixture::stored("original/movie.mp4", b"media")]);
    let zip = parsed(&bytes);
    assert!(zip.entries()[0].is_directory());
    assert!(
        zip.extract(
            &mut Cursor::new(&bytes),
            0,
            &mut Vec::new(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert_eq!(extract(&bytes, 1).0, b"media");
}

struct FailReader;
impl Read for FailReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("original read failure"))
    }
}
struct FailWriter;
impl Write for FailWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("original write failure"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn input_and_output_errors_are_reported_without_a_completion_proof() {
    assert!(
        deflate::decode(
            &mut FailReader,
            2,
            &mut Vec::new(),
            10,
            1,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("original read failure")
    );
    let bytes = stored_zip("movie.mp4", b"x");
    let zip = parsed(&bytes);
    assert!(
        zip.extract(
            &mut Cursor::new(bytes),
            0,
            &mut FailWriter,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("original write failure")
    );
}

#[test]
fn cli_zip_inspection_is_metadata_only_and_creates_no_other_files() {
    let directory = Directory::new();
    let path = directory.0.join("original.zip");
    let bytes = stored_zip(
        "Original.Movie.2026.1080p.mp4",
        include_bytes!("../examples/demo.mp4"),
    );
    fs::write(&path, &bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .args(["zip-inspect", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json::parse(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(report.get("content_verified"), Some(&false.into()));
    assert_eq!(
        report.get("format").and_then(json::Value::as_str),
        Some("zip")
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    for args in [
        vec!["zip-inspect"],
        vec!["zip-inspect", path.to_str().unwrap(), "--apply"],
    ] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_mynou"))
                .current_dir(&directory.0)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    #[cfg(unix)]
    {
        let link = directory.0.join("linked.zip");
        std::os::unix::fs::symlink(path, &link).unwrap();
        assert!(Zip::read_file(&link, Limits::default()).is_err());
    }
}
