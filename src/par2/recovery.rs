//! Bounded recovery into private buffers. No paths, writes or adoption authority.
use super::{RecoverySlice, Set, SliceChecksum, gf16};
use crate::{
    Result,
    crypto::{Md5, Sha256, md5},
    usenet::yenc::Crc32,
};
use std::{
    io::{Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

const CHUNK: usize = 64 << 10;
const OVERHEAD: u64 = 512 << 10;

/// Limits on additional buffers and field operations, excluding the captured Set
/// and caller-owned inputs. Applications may tighten, never raise, these bounds.
/// Multi-file recovery applies the byte/slice/erasure budgets to the entire set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryLimits {
    pub max_file_bytes: u64,
    pub max_slices: u32,
    pub max_missing_slices: u32,
    pub max_slice_bytes: u32,
    pub max_working_bytes: u64,
    pub max_field_operations: u64,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 << 20,
            max_slices: 256,
            max_missing_slices: 8,
            max_slice_bytes: 1 << 20,
            max_working_bytes: 32 << 20,
            max_field_operations: 128 << 20,
        }
    }
}

impl RecoveryLimits {
    pub fn validate(self) -> Result<()> {
        if !(1..=16 << 20).contains(&self.max_file_bytes)
            || !(1..=256).contains(&self.max_slices)
            || self.max_missing_slices > 8
            || !(4..=1 << 20).contains(&self.max_slice_bytes)
            || !self.max_slice_bytes.is_multiple_of(4)
            || !(1..=32 << 20).contains(&self.max_working_bytes)
            || !(1..=128 << 20).contains(&self.max_field_operations)
        {
            return Err("PAR2 recovery limits exceed the supported bounds".into());
        }
        Ok(())
    }
}

/// Aggregate recovery bounds. The existing single-file limits remain compatible;
/// their max_file_bytes field limits combined content when used here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultiRecoveryLimits {
    pub max_files: u32,
    pub recovery: RecoveryLimits,
}

impl Default for MultiRecoveryLimits {
    fn default() -> Self {
        Self {
            max_files: 8,
            recovery: RecoveryLimits::default(),
        }
    }
}

impl MultiRecoveryLimits {
    pub fn validate(self) -> Result<()> {
        if !(1..=8).contains(&self.max_files) {
            return Err("PAR2 multi-file recovery supports at most eight files".into());
        }
        self.recovery.validate()
    }
}

/// Explicit immutable bytes for one captured File ID. Empty/short bytes denote
/// a missing file/truncated prefix. An ID is not ownership or authorization.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryInput<'a> {
    pub(super) id: [u8; 16],
    pub(super) bytes: &'a [u8],
}

impl<'a> RecoveryInput<'a> {
    pub fn new(id: [u8; 16], bytes: &'a [u8]) -> Self {
        Self { id, bytes }
    }
}

/// Fully verified caller-owned output, bound to the captured File ID. This
/// integrity result grants no permission to write files or admit media.
#[derive(Debug, PartialEq, Eq)]
pub struct RecoveredFile {
    id: [u8; 16],
    bytes: Vec<u8>,
}

impl RecoveredFile {
    pub fn id(&self) -> &[u8; 16] {
        &self.id
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn active(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("PAR2 recovery was cancelled".into())
    }
}

fn buffer(length: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| "PAR2 recovery buffer allocation failed")?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn exact<R: Read>(reader: &mut R, bytes: &mut [u8], flag: &AtomicBool) -> Result<()> {
    for chunk in bytes.chunks_mut(CHUNK) {
        active(flag)?;
        reader
            .read_exact(chunk)
            .map_err(|error| format!("PAR2 recovery read: {error}"))?;
        active(flag)?;
    }
    active(flag)
}

pub(super) fn slice_matches(
    bytes: &[u8],
    expected: &SliceChecksum,
    flag: &AtomicBool,
) -> Result<bool> {
    let mut digest = Md5::default();
    let mut crc = Crc32::default();
    for chunk in bytes.chunks(CHUNK) {
        active(flag)?;
        digest.update(chunk);
        crc.update(chunk);
    }
    active(flag)?;
    Ok(digest.finalize() == expected.md5 && crc.finish() == expected.crc32)
}

pub(super) fn padded_slice(input: &[u8], index: usize, scratch: &mut [u8]) -> bool {
    scratch.fill(0);
    let start = index * scratch.len();
    let end = (start + scratch.len()).min(input.len());
    if start < end {
        scratch[..end - start].copy_from_slice(&input[start..end]);
    }
    start < input.len()
}

fn inverse_matrix(missing: &[usize], flag: &AtomicBool) -> Result<Vec<Vec<u16>>> {
    let count = missing.len();
    let mut rows = vec![vec![0u16; 2 * count]; count];
    for (exponent, row) in rows.iter_mut().enumerate() {
        for (column, index) in missing.iter().enumerate() {
            row[column] = gf16::coefficient(*index as u32, exponent as u32)?;
        }
        row[count + exponent] = 1;
    }
    for column in 0..count {
        active(flag)?;
        let pivot = (column..count)
            .find(|row| rows[*row][column] != 0)
            .ok_or("PAR2 recovery matrix is singular")?;
        rows.swap(column, pivot);
        let factor =
            gf16::inverse(rows[column][column]).ok_or("PAR2 recovery pivot is not invertible")?;
        for value in &mut rows[column] {
            *value = gf16::multiply(*value, factor);
        }
        let pivot_row = rows[column].clone();
        for (index, row) in rows.iter_mut().enumerate() {
            if index != column {
                let factor = row[column];
                for (target, source) in row.iter_mut().zip(&pivot_row) {
                    *target ^= gf16::multiply(*source, factor);
                }
            }
        }
    }
    active(flag)?;
    Ok(rows)
}

impl Set {
    /// Verify or reconstruct one described recoverable file. Short input denotes
    /// a truncated prefix; empty input denotes a wholly missing file. The input
    /// stays immutable and only a fully verified caller-owned Vec is returned.
    pub fn recover_single<R: Read + Seek>(
        &self,
        source: &mut R,
        protected: &[u8],
        limits: RecoveryLimits,
    ) -> Result<Vec<u8>> {
        self.recover_single_cancellable(source, protected, limits, &AtomicBool::new(true))
    }

    /// Cancellation cannot interrupt an arbitrary caller-provided blocking Read.
    /// Failed or cancelled reconstruction never returns its partial buffers.
    pub fn recover_single_cancellable<R: Read + Seek>(
        &self,
        source: &mut R,
        protected: &[u8],
        limits: RecoveryLimits,
        flag: &AtomicBool,
    ) -> Result<Vec<u8>> {
        active(flag)?;
        limits.validate()?;
        if self.files.len() != 1 || !self.files[0].recoverable {
            return Err("PAR2 recovery requires exactly one described recoverable file".into());
        }
        let inputs = [RecoveryInput::new(self.files[0].id, protected)];
        let mut outputs = self.recover_files_cancellable(
            source,
            &inputs,
            MultiRecoveryLimits {
                max_files: 1,
                recovery: limits,
            },
            flag,
        )?;
        Ok(outputs.remove(0).into_bytes())
    }

    /// Reconstruct every recoverable file in one captured set. Each File ID needs
    /// exactly one explicit input; caller order is irrelevant. Outputs follow
    /// captured Main order and are returned only after every file is verified.
    pub fn recover_files<R: Read + Seek>(
        &self,
        source: &mut R,
        inputs: &[RecoveryInput<'_>],
        limits: MultiRecoveryLimits,
    ) -> Result<Vec<RecoveredFile>> {
        self.recover_files_cancellable(source, inputs, limits, &AtomicBool::new(true))
    }

    /// No partial outputs escape failure/cancellation. Cancellation cannot
    /// interrupt an arbitrary caller-provided blocking Read.
    pub fn recover_files_cancellable<R: Read + Seek>(
        &self,
        source: &mut R,
        inputs: &[RecoveryInput<'_>],
        multi_limits: MultiRecoveryLimits,
        flag: &AtomicBool,
    ) -> Result<Vec<RecoveredFile>> {
        active(flag)?;
        multi_limits.validate()?;
        let count_files = self.files.iter().filter(|file| file.recoverable).count();
        if count_files == 0
            || count_files > multi_limits.max_files as usize
            || inputs.len() != count_files
        {
            return Err("PAR2 recovery requires one input per bounded recoverable file".into());
        }
        let files: Vec<_> = self.files.iter().filter(|file| file.recoverable).collect();
        for (index, input) in inputs.iter().enumerate() {
            if !files.iter().any(|file| file.id == input.id)
                || inputs[..index].iter().any(|other| other.id == input.id)
            {
                return Err("PAR2 recovery input File IDs are unknown or duplicated".into());
            }
        }
        let limits = multi_limits.recovery;
        let slice_bytes = self.slice_bytes as usize;
        if self.slice_bytes > limits.max_slice_bytes {
            return Err("PAR2 protected content exceeds recovery limits".into());
        }
        let mut protected = Vec::with_capacity(count_files);
        let mut lengths = Vec::with_capacity(count_files);
        let mut total_bytes = 0u64;
        let mut count = 0usize;
        for file in &files {
            let input = inputs
                .iter()
                .find(|input| input.id == file.id)
                .ok_or("PAR2 recoverable file input is missing")?;
            total_bytes = total_bytes
                .checked_add(file.bytes)
                .ok_or("PAR2 recovery content budget overflow")?;
            count = count
                .checked_add(file.slices.len())
                .ok_or("PAR2 recovery slice budget overflow")?;
            if total_bytes > limits.max_file_bytes
                || count > limits.max_slices as usize
                || input.bytes.len() as u64 > file.bytes
            {
                return Err("PAR2 protected content exceeds recovery limits".into());
            }
            lengths.push(usize::try_from(file.bytes).map_err(|_| "PAR2 file size is unsupported")?);
            protected.push(input.bytes);
        }
        let minimum_memory = total_bytes
            .checked_add(u64::from(self.slice_bytes))
            .and_then(|bytes| bytes.checked_add(OVERHEAD))
            .ok_or("PAR2 recovery memory budget overflow")?;
        if minimum_memory > limits.max_working_bytes {
            return Err("PAR2 recovery exceeds its working-memory budget".into());
        }
        self.verify_source(source, flag)?;
        let mut scratch = buffer(slice_bytes)?;
        let mut missing = Vec::new();
        let mut mapping = Vec::with_capacity(count);
        for (file_index, file) in files.iter().enumerate() {
            for (local, expected) in file.slices.iter().enumerate() {
                active(flag)?;
                let global = mapping.len();
                mapping.push((file_index, local));
                let complete = padded_slice(protected[file_index], local, &mut scratch)
                    && (local * slice_bytes + slice_bytes).min(lengths[file_index])
                        <= protected[file_index].len();
                if !complete || !slice_matches(&scratch, expected, flag)? {
                    missing.push(global);
                    if missing.len() > limits.max_missing_slices as usize {
                        return Err("PAR2 damaged slice count exceeds recovery limits".into());
                    }
                }
            }
        }
        let erased = missing.len() as u64;
        let working = erased
            .checked_mul(u64::from(self.slice_bytes))
            .and_then(|bytes| bytes.checked_add(minimum_memory))
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
        let mut residuals = Vec::new();
        for exponent in 0..missing.len() {
            let captured = self
                .recovery
                .iter()
                .find(|slice| slice.exponent == exponent as u32)
                .ok_or("PAR2 recovery requires consecutive parity exponents starting at zero")?;
            let mut row = self.read_recovery(source, captured, flag)?;
            for (index, (file_index, local)) in mapping.iter().enumerate() {
                if !missing.contains(&index) {
                    padded_slice(protected[*file_index], *local, &mut scratch);
                    gf16::add_scaled_cancellable(
                        &mut row,
                        &scratch,
                        gf16::coefficient(index as u32, exponent as u32)?,
                        flag,
                    )?;
                }
            }
            residuals.push(row);
        }
        let matrix = inverse_matrix(&missing, flag)?;
        let mut outputs = Vec::with_capacity(count_files);
        for (index, file) in files.iter().enumerate() {
            active(flag)?;
            let mut bytes = buffer(lengths[index])?;
            for (target, input) in bytes.chunks_mut(CHUNK).zip(protected[index].chunks(CHUNK)) {
                active(flag)?;
                target[..input.len()].copy_from_slice(input);
            }
            outputs.push(RecoveredFile { id: file.id, bytes });
        }
        for (row, index) in missing.iter().enumerate() {
            let (file_index, local) = mapping[*index];
            let file = files[file_index];
            scratch.fill(0);
            for (column, residual) in residuals.iter().enumerate() {
                gf16::add_scaled_cancellable(
                    &mut scratch,
                    residual,
                    matrix[row][missing.len() + column],
                    flag,
                )?;
            }
            let start = local * slice_bytes;
            let bytes = (lengths[file_index] - start).min(slice_bytes);
            if !slice_matches(&scratch, &file.slices[local], flag)?
                || scratch[bytes..].iter().any(|value| *value != 0)
            {
                return Err("PAR2 reconstructed slice integrity check failed".into());
            }
            outputs[file_index].bytes[start..start + bytes].copy_from_slice(&scratch[..bytes]);
        }
        for (file, output) in files.iter().zip(&outputs) {
            for (index, expected) in file.slices.iter().enumerate() {
                padded_slice(&output.bytes, index, &mut scratch);
                if !slice_matches(&scratch, expected, flag)? {
                    return Err("PAR2 final slice integrity check failed".into());
                }
            }
            let mut digest = Md5::default();
            for chunk in output.bytes.chunks(CHUNK) {
                active(flag)?;
                digest.update(chunk);
            }
            if digest.finalize() != file.md5
                || md5(&output.bytes[..output.bytes.len().min(16_384)]) != file.first_16k_md5
            {
                return Err("PAR2 reconstructed file integrity check failed".into());
            }
        }
        self.verify_source(source, flag)?;
        active(flag)?;
        Ok(outputs)
    }

    pub(super) fn verify_source<R: Read + Seek>(
        &self,
        reader: &mut R,
        flag: &AtomicBool,
    ) -> Result<()> {
        active(flag)?;
        let length = reader
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("PAR2 recovery seek: {error}"))?;
        if length != self.source_bytes {
            return Err("PAR2 source length changed after capture".into());
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("PAR2 recovery seek: {error}"))?;
        let mut bytes = [0u8; CHUNK];
        let mut remaining = length;
        let mut digest = Sha256::default();
        while remaining != 0 {
            let count = remaining.min(CHUNK as u64) as usize;
            exact(reader, &mut bytes[..count], flag)?;
            digest.update(&bytes[..count]);
            remaining -= count as u64;
        }
        active(flag)?;
        let current = reader
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("PAR2 recovery seek: {error}"))?;
        if current != length || digest.finalize() != self.source_sha256 {
            return Err("PAR2 source identity changed after capture".into());
        }
        active(flag)
    }

    fn read_recovery<R: Read + Seek>(
        &self,
        reader: &mut R,
        captured: &RecoverySlice,
        flag: &AtomicBool,
    ) -> Result<Vec<u8>> {
        active(flag)?;
        let offset = captured
            .data_offset
            .checked_sub(68)
            .ok_or("PAR2 captured recovery offset is invalid")?;
        let end = captured
            .data_offset
            .checked_add(u64::from(captured.bytes))
            .ok_or("PAR2 captured recovery extent overflow")?;
        if end > self.source_bytes || captured.bytes != self.slice_bytes {
            return Err("PAR2 captured recovery extent is invalid".into());
        }
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|error| format!("PAR2 recovery seek: {error}"))?;
        let mut header = [0u8; 68];
        exact(reader, &mut header, flag)?;
        if &header[..8] != b"PAR2\0PKT"
            || header[8..16] != (68 + u64::from(captured.bytes)).to_le_bytes()
            || header[16..32] != captured.packet_md5
            || header[32..48] != self.id
            || &header[48..64] != b"PAR 2.0\0RecvSlic"
            || header[64..68] != captured.exponent.to_le_bytes()
        {
            return Err("PAR2 captured recovery header changed".into());
        }
        let mut bytes = buffer(captured.bytes as usize)?;
        let mut packet = Md5::default();
        let mut payload = Sha256::default();
        packet.update(&header[32..]);
        for chunk in bytes.chunks_mut(CHUNK) {
            exact(reader, chunk, flag)?;
            packet.update(chunk);
            payload.update(chunk);
        }
        if packet.finalize() != captured.packet_md5 || payload.finalize() != captured.sha256 {
            return Err("PAR2 captured recovery payload changed".into());
        }
        active(flag)?;
        Ok(bytes)
    }
}
