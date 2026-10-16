//! The pre-index `series.json` snapshot (schemas 1 and 2) and the schema 3
//! marker that replaces it once the index owns series storage.
//!
//! Legacy parsing keeps every bound that applied to a snapshot (8 MiB file,
//! per-record and per-field limits, checksum, identity invariants). The former
//! 128-record and 20,000-episode totals are not applied: they limited how many
//! records the snapshot could hold, which the index no longer does.
use super::{Record, fields, hex, text};
use crate::{
    Result,
    crypto::{constant_time_eq, sha256},
    json::{self, Value},
    store::{before_private_open, private_options, reject_symlinks, same_inode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

/// Parser bound for the legacy file; the new store has no such total.
pub(super) const MAX_LEGACY_SNAPSHOT: usize = 8 * 1024 * 1024;
const MARKER_DOMAIN: &[u8] = b"mynou-series-index/1\0";

pub(super) enum Snapshot {
    Missing,
    /// A checksum-valid schema 1 or 2 snapshot: raw bytes and their digest.
    Legacy {
        bytes: Vec<u8>,
        digest: [u8; 32],
        schema: u64,
    },
    /// The schema 3 marker, binding the directory to one index identity.
    Marker {
        binding: String,
    },
}

pub(super) fn marker_bytes(identity: &[u8; 32]) -> Vec<u8> {
    let mut input = MARKER_DOMAIN.to_vec();
    input.extend_from_slice(identity);
    let mut envelope = Value::object();
    envelope.insert("schema_version", 3_u32);
    envelope.insert("records", Value::Array(Vec::new()));
    envelope.insert("digest", hex(&sha256(&input)));
    json::stringify(&envelope).into_bytes()
}

pub(super) fn marker_binding(identity: &[u8; 32]) -> String {
    let mut input = MARKER_DOMAIN.to_vec();
    input.extend_from_slice(identity);
    hex(&sha256(&input))
}

/// Reads and checksum-validates `series.json` without interpreting records.
pub(super) fn inspect(path: &Path, repair_permissions: bool) -> Result<Snapshot> {
    reject_symlinks(path)?;
    let inspected = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Snapshot::Missing),
        Err(error) => return Err(format!("Cannot inspect series snapshot: {error}")),
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() > MAX_LEGACY_SNAPSHOT as u64 {
                return Err("Invalid or oversized series snapshot".into());
            }
            metadata
        }
    };
    before_private_open(path);
    let mut file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| format!("Cannot open series snapshot: {error}"))?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if !opened.is_file() {
        return Err("Invalid or oversized series snapshot".into());
    }
    if opened.nlink() != 1 {
        return Err("Series snapshots cannot share hard links".into());
    }
    if !same_inode(&inspected, &opened) {
        return Err("Series snapshot changed while opening".into());
    }
    if repair_permissions {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_LEGACY_SNAPSHOT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_LEGACY_SNAPSHOT {
        return Err("Series snapshot grew beyond its bound".into());
    }
    let envelope =
        json::parse(std::str::from_utf8(&bytes).map_err(|_| "Invalid series snapshot encoding")?)?;
    fields(&envelope, &["schema_version", "records", "digest"])?;
    let schema = envelope
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or("Unsupported series snapshot schema")?;
    let payload = envelope
        .get("records")
        .ok_or("Missing series snapshot records")?;
    match schema {
        1 | 2 => {
            let digest = hex(&sha256(json::stringify(payload).as_bytes()));
            if !constant_time_eq(digest.as_bytes(), text(&envelope, "digest")?.as_bytes()) {
                return Err("Series snapshot checksum mismatch".into());
            }
            Ok(Snapshot::Legacy {
                digest: sha256(&bytes),
                bytes,
                schema,
            })
        }
        3 => {
            if payload.as_array().is_none_or(|records| !records.is_empty()) {
                return Err("Invalid series index marker".into());
            }
            Ok(Snapshot::Marker {
                binding: text(&envelope, "digest")?.to_owned(),
            })
        }
        _ => Err("Unsupported series snapshot schema".into()),
    }
}

/// Parses every record of a checksum-valid legacy snapshot and validates the
/// identity invariants between overlapping scopes. Nothing is written.
pub(super) fn records(bytes: &[u8], schema: u64) -> Result<Vec<Record>> {
    let envelope =
        json::parse(std::str::from_utf8(bytes).map_err(|_| "Invalid series snapshot encoding")?)?;
    let values = envelope
        .get("records")
        .and_then(Value::as_array)
        .ok_or("Invalid series snapshot records")?;
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    for value in values {
        if schema == 2 && (value.get("anchors").is_none() || value.get("numbering").is_none()) {
            return Err("Missing versioned numbering data".into());
        }
        let mut record = Record::from_json(value)?;
        record.plan.episodes.sort_by_key(|e| (e.season, e.episode));
        if records.insert(record.id.clone(), record).is_some() {
            return Err("Duplicate persistent series identifier".into());
        }
    }
    validate_identities(records.values())?;
    Ok(records.into_values().collect())
}

/// The invariants the whole snapshot used to enforce on every save.
fn validate_identities<'a>(records: impl Iterator<Item = &'a Record>) -> Result<()> {
    let mut keys = BTreeSet::new();
    let mut identities = BTreeMap::new();
    let mut owners = BTreeMap::new();
    for record in records {
        record.validate()?;
        if !keys.insert(record.plan.request.canonical_key()) {
            return Err("Duplicate persistent series scope".into());
        }
        for (id, number) in &record.anchors {
            let series = record.plan.request.tmdb_id;
            if identities
                .insert((series, *id), *number)
                .is_some_and(|old| old != *number)
                || owners
                    .insert((series, *number), *id)
                    .is_some_and(|old| old != *id)
            {
                return Err(
                    "Overlapping series scopes disagree on retained episode identities".into(),
                );
            }
        }
    }
    Ok(())
}
