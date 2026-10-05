//! Original fixed-delimiter announcement grammar; no pattern engine or URL fields.
use super::Announcement;
use crate::{Result, json::Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delimited {
    prefix: String,
    separator: String,
    suffix: String,
    fields: Vec<String>,
}
impl Delimited {
    pub(crate) fn from_json(v: &Value) -> Result<Option<Self>> {
        super::only(v, &["type", "prefix", "separator", "suffix", "fields"])?;
        let kind = super::text(v, "type")?;
        if kind == "json" {
            super::only(v, &["type"])?;
            return Ok(None);
        }
        if kind != "delimited" {
            return Err("IRC: unsupported announcement format".into());
        }
        let prefix = super::text(v, "prefix")?;
        let separator = super::text(v, "separator")?;
        let suffix = v
            .get("suffix")
            .map_or(Ok(String::new()), |_| super::text(v, "suffix"))?;
        let valid_literal = |s: &str, max: usize| {
            s.len() <= max && s.is_ascii() && !s.chars().any(char::is_control)
        };
        if prefix.is_empty()
            || !valid_literal(&prefix, 128)
            || separator.is_empty()
            || !valid_literal(&separator, 16)
            || !separator.bytes().any(|b| b.is_ascii_punctuation())
            || !valid_literal(&suffix, 128)
        {
            return Err("IRC: invalid announcement delimiters".into());
        }
        let fields = v
            .get("fields")
            .and_then(Value::as_array)
            .filter(|f| (6..=8).contains(&f.len()))
            .ok_or("IRC: announcement field bounds are invalid")?;
        let mut names = BTreeSet::new();
        let mut ordered = Vec::new();
        for f in fields {
            let name = f.as_str().ok_or("IRC: invalid announcement field")?;
            if !matches!(
                name,
                "title"
                    | "kind"
                    | "media_title"
                    | "year"
                    | "season"
                    | "episode"
                    | "tmdb_id"
                    | "info_hash"
            ) || !names.insert(name)
            {
                return Err("IRC: unknown or duplicate announcement field".into());
            }
            ordered.push(name.into());
        }
        if [
            "title",
            "kind",
            "media_title",
            "year",
            "tmdb_id",
            "info_hash",
        ]
        .iter()
        .any(|n| !names.contains(n))
            || names.contains("season") != names.contains("episode")
        {
            return Err("IRC: announcement format requires complete identity fields".into());
        }
        Ok(Some(Self {
            prefix,
            separator,
            suffix,
            fields: ordered,
        }))
    }
    pub(crate) fn configuration(&self) -> Value {
        let mut v = Value::object();
        v.insert("type", "delimited");
        v.insert("prefix", self.prefix.clone());
        v.insert("separator", self.separator.clone());
        v.insert("suffix", self.suffix.clone());
        v.insert(
            "fields",
            Value::Array(self.fields.iter().cloned().map(Value::from).collect()),
        );
        v
    }
    pub(crate) fn payload(&self, line: &str) -> Result<Option<String>> {
        let clean = formatting(line)?;
        if !clean.starts_with(&self.prefix) {
            return Ok(None);
        }
        Ok(Some(clean))
    }
    pub(crate) fn decode(&self, line: &str) -> Result<Announcement> {
        let body = line
            .strip_prefix(&self.prefix)
            .and_then(|s| s.strip_suffix(&self.suffix))
            .ok_or("IRC: announcement literals do not match")?;
        let mut parts = body.split(&self.separator);
        let mut v = Value::object();
        for field in &self.fields {
            let part = parts
                .next()
                .filter(|p| !p.is_empty() && p.trim() == *p)
                .ok_or("IRC: announcement fields do not match")?;
            let lower = part.to_ascii_lowercase();
            if lower.contains("://")
                || lower.contains("magnet:")
                || lower.contains("passkey=")
                || lower.contains("token=")
                || lower.contains("apikey=")
            {
                return Err("IRC: source links are not announcement fields".into());
            }
            if matches!(field.as_str(), "year" | "season" | "episode") {
                let n = part
                    .parse::<u32>()
                    .map_err(|_| "IRC: invalid announcement number")?;
                if n.to_string() != part {
                    return Err("IRC: announcement numbers must be canonical decimals".into());
                }
                v.insert(field, n);
            } else {
                v.insert(field, part);
            }
        }
        if parts.next().is_some() {
            return Err("IRC: extra or ambiguous announcement fields".into());
        }
        Announcement::from_json(&v)
    }
}
pub(crate) fn style(c: char) -> bool {
    matches!(
        c,
        '\u{2}'
            | '\u{3}'
            | '\u{4}'
            | '\u{f}'
            | '\u{11}'
            | '\u{16}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{1f}'
    )
}
pub(crate) fn formatting(line: &str) -> Result<String> {
    if line.len() > super::protocol::MAX_LINE - 2
        || line.chars().any(|c| c.is_control() && !style(c))
    {
        return Err("IRC: invalid text announcement bounds or controls".into());
    }
    let mut chars = line.chars().peekable();
    let mut clean = String::with_capacity(line.len());
    while let Some(c) = chars.next() {
        if c == '\u{3}' {
            let mut foreground = 0;
            while foreground < 2 && chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
                foreground += 1;
            }
            let mut after = chars.clone();
            if foreground > 0
                && after.next() == Some(',')
                && after.peek().is_some_and(char::is_ascii_digit)
            {
                chars.next();
                for _ in 0..2 {
                    if chars.peek().is_some_and(char::is_ascii_digit) {
                        chars.next();
                    }
                }
            }
        } else if c == '\u{4}' {
            let mut after = chars.clone();
            if (0..6).all(|_| after.next().is_some_and(|c| c.is_ascii_hexdigit())) {
                chars = after;
                let mut after = chars.clone();
                if after.next() == Some(',')
                    && (0..6).all(|_| after.next().is_some_and(|c| c.is_ascii_hexdigit()))
                {
                    chars = after;
                }
            }
        } else if !style(c) {
            clean.push(c);
        }
    }
    Ok(clean)
}
