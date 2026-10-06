//! Original, bounded archive parsers. Decoding never chooses filesystem paths.
pub mod deflate;
mod rar;
mod zip;

use crate::{Result, json::Value};
pub use rar::{Entry as RarEntry, Rar5};
pub use zip::{Entry, Method, VerifiedEntry, Zip};
pub(crate) fn checked_entry_path(path: &str) -> Result<bool> {
    zip::checked_entry_path(path)
}

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
    pub fn to_json(self) -> Value {
        let mut v = Value::object();
        v.insert("max_entries", self.max_entries);
        v.insert("max_entry_bytes", self.max_entry_bytes.to_string());
        v.insert("max_total_bytes", self.max_total_bytes.to_string());
        v.insert("max_ratio", self.max_ratio);
        v.insert("max_blocks", self.max_blocks);
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "max_entries",
                "max_entry_bytes",
                "max_total_bytes",
                "max_ratio",
                "max_blocks",
            ],
        )?;
        let defaults = Self::default();
        let number = |key: &str, default: u64| -> Result<u64> {
            v.get(key).map_or(Ok(default), |n| {
                n.as_u64()
                    .or_else(|| n.as_str()?.parse().ok())
                    .ok_or_else(|| format!("Archive limits: invalid {key}"))
            })
        };
        let limits = Self {
            max_entries: u32::try_from(number("max_entries", u64::from(defaults.max_entries))?)
                .map_err(|_| "Archive entry limit is too large")?,
            max_entry_bytes: number("max_entry_bytes", defaults.max_entry_bytes)?,
            max_total_bytes: number("max_total_bytes", defaults.max_total_bytes)?,
            max_ratio: u32::try_from(number("max_ratio", u64::from(defaults.max_ratio))?)
                .map_err(|_| "Archive ratio limit is too large")?,
            max_blocks: u32::try_from(number("max_blocks", u64::from(defaults.max_blocks))?)
                .map_err(|_| "Archive block limit is too large")?,
        };
        limits.validate()?;
        Ok(limits)
    }
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
