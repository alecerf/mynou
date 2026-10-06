//! Original, bounded archive parsers. Decoding never chooses filesystem paths.
pub mod deflate;
mod zip;

use crate::Result;
pub use zip::{Entry, Method, VerifiedEntry, Zip};

/// Captured resource policy; changing a policy does not change an existing Zip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: u32,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    pub max_ratio: u32,
    pub max_blocks: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            max_entry_bytes: 1 << 30,
            max_total_bytes: 4 << 30,
            max_ratio: 1000,
            max_blocks: 16_384,
        }
    }
}
impl Limits {
    pub fn validate(self) -> Result<()> {
        if !(1..=1024).contains(&self.max_entries)
            || !(1..=u64::from(u32::MAX) - 1).contains(&self.max_entry_bytes)
            || self.max_total_bytes < self.max_entry_bytes
            || self.max_total_bytes > 1 << 40
            || !(1..=10_000).contains(&self.max_ratio)
            || !(1..=65_536).contains(&self.max_blocks)
        {
            return Err("Archive limits exceed the supported bounds".into());
        }
        Ok(())
    }
}
