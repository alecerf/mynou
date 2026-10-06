//! Original bounded RAR5 stored-file format. No paths or output are published.
//! Structures follow https://www.rarlab.com/technote.htm, without reference code.
use super::{Limits, Method, VerifiedEntry, checked_entry_path, deflate};
use crate::{Result, crypto::Sha256, json::Value, usenet::yenc::Crc32};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

const SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const CHUNK: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    name: String,
    directory: bool,
    bytes: u64,
    crc32: u32,
    data_offset: u64,
}
impl Entry {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_directory(&self) -> bool {
        self.directory
    }
    pub fn method(&self) -> Method {
        Method::Stored
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn compressed_bytes(&self) -> u64 {
        self.bytes
    }
    pub fn crc32(&self) -> u32 {
        self.crc32
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rar5 {
    entries: Vec<Entry>,
    source_bytes: u64,
    declared_bytes: u64,
    limits: Limits,
    metadata_sha256: [u8; 32],
}

struct Fields<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        if length > self.bytes.len() - self.offset {
            return Err("RAR5 truncated header field".into());
        }
        let bytes = &self.bytes[self.offset..self.offset + length];
        self.offset += length;
        Ok(bytes)
    }
    fn vint(&mut self) -> Result<u64> {
        let mut value = 0;
        for index in 0..10 {
            let byte = self.take(1)?[0];
            if index == 9 && byte > 1 {
                return Err("RAR5 variable integer exceeds 64 bits".into());
            }
            value |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("RAR5 variable integer is too long".into())
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| "RAR5 truncated integer")?,
        ))
    }
    fn finish(self) -> Result<()> {
        if self.offset != self.bytes.len() {
            return Err("RAR5 unaccounted header data".into());
        }
        Ok(())
    }
}

fn time_records(bytes: &[u8]) -> Result<()> {
    let mut extra = Fields::new(bytes);
    let mut seen = BTreeSet::new();
    while extra.offset < bytes.len() {
        let length =
            usize::try_from(extra.vint()?).map_err(|_| "RAR5 extra record is too large")?;
        let mut record = Fields::new(extra.take(length)?);
        let kind = record.vint()?;
        if !seen.insert(kind) {
            return Err("RAR5 duplicate extra record".into());
        }
        if kind != 3 {
            return Err(
                "RAR5 encryption, hashes, redirection, owners and unknown extras are unsupported"
                    .into(),
            );
        }
        let flags = record.vint()?;
        if flags & !0x1f != 0 || flags & 0x10 != 0 && flags & 1 == 0 {
            return Err("RAR5 unsupported time flags".into());
        }
        for mask in [2, 4, 8] {
            if flags & mask != 0 {
                record.take(if flags & 1 != 0 { 4 } else { 8 })?;
            }
        }
        if flags & 0x11 == 0x11 {
            for mask in [2, 4, 8] {
                if flags & mask != 0 && record.u32()? >= 1_000_000_000 {
                    return Err("RAR5 invalid nanosecond time".into());
                }
            }
        }
        record.finish()?;
    }
    extra.finish()
}

fn file_entry(
    body: &[u8],
    extra_size: usize,
    data_size: u64,
    data_offset: u64,
    limits: Limits,
) -> Result<Entry> {
    if extra_size > 4096 || extra_size > body.len() {
        return Err("RAR5 extra area exceeds its bounds".into());
    }
    let end = body.len() - extra_size;
    let mut fields = Fields::new(&body[..end]);
    let flags = fields.vint()?;
    if flags & !7 != 0 {
        return Err("RAR5 unknown size or unsupported file flags".into());
    }
    let directory = flags & 1 != 0;
    let bytes = fields.vint()?;
    let attributes = fields.vint()?;
    if flags & 2 != 0 {
        fields.u32()?;
    }
    let expected_crc = if flags & 4 != 0 {
        fields.u32()?
    } else if directory {
        0
    } else {
        return Err("RAR5 payload CRC is required".into());
    };
    let compression = fields.vint()?;
    let version = compression & 0x3f;
    let dictionary = (compression >> 10) & 31;
    if version > 1
        || compression & 0x3c0 != 0
        || compression & !0x1f_ffff != 0
        || version == 0 && (dictionary > 15 || compression >> 15 != 0)
        || version == 1 && dictionary > 23
    {
        return Err(
            "RAR5 compressed, solid or unknown compression variants are unsupported".into(),
        );
    }
    let host = fields.vint()?;
    let length =
        usize::try_from(fields.vint()?).map_err(|_| "RAR5 filename length exceeds its bound")?;
    if length == 0 || length > 1024 {
        return Err("RAR5 filename length exceeds its bound".into());
    }
    let raw_name =
        std::str::from_utf8(fields.take(length)?).map_err(|_| "RAR5 filename must be UTF-8")?;
    if raw_name.contains('\u{fffe}') {
        return Err("RAR5 mapped nonportable names are unsupported".into());
    }
    let name = if directory && !raw_name.ends_with('/') {
        format!("{raw_name}/")
    } else {
        raw_name.to_owned()
    };
    if checked_entry_path(&name).map_err(|e| format!("RAR5 entry path: {e}"))? != directory {
        return Err("RAR5 filename and directory flags disagree".into());
    }
    match host {
        0 if attributes & !0x37 == 0 && (attributes & 0x10 != 0) == directory => (),
        1 if attributes & !0o170777 == 0 => match attributes & 0o170000 {
            0 => (),
            0o040000 if directory => (),
            0o100000 if !directory => (),
            _ => {
                return Err(
                    "RAR5 links, devices and conflicting file types are unsupported".into(),
                );
            }
        },
        _ => return Err("RAR5 unsupported host or file attributes".into()),
    }
    if bytes != data_size
        || bytes > limits.max_entry_bytes
        || directory && (bytes != 0 || expected_crc != 0)
    {
        return Err("RAR5 stored size or directory data exceeds its bounds".into());
    }
    fields.finish()?;
    time_records(&body[end..])?;
    Ok(Entry {
        name,
        directory,
        bytes,
        crc32: expected_crc,
        data_offset,
    })
}

impl Rar5 {
    pub fn read<R: Read + Seek>(reader: &mut R, limits: Limits) -> Result<Self> {
        Self::read_cancellable(reader, limits, &AtomicBool::new(true))
    }
    pub fn read_cancellable<R: Read + Seek>(
        reader: &mut R,
        limits: Limits,
        flag: &AtomicBool,
    ) -> Result<Self> {
        limits.validate()?;
        deflate::active(flag)?;
        let source_bytes = reader
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("RAR5 source length: {e}"))?;
        if source_bytes < 24 || source_bytes > limits.max_total_bytes.saturating_add(MAX_METADATA) {
            return Err("RAR5 source length exceeds its bounds".into());
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("RAR5 signature seek: {e}"))?;
        let mut signature = [0; 8];
        reader
            .read_exact(&mut signature)
            .map_err(|e| format!("RAR5 signature read: {e}"))?;
        if &signature != SIGNATURE {
            return Err("RAR5 signature required at offset zero; RAR4 and self-extracting prefixes are unsupported".into());
        }
        let mut offset = 8_u64;
        let mut metadata_bytes = 8_u64;
        let mut metadata = Sha256::new();
        metadata.update(&signature);
        let mut main = false;
        let mut entries = Vec::new();
        let mut names = BTreeSet::new();
        let mut declared_bytes = 0_u64;
        loop {
            deflate::active(flag)?;
            if source_bytes - offset < 5 {
                return Err("RAR5 end marker is missing or truncated".into());
            }
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| format!("RAR5 header seek: {e}"))?;
            let mut crc_bytes = [0; 4];
            reader
                .read_exact(&mut crc_bytes)
                .map_err(|e| format!("RAR5 header CRC read: {e}"))?;
            let mut size_bytes = Vec::with_capacity(3);
            let mut length = 0_usize;
            for index in 0..3 {
                if offset + 4 + index as u64 >= source_bytes {
                    return Err("RAR5 truncated header length".into());
                }
                let mut byte = [0];
                reader
                    .read_exact(&mut byte)
                    .map_err(|e| format!("RAR5 header length read: {e}"))?;
                size_bytes.push(byte[0]);
                length |= usize::from(byte[0] & 0x7f) << (index * 7);
                if byte[0] & 0x80 == 0 {
                    break;
                }
                if index == 2 {
                    return Err("RAR5 header size requires at most three bytes".into());
                }
            }
            let header_bytes = 4 + size_bytes.len() as u64 + length as u64;
            metadata_bytes = metadata_bytes
                .checked_add(header_bytes)
                .ok_or("RAR5 metadata length overflow")?;
            if length < 2 || metadata_bytes > MAX_METADATA || header_bytes > source_bytes - offset {
                return Err("RAR5 header or total metadata exceeds its bounds".into());
            }
            let mut header = vec![0; length];
            reader
                .read_exact(&mut header)
                .map_err(|e| format!("RAR5 header read: {e}"))?;
            deflate::active(flag)?;
            let mut crc = Crc32::default();
            crc.update(&size_bytes);
            crc.update(&header);
            if crc.finish() != u32::from_le_bytes(crc_bytes) {
                return Err("RAR5 header CRC verification failed".into());
            }
            metadata.update(&crc_bytes);
            metadata.update(&size_bytes);
            metadata.update(&header);
            let mut fields = Fields::new(&header);
            let kind = fields.vint()?;
            let flags = fields.vint()?;
            if flags & !3 != 0 {
                return Err("RAR5 split, dependent and unsupported block flags".into());
            }
            let extra_size = if flags & 1 != 0 {
                usize::try_from(fields.vint()?).map_err(|_| "RAR5 extra area is too large")?
            } else {
                0
            };
            let data_size = if flags & 2 != 0 { fields.vint()? } else { 0 };
            let data_offset = offset
                .checked_add(header_bytes)
                .ok_or("RAR5 data offset overflow")?;
            let next = data_offset
                .checked_add(data_size)
                .ok_or("RAR5 data size overflow")?;
            if next > source_bytes {
                return Err("RAR5 data span exceeds the source".into());
            }
            match kind {
                1 if !main && entries.is_empty() && offset == 8 => {
                    if flags != 0 || fields.vint()? & !0x10 != 0 {
                        return Err(
                            "RAR5 volumes, solid, recovery and main extras are unsupported".into(),
                        );
                    }
                    fields.finish()?;
                    main = true;
                }
                2 if main => {
                    if entries.len() >= limits.max_entries as usize {
                        return Err("RAR5 entry count exceeds its bound".into());
                    }
                    let entry = file_entry(
                        &header[fields.offset..],
                        extra_size,
                        data_size,
                        data_offset,
                        limits,
                    )?;
                    declared_bytes = declared_bytes
                        .checked_add(entry.bytes)
                        .ok_or("RAR5 declared output overflow")?;
                    if declared_bytes > limits.max_total_bytes {
                        return Err("RAR5 total declared output exceeds its bound".into());
                    }
                    if !names.insert(entry.name.trim_end_matches('/').to_lowercase()) {
                        return Err("RAR5 duplicate or case-colliding entry paths".into());
                    }
                    entries.push(entry);
                }
                5 if main => {
                    if flags != 0 || fields.vint()? != 0 || next != source_bytes {
                        return Err("RAR5 volume end flags or trailing data are unsupported".into());
                    }
                    fields.finish()?;
                    break;
                }
                _ => {
                    return Err(
                        "RAR5 header order, encryption, service or unknown blocks are unsupported"
                            .into(),
                    );
                }
            }
            offset = next;
        }
        let files: BTreeSet<_> = entries
            .iter()
            .filter(|e| !e.directory)
            .map(|e| e.name.to_lowercase())
            .collect();
        for entry in &entries {
            for (index, _) in entry.name.match_indices('/') {
                if files.contains(&entry.name[..index].to_lowercase()) {
                    return Err("RAR5 file path collides with a directory ancestor".into());
                }
            }
        }
        deflate::active(flag)?;
        Ok(Self {
            entries,
            source_bytes,
            declared_bytes,
            limits,
            metadata_sha256: metadata.finalize(),
        })
    }
    pub fn read_file(path: &Path, limits: Limits) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(path).map_err(|e| format!("RAR5 file metadata: {e}"))?;
        if !metadata.is_file() {
            return Err("RAR5 inspection requires a regular file without a symbolic link".into());
        }
        let mut file = File::open(path).map_err(|e| format!("RAR5 file open: {e}"))?;
        let opened = file
            .metadata()
            .map_err(|e| format!("RAR5 opened metadata: {e}"))?;
        if !opened.is_file() || metadata.len() != opened.len() {
            return Err("RAR5 input changed before inspection".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
                return Err("RAR5 input inode changed before inspection".into());
            }
        }
        Self::read(&mut file, limits)
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    pub fn declared_bytes(&self) -> u64 {
        self.declared_bytes
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    /// Rechecks metadata, then verifies size and mandatory CRC in a provisional sink.
    /// The caller owns all destinations, permissions, persistence and publication.
    pub fn extract<R: Read + Seek, W: Write>(
        &self,
        reader: &mut R,
        index: usize,
        writer: &mut W,
        flag: &AtomicBool,
    ) -> Result<VerifiedEntry> {
        let entry = self
            .entries
            .get(index)
            .ok_or("RAR5 entry index is out of bounds")?;
        if entry.directory {
            return Err("RAR5 directory is not an extractable payload".into());
        }
        if Self::read_cancellable(reader, self.limits, flag)? != *self {
            return Err("RAR5 structural metadata changed after inspection".into());
        }
        reader
            .seek(SeekFrom::Start(entry.data_offset))
            .map_err(|e| format!("RAR5 payload seek: {e}"))?;
        let mut remaining = entry.bytes;
        let mut buffer = vec![0; CHUNK];
        let mut crc = Crc32::default();
        let mut sha = Sha256::new();
        while remaining != 0 {
            deflate::active(flag)?;
            let count = remaining.min(CHUNK as u64) as usize;
            reader
                .read_exact(&mut buffer[..count])
                .map_err(|e| format!("RAR5 stored payload read: {e}"))?;
            deflate::active(flag)?;
            writer
                .write_all(&buffer[..count])
                .map_err(|e| format!("RAR5 provisional write: {e}"))?;
            crc.update(&buffer[..count]);
            sha.update(&buffer[..count]);
            remaining -= count as u64;
        }
        if crc.finish() != entry.crc32 {
            return Err("RAR5 payload CRC verification failed".into());
        }
        deflate::active(flag)?;
        Ok(VerifiedEntry::checked(
            entry.bytes,
            crc.finish(),
            sha.finalize(),
        ))
    }
    pub fn report(&self) -> Value {
        let mut v = Value::object();
        v.insert("format", "rar5");
        v.insert("source_bytes", self.source_bytes.to_string());
        v.insert("declared_bytes", self.declared_bytes.to_string());
        v.insert("content_verified", false);
        v.insert(
            "entries",
            Value::Array(
                self.entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let mut v = Value::object();
                        v.insert("index", index as u32);
                        v.insert("name", entry.name.clone());
                        v.insert("directory", entry.directory);
                        v.insert("method", "stored");
                        v.insert("compressed_bytes", entry.bytes.to_string());
                        v.insert("declared_bytes", entry.bytes.to_string());
                        v.insert("expected_crc32", format!("{:08x}", entry.crc32));
                        v
                    })
                    .collect(),
            ),
        );
        v
    }
}
