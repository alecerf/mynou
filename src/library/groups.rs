//! Reviewed whole-group baselines and one-file to one-file replacements.
use crate::{
    Result,
    crypto::sha256,
    engine::{Engine, lock, public_job},
    json::{self, Value},
    media,
    numbering::{self, EpisodeNumber, SourceNumber},
    organizer,
    pack::SharedFile,
    selection::tokens,
    store::{Job, RecordedRelease, Request, reject_symlinks},
    torrent::inspect_metadata,
};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRequest {
    pub action: String,
    pub release_title: String,
    pub source_url: Option<String>,
    pub file_path: Option<String>,
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl GroupRequest {
    pub fn from_json(value: &Value) -> Result<Self> {
        numbering::only(
            value,
            &[
                "action",
                "release_title",
                "source_url",
                "file_path",
                "apply",
                "plan_id",
            ],
        )?;
        let text = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("Missing or invalid group {key}"))
        };
        let optional = |key| match value.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            _ => Err(format!("Invalid group {key}")),
        };
        let result = Self {
            action: text("action")?,
            release_title: text("release_title")?,
            source_url: optional("source_url")?,
            file_path: optional("file_path")?,
            apply: match value.get("apply") {
                None => false,
                Some(value) => value.as_bool().ok_or("Group apply must be a boolean")?,
            },
            plan_id: optional("plan_id")?,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        if !["baseline", "replace"].contains(&self.action.as_str())
            || self.release_title.is_empty()
            || self.release_title.len() > 2048
            || self.release_title.chars().any(char::is_control)
            || self.apply != self.plan_id.is_some()
            || self.plan_id.as_ref().is_some_and(|id| {
                id.len() != 64
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err("Invalid shared-group action, release title or preview guard".into());
        }
        if self.action == "baseline" {
            if self.source_url.is_some() || self.file_path.is_some() {
                return Err("A group baseline does not acquire a source".into());
            }
        } else {
            let source = self
                .source_url
                .as_ref()
                .ok_or("Replacement requires a source URL")?;
            if source.is_empty() || source.len() > 65536 || source.chars().any(char::is_control) {
                return Err("Invalid shared replacement source".into());
            }
            crate::pack::validate_file_path(
                self.file_path
                    .as_deref()
                    .ok_or("Replacement requires an exact video path")?,
            )?;
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("action", self.action.clone());
        value.insert("release_title", self.release_title.clone());
        if let Some(source) = &self.source_url {
            value.insert("source_url", source.clone());
        }
        if let Some(path) = &self.file_path {
            value.insert("file_path", path.clone());
        }
        value.insert("apply", self.apply);
        if let Some(id) = &self.plan_id {
            value.insert("plan_id", id.clone());
        }
        value
    }
}

fn source_number(job: &Job) -> SourceNumber {
    job.request
        .source_numbering
        .unwrap_or(SourceNumber::SeasonEpisode(EpisodeNumber {
            season: job.request.season,
            episode: job.request.episode,
        }))
}

/// A season release or exact captured source range can describe the physical file.
/// This is an explicit mapping; it does not infer ownership from a release title.
pub(super) fn group_title_matches(jobs: &[Job], title: &str) -> bool {
    let Some(first) = jobs.first() else {
        return false;
    };
    let Some(last) = jobs.last() else {
        return false;
    };
    let start = source_number(first);
    let end = source_number(last);
    let consecutive =
        jobs.iter()
            .enumerate()
            .all(|(offset, job)| match (start, source_number(job)) {
                (SourceNumber::SeasonEpisode(a), SourceNumber::SeasonEpisode(b)) => {
                    a.season == b.season && b.episode == a.episode + offset as u32
                }
                (SourceNumber::Absolute(a), SourceNumber::Absolute(b)) => b == a + offset as u32,
                _ => false,
            });
    if !consecutive {
        return false;
    }
    if let SourceNumber::SeasonEpisode(number) = start {
        let mut request = first.request.clone();
        request.season = number.season;
        if crate::pack::season_title_matches(&request, title) {
            return true;
        }
    }
    let expected = tokens(&first.request.title);
    let actual = tokens(title);
    if expected.is_empty() || !actual.starts_with(&expected) {
        return false;
    }
    let mut rest = &actual[expected.len()..];
    if rest
        .first()
        .and_then(|value| value.parse::<u32>().ok())
        .is_some_and(|year| (1888..=2200).contains(&year))
    {
        if first.request.year != 0 && rest[0].parse::<u32>().ok() != Some(first.request.year) {
            return false;
        }
        rest = &rest[1..];
    }
    let split_range = rest.len() >= 2
        && start.matches(&rest[0])
        && match end {
            SourceNumber::SeasonEpisode(number) => {
                rest[1].strip_prefix('e').and_then(numbering::decimal_token) == Some(number.episode)
                    || end.matches(&rest[1])
            }
            SourceNumber::Absolute(_) => end.matches(&rest[1]),
        };
    let chained_range = match (start, end, rest.first()) {
        (SourceNumber::SeasonEpisode(a), SourceNumber::SeasonEpisode(b), Some(label)) => {
            let parts: Vec<_> = label.split('e').collect();
            parts.len() == 3
                && parts[0]
                    .strip_prefix('s')
                    .and_then(numbering::decimal_token)
                    == Some(a.season)
                && numbering::decimal_token(parts[1]) == Some(a.episode)
                && numbering::decimal_token(parts[2]) == Some(b.episode)
        }
        _ => false,
    };
    let consumed = if split_range {
        2
    } else if chained_range {
        1
    } else {
        return false;
    };
    !rest[consumed..].iter().any(|label| {
        numbering::conflicting_marker(start, label, first.request.year)
            || label
                .strip_prefix('s')
                .and_then(numbering::decimal_token)
                .is_some()
            || label == "season"
    })
}

fn verify_import(jobs: &[Job]) -> Result<Value> {
    let file = jobs
        .first()
        .and_then(|job| job.shared_file.as_ref())
        .ok_or("Missing shared import binding")?;
    let path = Path::new(&file.import_path);
    reject_symlinks(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| "Shared import is missing")?;
    if !metadata.is_file()
        || jobs
            .iter()
            .any(|job| job.imports != [file.import_path.clone()])
    {
        return Err("Every shared owner must retain its exact regular-file import".into());
    }
    if media::analyze(path)?.video_streams.is_empty() {
        return Err("Shared import has no declared video stream".into());
    }
    let mut value = Value::object();
    value.insert("bytes", metadata.len().to_string());
    value.insert(
        "modified",
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(Value::Null, |duration| {
                duration.as_nanos().to_string().into()
            }),
    );
    Ok(value)
}

impl Engine {
    pub fn library_group(&self, id: &str, query: &GroupRequest) -> Result<Value> {
        self.library_group_before(id, query, Instant::now() + Duration::from_secs(60))
    }
    pub fn library_group_before(
        &self,
        id: &str,
        query: &GroupRequest,
        deadline: Instant,
    ) -> Result<Value> {
        query.validate()?;
        if query.apply && self.read_only {
            return Err("Group application requires a running writable service".into());
        }
        let initial = lock(&self.store)?.current_shared_group(id)?;
        if query.action == "baseline" {
            if initial.iter().any(|job| job.release.is_some()) {
                return Err("Shared-group baseline is already recorded".into());
            }
        } else if initial
            .iter()
            .any(|job| !job.monitored || job.release.is_none())
        {
            return Err(
                "Replacement requires every owner to be monitored and a whole-group baseline"
                    .into(),
            );
        }
        if !group_title_matches(&initial, &query.release_title) {
            return Err(
                "Release title does not match the complete captured source range or season".into(),
            );
        }
        let import_stamp = verify_import(&initial)?;
        let config = self.configuration_for(&initial[0]);
        if initial.iter().any(|j| {
            j.requester.as_ref().map(|p| &p.capture)
                != initial[0].requester.as_ref().map(|p| &p.capture)
        }) {
            return Err("Shared-group requester policies must agree".into());
        }
        let (profile_name, profile) = config.selection.profile("episode")?;
        let release = RecordedRelease {
            title: query.release_title.clone(),
            profile: profile_name.into(),
        };
        let candidate = profile.assess(&release.title, &initial[0].request.title);
        let current = initial[0]
            .release
            .as_ref()
            .map(|old| profile.assess(&old.title, &initial[0].request.title));
        if query.action == "replace"
            && (!candidate.accepted
                || current.as_ref().is_some_and(|old| {
                    profile.cutoff_reached(old) || old.accepted && candidate.rank <= old.rank
                }))
        {
            return Err("Shared replacement must be an accepted quality improvement below the current cutoff".into());
        }
        let old = initial[0]
            .shared_file
            .as_ref()
            .ok_or("Missing shared-group binding")?;
        let binding = if query.action == "replace" {
            let source = query
                .source_url
                .as_deref()
                .ok_or("Missing replacement source")?;
            if initial.iter().any(|job| {
                job.request.source_url.as_deref() == Some(source)
                    || job.acquisition_url.as_deref() == Some(source)
            }) {
                return Err("Shared replacement must use a different acquisition source".into());
            }
            // Authenticated metadata only; do not queue transfers or fetch payload.
            let metadata = inspect_metadata(source, deadline)?;
            let path = query
                .file_path
                .as_deref()
                .ok_or("Missing replacement video path")?;
            if metadata.id == old.torrent_id
                || metadata
                    .files
                    .iter()
                    .filter(|file| file.path == path && file.length > 0 && !file.padding)
                    .count()
                    != 1
            {
                return Err(
                    "Replacement requires a new authenticated torrent and one exact nonempty video"
                        .into(),
                );
            }
            let mut file = SharedFile {
                torrent_id: metadata.id,
                file_path: path.into(),
                tmdb_id: old.tmdb_id,
                title: old.title.clone(),
                year: old.year,
                season: old.season,
                first_episode: old.first_episode,
                last_episode: old.last_episode,
                import_path: String::new(),
            };
            file.import_path = organizer::shared_target(&config.series_root, &file)?
                .to_str()
                .ok_or("Shared destination is not UTF-8")?
                .into();
            file.validate()?;
            Some(file)
        } else {
            None
        };
        if Instant::now() >= deadline {
            return Err("Shared-group action deadline exceeded; preview again".into());
        }
        let mut requests: Vec<Request> = initial.iter().map(|job| job.request.clone()).collect();
        if let Some(source) = &query.source_url {
            for request in &mut requests {
                request.source_url = Some(source.clone());
            }
        }
        let mut store = lock(&self.store)?;
        if store.current_shared_group(id)? != initial {
            return Err("Shared-group owners changed during review; preview again".into());
        }
        let known = if let Some(file) = &binding {
            store.check_group_replacement(&initial, file, &requests, &release)?
        } else {
            Vec::new()
        };
        let mut fence = Value::object();
        let mut proposal = query.clone();
        proposal.apply = false;
        proposal.plan_id = None;
        fence.insert("query", proposal.to_json());
        fence.insert(
            "parents",
            Value::Array(initial.iter().map(Job::to_json).collect()),
        );
        fence.insert("import", import_stamp);
        fence.insert("release", release.to_json());
        fence.insert("profile", profile.to_json());
        fence.insert(
            "replacement",
            binding.as_ref().map_or(Value::Null, SharedFile::to_json),
        );
        fence.insert(
            "existing",
            Value::Array(known.iter().map(Job::to_json).collect()),
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
            return Err("Shared-group preview changed; preview again before applying".into());
        }
        if Instant::now() >= deadline {
            return Err("Shared-group commit deadline exceeded; preview again".into());
        }
        let created = binding.is_some() && known.is_empty();
        let jobs = if query.apply {
            if let Some(file) = &binding {
                store.submit_group_replacement(&initial, file.clone(), requests, release.clone())?
            } else {
                store.set_group_baseline(id, release.clone())?
            }
        } else {
            known
        };
        let mut report = Value::object();
        report.insert("action", query.action.clone());
        report.insert("apply", query.apply);
        report.insert("plan_id", plan_id);
        report.insert("parent_group_id", old.id());
        report.insert("owners", initial.len() as u32);
        let mut public_release = release.to_json();
        public_release.insert(
            "title",
            crate::integrations::report_text(&release.title, 2048),
        );
        report.insert("release", public_release);
        report.insert("candidate_assessment", candidate.to_json());
        report.insert(
            "current_assessment",
            current.map_or(Value::Null, |assessment| assessment.to_json()),
        );
        report.insert("binding", binding.as_ref().unwrap_or(old).to_json());
        report.insert(
            "submitted",
            if query.apply && created {
                initial.len() as u32
            } else {
                0
            },
        );
        report.insert(
            "reused",
            if query.action == "replace" && !created {
                jobs.len() as u32
            } else {
                0
            },
        );
        report.insert(
            "parents",
            Value::Array(initial.iter().map(public_job).collect()),
        );
        report.insert("jobs", Value::Array(jobs.iter().map(public_job).collect()));
        Ok(report)
    }
}
