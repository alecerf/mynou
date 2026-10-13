//! Private captured candidates and the last settled routing position.
use super::{identity, num, text};
use crate::{
    Result,
    crypto::random_bytes,
    integrations::{FeedEntry, Release},
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
};
use std::collections::BTreeSet;

pub(super) const MAX_ENTRIES: usize = 200;
pub(super) const MAX_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub source: String,
    pub binding: String,
    pub serial: String,
    pub policy_revision: u64,
    pub record_revision: u64,
    pub captured_at: u64,
    pub count: usize,
    pub library: bool,
    pub after: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct Generation {
    pub header: Header,
    pub entries: Vec<FeedEntry>,
}
impl Header {
    pub(crate) fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("source", self.source.clone());
        value.insert("binding", self.binding.clone());
        value.insert("serial", self.serial.clone());
        value.insert("policy_revision", self.policy_revision.to_string());
        value.insert("record_revision", self.record_revision.to_string());
        value.insert("captured_at", self.captured_at.to_string());
        value.insert("count", self.count as u32);
        value.insert("library", self.library);
        value.insert("after", self.after.clone().map_or(Value::Null, Value::from));
        value
    }
    pub(crate) fn from_json(value: &Value) -> Result<Self> {
        crate::numbering::only(
            value,
            &[
                "source",
                "binding",
                "serial",
                "policy_revision",
                "record_revision",
                "captured_at",
                "count",
                "library",
                "after",
            ],
        )?;
        let after = match value.get("after") {
            Some(Value::Null) => None,
            Some(Value::String(text))
                if text.len() == 48
                    && text
                        .bytes()
                        .all(|byte| matches!(byte,b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Some(text.clone())
            }
            _ => return Err("Feed routing: invalid progress cursor".into()),
        };
        let count = usize::try_from(num(value, "count")?)
            .map_err(|_| "Feed routing: invalid capture count")?;
        let header = Self {
            source: text(value, "source")?,
            binding: text(value, "binding")?,
            serial: text(value, "serial")?,
            policy_revision: num(value, "policy_revision")?,
            record_revision: num(value, "record_revision")?,
            captured_at: num(value, "captured_at")?,
            count,
            library: value
                .get("library")
                .and_then(Value::as_bool)
                .ok_or("Feed routing: invalid phase")?,
            after,
        };
        if !valid_id(&header.source)
            || !valid_digest(&header.binding)
            || !identity(&header.serial)
            || header.policy_revision == 0
            || header.record_revision == 0
            || !(1..=MAX_ENTRIES).contains(&count)
        {
            return Err("Feed routing: invalid generation header".into());
        }
        Ok(header)
    }
}
impl Generation {
    pub(crate) fn new(
        record: &super::Record,
        policy_revision: u64,
        entries: Vec<FeedEntry>,
        at: u64,
    ) -> Result<Self> {
        let header = Header {
            source: record.id.clone(),
            binding: record.binding.clone(),
            serial: digest(&random_bytes::<16>()?)[..32].to_owned(),
            policy_revision,
            record_revision: record.revision,
            captured_at: at,
            count: entries.len(),
            library: false,
            after: None,
        };
        Header::from_json(&header.to_json())?;
        let generation = Self { header, entries };
        generation.encoded_entries()?;
        Ok(generation)
    }
    pub(crate) fn encoded_entries(&self) -> Result<Vec<Vec<u8>>> {
        if self.entries.len() != self.header.count {
            return Err("Feed routing: capture count disagrees".into());
        }
        let mut ids = BTreeSet::new();
        let mut bytes = 0;
        self.entries
            .iter()
            .enumerate()
            .map(|(ordinal, entry)| {
                if !ids.insert(&entry.id) {
                    return Err("Feed routing: duplicate captured identity".into());
                }
                let value = encode_entry(&self.header, ordinal, entry);
                decode_entry(&self.header, ordinal, &value)?;
                let value = json::stringify(&value).into_bytes();
                bytes += value.len();
                if bytes > MAX_BYTES {
                    return Err("Feed routing: capture exceeds byte budget".into());
                }
                Ok(value)
            })
            .collect()
    }
}
pub(crate) fn encode_entry(header: &Header, ordinal: usize, entry: &FeedEntry) -> Value {
    let mut value = Value::object();
    value.insert("source", header.source.clone());
    value.insert("generation", header.serial.clone());
    value.insert("ordinal", ordinal as u32);
    value.insert("id", entry.id.clone());
    value.insert("title", entry.release.title.clone());
    value.insert("url", entry.release.url.clone());
    value.insert("seeders", entry.release.seeders.to_string());
    value
}
pub(crate) fn decode_entry(header: &Header, ordinal: usize, value: &Value) -> Result<FeedEntry> {
    crate::numbering::only(
        value,
        &[
            "source",
            "generation",
            "ordinal",
            "id",
            "title",
            "url",
            "seeders",
        ],
    )?;
    let id = text(value, "id")?;
    let title = text(value, "title")?;
    let url = text(value, "url")?;
    if text(value, "source")? != header.source
        || text(value, "generation")? != header.serial
        || num(value, "ordinal")? != ordinal as u64
        || ordinal >= header.count
        || !identity(&id)
        || title.is_empty()
        || title.len() > 2048
        || title.chars().any(char::is_control)
        || url.len() > 8192
        || crate::integrations::acquisition_url(&url, "")? != url
    {
        return Err("Feed routing: invalid captured entry or provenance".into());
    }
    Ok(FeedEntry {
        id,
        release: Release {
            title,
            url,
            seeders: num(value, "seeders")?,
        },
    })
}
