//! Bounded file-backed recovery of one described file into a new private output.
//! Memory depends on the slice size and erasure count, never on the file size.
//! A checksum match grants no ownership: callers supply the exact plan.
use super::{
    FileDescription, Set, gf16,
    recovery::{CHUNK, OVERHEAD, active, buffer, inverse_matrix, select_rows, slice_matches},
};
use crate::{
    Result,
    crypto::{Md5, Sha256},
    store::{private_options, reject_symlinks, sync_directory},
};
use std::{
    fs,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const MAX_OWNER_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 128;
const PARTIAL: &str = ".partial";

/// Explicit bounds for file-backed recovery. Applications may tighten, never
/// raise, them. The in-memory `RecoveryLimits` remain unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamingLimits {
    pub max_file_bytes: u64,
    pub max_slices: u32,
    pub max_missing_slices: u32,
    pub max_slice_bytes: u32,
    pub max_working_bytes: u64,
    pub max_field_operations: u64,
}

impl Default for StreamingLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 30,
            max_slices: 32768,
            max_missing_slices: 8,
            max_slice_bytes: 1 << 20,
            max_working_bytes: 64 << 20,
            max_field_operations: 1 << 33,
        }
    }
}

impl StreamingLimits {
    pub fn validate(self) -> Result<()> {
        if !(1..=1 << 30).contains(&self.max_file_bytes)
            || !(1..=32768).contains(&self.max_slices)
            || self.max_missing_slices > 8
            || !(4..=1 << 20).contains(&self.max_slice_bytes)
            || !self.max_slice_bytes.is_multiple_of(4)
            || !(1..=64 << 20).contains(&self.max_working_bytes)
            || !(1..=1 << 34).contains(&self.max_field_operations)
        {
            return Err("PAR2 streaming limits exceed the supported bounds".into());
        }
        Ok(())
    }
}

/// Exact caller-captured destination and owner binding. `directory` must be an
/// existing private (no group/other access) non-symlink directory; the output
/// is created as a new file `file_name` and never replaces an existing path.
#[derive(Clone, Copy, Debug)]
pub struct StreamingPlan<'a> {
    pub directory: &'a Path,
    pub file_name: &'a str,
    pub owner: &'a [u8],
}

impl StreamingPlan<'_> {
    fn validate(&self) -> Result<()> {
        let name = self.file_name.as_bytes();
        if name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || name[0] == b'.'
            || self.file_name.ends_with(PARTIAL)
            || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || self.owner.is_empty()
            || self.owner.len() > MAX_OWNER_BYTES
        {
            return Err("PAR2 streaming plan name or owner is invalid".into());
        }
        reject_symlinks(self.directory)?;
        let metadata = fs::symlink_metadata(self.directory)
            .map_err(|error| format!("PAR2 streaming directory: {error}"))?;
        if !metadata.is_dir() {
            return Err("PAR2 streaming directory must be a directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("PAR2 streaming directory must be private".into());
            }
        }
        Ok(())
    }
}

/// Fully verified private output. The proof binds the captured set/source, file,
/// exact input digest, owner and limits; it is not a filesystem snapshot claim
/// and grants no permission to admit or import media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamedRecovery {
    set_id: [u8; 16],
    file_id: [u8; 16],
    path: PathBuf,
    bytes: u64,
    input_sha256: [u8; 32],
    policy_sha256: [u8; 32],
}

impl StreamedRecovery {
    pub fn file_id(&self) -> &[u8; 16] {
        &self.file_id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn input_sha256(&self) -> &[u8; 32] {
        &self.input_sha256
    }
    pub fn policy_sha256(&self) -> &[u8; 32] {
        &self.policy_sha256
    }

    /// Reopen and re-verify the exact private output against the captured set:
    /// padded slice checksums, full-file MD5, first-16-KiB MD5 and exact length.
    pub fn verify(&self, set: &Set, flag: &AtomicBool) -> Result<()> {
        active(flag)?;
        let file = set
            .files
            .iter()
            .find(|file| file.id == self.file_id)
            .ok_or("PAR2 streamed output file is not in the captured set")?;
        if set.id != self.set_id || file.bytes != self.bytes {
            return Err("PAR2 streamed output does not match the captured set".into());
        }
        reject_symlinks(&self.path)?;
        let mut block = buffer(set.slice_bytes as usize)?;
        check_output(&self.path, file, &mut block, flag)
    }
}

fn fill<R: Read>(reader: &mut R, bytes: &mut [u8], flag: &AtomicBool) -> Result<usize> {
    let mut total = 0;
    while total < bytes.len() {
        active(flag)?;
        let end = (total + CHUNK).min(bytes.len());
        match reader.read(&mut bytes[total..end]) {
            Ok(0) => break,
            Ok(count) => total += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("PAR2 recovery read: {error}")),
        }
    }
    active(flag)?;
    Ok(total)
}

fn seek<S: Seek>(stream: &mut S, from: SeekFrom) -> Result<u64> {
    stream
        .seek(from)
        .map_err(|error| format!("PAR2 recovery seek: {error}"))
}

fn slice_extent(file: &FileDescription, slice_bytes: usize, index: usize) -> (u64, usize) {
    let start = index as u64 * slice_bytes as u64;
    (start, (file.bytes - start).min(slice_bytes as u64) as usize)
}

fn check_output(
    path: &Path,
    file: &FileDescription,
    block: &mut [u8],
    flag: &AtomicBool,
) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("PAR2 streamed output: {error}"))?;
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Err("PAR2 streamed output is not the exact regular file".into());
    }
    let mut reader = private_options()
        .read(true)
        .open(path)
        .map_err(|error| format!("PAR2 streamed output open: {error}"))?;
    let mut whole = Md5::default();
    let mut prefix = Md5::default();
    for (index, expected) in file.slices.iter().enumerate() {
        let (start, valid) = slice_extent(file, block.len(), index);
        block.fill(0);
        if fill(&mut reader, &mut block[..valid], flag)? != valid
            || !slice_matches(block, expected, flag)?
        {
            return Err("PAR2 final slice integrity check failed".into());
        }
        whole.update(&block[..valid]);
        if start < 16_384 {
            prefix.update(&block[..valid.min((16_384 - start) as usize)]);
        }
    }
    let mut probe = [0u8; 1];
    if fill(&mut reader, &mut probe, flag)? != 0
        || whole.finalize() != file.md5
        || prefix.finalize() != file.first_16k_md5
    {
        return Err("PAR2 reconstructed file integrity check failed".into());
    }
    Ok(())
}

impl Set {
    /// Recover the single described file into a new private file. Equivalent to
    /// the cancellable form with a flag that is never cleared.
    pub fn recover_single_to_file<S: Read + Seek, I: Read + Seek>(
        &self,
        source: &mut S,
        input: &mut I,
        plan: &StreamingPlan<'_>,
        limits: StreamingLimits,
    ) -> Result<StreamedRecovery> {
        self.recover_single_to_file_cancellable(source, input, plan, limits, &AtomicBool::new(true))
    }

    /// Streams the immutable `input` (short = truncated prefix, empty = missing)
    /// in bounded chunks and writes only a verified result under `plan`. All
    /// planning, rank and parity reads happen before any file is created. The
    /// output appears under its final name only after complete read-back
    /// verification and never replaces an existing path; failure or cancellation
    /// removes the partial file. Cancellation cannot interrupt a blocking Read.
    pub fn recover_single_to_file_cancellable<S: Read + Seek, I: Read + Seek>(
        &self,
        source: &mut S,
        input: &mut I,
        plan: &StreamingPlan<'_>,
        limits: StreamingLimits,
        flag: &AtomicBool,
    ) -> Result<StreamedRecovery> {
        active(flag)?;
        limits.validate()?;
        plan.validate()?;
        if self.files.len() != 1 || !self.files[0].recoverable {
            return Err("PAR2 recovery requires exactly one described recoverable file".into());
        }
        let file = &self.files[0];
        let slice_bytes = self.slice_bytes as usize;
        let count = file.slices.len();
        if self.slice_bytes > limits.max_slice_bytes
            || file.bytes > limits.max_file_bytes
            || count > limits.max_slices as usize
            || count as u64 != file.bytes.div_ceil(u64::from(self.slice_bytes))
        {
            return Err("PAR2 protected content exceeds streaming limits".into());
        }
        let base_memory = 2 * u64::from(self.slice_bytes) + OVERHEAD;
        if base_memory > limits.max_working_bytes {
            return Err("PAR2 recovery exceeds its working-memory budget".into());
        }
        let target = plan.directory.join(plan.file_name);
        let partial = plan.directory.join(format!("{}{PARTIAL}", plan.file_name));
        for path in [&target, &partial] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Ok(_) => return Err("PAR2 streaming output path already exists".into()),
                Err(error) => return Err(format!("PAR2 streaming output path: {error}")),
            }
        }
        self.verify_source(source, flag)?;
        let input_len = seek(input, SeekFrom::End(0))?;
        if input_len > file.bytes {
            return Err("PAR2 recovery input exceeds the described file".into());
        }
        seek(input, SeekFrom::Start(0))?;

        let mut block = buffer(slice_bytes)?;
        let mut digest = Sha256::default();
        let mut missing = Vec::new();
        let mut consumed = 0u64;
        for (index, expected) in file.slices.iter().enumerate() {
            let (_, valid) = slice_extent(file, slice_bytes, index);
            block.fill(0);
            let got = fill(input, &mut block[..valid], flag)?;
            digest.update(&block[..got]);
            consumed += got as u64;
            if got != valid || !slice_matches(&block, expected, flag)? {
                missing.push(index);
                if missing.len() > limits.max_missing_slices as usize {
                    return Err("PAR2 damaged slice count exceeds recovery limits".into());
                }
            }
        }
        let mut probe = [0u8; 1];
        if consumed != input_len || fill(input, &mut probe, flag)? != 0 {
            return Err("PAR2 recovery input changed during reading".into());
        }
        let input_sha256 = digest.finalize();

        let erased = missing.len() as u64;
        let working = (erased + 2)
            .checked_mul(u64::from(self.slice_bytes))
            .and_then(|bytes| bytes.checked_add(OVERHEAD))
            .ok_or("PAR2 recovery memory budget overflow")?;
        let operations = erased
            .checked_mul(count as u64 + erased)
            .and_then(|words| words.checked_mul(u64::from(self.slice_bytes) / 2))
            .and_then(|words| words.checked_add(4 * erased * erased * erased))
            .ok_or("PAR2 recovery field-operation budget overflow")?;
        if working > limits.max_working_bytes || operations > limits.max_field_operations {
            return Err(
                "PAR2 recovery exceeds its working-memory or field-operation budget".into(),
            );
        }
        let mut available = limits.max_field_operations - operations;
        let selected = select_rows(&missing, &self.recovery, &mut available, flag)?;
        let matrix = inverse_matrix(&missing, &selected, flag)?;
        let mut residuals = Vec::with_capacity(selected.len());
        for captured in &selected {
            residuals.push(self.read_recovery(source, captured, flag)?);
        }

        let mut output = private_options()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|error| format!("PAR2 streaming output create: {error}"))?;
        let result = (|| -> Result<()> {
            output
                .set_len(file.bytes)
                .map_err(|error| format!("PAR2 streaming output size: {error}"))?;
            seek(input, SeekFrom::Start(0))?;
            let mut again = Sha256::default();
            let mut next = 0usize;
            for index in 0..count {
                let (start, valid) = slice_extent(file, slice_bytes, index);
                block.fill(0);
                let got = fill(input, &mut block[..valid], flag)?;
                again.update(&block[..got]);
                if missing.get(next) == Some(&index) {
                    next += 1;
                    continue;
                }
                if got != valid {
                    return Err("PAR2 recovery input changed during reading".into());
                }
                seek(&mut output, SeekFrom::Start(start))?;
                output
                    .write_all(&block[..valid])
                    .map_err(|error| format!("PAR2 streaming output write: {error}"))?;
                for (residual, captured) in residuals.iter_mut().zip(&selected) {
                    gf16::add_scaled_cancellable(
                        residual,
                        &block,
                        gf16::coefficient(index as u32, captured.exponent)?,
                        flag,
                    )?;
                }
            }
            if fill(input, &mut probe, flag)? != 0 || again.finalize() != input_sha256 {
                return Err("PAR2 recovery input changed during reading".into());
            }
            for (row, index) in missing.iter().enumerate() {
                let (start, valid) = slice_extent(file, slice_bytes, *index);
                block.fill(0);
                for (column, residual) in residuals.iter().enumerate() {
                    gf16::add_scaled_cancellable(
                        &mut block,
                        residual,
                        matrix[row][missing.len() + column],
                        flag,
                    )?;
                }
                if !slice_matches(&block, &file.slices[*index], flag)?
                    || block[valid..].iter().any(|value| *value != 0)
                {
                    return Err("PAR2 reconstructed slice integrity check failed".into());
                }
                seek(&mut output, SeekFrom::Start(start))?;
                output
                    .write_all(&block[..valid])
                    .map_err(|error| format!("PAR2 streaming output write: {error}"))?;
            }
            output
                .sync_all()
                .map_err(|error| format!("PAR2 streaming output sync: {error}"))?;
            check_output(&partial, file, &mut block, flag)?;
            self.verify_source(source, flag)?;
            active(flag)
        })();
        drop(output);
        let published = result.and_then(|()| {
            // A hard link cannot replace an existing path, unlike rename.
            fs::hard_link(&partial, &target)
                .map_err(|error| format!("PAR2 streaming output publish: {error}"))
        });
        let _ = fs::remove_file(&partial);
        published?;
        sync_directory(plan.directory)?;
        let mut policy = Sha256::default();
        policy.update(b"mynou-par2-streaming-v1");
        for part in [
            &self.id[..],
            &self.source_sha256,
            &self.source_bytes.to_le_bytes(),
            &file.id,
            &file.md5,
            &input_len.to_le_bytes(),
            &input_sha256,
            &(plan.owner.len() as u64).to_le_bytes(),
            plan.owner,
            &limits.max_file_bytes.to_le_bytes(),
            &limits.max_slices.to_le_bytes(),
            &limits.max_missing_slices.to_le_bytes(),
            &limits.max_slice_bytes.to_le_bytes(),
            &limits.max_working_bytes.to_le_bytes(),
            &limits.max_field_operations.to_le_bytes(),
        ] {
            policy.update(part);
        }
        Ok(StreamedRecovery {
            set_id: self.id,
            file_id: file.id,
            path: target,
            bytes: file.bytes,
            input_sha256,
            policy_sha256: policy.finalize(),
        })
    }
}
