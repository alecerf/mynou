//! Original yEnc decoding with mandatory CRC checks and exact multipart coverage.
use crate::Result;
use std::collections::BTreeMap;
pub const MAX_PART_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ASSEMBLED_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 1 << 40;
const MAX_PARTS: u32 = 32_768;
const MAX_PREAMBLE: usize = 16 * 1024;
const MAX_HEADER: usize = 4096;

const fn table() -> [u32; 256] {
    let mut out = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut n = i as u32;
        let mut b = 0;
        while b < 8 {
            n = (n >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(n & 1));
            b += 1;
        }
        out[i] = n;
        i += 1;
    }
    out
}
const CRC_TABLE: [u32; 256] = table();
#[derive(Clone, Copy)]
pub struct Crc32(u32);
impl Default for Crc32 {
    fn default() -> Self {
        Self(u32::MAX)
    }
}
impl Crc32 {
    pub fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 >> 8) ^ CRC_TABLE[usize::from((self.0 as u8) ^ b)];
        }
    }
    pub fn finish(self) -> u32 {
        !self.0
    }
}
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = Crc32::default();
    c.update(bytes);
    c.finish()
}

/// Constructed only after decoding and the article's CRC verification succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    name: String,
    total_size: u64,
    number: u32,
    total_parts: u32,
    begin: u64,
    data: Vec<u8>,
    file_crc: Option<u32>,
}
impl Part {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn total_size(&self) -> u64 {
        self.total_size
    }
    pub fn number(&self) -> u32 {
        self.number
    }
    pub fn total_parts(&self) -> u32 {
        self.total_parts
    }
    /// Zero-based byte offset in the decoded file.
    pub fn begin(&self) -> u64 {
        self.begin
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn file_crc(&self) -> Option<u32> {
        self.file_crc
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedFile {
    name: String,
    data: Vec<u8>,
    crc: u32,
}
impl VerifiedFile {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn crc32(&self) -> u32 {
        self.crc
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.trim() == s
        && s != "."
        && s != ".."
        && !s.ends_with('.')
        && !s.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}
fn fields<'a>(
    line: &'a [u8],
    prefix: &str,
    allowed: &[&str],
    filename: bool,
) -> Result<BTreeMap<&'a str, &'a str>> {
    if line.len() > MAX_HEADER {
        return Err("yEnc: header exceeds the limit".into());
    }
    let text = std::str::from_utf8(line).map_err(|_| "yEnc: header must be UTF-8")?;
    let mut rest = text.strip_prefix(prefix).ok_or("yEnc: invalid header")?;
    let mut map = BTreeMap::new();
    while !rest.is_empty() {
        let (key, tail) = rest.split_once('=').ok_or("yEnc: invalid field")?;
        if !allowed.contains(&key) {
            return Err("yEnc: unsupported field".into());
        }
        let (value, next) = if filename && key == "name" {
            (tail, "")
        } else {
            tail.split_once(' ').unwrap_or((tail, ""))
        };
        if value.is_empty() || map.insert(key, value).is_some() {
            return Err("yEnc: duplicate or empty field".into());
        }
        rest = next;
    }
    Ok(map)
}
fn num(m: &BTreeMap<&str, &str>, k: &str, max: u64) -> Result<u64> {
    m.get(k)
        .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0 && *n <= max)
        .ok_or_else(|| format!("yEnc: invalid {k}"))
}
fn checksum(m: &BTreeMap<&str, &str>, k: &str) -> Result<Option<u32>> {
    m.get(k)
        .map(|s| {
            if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("yEnc: invalid CRC".into());
            }
            u32::from_str_radix(s, 16).map_err(|_| "yEnc: invalid CRC".into())
        })
        .transpose()
}
fn line<'a>(bytes: &'a [u8], at: &mut usize, max: usize) -> Result<&'a [u8]> {
    let rest = &bytes[*at..];
    let n = rest
        .iter()
        .take(max.saturating_add(2))
        .position(|b| *b == b'\n')
        .ok_or("yEnc: missing or excessive line ending")?;
    if n == 0 || rest[n - 1] != b'\r' || n - 1 > max {
        return Err("yEnc: expected bounded CRLF lines".into());
    }
    *at += n + 1;
    Ok(&rest[..n - 1])
}
pub fn decode(bytes: &[u8]) -> Result<Part> {
    if bytes.is_empty() || bytes.len() > 2 * MAX_PART_BYTES + MAX_PREAMBLE + 3 * MAX_HEADER {
        return Err("yEnc: article size exceeds the limit".into());
    }
    let mut at = 0;
    let first = loop {
        let l = line(bytes, &mut at, MAX_HEADER)?;
        if l.starts_with(b"=ybegin ") {
            break l;
        }
        if at > MAX_PREAMBLE {
            return Err("yEnc: missing bounded header".into());
        }
    };
    let header = fields(
        first,
        "=ybegin ",
        &["line", "size", "part", "total", "name"],
        true,
    )?;
    let width = num(&header, "line", 998)? as usize;
    let total_size = num(&header, "size", MAX_FILE_BYTES)?;
    let name = header
        .get("name")
        .filter(|s| valid_name(s))
        .ok_or("yEnc: invalid file name")?
        .to_string();
    let multipart = header.contains_key("part") || header.contains_key("total");
    let (number, total_parts, begin, end) = if multipart {
        let number = num(&header, "part", u64::from(MAX_PARTS))? as u32;
        let total = num(&header, "total", u64::from(MAX_PARTS))? as u32;
        if number > total {
            return Err("yEnc: invalid part number".into());
        }
        let p = fields(
            line(bytes, &mut at, MAX_HEADER)?,
            "=ypart ",
            &["begin", "end"],
            false,
        )?;
        let begin = num(&p, "begin", total_size)?;
        let end = num(&p, "end", total_size)?;
        if begin > end {
            return Err("yEnc: invalid part range".into());
        }
        (number, total, begin - 1, end)
    } else {
        (1, 1, 0, total_size)
    };
    let size = usize::try_from(end - begin)
        .ok()
        .filter(|n| *n > 0 && *n <= MAX_PART_BYTES)
        .ok_or("yEnc: decoded part exceeds the limit")?;
    let mut data = Vec::with_capacity(size);
    let footer = loop {
        let l = line(bytes, &mut at, MAX_HEADER)?;
        if l.starts_with(b"=yend ") {
            break fields(l, "=yend ", &["size", "part", "pcrc32", "crc32"], false)?;
        }
        if l.is_empty() || l.len() > width + 1 {
            return Err("yEnc: invalid data line length".into());
        }
        let mut i = 0;
        while i < l.len() {
            let encoded = if l[i] == b'=' {
                i += 1;
                let b = *l.get(i).ok_or("yEnc: incomplete escape")?;
                b.wrapping_sub(64)
            } else {
                if matches!(l[i], 0 | b'\r' | b'\n') {
                    return Err("yEnc: unescaped control byte".into());
                }
                l[i]
            };
            if data.len() >= size {
                return Err("yEnc: decoded data exceeds the declared range".into());
            }
            data.push(encoded.wrapping_sub(42));
            i += 1;
        }
    };
    if bytes.len() - at > MAX_PREAMBLE || bytes[at..].iter().any(|b| !b.is_ascii_whitespace()) {
        return Err("yEnc: trailing payload is forbidden".into());
    }
    if data.len() != size || num(&footer, "size", MAX_PART_BYTES as u64)? != size as u64 {
        return Err("yEnc: decoded size mismatch".into());
    }
    let file_crc = checksum(&footer, "crc32")?;
    let part_crc = checksum(&footer, "pcrc32")?;
    if multipart {
        if num(&footer, "part", u64::from(MAX_PARTS))? != u64::from(number)
            || part_crc != Some(crc32(&data))
        {
            return Err("yEnc: part identity or CRC mismatch".into());
        }
        if (number == total_parts && file_crc.is_none())
            || (number != total_parts && file_crc.is_some())
        {
            return Err("yEnc: whole-file CRC belongs on the final part".into());
        }
    } else if footer.contains_key("part") || part_crc.is_some() || file_crc != Some(crc32(&data)) {
        return Err("yEnc: file CRC mismatch".into());
    }
    Ok(Part {
        name,
        total_size,
        number,
        total_parts,
        begin,
        data,
        file_crc,
    })
}
/// Assemble only exact coverage with all article CRCs and a matching whole-file CRC.
/// CRC detects accidental corruption; it is not cryptographic media identity.
pub fn assemble(mut parts: Vec<Part>) -> Result<VerifiedFile> {
    if parts.is_empty() || parts.len() > MAX_PARTS as usize {
        return Err("yEnc: invalid part collection".into());
    }
    parts.sort_by_key(Part::number);
    let first = &parts[0];
    let name = first.name.clone();
    let size = first.total_size;
    if size > MAX_ASSEMBLED_BYTES as u64 || parts.len() != first.total_parts as usize {
        return Err("yEnc: missing parts or in-memory assembly limit exceeded".into());
    }
    let count = first.total_parts;
    let mut data = Vec::with_capacity(size as usize);
    let mut crc = Crc32::default();
    let mut whole = None;
    for (i, p) in parts.into_iter().enumerate() {
        if p.name != name
            || p.total_size != size
            || p.total_parts != count
            || p.number as usize != i + 1
            || p.begin != data.len() as u64
        {
            return Err("yEnc: conflicting parts, overlap or gap".into());
        }
        if p.file_crc.is_some() {
            whole = p.file_crc;
        }
        crc.update(&p.data);
        data.extend_from_slice(&p.data);
    }
    let crc = crc.finish();
    if data.len() as u64 != size || whole != Some(crc) {
        return Err("yEnc: incomplete file or whole-file CRC mismatch".into());
    }
    Ok(VerifiedFile { name, data, crc })
}
