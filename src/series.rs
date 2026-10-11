//! Durable catalog plans and narrowly scoped monitoring controls.
//!
//! Inventory-wide storage lives in the disk-resident index (`storage`); the
//! per-plan bounds below limit one record, never how many records are kept.
mod engine;
mod legacy;
mod migration;
mod numbering;
mod storage;
use crate::{Result, date, integrations::report_text, json::Value, store::Request};
pub use engine::{CalendarQuery, SeriesPageQuery};
pub use numbering::{NumberingChoice, NumberingRequest};
use std::collections::{BTreeMap, BTreeSet};
pub use storage::SeriesStore;

/// Episodes in one catalog plan, and entries in one record's numbering history.
pub const MAX_EPISODES: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    pub catalog_id: Option<u64>,
    pub season: u32,
    pub episode: u32,
    pub title: String,
    pub air_date: Option<String>,
}

impl Episode {
    pub fn validate(&self) -> Result<()> {
        if self.season > 9999
            || self
                .catalog_id
                .is_some_and(|id| id == 0 || id > 9_007_199_254_740_991)
            || self.episode == 0
            || self.episode > 99999
            || self.title.len() > 2048
            || self.title.chars().any(char::is_control)
        {
            return Err("Invalid catalog episode".into());
        }
        if let Some(date) = &self.air_date {
            date::day(date)?;
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("season", self.season);
        value.insert(
            "catalog_id",
            self.catalog_id
                .map_or(Value::Null, |id| Value::Number(id as f64)),
        );
        value.insert("episode", self.episode);
        value.insert("title", self.title.clone());
        value.insert(
            "air_date",
            self.air_date.clone().map_or(Value::Null, Value::String),
        );
        value
    }
    fn from_json(value: &Value) -> Result<Self> {
        fields(
            value,
            &["catalog_id", "season", "episode", "title", "air_date"],
        )?;
        let episode = Self {
            catalog_id: match value.get("catalog_id") {
                Some(Value::Null) | None => None,
                Some(value) => Some(value.as_u64().ok_or("Invalid episode catalog identity")?),
            },
            season: small(value, "season")?,
            episode: small(value, "episode")?,
            title: text(value, "title")?.into(),
            air_date: match value.get("air_date") {
                Some(Value::String(text)) => Some(text.clone()),
                Some(Value::Null) => None,
                _ => return Err("Invalid episode air date".into()),
            },
        };
        episode.validate()?;
        Ok(episode)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub request: Request,
    pub episodes: Vec<Episode>,
}

impl Plan {
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;
        if self.request.kind != "series"
            || self
                .request
                .tmdb_id
                .is_none_or(|id| id == 0 || id > 9_007_199_254_740_991)
            || self.request.source_path.is_some()
            || self.request.source_url.is_some()
            || self.episodes.len() > MAX_EPISODES
        {
            return Err("Invalid series catalog plan".into());
        }
        let mut seen = BTreeSet::new();
        let mut identities = BTreeSet::new();
        for episode in &self.episodes {
            episode.validate()?;
            if !seen.insert((episode.season, episode.episode)) {
                return Err("Ambiguous duplicate episode numbering".into());
            }
            if episode.catalog_id.is_some_and(|id| !identities.insert(id)) {
                return Err("Ambiguous reused episode catalog identity".into());
            }
            if self.request.season != 0 && self.request.season != episode.season
                || self.request.episode != 0 && self.request.episode != episode.episode
            {
                return Err("Episode does not match the series scope".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub plan: Plan,
    pub monitored: bool,
    pub include_specials: bool,
    pub start_date: Option<String>,
    pub excluded: BTreeSet<(u32, u32)>,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub checked_at: u64,
    pub next_check_at: u64,
    pub last_error: Option<String>,
    pub anchors: BTreeMap<u64, crate::numbering::EpisodeNumber>,
    pub numbering: BTreeMap<u64, NumberingChoice>,
}

impl Record {
    pub fn episode_monitored(&self, episode: &Episode) -> bool {
        self.monitored
            && (episode.season != 0 || self.include_specials)
            && !self.excluded.contains(&(episode.season, episode.episode))
            && self.start_date.as_deref().is_none_or(|start| {
                episode
                    .air_date
                    .as_deref()
                    .is_some_and(|date| date >= start)
            })
    }
    pub fn episode_request(&self, episode: &Episode) -> Request {
        Request {
            kind: "episode".into(),
            title: self.plan.request.title.clone(),
            year: self.plan.request.year,
            season: episode.season,
            episode: episode.episode,
            source_path: None,
            source_url: None,
            source_numbering: episode
                .catalog_id
                .and_then(|id| self.numbering.get(&id).map(|choice| choice.source)),
            tmdb_id: self.plan.request.tmdb_id,
        }
    }
    fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        self.validate_numbering()?;
        if self.id.len() != 32
            || !self
                .id
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            || self.excluded.len() > MAX_EPISODES
            || self.revision == 0
            || self
                .last_error
                .as_ref()
                .is_some_and(|text| text.len() > 2048 || text.chars().any(char::is_control))
        {
            return Err("Invalid persistent series record".into());
        }
        if let Some(start) = &self.start_date {
            date::day(start)?;
        }
        if self
            .excluded
            .iter()
            .any(|(season, episode)| *season > 9999 || *episode == 0 || *episode > 99999)
        {
            return Err("Invalid excluded episode number".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("id", self.id.clone());
        value.insert("request", self.plan.request.to_json());
        value.insert(
            "episodes",
            Value::Array(self.plan.episodes.iter().map(Episode::to_json).collect()),
        );
        value.insert(
            "anchors",
            Value::Array(
                self.anchors
                    .iter()
                    .map(|(id, number)| {
                        let mut value = number.to_json();
                        value.insert("catalog_id", Value::Number(*id as f64));
                        value
                    })
                    .collect(),
            ),
        );
        value.insert(
            "numbering",
            Value::Array(
                self.numbering
                    .values()
                    .map(|choice| choice.to_json())
                    .collect(),
            ),
        );
        value.insert("monitored", self.monitored);
        value.insert("include_specials", self.include_specials);
        value.insert(
            "start_date",
            self.start_date.clone().map_or(Value::Null, Value::String),
        );
        value.insert(
            "excluded",
            Value::Array(
                self.excluded
                    .iter()
                    .map(|(season, episode)| {
                        Value::Array(vec![(*season).into(), (*episode).into()])
                    })
                    .collect(),
            ),
        );
        for (name, number) in [
            ("revision", self.revision),
            ("created_at", self.created_at),
            ("updated_at", self.updated_at),
            ("checked_at", self.checked_at),
            ("next_check_at", self.next_check_at),
        ] {
            value.insert(name, number.to_string());
        }
        value.insert(
            "last_error",
            self.last_error.clone().map_or(Value::Null, Value::String),
        );
        value
    }
    pub fn public_json(&self) -> Value {
        let mut value = self.to_json();
        if let Some(Value::Object(request)) = value.get_mut("request") {
            request.insert(
                "title".into(),
                report_text(&self.plan.request.title, 4096).into(),
            );
        }
        if let Some(Value::Array(episodes)) = value.get_mut("episodes") {
            for (episode, known) in episodes.iter_mut().zip(&self.plan.episodes) {
                episode.insert("monitored", self.episode_monitored(known));
                episode.insert(
                    "excluded",
                    self.excluded.contains(&(known.season, known.episode)),
                );
                if let Some(Value::String(title)) = episode.get_mut("title") {
                    *title = report_text(title, 2048);
                }
            }
        }
        value
    }
    pub fn summary_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("id", self.id.clone());
        let mut request = self.plan.request.to_json();
        request.insert("title", report_text(&self.plan.request.title, 4096));
        value.insert("request", request);
        value.insert("monitored", self.monitored);
        value.insert("include_specials", self.include_specials);
        value.insert(
            "start_date",
            self.start_date.clone().map_or(Value::Null, Value::String),
        );
        value.insert("episode_count", self.plan.episodes.len() as u32);
        value.insert(
            "undated_count",
            self.plan
                .episodes
                .iter()
                .filter(|episode| episode.air_date.is_none())
                .count() as u32,
        );
        value.insert(
            "unmapped_count",
            self.plan
                .episodes
                .iter()
                .filter(|episode| episode.catalog_id.is_none())
                .count() as u32,
        );
        value.insert("checked_at", self.checked_at.to_string());
        value.insert("next_check_at", self.next_check_at.to_string());
        value.insert(
            "last_error",
            self.last_error
                .as_ref()
                .map_or(Value::Null, |text| report_text(text, 2048).into()),
        );
        value
    }
    fn from_json(value: &Value) -> Result<Self> {
        fields(
            value,
            &[
                "id",
                "request",
                "episodes",
                "monitored",
                "include_specials",
                "start_date",
                "excluded",
                "revision",
                "created_at",
                "updated_at",
                "checked_at",
                "next_check_at",
                "last_error",
                "anchors",
                "numbering",
            ],
        )?;
        let episodes = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("Missing series episodes")?;
        if episodes.len() > MAX_EPISODES {
            return Err("Too many series episodes".into());
        }
        let exclusions = value
            .get("excluded")
            .and_then(Value::as_array)
            .ok_or("Missing episode exclusions")?;
        if exclusions.len() > MAX_EPISODES {
            return Err("Too many episode exclusions".into());
        }
        let mut excluded = BTreeSet::new();
        for entry in exclusions {
            let numbers = entry.as_array().ok_or("Invalid episode exclusion")?;
            if numbers.len() != 2 {
                return Err("Invalid episode exclusion".into());
            }
            let season = numbers[0]
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or("Invalid excluded season")?;
            let episode = numbers[1]
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or("Invalid excluded episode")?;
            if !excluded.insert((season, episode)) {
                return Err("Duplicate episode exclusion".into());
            }
        }
        let optional = |key| -> Result<Option<String>> {
            match value.get(key) {
                Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.clone())),
                _ => Err("Invalid optional series field".into()),
            }
        };
        let plan = Plan {
            request: Request::from_json(value.get("request").ok_or("Missing series identity")?)?,
            episodes: episodes
                .iter()
                .map(Episode::from_json)
                .collect::<Result<_>>()?,
        };
        let mut anchors = numbering::initial_anchors(&plan);
        if let Some(entries) = value.get("anchors") {
            let entries = entries.as_array().ok_or("Invalid numbering anchors")?;
            if entries.len() > MAX_EPISODES {
                return Err("Too many numbering anchors".into());
            }
            anchors.clear();
            for entry in entries {
                fields(entry, &["catalog_id", "season", "episode"])?;
                let id = numbering::catalog_id(entry)?;
                let number = crate::numbering::EpisodeNumber {
                    season: small(entry, "season")?,
                    episode: small(entry, "episode")?,
                };
                number.validate()?;
                if anchors.insert(id, number).is_some() {
                    return Err("Duplicate numbering anchor".into());
                }
            }
        }
        let mut numbering = BTreeMap::new();
        if let Some(entries) = value.get("numbering") {
            let entries = entries.as_array().ok_or("Invalid numbering choices")?;
            if entries.len() > MAX_EPISODES {
                return Err("Too many numbering choices".into());
            }
            for entry in entries {
                let choice = NumberingChoice::from_json(entry)?;
                if numbering.insert(choice.catalog_id, choice).is_some() {
                    return Err("Duplicate numbering choice".into());
                }
            }
        }
        let record = Self {
            id: text(value, "id")?.into(),
            plan,
            anchors,
            numbering,
            monitored: flag(value, "monitored")?,
            include_specials: flag(value, "include_specials")?,
            start_date: optional("start_date")?,
            excluded,
            revision: integer(value, "revision")?,
            created_at: integer(value, "created_at")?,
            updated_at: integer(value, "updated_at")?,
            checked_at: integer(value, "checked_at")?,
            next_check_at: integer(value, "next_check_at")?,
            last_error: optional("last_error")?,
        };
        record.validate()?;
        Ok(record)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let map = value.as_object().ok_or("Expected a series object")?;
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unexpected series field".into());
    }
    Ok(())
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing series field: {key}"))
}
fn integer(value: &Value, key: &str) -> Result<u64> {
    let text = text(value, key)?;
    if text.is_empty()
        || text.len() > 20
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(format!("Invalid series integer: {key}"));
    }
    text.parse()
        .map_err(|_| format!("Invalid series integer: {key}"))
}
fn small(value: &Value, key: &str) -> Result<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("Invalid series number: {key}"))
}
fn flag(value: &Value, key: &str) -> Result<bool> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("Missing series flag: {key}"))
}

#[cfg(test)]
mod tests;
