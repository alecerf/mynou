//! Original, bounded PAR2 core packet reader. Inspection never selects paths,
//! repairs files, writes state, or grants permission to import media.
pub mod gf16;
mod parser;
mod recovery;
mod verification;

pub use recovery::{MultiRecoveryLimits, RecoveredFile, RecoveryInput, RecoveryLimits};
pub use verification::{
    FileVerification, Verification, verify_directory, verify_directory_cancellable,
};

use crate::{Result, json::Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_source_bytes: u64,
    pub max_packets: u32,
    pub max_files: u32,
    pub max_slices: u32,
    pub max_slice_bytes: u32,
    pub max_metadata_bytes: u64,
    pub max_recovery_bytes: u64,
    pub max_file_bytes: u64,
    pub max_total_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 << 20,
            max_packets: 4096,
            max_files: 1024,
            max_slices: 32768,
            max_slice_bytes: 1 << 20,
            max_metadata_bytes: 8 << 20,
            max_recovery_bytes: 64 << 20,
            max_file_bytes: 4 << 30,
            max_total_file_bytes: 64 << 30,
        }
    }
}

impl Limits {
    pub fn validate(self) -> Result<()> {
        if !(64..=256 << 20).contains(&self.max_source_bytes)
            || !(1..=4096).contains(&self.max_packets)
            || !(1..=1024).contains(&self.max_files)
            || !(1..=32768).contains(&self.max_slices)
            || !(4..=1 << 20).contains(&self.max_slice_bytes)
            || !self.max_slice_bytes.is_multiple_of(4)
            || !(64..=8 << 20).contains(&self.max_metadata_bytes)
            || self.max_recovery_bytes > 64 << 20
            || !(1..=4 << 30).contains(&self.max_file_bytes)
            || self.max_total_file_bytes < self.max_file_bytes
            || self.max_total_file_bytes > 64 << 30
        {
            return Err("PAR2 limits exceed the supported bounds".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceChecksum {
    md5: [u8; 16],
    crc32: u32,
}
impl SliceChecksum {
    pub fn md5(&self) -> &[u8; 16] {
        &self.md5
    }
    pub fn crc32(&self) -> u32 {
        self.crc32
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDescription {
    id: [u8; 16],
    md5: [u8; 16],
    first_16k_md5: [u8; 16],
    bytes: u64,
    name: String,
    recoverable: bool,
    slices: Vec<SliceChecksum>,
}
impl FileDescription {
    pub fn id(&self) -> &[u8; 16] {
        &self.id
    }
    pub fn md5(&self) -> &[u8; 16] {
        &self.md5
    }
    pub fn first_16k_md5(&self) -> &[u8; 16] {
        &self.first_16k_md5
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_recoverable(&self) -> bool {
        self.recoverable
    }
    pub fn slices(&self) -> &[SliceChecksum] {
        &self.slices
    }
}

/// Checked source offsets and checksum binding, without retaining recovery data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverySlice {
    exponent: u32,
    data_offset: u64,
    bytes: u32,
    packet_md5: [u8; 16],
    sha256: [u8; 32],
}
impl RecoverySlice {
    pub fn exponent(&self) -> u32 {
        self.exponent
    }
    pub fn data_offset(&self) -> u64 {
        self.data_offset
    }
    pub fn bytes(&self) -> u32 {
        self.bytes
    }
    pub fn packet_md5(&self) -> &[u8; 16] {
        &self.packet_md5
    }
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Set {
    id: [u8; 16],
    slice_bytes: u32,
    // Preserve Main packet order for subsequent recovery coefficient assignment.
    files: Vec<FileDescription>,
    recovery: Vec<RecoverySlice>,
    creators: Vec<String>,
    packet_count: u32,
    ignored_packets: u32,
    source_bytes: u64,
    source_sha256: [u8; 32],
    set_id_matches_main: bool,
    limits: Limits,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

impl Set {
    pub fn id(&self) -> &[u8; 16] {
        &self.id
    }
    pub fn slice_bytes(&self) -> u32 {
        self.slice_bytes
    }
    pub fn files(&self) -> &[FileDescription] {
        &self.files
    }
    pub fn recovery(&self) -> &[RecoverySlice] {
        &self.recovery
    }
    pub fn creators(&self) -> &[String] {
        &self.creators
    }
    pub fn packet_count(&self) -> u32 {
        self.packet_count
    }
    pub fn ignored_packets(&self) -> u32 {
        self.ignored_packets
    }
    pub fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
    pub fn source_sha256(&self) -> &[u8; 32] {
        &self.source_sha256
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
    pub fn set_id_matches_main(&self) -> bool {
        self.set_id_matches_main
    }

    pub fn report(&self) -> Value {
        let mut value = Value::object();
        value.insert("format", "par2-core");
        value.insert("set_id", hex(&self.id));
        value.insert("set_id_matches_main", self.set_id_matches_main);
        value.insert("slice_bytes", self.slice_bytes);
        value.insert("source_bytes", self.source_bytes.to_string());
        value.insert("source_sha256", hex(&self.source_sha256));
        value.insert("packet_count", self.packet_count);
        value.insert("ignored_packets", self.ignored_packets);
        value.insert("packet_checksums_verified", true);
        value.insert("content_verified", false);
        value.insert("repair_supported", false);
        value.insert(
            "creators",
            Value::Array(
                self.creators
                    .iter()
                    .map(|c| Value::String(c.clone()))
                    .collect(),
            ),
        );
        value.insert(
            "files",
            Value::Array(
                self.files
                    .iter()
                    .map(|file| {
                        let mut v = Value::object();
                        v.insert("file_id", hex(&file.id));
                        v.insert("name", file.name.clone());
                        v.insert("bytes", file.bytes.to_string());
                        v.insert("md5", hex(&file.md5));
                        v.insert("first_16k_md5", hex(&file.first_16k_md5));
                        v.insert("recoverable", file.recoverable);
                        v.insert("checksum_slices", file.slices.len() as u32);
                        v
                    })
                    .collect(),
            ),
        );
        value.insert(
            "recovery",
            Value::Array(
                self.recovery
                    .iter()
                    .map(|slice| {
                        let mut v = Value::object();
                        v.insert("exponent", slice.exponent);
                        v.insert("bytes", slice.bytes);
                        v.insert("data_offset", slice.data_offset.to_string());
                        v.insert("sha256", hex(&slice.sha256));
                        v
                    })
                    .collect(),
            ),
        );
        value
    }
}
