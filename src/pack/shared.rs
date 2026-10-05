//! Explicit shared ownership is authenticated before one atomic acquisition commit.
use super::{MAX_PACK_EPISODES, validate_file_path};
use crate::{
    Result,
    crypto::sha256,
    date,
    engine::{Engine, lock, public_job},
    json::{self, Value},
    organizer,
    store::{Job, Request},
    torrent::inspect_metadata,
};
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

fn only(value: &Value, allowed: &[&str]) -> Result<()> {
    if value
        .as_object()
        .is_none_or(|fields| fields.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err("Unknown shared-file field or invalid object".into());
    }
    Ok(())
}
fn string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Shared-file {key} must be a string"))
}
fn number(value: &Value, key: &str) -> Result<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("Invalid shared-file {key}"))
}
fn valid_hash(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Every owner captures the same immutable physical identity and library destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedFile {
    pub torrent_id: String,
    pub file_path: String,
    pub tmdb_id: u64,
    pub title: String,
    pub year: u32,
    pub season: u32,
    pub first_episode: u32,
    pub last_episode: u32,
    pub import_path: String,
}
impl SharedFile {
    pub fn id(&self) -> String {
        let mut value = Value::object();
        value.insert("torrent_id", self.torrent_id.clone());
        value.insert("file_path", self.file_path.clone());
        value.insert("tmdb_id", self.tmdb_id);
        value.insert("season", self.season);
        value.insert("first_episode", self.first_episode);
        value.insert("last_episode", self.last_episode);
        sha256(json::stringify(&value).as_bytes())[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    pub fn validate(&self) -> Result<()> {
        validate_file_path(&self.file_path)?;
        if !valid_hash(&self.torrent_id, &[40, 64])
            || self.tmdb_id == 0
            || self.tmdb_id > 9_007_199_254_740_991
            || self.season > 9999
            || self.first_episode == 0
            || self.last_episode > 99999
            || self.last_episode <= self.first_episode
            || self.last_episode - self.first_episode >= MAX_PACK_EPISODES as u32
            || self.import_path.is_empty()
            || self.import_path.len() > 4096
            || self.import_path.chars().any(char::is_control)
            || self.import_path.contains('\\')
            || Path::new(&self.import_path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("Invalid shared-file identity, owner range or import path".into());
        }
        self.owner_request(self.first_episode, "[validated source]")
            .validate()?;
        let filename = organizer::shared_target(Path::new(""), self)?;
        if Path::new(&self.import_path).file_name() != filename.file_name() {
            return Err("Shared import filename differs from its physical ownership".into());
        }
        Ok(())
    }
    pub(crate) fn owner_request(&self, episode: u32, source: &str) -> Request {
        Request {
            kind: "episode".into(),
            title: self.title.clone(),
            year: self.year,
            season: self.season,
            episode,
            source_path: None,
            source_url: Some(source.into()),
            tmdb_id: Some(self.tmdb_id),
            source_numbering: None,
        }
    }
    pub(crate) fn validate_job(&self, job: &Job) -> Result<()> {
        self.validate()?;
        if job.request.kind != "episode"
            || job.request.tmdb_id != Some(self.tmdb_id)
            || job.request.title != self.title
            || job.request.year != self.year
            || job.request.season != self.season
            || !(self.first_episode..=self.last_episode).contains(&job.request.episode)
            || job.request.source_url.is_none()
            || job.request.source_path.is_some()
            || job.pack_file.as_deref() != Some(self.file_path.as_str())
            || job.pack_origin.is_some()
            || job.upgrade_parent.is_some()
            || job
                .download_id
                .as_ref()
                .is_some_and(|id| id != &self.torrent_id)
            || job
                .acquisition_url
                .as_ref()
                .is_some_and(|url| Some(url) != job.request.source_url.as_ref())
            || job.imports.len() > 1
            || job
                .imports
                .first()
                .is_some_and(|path| path != &self.import_path)
            || (job.state == "ready" && job.imports.is_empty())
        {
            return Err("Shared-file owner or import provenance differs from its binding".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        for (key, text) in [
            ("torrent_id", &self.torrent_id),
            ("file_path", &self.file_path),
            ("title", &self.title),
            ("import_path", &self.import_path),
        ] {
            value.insert(key, text.clone());
        }
        value.insert("tmdb_id", self.tmdb_id);
        value.insert("year", self.year);
        value.insert("season", self.season);
        value.insert("first_episode", self.first_episode);
        value.insert("last_episode", self.last_episode);
        value
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        only(
            value,
            &[
                "torrent_id",
                "file_path",
                "tmdb_id",
                "title",
                "year",
                "season",
                "first_episode",
                "last_episode",
                "import_path",
            ],
        )?;
        let result = Self {
            torrent_id: string(value, "torrent_id")?,
            file_path: string(value, "file_path")?,
            tmdb_id: value
                .get("tmdb_id")
                .and_then(Value::as_u64)
                .ok_or("Missing shared-file TMDB identity")?,
            title: string(value, "title")?,
            year: number(value, "year")?,
            season: number(value, "season")?,
            first_episode: number(value, "first_episode")?,
            last_episode: number(value, "last_episode")?,
            import_path: string(value, "import_path")?,
        };
        result.validate()?;
        Ok(result)
    }
}

/// One file per action. A new group requires all consecutive owners; an existing
/// group permits an explicitly requested subset without changing its full binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedFileRequest {
    pub source_url: String,
    pub file_path: String,
    pub season: u32,
    pub episodes: Vec<u32>,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl SharedFileRequest {
    pub fn from_json(value: &Value) -> Result<Self> {
        only(
            value,
            &[
                "source_url",
                "file_path",
                "season",
                "episodes",
                "apply",
                "plan_id",
            ],
        )?;
        let episodes = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("Shared-file episodes must be an array")?;
        if episodes.is_empty() || episodes.len() > MAX_PACK_EPISODES {
            return Err("Shared-file requests require 1 to 64 episode numbers".into());
        }
        let result = Self {
            source_url: string(value, "source_url")?,
            file_path: string(value, "file_path")?,
            season: number(value, "season")?,
            episodes: episodes
                .iter()
                .map(|e| {
                    e.as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| "Invalid shared-file episode".into())
                })
                .collect::<Result<Vec<_>>>()?,
            apply: match value.get("apply") {
                None => false,
                Some(Value::Bool(b)) => *b,
                _ => return Err("Shared-file apply must be boolean".into()),
            },
            plan_id: match value.get("plan_id") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return Err("Shared-file plan_id must be a string".into()),
            },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        validate_file_path(&self.file_path)?;
        let unique: BTreeSet<_> = self.episodes.iter().copied().collect();
        if self.source_url.trim().is_empty()
            || self.source_url.len() > 65536
            || self.source_url.chars().any(char::is_control)
            || self.season > 9999
            || self.episodes.is_empty()
            || self.episodes.len() > MAX_PACK_EPISODES
            || unique.len() != self.episodes.len()
            || unique.first().is_none_or(|n| *n == 0)
            || unique.last().is_none_or(|n| *n > 99999)
            || self.apply != self.plan_id.is_some()
            || self
                .plan_id
                .as_ref()
                .is_some_and(|id| !valid_hash(id, &[64]))
        {
            return Err("Invalid shared-file source, owners or preview guard".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("source_url", self.source_url.clone());
        value.insert("file_path", self.file_path.clone());
        value.insert("season", self.season);
        value.insert(
            "episodes",
            Value::Array(self.episodes.iter().map(|n| Value::from(*n)).collect()),
        );
        value.insert("apply", self.apply);
        if let Some(id) = &self.plan_id {
            value.insert("plan_id", id.clone());
        }
        value
    }
}

impl Engine {
    pub fn shared_file(&self, id: &str, query: &SharedFileRequest) -> Result<Value> {
        self.shared_file_before(id, query, Instant::now() + Duration::from_secs(60))
    }
    pub fn shared_file_before(
        &self,
        id: &str,
        query: &SharedFileRequest,
        deadline: Instant,
    ) -> Result<Value> {
        query.validate()?;
        if query.apply && self.read_only {
            return Err("Shared-file acquisition requires writable storage".into());
        }
        let initial = lock(&self.series_store)?
            .get(id)
            .ok_or("Unknown tracked series")?;
        let today = date::today();
        for number in &query.episodes {
            let episode = initial
                .plan
                .episodes
                .iter()
                .find(|e| e.season == query.season && e.episode == *number)
                .ok_or("Shared owner is absent from the accepted catalog plan")?;
            if episode.catalog_id.is_none()
                || episode
                    .air_date
                    .as_deref()
                    .is_none_or(|day| day > today.as_str())
            {
                return Err(
                    "Shared owners require known catalog identities and already aired dates".into(),
                );
            }
        }
        // Metadata-only I/O precedes both storage locks. No transfer or payload is scheduled.
        let metadata = inspect_metadata(&query.source_url, deadline)?;
        if metadata
            .files
            .iter()
            .filter(|file| file.path == query.file_path && !file.padding && file.length > 0)
            .count()
            != 1
        {
            return Err(
                "Shared video path is absent, empty or ambiguous in authenticated metadata".into(),
            );
        }
        let series = lock(&self.series_store)?;
        if query.apply {
            series.check_writable()?;
        }
        let record = series.get(id).ok_or("Unknown tracked series")?;
        if record != initial {
            return Err(
                "Shared-file catalog changed during metadata inspection; preview again".into(),
            );
        }
        let mut store = lock(&self.store)?;
        let all = store.list();
        let mut episodes = query.episodes.clone();
        episodes.sort_unstable();
        let known = all
            .iter()
            .filter_map(|job| job.shared_file.as_ref())
            .find(|file| file.torrent_id == metadata.id && file.file_path == query.file_path);
        let file = if let Some(known) = known {
            if record.plan.request.tmdb_id != Some(known.tmdb_id)
                || query.season != known.season
                || episodes
                    .iter()
                    .any(|n| !(known.first_episode..=known.last_episode).contains(n))
            {
                return Err("Physical video already has different canonical owners".into());
            }
            known.clone()
        } else {
            if episodes.len() < 2 || episodes.windows(2).any(|p| p[1] != p[0] + 1) {
                return Err(
                    "New shared ownership requires 2 to 64 consecutive episodes in one season"
                        .into(),
                );
            }
            let mut file = SharedFile {
                torrent_id: metadata.id.clone(),
                file_path: query.file_path.clone(),
                tmdb_id: record
                    .plan
                    .request
                    .tmdb_id
                    .ok_or("Shared ownership requires a known TMDB series identity")?,
                title: record.plan.request.title.clone(),
                year: record.plan.request.year,
                season: query.season,
                first_episode: episodes[0],
                last_episode: *episodes.last().ok_or("Missing shared owners")?,
                import_path: String::new(),
            };
            file.import_path = organizer::shared_target(&self.config.series_root, &file)?
                .to_str()
                .ok_or("Shared import path is not UTF-8")?
                .into();
            file.validate()?;
            file
        };
        let mut requests = Vec::new();
        for number in &episodes {
            let episode = record
                .plan
                .episodes
                .iter()
                .find(|e| e.season == query.season && e.episode == *number)
                .ok_or("Shared owner is absent from the accepted catalog plan")?;
            if episode.catalog_id.is_none()
                || episode
                    .air_date
                    .as_deref()
                    .is_none_or(|day| day > today.as_str())
            {
                return Err(
                    "Shared owners require known catalog identities and already aired dates".into(),
                );
            }
            let mut request = record.episode_request(episode);
            request.source_url = Some(query.source_url.clone());
            request.validate()?;
            requests.push(request);
        }
        let existing = store.check_shared_submission(&file, &requests)?;
        let mut fence = Value::object();
        fence.insert("series", record.to_json());
        fence.insert("today", today);
        fence.insert("binding", file.to_json());
        fence.insert("source_url", query.source_url.clone());
        fence.insert(
            "requests",
            Value::Array(requests.iter().map(Request::to_json).collect()),
        );
        fence.insert(
            "owners",
            Value::Array(
                all.iter()
                    .filter(|job| job.shared_file.as_ref() == Some(&file))
                    .map(Job::to_json)
                    .collect(),
            ),
        );
        let plan_id: String = sha256(json::stringify(&fence).as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if query
            .plan_id
            .as_ref()
            .is_some_and(|guard| guard != &plan_id)
        {
            return Err("Shared-file preview changed; preview again before acquisition".into());
        }
        let new_count = if existing.is_empty() {
            requests.len()
        } else {
            0
        };
        let jobs = if query.apply {
            store.submit_shared_file(file.clone(), requests)?
        } else {
            existing
        };
        let mut report = Value::object();
        report.insert("series_id", id.to_owned());
        report.insert("plan_id", plan_id);
        report.insert("apply", query.apply);
        report.insert("binding", file.to_json());
        report.insert("group_id", file.id());
        report.insert("new_owners", new_count as u32);
        report.insert("submitted", if query.apply { new_count as u32 } else { 0 });
        report.insert(
            "reused",
            jobs.len() as u32 - if query.apply { new_count as u32 } else { 0 },
        );
        report.insert("jobs", Value::Array(jobs.iter().map(public_job).collect()));
        // Sources can carry credentials; management reports never echo them.
        Ok(report)
    }
}
