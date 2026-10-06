//! Bounded queue identities, reservations and retained attempt budgets.
use crate::{
    Result,
    json::{self, Value},
    requesters::{digest, valid_digest, valid_id},
};
use std::collections::{BTreeMap, BTreeSet};
pub(super) const MAX_RECORDS: usize = 256;
pub(super) const MAX_SOURCES: usize = 64;
pub(super) const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_SNAPSHOT: usize = 8 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Preparing,
    Held,
    Queued,
    Downloading,
    Verifying,
    Paused,
    Complete,
    Cancelled,
    Failed,
}
impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Held => "held",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Verifying => "verifying",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "preparing" => Ok(Self::Preparing),
            "held" => Ok(Self::Held),
            "queued" => Ok(Self::Queued),
            "downloading" => Ok(Self::Downloading),
            "verifying" => Ok(Self::Verifying),
            "paused" => Ok(Self::Paused),
            "complete" => Ok(Self::Complete),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err("Usenet queue: invalid state".into()),
        }
    }
}
pub(super) fn text(v: &Value, k: &str) -> Result<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Usenet queue: invalid {k}"))
}
pub(super) fn number(v: &Value, k: &str) -> Result<u64> {
    v.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| format!("Usenet queue: invalid {k}"))
}
#[derive(Clone)]
pub(super) struct Reservation {
    pub part: u32,
    pub token: String,
}
#[derive(Clone)]
pub(super) struct Record {
    pub owner: Option<super::Owner>,
    pub id: String,
    pub source: String,
    pub file_index: usize,
    pub server: String,
    pub binding: String,
    pub max_file: u64,
    pub limit: u8,
    pub order: u64,
    pub revision: u64,
    pub phase: Phase,
    pub attempts: Vec<u8>,
    pub reservation: Option<Reservation>,
    pub next_attempt: u64,
    pub error: Option<String>,
}
impl Record {
    pub fn identity(
        source: &str,
        index: usize,
        server: &str,
        binding: &str,
        max_file: u64,
        limit: u8,
    ) -> String {
        digest(
            json::stringify(&Value::Array(vec![
                source.into(),
                (index as u32).into(),
                server.into(),
                binding.into(),
                max_file.to_string().into(),
                u32::from(limit).into(),
            ]))
            .as_bytes(),
        )
    }
    pub fn json(&self) -> Value {
        let mut v = Value::object();
        // Omit the field for preceding raw records to preserve their format contract.
        if let Some(owner) = &self.owner {
            v.insert("owner", owner.to_json());
        }
        v.insert("id", self.id.clone());
        v.insert("source", self.source.clone());
        v.insert("file_index", self.file_index as u32);
        v.insert("server", self.server.clone());
        v.insert("binding", self.binding.clone());
        v.insert("max_file", self.max_file.to_string());
        v.insert("limit", u32::from(self.limit));
        v.insert("order", self.order.to_string());
        v.insert("revision", self.revision.to_string());
        v.insert("state", self.phase.name());
        v.insert(
            "attempts",
            Value::Array(self.attempts.iter().map(|n| u32::from(*n).into()).collect()),
        );
        v.insert(
            "reservation",
            self.reservation.as_ref().map_or(Value::Null, |r| {
                let mut p = Value::object();
                p.insert("part", r.part);
                p.insert("token", r.token.clone());
                p
            }),
        );
        v.insert("next_attempt", self.next_attempt.to_string());
        v.insert("error", self.error.clone().map_or(Value::Null, Value::from));
        v
    }
    fn parse(v: &Value) -> Result<Self> {
        crate::numbering::only(
            v,
            &[
                "id",
                "source",
                "file_index",
                "server",
                "binding",
                "max_file",
                "limit",
                "order",
                "revision",
                "state",
                "attempts",
                "reservation",
                "next_attempt",
                "error",
                "owner",
            ],
        )?;
        let rows = v
            .get("attempts")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty() && a.len() <= super::super::nzb::MAX_SEGMENTS)
            .ok_or("Usenet queue: invalid attempts")?;
        let attempts = rows
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| "Usenet queue: invalid attempt count".to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        let reservation = match v.get("reservation") {
            Some(Value::Null) => None,
            Some(v) => {
                crate::numbering::only(v, &["part", "token"])?;
                Some(Reservation {
                    part: u32::try_from(number(v, "part")?)
                        .map_err(|_| "Usenet queue: invalid reserved part")?,
                    token: text(v, "token")?,
                })
            }
            None => return Err("Usenet queue: missing reservation".into()),
        };
        let error = match v.get("error") {
            Some(Value::Null) => None,
            Some(Value::String(s))
                if matches!(
                    s.as_str(),
                    "article_failed"
                        | "verification_failed"
                        | "attempts_exhausted"
                        | "provider_removed"
                        | "owner_inactive"
                ) =>
            {
                Some(s.clone())
            }
            _ => return Err("Usenet queue: invalid diagnostic".into()),
        };
        let r = Self {
            owner: v.get("owner").map(super::Owner::from_json).transpose()?,
            id: text(v, "id")?,
            source: text(v, "source")?,
            file_index: usize::try_from(number(v, "file_index")?)
                .map_err(|_| "Usenet queue: invalid file index")?,
            server: text(v, "server")?,
            binding: text(v, "binding")?,
            max_file: number(v, "max_file")?,
            limit: u8::try_from(number(v, "limit")?)
                .map_err(|_| "Usenet queue: invalid attempt limit")?,
            order: number(v, "order")?,
            revision: number(v, "revision")?,
            phase: Phase::parse(&text(v, "state")?)?,
            attempts,
            reservation,
            next_attempt: number(v, "next_attempt")?,
            error,
        };
        if !valid_digest(&r.id)
            || !valid_digest(&r.source)
            || !valid_digest(&r.binding)
            || !valid_id(&r.server)
            || r.file_index >= super::super::nzb::MAX_FILES
            || r.max_file == 0
            || r.max_file > 1 << 40
            || !(1..=10).contains(&r.limit)
            || r.attempts.iter().any(|n| *n > r.limit)
            || r.order == 0
            || r.revision < r.order
            || r.id
                != Self::captured_identity(
                    &r.source,
                    r.file_index,
                    &r.server,
                    &r.binding,
                    r.max_file,
                    r.limit,
                    r.owner.as_ref(),
                )
            || matches!(r.phase, Phase::Downloading | Phase::Verifying) != r.reservation.is_some()
            || r.reservation.as_ref().is_some_and(|p| {
                !valid_digest(&p.token)
                    || if r.phase == Phase::Verifying {
                        p.part != 0
                    } else {
                        p.part == 0
                            || p.part as usize > r.attempts.len()
                            || r.attempts[p.part as usize - 1] == 0
                    }
            })
            || (matches!(r.phase, Phase::Preparing | Phase::Held)
                && (r.attempts.iter().any(|n| *n != 0) || r.error.is_some()))
            || (r.phase == Phase::Held && r.owner.is_none())
            || (r.error.as_deref() == Some("owner_inactive") && r.owner.is_none())
            || (r.phase != Phase::Queued && r.next_attempt != 0)
        {
            return Err("Usenet queue: inconsistent record".into());
        }
        Ok(r)
    }
}
#[derive(Clone, Default)]
pub(super) struct Data {
    pub revision: u64,
    pub records: BTreeMap<String, Record>,
}
impl Data {
    pub fn json(&self) -> Value {
        let mut v = Value::object();
        v.insert("revision", self.revision.to_string());
        v.insert(
            "records",
            Value::Array(self.records.values().map(Record::json).collect()),
        );
        v
    }
    pub fn parse(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["revision", "records"])?;
        let revision = number(v, "revision")?;
        let rows = v
            .get("records")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_RECORDS)
            .ok_or("Usenet queue: excessive records")?;
        let mut records = BTreeMap::new();
        let mut orders = BTreeSet::new();
        let mut sources = BTreeSet::new();
        let mut count = 0;
        let mut locations = BTreeMap::new();
        let mut owners = BTreeSet::new();
        for v in rows {
            let r = Record::parse(v)?;
            let occupied = locations.insert((r.source.clone(), r.file_index), r.owner.is_some());
            if occupied.is_some_and(|owned| owned || r.owner.is_some())
                || r.owner
                    .as_ref()
                    .is_some_and(|o| !owners.insert(o.job_id.clone()))
            {
                return Err("Usenet queue: conflicting library ownership".into());
            }
            count += r.attempts.len();
            sources.insert(r.source.clone());
            if r.revision > revision
                || !orders.insert(r.order)
                || records.insert(r.id.clone(), r).is_some()
            {
                return Err("Usenet queue: conflicting records".into());
            }
        }
        if count > super::super::nzb::MAX_SEGMENTS || sources.len() > MAX_SOURCES {
            return Err("Usenet queue: retained inventory exceeds bounds".into());
        }
        Ok(Self { revision, records })
    }
    pub fn next(&mut self) -> Result<u64> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("Usenet queue: revision exhausted")?;
        Ok(self.revision)
    }
}
