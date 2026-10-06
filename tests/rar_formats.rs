//! Original synthetic RAR5 structure and payload fixtures. CI execution only.
mod archive_support;
mod library_support;
mod rar_support;
use archive_support::{hex, reference_crc};
use library_support::Directory;
use mynou::{
    archive::{Limits, Method, Rar5},
    crypto::sha256,
    json::Value,
};
use rar_support::{Fixture, archive, block, file_body, header, stored, vint};
use std::{
    fs,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn read(bytes: &[u8]) -> mynou::Result<Rar5> {
    Rar5::read(&mut Cursor::new(bytes), Limits::default())
}

#[test]
fn original_stored_media_streams_exact_bytes_and_verifies_crc_and_sha() {
    let media = include_bytes!("../examples/demo.mp4");
    let bytes = archive(&[
        Fixture::directory("Original"),
        Fixture::stored("Original/Original.Movie.2026.1080p.mp4", media),
    ]);
    let rar = read(&bytes).unwrap();
    assert_eq!(rar.source_bytes(), bytes.len() as u64);
    assert_eq!(rar.declared_bytes(), media.len() as u64);
    assert_eq!(rar.entries().len(), 2);
    assert!(rar.entries()[0].is_directory());
    assert_eq!(rar.entries()[0].name(), "Original/");
    assert_eq!(rar.entries()[1].method(), Method::Stored);
    assert_eq!(rar.entries()[1].compressed_bytes(), media.len() as u64);
    assert_eq!(rar.limits(), Limits::default());
    let mut output = Vec::new();
    let proof = rar
        .extract(
            &mut Cursor::new(&bytes),
            1,
            &mut output,
            &AtomicBool::new(true),
        )
        .unwrap();
    assert_eq!(output, media);
    assert_eq!(proof.bytes(), media.len() as u64);
    assert_eq!(proof.crc32(), reference_crc(media));
    assert_eq!(proof.sha256(), &sha256(media));
    assert_eq!(
        rar.report().get("content_verified"),
        Some(&Value::Bool(false))
    );
    assert!(
        rar.extract(
            &mut Cursor::new(&bytes),
            0,
            &mut Vec::new(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert!(
        rar.extract(
            &mut Cursor::new(&bytes),
            2,
            &mut Vec::new(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
}

#[test]
fn an_independent_ascii_golden_archive_has_the_original_expected_payload() {
    let bytes = hex(include_str!("rar_support/original.hex"));
    let rar = read(&bytes).unwrap();
    assert_eq!(rar.entries()[0].name(), "original.txt");
    let mut output = Vec::new();
    let proof = rar
        .extract(
            &mut Cursor::new(&bytes),
            0,
            &mut output,
            &AtomicBool::new(true),
        )
        .unwrap();
    assert_eq!(output, b"Original RAR5 stored fixture.\n");
    assert_eq!(proof.sha256(), &sha256(&output));
}

#[test]
fn empty_archives_empty_files_and_checked_windows_attributes_are_supported() {
    assert!(read(&archive(&[])).unwrap().entries().is_empty());
    let mut entry = Fixture::stored("empty.txt", &[]);
    entry.host = 0;
    entry.attributes = 0x20;
    let bytes = archive(&[entry]);
    let rar = read(&bytes).unwrap();
    let proof = rar
        .extract(
            &mut Cursor::new(&bytes),
            0,
            &mut Vec::new(),
            &AtomicBool::new(true),
        )
        .unwrap();
    assert_eq!(proof.bytes(), 0);
    assert_eq!(proof.crc32(), 0);
    assert_eq!(proof.sha256(), &sha256(&[]));
}

#[test]
fn crc_covers_the_encoded_header_size_and_all_header_fields() {
    let bytes = stored("original.txt", b"Original");
    for offset in [8, 12, 13, 14, 15, 16, 20] {
        let mut bad = bytes.clone();
        bad[offset] ^= 1;
        assert!(read(&bad).is_err(), "{offset}");
    }
    let mut bytes = bytes;
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    assert!(read(&bytes).is_err());
}

#[test]
fn padded_header_sizes_and_fields_remain_bounded_and_crc_checked() {
    let mut bytes = rar_support::SIGNATURE.to_vec();
    let body = [0x81, 0x80, 0, 0, 0];
    let size = [0x85, 0x80, 0];
    let mut checked = size.to_vec();
    checked.extend_from_slice(&body);
    bytes.extend_from_slice(&reference_crc(&checked).to_le_bytes());
    bytes.extend(checked);
    bytes.extend(header(5, 0, &[0]));
    assert!(read(&bytes).unwrap().entries().is_empty());
    for malformed in [vec![0x80; 10], [vec![0x80; 9], vec![2]].concat()] {
        let mut fields = malformed;
        fields.extend_from_slice(&[0; 20]);
        let mut bytes = rar_support::SIGNATURE.to_vec();
        bytes.extend(header(1, 0, &fields));
        bytes.extend(header(5, 0, &[0]));
        assert!(read(&bytes).is_err());
    }
    let mut bytes = rar_support::SIGNATURE.to_vec();
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&[0x80; 4]);
    bytes.extend_from_slice(&[0; 16]);
    assert!(read(&bytes).unwrap_err().contains("three bytes"));
}

#[test]
fn unsupported_compression_versions_solid_streams_and_missing_crc_fail_explicitly() {
    for compression in [2, 0x40, 0x80, 0x280, 16 << 10, (24 << 10) | 1, 1 << 21] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.compression = compression;
        assert!(read(&archive(&[entry])).is_err(), "{compression}");
    }
    let mut entry = Fixture::stored("original.txt", b"Original");
    entry.file_flags = 0;
    assert!(read(&archive(&[entry])).unwrap_err().contains("CRC"));
    for compression in [1, 15 << 10, (23 << 10) | 1, 0x1f0000 | 1] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.compression = compression;
        assert!(read(&archive(&[entry])).is_ok(), "{compression}");
    }
}

#[test]
fn unsafe_duplicate_case_colliding_and_file_ancestor_names_are_rejected() {
    for name in [
        "",
        "../outside.txt",
        "/absolute.txt",
        "C:/outside.txt",
        "a\\b.txt",
        "a//b.txt",
        "a/./b.txt",
        "a/../b.txt",
        "CON.txt",
        "a/NUL.txt",
        "a/trailing. ",
        "a\0.txt",
        "mapped\u{fffe}.txt",
    ] {
        assert!(read(&stored(name, b"Original")).is_err(), "{name:?}");
    }
    for names in [
        ["same.txt", "same.txt"],
        ["Same.txt", "same.txt"],
        ["original", "original/child.txt"],
    ] {
        assert!(
            read(&archive(&[
                Fixture::stored(names[0], b"a"),
                Fixture::stored(names[1], b"b")
            ]))
            .is_err()
        );
    }
    assert!(
        read(&archive(&[
            Fixture::directory("Original"),
            Fixture::directory("original/")
        ]))
        .is_err()
    );
    for name in [
        "x".repeat(256),
        std::iter::repeat_n("a", 33).collect::<Vec<_>>().join("/"),
    ] {
        assert!(read(&stored(&name, b"Original")).is_err());
    }
}

#[test]
fn links_devices_host_unknown_size_and_directory_conflicts_are_rejected() {
    for (host, attributes) in [
        (1, 0o120600),
        (1, 0o020600),
        (1, 0o040700),
        (1, 0o104600),
        (0, 0x400),
        (0, 0x10),
        (2, 0),
    ] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.host = host;
        entry.attributes = attributes;
        assert!(read(&archive(&[entry])).is_err());
    }
    for flags in [8, 16, 0x80] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.file_flags |= flags;
        assert!(read(&archive(&[entry])).is_err());
    }
    for flags in [4, 8, 16, 32, 64] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.common_flags |= flags;
        assert!(read(&archive(&[entry])).is_err());
    }
    assert!(read(&stored("original/", b"Original")).is_err());
    let mut entry = Fixture::directory("original");
    entry.bytes = b"Original";
    assert!(read(&archive(&[entry])).is_err());
}

#[test]
fn main_end_header_order_service_encryption_volumes_and_trailing_data_are_rejected() {
    let empty = archive(&[]);
    for flags in [1, 2, 4, 8, 32] {
        let mut bytes = rar_support::SIGNATURE.to_vec();
        bytes.extend(header(1, 0, &vint(flags)));
        bytes.extend(header(5, 0, &[0]));
        assert!(read(&bytes).is_err());
    }
    let mut locked = rar_support::SIGNATURE.to_vec();
    locked.extend(header(1, 0, &[16]));
    locked.extend(header(5, 0, &[0]));
    assert!(read(&locked).is_ok());
    for kind in [1, 3, 4, 7] {
        let mut bytes = empty[..16].to_vec();
        bytes.extend(header(kind, 0, &[0; 10]));
        bytes.extend(header(5, 0, &[0]));
        assert!(read(&bytes).is_err());
    }
    for end in [vec![1], vec![0, 0]] {
        let mut bytes = empty[..16].to_vec();
        bytes.extend(header(5, 0, &end));
        assert!(read(&bytes).is_err());
    }
    let mut bytes = empty.clone();
    bytes.push(0);
    assert!(read(&bytes).is_err());
    let mut bytes = b"Original SFX".to_vec();
    bytes.extend(&empty);
    assert!(read(&bytes).is_err());
    let mut bytes = empty.clone();
    bytes[6] = 0;
    assert!(read(&bytes).is_err());
    let mut bytes = rar_support::SIGNATURE.to_vec();
    bytes.extend(header(5, 0, &[0]));
    bytes.extend_from_slice(&[0; 10]);
    assert!(read(&bytes).is_err());
}

#[test]
fn only_bounded_time_extras_are_accepted_without_affecting_payload_names() {
    let mut time = vint(3);
    time.extend(vint(0x13));
    time.extend_from_slice(&123456_u32.to_le_bytes());
    time.extend_from_slice(&999999999_u32.to_le_bytes());
    let mut record = vint(time.len() as u64);
    record.extend(&time);
    let mut entry = Fixture::stored("original.txt", b"Original");
    entry.common_flags |= 1;
    entry.extra = record.clone();
    entry.file_flags |= 2;
    assert!(read(&archive(&[entry])).is_ok());
    for kind in [1, 2, 4, 5, 6, 7, 127] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.common_flags |= 1;
        entry.extra = vec![1, kind];
        assert!(read(&archive(&[entry])).is_err());
    }
    let mut entry = Fixture::stored("original.txt", b"Original");
    entry.common_flags |= 1;
    entry.extra = [record.clone(), record].concat();
    assert!(read(&archive(&[entry])).unwrap_err().contains("duplicate"));
    let mut entry = Fixture::stored("original.txt", b"Original");
    entry.common_flags |= 1;
    entry.extra = vec![7, 3, 0x13, 0, 0, 0, 0, 0];
    assert!(read(&archive(&[entry])).is_err());
}

#[test]
fn size_entry_and_total_budgets_are_checked_before_payload_writes() {
    let bytes = archive(&[
        Fixture::stored("a.txt", b"Original"),
        Fixture::stored("b.txt", b"Original"),
    ]);
    for limits in [
        Limits {
            max_entries: 1,
            ..Limits::default()
        },
        Limits {
            max_entry_bytes: 7,
            ..Limits::default()
        },
        Limits {
            max_entry_bytes: 8,
            max_total_bytes: 15,
            ..Limits::default()
        },
    ] {
        assert!(Rar5::read(&mut Cursor::new(&bytes), limits).is_err());
    }
    for declared in [0, 7, 9, u64::MAX] {
        let mut entry = Fixture::stored("original.txt", b"Original");
        entry.declared = Some(declared);
        assert!(read(&archive(&[entry])).is_err(), "{declared}");
    }
    let limits = Limits {
        max_entries: 1,
        max_entry_bytes: 8,
        max_total_bytes: 8,
        ..Limits::default()
    };
    let bytes = stored("original.txt", b"Original");
    assert!(Rar5::read(&mut Cursor::new(&bytes), limits).is_ok());
}

struct Counted {
    input: Cursor<Vec<u8>>,
    reads: Arc<AtomicUsize>,
    reported: Option<u64>,
}
impl Read for Counted {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.input.read(bytes)?;
        self.reads.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}
impl Seek for Counted {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let position = self.input.seek(to)?;
        Ok(if to == SeekFrom::End(0) {
            self.reported.unwrap_or(position)
        } else {
            position
        })
    }
}

#[test]
fn inspection_skips_payload_bytes_and_extraction_never_reads_the_end_header() {
    let payload = vec![b'M'; 3 * 65536 + 7];
    let bytes = stored("original.txt", &payload);
    let reads = Arc::new(AtomicUsize::new(0));
    let mut input = Counted {
        input: Cursor::new(bytes.clone()),
        reads: reads.clone(),
        reported: None,
    };
    let rar = Rar5::read(&mut input, Limits::default()).unwrap();
    assert!(reads.load(Ordering::Relaxed) < 1024);
    let mut output = Vec::new();
    let proof = rar
        .extract(&mut input, 0, &mut output, &AtomicBool::new(true))
        .unwrap();
    assert_eq!(output, payload);
    assert_eq!(proof.sha256(), &sha256(&payload));
    assert_eq!(input.input.position(), bytes.len() as u64 - 8);
}

#[test]
fn oversized_header_claims_fail_before_any_body_allocation_or_read() {
    let mut bytes = rar_support::SIGNATURE.to_vec();
    bytes.extend(header(1, 0, &[0]));
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend(vint((1 << 21) - 1));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut input = Counted {
        input: Cursor::new(bytes),
        reads: reads.clone(),
        reported: Some((1 << 21) + 128),
    };
    assert!(
        Rar5::read(&mut input, Limits::default())
            .unwrap_err()
            .contains("metadata")
    );
    assert!(reads.load(Ordering::Relaxed) < 32);
}

#[test]
fn selected_truncations_invalid_utf8_and_unaccounted_header_bytes_are_rejected() {
    let bytes = stored("original.txt", b"Original");
    for prefix in 0..bytes.len() {
        assert!(read(&bytes[..prefix]).is_err(), "{prefix}");
    }
    let entry = Fixture::stored("original.txt", b"Original");
    let mut body = file_body(&entry);
    let last = body.len() - 1;
    body[last] = 0xff;
    let mut bytes = rar_support::SIGNATURE.to_vec();
    bytes.extend(header(1, 0, &[0]));
    bytes.extend(block(&body));
    bytes.extend_from_slice(entry.bytes);
    bytes.extend(header(5, 0, &[0]));
    assert!(read(&bytes).is_err());
    let mut body = file_body(&entry);
    body.push(0);
    let mut bytes = rar_support::SIGNATURE.to_vec();
    bytes.extend(header(1, 0, &[0]));
    bytes.extend(block(&body));
    bytes.extend_from_slice(entry.bytes);
    bytes.extend(header(5, 0, &[0]));
    assert!(read(&bytes).is_err());
}

#[test]
fn changed_headers_are_rejected_before_decoding_and_corrupt_payloads_never_return_proofs() {
    let bytes = stored("original.txt", b"Original");
    let rar = read(&bytes).unwrap();
    let mut output = Vec::new();
    let changed = stored("different.txt", b"Original");
    assert!(
        rar.extract(
            &mut Cursor::new(changed),
            0,
            &mut output,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("metadata changed")
    );
    assert!(output.is_empty());
    let mut corrupt = bytes.clone();
    let data = bytes.len() - 8 - b"Original".len();
    corrupt[data] ^= 1;
    assert!(
        rar.extract(
            &mut Cursor::new(corrupt),
            0,
            &mut output,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("payload CRC")
    );
    assert_eq!(output.len(), 8);
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
fn cancellation_at_the_final_write_cannot_return_a_verified_payload() {
    for payload in [b"Original".to_vec(), vec![b'M'; 65536 + 1]] {
        let bytes = stored("original.txt", &payload);
        let rar = read(&bytes).unwrap();
        let flag = AtomicBool::new(true);
        let mut output = CancelSink {
            flag: &flag,
            bytes: Vec::new(),
        };
        assert!(
            rar.extract(&mut Cursor::new(bytes), 0, &mut output, &flag)
                .is_err()
        );
        assert_eq!(output.bytes.len(), payload.len().min(65536));
    }
    let bytes = stored("original.txt", b"Original");
    assert!(
        Rar5::read_cancellable(
            &mut Cursor::new(&bytes),
            Limits::default(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    let rar = read(&bytes).unwrap();
    let mut output = Vec::new();
    assert!(
        rar.extract(
            &mut Cursor::new(bytes),
            0,
            &mut output,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(output.is_empty());
}

struct FailedSink;
impl Write for FailedSink {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("Original RAR write failure"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn sink_errors_cannot_publish_a_successful_payload_proof() {
    let bytes = stored("original.txt", b"Original");
    let rar = read(&bytes).unwrap();
    assert!(
        rar.extract(
            &mut Cursor::new(bytes),
            0,
            &mut FailedSink,
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .contains("Original RAR write failure")
    );
}

#[test]
fn rar_inspection_cli_is_metadata_only_and_has_no_creation_or_apply_route() {
    let d = Directory::new();
    let path = d.0.join("original.rar");
    let bytes = stored("original.txt", b"Original");
    fs::write(&path, &bytes).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&d.0)
        .args(["rar-inspect", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let v = mynou::json::parse(std::str::from_utf8(&result.stdout).unwrap()).unwrap();
    assert_eq!(v.get("format").and_then(Value::as_str), Some("rar5"));
    assert_eq!(v.get("content_verified"), Some(&Value::Bool(false)));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(&d.0).unwrap().count(), 1);
    for args in [
        vec!["rar-inspect"],
        vec!["rar-inspect", path.to_str().unwrap(), "--apply"],
        vec![
            "rar-inspect",
            path.to_str().unwrap(),
            "--config",
            "unknown.json",
        ],
    ] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_mynou"))
                .current_dir(&d.0)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    #[cfg(unix)]
    {
        let link = d.0.join("link.rar");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Rar5::read_file(&link, Limits::default()).is_err());
    }
}
