//! Single-file verified private receipts and streamed output. No network or library writes.
mod disk;
use super::{
    nzb::Nzb,
    yenc::{Crc32, MAX_PART_BYTES, Part},
};
use crate::{
    Result,
    crypto::{Sha256, random_bytes},
    json::{self, Value},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
const MAX_FILE: u64 = 1 << 40;
#[derive(Clone)]
struct Summary {
    name: String,
    size: u64,
    total: u32,
    begin: u64,
    length: u64,
    file_crc: Option<u32>,
}
impl From<&Part> for Summary {
    fn from(p: &Part) -> Self {
        Self {
            name: p.name().into(),
            size: p.total_size(),
            total: p.total_parts(),
            begin: p.begin(),
            length: p.data().len() as u64,
            file_crc: p.file_crc(),
        }
    }
}
#[derive(Clone)]
struct Output {
    name: String,
    size: u64,
    crc: u32,
    sha: String,
    temp: String,
}
impl Output {
    fn json(&self) -> Value {
        let mut v = Value::object();
        v.insert("name", self.name.clone());
        v.insert("size", self.size.to_string());
        v.insert("crc", self.crc);
        v.insert("sha", self.sha.clone());
        v.insert("temp", self.temp.clone());
        v
    }
    fn parse(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["name", "size", "crc", "sha", "temp"])?;
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("Usenet workspace: invalid {k}"))
        };
        let r = Self {
            name: s("name")?,
            size: s("size")?
                .parse()
                .map_err(|_| "Usenet workspace: invalid output size")?,
            crc: v
                .get("crc")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or("Usenet workspace: invalid output CRC")?,
            sha: s("sha")?,
            temp: s("temp")?,
        };
        if !super::yenc::valid_name(&r.name)
            || r.size == 0
            || r.size > MAX_FILE
            || !crate::requesters::valid_digest(&r.sha)
            || !crate::requesters::valid_digest(&r.temp)
        {
            return Err("Usenet workspace: invalid output proof".into());
        }
        Ok(r)
    }
}
/// Retain original NZB bytes separately. They are required again when reopening.
/// Only private new workspace directories may be created; existing data is never adopted implicitly.
pub struct Workspace {
    root: PathBuf,
    _owner: File,
    plan: Value,
    binding: String,
    count: u32,
    articles: BTreeMap<u32, String>,
    max_file: u64,
    parts: BTreeMap<u32, Summary>,
    phase: String,
    output: Option<Output>,
    read_only: bool,
    poisoned: bool,
}
fn plan(
    source: &[u8],
    index: usize,
    server_binding: &str,
    max_file: u64,
) -> Result<(Value, BTreeMap<u32, String>)> {
    if !crate::requesters::valid_digest(server_binding) || max_file == 0 || max_file > MAX_FILE {
        return Err("Usenet workspace: invalid immutable limits or provider binding".into());
    }
    let nzb = Nzb::parse(source)?;
    let file = nzb
        .files
        .get(index)
        .ok_or("Usenet workspace: file index not present")?;
    let mut p = Value::object();
    p.insert("source_id", nzb.id);
    p.insert("file_index", index as u32);
    p.insert("server_binding", server_binding);
    p.insert("max_file", max_file.to_string());
    p.insert("parts", file.segments.len() as u32);
    Ok((
        p,
        file.segments
            .iter()
            .map(|s| (s.number, crate::requesters::digest(s.message_id.as_bytes())))
            .collect(),
    ))
}
fn directory(path: &Path) -> Result<()> {
    reject_symlinks(path).map_err(|_| "Usenet workspace: invalid directory path")?;
    let m = fs::symlink_metadata(path).map_err(|_| "Usenet workspace: cannot inspect directory")?;
    if !m.is_dir() {
        return Err("Usenet workspace: expected private directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err("Usenet workspace: directory must be private".into());
        }
    }
    Ok(())
}
fn clean_root(root: &Path) -> Result<PathBuf> {
    if root.file_name().is_none() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Usenet workspace: clean directory path required".into());
    }
    Ok(if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| "Usenet workspace: current directory unavailable")?
            .join(root)
    })
}

impl Workspace {
    pub fn create(
        root: &Path,
        source: &[u8],
        index: usize,
        server_binding: &str,
        max_file: u64,
    ) -> Result<Self> {
        let (p, articles) = plan(source, index, server_binding, max_file)?;
        let root = clean_root(root)?;
        let count = articles.len() as u32;
        if root.file_name().is_none()
            || root.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err("Usenet workspace: clean directory path required".into());
        }
        reject_symlinks(&root).map_err(|_| "Usenet workspace: invalid directory path")?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&root)
            .map_err(|_| "Usenet workspace: new private directory required")?;
        let owner = private_options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(root.join(".owner"))
            .map_err(|_| "Usenet workspace: cannot create owner")?;
        builder
            .create(root.join("output"))
            .map_err(|_| "Usenet workspace: cannot create private output directory")?;
        owner
            .try_lock()
            .map_err(|_| "Usenet workspace: owner unavailable")?;
        owner
            .sync_all()
            .map_err(|_| "Usenet workspace: owner synchronization failed")?;
        let binding = crate::requesters::digest(json::stringify(&p).as_bytes());
        let mut w = Self {
            root: root.clone(),
            _owner: owner,
            plan: p,
            binding,
            count,
            articles,
            max_file,
            parts: BTreeMap::new(),
            phase: "collecting".into(),
            output: None,
            read_only: false,
            poisoned: false,
        };
        w.save()?;
        sync_directory(root.parent().ok_or("Usenet workspace: missing parent")?)
            .map_err(|_| "Usenet workspace: parent synchronization failed")?;
        Ok(w)
    }
    pub fn open(
        root: &Path,
        source: &[u8],
        index: usize,
        server_binding: &str,
        max_file: u64,
        read_only: bool,
    ) -> Result<Self> {
        let (p, articles) = plan(source, index, server_binding, max_file)?;
        let root = clean_root(root)?;
        let count = articles.len() as u32;
        directory(&root)?;
        directory(&root.join("output"))?;
        let owner = disk::private_file(&root.join(".owner"), 0)?;
        owner
            .try_lock()
            .map_err(|_| "Usenet workspace: already owned")?;
        let (v, data) = disk::read_frame(&root.join("workspace.bin"), disk::DESCRIPTOR, 0)?;
        crate::numbering::only(&v, &["plan", "phase", "output"])?;
        if !data.is_empty() || v.get("plan") != Some(&p) {
            return Err("Usenet workspace: immutable source, provider or limits changed".into());
        }
        let phase = v
            .get("phase")
            .and_then(Value::as_str)
            .filter(|s| matches!(*s, "collecting" | "prepared" | "ready"))
            .ok_or("Usenet workspace: invalid phase")?
            .to_owned();
        let output = match v.get("output") {
            Some(Value::Null) => None,
            Some(v) => Some(Output::parse(v)?),
            None => return Err("Usenet workspace: missing output field".into()),
        };
        if (phase == "collecting") != output.is_none() {
            return Err("Usenet workspace: inconsistent output proof".into());
        }
        let binding = crate::requesters::digest(json::stringify(&p).as_bytes());
        let mut w = Self {
            root: root.clone(),
            _owner: owner,
            plan: p,
            binding,
            count,
            articles,
            max_file,
            parts: BTreeMap::new(),
            phase,
            output,
            read_only,
            poisoned: false,
        };
        for n in 1..=count {
            let path = w.part_path(n);
            match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("Usenet workspace: cannot inspect receipt".into()),
                Ok(_) => {
                    let part = w.read_part(n)?;
                    w.validate(&part)?;
                    w.parts.insert(n, Summary::from(&part));
                }
            }
        }
        w.recover()?;
        Ok(w)
    }
    fn part_path(&self, n: u32) -> PathBuf {
        self.root.join(format!("part-{n:05}.bin"))
    }
    fn writable(&self) -> Result<()> {
        if self.read_only || self.poisoned {
            Err("Usenet workspace: writable recovery required".into())
        } else {
            Ok(())
        }
    }
    fn save(&mut self) -> Result<()> {
        self.writable()?;
        let mut v = Value::object();
        v.insert("plan", self.plan.clone());
        v.insert("phase", self.phase.clone());
        v.insert(
            "output",
            self.output.as_ref().map_or(Value::Null, Output::json),
        );
        let result = disk::write_frame(&self.root.join("workspace.bin"), disk::DESCRIPTOR, &v, &[]);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn validate(&self, p: &Part) -> Result<()> {
        if p.number() == 0
            || p.number() > self.count
            || p.total_parts() != self.count
            || p.total_size() > self.max_file
        {
            return Err("Usenet workspace: part identity or size conflicts with inventory".into());
        }
        if let Some(other) = self.parts.values().next()
            && (other.name != p.name()
                || other.size != p.total_size()
                || other.total != p.total_parts())
        {
            return Err("Usenet workspace: conflicting decoded file identity".into());
        }
        Ok(())
    }
    fn read_part(&self, n: u32) -> Result<Part> {
        let (v, data) = disk::read_frame(&self.part_path(n), disk::RECEIPT, MAX_PART_BYTES)?;
        crate::numbering::only(&v, &["binding", "article", "part"])?;
        if v.get("binding").and_then(Value::as_str) != Some(self.binding.as_str())
            || v.get("article").and_then(Value::as_str) != self.articles.get(&n).map(String::as_str)
        {
            return Err("Usenet workspace: foreign receipt binding".into());
        }
        let p = Part::from_receipt(
            v.get("part")
                .ok_or("Usenet workspace: missing part proof")?,
            data,
        )?;
        if p.number() != n {
            return Err("Usenet workspace: receipt number mismatch".into());
        }
        Ok(p)
    }
    /// Idempotent only for exactly the same verified receipt. Conflicts never overwrite bytes.
    pub fn accept(&mut self, p: &Part, article_id: &str) -> Result<()> {
        self.writable()?;
        self.validate(p)?;
        if !super::nzb::valid_message_id(article_id)
            || self.articles.get(&p.number())
                != Some(&crate::requesters::digest(article_id.as_bytes()))
        {
            return Err("Usenet workspace: article identity does not match NZB inventory".into());
        }
        if self.parts.contains_key(&p.number()) {
            if self.read_part(p.number())? != *p {
                return Err("Usenet workspace: immutable receipt conflict".into());
            }
            return Ok(());
        }
        if self.phase != "collecting" {
            return Err("Usenet workspace: output already prepared".into());
        }
        if fs::symlink_metadata(self.part_path(p.number())).is_ok() {
            return Err(
                "Usenet workspace: unexpected existing receipt will not be overwritten".into(),
            );
        }
        let mut h = Value::object();
        h.insert("binding", self.binding.clone());
        h.insert("article", self.articles[&p.number()].clone());
        h.insert("part", p.receipt_header());
        let result = disk::write_frame(&self.part_path(p.number()), disk::RECEIPT, &h, p.data());
        if result.is_err() {
            self.poisoned = true;
            return result;
        }
        self.parts.insert(p.number(), Summary::from(p));
        Ok(())
    }
    fn complete(&self) -> Result<(&Summary, u32)> {
        let first = self
            .parts
            .get(&1)
            .ok_or("Usenet workspace: missing verified parts")?;
        if self.parts.len() != self.count as usize {
            return Err("Usenet workspace: missing verified parts".into());
        }
        let mut at = 0;
        for n in 1..=self.count {
            let p = self
                .parts
                .get(&n)
                .ok_or("Usenet workspace: missing verified part")?;
            if p.begin != at {
                return Err("Usenet workspace: overlap or gap".into());
            }
            at = at
                .checked_add(p.length)
                .ok_or("Usenet workspace: range overflow")?;
        }
        if at != first.size {
            return Err("Usenet workspace: incomplete byte coverage".into());
        }
        Ok((
            first,
            self.parts[&self.count]
                .file_crc
                .ok_or("Usenet workspace: missing whole-file CRC")?,
        ))
    }
    fn check_output(&self, path: &Path, o: &Output) -> Result<()> {
        let f = disk::private_file(path, self.max_file)?;
        if f.metadata()
            .map_err(|_| "Usenet workspace: output metadata unavailable")?
            .len()
            != o.size
        {
            return Err("Usenet workspace: output size mismatch".into());
        }
        let mut f = f.take(o.size + 1);
        let mut hash = Sha256::new();
        let mut crc = Crc32::default();
        let mut buf = [0_u8; 65536];
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|_| "Usenet workspace: output read failed")?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            crc.update(&buf[..n]);
        }
        let digest = hash
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if digest != o.sha || crc.finish() != o.crc {
            return Err("Usenet workspace: corrupt output".into());
        }
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        let Some(o) = self.output.clone() else {
            return Ok(());
        };
        let (first, crc) = self.complete()?;
        if o.name != first.name || o.size != first.size || o.crc != crc {
            return Err("Usenet workspace: output does not match verified receipts".into());
        }
        let final_path = self.root.join("output").join(&o.name);
        let temp = self.root.join(format!(".assembly-{}.tmp", o.temp));
        match fs::symlink_metadata(&final_path) {
            Ok(_) => self.check_output(&final_path, &o)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.phase == "prepared" => {
                self.check_output(&temp, &o)?;
                if self.read_only {
                    return Ok(());
                }
                fs::rename(&temp, &final_path)
                    .map_err(|_| "Usenet workspace: output publication failed")?;
            }
            _ => return Err("Usenet workspace: verified output is missing".into()),
        }
        if self.phase == "prepared" && !self.read_only {
            if sync_directory(&self.root.join("output")).is_err() {
                self.poisoned = true;
                return Err(
                    "Usenet workspace: output durability uncertain; reopen required".into(),
                );
            }
            self.phase = "ready".into();
            self.save()?;
        }
        Ok(())
    }
    pub fn assemble(&mut self) -> Result<PathBuf> {
        self.writable()?;
        if self.phase != "collecting" {
            self.recover()?;
            return self
                .available_file()
                .ok_or_else(|| "Usenet workspace: output remains unpublished".into());
        }
        let (first, expected_crc) = self.complete()?;
        let name = first.name.clone();
        let size = first.size;
        let final_path = self.root.join("output").join(&name);
        if fs::symlink_metadata(&final_path).is_ok() {
            return Err("Usenet workspace: existing output will not be overwritten".into());
        }
        let temp_id = crate::requesters::digest(&random_bytes::<16>()?);
        let temp = self.root.join(format!(".assembly-{temp_id}.tmp"));
        let mut file = private_options()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|_| "Usenet workspace: cannot create exclusive output")?;
        let result = (|| -> Result<Output> {
            let mut crc = Crc32::default();
            let mut hash = Sha256::new();
            for n in 1..=self.count {
                let p = self.read_part(n)?;
                self.validate(&p)?;
                let known = &self.parts[&n];
                if p.begin() != known.begin
                    || p.data().len() as u64 != known.length
                    || p.file_crc() != known.file_crc
                {
                    return Err("Usenet workspace: receipt changed before assembly".into());
                }
                for bytes in p.data().chunks(65536) {
                    file.write_all(bytes)
                        .map_err(|_| "Usenet workspace: output write failed")?;
                    crc.update(bytes);
                    hash.update(bytes);
                }
            }
            if crc.finish() != expected_crc
                || file
                    .metadata()
                    .map_err(|_| "Usenet workspace: output metadata failed")?
                    .len()
                    != size
            {
                return Err("Usenet workspace: whole-file integrity mismatch".into());
            }
            file.sync_all()
                .map_err(|_| "Usenet workspace: output synchronization failed")?;
            Ok(Output {
                name,
                size,
                crc: expected_crc,
                sha: hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
                temp: temp_id,
            })
        })();
        match result {
            Ok(o) => {
                self.output = Some(o);
                self.phase = "prepared".into();
                self.save()?;
                self.recover()?;
                self.available_file()
                    .ok_or_else(|| "Usenet workspace: output not ready".into())
            }
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(e)
            }
        }
    }
    pub fn available_file(&self) -> Option<PathBuf> {
        if self.phase == "ready" && !self.poisoned {
            self.output
                .as_ref()
                .map(|o| self.root.join("output").join(&o.name))
        } else {
            None
        }
    }
    pub(super) fn verify_ready(&mut self) -> Result<PathBuf> {
        let result = (|| {
            let mut parts = BTreeMap::new();
            for n in 1..=self.count {
                let p = self.read_part(n)?;
                self.validate(&p)?;
                parts.insert(n, Summary::from(&p));
            }
            self.parts = parts;
            self.recover()?;
            self.available_file()
                .ok_or_else(|| "Usenet workspace: verified output is unavailable".into())
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    pub(super) fn first_missing(&self) -> Option<u32> {
        (1..=self.count).find(|n| !self.parts.contains_key(n))
    }
    pub(super) fn verified_numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.parts.keys().copied()
    }
    pub fn report(&self) -> Value {
        let mut v = Value::object();
        v.insert("binding", self.binding.clone());
        v.insert(
            "phase",
            if self.poisoned {
                "recovery_required"
            } else {
                self.phase.as_str()
            },
        );
        v.insert("verified_parts", self.parts.len() as u32);
        v.insert("total_parts", self.count);
        v.insert("ready", self.phase == "ready" && !self.poisoned);
        v
    }
}
