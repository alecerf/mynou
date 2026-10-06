//! Captured archive provenance and private resumable extraction. No library imports.
use super::{queue::Owner, workspace::disk};
use crate::{
    Result,
    archive::{Limits, Method, Rar5, VerifiedEntry, Zip, deflate},
    crypto::Sha256,
    json::Value,
    requesters::valid_digest,
    store::{private_options, reject_symlinks, sync_directory},
    usenet::yenc::Crc32,
};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
const MAGIC: &[u8; 8] = b"MYNOUA01";
const RAR_MAGIC: &[u8; 8] = b"MYNOUA02";
const CHUNK: usize = 65_536;

/// An explicit format identity; legacy ZIP plans omit it in their serialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Rar5,
}
impl Format {
    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        match path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("zip") => Ok(Self::Zip),
            Some("rar") => Ok(Self::Rar5),
            _ => Err("Usenet archive: unsupported source extension".into()),
        }
    }
    fn magic(self) -> &'static [u8; 8] {
        match self {
            Self::Zip => MAGIC,
            Self::Rar5 => RAR_MAGIC,
        }
    }
}

pub(crate) enum Parsed {
    Zip(Zip),
    Rar5(Rar5),
}
pub(crate) struct Entry<'a> {
    pub name: &'a str,
    pub directory: bool,
    pub bytes: u64,
    pub compressed: u64,
    pub crc: u32,
    pub method: Method,
}
impl Parsed {
    pub(crate) fn read<R: Read + Seek>(
        input: &mut R,
        format: Format,
        limits: Limits,
        flag: &AtomicBool,
    ) -> Result<Self> {
        match format {
            Format::Zip => Zip::read_cancellable(input, limits, flag).map(Self::Zip),
            Format::Rar5 => Rar5::read_cancellable(input, limits, flag).map(Self::Rar5),
        }
    }
    fn format(&self) -> Format {
        match self {
            Self::Zip(_) => Format::Zip,
            Self::Rar5(_) => Format::Rar5,
        }
    }
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Zip(a) => a.entries().len(),
            Self::Rar5(a) => a.entries().len(),
        }
    }
    pub(crate) fn entry(&self, index: usize) -> Result<Entry<'_>> {
        match self {
            Self::Zip(a) => {
                let e = a
                    .entries()
                    .get(index)
                    .ok_or("Usenet archive: selected ZIP entry is absent")?;
                Ok(Entry {
                    name: e.name(),
                    directory: e.is_directory(),
                    bytes: e.bytes(),
                    compressed: e.compressed_bytes(),
                    crc: e.crc32(),
                    method: e.method(),
                })
            }
            Self::Rar5(a) => {
                let e = a
                    .entries()
                    .get(index)
                    .ok_or("Usenet archive: selected RAR5 entry is absent")?;
                Ok(Entry {
                    name: e.name(),
                    directory: e.is_directory(),
                    bytes: e.bytes(),
                    compressed: e.compressed_bytes(),
                    crc: e.crc32(),
                    method: e.method(),
                })
            }
        }
    }
    fn limits(&self) -> Limits {
        match self {
            Self::Zip(a) => a.limits(),
            Self::Rar5(a) => a.limits(),
        }
    }
    fn source_bytes(&self) -> u64 {
        match self {
            Self::Zip(a) => a.source_bytes(),
            Self::Rar5(a) => a.source_bytes(),
        }
    }
    fn extract<R: Read + Seek, W: Write>(
        &self,
        input: &mut R,
        index: usize,
        output: &mut W,
        flag: &AtomicBool,
    ) -> Result<VerifiedEntry> {
        match self {
            Self::Zip(a) => a.extract(input, index, output, flag),
            Self::Rar5(a) => a.extract(input, index, output, flag),
        }
    }
}

fn text(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Usenet archive: invalid {key}"))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(|n| n.as_u64().or_else(|| n.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Usenet archive: invalid {key}"))
}
fn leaf(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| "Usenet archive: missing output filename".into())
}
fn directory(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "Usenet archive: cannot inspect private directory")?;
    if !metadata.is_dir() {
        return Err("Usenet archive: private directory required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("Usenet archive: directory must be private".into());
        }
    }
    Ok(())
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Usenet archive: cannot inspect private path".into()),
    }
}
fn mkdir(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|_| "Usenet archive: exclusive private directory creation failed")?;
    directory(path)?;
    sync_directory(
        path.parent()
            .ok_or("Usenet archive: directory has no parent")?,
    )
}
fn names(path: &Path, allowed: &[&str]) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(path).map_err(|_| "Usenet archive: cannot list private directory")? {
        let entry = entry.map_err(|_| "Usenet archive: cannot inspect private entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "Usenet archive: invalid private filename")?;
        if !allowed.contains(&name.as_str()) || !found.insert(name) {
            return Err(
                "Usenet archive: unknown private data will not be adopted or cleaned".into(),
            );
        }
    }
    Ok(found)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub format: Format,
    pub owner: Owner,
    pub transfer_id: String,
    pub source_name: String,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub entry_index: u32,
    pub entry_name: String,
    pub entry_bytes: u64,
    pub entry_crc32: u32,
    pub entry_compressed_bytes: u64,
    pub entry_method: String,
    pub limits: Limits,
}
impl Plan {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        if self.format == Format::Rar5 {
            v.insert("format", "rar5-stored");
        }
        v.insert("owner", self.owner.to_json());
        v.insert("transfer_id", self.transfer_id.clone());
        v.insert("source_name", self.source_name.clone());
        v.insert("source_bytes", self.source_bytes.to_string());
        v.insert("source_sha256", self.source_sha256.clone());
        v.insert("entry_index", self.entry_index);
        v.insert("entry_name", self.entry_name.clone());
        v.insert("entry_bytes", self.entry_bytes.to_string());
        v.insert("entry_crc32", self.entry_crc32);
        v.insert(
            "entry_compressed_bytes",
            self.entry_compressed_bytes.to_string(),
        );
        v.insert("entry_method", self.entry_method.clone());
        v.insert("limits", self.limits.to_json());
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "format",
                "owner",
                "transfer_id",
                "source_name",
                "source_bytes",
                "source_sha256",
                "entry_index",
                "entry_name",
                "entry_bytes",
                "entry_crc32",
                "entry_compressed_bytes",
                "entry_method",
                "limits",
            ],
        )?;
        let plan = Self {
            format: match v.get("format") {
                None => Format::Zip,
                Some(Value::String(s)) if s == "rar5-stored" => Format::Rar5,
                _ => return Err("Usenet archive: invalid captured format".into()),
            },
            owner: Owner::from_json(v.get("owner").ok_or("Usenet archive: missing owner")?)?,
            transfer_id: text(v, "transfer_id")?,
            source_name: text(v, "source_name")?,
            source_bytes: number(v, "source_bytes")?,
            source_sha256: text(v, "source_sha256")?,
            entry_index: u32::try_from(number(v, "entry_index")?)
                .map_err(|_| "Usenet archive: invalid entry index")?,
            entry_name: text(v, "entry_name")?,
            entry_bytes: number(v, "entry_bytes")?,
            entry_crc32: u32::try_from(number(v, "entry_crc32")?)
                .map_err(|_| "Usenet archive: invalid CRC")?,
            entry_compressed_bytes: number(v, "entry_compressed_bytes")?,
            entry_method: text(v, "entry_method")?,
            limits: Limits::from_json(
                v.get("limits")
                    .ok_or("Usenet archive: missing captured limits")?,
            )?,
        };
        if !valid_digest(&plan.transfer_id)
            || !valid_digest(&plan.source_sha256)
            || !super::yenc::valid_name(&plan.source_name)
            || plan.source_name.len() > 255
            || Format::from_path(Path::new(&plan.source_name))? != plan.format
            || match plan.format {
                Format::Zip => !(22..u64::from(u32::MAX)).contains(&plan.source_bytes),
                Format::Rar5 => {
                    plan.source_bytes < 24
                        || plan.source_bytes
                            > plan.limits.max_total_bytes.saturating_add(2 * 1024 * 1024)
                }
            }
            || plan.entry_index >= plan.limits.max_entries
            || crate::archive::checked_entry_path(&plan.entry_name)?
            || plan.entry_bytes == 0
            || plan.entry_bytes > plan.limits.max_entry_bytes
            || plan.entry_compressed_bytes > plan.source_bytes
            || plan.entry_bytes
                > plan
                    .entry_compressed_bytes
                    .saturating_mul(u64::from(plan.limits.max_ratio))
            || !matches!(plan.entry_method.as_str(), "stored" | "deflate")
            || plan.format == Format::Rar5 && plan.entry_method != "stored"
            || plan.entry_method == "stored" && plan.entry_bytes != plan.entry_compressed_bytes
            || plan.to_json() != *v
        {
            return Err("Usenet archive: invalid or noncanonical captured plan".into());
        }
        Ok(plan)
    }
    pub fn output_name(&self) -> Result<String> {
        leaf(&self.entry_name)
    }
    pub(crate) fn capture(
        owner: Owner,
        transfer_id: String,
        source: &Path,
        bytes: u64,
        sha: &str,
        archive: &Parsed,
        index: usize,
    ) -> Result<Self> {
        let entry = archive.entry(index)?;
        let plan = Self {
            format: archive.format(),
            owner,
            transfer_id,
            source_name: leaf(
                source
                    .to_str()
                    .ok_or("Usenet archive: invalid source path")?,
            )?,
            source_bytes: bytes,
            source_sha256: sha.into(),
            entry_index: index as u32,
            entry_name: entry.name.into(),
            entry_bytes: entry.bytes,
            entry_crc32: entry.crc,
            entry_compressed_bytes: entry.compressed,
            entry_method: entry.method.name().into(),
            limits: archive.limits(),
        };
        Self::from_json(&plan.to_json())
    }
    fn check_entry(&self, archive: &Parsed) -> Result<()> {
        let entry = archive.entry(self.entry_index as usize)?;
        if archive.format() != self.format
            || archive.source_bytes() != self.source_bytes
            || archive.limits() != self.limits
            || entry.name != self.entry_name
            || entry.directory
            || entry.bytes != self.entry_bytes
            || entry.crc != self.entry_crc32
            || entry.compressed != self.entry_compressed_bytes
            || entry.method.name() != self.entry_method
        {
            return Err("Usenet archive: source directory differs from captured selection".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    pub bytes: u64,
    pub crc32: u32,
    pub sha256: String,
}
impl Proof {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("bytes", self.bytes.to_string());
        v.insert("crc32", self.crc32);
        v.insert("sha256", self.sha256.clone());
        v
    }
    pub fn from_json(v: &Value, plan: &Plan) -> Result<Self> {
        crate::numbering::only(v, &["bytes", "crc32", "sha256"])?;
        let proof = Self {
            bytes: number(v, "bytes")?,
            crc32: u32::try_from(number(v, "crc32")?)
                .map_err(|_| "Usenet archive: invalid output CRC")?,
            sha256: text(v, "sha256")?,
        };
        if proof.bytes != plan.entry_bytes
            || proof.crc32 != plan.entry_crc32
            || !valid_digest(&proof.sha256)
            || proof.to_json() != *v
        {
            return Err("Usenet archive: output proof differs from captured entry".into());
        }
        Ok(proof)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preparation {
    pub plan: Plan,
    pub output: Option<Proof>,
}
impl Preparation {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("plan", self.plan.to_json());
        v.insert(
            "output",
            self.output.as_ref().map_or(Value::Null, Proof::to_json),
        );
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["plan", "output"])?;
        let plan = Plan::from_json(v.get("plan").ok_or("Usenet archive: missing plan")?)?;
        let output = match v.get("output") {
            Some(Value::Null) => None,
            Some(v) => Some(Proof::from_json(v, &plan)?),
            None => return Err("Usenet archive: missing output state".into()),
        };
        Ok(Self { plan, output })
    }
}

fn checked_source(path: &Path, plan: &Plan, flag: &AtomicBool) -> Result<File> {
    deflate::active(flag)?;
    if path.file_name().and_then(|n| n.to_str()) != Some(plan.source_name.as_str()) {
        return Err("Usenet archive: source filename changed".into());
    }
    let mut file = disk::private_file(path, plan.source_bytes)?;
    if file
        .metadata()
        .map_err(|_| "Usenet archive: cannot inspect source")?
        .len()
        != plan.source_bytes
    {
        return Err("Usenet archive: source size changed".into());
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut total = 0;
    loop {
        deflate::active(flag)?;
        let cap = (plan.source_bytes - total + 1).min(CHUNK as u64) as usize;
        let count = file
            .read(&mut buffer[..cap])
            .map_err(|_| "Usenet archive: source read failed")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > plan.source_bytes {
            return Err("Usenet archive: source grew beyond its captured bound".into());
        }
        hash.update(&buffer[..count]);
    }
    if total != plan.source_bytes || digest_bytes(hash.finalize()) != plan.source_sha256 {
        return Err("Usenet archive: original source digest changed".into());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "Usenet archive: source seek failed")?;
    Ok(file)
}
fn digest_bytes(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn checked_output(path: &Path, plan: &Plan, proof: &Proof, flag: &AtomicBool) -> Result<()> {
    deflate::active(flag)?;
    let mut file = disk::private_file(path, plan.entry_bytes)?;
    if file
        .metadata()
        .map_err(|_| "Usenet archive: output metadata failed")?
        .len()
        != proof.bytes
    {
        return Err("Usenet archive: output size differs from proof".into());
    }
    let mut crc = Crc32::default();
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut total = 0;
    loop {
        deflate::active(flag)?;
        let cap = (proof.bytes - total + 1).min(CHUNK as u64) as usize;
        let n = file
            .read(&mut buffer[..cap])
            .map_err(|_| "Usenet archive: output read failed")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > proof.bytes {
            return Err("Usenet archive: output grew beyond its captured bound".into());
        }
        crc.update(&buffer[..n]);
        hash.update(&buffer[..n]);
    }
    if total != proof.bytes
        || crc.finish() != proof.crc32
        || digest_bytes(hash.finalize()) != proof.sha256
    {
        return Err("Usenet archive: corrupt private output".into());
    }
    deflate::active(flag)
}

/// A known writing intent may contain unverified bytes. They are never returned
/// as a ready payload; a fresh authorized worker restarts this exact private file.
pub(crate) struct Workspace {
    root: PathBuf,
    plan: Plan,
    _owner: File,
    output: Option<Proof>,
}
impl Workspace {
    fn inspect(root: &Path, preparation: &Preparation, flag: &AtomicBool) -> Result<Option<Proof>> {
        Preparation::from_json(&preparation.to_json())?;
        directory(root)?;
        let found = names(root, &[".owner", "output", "archive.bin"])?;
        if found.contains(".owner") {
            disk::private_file(&root.join(".owner"), 0)?;
        }
        let name = preparation.plan.output_name()?;
        let output = root.join("output");
        let files = if found.contains("output") {
            directory(&output)?;
            names(&output, &[&name])?
        } else {
            BTreeSet::new()
        };
        if !found.contains("archive.bin") {
            if !files.is_empty() || preparation.output.is_some() {
                return Err("Usenet archive: payload lacks a checked writing intent".into());
            }
            return Ok(None);
        }
        if !found.contains(".owner") || !found.contains("output") {
            return Err(
                "Usenet archive: descriptor lacks its private owner or output directory".into(),
            );
        }
        let (v, bytes) = disk::read_frame(
            &root.join("archive.bin"),
            preparation.plan.format.magic(),
            0,
        )?;
        crate::numbering::only(&v, &["plan", "phase", "output"])?;
        if !bytes.is_empty() || v.get("plan") != Some(&preparation.plan.to_json()) {
            return Err("Usenet archive: foreign extraction descriptor".into());
        }
        let proof = match (v.get("phase").and_then(Value::as_str), v.get("output")) {
            (Some("writing"), Some(Value::Null)) => None,
            (Some("complete"), Some(v)) => Some(Proof::from_json(v, &preparation.plan)?),
            _ => return Err("Usenet archive: invalid extraction phase or output proof".into()),
        };
        if preparation
            .output
            .as_ref()
            .is_some_and(|p| proof.as_ref() != Some(p))
        {
            return Err("Usenet archive: journal and extraction proofs disagree".into());
        }
        if let Some(proof) = &proof {
            checked_output(&output.join(&name), &preparation.plan, proof, flag)?;
        } else if files.contains(&name) {
            disk::private_file(&output.join(&name), preparation.plan.entry_bytes)?;
        }
        deflate::active(flag)?;
        Ok(proof)
    }
    pub(crate) fn preflight(root: &Path, source: &Path, preparation: &Preparation) -> Result<()> {
        Preparation::from_json(&preparation.to_json())?;
        let flag = AtomicBool::new(true);
        checked_source(source, &preparation.plan, &flag)?;
        if exists(root)? {
            Self::inspect(root, preparation, &flag)?;
        } else if preparation.output.is_some() {
            return Err("Usenet archive: captured private output is absent".into());
        }
        Ok(())
    }
    pub(crate) fn open(
        root: &Path,
        source: &Path,
        preparation: &Preparation,
        flag: &AtomicBool,
    ) -> Result<Self> {
        Preparation::from_json(&preparation.to_json())?;
        checked_source(source, &preparation.plan, flag)?;
        let output = if exists(root)? {
            Self::inspect(root, preparation, flag)?
        } else {
            if preparation.output.is_some() {
                return Err("Usenet archive: captured private output is absent".into());
            }
            deflate::active(flag)?;
            mkdir(root)?;
            None
        };
        deflate::active(flag)?;
        let owner_path = root.join(".owner");
        let owner = if exists(&owner_path)? {
            disk::private_file(&owner_path, 0)?
        } else {
            let owner = private_options()
                .create_new(true)
                .write(true)
                .open(&owner_path)
                .map_err(|_| "Usenet archive: exclusive owner creation failed")?;
            owner
                .sync_all()
                .map_err(|_| "Usenet archive: owner synchronization failed")?;
            owner
        };
        owner
            .try_lock()
            .map_err(|_| "Usenet archive: extraction workspace is already owned")?;
        if !exists(&root.join("output"))? {
            mkdir(&root.join("output"))?;
        }
        let mut workspace = Self {
            root: root.to_owned(),
            plan: preparation.plan.clone(),
            _owner: owner,
            output,
        };
        if !exists(&root.join("archive.bin"))? {
            workspace.save()?;
        }
        sync_directory(root)?;
        Ok(workspace)
    }
    fn save(&mut self) -> Result<()> {
        let mut v = Value::object();
        v.insert("plan", self.plan.to_json());
        v.insert(
            "phase",
            if self.output.is_some() {
                "complete"
            } else {
                "writing"
            },
        );
        v.insert(
            "output",
            self.output.as_ref().map_or(Value::Null, Proof::to_json),
        );
        disk::write_frame(
            &self.root.join("archive.bin"),
            self.plan.format.magic(),
            &v,
            &[],
        )
    }
    pub(crate) fn path(&self) -> Result<PathBuf> {
        Ok(self.root.join("output").join(self.plan.output_name()?))
    }
    pub(crate) fn extract(&mut self, source: &Path, flag: &AtomicBool) -> Result<Proof> {
        let mut input = checked_source(source, &self.plan, flag)?;
        let archive = Parsed::read(&mut input, self.plan.format, self.plan.limits, flag)?;
        self.plan.check_entry(&archive)?;
        let path = self.path()?;
        if let Some(proof) = &self.output {
            checked_output(&path, &self.plan, proof, flag)?;
            return Ok(proof.clone());
        }
        deflate::active(flag)?;
        let mut file = if exists(&path)? {
            let checked = disk::private_file(&path, self.plan.entry_bytes)?;
            let file = private_options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|_| "Usenet archive: cannot reopen known partial output")?;
            let before = checked
                .metadata()
                .map_err(|_| "Usenet archive: partial metadata failed")?;
            let after = file
                .metadata()
                .map_err(|_| "Usenet archive: partial metadata failed")?;
            if before.len() != after.len() || !after.is_file() {
                return Err("Usenet archive: partial output changed before restart".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if before.dev() != after.dev()
                    || before.ino() != after.ino()
                    || after.nlink() != 1
                    || after.mode() & 0o077 != 0
                {
                    return Err("Usenet archive: partial inode changed before restart".into());
                }
            }
            deflate::active(flag)?;
            file.set_len(0)
                .map_err(|_| "Usenet archive: known partial restart failed")?;
            file
        } else {
            private_options()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|_| "Usenet archive: exclusive output creation failed")?
        };
        let decoded =
            archive.extract(&mut input, self.plan.entry_index as usize, &mut file, flag)?;
        deflate::active(flag)?;
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|_| "Usenet archive: output synchronization failed")?;
        // The original archive remains bound after decoding and before the proof.
        checked_source(source, &self.plan, flag)?;
        let proof = Proof {
            bytes: decoded.bytes(),
            crc32: decoded.crc32(),
            sha256: digest_bytes(*decoded.sha256()),
        };
        Proof::from_json(&proof.to_json(), &self.plan)?;
        checked_output(&path, &self.plan, &proof, flag)?;
        deflate::active(flag)?;
        sync_directory(&self.root.join("output"))?;
        self.output = Some(proof.clone());
        self.save()?;
        deflate::active(flag)?;
        Ok(proof)
    }
}

pub(crate) fn validate_namespace(root: &Path, plans: &BTreeSet<String>) -> Result<()> {
    if !exists(root)? {
        return Ok(());
    }
    directory(root)?;
    for entry in
        fs::read_dir(root).map_err(|_| "Usenet archive: cannot inspect retained namespace")?
    {
        let entry = entry.map_err(|_| "Usenet archive: cannot inspect retained workspace")?;
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| "Usenet archive: invalid retained identity")?;
        if !valid_digest(&id) || !plans.contains(&id) {
            return Err(
                "Usenet archive: retained extraction has no matching library intent".into(),
            );
        }
        directory(&entry.path())?;
    }
    Ok(())
}
pub(crate) fn prepare_namespace(root: &Path, flag: &AtomicBool) -> Result<()> {
    deflate::active(flag)?;
    if exists(root)? {
        directory(root)
    } else {
        mkdir(root)
    }
}
pub(crate) fn namespace_present(root: &Path) -> Result<bool> {
    exists(root)
}

#[cfg(test)]
#[path = "../../tests/archive_support/mod.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requesters::digest;
    use std::{
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const MEDIA: &[u8] = include_bytes!("../../examples/demo.mp4");
    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
        source: PathBuf,
        preparation: Preparation,
    }
    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let parent = std::env::temp_dir().join(format!(
                "mynou-archive-proof-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&parent).unwrap();
            let source = parent.join("Original.Movie.2026.1080p.zip");
            let bytes = fixtures::stored_zip("Original/Original.Movie.2026.1080p.mp4", MEDIA);
            let mut file = private_options()
                .create_new(true)
                .write(true)
                .open(&source)
                .unwrap();
            file.write_all(&bytes).unwrap();
            file.sync_all().unwrap();
            let zip = Zip::read(&mut Cursor::new(&bytes), Limits::default()).unwrap();
            let owner = Owner {
                job_id: "original-job".into(),
                binding: digest(b"original-owner"),
            };
            let plan = Plan::capture(
                owner,
                digest(b"original-transfer"),
                &source,
                bytes.len() as u64,
                &digest(&bytes),
                &Parsed::Zip(zip),
                0,
            )
            .unwrap();
            Self {
                root: parent.join(&plan.transfer_id),
                parent,
                source,
                preparation: Preparation { plan, output: None },
            }
        }
        fn open(&self) -> Workspace {
            Workspace::open(
                &self.root,
                &self.source,
                &self.preparation,
                &AtomicBool::new(true),
            )
            .unwrap()
        }
        fn complete(&self) -> Proof {
            let mut workspace = self.open();
            workspace
                .extract(&self.source, &AtomicBool::new(true))
                .unwrap()
        }
        fn output(&self) -> PathBuf {
            self.root
                .join("output")
                .join(self.preparation.plan.output_name().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn writing_intent_restarts_known_partial_bytes_without_adopting_them() {
        let f = Fixture::new();
        let workspace = f.open();
        let path = workspace.path().unwrap();
        let mut partial = private_options()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        partial.write_all(&MEDIA[..19]).unwrap();
        partial.sync_all().unwrap();
        drop(partial);
        drop(workspace);
        let descriptor = fs::read(f.root.join("archive.bin")).unwrap();
        Workspace::preflight(&f.root, &f.source, &f.preparation).unwrap();
        assert_eq!(fs::read(&path).unwrap(), MEDIA[..19]);
        assert_eq!(fs::read(f.root.join("archive.bin")).unwrap(), descriptor);
        let proof = f.complete();
        assert_eq!(proof.bytes, MEDIA.len() as u64);
        assert_eq!(proof.sha256, digest(MEDIA));
        assert_eq!(fs::read(path).unwrap(), MEDIA);
    }
    #[test]
    fn completed_descriptor_can_join_the_preceding_journal_intent_without_rewriting() {
        let f = Fixture::new();
        let proof = f.complete();
        let descriptor = fs::read(f.root.join("archive.bin")).unwrap();
        let before = fs::metadata(f.output()).unwrap().modified().unwrap();
        Workspace::preflight(&f.root, &f.source, &f.preparation).unwrap();
        assert_eq!(f.complete(), proof);
        assert_eq!(fs::read(f.root.join("archive.bin")).unwrap(), descriptor);
        assert_eq!(
            fs::metadata(f.output()).unwrap().modified().unwrap(),
            before
        );
        let mut linked = f.preparation.clone();
        linked.output = Some(proof);
        Workspace::preflight(&f.root, &f.source, &linked).unwrap();
    }
    #[test]
    fn corrupt_descriptor_and_changed_original_source_are_preserved_without_repair() {
        let f = Fixture::new();
        f.complete();
        let path = f.root.join("archive.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[25] ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(Workspace::preflight(&f.root, &f.source, &f.preparation).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::read(f.output()).unwrap(), MEDIA);
        let f = Fixture::new();
        f.complete();
        let descriptor = fs::read(f.root.join("archive.bin")).unwrap();
        let mut bytes = fs::read(&f.source).unwrap();
        bytes[40] ^= 1;
        fs::write(&f.source, &bytes).unwrap();
        assert!(
            Workspace::preflight(&f.root, &f.source, &f.preparation)
                .unwrap_err()
                .contains("source digest")
        );
        assert_eq!(fs::read(&f.source).unwrap(), bytes);
        assert_eq!(fs::read(f.root.join("archive.bin")).unwrap(), descriptor);
    }
    #[test]
    fn foreign_owner_entry_limits_and_output_proofs_cannot_be_rebound() {
        let f = Fixture::new();
        let proof = f.complete();
        for field in ["owner", "entry", "limits", "output"] {
            let mut altered = f.preparation.clone();
            match field {
                "owner" => altered.plan.owner.binding = digest(b"another-owner"),
                "entry" => altered.plan.entry_name = "Other/Original.Movie.2026.1080p.mp4".into(),
                "limits" => altered.plan.limits.max_blocks += 1,
                "output" => {
                    let mut changed = proof.clone();
                    changed.sha256 = digest(b"another-output");
                    altered.output = Some(changed);
                }
                _ => unreachable!(),
            }
            assert!(
                Workspace::preflight(&f.root, &f.source, &altered).is_err(),
                "{field}"
            );
        }
        assert_eq!(fs::read(f.output()).unwrap(), MEDIA);
    }
    #[test]
    fn unknown_files_unproven_output_and_orphan_namespaces_are_not_adopted() {
        let f = Fixture::new();
        mkdir(&f.root).unwrap();
        mkdir(&f.root.join("output")).unwrap();
        let mut file = private_options()
            .create_new(true)
            .write(true)
            .open(f.output())
            .unwrap();
        file.write_all(MEDIA).unwrap();
        drop(file);
        assert!(
            Workspace::preflight(&f.root, &f.source, &f.preparation)
                .unwrap_err()
                .contains("writing intent")
        );
        assert!(
            Workspace::open(&f.root, &f.source, &f.preparation, &AtomicBool::new(true)).is_err()
        );
        assert_eq!(fs::read(f.output()).unwrap(), MEDIA);
        let f = Fixture::new();
        f.complete();
        let path = f.root.join("foreign.bin");
        fs::write(&path, b"original unknown file").unwrap();
        assert!(
            Workspace::preflight(&f.root, &f.source, &f.preparation)
                .unwrap_err()
                .contains("unknown")
        );
        assert_eq!(fs::read(path).unwrap(), b"original unknown file");
        assert!(validate_namespace(&f.parent, &BTreeSet::new()).is_err());
    }
    #[test]
    fn known_empty_scaffolding_resumes_but_cancelled_open_creates_nothing() {
        let f = Fixture::new();
        mkdir(&f.root).unwrap();
        Workspace::preflight(&f.root, &f.source, &f.preparation).unwrap();
        f.complete();
        assert_eq!(fs::read(f.output()).unwrap(), MEDIA);
        let f = Fixture::new();
        assert!(
            Workspace::open(&f.root, &f.source, &f.preparation, &AtomicBool::new(false)).is_err()
        );
        assert!(!f.root.exists());
    }
    #[test]
    fn corrupt_outputs_and_private_inode_aliases_fail_read_only_preflight() {
        let f = Fixture::new();
        f.complete();
        let mut bytes = fs::read(f.output()).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(f.output(), &bytes).unwrap();
        assert!(
            Workspace::preflight(&f.root, &f.source, &f.preparation)
                .unwrap_err()
                .contains("corrupt private output")
        );
        assert_eq!(fs::read(f.output()).unwrap(), bytes);
        #[cfg(unix)]
        {
            let f = Fixture::new();
            f.complete();
            let alias = f.parent.join("alias.mp4");
            fs::hard_link(f.output(), &alias).unwrap();
            assert!(Workspace::preflight(&f.root, &f.source, &f.preparation).is_err());
            assert_eq!(fs::read(&alias).unwrap(), MEDIA);
            let f = Fixture::new();
            f.complete();
            let output = f.output();
            let original = f.parent.join("original.mp4");
            fs::rename(&output, &original).unwrap();
            std::os::unix::fs::symlink(&original, &output).unwrap();
            assert!(Workspace::preflight(&f.root, &f.source, &f.preparation).is_err());
            assert_eq!(fs::read(original).unwrap(), MEDIA);
        }
    }
}
