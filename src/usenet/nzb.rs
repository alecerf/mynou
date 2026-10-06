//! Strict NZB identities and bounded article inventories. All metadata is untrusted.
use crate::{
    Result,
    crypto::sha256,
    json::Value,
    xml::{Element, parse_xml},
};
use std::collections::{BTreeMap, BTreeSet};
use std::{io::Read, path::Path};

pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FILES: usize = 1024;
pub const MAX_SEGMENTS: usize = 32_768;
pub const MAX_ARTICLE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub number: u32,
    pub advertised_bytes: u64,
    /// Bare ASCII message ID, without angle brackets or command delimiters.
    pub message_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub subject: String,
    pub groups: Vec<String>,
    pub segments: Vec<Segment>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nzb {
    /// SHA-256 of the exact input bytes, not a claim about article contents.
    pub id: String,
    pub files: Vec<File>,
}
fn leaf(e: &Element) -> Result<&str> {
    if !e.children.is_empty() {
        return Err("NZB: nested leaf content is forbidden".into());
    }
    Ok(e.text.trim())
}
fn attributes(e: &Element, allowed: &[&str]) -> Result<()> {
    if e.attrs.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err("NZB: unsupported attribute".into());
    }
    Ok(())
}
fn number(e: &Element, k: &str, max: u64) -> Result<u64> {
    e.attrs
        .get(k)
        .filter(|s| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0 && *n <= max)
        .ok_or_else(|| format!("NZB: invalid {k}"))
}
pub fn valid_message_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 998
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'\\' | b'"'))
        && s.split_once('@')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('@'))
}
fn valid_group(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'))
}
impl Nzb {
    pub fn read_file(path: &Path) -> Result<Self> {
        Self::parse(&Self::read_bytes(path)?)
    }
    pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
        crate::store::reject_symlinks(path).map_err(|_| "NZB: invalid input path")?;
        let m = std::fs::symlink_metadata(path).map_err(|_| "NZB: cannot inspect input")?;
        if !m.is_file() || m.len() == 0 || m.len() > MAX_BYTES as u64 {
            return Err("NZB: input must be a bounded regular file".into());
        }
        let file = crate::store::private_options()
            .read(true)
            .open(path)
            .map_err(|_| "NZB: cannot open input")?;
        if !file
            .metadata()
            .map_err(|_| "NZB: cannot inspect input")?
            .is_file()
        {
            return Err("NZB: input must be a regular file".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "NZB: cannot read input")?;
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err("NZB: document size exceeds the limit".into());
        }
        Ok(bytes)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err("NZB: document size exceeds the limit".into());
        }
        let root =
            parse_xml(std::str::from_utf8(bytes).map_err(|_| "NZB: document must be UTF-8")?)
                .map_err(|_| "NZB: invalid or unsupported XML")?;
        if root.name != "nzb" || !root.text.trim().is_empty() {
            return Err("NZB: expected an nzb root".into());
        }
        attributes(&root, &["xmlns"])?;
        if root
            .attrs
            .get("xmlns")
            .is_some_and(|s| s != "http://www.newzbin.com/DTD/2003/nzb")
        {
            return Err("NZB: unsupported namespace".into());
        }
        let mut files = Vec::new();
        let mut identities = BTreeSet::new();
        let mut heads = 0;
        for e in &root.children {
            if e.name == "head" {
                heads += 1;
                attributes(e, &[])?;
                if heads > 1 || !e.text.trim().is_empty() || e.children.len() > 32 {
                    return Err("NZB: invalid head".into());
                }
                for m in &e.children {
                    attributes(m, &["type"])?;
                    if m.name != "meta"
                        || leaf(m)?.len() > 4096
                        || m.attrs
                            .get("type")
                            .is_none_or(|s| s.is_empty() || s.len() > 128)
                    {
                        return Err("NZB: invalid metadata".into());
                    }
                }
                continue;
            }
            if e.name != "file" || files.len() >= MAX_FILES || !e.text.trim().is_empty() {
                return Err("NZB: invalid file collection".into());
            }
            attributes(e, &["subject", "poster", "date"])?;
            let subject = e
                .attrs
                .get("subject")
                .filter(|s| {
                    !s.trim().is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control)
                })
                .ok_or("NZB: invalid subject")?
                .clone();
            if e.attrs
                .get("poster")
                .is_some_and(|s| s.len() > 2048 || s.chars().any(char::is_control))
            {
                return Err("NZB: invalid poster".into());
            }
            if let Some(date) = e.attrs.get("date")
                && (date.len() > 20
                    || date.is_empty()
                    || !date.bytes().all(|b| b.is_ascii_digit())
                    || date.parse::<u64>().is_err())
            {
                return Err("NZB: invalid date".into());
            }
            if e.children.len() != 2 {
                return Err("NZB: expected groups and segments".into());
            }
            let groups = e
                .children
                .iter()
                .filter(|c| c.name == "groups")
                .collect::<Vec<_>>();
            let segments = e
                .children
                .iter()
                .filter(|c| c.name == "segments")
                .collect::<Vec<_>>();
            if groups.len() != 1 || segments.len() != 1 {
                return Err("NZB: duplicate or missing collection".into());
            }
            let (g, s) = (groups[0], segments[0]);
            attributes(g, &[])?;
            attributes(s, &[])?;
            if !g.text.trim().is_empty()
                || !s.text.trim().is_empty()
                || g.children.is_empty()
                || g.children.len() > 32
                || s.children.is_empty()
                || s.children.len() > MAX_SEGMENTS
            {
                return Err("NZB: invalid groups or segments".into());
            }
            let mut group_names = BTreeSet::new();
            for e in &g.children {
                attributes(e, &[])?;
                let name = leaf(e)?;
                if e.name != "group" || !valid_group(name) || !group_names.insert(name.to_owned()) {
                    return Err("NZB: invalid or duplicate group".into());
                }
            }
            let mut parts = BTreeMap::new();
            for e in &s.children {
                attributes(e, &["bytes", "number"])?;
                let message_id = leaf(e)?;
                if e.name != "segment"
                    || !valid_message_id(message_id)
                    || identities.len() >= MAX_SEGMENTS
                    || !identities.insert(message_id.to_owned())
                {
                    return Err("NZB: invalid, duplicate or excessive article identity".into());
                }
                let part = Segment {
                    number: number(e, "number", MAX_SEGMENTS as u64)? as u32,
                    advertised_bytes: number(e, "bytes", MAX_ARTICLE_BYTES)?,
                    message_id: message_id.into(),
                };
                if parts.insert(part.number, part).is_some() {
                    return Err("NZB: duplicate segment number".into());
                }
            }
            if parts.keys().copied().ne(1..=parts.len() as u32) {
                return Err("NZB: segment numbers must be contiguous from one".into());
            }
            files.push(File {
                subject,
                groups: group_names.into_iter().collect(),
                segments: parts.into_values().collect(),
            });
        }
        if files.is_empty() {
            return Err("NZB: document contains no files".into());
        }
        let id = sha256(bytes).iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self { id, files })
    }
    /// Public inspection excludes subjects, posters, groups and message IDs.
    pub fn report(&self) -> Value {
        let mut v = Value::object();
        v.insert("id", self.id.clone());
        v.insert("file_count", self.files.len() as u32);
        v.insert(
            "segment_count",
            self.files.iter().map(|f| f.segments.len()).sum::<usize>() as u32,
        );
        v.insert(
            "advertised_bytes",
            self.files
                .iter()
                .flat_map(|f| &f.segments)
                .map(|s| s.advertised_bytes)
                .sum::<u64>()
                .to_string(),
        );
        v.insert("content_verified", false);
        v
    }
}
