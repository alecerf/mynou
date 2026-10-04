//! Season-pack decisions stay outside storage locks until a bounded, fenced commit.
use super::{MAX_PACK_EPISODES, PackEpisode, PackSubmission, validate_file_path};
use crate::{
    Result,
    crypto::sha256,
    date,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    selection::tokens,
    series::{Episode, Record},
    store::{RecordedRelease, Request, Store},
    torrent::{TorrentMetadata, inspect_metadata},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

fn valid_id(value: &str, sizes: &[usize]) -> bool {
    sizes.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackOrigin {
    pub series_id: String,
    pub series_revision: u64,
    pub season: u32,
    pub scope_id: String,
    pub candidate_id: String,
    pub torrent_id: String,
    /// Pack-title provenance is not an individual episode's upgrade baseline.
    pub release: RecordedRelease,
}

impl PackOrigin {
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.series_id, &[32])
            || self.series_revision == 0
            || self.season > 9999
            || !valid_id(&self.scope_id, &[64])
            || !valid_id(&self.candidate_id, &[64])
            || !valid_id(&self.torrent_id, &[40, 64])
        {
            return Err("Invalid automatic pack provenance".into());
        }
        self.release.validate()
    }
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("series_id", self.series_id.clone());
        value.insert("series_revision", self.series_revision.to_string());
        value.insert("season", self.season);
        value.insert("scope_id", self.scope_id.clone());
        value.insert("candidate_id", self.candidate_id.clone());
        value.insert("torrent_id", self.torrent_id.clone());
        value.insert("release", self.release.to_json());
        value
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        only(
            value,
            &[
                "series_id",
                "series_revision",
                "season",
                "scope_id",
                "candidate_id",
                "torrent_id",
                "release",
            ],
        )?;
        let string = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "Invalid automatic pack provenance".to_owned())
        };
        let result = Self {
            series_id: string("series_id")?,
            series_revision: string("series_revision")?
                .parse()
                .map_err(|_| "Invalid pack series revision")?,
            season: season(value)?,
            scope_id: string("scope_id")?,
            candidate_id: string("candidate_id")?,
            torrent_id: string("torrent_id")?,
            release: RecordedRelease::from_json(
                value
                    .get("release")
                    .ok_or("Missing pack release provenance")?,
            )?,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoPackRequest {
    pub season: u32,
    pub apply: bool,
    pub scope_id: Option<String>,
    pub candidate_id: Option<String>,
}

fn only(value: &Value, allowed: &[&str]) -> Result<()> {
    let fields = value.as_object().ok_or("Pack search must be an object")?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unknown pack search or provenance field".into());
    }
    Ok(())
}
fn season(value: &Value) -> Result<u32> {
    value
        .get("season")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n <= 9999)
        .ok_or_else(|| "Pack search requires a season from 0 to 9999".into())
}
impl AutoPackRequest {
    pub fn from_json(value: &Value) -> Result<Self> {
        only(value, &["season", "apply", "scope_id", "candidate_id"])?;
        let apply = match value.get("apply") {
            None => false,
            Some(value) => value.as_bool().ok_or("Pack apply must be a boolean")?,
        };
        let guard = |key| match value.get(key) {
            None => Ok(None),
            Some(Value::String(id)) if valid_id(id, &[64]) => Ok(Some(id.clone())),
            _ => {
                Err("Pack guards require a 64-character lowercase hexadecimal identity".to_owned())
            }
        };
        let result = Self {
            season: season(value)?,
            apply,
            scope_id: guard("scope_id")?,
            candidate_id: guard("candidate_id")?,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        if self.season > 9999
            || self.scope_id.is_some() != self.candidate_id.is_some()
            || (!self.apply && self.scope_id.is_some())
            || self
                .scope_id
                .iter()
                .chain(&self.candidate_id)
                .any(|id| !valid_id(id, &[64]))
        {
            return Err("Pack preview guards must be supplied together for an apply action".into());
        }
        Ok(())
    }
}

fn public_episode(episode: &Episode) -> Value {
    let mut value = episode.to_json();
    value.insert("title", integrations::report_text(&episode.title, 2048));
    value
}

pub(super) struct Scope {
    pub id: String,
    record: Record,
    episodes: Vec<Episode>,
}

pub(super) fn capture_scope(record: &Record, store: &Store, season: u32) -> Result<Scope> {
    if season > 9999
        || !record
            .plan
            .episodes
            .iter()
            .any(|episode| episode.season == season)
    {
        return Err("Season is absent from the accepted catalog plan".into());
    }
    if record
        .plan
        .episodes
        .iter()
        .filter(|ep| ep.season == season)
        .any(|ep| {
            record
                .episode_request(ep)
                .source_numbering
                .is_some_and(|number| {
                    number
                        != crate::numbering::SourceNumber::SeasonEpisode(
                            crate::numbering::EpisodeNumber {
                                season: ep.season,
                                episode: ep.episode,
                            },
                        )
                })
        })
    {
        return Err("Alternate source numbering requires explicit pack file mappings".into());
    }
    let today = date::today();
    let existing = store.media_keys();
    // An explicit on-demand pack action does not enable background series monitoring.
    let mut episodes: Vec<_> = record
        .plan
        .episodes
        .iter()
        .filter(|episode| {
            episode.season == season
                && (season != 0 || record.include_specials)
                && !record.excluded.contains(&(season, episode.episode))
                && episode.catalog_id.is_some()
                && episode.air_date.as_deref().is_some_and(|day| {
                    day <= today.as_str()
                        && record
                            .start_date
                            .as_deref()
                            .is_none_or(|start| day >= start)
                })
                && !existing.contains(&record.episode_request(episode).media_key())
        })
        .cloned()
        .collect();
    episodes.sort_by_key(|episode| episode.episode);
    if episodes.len() > MAX_PACK_EPISODES {
        return Err("Automatic pack scope exceeds 64 missing aired episodes".into());
    }
    let mut identity = record.to_json();
    if let Value::Object(fields) = &mut identity {
        for key in [
            "created_at",
            "updated_at",
            "checked_at",
            "next_check_at",
            "last_error",
        ] {
            fields.remove(key);
        }
    }
    identity.insert("pack_season", season);
    identity.insert("today", today);
    identity.insert(
        "missing_episodes",
        Value::Array(episodes.iter().map(Episode::to_json).collect()),
    );
    let id = sha256(json::stringify(&identity).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Scope {
        id,
        record: record.clone(),
        episodes,
    })
}

fn season_marker(text: &str) -> Option<u32> {
    let number = text.strip_prefix('s')?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .then(|| number.parse::<u32>().ok())
        .flatten()
        .filter(|n| *n <= 9999)
}
fn episode_marker(text: &str) -> Option<(u32, u32)> {
    let (s, e) = if let Some(rest) = text.strip_prefix('s') {
        rest.split_once('e')?
    } else {
        text.split_once('x')?
    };
    if s.is_empty() || e.is_empty() || !s.bytes().chain(e.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let season = s.parse::<u32>().ok()?;
    let episode = e.parse::<u32>().ok()?;
    if season > 9999 || episode == 0 || episode > 99999 || (!text.starts_with('s') && season >= 320)
    {
        return None;
    }
    Some((season, episode))
}
fn episode_like(text: &str) -> bool {
    (text.starts_with('s')
        && text.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
        && text.contains('e'))
        || text
            .strip_prefix('e')
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        || text.split_once('x').is_some_and(|(s, e)| {
            !s.is_empty()
                && !e.is_empty()
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u32>().is_ok_and(|n| n < 320)
        })
}

pub(crate) fn season_title_matches(request: &Request, title: &str) -> bool {
    let expected = tokens(&request.title);
    let actual = tokens(title);
    if expected.is_empty() || !actual.starts_with(&expected) {
        return false;
    }
    let mut rest = &actual[expected.len()..];
    if let Some(year) = rest
        .first()
        .and_then(|token| token.parse::<u32>().ok())
        .filter(|n| (1888..=2200).contains(n))
    {
        if request.year != 0 && year != request.year {
            return false;
        }
        rest = &rest[1..];
    }
    let (found, consumed) = if rest.first().is_some_and(|token| token == "season") {
        (rest.get(1).and_then(|n| n.parse::<u32>().ok()), 2)
    } else {
        (rest.first().and_then(|token| season_marker(token)), 1)
    };
    found == Some(request.season)
        && rest.len() >= consumed
        && !rest[consumed..]
            .iter()
            .any(|token| season_marker(token).is_some() || episode_like(token) || token == "season")
}

fn map_metadata(
    scope: &Scope,
    metadata: &TorrentMetadata,
    season: u32,
) -> Result<Vec<PackEpisode>> {
    let known: BTreeMap<_, _> = scope
        .record
        .plan
        .episodes
        .iter()
        .map(|episode| ((episode.season, episode.episode), episode))
        .collect();
    let desired: BTreeSet<_> = scope
        .episodes
        .iter()
        .map(|episode| (episode.season, episode.episode))
        .collect();
    let expected = tokens(&scope.record.plan.request.title);
    let mut seen = BTreeSet::new();
    let mut mapped = Vec::new();
    for file in &metadata.files {
        let path = Path::new(&file.path);
        if file.padding
            || !path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                ["mp4", "m4v", "mov", "mkv", "webm", "avi"]
                    .contains(&e.to_ascii_lowercase().as_str())
            })
        {
            continue;
        }
        validate_file_path(&file.path)?;
        let basename = path
            .file_stem()
            .and_then(|n| n.to_str())
            .ok_or("Invalid pack video filename")?;
        if ["sample", "trailer"].contains(&basename.to_ascii_lowercase().as_str())
            || path.parent().is_some_and(|parent| {
                parent.components().skip(1).any(|component| {
                    component.as_os_str().to_str().is_some_and(|n| {
                        ["samples", "trailers", "extras"].contains(&n.to_ascii_lowercase().as_str())
                    })
                })
            })
        {
            continue;
        }
        let name = tokens(basename);
        let markers: Vec<_> = name
            .iter()
            .enumerate()
            .filter_map(|(index, token)| episode_marker(token).map(|pair| (index, pair)))
            .collect();
        if markers.len() != 1
            || name.iter().enumerate().any(|(index, token)| {
                episode_like(token) && markers.first().is_none_or(|(at, _)| index != *at)
            })
        {
            return Err(
                "Pack video numbering is missing, conflicting, absolute, or multi-episode".into(),
            );
        }
        let (at, pair) = markers[0];
        let prefix = &name[..at];
        if !prefix.is_empty()
            && !(prefix == expected.as_slice()
                || (prefix.starts_with(&expected)
                    && prefix.len() == expected.len() + 1
                    && prefix.last().and_then(|n| n.parse::<u32>().ok())
                        == Some(scope.record.plan.request.year)))
        {
            return Err("Pack video prefix conflicts with the accepted series title".into());
        }
        if pair.0 != season || !known.contains_key(&pair) {
            return Err("Pack video episode is absent from the requested catalog season".into());
        }
        if path.parent().is_some_and(|parent| {
            parent.components().any(|component| {
                component.as_os_str().to_str().is_some_and(|part| {
                    tokens(part)
                        .iter()
                        .filter_map(|token| season_marker(token))
                        .any(|n| n != season)
                })
            })
        }) {
            return Err("Pack directory season conflicts with its video numbering".into());
        }
        if !seen.insert(pair) {
            return Err("Pack has duplicate video assignments for one catalog episode".into());
        }
        if desired.contains(&pair) {
            if file.length == 0 {
                return Err("Mapped pack video is empty".into());
            }
            mapped.push(PackEpisode {
                season: pair.0,
                episode: pair.1,
                file_path: file.path.clone(),
            });
        }
    }
    if mapped.len() != desired.len() {
        return Err("Pack is missing one or more requested aired episodes".into());
    }
    mapped.sort_by_key(|mapping| (mapping.season, mapping.episode));
    Ok(mapped)
}

impl Engine {
    pub fn search_packs(&self, id: &str, query: &AutoPackRequest) -> Result<Value> {
        self.search_packs_before(id, query, Instant::now() + Duration::from_secs(90))
    }
    pub fn search_packs_before(
        &self,
        id: &str,
        query: &AutoPackRequest,
        deadline: Instant,
    ) -> Result<Value> {
        query.validate()?;
        if query.apply && self.read_only {
            return Err("Automatic pack acquisition requires writable storage".into());
        }
        let deadline = deadline.min(Instant::now() + Duration::from_secs(90));
        if Instant::now() >= deadline {
            return Err("Pack search deadline exceeded".into());
        }
        let scope = {
            let series = lock(&self.series_store)?;
            let record = series.get(id).ok_or("Unknown tracked series")?;
            {
                let jobs = lock(&self.store)?;
                capture_scope(&record, &jobs, query.season)?
            }
        };
        if query.scope_id.as_ref().is_some_and(|id| id != &scope.id) {
            return Err("Pack preview scope changed; search again before acquiring".into());
        }
        let mut request = scope.record.plan.request.clone();
        request.season = query.season;
        request.episode = 0;
        let (mut report, candidates) = if scope.episodes.is_empty() {
            let mut report = Value::object();
            report.insert("accepted", Value::Array(Vec::new()));
            report.insert("rejected", Value::Array(Vec::new()));
            (report, Vec::new())
        } else {
            integrations::search_pack_candidates(&self.config, &request, deadline)?
        };
        report.insert("series_id", id.to_owned());
        report.insert("season", query.season);
        report.insert("series_revision", scope.record.revision.to_string());
        report.insert("scope_id", scope.id.clone());
        report.insert("apply", query.apply);
        report.insert("scope_empty", scope.episodes.is_empty());
        report.insert(
            "requested_episodes",
            Value::Array(scope.episodes.iter().map(public_episode).collect()),
        );
        report.insert("selected_candidate_id", Value::Null);
        let mut decisions = Vec::new();
        let mut selected = None;
        let mut metadata_cache = BTreeMap::<String, Result<TorrentMetadata>>::new();
        report.insert("metadata_candidate_limit", 8_u32);
        for mut candidate in candidates {
            if self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("Pack search stopped or exceeded its deadline".into());
            }
            let metadata = if let Some(cached) = metadata_cache.get(&candidate.url) {
                cached.clone()
            } else {
                let fetched = inspect_metadata(
                    &candidate.url,
                    deadline.min(Instant::now() + Duration::from_secs(10)),
                );
                metadata_cache.insert(candidate.url.clone(), fetched.clone());
                fetched
            };
            let result = metadata.and_then(|metadata| {
                map_metadata(&scope, &metadata, query.season).map(|mapping| (metadata, mapping))
            });
            let mut decision = Value::object();
            decision.insert("candidate_id", candidate.id.clone());
            match result {
                Ok((metadata, mappings)) => {
                    let mut bound = Value::object();
                    bound.insert("scope_id", scope.id.clone());
                    bound.insert("release_id", candidate.id.clone());
                    bound.insert("torrent_id", metadata.id.clone());
                    bound.insert(
                        "paths",
                        Value::Array(
                            mappings
                                .iter()
                                .map(|m| m.file_path.clone().into())
                                .collect(),
                        ),
                    );
                    let bound_id: String = sha256(json::stringify(&bound).as_bytes())
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    if let Some(Value::Array(accepted)) = report.get_mut("accepted") {
                        for row in accepted.iter_mut().filter(|row| {
                            row.get("id").and_then(Value::as_str) == Some(candidate.id.as_str())
                        }) {
                            row.insert("id", bound_id.clone());
                            row.insert("metadata_resolved", true);
                        }
                    }
                    candidate.id = bound_id;
                    decision.insert("candidate_id", candidate.id.clone());
                    decision.insert("status", "mapped");
                    report.insert("selected_candidate_id", candidate.id.clone());
                    report.insert("torrent_id", metadata.id.clone());
                    report.insert(
                        "mapping",
                        Value::Array(
                            mappings
                                .iter()
                                .map(|mapping| {
                                    let episode = scope
                                        .episodes
                                        .iter()
                                        .find(|e| e.episode == mapping.episode)
                                        .ok_or("Mapped target is absent from the captured scope")?;
                                    let mut row = public_episode(episode);
                                    row.insert(
                                        "file_path",
                                        integrations::report_text(&mapping.file_path, 4096),
                                    );
                                    Ok(row)
                                })
                                .collect::<Result<Vec<_>>>()?,
                        ),
                    );
                    selected = Some((candidate, metadata, mappings));
                }
                Err(error) => {
                    decision.insert("status", "unresolved");
                    decision.insert("reason", integrations::report_text(&error, 512));
                }
            }
            decisions.push(decision);
            if selected.is_some() {
                break;
            }
        }
        report.insert("metadata_decisions", Value::Array(decisions));
        // Recheck catalog content as well as revision: refreshes can change a plan without settings changes.
        {
            let series = lock(&self.series_store)?;
            let current = series.get(id).ok_or("Unknown tracked series")?;
            let jobs = lock(&self.store)?;
            if capture_scope(&current, &jobs, query.season)?.id != scope.id
                || self.stopped.load(Ordering::Acquire)
                || Instant::now() >= deadline
            {
                return Err(
                    "Pack catalog or request scope changed; search result discarded".into(),
                );
            }
        }
        if let Some(expected) = &query.candidate_id
            && selected
                .as_ref()
                .is_none_or(|(candidate, _, _)| &candidate.id != expected)
        {
            return Err("Pack preview candidate changed; search again before acquiring".into());
        }
        if query.apply && !scope.episodes.is_empty() {
            let (candidate, metadata, episodes) = selected
                .ok_or("No pack resolves every requested episode within the metadata limits")?;
            let origin = PackOrigin {
                series_id: id.into(),
                series_revision: scope.record.revision,
                season: query.season,
                scope_id: scope.id,
                candidate_id: candidate.id,
                torrent_id: metadata.id,
                release: RecordedRelease {
                    title: candidate.title,
                    profile: candidate.profile,
                },
            };
            let submission = self.submit_pack_with_origin(
                id,
                &PackSubmission {
                    source_url: candidate.url,
                    episodes,
                },
                Some(&origin),
            )?;
            report.insert("submission", submission);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::Plan;
    use crate::torrent::MetadataFile;

    fn scope() -> Scope {
        let request = Request {
            kind: "series".into(),
            title: "Fixture Series".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_numbering: None,
            tmdb_id: Some(42),
            source_url: None,
            source_path: None,
        };
        let episodes: Vec<_> = (1..=3)
            .map(|number| Episode {
                catalog_id: Some(u64::from(number)),
                season: 1,
                episode: number,
                title: "Catalog title".into(),
                air_date: Some("2024-01-01".into()),
            })
            .collect();
        Scope {
            id: "a".repeat(64),
            episodes: episodes[..2].to_vec(),
            record: Record {
                id: "b".repeat(32),
                anchors: episodes
                    .iter()
                    .filter_map(|ep| {
                        ep.catalog_id.map(|id| {
                            (
                                id,
                                crate::numbering::EpisodeNumber {
                                    season: ep.season,
                                    episode: ep.episode,
                                },
                            )
                        })
                    })
                    .collect(),
                numbering: BTreeMap::new(),
                plan: Plan { request, episodes },
                monitored: false,
                include_specials: false,
                start_date: None,
                excluded: BTreeSet::new(),
                revision: 1,
                created_at: 1,
                updated_at: 1,
                checked_at: 1,
                next_check_at: 1,
                last_error: None,
            },
        }
    }
    fn metadata(names: &[&str]) -> TorrentMetadata {
        TorrentMetadata {
            id: "a".repeat(40),
            files: names
                .iter()
                .map(|path| MetadataFile {
                    path: (*path).into(),
                    length: 1,
                    padding: false,
                })
                .collect(),
        }
    }

    #[test]
    fn season_pack_identity_does_not_loosen_movie_or_episode_identity() {
        let mut request = scope().record.plan.request;
        request.season = 1;
        for title in [
            "Fixture.Series.S01.1080p",
            "Fixture.Series.2024.S01.Complete",
            "Fixture Series Season 1 WEB-DL",
            "Fixture.Series.S1",
        ] {
            assert!(season_title_matches(&request, title), "Rejected {title}");
        }
        for title in [
            "Fixture.Series.Sequel.S01",
            "Other.Series.S01",
            "Fixture.Series.2023.S01",
            "Fixture.Series.S02",
            "Fixture.Series.S01.S02",
            "Fixture.Series.S01-S03",
            "Fixture.Series.S01E01",
            "Fixture.Series.S01.E01",
            "Fixture.Series.S01.1x02",
            "Fixture.Series.Season.1.Season.2",
            "Fixture.Series.1080p",
            "Fixture.Series.S10000",
        ] {
            assert!(!season_title_matches(&request, title), "Accepted {title}");
        }
    }

    #[test]
    fn automatic_mapping_requires_unique_explicit_episode_numbers_and_complete_coverage() {
        let scope = scope();
        let result = map_metadata(
            &scope,
            &metadata(&[
                "Pack/S01E01.mp4",
                "Pack/Fixture.Series.1x02.mkv",
                "Pack/S01E03.mp4",
                "Pack/sample.mp4",
                "Pack/extras/trailer.mp4",
                "Pack/info.txt",
            ]),
            1,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].episode, 2);
        for bad in [
            "Pack/001.mp4",
            "Pack/S01E01E02.mp4",
            "Pack/S01E01-E02.mp4",
            "Pack/S01E01.S01E01.mp4",
            "Pack/Other.Series.S01E01.mp4",
            "Pack/S02E01.mp4",
            "Pack/S01E99.mp4",
            "Pack/S01E100000.mp4",
            "Pack/S10000E01.mp4",
            "Pack/S01E00.mp4",
            "Pack/S02/S01E01.mp4",
            "Pack/../S01E01.mp4",
            "Pack/S01E01.mp4/token",
        ] {
            assert!(
                map_metadata(&scope, &metadata(&[bad, "Pack/S01E02.mp4"]), 1).is_err(),
                "Accepted {bad}"
            );
        }
        assert!(map_metadata(&scope, &metadata(&["Pack/S01E01.mp4"]), 1).is_err());
        assert!(
            map_metadata(
                &scope,
                &metadata(&["Pack/S01E01.mp4", "Pack/S01E02.mp4", "Pack/S01E01.mkv"]),
                1
            )
            .is_err()
        );
        let mut empty = metadata(&["Pack/S01E01.mp4", "Pack/S01E02.mp4"]);
        empty.files[0].length = 0;
        assert!(map_metadata(&scope, &empty, 1).is_err());
    }

    #[test]
    fn strict_pack_queries_and_provenance_reject_ambiguous_types_and_partial_guards() {
        for text in [
            r#"{}"#,
            r#"{"season":1.5}"#,
            r#"{"season":-1}"#,
            r#"{"season":10000}"#,
            r#"{"season":"1"}"#,
            r#"{"season":1,"apply":"true"}"#,
            r#"{"season":1,"scope_id":"abc"}"#,
            r#"{"season":1,"unexpected":true}"#,
        ] {
            assert!(
                AutoPackRequest::from_json(&json::parse(text).unwrap()).is_err(),
                "Accepted {text}"
            );
        }
        let mut query = AutoPackRequest {
            season: 1,
            apply: true,
            scope_id: Some("a".repeat(64)),
            candidate_id: None,
        };
        assert!(query.validate().is_err());
        query.candidate_id = Some("b".repeat(64));
        assert!(query.validate().is_ok());
        query.apply = false;
        assert!(query.validate().is_err());
        let origin = PackOrigin {
            series_id: "a".repeat(32),
            series_revision: u64::MAX,
            season: 1,
            scope_id: "b".repeat(64),
            candidate_id: "c".repeat(64),
            torrent_id: "d".repeat(40),
            release: RecordedRelease {
                title: "Fixture.Series.S01".into(),
                profile: "any".into(),
            },
        };
        assert_eq!(PackOrigin::from_json(&origin.to_json()).unwrap(), origin);
        let mut bad = origin.to_json();
        bad.insert("extra", true);
        assert!(PackOrigin::from_json(&bad).is_err());
    }
}
