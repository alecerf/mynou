//! Narrow browser-only progress projections and a content-pinned first-party asset.
use super::{
    Response,
    forms::{Form, valid_id},
};
use crate::{
    Result,
    crypto::sha256,
    engine::{Engine, lock},
    json::{self, Value},
};
use std::{collections::BTreeSet, sync::OnceLock};

pub(super) const SCRIPT: &str = include_str!("live.js");
pub(super) const MAX_REPLY: usize = 48 * 1024;
const JOB_STATES: &[&str] = &[
    "queued",
    "processing",
    "downloading",
    "scanning",
    "importing",
    "imported",
    "staged",
    "ready",
    "failed",
    "cancelled",
];
pub(super) fn integrity() -> &'static str {
    static INTEGRITY: OnceLock<String> = OnceLock::new();
    INTEGRITY.get_or_init(|| {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let digest = sha256(SCRIPT.as_bytes());
        let mut result = String::from("sha256-");
        for chunk in digest.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            for index in [a >> 2, ((a & 3) << 4) | (b >> 4)] {
                result.push(char::from(TABLE[usize::from(index)]));
            }
            result.push(if chunk.len() > 1 {
                char::from(TABLE[usize::from(((b & 15) << 2) | (c >> 6))])
            } else {
                '='
            });
            result.push(if chunk.len() > 2 {
                char::from(TABLE[usize::from(c & 63)])
            } else {
                '='
            });
        }
        result
    })
}

pub(super) fn error(status: u16, code: &str) -> Response {
    let mut value = Value::object();
    value.insert("error", code);
    Response {
        allow: "GET",
        ..response(status, value)
    }
}

fn response(status: u16, value: Value) -> Response {
    Response {
        content_type: "application/json; charset=utf-8",
        ..Response::html(status, json::stringify(&value))
    }
}

pub(super) fn ids(query: &str, native: bool) -> Result<Vec<String>> {
    if query.len() > 4096 {
        return Err("Live query exceeds the limit".into());
    }
    let form = Form::parse(query.as_bytes())?;
    form.only(&["ids"])?;
    let mut ids = Vec::new();
    let mut unique = BTreeSet::new();
    for id in form.value("ids")?.split(',') {
        if ids.len() == 50 || !valid_id(id, native) {
            return Err("Live updates require between one and fifty valid identifiers".into());
        }
        let id = id.to_ascii_lowercase();
        if !unique.insert(id.clone()) {
            return Err("Live identifiers must be unique".into());
        }
        ids.push(id);
    }
    Ok(ids)
}

pub(super) fn snapshot(engine: &Engine, kind: &str, ids: &[String]) -> Result<Response> {
    let mut entries = Vec::with_capacity(ids.len());
    if kind == "jobs" {
        let store = lock(&engine.store)?;
        for id in ids {
            let mut entry = identity(id);
            if let Some((status, completion, attempts)) = store.job_progress(id) {
                entry.insert("state", state(status, JOB_STATES)?);
                entry.insert("progress", progress(completion));
                entry.insert("attempts", attempts);
            } else {
                entry.insert("missing", true);
            }
            entries.push(entry);
        }
    } else {
        entries = engine
            .transfer_progress(ids)?
            .as_array()
            .ok_or("Transfer progress is unavailable")?
            .to_vec();
    }
    let mut value = Value::object();
    value.insert("kind", kind);
    value.insert("entries", Value::Array(entries));
    let reply = response(200, value);
    if reply.body.len() > MAX_REPLY {
        return Err("Live reply exceeds the limit".into());
    }
    Ok(reply)
}

fn identity(id: &str) -> Value {
    let mut entry = Value::object();
    entry.insert("id", id);
    entry
}

fn state<'a>(value: &'a str, allowed: &[&str]) -> Result<&'a str> {
    if allowed.contains(&value) {
        Ok(value)
    } else {
        Err("Progress state is unavailable".into())
    }
}

fn progress(value: f64) -> Value {
    Value::Number(if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    })
}
