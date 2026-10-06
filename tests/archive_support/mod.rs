//! Original synthetic ZIP writer for independent format and admission fixtures.
#![allow(dead_code)]

pub fn reference_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 == 0 { 0 } else { 0xedb8_8320 };
        }
    }
    !crc
}
fn word(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
fn dword(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

pub struct Fixture<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    /// None means stored; Some contains independently generated raw DEFLATE.
    pub deflate: Option<&'a [u8]>,
    /// Some(true) uses a signed descriptor, Some(false) an unsigned descriptor.
    pub descriptor: Option<bool>,
    pub external: u32,
    pub extra: &'a [u8],
}
impl<'a> Fixture<'a> {
    pub fn stored(name: &'a str, bytes: &'a [u8]) -> Self {
        Self {
            name,
            bytes,
            deflate: None,
            descriptor: None,
            external: 0o100600 << 16,
            extra: &[],
        }
    }
}

pub fn archive(entries: &[Fixture<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let start = out.len() as u32;
        let payload = entry.deflate.unwrap_or(entry.bytes);
        let method = if entry.deflate.is_some() { 8 } else { 0 };
        let flags = 0x0800 | if entry.descriptor.is_some() { 8 } else { 0 };
        let crc = reference_crc(entry.bytes);
        dword(&mut out, 0x0403_4b50);
        word(&mut out, 20);
        word(&mut out, flags);
        word(&mut out, method);
        word(&mut out, 0);
        word(&mut out, 0);
        for value in [crc, payload.len() as u32, entry.bytes.len() as u32] {
            dword(&mut out, if entry.descriptor.is_some() { 0 } else { value });
        }
        word(&mut out, entry.name.len() as u16);
        word(&mut out, entry.extra.len() as u16);
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(entry.extra);
        out.extend_from_slice(payload);
        if let Some(signed) = entry.descriptor {
            if signed {
                dword(&mut out, 0x0807_4b50);
            }
            for value in [crc, payload.len() as u32, entry.bytes.len() as u32] {
                dword(&mut out, value);
            }
        }
        dword(&mut central, 0x0201_4b50);
        word(&mut central, (3 << 8) | 20);
        word(&mut central, 20);
        word(&mut central, flags);
        word(&mut central, method);
        word(&mut central, 0);
        word(&mut central, 0);
        for value in [crc, payload.len() as u32, entry.bytes.len() as u32] {
            dword(&mut central, value);
        }
        word(&mut central, entry.name.len() as u16);
        word(&mut central, entry.extra.len() as u16);
        word(&mut central, 0);
        word(&mut central, 0);
        word(&mut central, 0);
        dword(&mut central, entry.external);
        dword(&mut central, start);
        central.extend_from_slice(entry.name.as_bytes());
        central.extend_from_slice(entry.extra);
    }
    let directory_offset = out.len() as u32;
    let directory_length = central.len() as u32;
    out.extend_from_slice(&central);
    dword(&mut out, 0x0605_4b50);
    word(&mut out, 0);
    word(&mut out, 0);
    word(&mut out, entries.len() as u16);
    word(&mut out, entries.len() as u16);
    dword(&mut out, directory_length);
    dword(&mut out, directory_offset);
    word(&mut out, 0);
    out
}
pub fn stored_zip(name: &str, bytes: &[u8]) -> Vec<u8> {
    archive(&[Fixture::stored(name, bytes)])
}
pub fn hex(text: &str) -> Vec<u8> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
pub fn original_payload() -> Vec<u8> {
    let mut bytes =
        b"Mynou original archive fixture: 0123456789 and repeated media metadata.\n".repeat(1500);
    bytes.extend((0..16).flat_map(|_| 0..=255u8));
    bytes.extend(std::iter::repeat_n(b'Z', 32_768));
    bytes
}
