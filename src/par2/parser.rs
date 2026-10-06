//! Strict, contiguous core packets. Damaged-packet resynchronization and volume
//! merging are deliberately separate from this bounded format foundation.
use super::{FileDescription, Limits, RecoverySlice, Set, SliceChecksum};
use crate::{
    Result,
    crypto::{Md5, Sha256, md5},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 64 << 10;
const MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
const DESCRIPTION: &[u8; 16] = b"PAR 2.0\0FileDesc";
const CHECKSUMS: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
const RECOVERY: &[u8; 16] = b"PAR 2.0\0RecvSlic";
const CREATOR: &[u8; 16] = b"PAR 2.0\0Creator\0";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Main,
    Description,
    Checksums,
    Recovery,
    Creator,
    Ignored,
}

fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("PAR2 operation was cancelled".into())
    }
}
fn exact<R: Read>(reader: &mut R, bytes: &mut [u8], flag: &AtomicBool) -> Result<()> {
    active(flag)?;
    reader
        .read_exact(bytes)
        .map_err(|e| format!("PAR2 read: {e}"))?;
    active(flag)
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("checked PAR2 field"),
    )
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("checked PAR2 field"),
    )
}
fn id_at(bytes: &[u8], offset: usize) -> [u8; 16] {
    bytes[offset..offset + 16]
        .try_into()
        .expect("checked PAR2 field")
}
fn bounded_add(total: &mut u64, add: u64, max: u64, message: &str) -> Result<()> {
    *total = total
        .checked_add(add)
        .filter(|v| *v <= max)
        .ok_or(message)?;
    Ok(())
}
fn kind(packet_type: &[u8; 16]) -> Result<Kind> {
    Ok(match packet_type {
        t if t == MAIN => Kind::Main,
        t if t == DESCRIPTION => Kind::Description,
        t if t == CHECKSUMS => Kind::Checksums,
        t if t == RECOVERY => Kind::Recovery,
        t if t == CREATOR => Kind::Creator,
        t if t.starts_with(b"PAR 2.0\0") => {
            return Err(
                "PAR2 optional or unknown standard packet semantics are unsupported".into(),
            );
        }
        _ => Kind::Ignored,
    })
}
fn body_bounds(kind: Kind, length: u64, limits: Limits) -> Result<()> {
    let valid = match kind {
        Kind::Main => {
            length >= 12
                && (length - 12).is_multiple_of(16)
                && length <= 12 + 16 * u64::from(limits.max_files)
        }
        Kind::Description => (60..=1080).contains(&length),
        Kind::Checksums => {
            length >= 16
                && (length - 16).is_multiple_of(20)
                && (length - 16) / 20 <= u64::from(limits.max_slices)
        }
        Kind::Recovery => length >= 8 && length - 4 <= u64::from(limits.max_slice_bytes),
        Kind::Creator => (4..=1024).contains(&length),
        Kind::Ignored => length <= limits.max_metadata_bytes,
    };
    if valid {
        Ok(())
    } else {
        Err("PAR2 packet body exceeds its type or resource bounds".into())
    }
}
fn text(bytes: &[u8], maximum: usize) -> Result<&str> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    if end == 0
        || end > maximum
        || bytes.len() - end > 3
        || bytes[end..].iter().any(|b| *b != 0)
        || bytes[..end].iter().any(|b| !(0x20..=0x7e).contains(b))
    {
        return Err("PAR2 text requires bounded printable ASCII and exact zero padding".into());
    }
    std::str::from_utf8(&bytes[..end]).map_err(|_| "PAR2 text is invalid ASCII".into())
}
fn description(bytes: &[u8], limits: Limits) -> Result<FileDescription> {
    let name = text(&bytes[56..], 1024)?;
    if crate::archive::checked_entry_path(name).map_err(|e| format!("PAR2 path: {e}"))? {
        return Err("PAR2 file description cannot name a directory".into());
    }
    let file_bytes = u64_at(bytes, 48);
    if file_bytes > limits.max_file_bytes {
        return Err("PAR2 declared file exceeds the file byte limit".into());
    }
    let id = id_at(bytes, 0);
    let first_16k_md5 = id_at(bytes, 32);
    let mut expected = Md5::new();
    expected.update(&first_16k_md5);
    expected.update(&file_bytes.to_le_bytes());
    expected.update(name.as_bytes());
    if expected.finalize() != id {
        return Err("PAR2 file identifier disagrees with its unpadded name and metadata".into());
    }
    let file_md5 = id_at(bytes, 16);
    if (file_bytes <= 16_384 && file_md5 != first_16k_md5)
        || (file_bytes == 0 && file_md5 != md5(&[]))
    {
        return Err("PAR2 short or empty file hashes disagree".into());
    }
    Ok(FileDescription {
        id,
        md5: file_md5,
        first_16k_md5,
        bytes: file_bytes,
        name: name.into(),
        recoverable: false,
        slices: Vec::new(),
    })
}
fn insert<K: Ord, V: PartialEq>(map: &mut BTreeMap<K, V>, key: K, value: V) -> Result<()> {
    if let Some(old) = map.get(&key) {
        if old != &value {
            return Err("PAR2 duplicate packet has conflicting content".into());
        }
    } else {
        map.insert(key, value);
    }
    Ok(())
}

impl Set {
    /// Reads one complete, strict core packet set. Does not read protected files.
    pub fn read<R: Read + Seek>(reader: &mut R, limits: Limits) -> Result<Self> {
        Self::read_cancellable(reader, limits, &AtomicBool::new(true))
    }
    pub fn read_cancellable<R: Read + Seek>(
        reader: &mut R,
        limits: Limits,
        flag: &AtomicBool,
    ) -> Result<Self> {
        limits.validate()?;
        active(flag)?;
        let source_bytes = reader
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("PAR2 source length: {e}"))?;
        if !(64..=limits.max_source_bytes).contains(&source_bytes) {
            return Err("PAR2 source exceeds the source byte bounds".into());
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("PAR2 source seek: {e}"))?;
        let mut source_hash = Sha256::new();
        let mut offset = 0u64;
        let mut packet_count = 0u32;
        let mut ignored_packets = 0u32;
        let mut metadata_bytes = 0u64;
        let mut recovery_bytes = 0u64;
        let mut checksum_count = 0u64;
        let mut set_id = None;
        let mut main: Option<Vec<u8>> = None;
        let mut descriptions = BTreeMap::new();
        let mut checksums: BTreeMap<[u8; 16], Vec<SliceChecksum>> = BTreeMap::new();
        let mut recovery: BTreeMap<u32, RecoverySlice> = BTreeMap::new();
        let mut creators = BTreeSet::new();
        let mut buffer = vec![0; CHUNK];
        while offset < source_bytes {
            active(flag)?;
            if source_bytes - offset < 64 || packet_count == limits.max_packets {
                return Err("PAR2 truncated header or packet count limit".into());
            }
            let mut header = [0; 64];
            exact(reader, &mut header, flag)?;
            if &header[..8] != b"PAR2\0PKT" {
                return Err("PAR2 packet magic is invalid".into());
            }
            let length = u64_at(&header, 8);
            if length < 64 || !length.is_multiple_of(4) || length > source_bytes - offset {
                return Err("PAR2 packet length is invalid or exceeds the captured source".into());
            }
            let id = id_at(&header, 32);
            if set_id.is_some_and(|old| old != id) {
                return Err("PAR2 packets belong to different recovery sets".into());
            }
            set_id = Some(id);
            let packet_type = id_at(&header, 48);
            let kind = kind(&packet_type)?;
            let body_length = length - 64;
            body_bounds(kind, body_length, limits)?;
            let metadata = if kind == Kind::Recovery { 68 } else { length };
            bounded_add(
                &mut metadata_bytes,
                metadata,
                limits.max_metadata_bytes,
                "PAR2 metadata byte limit exceeded",
            )?;
            if kind == Kind::Recovery {
                bounded_add(
                    &mut recovery_bytes,
                    body_length - 4,
                    limits.max_recovery_bytes,
                    "PAR2 recovery byte limit exceeded",
                )?;
            }
            source_hash.update(&header);
            let mut packet_hash = Md5::new();
            packet_hash.update(&header[32..]);
            let mut payload_hash = Sha256::new();
            let retained = !matches!(kind, Kind::Recovery | Kind::Ignored);
            let mut body = Vec::with_capacity(if retained { body_length as usize } else { 0 });
            let mut consumed = 0u64;
            let mut exponent = 0;
            while consumed < body_length {
                let take = (body_length - consumed).min(CHUNK as u64) as usize;
                exact(reader, &mut buffer[..take], flag)?;
                source_hash.update(&buffer[..take]);
                packet_hash.update(&buffer[..take]);
                if retained {
                    body.extend_from_slice(&buffer[..take]);
                } else if kind == Kind::Recovery {
                    let skip = if consumed == 0 {
                        exponent = u32_at(&buffer[..take], 0);
                        4
                    } else {
                        0
                    };
                    payload_hash.update(&buffer[skip..take]);
                }
                consumed += take as u64;
            }
            let packet_md5 = id_at(&header, 16);
            if packet_hash.finalize() != packet_md5 {
                return Err("PAR2 packet MD5 checksum mismatch".into());
            }
            match kind {
                Kind::Main => {
                    let slice_bytes = u64_at(&body, 0);
                    let ids = (body.len() - 12) / 16;
                    if slice_bytes == 0
                        || !slice_bytes.is_multiple_of(4)
                        || slice_bytes > u64::from(limits.max_slice_bytes)
                        || u32_at(&body, 8) as usize > ids
                    {
                        return Err(
                            "PAR2 Main slice size or recoverable file count is invalid".into()
                        );
                    }
                    let mut unique = BTreeSet::new();
                    for id in body[12..].as_chunks::<16>().0 {
                        if !unique.insert(*id) {
                            return Err("PAR2 Main contains duplicate file identifiers".into());
                        }
                    }
                    if main.as_ref().is_some_and(|old| old != &body) {
                        return Err("PAR2 duplicate Main packet conflicts".into());
                    }
                    main = Some(body);
                }
                Kind::Description => {
                    let file = description(&body, limits)?;
                    if !descriptions.contains_key(&file.id)
                        && descriptions.len() == limits.max_files as usize
                    {
                        return Err("PAR2 file description count limit exceeded".into());
                    }
                    insert(&mut descriptions, file.id, file)?;
                }
                Kind::Checksums => {
                    let id = id_at(&body, 0);
                    let count = (body.len() - 16) / 20;
                    if !checksums.contains_key(&id) {
                        if checksums.len() == limits.max_files as usize {
                            return Err("PAR2 checksum file count limit exceeded".into());
                        }
                        bounded_add(
                            &mut checksum_count,
                            count as u64,
                            u64::from(limits.max_slices),
                            "PAR2 total checksum slice limit exceeded",
                        )?;
                    }
                    let slices = body[16..]
                        .as_chunks::<20>()
                        .0
                        .iter()
                        .map(|bytes| SliceChecksum {
                            md5: id_at(bytes, 0),
                            crc32: u32_at(bytes, 16),
                        })
                        .collect();
                    insert(&mut checksums, id, slices)?;
                }
                Kind::Recovery => {
                    if exponent >= 65535 {
                        return Err(
                            "PAR2 recovery exponent exceeds the supported field period".into()
                        );
                    }
                    let slice = RecoverySlice {
                        exponent,
                        data_offset: offset + 68,
                        bytes: (body_length - 4) as u32,
                        packet_md5,
                        sha256: payload_hash.finalize(),
                    };
                    if let Some(old) = recovery.get(&exponent) {
                        if old.bytes != slice.bytes
                            || old.packet_md5 != slice.packet_md5
                            || old.sha256 != slice.sha256
                        {
                            return Err("PAR2 duplicate recovery exponent conflicts".into());
                        }
                    } else {
                        recovery.insert(exponent, slice);
                    }
                }
                Kind::Creator => {
                    let creator = text(&body, 1024)?;
                    if !creators.contains(creator) && creators.len() == 16 {
                        return Err("PAR2 creator count limit exceeded".into());
                    }
                    creators.insert(creator.to_owned());
                }
                Kind::Ignored => ignored_packets += 1,
            }
            packet_count += 1;
            offset += length;
        }
        if reader
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("PAR2 final source length: {e}"))?
            != source_bytes
        {
            return Err("PAR2 source length changed during inspection".into());
        }
        let main = main.ok_or("PAR2 complete inspection requires a Main packet")?;
        if creators.is_empty() {
            return Err("PAR2 complete inspection requires a Creator packet".into());
        }
        let slice_bytes = u64_at(&main, 0) as u32;
        let recoverable = u32_at(&main, 8) as usize;
        let ids = main[12..].as_chunks::<16>().0;
        if descriptions.len() != ids.len() {
            return Err("PAR2 file descriptions do not match the complete Main set".into());
        }
        let mut files = Vec::with_capacity(ids.len());
        let mut names: BTreeSet<String> = BTreeSet::new();
        let mut declared_bytes = 0;
        let mut input_slices = 0;
        for (index, id) in ids.iter().enumerate() {
            let mut file = descriptions
                .remove(id)
                .ok_or("PAR2 Main file description is missing")?;
            file.recoverable = index < recoverable;
            let count = file.bytes.div_ceil(u64::from(slice_bytes));
            if file.recoverable {
                bounded_add(
                    &mut input_slices,
                    count,
                    u64::from(limits.max_slices),
                    "PAR2 recoverable input slice limit exceeded",
                )?;
            }
            match checksums.remove(id) {
                Some(slices) if slices.len() as u64 == count => file.slices = slices,
                None if !file.recoverable => (),
                _ => {
                    return Err(
                        "PAR2 input checksums are missing or disagree with the file slice count"
                            .into(),
                    );
                }
            }
            bounded_add(
                &mut declared_bytes,
                file.bytes,
                limits.max_total_file_bytes,
                "PAR2 total declared file byte limit exceeded",
            )?;
            let name = file.name.to_ascii_lowercase();
            if names.iter().any(|old| {
                old == &name
                    || old.starts_with(&(name.clone() + "/"))
                    || name.starts_with(&(old.clone() + "/"))
            }) {
                return Err(
                    "PAR2 duplicate, case-alias or file-ancestor paths are unsupported".into(),
                );
            }
            names.insert(name);
            files.push(file);
        }
        if !checksums.is_empty() {
            return Err("PAR2 orphan input checksum packet".into());
        }
        if recovery.values().any(|r| r.bytes != slice_bytes) {
            return Err("PAR2 recovery data length disagrees with Main slice size".into());
        }
        active(flag)?;
        Ok(Self {
            id: set_id.ok_or("PAR2 set identifier is missing")?,
            slice_bytes,
            files,
            recovery: recovery.into_values().collect(),
            creators: creators.into_iter().collect(),
            packet_count,
            ignored_packets,
            source_bytes,
            source_sha256: source_hash.finalize(),
            set_id_matches_main: md5(&main) == set_id.expect("checked PAR2 set identifier"),
            limits,
        })
    }

    pub fn read_file(path: &Path, limits: Limits) -> Result<Self> {
        let before = fs::symlink_metadata(path).map_err(|e| format!("PAR2 file metadata: {e}"))?;
        if !before.is_file() {
            return Err("PAR2 inspection requires a regular file without a symbolic link".into());
        }
        let mut file = File::open(path).map_err(|e| format!("PAR2 file open: {e}"))?;
        let opened = file
            .metadata()
            .map_err(|e| format!("PAR2 opened metadata: {e}"))?;
        if !opened.is_file() || before.len() != opened.len() {
            return Err("PAR2 input changed before inspection".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.dev() != opened.dev() || before.ino() != opened.ino() {
                return Err("PAR2 input inode changed before inspection".into());
            }
        }
        Self::read(&mut file, limits)
    }
}
