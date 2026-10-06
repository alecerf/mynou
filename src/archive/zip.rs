//! Checked classic single-disk ZIP; offsets never authorize filesystem writes.
use super::{Limits, deflate};
use crate::{Result, crypto::Sha256, json::Value, usenet::yenc::Crc32};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_EXTRA: usize = 4096;
const MAX_NAME: usize = 1024;
const CHUNK: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Stored,
    Deflate,
}
impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Deflate => "deflate",
        }
    }
}

/// Offsets and expected checksums can only be constructed by the checked parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    name: String,
    directory: bool,
    method: Method,
    compressed_bytes: u64,
    bytes: u64,
    crc32: u32,
    local_offset: u64,
    data_offset: u64,
    end_offset: u64,
}
impl Entry {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_directory(&self) -> bool {
        self.directory
    }
    pub fn method(&self) -> Method {
        self.method
    }
    pub fn compressed_bytes(&self) -> u64 {
        self.compressed_bytes
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn crc32(&self) -> u32 {
        self.crc32
    }
}

/// Payload verification, distinct from a metadata-only directory inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEntry {
    bytes: u64,
    crc32: u32,
    sha256: [u8; 32],
}
impl VerifiedEntry {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn crc32(&self) -> u32 {
        self.crc32
    }
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zip {
    entries: Vec<Entry>,
    source_bytes: u64,
    declared_bytes: u64,
    limits: Limits,
    metadata_sha256: [u8; 32],
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
fn at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    length: usize,
    flag: &AtomicBool,
) -> Result<Vec<u8>> {
    deflate::active(flag)?;
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("ZIP seek: {e}"))?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("ZIP read: {e}"))?;
    deflate::active(flag)?;
    Ok(bytes)
}
fn extra(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_EXTRA {
        return Err("ZIP extra fields exceed the metadata limit".into());
    }
    let mut offset = 0;
    let mut ids = BTreeSet::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 4 {
            return Err("Truncated ZIP extra field".into());
        }
        let id = u16_at(bytes, offset);
        let length = usize::from(u16_at(bytes, offset + 2));
        offset += 4;
        if length > bytes.len() - offset {
            return Err("Truncated ZIP extra field payload".into());
        }
        if !ids.insert(id) {
            return Err("Duplicate ZIP extra field".into());
        }
        if matches!(id, 0x0001 | 0x000d | 0x0017 | 0x0018 | 0x7075 | 0x9901) {
            return Err(
                "ZIP64, alternate paths, links and encryption extras are unsupported".into(),
            );
        }
        offset += length;
    }
    Ok(())
}
fn name(bytes: &[u8], flags: u16) -> Result<(String, bool)> {
    if bytes.is_empty() || bytes.len() > MAX_NAME || (flags & 0x0800 == 0 && !bytes.is_ascii()) {
        return Err("ZIP names require bounded ASCII or explicit UTF-8".into());
    }
    let name = std::str::from_utf8(bytes).map_err(|_| "Invalid ZIP UTF-8 name")?;
    let directory = name.ends_with('/');
    let path = if directory {
        &name[..name.len() - 1]
    } else {
        name
    };
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', ':'])
        || path.chars().any(|c| c.is_control())
    {
        return Err("Unsafe ZIP entry path".into());
    }
    let components: Vec<_> = path.split('/').collect();
    if components.len() > 32
        || components.iter().any(|s| {
            s.is_empty() || *s == "." || *s == ".." || s.ends_with(['.', ' ']) || s.len() > 255
        })
    {
        return Err("Unsafe or excessively deep ZIP entry path".into());
    }
    // Avoid Windows device aliases even when inspected on a Unix host.
    for component in components {
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err("ZIP path contains a reserved device name".into());
        }
    }
    Ok((name.into(), directory))
}
fn attributes(made: u16, external: u32, directory: bool) -> Result<()> {
    let host = made >> 8;
    if !matches!(host, 0 | 3) || external & 0xffff & !0x37 != 0 {
        return Err("Unsupported ZIP host or file attributes".into());
    }
    if external & 0x10 != 0 && !directory {
        return Err("ZIP directory attributes conflict with its name".into());
    }
    if host == 3 {
        match (external >> 16) & 0o170000 {
            0 => (),
            0o040000 if directory => (),
            0o100000 if !directory => (),
            _ => {
                return Err("ZIP links, devices and conflicting file types are unsupported".into());
            }
        }
    }
    Ok(())
}

impl Zip {
    /// Reads headers and the central directory; does not decode or verify payloads.
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
            .map_err(|e| format!("ZIP length: {e}"))?;
        if !(22..u64::from(u32::MAX)).contains(&source_bytes) {
            return Err("ZIP source length requires classic ZIP within four GiB".into());
        }
        let tail_length = source_bytes.min(65_557) as usize;
        let tail_start = source_bytes - tail_length as u64;
        let tail = at(reader, tail_start, tail_length, flag)?;
        let end = (0..=tail.len() - 22)
            .rev()
            .find(|&i| {
                u32_at(&tail, i) == 0x0605_4b50
                    && i + 22 + usize::from(u16_at(&tail, i + 20)) == tail.len()
            })
            .ok_or("ZIP end record is missing or has trailing data")?;
        let eocd = &tail[end..];
        if u16_at(eocd, 4) != 0 || u16_at(eocd, 6) != 0 || u16_at(eocd, 8) != u16_at(eocd, 10) {
            return Err("Multi-disk ZIP is unsupported".into());
        }
        let count = u16_at(eocd, 10);
        let directory_bytes = u32_at(eocd, 12);
        let directory_offset = u32_at(eocd, 16);
        if count == u16::MAX || directory_bytes == u32::MAX || directory_offset == u32::MAX {
            return Err("ZIP64 is unsupported".into());
        }
        if u32::from(count) > limits.max_entries
            || u64::from(directory_bytes) > MAX_METADATA
            || u64::from(directory_offset) + u64::from(directory_bytes) != tail_start + end as u64
        {
            return Err("ZIP directory exceeds its bounds or has hidden data".into());
        }
        let directory = at(
            reader,
            u64::from(directory_offset),
            directory_bytes as usize,
            flag,
        )?;
        let mut metadata = Sha256::new();
        metadata.update(eocd);
        metadata.update(&directory);
        let mut entries = Vec::with_capacity(usize::from(count));
        let mut names = BTreeSet::new();
        let mut offset = 0;
        let mut declared_bytes = 0u64;
        for _ in 0..count {
            deflate::active(flag)?;
            if directory.len() - offset < 46 || u32_at(&directory, offset) != 0x0201_4b50 {
                return Err("Truncated or invalid ZIP central header".into());
            }
            let central = &directory[offset..offset + 46];
            let made = u16_at(central, 4);
            let needed = u16_at(central, 6);
            let flags = u16_at(central, 8);
            let method_code = u16_at(central, 10);
            let method = match method_code {
                0 => Method::Stored,
                8 => Method::Deflate,
                _ => return Err("ZIP compression method is unsupported".into()),
            };
            if !(10..=20).contains(&needed)
                || ((method == Method::Deflate || flags & 8 != 0) && needed != 20)
                || flags & !0x080e != 0
                || (method == Method::Stored && flags & 6 != 0)
                || u16_at(central, 34) != 0
            {
                return Err("ZIP encryption, flags, versions or disks are unsupported".into());
            }
            let crc32 = u32_at(central, 16);
            let compressed = u32_at(central, 20);
            let bytes = u32_at(central, 24);
            let local_offset = u32_at(central, 42);
            if [compressed, bytes, local_offset].contains(&u32::MAX) {
                return Err("ZIP64 is unsupported".into());
            }
            let name_length = usize::from(u16_at(central, 28));
            let extra_length = usize::from(u16_at(central, 30));
            let comment_length = usize::from(u16_at(central, 32));
            let variable_length = name_length + extra_length + comment_length;
            if variable_length > directory.len() - offset - 46 {
                return Err("Truncated ZIP central metadata".into());
            }
            let variable = &directory[offset + 46..offset + 46 + variable_length];
            let (name, is_directory) = name(&variable[..name_length], flags)?;
            extra(&variable[name_length..name_length + extra_length])?;
            attributes(made, u32_at(central, 38), is_directory)?;
            let folded = name.trim_end_matches('/').to_lowercase();
            if !names.insert(folded) {
                return Err("Duplicate or case-colliding ZIP entry path".into());
            }
            if u64::from(bytes) > limits.max_entry_bytes
                || u64::from(bytes)
                    > u64::from(compressed).saturating_mul(u64::from(limits.max_ratio))
                || (method == Method::Stored && bytes != compressed)
                || (is_directory
                    && (method != Method::Stored
                        || bytes != 0
                        || compressed != 0
                        || crc32 != 0
                        || flags & 8 != 0))
            {
                return Err(
                    "ZIP entry exceeds its size/ratio policy or has invalid directory data".into(),
                );
            }
            declared_bytes += u64::from(bytes);
            if declared_bytes > limits.max_total_bytes {
                return Err("ZIP declared output exceeds its total byte limit".into());
            }
            let local_offset = u64::from(local_offset);
            if local_offset + 30 > u64::from(directory_offset) {
                return Err("ZIP local header lies outside its data area".into());
            }
            let local = at(reader, local_offset, 30, flag)?;
            if u32_at(&local, 0) != 0x0403_4b50
                || u16_at(&local, 4) != needed
                || u16_at(&local, 6) != flags
                || u16_at(&local, 8) != method_code
                || local[10..14] != central[12..16]
                || usize::from(u16_at(&local, 26)) != name_length
            {
                return Err("ZIP local and central headers disagree".into());
            }
            let local_values = [u32_at(&local, 14), u32_at(&local, 18), u32_at(&local, 22)];
            if local_values != [crc32, compressed, bytes]
                && !(flags & 8 != 0 && local_values == [0; 3])
            {
                return Err("ZIP local and central checksums or sizes disagree".into());
            }
            let local_extra = usize::from(u16_at(&local, 28));
            if local_extra > MAX_EXTRA {
                return Err("ZIP local extra fields exceed the metadata limit".into());
            }
            let data_offset = local_offset + 30 + (name_length + local_extra) as u64;
            let data_end = data_offset + u64::from(compressed);
            if data_end > u64::from(directory_offset) {
                return Err("ZIP compressed entry lies outside its data area".into());
            }
            let local_variable = at(reader, local_offset + 30, name_length + local_extra, flag)?;
            if local_variable[..name_length] != variable[..name_length] {
                return Err("ZIP local and central names disagree".into());
            }
            extra(&local_variable[name_length..])?;
            metadata.update(&local);
            metadata.update(&local_variable);
            let mut end_offset = data_end;
            if flags & 8 != 0 {
                if data_end + 12 > u64::from(directory_offset) {
                    return Err("Truncated ZIP data descriptor".into());
                }
                let first = at(reader, data_end, 4, flag)?;
                let signed = u32_at(&first, 0) == 0x0807_4b50;
                end_offset += if signed { 16 } else { 12 };
                if end_offset > u64::from(directory_offset) {
                    return Err("Truncated ZIP data descriptor".into());
                }
                let descriptor = at(reader, data_end + if signed { 4 } else { 0 }, 12, flag)?;
                if [
                    u32_at(&descriptor, 0),
                    u32_at(&descriptor, 4),
                    u32_at(&descriptor, 8),
                ] != [crc32, compressed, bytes]
                {
                    return Err("ZIP data descriptor disagrees with its central header".into());
                }
                metadata.update(&first);
                metadata.update(&descriptor);
            }
            entries.push(Entry {
                name,
                directory: is_directory,
                method,
                compressed_bytes: u64::from(compressed),
                bytes: u64::from(bytes),
                crc32,
                local_offset,
                data_offset,
                end_offset,
            });
            offset += 46 + variable_length;
        }
        if offset != directory.len() {
            return Err("ZIP directory contains unaccounted metadata".into());
        }
        let mut spans: Vec<_> = entries
            .iter()
            .map(|e| (e.local_offset, e.end_offset))
            .collect();
        spans.sort_unstable();
        let mut previous = 0;
        for (start, end) in spans {
            if start != previous || end < start {
                return Err("ZIP local entries overlap or contain hidden data".into());
            }
            previous = end;
        }
        if previous != u64::from(directory_offset) {
            return Err("ZIP has a prefix or unaccounted data before its directory".into());
        }
        // A file may not also be an ancestor directory, regardless of case.
        let files: BTreeSet<_> = entries
            .iter()
            .filter(|e| !e.directory)
            .map(|e| e.name.to_lowercase())
            .collect();
        for entry in &entries {
            for (index, _) in entry.name.match_indices('/') {
                if files.contains(&entry.name[..index].to_lowercase()) {
                    return Err("ZIP file path collides with a directory ancestor".into());
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
        if !fs::symlink_metadata(path)
            .map_err(|e| format!("ZIP metadata: {e}"))?
            .is_file()
        {
            return Err("ZIP inspection requires a regular file, without a symbolic link".into());
        }
        let mut file = File::open(path).map_err(|e| format!("ZIP open: {e}"))?;
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

    /// Recheck all structural metadata before decoding into a provisional sink.
    /// Success checks exact decoded size and CRC; publication remains the caller's
    /// transaction, ownership, identity and media-analysis responsibility.
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
            .ok_or("ZIP entry index is out of bounds")?;
        if entry.directory {
            return Err("ZIP directories are not extractable payloads".into());
        }
        if Self::read_cancellable(reader, self.limits, flag)? != *self {
            return Err("ZIP structural metadata changed after inspection".into());
        }
        reader
            .seek(SeekFrom::Start(entry.data_offset))
            .map_err(|e| format!("ZIP payload seek: {e}"))?;
        let decoded = match entry.method {
            Method::Deflate => deflate::decode(
                reader,
                entry.compressed_bytes,
                writer,
                entry.bytes,
                self.limits.max_blocks,
                flag,
            )?,
            Method::Stored => {
                let mut remaining = entry.bytes;
                let mut buffer = vec![0; CHUNK];
                let mut crc = Crc32::default();
                let mut sha = Sha256::new();
                while remaining != 0 {
                    deflate::active(flag)?;
                    let count = remaining.min(CHUNK as u64) as usize;
                    reader
                        .read_exact(&mut buffer[..count])
                        .map_err(|e| format!("ZIP stored payload read: {e}"))?;
                    deflate::active(flag)?;
                    writer
                        .write_all(&buffer[..count])
                        .map_err(|e| format!("ZIP output write: {e}"))?;
                    crc.update(&buffer[..count]);
                    sha.update(&buffer[..count]);
                    remaining -= count as u64;
                }
                deflate::Decoded {
                    bytes: entry.bytes,
                    crc32: crc.finish(),
                    sha256: sha.finalize(),
                }
            }
        };
        if decoded.bytes != entry.bytes || decoded.crc32 != entry.crc32 {
            return Err("ZIP payload size or CRC verification failed".into());
        }
        deflate::active(flag)?;
        Ok(VerifiedEntry {
            bytes: decoded.bytes,
            crc32: decoded.crc32,
            sha256: decoded.sha256,
        })
    }
    pub fn report(&self) -> Value {
        let mut result = Value::object();
        result.insert("format", "zip");
        result.insert("source_bytes", self.source_bytes.to_string());
        result.insert("declared_bytes", self.declared_bytes.to_string());
        result.insert("content_verified", false);
        result.insert(
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
                        v.insert("method", entry.method.name());
                        v.insert("compressed_bytes", entry.compressed_bytes.to_string());
                        v.insert("declared_bytes", entry.bytes.to_string());
                        v.insert("expected_crc32", format!("{:08x}", entry.crc32));
                        v
                    })
                    .collect(),
            ),
        );
        result
    }
}
