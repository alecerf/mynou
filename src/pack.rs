//! Explicit, bounded file-to-episode mappings for one native torrent acquisition.
use crate::{
    Result, date,
    engine::{Engine, lock, public_job},
    json::Value,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
mod automatic;
pub(crate) use automatic::season_title_matches;
pub use automatic::{AutoPackRequest, PackOrigin};

pub const MAX_PACK_EPISODES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackEpisode {
    pub season: u32,
    pub episode: u32,
    pub file_path: String,
}

#[derive(Clone, Debug)]
pub struct PackSubmission {
    pub source_url: String,
    pub episodes: Vec<PackEpisode>,
}

pub fn validate_file_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || path.contains(['\\', ':'])
        || path.split('/').count() > 32
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.len() > 255)
        || !Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ["mp4", "m4v", "mov", "mkv", "webm", "avi"]
                    .contains(&ext.to_ascii_lowercase().as_str())
            })
    {
        return Err("Pack mapping requires a relative video path with normal components".into());
    }
    Ok(())
}

impl PackSubmission {
    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = value
            .as_object()
            .ok_or("Pack submission must be an object")?;
        if fields
            .keys()
            .any(|key| !["source_url", "episodes"].contains(&key.as_str()))
        {
            return Err("Unknown pack submission field".into());
        }
        let entries = value
            .get("episodes")
            .and_then(Value::as_array)
            .ok_or("Pack episodes must be an array")?;
        if entries.is_empty() || entries.len() > MAX_PACK_EPISODES {
            return Err("A pack submission requires 1 to 64 episode mappings".into());
        }
        let mut episodes = Vec::with_capacity(entries.len());
        for entry in entries {
            let fields = entry.as_object().ok_or("Pack episode must be an object")?;
            if fields
                .keys()
                .any(|key| !["season", "episode", "file_path"].contains(&key.as_str()))
            {
                return Err("Unknown pack episode field".into());
            }
            let number = |key| {
                entry
                    .get(key)
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| format!("Invalid pack {key}"))
            };
            episodes.push(PackEpisode {
                season: number("season")?,
                episode: number("episode")?,
                file_path: entry
                    .get("file_path")
                    .and_then(Value::as_str)
                    .ok_or("Pack file_path must be a string")?
                    .into(),
            });
        }
        let pack = Self {
            source_url: value
                .get("source_url")
                .and_then(Value::as_str)
                .ok_or("Pack source_url must be a string")?
                .into(),
            episodes,
        };
        pack.validate()?;
        Ok(pack)
    }
    pub fn validate(&self) -> Result<()> {
        if self.source_url.trim().is_empty()
            || self.source_url.len() > 65_536
            || self.source_url.chars().any(char::is_control)
            || self.episodes.is_empty()
            || self.episodes.len() > MAX_PACK_EPISODES
        {
            return Err("Invalid pack source or mapping count".into());
        }
        let mut numbers = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for episode in &self.episodes {
            if episode.season > 9999
                || episode.episode == 0
                || episode.episode > 99999
                || !numbers.insert((episode.season, episode.episode))
                || !paths.insert(&episode.file_path)
            {
                return Err("Pack mappings require unique episode numbers and unique files".into());
            }
            validate_file_path(&episode.file_path)?;
        }
        Ok(())
    }
}

impl Engine {
    pub fn remap_pack(&self, id: &str, path: String) -> Result<Value> {
        if self.read_only {
            return Err("Pack mapping correction requires writable storage".into());
        }
        Ok(public_job(&lock(&self.store)?.remap_pack(id, path)?))
    }
    pub fn submit_pack(&self, id: &str, pack: &PackSubmission) -> Result<Value> {
        self.submit_pack_with_origin(id, pack, None)
    }
    pub(super) fn submit_pack_with_origin(
        &self,
        id: &str,
        pack: &PackSubmission,
        origin: Option<&PackOrigin>,
    ) -> Result<Value> {
        if self.read_only {
            return Err("Pack submission requires writable storage".into());
        }
        pack.validate()?;
        // Entire input, catalog scope and source-key collisions are checked before the first commit.
        // Series then requests matches the monitoring coordinator's lock order; no network I/O.
        let series = lock(&self.series_store)?;
        series.check_writable()?;
        let record = series.get(id).ok_or("Unknown tracked series")?;
        let mut requests = Vec::with_capacity(pack.episodes.len());
        let today = date::today();
        for mapping in &pack.episodes {
            let episode = record
                .plan
                .episodes
                .iter()
                .find(|episode| {
                    episode.season == mapping.season && episode.episode == mapping.episode
                })
                .ok_or("Mapped episode is absent from the accepted catalog plan")?;
            if episode.catalog_id.is_none()
                || episode
                    .air_date
                    .as_deref()
                    .is_none_or(|day| day > today.as_str())
            {
                return Err(
                    "Pack acquisition requires known identities and already aired episode dates"
                        .into(),
                );
            }
            let mut request = record.episode_request(episode);
            request.source_url = Some(pack.source_url.clone());
            request.validate()?;
            requests.push((request, mapping.file_path.clone()));
        }
        let mut store = lock(&self.store)?;
        if let Some(origin) = origin {
            origin.validate()?;
            if origin.series_id != id
                || origin.series_revision != record.revision
                || automatic::capture_scope(&record, &store, origin.season)?.id != origin.scope_id
            {
                return Err("Pack catalog or request scope changed; acquisition discarded".into());
            }
        }
        let existing = store.list();
        let mut by_media = BTreeMap::new();
        let mut by_key = BTreeMap::new();
        for job in &existing {
            by_key.insert(job.key.clone(), job);
            let key = job.request.media_key();
            if by_media
                .get(&key)
                .is_none_or(|known: &&crate::store::Job| known.state != "ready")
            {
                by_media.insert(key, job);
            }
        }
        for (request, path) in &requests {
            let media_key = request.media_key();
            if by_key
                .get(&request.canonical_key())
                .is_some_and(|job| job.request.media_key() != media_key)
                || by_media.get(&media_key).is_some_and(|job| {
                    job.request.source_url == request.source_url
                        && job.pack_file.as_ref().is_some_and(|known| known != path)
                })
            {
                return Err("Pack source or mapping conflicts with an existing request".into());
            }
        }
        let new_count = requests
            .iter()
            .filter(|(request, _)| !by_media.contains_key(&request.media_key()))
            .count();
        store.check_submission_capacity(new_count)?;
        let mut submitted = 0_u32;
        let mut jobs = Vec::with_capacity(requests.len());
        for (request, path) in requests {
            let job = if let Some(job) = by_media.get(&request.media_key()) {
                (*job).clone()
            } else {
                let job = if let Some(origin) = origin {
                    store.submit_auto_pack(request, path, origin.clone())?
                } else {
                    store.submit_pack(request, path)?
                };
                submitted += 1;
                job
            };
            jobs.push(public_job(&job));
        }
        let mut value = Value::object();
        value.insert("series_id", id.to_owned());
        value.insert("submitted", submitted);
        value.insert("reused", jobs.len() as u32 - submitted);
        value.insert("jobs", Value::Array(jobs));
        Ok(value)
    }
}
