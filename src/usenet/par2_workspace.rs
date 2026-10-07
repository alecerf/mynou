//! Owner-bound private recovery staging. No library admission or existing-file repair.
use super::{queue::Owner, workspace::disk};
use crate::{
    Result,
    crypto::Sha256,
    json::{self, Value},
    par2::{MultiRecoveryLimits, RecoveryInput, Set},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata},
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const MAGIC: &[u8; 8] = b"MYNOUPR1";
const CHUNK: usize = 64 << 10;
const OVERHEAD: u64 = 1 << 20;
const MAX_HEADER: usize = 16 << 10;

fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("PAR2 workspace: operation cancelled".into())
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        value.push(char::from(DIGITS[(byte >> 4) as usize]));
        value.push(char::from(DIGITS[(byte & 15) as usize]));
    }
    value
}

fn digest(bytes: &[u8], flag: &AtomicBool) -> Result<String> {
    let mut hash = Sha256::default();
    for chunk in bytes.chunks(CHUNK) {
        active(flag)?;
        hash.update(chunk);
    }
    active(flag)?;
    Ok(hex(&hash.finalize()))
}

fn plan(owner: &Owner, set: &Set, source: &[u8], limits: MultiRecoveryLimits, flag: &AtomicBool) -> Result<Value> {
    active(flag)?;
    Owner::from_json(&owner.to_json())?;
    limits.validate()?;
    let files: Vec<_> = set.files().iter().filter(|f| f.is_recoverable()).collect();
    let bytes = files.iter().try_fold(0u64, |n, f| n.checked_add(f.bytes()));
    let slices = files.iter().try_fold(0usize, |n, f| n.checked_add(f.slices().len()));
    if files.is_empty()
        || files.len() > limits.max_files as usize
        || bytes.is_none_or(|n| n > limits.recovery.max_file_bytes
            || n + u64::from(set.slice_bytes()) + OVERHEAD > limits.recovery.max_working_bytes)
        || slices.is_none_or(|n| n > limits.recovery.max_slices as usize)
        || set.slice_bytes() > limits.recovery.max_slice_bytes
        || source.len() as u64 != set.source_bytes()
    {
        return Err("PAR2 workspace: captured source or aggregate bounds exceeded".into());
    }
    if digest(source, flag)? != hex(set.source_sha256()) {
        return Err("PAR2 workspace: captured source changed".into());
    }
    let mut policy = Value::object();
    policy.insert("max_files", limits.max_files);
    policy.insert("max_file_bytes", limits.recovery.max_file_bytes.to_string());
    policy.insert("max_slices", limits.recovery.max_slices);
    policy.insert("max_missing_slices", limits.recovery.max_missing_slices);
    policy.insert("max_slice_bytes", limits.recovery.max_slice_bytes);
    policy.insert("max_working_bytes", limits.recovery.max_working_bytes.to_string());
    policy.insert("max_field_operations", limits.recovery.max_field_operations.to_string());
    let inventory = files.iter().map(|file| {
        let mut item = Value::object();
        item.insert("file_id", hex(file.id()));
        item.insert("name", file.name());
        item.insert("bytes", file.bytes().to_string());
        item.insert("md5", hex(file.md5()));
        item.insert("first_16k_md5", hex(file.first_16k_md5()));
        item.insert("slices", file.slices().len() as u32);
        item
    }).collect();
    let mut value = Value::object();
    value.insert("format", 1u32);
    value.insert("owner", owner.to_json());
    value.insert("source_sha256", hex(set.source_sha256()));
    value.insert("source_bytes", set.source_bytes().to_string());
    value.insert("set_id", hex(set.id()));
    value.insert("policy", policy);
    value.insert("files", Value::Array(inventory));
    Ok(value)
}

fn clean_root(root: &Path) -> Result<PathBuf> {
    if root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("PAR2 workspace: clean path required".into());
    }
    let root = std::path::absolute(root).map_err(|_| "PAR2 workspace: unavailable path")?;
    if root.file_name().is_none() || root.as_os_str().len() > 8192 || root.components().count() > 128 {
        return Err("PAR2 workspace: path exceeds supported bounds".into());
    }
    Ok(root)
}

fn directory(path: &Path) -> Result<Metadata> {
    if !cfg!(unix) {
        return Err("PAR2 workspace: Unix private-filesystem support required".into());
    }
    reject_symlinks(path).map_err(|_| "PAR2 workspace: linked directory")?;
    let m = fs::symlink_metadata(path).map_err(|_| "PAR2 workspace: missing directory")?;
    if !m.is_dir() {
        return Err("PAR2 workspace: directory required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err("PAR2 workspace: private directory required".into());
        }
    }
    Ok(m)
}

fn same(before: &Metadata, after: &Metadata) -> bool {
    if before.file_type() != after.file_type() || before.is_file() && before.len() != after.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return false;
        }
    }
    true
}

/// A checked private output. It is not an import or write capability.
pub struct PrivateOutput {
    id: [u8; 16],
    name: String,
    path: PathBuf,
}

impl PrivateOutput {
    pub fn id(&self) -> &[u8; 16] {
        &self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The caller retains immutable PAR2 metadata/source and controls the private
/// parent namespace. A captured Owner is not evidence of current authorization.
pub struct RecoveryWorkspace<'a> {
    root: PathBuf,
    parent_identity: Metadata,
    root_identity: Metadata,
    owner: File,
    descriptor: Value,
    hashes: Vec<String>,
    set: &'a Set,
    source: &'a [u8],
    limits: MultiRecoveryLimits,
}

impl<'a> RecoveryWorkspace<'a> {
    /// Reconstruct every output before creating a new private namespace. An
    /// interrupted staging directory is unpublished, never implicitly adopted.
    pub fn create(
        root: &Path,
        owner: &Owner,
        set: &'a Set,
        source: &'a [u8],
        inputs: &[RecoveryInput<'_>],
        limits: MultiRecoveryLimits,
        flag: &AtomicBool,
    ) -> Result<Self> {
        let captured = plan(owner, set, source, limits, flag)?;
        let outputs = set.recover_files_cancellable(&mut Cursor::new(source), inputs, limits, flag)?;
        let hashes: Vec<_> = outputs.iter().map(|f| digest(f.bytes(), flag)).collect::<Result<_>>()?;
        let mut descriptor = Value::object();
        descriptor.insert("plan", captured);
        descriptor.insert("outputs", Value::Array(hashes.iter().map(|s| Value::from(s.clone())).collect()));
        if json::stringify(&descriptor).len() > MAX_HEADER {
            return Err("PAR2 workspace: descriptor limit exceeded".into());
        }
        let root = clean_root(root)?;
        let parent = root.parent().ok_or("PAR2 workspace: parent required")?;
        let parent_identity = directory(parent)?;
        reject_symlinks(&root).map_err(|_| "PAR2 workspace: linked root")?;
        active(flag)?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root).map_err(|_| "PAR2 workspace: new directory required")?;
        let root_identity = directory(&root)?;
        let owner_file = private_options().create_new(true).read(true).write(true).open(root.join(".owner"))
            .map_err(|_| "PAR2 workspace: owner creation failed")?;
        owner_file.try_lock().map_err(|_| "PAR2 workspace: owner unavailable")?;
        owner_file.sync_all().map_err(|_| "PAR2 workspace: owner sync failed")?;
        let workspace = Self { root, parent_identity, root_identity, owner: owner_file,
            descriptor, hashes, set, source, limits };
        workspace.check()?;
        for output in &outputs {
            active(flag)?;
            workspace.check()?;
            let mut file = private_options().create_new(true).write(true).open(workspace.output_path(output.id()))
                .map_err(|_| "PAR2 workspace: new private output required")?;
            for chunk in output.bytes().chunks(CHUNK) {
                active(flag)?;
                file.write_all(chunk).map_err(|_| "PAR2 workspace: output write failed")?;
            }
            active(flag)?;
            file.sync_all().map_err(|_| "PAR2 workspace: output sync failed")?;
        }
        drop(outputs);
        workspace.verify_outputs(flag, false)?;
        sync_directory(&workspace.root)?;
        sync_directory(workspace.root.parent().ok_or("PAR2 workspace: parent required")?)?;
        active(flag)?;
        workspace.check()?;
        disk::write_frame(&workspace.root.join("recovery.bin"), MAGIC, &workspace.descriptor, &[])?;
        // The synchronized descriptor is the commit point. Later cancellation
        // cannot undo that publication; callers must explicitly reverify on use.
        Ok(workspace)
    }

    /// Read-only reopen requires the exact expected owner, source and policy.
    pub fn open(
        root: &Path,
        owner: &Owner,
        set: &'a Set,
        source: &'a [u8],
        limits: MultiRecoveryLimits,
        flag: &AtomicBool,
    ) -> Result<Self> {
        let captured = plan(owner, set, source, limits, flag)?;
        let root = clean_root(root)?;
        let parent_identity = directory(root.parent().ok_or("PAR2 workspace: parent required")?)?;
        let root_identity = directory(&root)?;
        let owner_file = disk::private_file(&root.join(".owner"), 0)?;
        owner_file.try_lock().map_err(|_| "PAR2 workspace: already owned")?;
        let (descriptor, data) = disk::read_frame(&root.join("recovery.bin"), MAGIC, 0)?;
        crate::numbering::only(&descriptor, &["plan", "outputs"])?;
        if !data.is_empty() || descriptor.get("plan") != Some(&captured) {
            return Err("PAR2 workspace: immutable owner, source or policy differs".into());
        }
        let count = set.files().iter().filter(|f| f.is_recoverable()).count();
        let hashes: Vec<_> = descriptor.get("outputs").and_then(Value::as_array)
            .filter(|v| v.len() == count).ok_or("PAR2 workspace: incomplete output proof")?
            .iter().map(|v| v.as_str().filter(|s| crate::requesters::valid_digest(s))
                .map(str::to_owned).ok_or_else(|| "PAR2 workspace: invalid output digest".into()))
            .collect::<Result<_>>()?;
        let workspace = Self { root, parent_identity, root_identity, owner: owner_file,
            descriptor, hashes, set, source, limits };
        workspace.verified_files(flag)?;
        Ok(workspace)
    }

    fn output_path(&self, id: &[u8; 16]) -> PathBuf {
        self.root.join(format!("file-{}.bin", hex(id)))
    }

    fn check(&self) -> Result<()> {
        if !same(&self.parent_identity, &directory(self.root.parent().ok_or("PAR2 workspace: parent required")?)?)
            || !same(&self.root_identity, &directory(&self.root)?)
            || !same(&self.owner.metadata().map_err(|_| "PAR2 workspace: owner metadata failed")?,
                &disk::private_file(&self.root.join(".owner"), 0)?.metadata().map_err(|_| "PAR2 workspace: owner metadata failed")?)
        {
            return Err("PAR2 workspace: namespace identity changed".into());
        }
        Ok(())
    }

    fn inventory(&self, published: bool) -> Result<()> {
        let mut names: BTreeSet<_> = self.set.files().iter().filter(|f| f.is_recoverable())
            .map(|f| format!("file-{}.bin", hex(f.id()))).collect();
        names.insert(".owner".into());
        if published {
            names.insert("recovery.bin".into());
        }
        let expected = names.len();
        for (index, item) in fs::read_dir(&self.root).map_err(|_| "PAR2 workspace: inventory unavailable")?.enumerate() {
            let item = item.map_err(|_| "PAR2 workspace: inventory unavailable")?;
            if index >= expected || !item.file_name().to_str().is_some_and(|name| names.remove(name)) {
                return Err("PAR2 workspace: unexpected private inventory".into());
            }
        }
        if !names.is_empty() {
            return Err("PAR2 workspace: incomplete private inventory".into());
        }
        Ok(())
    }

    fn verify_outputs(&self, flag: &AtomicBool, published: bool) -> Result<Vec<PrivateOutput>> {
        active(flag)?;
        self.check()?;
        self.inventory(published)?;
        let files: Vec<_> = self.set.files().iter().filter(|f| f.is_recoverable()).collect();
        let mut contents = Vec::new();
        let mut identities = Vec::new();
        for (index, declared) in files.iter().enumerate() {
            active(flag)?;
            self.check()?;
            let path = self.output_path(declared.id());
            let mut file = disk::private_file(&path, declared.bytes())?;
            let captured = file.metadata().map_err(|_| "PAR2 workspace: output metadata failed")?;
            if captured.len() != declared.bytes() {
                return Err("PAR2 workspace: output length differs".into());
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(declared.bytes() as usize).map_err(|_| "PAR2 workspace: allocation failed")?;
            bytes.resize(declared.bytes() as usize, 0);
            for chunk in bytes.chunks_mut(CHUNK) {
                active(flag)?;
                file.read_exact(chunk).map_err(|_| "PAR2 workspace: output read failed")?;
            }
            if digest(&bytes, flag)? != self.hashes[index]
                || !same(&captured, &file.metadata().map_err(|_| "PAR2 workspace: output metadata failed")?)
            {
                return Err("PAR2 workspace: output proof changed".into());
            }
            contents.push(bytes);
            identities.push((path, captured));
        }
        let inputs: Vec<_> = files.iter().zip(&contents).map(|(f, bytes)| RecoveryInput::new(*f.id(), bytes)).collect();
        let verified = self.set.verify_files_cancellable(&mut Cursor::new(self.source), &inputs, self.limits, flag)?;
        if !verified.content_verified() {
            return Err("PAR2 workspace: protected integrity mismatch".into());
        }
        for (path, captured) in &identities {
            if !same(captured, &disk::private_file(path, captured.len())?.metadata().map_err(|_| "PAR2 workspace: output metadata failed")?) {
                return Err("PAR2 workspace: output path changed".into());
            }
        }
        self.check()?;
        self.inventory(published)?;
        active(flag)?;
        Ok(files.iter().zip(identities).map(|(f, (path, _))| PrivateOutput {
            id: *f.id(), name: f.name().to_owned(), path,
        }).collect())
    }

    /// Verify the complete inventory again before exposing any private paths.
    /// This is not a hostile concurrent filesystem snapshot or live permission.
    pub fn verified_files(&self, flag: &AtomicBool) -> Result<Vec<PrivateOutput>> {
        active(flag)?;
        let read_descriptor = || -> Result<()> {
            let (value, data) = disk::read_frame(&self.root.join("recovery.bin"), MAGIC, 0)?;
            if value != self.descriptor || !data.is_empty() {
                return Err("PAR2 workspace: descriptor changed".into());
            }
            Ok(())
        };
        self.check()?;
        read_descriptor()?;
        let outputs = self.verify_outputs(flag, true)?;
        read_descriptor()?;
        self.check()?;
        active(flag)?;
        Ok(outputs)
    }
}
