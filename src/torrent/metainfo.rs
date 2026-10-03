use crate::{
    Result,
    bencode::Value,
    crypto::{sha1, sha256},
};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

pub const BLOCK: usize = 16 * 1024;
pub const MAX_META: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct MediaFile {
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
    pub padding: bool,
    pub root: Option<[u8; 32]>,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub info: Vec<u8>,
    pub encoded: Vec<u8>,
    pub v1: Option<[u8; 20]>,
    pub v2: Option<[u8; 32]>,
    pub private: bool,
    pub piece_length: usize,
    pub pieces: Vec<Option<[u8; 20]>>,
    pub v2_pieces: Vec<Option<[u8; 32]>>,
    pub files: Vec<MediaFile>,
    pub total: u64,
    pub trackers: Vec<String>,
}

fn dict(v: &Value) -> Result<&BTreeMap<Vec<u8>, Value>> {
    if let Value::Dict(v) = v {
        Ok(v)
    } else {
        Err("Expected a torrent dictionary".into())
    }
}
pub fn bytes(v: &Value) -> Result<&[u8]> {
    if let Value::Bytes(v) = v {
        Ok(v)
    } else {
        Err("Expected torrent bytes".into())
    }
}
pub fn integer(v: &Value) -> Result<i64> {
    if let Value::Int(v) = v {
        Ok(*v)
    } else {
        Err("Expected a torrent integer".into())
    }
}
fn required<'a>(d: &'a BTreeMap<Vec<u8>, Value>, key: &[u8]) -> Result<&'a Value> {
    d.get(key)
        .ok_or_else(|| "Missing torrent metadata field".into())
}
fn positive(v: &Value) -> Result<u64> {
    u64::try_from(integer(v)?).map_err(|_| "Negative torrent length".into())
}
fn component(v: &[u8]) -> Result<String> {
    let s = std::str::from_utf8(v).map_err(|_| "Torrent file name is not valid UTF-8")?;
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\\', '\0', ':']) || s.len() > 255
    {
        return Err("Torrent path is not allowed".into());
    }
    Ok(s.to_owned())
}
pub fn confined(base: &Path, path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("Torrent path is not allowed".into());
    }
    let mut current = base.to_path_buf();
    for part in path.components() {
        current.push(part);
        if let Ok(m) = std::fs::symlink_metadata(&current)
            && (m.file_type().is_symlink() || (!m.is_dir() && !m.is_file()))
        {
            return Err("Links and special files are not allowed in downloads".into());
        }
    }
    Ok(current)
}

// Extract the original info dictionary bytes: re-encoding would change noncanonical hashes.
pub fn raw_info(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() > MAX_META {
        return Err("Torrent metadata is too large".into());
    }
    crate::bencode::parse_info_raw(input)
}

impl Meta {
    pub fn parse(encoded: &[u8]) -> Result<Self> {
        let info = raw_info(encoded)?;
        Self::from_info(info, encoded.to_vec())
    }
    pub fn from_info(info: Vec<u8>, encoded: Vec<u8>) -> Result<Self> {
        if info.len() > MAX_META {
            return Err("Torrent metadata is too large".into());
        }
        let root = crate::bencode::parse(&encoded)?;
        let top = dict(&root)?;
        let value = crate::bencode::parse(&info)?;
        let d = dict(&value)?;
        let name = component(bytes(required(d, b"name")?)?)?;
        let piece_length = usize::try_from(positive(required(d, b"piece length")?)?)
            .map_err(|_| "Piece length exceeds limits")?;
        if !(BLOCK..=16 * 1024 * 1024).contains(&piece_length) || !piece_length.is_power_of_two() {
            return Err("Torrent piece length is not allowed".into());
        }
        let private = match d
            .get(b"private".as_slice())
            .map(integer)
            .transpose()?
            .unwrap_or(0)
        {
            0 => false,
            1 => true,
            _ => return Err("Invalid torrent private flag".into()),
        };
        let v2 = d
            .get(b"meta version".as_slice())
            .map(integer)
            .transpose()?
            .map(|v| {
                if v == 2 {
                    Ok(sha256(&info))
                } else {
                    Err("Unknown torrent version")
                }
            })
            .transpose()?;
        let mut files = Vec::new();
        let mut v1_hashes = Vec::new();
        let mut total = 0u64;
        let v1 = if let Some(p) = d.get(b"pieces".as_slice()) {
            let p = bytes(p)?;
            if p.len() % 20 != 0 {
                return Err("Invalid v1 hash list".into());
            }
            for h in p.as_chunks::<20>().0 {
                let mut a = [0; 20];
                a.copy_from_slice(h);
                v1_hashes.push(Some(a));
            }
            if let Some(list) = d.get(b"files".as_slice()) {
                let Value::List(list) = list else {
                    return Err("Invalid torrent file list".into());
                };
                if list.len() > 100_000 {
                    return Err("Too many torrent files".into());
                }
                for entry in list {
                    let e = dict(entry)?;
                    let length = positive(required(e, b"length")?)?;
                    let Value::List(parts) = required(e, b"path")? else {
                        return Err("Invalid torrent path".into());
                    };
                    if parts.is_empty() || parts.len() > 64 {
                        return Err("Invalid torrent path".into());
                    }
                    let mut path = PathBuf::from(&name);
                    for part in parts {
                        path.push(component(bytes(part)?)?);
                    }
                    let padding = e
                        .get(b"attr".as_slice())
                        .map(bytes)
                        .transpose()?
                        .is_some_and(|v| v.contains(&b'p'));
                    files.push(MediaFile {
                        path,
                        offset: total,
                        length,
                        padding,
                        root: None,
                    });
                    total = total
                        .checked_add(length)
                        .ok_or("Torrent size exceeds limits")?;
                }
            } else {
                total = positive(required(d, b"length")?)?;
                files.push(MediaFile {
                    path: PathBuf::from(&name),
                    offset: 0,
                    length: total,
                    padding: false,
                    root: None,
                });
            }
            if total.div_ceil(piece_length as u64) != v1_hashes.len() as u64 {
                return Err("V1 piece count mismatch".into());
            }
            Some(sha1(&info))
        } else {
            None
        };
        let mut v2_files = Vec::new();
        if v2.is_some() {
            fn walk(
                node: &Value,
                path: &Path,
                out: &mut Vec<MediaFile>,
                offset: &mut u64,
                piece: usize,
                depth: usize,
            ) -> Result<()> {
                if depth > 64 || out.len() > 100_000 {
                    return Err("V2 file tree exceeds limits".into());
                }
                let d = dict(node)?;
                if let Some(leaf) = d.get(b"".as_slice()) {
                    if d.len() != 1 {
                        return Err("Ambiguous v2 file tree".into());
                    }
                    let leaf = dict(leaf)?;
                    let length = positive(required(leaf, b"length")?)?;
                    let root = if length == 0 {
                        None
                    } else {
                        let root = bytes(required(leaf, b"pieces root")?)?;
                        if root.len() != 32 {
                            return Err("Invalid v2 Merkle root".into());
                        }
                        let mut hash = [0; 32];
                        hash.copy_from_slice(root);
                        Some(hash)
                    };
                    let padding = leaf
                        .get(b"attr".as_slice())
                        .map(bytes)
                        .transpose()?
                        .is_some_and(|v| v.contains(&b'p'));
                    out.push(MediaFile {
                        path: path.to_path_buf(),
                        offset: *offset,
                        length,
                        padding,
                        root,
                    });
                    *offset = offset
                        .checked_add(length.div_ceil(piece as u64) * piece as u64)
                        .ok_or("Torrent size exceeds limits")?;
                } else {
                    for (part, child) in d {
                        walk(
                            child,
                            &path.join(component(part)?),
                            out,
                            offset,
                            piece,
                            depth + 1,
                        )?;
                    }
                }
                Ok(())
            }
            let mut offset = 0;
            walk(
                required(d, b"file tree")?,
                Path::new(""),
                &mut v2_files,
                &mut offset,
                piece_length,
                0,
            )?;
            let single = v2_files.len() == 1 && v2_files[0].path == Path::new(&name);
            if !single {
                for f in &mut v2_files {
                    f.path = Path::new(&name).join(&f.path);
                }
            }
            if v1.is_none() {
                files = v2_files.clone();
                total = files.iter().map(|f| f.offset + f.length).max().unwrap_or(0);
            } else {
                let real: Vec<_> = files.iter().filter(|f| !f.padding).collect();
                if real.len() != v2_files.len() {
                    return Err("Incompatible hybrid file lists".into());
                }
                for (a, b) in real.into_iter().zip(&v2_files) {
                    if a.path != b.path || a.length != b.length || a.offset != b.offset {
                        return Err("Incompatible hybrid file position".into());
                    }
                }
                for f in &mut files {
                    f.root = v2_files
                        .iter()
                        .find(|v| v.path == f.path)
                        .and_then(|v| v.root);
                }
            }
        }
        if v1.is_none() && v2.is_none() {
            return Err("Torrent has neither a v1 nor a v2 hash".into());
        }
        if total > 16 * 1024 * 1024 * 1024 * 1024u64 {
            return Err("Torrent size exceeds limits".into());
        }
        let mut unique = std::collections::BTreeSet::new();
        for f in &files {
            if !unique.insert(f.path.clone()) {
                return Err("Duplicate torrent path".into());
            }
        }
        let count = total.div_ceil(piece_length as u64) as usize;
        if count > 1_048_576 {
            return Err("Too many torrent pieces".into());
        }
        let mut v2_pieces = vec![None; count];
        let layers = top.get(b"piece layers".as_slice()).map(dict).transpose()?;
        for f in &v2_files {
            let Some(root) = f.root else { continue };
            let start = (f.offset / piece_length as u64) as usize;
            let n = f.length.div_ceil(piece_length as u64) as usize;
            if n == 1 {
                v2_pieces[start] = Some(root);
            } else if let Some(layer) = layers.and_then(|l| l.get(root.as_slice())) {
                let layer = bytes(layer)?;
                if layer.len() != n * 32 {
                    return Err("V2 piece layer mismatch".into());
                }
                let mut hashes = Vec::with_capacity(n);
                for h in layer.as_chunks::<32>().0 {
                    let mut a = [0; 32];
                    a.copy_from_slice(h);
                    hashes.push(a);
                }
                if merkle_hashes(&hashes, zero_hash(piece_length / BLOCK)) != root {
                    return Err("V2 piece layer has not been authenticated".into());
                }
                for (i, h) in hashes.into_iter().enumerate() {
                    v2_pieces[start + i] = Some(h);
                }
            }
        }
        if v1_hashes.is_empty() {
            v1_hashes.resize(count, None);
        }
        let mut trackers = Vec::new();
        if let Some(t) = top.get(b"announce".as_slice())
            && let Ok(t) = std::str::from_utf8(bytes(t)?)
        {
            trackers.push(t.to_owned());
        }
        if let Some(Value::List(tiers)) = top.get(b"announce-list".as_slice()) {
            for tier in tiers {
                if let Value::List(urls) = tier {
                    for u in urls {
                        if let Ok(u) = std::str::from_utf8(bytes(u)?)
                            && !trackers.iter().any(|v| v == u)
                        {
                            trackers.push(u.to_owned());
                        }
                    }
                }
            }
        }
        if trackers.len() > 256 {
            return Err("Too many torrent trackers".into());
        }
        Ok(Self {
            info,
            encoded,
            v1,
            v2,
            private,
            piece_length,
            pieces: v1_hashes,
            v2_pieces,
            files,
            total,
            trackers,
        })
    }
    pub fn count(&self) -> usize {
        self.pieces.len()
    }
    pub fn piece_size(&self, index: usize) -> usize {
        if self.v1.is_none() {
            return self
                .v2_file(index)
                .map(|f| {
                    f.length
                        .saturating_sub(index as u64 * self.piece_length as u64 - f.offset)
                        .min(self.piece_length as u64) as usize
                })
                .unwrap_or(0);
        }
        (self
            .total
            .saturating_sub(index as u64 * self.piece_length as u64))
        .min(self.piece_length as u64) as usize
    }
    pub fn wire_piece_size(&self, index: usize, v2_wire: bool) -> usize {
        if v2_wire {
            self.v2_file(index)
                .map(|f| {
                    f.length
                        .saturating_sub(index as u64 * self.piece_length as u64 - f.offset)
                        .min(self.piece_length as u64) as usize
                })
                .unwrap_or(0)
        } else {
            self.piece_size(index)
        }
    }
    pub fn v2_file(&self, piece: usize) -> Option<&MediaFile> {
        let offset = piece as u64 * self.piece_length as u64;
        self.files
            .iter()
            .find(|f| !f.padding && f.offset <= offset && offset < f.offset + f.length)
    }
    pub fn verify(&self, piece: usize, data: &[u8]) -> bool {
        if data.len() != self.piece_size(piece) {
            return false;
        }
        if let Some(expected) = self.pieces.get(piece).copied().flatten()
            && sha1(data) != expected
        {
            return false;
        }
        if self.v2.is_some() {
            let Some(f) = self.v2_file(piece) else {
                return self.pieces[piece].is_some();
            };
            let actual_len = f
                .length
                .saturating_sub(piece as u64 * self.piece_length as u64 - f.offset)
                .min(self.piece_length as u64) as usize;
            let pad_blocks = if f.length <= self.piece_length as u64 {
                actual_len.div_ceil(BLOCK).next_power_of_two()
            } else {
                self.piece_length / BLOCK
            };
            let actual = merkle_data(&data[..actual_len], pad_blocks);
            if let Some(expected) = self.v2_pieces[piece] {
                if actual != expected {
                    return false;
                }
            } else if self.v1.is_none() {
                return false;
            }
        }
        true
    }
}

pub fn pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut data = [0; 64];
    data[..32].copy_from_slice(a);
    data[32..].copy_from_slice(b);
    sha256(&data)
}
pub fn zero_hash(blocks: usize) -> [u8; 32] {
    let mut h = [0; 32];
    let mut n = 1;
    while n < blocks {
        h = pair(&h, &h);
        n *= 2;
    }
    h
}
pub fn merkle_hashes(hashes: &[[u8; 32]], padding: [u8; 32]) -> [u8; 32] {
    if hashes.is_empty() {
        return padding;
    }
    let mut h = hashes.to_vec();
    h.resize(h.len().next_power_of_two(), padding);
    while h.len() > 1 {
        h = h
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| pair(&v[0], &v[1]))
            .collect();
    }
    h[0]
}
pub fn merkle_data(data: &[u8], blocks: usize) -> [u8; 32] {
    let mut h: Vec<_> = data.chunks(BLOCK).map(sha256).collect();
    h.resize(blocks.max(1), [0; 32]);
    merkle_hashes(&h, [0; 32])
}
