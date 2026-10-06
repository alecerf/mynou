//! Original RAR5 fixture encoder from the published structure specification.
#![allow(dead_code)]
use super::archive_support::reference_crc;

pub const SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";
pub fn vint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        bytes.push(byte | if value != 0 { 128 } else { 0 });
        if value == 0 {
            return bytes;
        }
    }
}
pub fn block(body: &[u8]) -> Vec<u8> {
    let mut checked = vint(body.len() as u64);
    checked.extend_from_slice(body);
    let mut out = reference_crc(&checked).to_le_bytes().to_vec();
    out.extend(checked);
    out
}
pub fn header(kind: u64, flags: u64, fields: &[u8]) -> Vec<u8> {
    let mut body = vint(kind);
    body.extend(vint(flags));
    body.extend_from_slice(fields);
    block(&body)
}
pub struct Fixture<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub common_flags: u64,
    pub file_flags: u64,
    pub attributes: u64,
    pub host: u64,
    pub compression: u64,
    pub declared: Option<u64>,
    pub expected_crc: Option<u32>,
    pub extra: Vec<u8>,
}
impl<'a> Fixture<'a> {
    pub fn stored(name: &'a str, bytes: &'a [u8]) -> Self {
        Self {
            name,
            bytes,
            common_flags: 2,
            file_flags: 4,
            attributes: 0o100600,
            host: 1,
            compression: 0,
            declared: None,
            expected_crc: None,
            extra: Vec::new(),
        }
    }
    pub fn directory(name: &'a str) -> Self {
        Self {
            file_flags: 1,
            attributes: 0o040700,
            common_flags: 0,
            ..Self::stored(name, &[])
        }
    }
}
pub fn file_body(entry: &Fixture<'_>) -> Vec<u8> {
    let mut body = vint(2);
    body.extend(vint(entry.common_flags));
    if entry.common_flags & 1 != 0 {
        body.extend(vint(entry.extra.len() as u64));
    }
    if entry.common_flags & 2 != 0 {
        body.extend(vint(entry.bytes.len() as u64));
    }
    body.extend(vint(entry.file_flags));
    body.extend(vint(entry.declared.unwrap_or(entry.bytes.len() as u64)));
    body.extend(vint(entry.attributes));
    if entry.file_flags & 2 != 0 {
        body.extend_from_slice(&123456_u32.to_le_bytes());
    }
    if entry.file_flags & 4 != 0 {
        body.extend_from_slice(
            &entry
                .expected_crc
                .unwrap_or_else(|| reference_crc(entry.bytes))
                .to_le_bytes(),
        );
    }
    body.extend(vint(entry.compression));
    body.extend(vint(entry.host));
    body.extend(vint(entry.name.len() as u64));
    body.extend_from_slice(entry.name.as_bytes());
    body.extend_from_slice(&entry.extra);
    body
}
pub fn archive(entries: &[Fixture<'_>]) -> Vec<u8> {
    let mut bytes = SIGNATURE.to_vec();
    bytes.extend(header(1, 0, &[0]));
    for entry in entries {
        bytes.extend(block(&file_body(entry)));
        bytes.extend_from_slice(entry.bytes);
    }
    bytes.extend(header(5, 0, &[0]));
    bytes
}
pub fn stored(name: &str, bytes: &[u8]) -> Vec<u8> {
    archive(&[Fixture::stored(name, bytes)])
}
