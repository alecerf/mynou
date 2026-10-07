//! Original synthetic PAR2 fixtures and published RFC 1321 vectors. CI only.
mod archive_support;
mod library_support;

use archive_support::{hex, reference_crc};
use library_support::Directory;
use mynou::{
    crypto::{Md5, md5, sha256},
    json::{self, Value},
    par2::{
        Limits, MultiRecoveryLimits, RecoveryInput, RecoveryLimits, Set, verify_directory,
        verify_directory_cancellable,
    },
};
use std::{
    fs,
    io::{self, Cursor, Read, Seek, SeekFrom},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

const MAIN: [u8; 16] = *b"PAR 2.0\0Main\0\0\0\0";
const DESCRIPTION: [u8; 16] = *b"PAR 2.0\0FileDesc";
const CHECKSUMS: [u8; 16] = *b"PAR 2.0\0IFSC\0\0\0\0";
const RECOVERY: [u8; 16] = *b"PAR 2.0\0RecvSlic";
const CREATOR: [u8; 16] = *b"PAR 2.0\0Creator\0";

fn padded(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    bytes
}

fn description(name: &str, data: &[u8]) -> Vec<u8> {
    let first = md5(&data[..data.len().min(16_384)]);
    let mut identity = first.to_vec();
    identity.extend_from_slice(&(data.len() as u64).to_le_bytes());
    identity.extend_from_slice(name.as_bytes());
    let mut body = md5(&identity).to_vec();
    body.extend_from_slice(&md5(data));
    body.extend_from_slice(&first);
    body.extend_from_slice(&(data.len() as u64).to_le_bytes());
    body.extend_from_slice(&padded(name.as_bytes().to_vec()));
    body
}

fn packet(kind: [u8; 16], set: [u8; 16], body: &[u8]) -> Vec<u8> {
    let mut checked = set.to_vec();
    checked.extend_from_slice(&kind);
    checked.extend_from_slice(body);
    let mut bytes = b"PAR2\0PKT".to_vec();
    bytes.extend_from_slice(&(64 + body.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&md5(&checked));
    bytes.extend_from_slice(&checked);
    bytes
}

#[derive(Clone)]
struct Fixture {
    packets: Vec<([u8; 16], Vec<u8>)>,
}

impl Fixture {
    fn recovery(data: &[u8], slice_bytes: usize, rows: u32) -> Self {
        Self::multi_recovery(&[("Original.bin", data)], 1, slice_bytes, rows)
    }

    fn multi_recovery(
        files: &[(&str, &[u8])],
        recoverable: u32,
        slice_bytes: usize,
        rows: u32,
    ) -> Self {
        let mut fixture = Self::files(files, recoverable, slice_bytes);
        fixture.packets.retain(|(kind, _)| *kind != RECOVERY);
        // Published primitive constants; parity uses the independent polynomial
        // long-division oracle below, never the production field implementation.
        let constants = [2u16, 4, 16, 128, 256, 2048, 8192, 16384, 4107, 32856, 17132];
        for exponent in 0..rows {
            let mut parity = vec![0u8; slice_bytes];
            for (index, slice) in files
                .iter()
                .take(recoverable as usize)
                .flat_map(|(_, data)| data.chunks(slice_bytes))
                .enumerate()
            {
                let mut padded = slice.to_vec();
                padded.resize(slice_bytes, 0);
                let mut factor = 1;
                for _ in 0..exponent {
                    factor = reference_multiply(factor, constants[index]);
                }
                for (target, source) in parity
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(padded.as_chunks::<2>().0)
                {
                    *target = (u16::from_le_bytes(*target)
                        ^ reference_multiply(u16::from_le_bytes(*source), factor))
                    .to_le_bytes();
                }
            }
            let mut body = exponent.to_le_bytes().to_vec();
            body.extend_from_slice(&parity);
            fixture.packets.push((RECOVERY, body));
        }
        fixture
    }

    fn files(files: &[(&str, &[u8])], recoverable: u32, slice_bytes: usize) -> Self {
        let mut main = (slice_bytes as u64).to_le_bytes().to_vec();
        main.extend_from_slice(&recoverable.to_le_bytes());
        let mut packets = Vec::new();
        for (name, data) in files {
            let body = description(name, data);
            main.extend_from_slice(&body[..16]);
            let mut checksums = body[..16].to_vec();
            for slice in data.chunks(slice_bytes) {
                let mut bytes = slice.to_vec();
                bytes.resize(slice_bytes, 0);
                checksums.extend_from_slice(&md5(&bytes));
                checksums.extend_from_slice(&reference_crc(&bytes).to_le_bytes());
            }
            packets.push((DESCRIPTION, body));
            packets.push((CHECKSUMS, checksums));
        }
        packets.push((MAIN, main));
        packets.push((CREATOR, padded(b"Mynou original fixture".to_vec())));
        let mut recovery = 42u32.to_le_bytes().to_vec();
        recovery.resize(4 + slice_bytes, 0);
        packets.push((RECOVERY, recovery));
        Self { packets }
    }

    fn small() -> Self {
        Self::files(&[("Original.Movie.2026.mp4", b"abcde")], 1, 4)
    }

    fn body_mut(&mut self, kind: [u8; 16]) -> &mut Vec<u8> {
        &mut self.packets.iter_mut().find(|p| p.0 == kind).unwrap().1
    }

    fn bytes_with_id(&self, set: [u8; 16]) -> Vec<u8> {
        self.packets
            .iter()
            .flat_map(|(kind, body)| packet(*kind, set, body))
            .collect()
    }

    fn bytes(&self) -> Vec<u8> {
        let set = self
            .packets
            .iter()
            .find(|p| p.0 == MAIN)
            .map_or([0; 16], |p| md5(&p.1));
        self.bytes_with_id(set)
    }
}

fn read(bytes: &[u8]) -> mynou::Result<Set> {
    Set::read(&mut Cursor::new(bytes), Limits::default())
}

fn reference_multiply(left: u16, right: u16) -> u16 {
    let mut product = 0u32;
    for bit in 0..16 {
        if right & (1 << bit) != 0 {
            product ^= u32::from(left) << bit;
        }
    }
    for bit in (16..32).rev() {
        if product & (1 << bit) != 0 {
            product ^= 0x1_100b << (bit - 16);
        }
    }
    product as u16
}

#[test]
fn multi_recovery_uses_global_coefficients_with_reordered_inputs_and_empty_files() {
    let first = b"abcdefghijklmn";
    let second = b"UVWXY";
    let source = Fixture::multi_recovery(
        &[
            ("Zulu.bin", first),
            ("Alpha.bin", second),
            ("Empty.bin", b""),
        ],
        3,
        4,
        6,
    )
    .bytes();
    let set = read(&source).unwrap();
    let mut damaged = first.to_vec();
    damaged[5] ^= 0x55;
    damaged[13] ^= 0xaa;
    let before = damaged.clone();
    let inputs = [
        RecoveryInput::new(*set.files()[2].id(), b""),
        RecoveryInput::new(*set.files()[1].id(), b""),
        RecoveryInput::new(*set.files()[0].id(), &damaged),
    ];
    let outputs = set
        .recover_files(
            &mut Cursor::new(&source),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert_eq!(outputs.len(), 3);
    for (index, expected) in [first.as_slice(), second.as_slice(), b""]
        .iter()
        .enumerate()
    {
        assert_eq!(outputs[index].id(), set.files()[index].id());
        assert_eq!(outputs[index].bytes(), *expected);
    }
    assert_eq!(damaged, before);
    assert_eq!(first, b"abcdefghijklmn");
    assert_eq!(second, b"UVWXY");
}

#[test]
fn multi_recovery_supports_eight_erasures_across_wholly_missing_files() {
    let first = b"abcdefghijklmnop";
    let second = b"ABCDEFGHIJKLMNOP";
    let source =
        Fixture::multi_recovery(&[("one.bin", first), ("two.bin", second)], 2, 4, 8).bytes();
    let set = read(&source).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[0].id(), b""),
        RecoveryInput::new(*set.files()[1].id(), b""),
    ];
    let outputs = set
        .recover_files(
            &mut Cursor::new(&source),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert_eq!(outputs[0].bytes(), first);
    assert_eq!(outputs[1].bytes(), second);
    let mut tighter = MultiRecoveryLimits::default();
    tighter.recovery.max_missing_slices = 7;
    assert!(
        set.recover_files(&mut Cursor::new(&source), &inputs, tighter)
            .is_err()
    );
}

#[test]
fn multi_recovery_rejects_ambiguous_identity_coverage_before_reading_source() {
    let source = Fixture::multi_recovery(
        &[
            ("one.bin", b"abcd"),
            ("two.bin", b"EFGH"),
            ("note.txt", b"spare"),
        ],
        2,
        4,
        2,
    )
    .bytes();
    let set = read(&source).unwrap();
    let first = RecoveryInput::new(*set.files()[0].id(), b"abcd");
    let second = RecoveryInput::new(*set.files()[1].id(), b"EFGH");
    let unrelated = RecoveryInput::new(*set.files()[2].id(), b"spare");
    for inputs in [
        vec![first],
        vec![first, first],
        vec![first, unrelated],
        vec![RecoveryInput::new([0; 16], b""), second],
        vec![first, second, unrelated],
        vec![RecoveryInput::new(*set.files()[0].id(), b"abcdef"), second],
    ] {
        let mut reader = Cursor::new(&source);
        assert!(
            set.recover_files(&mut reader, &inputs, MultiRecoveryLimits::default())
                .is_err()
        );
        assert_eq!(reader.position(), 0);
    }
    let outputs = set
        .recover_files(
            &mut Cursor::new(&source),
            &[second, first],
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].id(), set.files()[0].id());
    assert_eq!(outputs[1].id(), set.files()[1].id());
    assert!(
        set.recover_single(
            &mut Cursor::new(&source),
            b"abcd",
            RecoveryLimits::default()
        )
        .is_err()
    );
}

#[test]
fn multi_recovery_applies_content_slice_erasure_memory_and_work_budgets_to_the_set() {
    let source = Fixture::multi_recovery(
        &[("one.bin", b"abcdefghi"), ("two.bin", b"ABCDEFGHI")],
        2,
        8,
        4,
    )
    .bytes();
    let set = read(&source).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[0].id(), b""),
        RecoveryInput::new(*set.files()[1].id(), b""),
    ];
    let limits = MultiRecoveryLimits::default();
    for recovery in [
        RecoveryLimits {
            max_file_bytes: 17,
            ..limits.recovery
        },
        RecoveryLimits {
            max_slices: 3,
            ..limits.recovery
        },
        RecoveryLimits {
            max_missing_slices: 3,
            ..limits.recovery
        },
        RecoveryLimits {
            max_slice_bytes: 4,
            ..limits.recovery
        },
        RecoveryLimits {
            max_working_bytes: (512 << 10) + 8 + 9,
            ..limits.recovery
        },
        RecoveryLimits {
            max_field_operations: 1,
            ..limits.recovery
        },
    ] {
        assert!(
            set.recover_files(
                &mut Cursor::new(&source),
                &inputs,
                MultiRecoveryLimits { recovery, ..limits },
            )
            .is_err()
        );
    }
    for max_files in [0, 1, 9] {
        assert!(
            set.recover_files(
                &mut Cursor::new(&source),
                &inputs,
                MultiRecoveryLimits {
                    max_files,
                    ..limits
                },
            )
            .is_err()
        );
    }
    let nine: Vec<_> = (0..9)
        .map(|i| (format!("empty-{i}.bin"), b"".as_slice()))
        .collect();
    let names: Vec<_> = nine
        .iter()
        .map(|(name, data)| (name.as_str(), *data))
        .collect();
    let source = Fixture::multi_recovery(&names, 9, 4, 0).bytes();
    let set = read(&source).unwrap();
    let inputs: Vec<_> = set
        .files()
        .iter()
        .map(|f| RecoveryInput::new(*f.id(), b""))
        .collect();
    assert!(
        set.recover_files(&mut Cursor::new(&source), &inputs, limits)
            .is_err()
    );
}

#[test]
fn multi_recovery_requires_combined_consecutive_rows_and_mathematically_correct_parity() {
    let original =
        Fixture::multi_recovery(&[("one.bin", b"abcde"), ("two.bin", b"FGHIJK")], 2, 4, 4);
    let mut incorrect = original.clone();
    incorrect.body_mut(RECOVERY)[4] ^= 0x55;
    let mut insufficient = original;
    insufficient
        .packets
        .retain(|(kind, body)| *kind != RECOVERY || body[..4] != 1u32.to_le_bytes());
    for fixture in [incorrect, insufficient] {
        let source = fixture.bytes(); // Hash-valid packets do not certify parity.
        let set = read(&source).unwrap();
        let inputs: Vec<_> = set
            .files()
            .iter()
            .map(|f| RecoveryInput::new(*f.id(), b""))
            .collect();
        assert!(
            set.recover_files(
                &mut Cursor::new(&source),
                &inputs,
                MultiRecoveryLimits::default()
            )
            .is_err()
        );
    }
}

#[test]
fn multi_recovery_checks_large_chunks_truncated_prefixes_and_each_files_zero_padding() {
    let first = vec![0x37; 65_541];
    let second = vec![0xc9; 65_543];
    let source =
        Fixture::multi_recovery(&[("one.bin", &first), ("two.bin", &second)], 2, 65_540, 4).bytes();
    let set = read(&source).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[0].id(), &first[..65_530]),
        RecoveryInput::new(*set.files()[1].id(), b""),
    ];
    let outputs = set
        .recover_files(
            &mut Cursor::new(&source),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert_eq!(outputs[0].bytes(), first);
    assert_eq!(outputs[1].bytes(), second);
    assert_eq!(first, vec![0x37; 65_541]);
    assert_eq!(second, vec![0xc9; 65_543]);
}

#[test]
fn multi_recovery_returns_no_outputs_if_a_later_file_fails_whole_file_integrity() {
    let second = vec![0x7b; 16_385];
    let mut fixture =
        Fixture::multi_recovery(&[("one.bin", b"abcd"), ("two.bin", &second)], 2, 4096, 0);
    let body = &mut fixture
        .packets
        .iter_mut()
        .filter(|p| p.0 == DESCRIPTION)
        .nth(1)
        .unwrap()
        .1;
    body[16] ^= 1; // Whole-file MD5 is excluded from File ID; all slice checks still match.
    let source = fixture.bytes();
    let set = read(&source).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[0].id(), b"abcd"),
        RecoveryInput::new(*set.files()[1].id(), &second),
    ];
    assert!(
        set.recover_files(
            &mut Cursor::new(&source),
            &inputs,
            MultiRecoveryLimits::default()
        )
        .unwrap_err()
        .contains("file integrity")
    );
    assert_eq!(second, vec![0x7b; 16_385]);
}

#[test]
fn multi_recovery_rejects_source_changes_and_cancellation_without_partial_outputs() {
    let source =
        Fixture::multi_recovery(&[("one.bin", b"abcde"), ("two.bin", b"FGHIJK")], 2, 4, 4).bytes();
    let set = read(&source).unwrap();
    let inputs: Vec<_> = set
        .files()
        .iter()
        .map(|f| RecoveryInput::new(*f.id(), b""))
        .collect();
    let offset = set.recovery()[0].data_offset() - 68;
    for (start, end, byte) in [
        (Some(offset), 0, offset as usize + 16),
        (Some(offset), 0, offset as usize + 68),
        (None, 3, 80),
    ] {
        let mut reader = MutatedSource {
            cursor: Cursor::new(source.clone()),
            mutate_at_start: start,
            mutate_at_end: end,
            endings: 0,
            byte,
        };
        assert!(
            set.recover_files(&mut reader, &inputs, MultiRecoveryLimits::default())
                .is_err()
        );
    }
    let flag = AtomicBool::new(false);
    let mut reader = Cursor::new(&source);
    assert!(
        set.recover_files_cancellable(&mut reader, &inputs, MultiRecoveryLimits::default(), &flag)
            .is_err()
    );
    assert_eq!(reader.position(), 0);
    for grow in [false, true] {
        let flag = AtomicBool::new(true);
        let mut reader = Interrupted {
            cursor: Cursor::new(source.clone()),
            flag: &flag,
            grow,
            endings: 0,
        };
        assert!(
            set.recover_files_cancellable(
                &mut reader,
                &inputs,
                MultiRecoveryLimits::default(),
                &flag
            )
            .is_err()
        );
    }
}

#[test]
fn recovery_returns_verified_bytes_for_damage_truncation_and_missing_input() {
    let original = b"Original parity, not third-party bytes!";
    let fixture = Fixture::recovery(original, 8, 5);
    let source = fixture.bytes();
    let set = read(&source).unwrap();
    let limits = RecoveryLimits::default();
    assert_eq!(
        set.recover_single(&mut Cursor::new(&source), original, limits)
            .unwrap(),
        original
    );
    for indexes in [&[0usize][..], &[1, 3][..], &[0, 2, 4][..]] {
        let mut damaged = original.to_vec();
        for index in indexes {
            damaged[index * 8] ^= 0x5a;
        }
        let captured = damaged.clone();
        assert_eq!(
            set.recover_single(&mut Cursor::new(&source), &damaged, limits)
                .unwrap(),
            original
        );
        assert_eq!(damaged, captured);
    }
    for length in [0, 1, 7, 16, original.len() - 1] {
        assert_eq!(
            set.recover_single(&mut Cursor::new(&source), &original[..length], limits)
                .unwrap(),
            original
        );
    }
    let empty = Fixture::recovery(b"", 4, 0).bytes();
    assert!(
        read(&empty)
            .unwrap()
            .recover_single(&mut Cursor::new(&empty), b"", limits)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recovery_supports_eight_missing_slices_and_rejects_a_tighter_erasure_limit() {
    let original: [u8; 32] = std::array::from_fn(|index| index as u8);
    let source = Fixture::recovery(&original, 4, 8).bytes();
    let set = read(&source).unwrap();
    let limits = RecoveryLimits::default();
    assert_eq!(
        set.recover_single(&mut Cursor::new(&source), b"", limits)
            .unwrap(),
        original
    );
    assert!(
        set.recover_single(
            &mut Cursor::new(&source),
            b"",
            RecoveryLimits {
                max_missing_slices: 7,
                ..limits
            }
        )
        .is_err()
    );
}

#[test]
fn recovery_checks_large_slice_chunks_and_zero_padding_with_an_independent_oracle() {
    let original: Vec<u8> = (0..65_547).map(|index| (index % 251) as u8).collect();
    let source = Fixture::recovery(&original, 65_540, 2).bytes();
    let set = read(&source).unwrap();
    let mut damaged = original.clone();
    damaged[65_536] ^= 0xa5;
    *damaged.last_mut().unwrap() ^= 0x5a;
    let captured = damaged.clone();
    assert_eq!(
        set.recover_single(
            &mut Cursor::new(&source),
            &damaged,
            RecoveryLimits::default()
        )
        .unwrap(),
        original
    );
    assert_eq!(damaged, captured);
}

#[test]
fn recovery_requires_consecutive_rows_and_rejects_incorrect_parity() {
    let original = b"abcdefghijklmnopq";
    let mut fixture = Fixture::recovery(original, 8, 3);
    let limits = RecoveryLimits::default();
    fixture
        .packets
        .retain(|(kind, body)| *kind != RECOVERY || body[..4] != 1u32.to_le_bytes());
    let source = fixture.bytes();
    assert!(
        read(&source)
            .unwrap()
            .recover_single(&mut Cursor::new(&source), b"", limits)
            .is_err()
    );
    let mut fixture = Fixture::recovery(original, 8, 3);
    fixture.body_mut(RECOVERY)[4] ^= 1;
    let source = fixture.bytes(); // Valid packet hashes, mathematically wrong parity.
    assert!(
        read(&source)
            .unwrap()
            .recover_single(&mut Cursor::new(&source), b"", limits)
            .unwrap_err()
            .contains("integrity")
    );
    let multiple = Fixture::files(&[("a", b"abcd"), ("b", b"efgh")], 2, 4).bytes();
    assert!(
        read(&multiple)
            .unwrap()
            .recover_single(&mut Cursor::new(&multiple), b"abcd", limits)
            .is_err()
    );
}

#[test]
fn recovery_enforces_input_source_and_resource_boundaries() {
    let original = b"abcdefghijklmnopq";
    let fixture = Fixture::recovery(original, 8, 3);
    let source = fixture.bytes();
    let set = read(&source).unwrap();
    let limits = RecoveryLimits::default();
    let mut changed = source.clone();
    *changed.last_mut().unwrap() ^= 1;
    for bytes in [changed, source[..source.len() - 1].to_vec()] {
        assert!(
            set.recover_single(&mut Cursor::new(bytes), b"", limits)
                .is_err()
        );
    }
    for restricted in [
        RecoveryLimits {
            max_file_bytes: 16,
            ..limits
        },
        RecoveryLimits {
            max_slices: 2,
            ..limits
        },
        RecoveryLimits {
            max_missing_slices: 2,
            ..limits
        },
        RecoveryLimits {
            max_slice_bytes: 4,
            ..limits
        },
        RecoveryLimits {
            max_working_bytes: 1,
            ..limits
        },
        RecoveryLimits {
            max_field_operations: 1,
            ..limits
        },
        RecoveryLimits {
            max_missing_slices: 9,
            ..limits
        },
    ] {
        assert!(
            set.recover_single(&mut Cursor::new(&source), b"", restricted)
                .is_err()
        );
    }
    assert!(
        set.recover_single(&mut Cursor::new(&source), &[0; 18], limits)
            .is_err()
    );
    let flag = AtomicBool::new(false);
    let mut reader = Cursor::new(&source);
    assert!(
        set.recover_single_cancellable(&mut reader, b"", limits, &flag)
            .is_err()
    );
    assert_eq!(reader.position(), 0);
}

#[test]
fn recovery_checks_full_file_integrity_even_when_every_slice_matches() {
    let original = vec![b'x'; 16_385];
    let mut fixture = Fixture::recovery(&original, 4096, 0);
    fixture.body_mut(DESCRIPTION)[16] ^= 1;
    let source = fixture.bytes();
    assert!(
        read(&source)
            .unwrap()
            .recover_single(
                &mut Cursor::new(&source),
                &original,
                RecoveryLimits::default()
            )
            .unwrap_err()
            .contains("file integrity")
    );
}

struct MutatedSource {
    cursor: Cursor<Vec<u8>>,
    mutate_at_start: Option<u64>,
    mutate_at_end: u32,
    endings: u32,
    byte: usize,
}
impl Read for MutatedSource {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.cursor.read(bytes)
    }
}
impl Seek for MutatedSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if from == SeekFrom::End(0) {
            self.endings += 1;
        }
        let change = self
            .mutate_at_start
            .is_some_and(|offset| from == SeekFrom::Start(offset))
            || (from == SeekFrom::End(0) && self.endings == self.mutate_at_end);
        if change {
            self.cursor.get_mut()[self.byte] ^= 1;
            self.mutate_at_start = None;
        }
        self.cursor.seek(from)
    }
}

#[test]
fn recovery_rejects_source_mutation_between_capture_hash_payload_and_return() {
    let source = Fixture::recovery(b"abcdefghijklmnopq", 8, 3).bytes();
    let set = read(&source).unwrap();
    let offset = set.recovery()[0].data_offset() - 68;
    for byte in [offset as usize + 16, offset as usize + 68] {
        let mut reader = MutatedSource {
            cursor: Cursor::new(source.clone()),
            mutate_at_start: Some(offset),
            mutate_at_end: 0,
            endings: 0,
            byte,
        };
        assert!(
            set.recover_single(&mut reader, b"", RecoveryLimits::default())
                .unwrap_err()
                .contains("changed")
        );
    }
    let mut reader = MutatedSource {
        cursor: Cursor::new(source),
        mutate_at_start: None,
        mutate_at_end: 3, // Beginning of the final whole-source identity scan.
        endings: 0,
        byte: 80,
    };
    assert!(
        set.recover_single(&mut reader, b"", RecoveryLimits::default())
            .unwrap_err()
            .contains("identity changed")
    );
}

#[test]
fn recovery_cancellation_and_source_growth_during_reads_return_no_output() {
    let source = Fixture::recovery(b"abcdefghijklmnopq", 8, 3).bytes();
    let set = read(&source).unwrap();
    for grow in [false, true] {
        let flag = AtomicBool::new(true);
        let mut reader = Interrupted {
            cursor: Cursor::new(source.clone()),
            flag: &flag,
            grow,
            endings: 0,
        };
        assert!(
            set.recover_single_cancellable(&mut reader, b"", RecoveryLimits::default(), &flag)
                .is_err()
        );
    }
}

#[test]
fn published_rfc_1321_vectors_match_one_shot_and_streamed_boundaries() {
    let vectors = [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("a", "0cc175b9c0f1b6a831c399e269772661"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "c3fcd3d76192e4007dfb496cca67e13b",
        ),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "d174ab98d277d9f5a5611c2c9f419d9f",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ];
    for (input, expected) in vectors {
        assert_eq!(md5(input.as_bytes()).to_vec(), hex(expected));
        for size in [1, 7, 31, 55, 56, 63, 64, 65] {
            let mut digest = Md5::default();
            for chunk in input.as_bytes().chunks(size) {
                digest.update(chunk);
                digest.update(&[]);
            }
            assert_eq!(digest.finalize().to_vec(), hex(expected));
        }
    }
}

#[test]
fn original_md5_streaming_matches_the_known_million_a_digest() {
    let mut digest = Md5::new();
    for _ in 0..1000 {
        digest.update(&[b'a'; 1000]);
    }
    assert_eq!(
        digest.finalize().to_vec(),
        hex("7707d6ae4e027c70eea2a935c2296f21")
    );
}

#[test]
fn complete_unordered_packets_join_original_file_and_recovery_metadata() {
    let fixture = Fixture::small();
    let bytes = fixture.bytes();
    let set = read(&bytes).unwrap();
    assert_eq!(set.source_bytes(), bytes.len() as u64);
    assert_eq!(set.source_sha256(), &sha256(&bytes));
    assert!(set.set_id_matches_main());
    assert_eq!(set.limits(), Limits::default());
    assert_eq!(set.slice_bytes(), 4);
    assert_eq!(set.files()[0].name(), "Original.Movie.2026.mp4");
    assert_eq!(set.files()[0].bytes(), 5);
    assert_eq!(set.files()[0].md5(), &md5(b"abcde"));
    assert_eq!(set.files()[0].first_16k_md5(), &md5(b"abcde"));
    assert!(set.files()[0].is_recoverable());
    assert_eq!(set.files()[0].slices().len(), 2);
    assert_eq!(set.files()[0].slices()[1].md5(), &md5(b"e\0\0\0"));
    assert_eq!(
        set.files()[0].slices()[1].crc32(),
        reference_crc(b"e\0\0\0")
    );
    assert_eq!(set.recovery()[0].exponent(), 42);
    assert_eq!(set.recovery()[0].bytes(), 4);
    assert_eq!(set.recovery()[0].sha256(), &sha256(&[0; 4]));
    let offset = set.recovery()[0].data_offset() as usize;
    assert_eq!(&bytes[offset..offset + 4], &[0; 4]);
    let report = set.report();
    assert_eq!(
        report.get("packet_checksums_verified"),
        Some(&Value::Bool(true))
    );
    assert_eq!(report.get("content_verified"), Some(&Value::Bool(false)));
    assert_eq!(report.get("repair_supported"), Some(&Value::Bool(false)));
    assert_eq!(set.creators(), &["Mynou original fixture"]);
    assert_eq!(set.packet_count(), 5);
    assert_eq!(set.ignored_packets(), 0);
}

#[test]
fn main_order_nonrecoverable_files_and_empty_files_remain_explicit() {
    let mut fixture = Fixture::files(&[("z.mp4", b"abcd"), ("a.txt", b"")], 1, 4);
    fixture
        .packets
        .retain(|p| p.0 != CHECKSUMS || p.1.len() > 16);
    fixture.packets.reverse();
    let set = read(&fixture.bytes()).unwrap();
    assert_eq!(set.files()[0].name(), "z.mp4");
    assert_eq!(set.files()[1].name(), "a.txt");
    assert!(!set.files()[1].is_recoverable());
    assert!(set.files()[1].slices().is_empty());
    let empty = Fixture::files(&[("empty.txt", b"")], 1, 4);
    assert!(read(&empty.bytes()).unwrap().files()[0].slices().is_empty());
}

#[test]
fn identical_packets_deduplicate_without_discarding_source_accounting() {
    let mut fixture = Fixture::small();
    fixture.packets.extend(fixture.packets.clone());
    let set = read(&fixture.bytes()).unwrap();
    assert_eq!(set.files().len(), 1);
    assert_eq!(set.recovery().len(), 1);
    assert_eq!(set.creators().len(), 1);
    assert_eq!(set.packet_count(), 10);
}

#[test]
fn bad_magic_truncation_lengths_and_checksums_fail_closed() {
    let valid = Fixture::small().bytes();
    for length in [0, 1, 63, 64, valid.len() - 1] {
        assert!(read(&valid[..length]).is_err());
    }
    for index in [0, 16, 32, 48, 64, valid.len() - 1] {
        let mut bytes = valid.clone();
        bytes[index] ^= 1;
        assert!(read(&bytes).is_err());
    }
    for length in [0u64, 63, 65, u64::MAX] {
        let mut bytes = valid.clone();
        bytes[8..16].copy_from_slice(&length.to_le_bytes());
        assert!(read(&bytes).is_err());
    }
}

#[test]
fn checksum_valid_wrong_set_and_main_identifiers_are_rejected() {
    let fixture = Fixture::small();
    let error = read(&fixture.bytes_with_id([7; 16])).unwrap_err();
    assert!(error.contains("set identifier"), "{error}");
    let mut bytes = fixture.bytes();
    let first_len = 64 + fixture.packets[0].1.len();
    bytes[..first_len].copy_from_slice(&packet(
        fixture.packets[0].0,
        [7; 16],
        &fixture.packets[0].1,
    ));
    assert!(
        read(&bytes)
            .unwrap_err()
            .contains("different recovery sets")
    );
}

#[test]
fn checksum_valid_file_identity_short_hash_and_padding_errors_are_rejected() {
    for offset in [0, 16, 32, 48] {
        let mut fixture = Fixture::small();
        fixture.body_mut(DESCRIPTION)[offset] ^= 1;
        assert!(read(&fixture.bytes()).is_err());
    }
    let mut fixture = Fixture::files(&[("x", b"a")], 1, 4);
    *fixture.body_mut(DESCRIPTION).last_mut().unwrap() = b'!';
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    fixture.body_mut(CREATOR).extend_from_slice(&[0; 4]);
    assert!(read(&fixture.bytes()).is_err());
}

#[test]
fn unsafe_nonascii_and_aliased_file_names_never_become_paths() {
    for name in [
        "../x",
        "/absolute",
        "C:/drive",
        "a\\b",
        "a//b",
        "a/./b",
        "a/",
        "a\n",
        "café",
    ] {
        assert!(
            read(&Fixture::files(&[(name, b"abcd")], 1, 4).bytes()).is_err(),
            "{name}"
        );
    }
    for names in [["a.mp4", "A.mp4"], ["a", "a/b.mp4"]] {
        let fixture = Fixture::files(&[(names[0], b"abcd"), (names[1], b"abcd")], 2, 4);
        assert!(read(&fixture.bytes()).is_err());
    }
}

#[test]
fn missing_or_orphan_packets_cannot_claim_a_complete_set() {
    for kind in [MAIN, CREATOR, DESCRIPTION, CHECKSUMS] {
        let mut fixture = Fixture::small();
        fixture.packets.retain(|p| p.0 != kind);
        assert!(read(&fixture.bytes()).is_err());
    }
    let mut fixture = Fixture::small();
    let mut orphan = fixture.body_mut(CHECKSUMS).clone();
    orphan[0] ^= 1;
    fixture.packets.push((CHECKSUMS, orphan));
    assert!(read(&fixture.bytes()).is_err());
}

#[test]
fn conflicting_description_checksum_creator_and_recovery_semantics_fail() {
    for kind in [DESCRIPTION, CHECKSUMS, RECOVERY] {
        let mut fixture = Fixture::small();
        let mut conflicting = fixture.body_mut(kind).clone();
        if kind == DESCRIPTION {
            conflicting[16] ^= 1;
            conflicting[32] ^= 1;
        } else {
            *conflicting.last_mut().unwrap() ^= 1;
        }
        fixture.packets.push((kind, conflicting));
        assert!(read(&fixture.bytes()).is_err());
    }
    let mut fixture = Fixture::small();
    fixture.body_mut(CREATOR)[0] = 0;
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    fixture.body_mut(RECOVERY)[..4].copy_from_slice(&65535u32.to_le_bytes());
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    fixture.body_mut(RECOVERY).extend_from_slice(&[0; 4]);
    assert!(read(&fixture.bytes()).is_err());
}

#[test]
fn main_counts_slice_size_and_checksum_joins_are_strict() {
    for size in [0u64, 3, (1 << 20) + 4] {
        let mut fixture = Fixture::small();
        fixture.body_mut(MAIN)[..8].copy_from_slice(&size.to_le_bytes());
        assert!(read(&fixture.bytes()).is_err());
    }
    let mut fixture = Fixture::small();
    fixture.body_mut(MAIN)[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    let id = fixture.body_mut(MAIN)[12..28].to_vec();
    fixture.body_mut(MAIN).extend_from_slice(&id);
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    fixture.body_mut(CHECKSUMS).truncate(36);
    assert!(read(&fixture.bytes()).is_err());
}

#[test]
fn unknown_standard_semantics_fail_but_verified_private_packets_are_counted() {
    let mut fixture = Fixture::small();
    fixture.packets.push((*b"PAR 2.0\0Unknown\0", vec![0; 4]));
    assert!(read(&fixture.bytes()).is_err());
    let mut fixture = Fixture::small();
    fixture.packets.push((*b"MYNOU-PRIVATE-v1", vec![0; 4]));
    assert_eq!(read(&fixture.bytes()).unwrap().ignored_packets(), 1);
}

#[test]
fn independent_resource_limits_fail_without_unbounded_allocation() {
    let bytes = Fixture::small().bytes();
    let limits = [
        Limits {
            max_source_bytes: 64,
            ..Limits::default()
        },
        Limits {
            max_packets: 1,
            ..Limits::default()
        },
        Limits {
            max_slices: 1,
            ..Limits::default()
        },
        Limits {
            max_metadata_bytes: 64,
            ..Limits::default()
        },
        Limits {
            max_recovery_bytes: 0,
            ..Limits::default()
        },
        Limits {
            max_file_bytes: 4,
            ..Limits::default()
        },
    ];
    for limits in limits {
        assert!(Set::read(&mut Cursor::new(&bytes), limits).is_err());
    }
    let two = Fixture::files(&[("a", b"abcd"), ("b", b"abcd")], 2, 4).bytes();
    for limits in [
        Limits {
            max_files: 1,
            ..Limits::default()
        },
        Limits {
            max_file_bytes: 4,
            max_total_file_bytes: 4,
            ..Limits::default()
        },
    ] {
        assert!(Set::read(&mut Cursor::new(&two), limits).is_err());
    }
    assert!(
        Limits {
            max_slice_bytes: 3,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            max_packets: 0,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
}

struct Interrupted<'a> {
    cursor: Cursor<Vec<u8>>,
    flag: &'a AtomicBool,
    grow: bool,
    endings: u32,
}
impl Read for Interrupted<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let read = self.cursor.read(bytes)?;
        if !self.grow {
            self.flag.store(false, Ordering::Release);
        }
        Ok(read)
    }
}
impl Seek for Interrupted<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if from == SeekFrom::End(0) {
            self.endings += 1;
            if self.grow && self.endings > 1 {
                self.cursor.get_mut().push(0);
            }
        }
        self.cursor.seek(from)
    }
}

#[test]
fn cancellation_before_and_during_reads_and_source_growth_fail_closed() {
    let bytes = Fixture::small().bytes();
    assert!(
        Set::read_cancellable(
            &mut Cursor::new(&bytes),
            Limits::default(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    for grow in [false, true] {
        let flag = AtomicBool::new(true);
        let mut reader = Interrupted {
            cursor: Cursor::new(bytes.clone()),
            flag: &flag,
            grow,
            endings: 0,
        };
        assert!(Set::read_cancellable(&mut reader, Limits::default(), &flag).is_err());
    }
}

#[test]
fn recovery_payloads_stream_across_chunks_and_sha_matches_without_retention() {
    let mut fixture = Fixture::files(&[("large.mp4", b"abcd")], 1, 1 << 17);
    let data = vec![0x5a; 1 << 17];
    fixture.body_mut(RECOVERY)[4..].copy_from_slice(&data);
    let set = read(&fixture.bytes()).unwrap();
    assert_eq!(set.recovery()[0].sha256(), &sha256(&data));
    assert_eq!(set.recovery()[0].bytes(), 1 << 17);
}

#[test]
fn cli_inspection_is_read_only_and_never_opens_declared_media() {
    let directory = Directory::new();
    let path = directory.0.join("original.par2");
    let bytes = Fixture::small().bytes();
    fs::write(&path, &bytes).unwrap();
    let before = fs::read_dir(&directory.0).unwrap().count();
    let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .current_dir(&directory.0)
        .args(["par2-inspect", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json::parse(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap();
    assert_eq!(
        report.get("format").and_then(Value::as_str),
        Some("par2-core")
    );
    assert_eq!(report.get("repair_supported"), Some(&Value::Bool(false)));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), before);
    fs::write(&path, b"corrupt").unwrap();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_mynou"))
            .current_dir(&directory.0)
            .args(["par2-inspect", path.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(&path).unwrap(), b"corrupt");
    assert!(Set::read_file(&directory.0, Limits::default()).is_err());
}

#[cfg(unix)]
#[test]
fn cli_input_symbolic_links_are_rejected_without_writes() {
    let directory = Directory::new();
    let source = directory.0.join("original.par2");
    fs::write(&source, Fixture::small().bytes()).unwrap();
    let link = directory.0.join("link.par2");
    std::os::unix::fs::symlink(&source, &link).unwrap();
    assert!(Set::read_file(&link, Limits::default()).is_err());
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn verification_reports_main_order_global_damage_without_using_parity() {
    let bytes = Fixture::files(
        &[("first.bin", b"abcde"), ("second.bin", b"123456789")],
        2,
        4,
    )
    .bytes();
    let set = read(&bytes).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[1].id(), b"1234XXXX9"),
        RecoveryInput::new(*set.files()[0].id(), b"abcde"),
    ];
    let report = set
        .verify_files(
            &mut Cursor::new(&bytes),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert!(!report.content_verified());
    assert!(report.files()[0].content_verified());
    assert_eq!(report.files()[1].damaged_slices(), &[(1, 3)]);
    assert_eq!(report.files()[0].id(), set.files()[0].id());
    for key in [
        "repair_supported",
        "parity_verified",
        "ownership_verified",
        "snapshot_guaranteed",
    ] {
        assert_eq!(report.to_json().get(key), Some(&Value::Bool(false)));
    }
}

#[test]
fn verification_distinguishes_truncated_slices_and_all_missing_content() {
    let bytes = Fixture::files(
        &[("first.bin", b"abcdefgh"), ("second.bin", b"12345")],
        2,
        4,
    )
    .bytes();
    let set = read(&bytes).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[0].id(), b"abcde"),
        RecoveryInput::new(*set.files()[1].id(), b""),
    ];
    let report = set
        .verify_files(
            &mut Cursor::new(&bytes),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert_eq!(report.files()[0].damaged_slices(), &[(1, 1)]);
    assert_eq!(report.files()[1].damaged_slices(), &[(0, 2), (1, 3)]);
    assert!(!report.files()[0].content_verified());
    assert!(!report.files()[1].content_verified());
}

#[test]
fn verification_handles_empty_and_nonrecoverable_descriptions() {
    let bytes = Fixture::files(
        &[
            ("empty.bin", b""),
            ("data.bin", b"abcd"),
            ("note.txt", b"x"),
        ],
        2,
        4,
    )
    .bytes();
    let set = read(&bytes).unwrap();
    let inputs = [
        RecoveryInput::new(*set.files()[1].id(), b"abcd"),
        RecoveryInput::new(*set.files()[0].id(), b""),
    ];
    let report = set
        .verify_files(
            &mut Cursor::new(&bytes),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert!(report.content_verified());
    assert_eq!(report.files().len(), 2);
    assert!(report.files()[0].damaged_slices().is_empty());
    assert_eq!(
        report.files()[0].to_json().get("present"),
        Some(&Value::Null)
    );
}

#[test]
fn verification_never_hides_whole_file_disagreement_behind_good_slices() {
    let original = vec![0x5a; 18_000];
    let mut fixture = Fixture::files(&[("long.bin", &original)], 1, 1024);
    fixture.body_mut(DESCRIPTION)[16] ^= 1;
    let bytes = fixture.bytes();
    let set = read(&bytes).unwrap();
    let inputs = [RecoveryInput::new(*set.files()[0].id(), &original)];
    let report = set
        .verify_files(
            &mut Cursor::new(&bytes),
            &inputs,
            MultiRecoveryLimits::default(),
        )
        .unwrap();
    assert!(report.files()[0].damaged_slices().is_empty());
    assert!(!report.content_verified());
    assert_eq!(
        report.files()[0].to_json().get("md5_matches"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        report.files()[0].to_json().get("first_16k_md5_matches"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn verification_rejects_bad_input_coverage_and_budgets_before_source_reads() {
    let bytes = Fixture::files(&[("a.bin", b"abcdefgh"), ("b.bin", b"12345")], 2, 4).bytes();
    let set = read(&bytes).unwrap();
    let a = RecoveryInput::new(*set.files()[0].id(), b"abcdefgh");
    let b = RecoveryInput::new(*set.files()[1].id(), b"12345");
    for inputs in [
        vec![],
        vec![a],
        vec![a, a],
        vec![a, RecoveryInput::new([0; 16], b"")],
        vec![
            RecoveryInput::new(*set.files()[0].id(), b"too many bytes"),
            b,
        ],
    ] {
        assert!(
            set.verify_files(
                &mut Cursor::new([]),
                &inputs,
                MultiRecoveryLimits::default()
            )
            .is_err()
        );
    }
    let mut limits = MultiRecoveryLimits {
        max_files: 1,
        ..MultiRecoveryLimits::default()
    };
    assert!(
        set.verify_files(&mut Cursor::new([]), &[a, b], limits)
            .is_err()
    );
    limits = MultiRecoveryLimits::default();
    limits.recovery.max_file_bytes = 12;
    assert!(
        set.verify_files(&mut Cursor::new([]), &[a, b], limits)
            .is_err()
    );
    limits = MultiRecoveryLimits::default();
    limits.recovery.max_slices = 3;
    assert!(
        set.verify_files(&mut Cursor::new([]), &[a, b], limits)
            .is_err()
    );
    limits = MultiRecoveryLimits::default();
    limits.recovery.max_working_bytes = 4;
    assert!(
        set.verify_files(&mut Cursor::new([]), &[a, b], limits)
            .is_err()
    );
}

#[test]
fn verification_can_diagnose_more_damage_than_the_recovery_erasure_budget() {
    let original = [0x5a; 40];
    let bytes = Fixture::files(&[("ten-slices.bin", &original)], 1, 4).bytes();
    let set = read(&bytes).unwrap();
    let inputs = [RecoveryInput::new(*set.files()[0].id(), b"")];
    let mut limits = MultiRecoveryLimits::default();
    limits.recovery.max_missing_slices = 0;
    let report = set
        .verify_files(&mut Cursor::new(&bytes), &inputs, limits)
        .unwrap();
    assert_eq!(report.files()[0].damaged_slices().len(), 10);
    assert!(!report.content_verified());
}

#[test]
fn verification_rejects_changed_source_and_cancelled_reads() {
    let bytes = Fixture::small().bytes();
    let set = read(&bytes).unwrap();
    let inputs = [RecoveryInput::new(*set.files()[0].id(), b"abcde")];
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() ^= 1;
    assert!(
        set.verify_files(
            &mut Cursor::new(changed),
            &inputs,
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    let flag = AtomicBool::new(false);
    assert!(
        set.verify_files_cancellable(
            &mut Cursor::new(&bytes),
            &inputs,
            MultiRecoveryLimits::default(),
            &flag
        )
        .is_err()
    );
    flag.store(true, Ordering::Release);
    let mut interrupted = Interrupted {
        cursor: Cursor::new(bytes),
        flag: &flag,
        grow: false,
        endings: 0,
    };
    assert!(
        set.verify_files_cancellable(
            &mut interrupted,
            &inputs,
            MultiRecoveryLimits::default(),
            &flag
        )
        .is_err()
    );
}

#[test]
fn directory_verification_preserves_inputs_and_reports_missing_empty_files() {
    let directory = Directory::new();
    let source = directory.0.join("set.par2");
    let bytes = Fixture::files(&[("empty.bin", b""), ("nested/data.bin", b"abcde")], 2, 4).bytes();
    fs::write(&source, &bytes).unwrap();
    fs::create_dir(directory.0.join("nested")).unwrap();
    fs::write(directory.0.join("nested/data.bin"), b"abcde").unwrap();
    let report = verify_directory(
        &source,
        &directory.0,
        Limits::default(),
        MultiRecoveryLimits::default(),
    )
    .unwrap();
    assert!(!report.content_verified());
    assert_eq!(
        report.files()[0].to_json().get("present"),
        Some(&Value::Bool(false))
    );
    assert!(report.files()[0].damaged_slices().is_empty());
    fs::write(directory.0.join("empty.bin"), b"").unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .unwrap()
        .content_verified()
    );
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(
        fs::read(directory.0.join("nested/data.bin")).unwrap(),
        b"abcde"
    );
    assert!(!directory.0.join("mynou.json").exists());
    assert!(!directory.0.join("state").exists());
}

#[test]
fn directory_verification_does_not_create_missing_paths_and_rejects_types_and_bounds() {
    let directory = Directory::new();
    let source = directory.0.join("set.par2");
    let bytes = Fixture::files(&[("missing/data.bin", b"abcde")], 1, 4).bytes();
    fs::write(&source, &bytes).unwrap();
    let report = verify_directory(
        &source,
        &directory.0,
        Limits::default(),
        MultiRecoveryLimits::default(),
    )
    .unwrap();
    assert_eq!(report.files()[0].damaged_slices(), &[(0, 0), (1, 1)]);
    assert!(!directory.0.join("missing").exists());
    assert!(
        verify_directory(
            &source,
            &directory.0.join("absent"),
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    fs::create_dir(directory.0.join("missing")).unwrap();
    fs::create_dir(directory.0.join("missing/data.bin")).unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    fs::remove_dir(directory.0.join("missing/data.bin")).unwrap();
    fs::write(directory.0.join("missing/data.bin"), b"oversized").unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    let mut limits = MultiRecoveryLimits::default();
    limits.recovery.max_file_bytes = 4;
    assert!(verify_directory(&source, &directory.0, Limits::default(), limits).is_err());
    assert!(
        verify_directory_cancellable(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(
        fs::read(directory.0.join("missing/data.bin")).unwrap(),
        b"oversized"
    );
}

#[cfg(unix)]
#[test]
fn directory_verification_rejects_links_in_source_root_and_protected_ancestors() {
    use std::os::unix::fs::symlink;
    let directory = Directory::new();
    let source = directory.0.join("set.par2");
    fs::write(
        &source,
        Fixture::files(&[("nested/data.bin", b"abcde")], 1, 4).bytes(),
    )
    .unwrap();
    let actual = directory.0.join("actual");
    fs::create_dir(&actual).unwrap();
    fs::write(actual.join("data.bin"), b"abcde").unwrap();
    symlink(&actual, directory.0.join("nested")).unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    fs::remove_file(directory.0.join("nested")).unwrap();
    fs::create_dir(directory.0.join("nested")).unwrap();
    symlink(actual.join("data.bin"), directory.0.join("nested/data.bin")).unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    symlink(&source, directory.0.join("link.par2")).unwrap();
    assert!(
        verify_directory(
            &directory.0.join("link.par2"),
            &directory.0,
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    symlink(&directory.0, directory.0.join("root-link")).unwrap();
    assert!(
        verify_directory(
            &source,
            &directory.0.join("root-link"),
            Limits::default(),
            MultiRecoveryLimits::default()
        )
        .is_err()
    );
    assert_eq!(fs::read(actual.join("data.bin")).unwrap(), b"abcde");
}

#[test]
fn cli_verification_outputs_diagnosis_and_fails_on_damage_without_initialization() {
    let directory = Directory::new();
    let source = directory.0.join("set.par2");
    let bytes = Fixture::small().bytes();
    fs::write(&source, &bytes).unwrap();
    let protected = directory.0.join("Original.Movie.2026.mp4");
    for (input, clean) in [(b"abcde".as_slice(), true), (b"XXXXX".as_slice(), false)] {
        fs::write(&protected, input).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
            .current_dir(&directory.0)
            .args([
                "par2-verify",
                source.to_str().unwrap(),
                "--root",
                directory.0.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            clean,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json::parse(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap();
        assert_eq!(report.get("content_verified"), Some(&Value::Bool(clean)));
        assert_eq!(fs::read(&protected).unwrap(), input);
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
    }
    for options in [
        vec![],
        vec!["--config", "forbidden.json"],
        vec!["--root", "missing"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mynou"))
            .current_dir(&directory.0)
            .args(["par2-verify", source.to_str().unwrap()])
            .args(options)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!directory.0.join("forbidden.json").exists());
}

#[test]
fn verification_budgets_live_source_hash_and_report_overhead() {
    let bytes = Fixture::files(&[("padded.bin", b"abcde")], 1, 1 << 20).bytes();
    let set = read(&bytes).unwrap();
    let inputs = [RecoveryInput::new(*set.files()[0].id(), b"abcde")];
    let tight = MultiRecoveryLimits {
        recovery: RecoveryLimits {
            max_working_bytes: (1 << 20) + (64 << 10),
            ..RecoveryLimits::default()
        },
        ..MultiRecoveryLimits::default()
    };
    let error = set
        .verify_files(&mut Cursor::new(&bytes), &inputs, tight)
        .unwrap_err();
    assert!(error.contains("scratch limits"));
    let allowed = MultiRecoveryLimits {
        recovery: RecoveryLimits {
            max_working_bytes: (1 << 20) + (512 << 10),
            ..tight.recovery
        },
        ..tight
    };
    assert!(
        set.verify_files(&mut Cursor::new(&bytes), &inputs, allowed)
            .unwrap()
            .content_verified()
    );
}
