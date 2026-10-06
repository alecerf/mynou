//! Captured ZIP provenance and private resumable extraction. No library imports.
use super::{queue::Owner, workspace::disk};
use crate::{
    Result,
    archive::{Limits, Zip, deflate},
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
const CHUNK: usize = 65_536;

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
            || !Path::new(&plan.source_name)
                .extension()
                .and_then(|n| n.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
            || !(22..u64::from(u32::MAX)).contains(&plan.source_bytes)
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
        zip: &Zip,
        index: usize,
    ) -> Result<Self> {
        let entry = zip
            .entries()
            .get(index)
            .ok_or("Usenet archive: selected entry is absent")?;
        let plan = Self {
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
            entry_name: entry.name().into(),
            entry_bytes: entry.bytes(),
            entry_crc32: entry.crc32(),
            entry_compressed_bytes: entry.compressed_bytes(),
            entry_method: entry.method().name().into(),
            limits: zip.limits(),
        };
        Self::from_json(&plan.to_json())
    }
    fn check_entry(&self, zip: &Zip) -> Result<()> {
        let entry = zip
            .entries()
            .get(self.entry_index as usize)
            .ok_or("Usenet archive: selected entry is absent")?;
        if zip.source_bytes() != self.source_bytes
            || zip.limits() != self.limits
            || entry.name() != self.entry_name
            || entry.is_directory()
            || entry.bytes() != self.entry_bytes
            || entry.crc32() != self.entry_crc32
            || entry.compressed_bytes() != self.entry_compressed_bytes
            || entry.method().name() != self.entry_method
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
        let (v, bytes) = disk::read_frame(&root.join("archive.bin"), MAGIC, 0)?;
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
        disk::write_frame(&self.root.join("archive.bin"), MAGIC, &v, &[])
    }
    pub(crate) fn path(&self) -> Result<PathBuf> {
        Ok(self.root.join("output").join(self.plan.output_name()?))
    }
    pub(crate) fn extract(&mut self, source: &Path, flag: &AtomicBool) -> Result<Proof> {
        let mut input = checked_source(source, &self.plan, flag)?;
        let zip = Zip::read_cancellable(&mut input, self.plan.limits, flag)?;
        self.plan.check_entry(&zip)?;
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
        let decoded = zip.extract(&mut input, self.plan.entry_index as usize, &mut file, flag)?;
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
