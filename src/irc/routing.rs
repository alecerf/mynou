//! Hash-pinned candidates attach only to existing, admitted canonical work.
use super::{Record, State, hash, only, text, valid_id};
use crate::{
    Result,
    config::Config,
    engine::{Engine, lock},
    json::Value,
    numbering::{EpisodeNumber, SourceNumber},
    selection::tokens,
    store::{self, Job, RecordedRelease, Store},
    torrent::inspection::TorrentMetadata,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Immutable acquisition provenance; its private magnet is never a public report.
#[derive(Clone, Debug, PartialEq)]
pub struct Origin {
    pub announcement_id: String,
    pub job_id: String,
    pub source_id: String,
    pub binding: String,
    pub rule_id: String,
    pub fingerprint: String,
    pub media_key: String,
    pub torrent_id: String,
    pub torrent_aliases: Vec<String>,
    pub magnet: String,
    pub file: String,
    pub file_length: u64,
    pub release: RecordedRelease,
}

fn torrent_hash(s: &str) -> bool {
    [40, 64].contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !s.bytes().all(|b| b == b'0')
        && !(s.len() == 64 && s[..40].bytes().all(|b| b == b'0'))
}
fn job_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn xt(id: &str) -> String {
    format!(
        "urn:{}{id}",
        if id.len() == 40 { "btih:" } else { "btmh:1220" }
    )
}

/// Parse the restricted magnet without DNS, metadata traffic, or permissive extras.
pub(crate) fn validate_magnet(s: &str, expected: &str) -> Result<()> {
    if !torrent_hash(expected)
        || s.len() > 8192
        || !s.is_ascii()
        || s.bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err("IRC: invalid pinned magnet".into());
    }
    let query = s
        .strip_prefix("magnet:?")
        .ok_or("IRC: acquisition requires a pinned magnet")?;
    let mut identity = false;
    let mut trackers = BTreeSet::new();
    let mut peers = BTreeSet::new();
    for part in query.split('&') {
        let (key, encoded) = part
            .split_once('=')
            .ok_or("IRC: invalid magnet parameter")?;
        let value =
            crate::torrent::url_decode(encoded).map_err(|_| "IRC: invalid magnet escape")?;
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err("IRC: invalid magnet parameter value".into());
        }
        match key {
            "xt" if !identity && value == xt(expected) => identity = true,
            "tr" => {
                let parsed = if let Some(rest) = value.strip_prefix("udp://") {
                    crate::net::parse_url(&format!("https://{rest}"))
                } else {
                    crate::net::parse_url(&value)
                }
                .map_err(|_| "IRC: invalid configured tracker")?;
                if value.starts_with("http://")
                    && !parsed
                        .host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
                {
                    return Err("IRC: plaintext HTTP trackers require literal loopback".into());
                }
                if trackers.len() == 4 || !trackers.insert(value) {
                    return Err("IRC: too many or duplicate configured trackers".into());
                }
            }
            "x.pe" => {
                let address = value
                    .parse::<SocketAddr>()
                    .map_err(|_| "IRC: peers require literal IP addresses")?;
                if address.port() == 0
                    || address.ip().is_unspecified()
                    || address.ip().is_multicast()
                    || peers.len() == 8
                    || !peers.insert(address)
                {
                    return Err("IRC: invalid, duplicate or excessive configured peers".into());
                }
            }
            _ => return Err("IRC: unsupported or duplicate magnet parameter".into()),
        }
    }
    if !identity || (trackers.is_empty() && peers.is_empty()) {
        return Err("IRC: a pinned identity and configured discovery are required".into());
    }
    Ok(())
}

pub(crate) fn validate_template(s: &str) -> Result<()> {
    if s.matches("{xt}").count() != 1 || s.replace("{xt}", "").contains(['{', '}']) {
        return Err("IRC: magnet_template requires exactly one {xt} placeholder".into());
    }
    for id in ["1".repeat(40), "1".repeat(64)] {
        validate_magnet(&s.replace("{xt}", &xt(&id)), &id)?;
    }
    Ok(())
}
pub(crate) fn render_template(s: &str, id: &str) -> Result<String> {
    let magnet = s.replace("{xt}", &xt(id));
    validate_magnet(&magnet, id)?;
    Ok(magnet)
}

impl Origin {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        for (k, s) in [
            ("announcement_id", &self.announcement_id),
            ("job_id", &self.job_id),
            ("source_id", &self.source_id),
            ("binding", &self.binding),
            ("rule_id", &self.rule_id),
            ("fingerprint", &self.fingerprint),
            ("media_key", &self.media_key),
            ("torrent_id", &self.torrent_id),
            ("magnet", &self.magnet),
            ("file", &self.file),
        ] {
            v.insert(k, s.clone());
        }
        v.insert("file_length", self.file_length.to_string());
        v.insert(
            "torrent_aliases",
            Value::Array(
                self.torrent_aliases
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        v.insert("release", self.release.to_json());
        v
    }
    pub fn public_json(&self) -> Value {
        let mut v = self.to_json();
        if let Value::Object(m) = &mut v {
            for key in ["binding", "fingerprint", "magnet"] {
                m.remove(key);
            }
        }
        v.insert("file", crate::integrations::report_text(&self.file, 4096));
        let mut release = self.release.to_json();
        release.insert(
            "title",
            crate::integrations::report_text(&self.release.title, 2048),
        );
        v.insert("release", release);
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "announcement_id",
                "job_id",
                "source_id",
                "binding",
                "rule_id",
                "fingerprint",
                "media_key",
                "torrent_id",
                "torrent_aliases",
                "magnet",
                "file",
                "file_length",
                "release",
            ],
        )?;
        let result = Self {
            announcement_id: text(v, "announcement_id")?,
            job_id: text(v, "job_id")?,
            source_id: text(v, "source_id")?,
            binding: text(v, "binding")?,
            rule_id: text(v, "rule_id")?,
            fingerprint: text(v, "fingerprint")?,
            media_key: text(v, "media_key")?,
            torrent_id: text(v, "torrent_id")?,
            magnet: text(v, "magnet")?,
            file: text(v, "file")?,
            torrent_aliases: v
                .get("torrent_aliases")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty() && a.len() <= 2)
                .ok_or("IRC: invalid torrent aliases")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("IRC: invalid torrent alias".into())
                })
                .collect::<Result<_>>()?,
            file_length: super::integer(v, "file_length")?,
            release: RecordedRelease::from_json(
                v.get("release").ok_or("IRC: missing routed release")?,
            )?,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        if !hash(&self.announcement_id)
            || !job_id(&self.job_id)
            || !valid_id(&self.source_id)
            || !valid_id(&self.rule_id)
            || !hash(&self.binding)
            || !hash(&self.fingerprint)
            || self.media_key.is_empty()
            || self.media_key.len() > 2048
            || self.media_key.chars().any(char::is_control)
            || self.file_length == 0
            || !video(&self.file)
            || self.torrent_aliases.is_empty()
            || self.torrent_aliases.len() > 2
            || !self.torrent_aliases.contains(&self.torrent_id)
            || self.torrent_aliases.iter().any(|id| !torrent_hash(id))
            || self
                .torrent_aliases
                .iter()
                .map(String::len)
                .collect::<BTreeSet<_>>()
                .len()
                != self.torrent_aliases.len()
        {
            return Err("IRC: invalid acquisition provenance".into());
        }
        crate::pack::validate_file_path(&self.file)?;
        self.release.validate()?;
        validate_magnet(&self.magnet, &self.torrent_id)
    }
    pub fn validate_job(&self, job: &Job) -> Result<()> {
        self.validate()?;
        if job.id != self.job_id
            || job.request.media_key() != self.media_key
            || !matches!(job.request.kind.as_str(), "movie" | "episode")
            || job.request.source_path.is_some()
            || job.request.source_url.is_some()
            || job.upgrade_parent.is_some()
            || job.pack_file.is_some()
            || job.pack_origin.is_some()
            || job.shared_file.is_some()
            || job.shared_upgrade.is_some()
            || job.acquisition_url.as_deref() != Some(&self.magnet)
            || job.release.as_ref() != Some(&self.release)
            || job
                .download_id
                .as_ref()
                .is_some_and(|id| !self.torrent_aliases.contains(id))
            || job.files.len() > 1
            || job.imports.len() > 1
            || (!job.files.is_empty() && job.download_id.is_none())
            || (!job.imports.is_empty() && job.files.len() != 1)
            || !title_matches(&job.request, &self.release.title)
            || !title_matches(
                &job.request,
                Path::new(&self.file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
            )
        {
            return Err("IRC: job differs from immutable verified candidate".into());
        }
        if let Some(file) = job.files.first() {
            let file = Path::new(file);
            let relative = Path::new(
                job.download_id
                    .as_deref()
                    .ok_or("IRC: missing native identity")?,
            )
            .join(&self.file);
            if !file.is_absolute()
                || !file.ends_with(relative)
                || file
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
            {
                return Err("IRC: job files differ from the verified torrent path".into());
            }
        }
        Ok(())
    }
    pub(crate) fn validate_record(&self, r: &Record) -> Result<()> {
        if self.announcement_id != r.id
            || self.source_id != r.source_id
            || self.binding != r.binding
            || self.fingerprint != r.fingerprint
            || self.media_key != r.announcement.request.media_key()
            || self.torrent_id != r.announcement.info_hash
            || self.release.title != r.announcement.title
            || !r.evaluations.iter().any(|e| {
                e.rule_id == self.rule_id
                    && e.outcome == "matched"
                    && e.profile == self.release.profile
            })
            || super::outcome(&r.evaluations) != "matched"
        {
            return Err("IRC: acquisition differs from the original claim and rule".into());
        }
        self.validate()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Route {
    pub phase: String,
    pub origin: Origin,
    pub reserved_at: u64,
}
impl Route {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("phase", self.phase.clone());
        v.insert("origin", self.origin.to_json());
        v.insert("reserved_at", self.reserved_at.to_string());
        v
    }
    pub(crate) fn public_json(&self) -> Value {
        let mut v = self.to_json();
        v.insert("origin", self.origin.public_json());
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        only(v, &["phase", "origin", "reserved_at"])?;
        let r = Self {
            phase: text(v, "phase")?,
            origin: Origin::from_json(v.get("origin").ok_or("IRC: missing reservation origin")?)?,
            reserved_at: super::integer(v, "reserved_at")?,
        };
        if !matches!(r.phase.as_str(), "reserved" | "routed" | "aborted") {
            return Err("IRC: invalid acquisition reservation".into());
        }
        Ok(r)
    }
}

pub(crate) fn eligible(job: &Job) -> bool {
    job.state == "queued"
        && job.lease_id.is_none()
        && job.lease_until == 0
        && matches!(job.request.kind.as_str(), "movie" | "episode")
        && job.request.tmdb_id.is_some()
        && job.request.source_path.is_none()
        && job.request.source_url.is_none()
        && job.acquisition_url.is_none()
        && job.download_id.is_none()
        && job.release.is_none()
        && job.files.is_empty()
        && job.imports.is_empty()
        && job.upgrade_parent.is_none()
        && job.pack_file.is_none()
        && job.pack_origin.is_none()
        && job.shared_file.is_none()
        && job.shared_upgrade.is_none()
        && job.irc_origin.is_none()
}

pub(crate) fn waits_for_candidate(config: &Config, job: &Job) -> bool {
    if !eligible(job)
        || !config.downloads_enabled
        || !config
            .irc
            .rules
            .iter()
            .any(|r| r.enabled && r.action == "grab")
    {
        return false;
    }
    let Ok((name, profile)) = admitted_profile(config, job) else {
        return false;
    };
    config.irc.rules.iter().any(|r| {
        r.enabled
            && r.action == "grab"
            && r.kind == job.request.kind
            && r.profile == name
            && config.selection.profiles.get(name) == Some(profile)
            && config
                .irc
                .sources
                .iter()
                .any(|s| s.id == r.source && s.enabled && s.magnet_template.is_some())
    })
}

fn admitted_profile<'a>(
    config: &'a Config,
    job: &'a Job,
) -> Result<(&'a str, &'a crate::selection::Profile)> {
    if let Some(p) = &job.requester {
        Ok((&p.capture.profile_name, &p.capture.profile))
    } else {
        config.selection.profile(&job.request.kind)
    }
}

fn video(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| {
            ["mp4", "m4v", "mov", "mkv", "webm", "avi", "ts", "m2ts"]
                .iter()
                .any(|ext| s.eq_ignore_ascii_case(ext))
        })
}
fn title_matches(request: &crate::store::Request, label: &str) -> bool {
    let t = tokens(label);
    let name = tokens(&request.title);
    if name.is_empty() || !t.starts_with(&name) {
        return false;
    }
    let mut suffix = &t[name.len()..];
    if request.year != 0 {
        if suffix.first().and_then(|s| s.parse::<u32>().ok()) != Some(request.year) {
            return false;
        }
        suffix = &suffix[1..];
    }
    if request.kind == "movie" {
        !suffix.iter().any(|s| {
            crate::numbering::episode_marker(s).is_some()
                || (s.starts_with('s') && s.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
        })
    } else {
        let number = request
            .source_numbering
            .unwrap_or(SourceNumber::SeasonEpisode(EpisodeNumber {
                season: request.season,
                episode: request.episode,
            }));
        suffix.iter().filter(|s| number.matches(s)).count() == 1
            && !suffix
                .iter()
                .any(|s| crate::numbering::conflicting_marker(number, s, request.year))
    }
}

fn choose_file(job: &Job, metadata: &TorrentMetadata) -> Result<(String, u64)> {
    let mut files = metadata
        .files
        .iter()
        .filter(|f| !f.padding && f.length != 0 && video(&f.path));
    let file = files.next().ok_or("IRC: verified metadata has no video")?;
    if files.next().is_some()
        || !title_matches(
            &job.request,
            Path::new(&file.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(""),
        )
    {
        return Err(
            "IRC: metadata is a pack, ambiguous, or differs from the admitted source labels".into(),
        );
    }
    crate::pack::validate_file_path(&file.path)?;
    Ok((file.path.clone(), file.length))
}

/// Validate both directions before any native workers can resume.
pub(crate) fn recovered_state(state: &State, jobs: &Store) -> Result<State> {
    let mut next = state.clone();
    for r in state.records.values() {
        if let Some(route) = &r.route {
            let job = jobs
                .get(&route.origin.job_id)
                .ok_or("IRC: reserved job is missing")?;
            if route.phase == "aborted" {
                if job
                    .irc_origin
                    .as_ref()
                    .is_some_and(|o| o.announcement_id == r.id)
                {
                    return Err("IRC: aborted reservation has an acquisition".into());
                }
                continue;
            }
            if let Some(origin) = &job.irc_origin {
                if origin != &route.origin
                    && route.phase == "reserved"
                    && origin.announcement_id != r.id
                {
                    next.records
                        .get_mut(&r.id)
                        .ok_or("IRC: missing recovery row")?
                        .route
                        .as_mut()
                        .ok_or("IRC: missing recovery reservation")?
                        .phase = "aborted".into();
                    continue;
                }
                if origin != &route.origin {
                    return Err("IRC: job and durable reservation disagree".into());
                }
                origin.validate_job(&job)?;
                if route.phase == "reserved" {
                    next.records
                        .get_mut(&r.id)
                        .ok_or("IRC: missing recovery row")?
                        .route
                        .as_mut()
                        .ok_or("IRC: missing recovery reservation")?
                        .phase = "routed".into();
                }
            } else if route.phase == "reserved" {
                // Interrupted admission is never replayed into another acquisition.
                next.records
                    .get_mut(&r.id)
                    .ok_or("IRC: missing recovery row")?
                    .route
                    .as_mut()
                    .ok_or("IRC: missing recovery reservation")?
                    .phase = "aborted".into();
            } else if route.phase == "routed" {
                return Err("IRC: routed job has lost its immutable origin".into());
            }
        }
    }
    let mut physical = BTreeSet::new();
    for job in jobs.list() {
        if let Some(origin) = &job.irc_origin {
            origin.validate_job(&job)?;
            let route = state
                .records
                .get(&origin.announcement_id)
                .and_then(|r| r.route.as_ref())
                .ok_or("IRC: acquisition has no durable announcement reservation")?;
            if &route.origin != origin
                || route.phase == "aborted"
                || origin
                    .torrent_aliases
                    .iter()
                    .any(|id| !physical.insert((id.clone(), origin.file.clone())))
            {
                return Err("IRC: acquisition provenance or physical ownership conflicts".into());
            }
        }
    }
    Ok(next)
}

#[derive(Default)]
pub(crate) struct Runtime {
    outcomes: BTreeMap<String, String>,
    retry: BTreeMap<String, Instant>,
    cursor: Option<String>,
}
impl Runtime {
    pub(crate) fn annotate(&self, record: &Record) -> Value {
        let mut v = record.public_json();
        v.insert(
            "routing_outcome",
            record
                .route
                .as_ref()
                .map(|r| r.phase.as_str())
                .or_else(|| self.outcomes.get(&record.id).map(String::as_str))
                .unwrap_or("not_attempted"),
        );
        v
    }
}

fn matched_rule<'a>(config: &'a Config, r: &Record, fingerprint: &str) -> Result<&'a super::Rule> {
    if r.decision != "pending"
        || r.route.is_some()
        || super::outcome(&r.evaluations) != "matched"
        || r.fingerprint != fingerprint
    {
        return Err("IRC: claim is reviewed, stale, reserved or has conflicting rules".into());
    }
    let source = config.irc.source(&r.source_id)?;
    if !source.enabled || source.binding() != r.binding || source.magnet_template.is_none() {
        return Err("IRC: acquisition source is disabled or changed".into());
    }
    let evaluation = r
        .evaluations
        .iter()
        .find(|e| e.outcome == "matched")
        .ok_or("IRC: unmatched claim")?;
    config
        .irc
        .rules
        .iter()
        .find(|rule| rule.id == evaluation.rule_id && rule.enabled && rule.action == "grab")
        .ok_or("IRC: matched rule does not authorize acquisition".into())
}

fn candidate(
    config: &Config,
    r: &Record,
    rule: &super::Rule,
    jobs: &Store,
    requesters: &crate::requesters::State,
) -> Result<Job> {
    let matches = jobs.irc_jobs(&r.announcement.request);
    let eligible: Vec<_> = matches.iter().filter(|j| eligible(j)).collect();
    if eligible.len() != 1
        || matches.iter().any(|j| {
            j.state == "ready"
                || j.shared_file.is_some()
                || j.pack_file.is_some()
                || j.upgrade_parent.is_some()
        })
    {
        return Err("IRC: waiting for one eligible admitted job without existing ownership".into());
    }
    let job = (*eligible[0]).clone();
    if !crate::requesters::engine::interest(requesters, &job, config) {
        return Err("IRC: acquisition has no approved demand".into());
    }
    if tokens(&job.request.title) != tokens(&r.announcement.request.title)
        || job.request.year != r.announcement.request.year
        || !title_matches(&job.request, &r.announcement.title)
    {
        return Err("IRC: claim differs from the admitted canonical title or source labels".into());
    }
    let (name, profile) = admitted_profile(config, &job)?;
    if name != rule.profile
        || config.selection.profiles.get(name) != Some(profile)
        || !profile
            .assess(&r.announcement.title, &job.request.title)
            .accepted
    {
        return Err("IRC: candidate differs from the admitted profile".into());
    }
    Ok(job)
}

impl Engine {
    /// One bounded automatic pass. Configuration authorizes grabs; this creates no demand.
    pub fn irc_route_pending(&self) -> Result<Value> {
        if self.read_only || !self.config.downloads_enabled || self.stopped.load(Ordering::Acquire)
        {
            return Err("IRC: automatic routing requires writable enabled downloads".into());
        }
        let _pass = lock(&self.irc_route_lock)?;
        let fingerprint = self.config.irc.fingerprint(&self.config.selection);
        let now = Instant::now();
        let record = {
            let ledger = lock(&self.irc_store)?;
            let runtime = lock(&self.irc_route_runtime)?;
            let available = || {
                ledger.state.records.values().filter(|r| {
                    runtime.retry.get(&r.id).is_none_or(|at| now >= *at)
                        && matched_rule(&self.config, r, &fingerprint).is_ok()
                })
            };
            available()
                .find(|r| runtime.cursor.as_ref().is_none_or(|cursor| &r.id > cursor))
                .or_else(|| available().next())
                .cloned()
        };
        let mut report = Value::object();
        report.insert("routed", false);
        let Some(record) = record else {
            report.insert("outcome", "idle");
            return Ok(report);
        };
        report.insert("announcement_id", record.id.clone());
        let result = self.route_announcement(&record, &fingerprint);
        let mut runtime = lock(&self.irc_route_runtime)?;
        runtime.cursor = Some(record.id.clone());
        let outcome = match result.as_ref().err().map(String::as_str) {
            None => "routed",
            Some("IRC: waiting for one eligible admitted job without existing ownership") => {
                "waiting_for_admitted_job"
            }
            Some("IRC: acquisition has no approved demand") => "approval_required",
            Some("IRC: claim differs from the admitted canonical title or source labels") => {
                "claim_mismatch"
            }
            Some("IRC: candidate differs from the admitted profile") => "profile_mismatch",
            Some("IRC: metadata unavailable or unauthenticated") => "metadata_unavailable",
            Some("IRC: metadata file rejected") => "metadata_rejected",
            Some(
                "IRC: admission changed or stopped"
                | "IRC: admitted job changed during metadata inspection",
            ) => "admission_changed",
            _ => "admission_or_storage_rejected",
        };
        runtime.outcomes.insert(record.id.clone(), outcome.into());
        runtime
            .retry
            .insert(record.id, Instant::now() + Duration::from_secs(30));
        report.insert("outcome", outcome);
        if let Ok(job) = result {
            report.insert("routed", true);
            report.insert("job_id", job.id);
        }
        // Protocol and discovery errors may contain credentials; reports use fixed outcomes.
        Ok(report)
    }
    fn route_announcement(&self, record: &Record, fingerprint: &str) -> Result<Job> {
        let rule = matched_rule(&self.config, record, fingerprint)?;
        let job = {
            let requesters = lock(&self.requester_store)?;
            let jobs = lock(&self.store)?;
            candidate(&self.config, record, rule, &jobs, &requesters.state)?
        };
        let source = self.config.irc.source(&record.source_id)?;
        let magnet = render_template(
            source
                .magnet_template
                .as_deref()
                .ok_or("IRC: missing magnet template")?,
            &record.announcement.info_hash,
        )?;
        // Metadata I/O happens before admission and outside every persistent-store lock.
        let metadata = crate::torrent::inspection::inspect_metadata(
            &magnet,
            Instant::now() + Duration::from_secs(10),
        )
        .map_err(|_| "IRC: metadata unavailable or unauthenticated")?;
        if metadata.id != record.announcement.info_hash {
            return Err("IRC: metadata identity differs".into());
        }
        let (file, file_length) =
            choose_file(&job, &metadata).map_err(|_| "IRC: metadata file rejected")?;
        let origin = Origin {
            announcement_id: record.id.clone(),
            job_id: job.id.clone(),
            source_id: record.source_id.clone(),
            binding: record.binding.clone(),
            rule_id: rule.id.clone(),
            fingerprint: record.fingerprint.clone(),
            media_key: job.request.media_key(),
            torrent_id: metadata.id,
            torrent_aliases: metadata.aliases,
            magnet,
            file,
            file_length,
            release: RecordedRelease {
                title: record.announcement.title.clone(),
                profile: rule.profile.clone(),
            },
        };
        let mut ledger = lock(&self.irc_store)?;
        let current = ledger
            .state
            .records
            .get(&record.id)
            .ok_or("IRC: missing claim")?;
        if current != record || self.stopped.load(Ordering::Acquire) {
            return Err("IRC: admission changed or stopped".into());
        }
        let current_rule = matched_rule(&self.config, current, fingerprint)?;
        let requesters = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let current_job = candidate(
            &self.config,
            current,
            current_rule,
            &jobs,
            &requesters.state,
        )?;
        if current_job != job {
            return Err("IRC: admitted job changed during metadata inspection".into());
        }
        origin.validate_record(current)?;
        jobs.check_irc_origin(&origin)?;
        let mut next = ledger.state.clone();
        let row = next
            .records
            .get_mut(&record.id)
            .ok_or("IRC: missing admission row")?;
        row.route = Some(Route {
            phase: "reserved".into(),
            origin: origin.clone(),
            reserved_at: store::now(),
        });
        row.revision = row
            .revision
            .checked_add(1)
            .ok_or("IRC: row revision overflow")?;
        ledger.save(next)?;
        // A crash between these checked writes leaves a recoverable, non-replayable intent.
        let job = jobs.attach_irc_origin(origin)?;
        let mut next = ledger.state.clone();
        next.records
            .get_mut(&record.id)
            .ok_or("IRC: missing committed row")?
            .route
            .as_mut()
            .ok_or("IRC: missing committed reservation")?
            .phase = "routed".into();
        ledger.save(next)?;
        Ok(job)
    }
    pub(crate) fn irc_routing_json(&self) -> Result<Value> {
        let runtime = lock(&self.irc_route_runtime)?;
        let mut v = Value::object();
        v.insert("retry_secs", 30_u32);
        v.insert(
            "outcomes",
            Value::Object(
                runtime
                    .outcomes
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.clone())))
                    .collect(),
            ),
        );
        Ok(v)
    }
}

pub(crate) fn start(engine: &Arc<Engine>, handles: &mut Vec<JoinHandle<()>>) {
    if !engine.config.downloads_enabled
        || !engine
            .config
            .irc
            .rules
            .iter()
            .any(|r| r.enabled && r.action == "grab")
    {
        return;
    }
    let engine = engine.clone();
    handles.push(thread::spawn(move || {
        while !engine.stopped.load(Ordering::Acquire) {
            let _ = engine.irc_route_pending();
            engine.wait(1000);
        }
    }));
}
