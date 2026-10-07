//! Read-only protected-content diagnosis. Integrity never grants write authority.
use super::{MultiRecoveryLimits, RecoveryInput, Set, hex, recovery};
use crate::{
    Result,
    crypto::{Md5, md5},
    json::Value,
    store::private_options,
};
use std::{
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 64 << 10;
const OVERHEAD: u64 = 64 << 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileVerification {
    id: [u8; 16],
    name: String,
    expected_bytes: u64,
    input_bytes: u64,
    slice_count: u32,
    first_global_slice: u32,
    present: Option<bool>,
    damaged_slices: Vec<(u32, u32)>,
    md5_matches: bool,
    first_16k_md5_matches: bool,
}

impl FileVerification {
    pub fn id(&self) -> &[u8; 16] {
        &self.id
    }

    /// Pairs of local and global Main-order slice indices.
    pub fn damaged_slices(&self) -> &[(u32, u32)] {
        &self.damaged_slices
    }

    pub fn content_verified(&self) -> bool {
        self.present != Some(false)
            && self.input_bytes == self.expected_bytes
            && self.damaged_slices.is_empty()
            && self.md5_matches
            && self.first_16k_md5_matches
    }

    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("file_id", hex(&self.id));
        v.insert("name", self.name.clone());
        v.insert("expected_bytes", self.expected_bytes.to_string());
        v.insert("input_bytes", self.input_bytes.to_string());
        v.insert("slice_count", self.slice_count);
        v.insert("first_global_slice", self.first_global_slice);
        v.insert("present", self.present.map_or(Value::Null, Value::from));
        v.insert("md5_matches", self.md5_matches);
        v.insert("first_16k_md5_matches", self.first_16k_md5_matches);
        v.insert("content_verified", self.content_verified());
        v.insert(
            "damaged_slices",
            Value::Array(
                self.damaged_slices
                    .iter()
                    .map(|(local, global)| {
                        let mut slice = Value::object();
                        slice.insert("local_index", *local);
                        slice.insert("global_index", *global);
                        slice
                    })
                    .collect(),
            ),
        );
        v
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    set_id: [u8; 16],
    source_sha256: [u8; 32],
    files: Vec<FileVerification>,
}

impl Verification {
    pub fn files(&self) -> &[FileVerification] {
        &self.files
    }

    pub fn content_verified(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(FileVerification::content_verified)
    }

    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("format", "par2-content-verification");
        v.insert("set_id", hex(&self.set_id));
        v.insert("source_sha256", hex(&self.source_sha256));
        v.insert("content_verified", self.content_verified());
        v.insert("parity_verified", false);
        v.insert("repair_supported", false);
        v.insert("ownership_verified", false);
        v.insert("snapshot_guaranteed", false);
        v.insert(
            "files",
            Value::Array(self.files.iter().map(FileVerification::to_json).collect()),
        );
        v
    }
}

fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("PAR2 verification was cancelled".into())
    }
}

impl Set {
    fn verification_bounds(&self, limits: MultiRecoveryLimits) -> Result<()> {
        limits.validate()?;
        let files: Vec<_> = self.files.iter().filter(|f| f.recoverable).collect();
        let bytes = files.iter().try_fold(0u64, |n, f| n.checked_add(f.bytes));
        let slices = files
            .iter()
            .try_fold(0usize, |n, f| n.checked_add(f.slices.len()));
        if files.is_empty()
            || files.len() > limits.max_files as usize
            || bytes.is_none_or(|n| n > limits.recovery.max_file_bytes)
            || slices.is_none_or(|n| n > limits.recovery.max_slices as usize)
            || self.slice_bytes > limits.recovery.max_slice_bytes
            || u64::from(self.slice_bytes) + OVERHEAD > limits.recovery.max_working_bytes
        {
            return Err("PAR2 verification exceeds aggregate content or scratch limits".into());
        }
        Ok(())
    }

    /// Diagnose immutable inputs, not parity or ownership. Empty bytes denote a
    /// missing/truncated input; filesystem presence is unknown to this API.
    pub fn verify_files<R: Read + Seek>(
        &self,
        source: &mut R,
        inputs: &[RecoveryInput<'_>],
        limits: MultiRecoveryLimits,
    ) -> Result<Verification> {
        self.verify_files_cancellable(source, inputs, limits, &AtomicBool::new(true))
    }

    pub fn verify_files_cancellable<R: Read + Seek>(
        &self,
        source: &mut R,
        inputs: &[RecoveryInput<'_>],
        limits: MultiRecoveryLimits,
        flag: &AtomicBool,
    ) -> Result<Verification> {
        active(flag)?;
        self.verification_bounds(limits)?;
        let files: Vec<_> = self.files.iter().filter(|f| f.recoverable).collect();
        if inputs.len() != files.len()
            || inputs.iter().enumerate().any(|(index, input)| {
                !files.iter().any(|file| file.id == input.id)
                    || inputs[..index].iter().any(|other| other.id == input.id)
            })
        {
            return Err("PAR2 verification requires exactly one input per recoverable File ID".into());
        }
        for file in &files {
            let input = inputs.iter().find(|input| input.id == file.id).unwrap();
            if input.bytes.len() as u64 > file.bytes {
                return Err("PAR2 verification input exceeds its captured length".into());
            }
        }
        self.verify_source(source, flag)?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(self.slice_bytes as usize)
            .map_err(|_| "PAR2 verification scratch allocation failed")?;
        scratch.resize(self.slice_bytes as usize, 0);
        let mut global = 0u32;
        let mut results = Vec::with_capacity(files.len());
        for file in files {
            active(flag)?;
            let input = inputs.iter().find(|input| input.id == file.id).unwrap();
            let first_global_slice = global;
            let mut damaged_slices = Vec::new();
            for (local, expected) in file.slices.iter().enumerate() {
                active(flag)?;
                let complete = recovery::padded_slice(input.bytes, local, &mut scratch)
                    && ((local + 1) as u64 * u64::from(self.slice_bytes)).min(file.bytes)
                        <= input.bytes.len() as u64;
                if !complete || !recovery::slice_matches(&scratch, expected, flag)? {
                    damaged_slices.push((local as u32, global));
                }
                global += 1;
            }
            let mut hash = Md5::default();
            for chunk in input.bytes.chunks(CHUNK) {
                active(flag)?;
                hash.update(chunk);
            }
            results.push(FileVerification {
                id: file.id,
                name: file.name.clone(),
                expected_bytes: file.bytes,
                input_bytes: input.bytes.len() as u64,
                slice_count: file.slices.len() as u32,
                first_global_slice,
                present: None,
                damaged_slices,
                md5_matches: hash.finalize() == file.md5,
                first_16k_md5_matches: md5(&input.bytes[..input.bytes.len().min(16_384)])
                    == file.first_16k_md5,
            });
        }
        self.verify_source(source, flag)?;
        active(flag)?;
        Ok(Verification {
            set_id: self.id,
            source_sha256: self.source_sha256,
            files: results,
        })
    }
}

struct Fence {
    path: PathBuf,
    metadata: Option<Metadata>,
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

fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err("PAR2 verification rejects symbolic links".into()),
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("PAR2 verification cannot inspect selected path".into()),
    }
}

fn fences(path: &Path) -> Result<Vec<Fence>> {
    if path.as_os_str().len() > 8192 || path.components().count() > 128 {
        return Err("PAR2 verification path exceeds supported bounds".into());
    }
    path.ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .enumerate()
        .map(|(index, p)| {
            let m = metadata(p)?;
            if index > 0 && m.as_ref().is_some_and(|v| !v.is_dir()) {
                return Err("PAR2 verification parent is not a directory".into());
            }
            Ok(Fence {
                path: p.to_owned(),
                metadata: m,
            })
        })
        .collect()
}

fn check(fences: &[Fence]) -> Result<()> {
    for fence in fences {
        match (&fence.metadata, metadata(&fence.path)?) {
            (Some(before), Some(after)) if same(before, &after) => {}
            (None, None) => {}
            _ => return Err("PAR2 verification selected path changed".into()),
        }
    }
    Ok(())
}

fn open(path: &Path, captured: &Metadata) -> Result<File> {
    if !captured.is_file() {
        return Err("PAR2 verification requires regular files".into());
    }
    let file = private_options()
        .read(true)
        .open(path)
        .map_err(|_| "PAR2 verification cannot open selected regular file")?;
    if !same(captured, &file.metadata().map_err(|_| "PAR2 metadata failed")?) {
        return Err("PAR2 verification opened identity changed".into());
    }
    Ok(file)
}

/// Read only exact protected names under an explicit existing root. No paths are
/// created or written. Identity/re-read checks do not guarantee an atomic snapshot.
pub fn verify_directory(
    par2_path: &Path,
    root: &Path,
    format_limits: super::Limits,
    limits: MultiRecoveryLimits,
) -> Result<Verification> {
    verify_directory_cancellable(
        par2_path,
        root,
        format_limits,
        limits,
        &AtomicBool::new(true),
    )
}

/// Cancellation is checked between bounded reads; it cannot interrupt a blocking
/// OS call. No partial verification report is returned after cancellation.
pub fn verify_directory_cancellable(
    par2_path: &Path,
    root: &Path,
    format_limits: super::Limits,
    limits: MultiRecoveryLimits,
    flag: &AtomicBool,
) -> Result<Verification> {
    active(flag)?;
    limits.validate()?;
    format_limits.validate()?;
    let root = std::path::absolute(root).map_err(|_| "PAR2 verification root is unavailable")?;
    let root_fences = fences(&root)?;
    if root_fences
        .iter()
        .any(|f| f.metadata.as_ref().is_none_or(|m| !m.is_dir()))
    {
        return Err("PAR2 verification requires an existing directory root".into());
    }
    let par2_path = std::path::absolute(par2_path)
        .map_err(|_| "PAR2 verification source path is unavailable")?;
    let source_fences = fences(&par2_path)?;
    let source_metadata = source_fences[0]
        .metadata
        .as_ref()
        .ok_or("PAR2 verification source is missing")?;
    let mut source = open(&par2_path, source_metadata)?;
    check(&source_fences)?;
    let set = Set::read_cancellable(&mut source, format_limits, flag)?;
    set.verification_bounds(limits)?;
    let protected_bytes: u64 = set
        .files
        .iter()
        .filter(|f| f.recoverable)
        .map(|f| f.bytes)
        .sum();
    // At most ten path chains, each bounded to 128 components / 8 KiB paths,
    // plus metadata/report/reread overhead. Caller buffers are internal here.
    if protected_bytes + u64::from(set.slice_bytes) + (12 << 20)
        > limits.recovery.max_working_bytes
    {
        return Err("PAR2 directory verification exceeds its working-memory budget".into());
    }
    let mut contents = Vec::new();
    let mut selected = Vec::new();
    for file in set.files.iter().filter(|f| f.recoverable) {
        active(flag)?;
        check(&root_fences)?;
        let path = root.join(&file.name);
        let captured = fences(&path)?;
        let mut input = match &captured[0].metadata {
            None => None,
            Some(m) if m.len() <= file.bytes => Some(open(&path, m)?),
            Some(_) => return Err("PAR2 protected file exceeds captured length".into()),
        };
        check(&captured)?;
        let mut bytes = Vec::new();
        if let Some(reader) = &mut input {
            let length = captured[0].metadata.as_ref().unwrap().len() as usize;
            bytes
                .try_reserve_exact(length)
                .map_err(|_| "PAR2 protected input allocation failed")?;
            bytes.resize(length, 0);
            for chunk in bytes.chunks_mut(CHUNK) {
                active(flag)?;
                reader
                    .read_exact(chunk)
                    .map_err(|_| "PAR2 protected input read failed")?;
                active(flag)?;
            }
        }
        check(&captured)?;
        contents.push((file.id, bytes));
        selected.push((captured, input));
    }
    let inputs: Vec<_> = contents
        .iter()
        .map(|(id, bytes)| RecoveryInput::new(*id, bytes))
        .collect();
    let mut result = set.verify_files_cancellable(&mut source, &inputs, limits, flag)?;
    let mut scratch = [0u8; CHUNK];
    for (((captured, input), (_, bytes)), report) in
        selected.iter_mut().zip(&contents).zip(&mut result.files)
    {
        active(flag)?;
        check(&root_fences)?;
        check(captured)?;
        report.present = Some(input.is_some());
        if let Some(reader) = input {
            if !same(
                captured[0].metadata.as_ref().unwrap(),
                &reader.metadata().map_err(|_| "PAR2 protected metadata failed")?,
            ) {
                return Err("PAR2 protected file changed".into());
            }
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|_| "PAR2 protected reread failed")?;
            for chunk in bytes.chunks(CHUNK) {
                active(flag)?;
                reader
                    .read_exact(&mut scratch[..chunk.len()])
                    .map_err(|_| "PAR2 protected reread failed")?;
                if &scratch[..chunk.len()] != chunk {
                    return Err("PAR2 protected bytes changed during verification".into());
                }
            }
        }
    }
    active(flag)?;
    check(&root_fences)?;
    check(&source_fences)?;
    for (captured, _) in &selected {
        check(captured)?;
    }
    set.verify_source(&mut source, flag)?;
    active(flag)?;
    Ok(result)
}
