//! Guarded numbering decisions retain canonical identities before catalog refresh.
use super::{MAX_EPISODES, Plan, Record};
use crate::{
    Result,
    crypto::sha256,
    engine::{Engine, lock},
    integrations,
    json::{self, Value},
    numbering::{EpisodeNumber, SourceNumber, only},
    store,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberingChoice {
    pub catalog_id: u64,
    pub catalog: EpisodeNumber,
    pub source: SourceNumber,
}

impl NumberingChoice {
    pub fn to_json(self) -> Value {
        let mut value = Value::object();
        value.insert("catalog_id", Value::Number(self.catalog_id as f64));
        value.insert("catalog", self.catalog.to_json());
        value.insert("source", self.source.to_json());
        value
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        only(value, &["catalog_id", "catalog", "source"])?;
        let result = Self {
            catalog_id: catalog_id(value)?,
            catalog: EpisodeNumber::from_json(
                value.get("catalog").ok_or("Missing catalog number")?,
            )?,
            source: SourceNumber::from_json(value.get("source").ok_or("Missing source number")?)?,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(self) -> Result<()> {
        if self.catalog_id == 0 || self.catalog_id > 9_007_199_254_740_991 {
            return Err("Invalid numbering catalog identity".into());
        }
        self.catalog.validate()?;
        self.source.validate()
    }
}

#[derive(Clone, Debug)]
pub struct NumberingRequest {
    pub changes: Vec<NumberingChoice>,
    pub apply: bool,
    pub plan_id: Option<String>,
}

impl NumberingRequest {
    pub fn from_json(value: &Value) -> Result<Self> {
        only(value, &["changes", "apply", "plan_id"])?;
        let changes = value
            .get("changes")
            .and_then(Value::as_array)
            .ok_or("Numbering changes must be an array")?;
        if changes.len() > MAX_EPISODES {
            return Err("Numbering decisions allow at most 2000 changes".into());
        }
        let result = Self {
            changes: changes
                .iter()
                .map(NumberingChoice::from_json)
                .collect::<Result<_>>()?,
            apply: value.get("apply").map_or(Ok(false), |v| {
                v.as_bool().ok_or("Numbering apply must be a boolean")
            })?,
            plan_id: match value.get("plan_id") {
                None => None,
                Some(Value::String(id)) => Some(id.clone()),
                _ => return Err("Invalid numbering plan identity".into()),
            },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        if self.changes.len() > MAX_EPISODES
            || self.apply != self.plan_id.is_some()
            || self.plan_id.as_ref().is_some_and(|id| {
                id.len() != 64 || !id.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
            })
        {
            return Err("Numbering apply requires the exact 64-character preview plan ID".into());
        }
        let mut ids = BTreeSet::new();
        for choice in &self.changes {
            choice.validate()?;
            if !ids.insert(choice.catalog_id) {
                return Err("Duplicate numbering catalog identity".into());
            }
        }
        Ok(())
    }
}

pub(super) fn catalog_id(value: &Value) -> Result<u64> {
    value
        .get("catalog_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0 && *id <= 9_007_199_254_740_991)
        .ok_or_else(|| "Invalid numbering catalog identity".into())
}

pub(super) fn initial_anchors(plan: &Plan) -> BTreeMap<u64, EpisodeNumber> {
    plan.episodes
        .iter()
        .filter_map(|ep| {
            ep.catalog_id.map(|id| {
                (
                    id,
                    EpisodeNumber {
                        season: ep.season,
                        episode: ep.episode,
                    },
                )
            })
        })
        .collect()
}

impl Record {
    pub(super) fn validate_numbering(&self) -> Result<()> {
        if self.anchors.len() > MAX_EPISODES || self.numbering.len() > MAX_EPISODES {
            return Err("Series numbering history exceeds 2000 identities".into());
        }
        let mut canonical = BTreeSet::new();
        let mut source = BTreeSet::new();
        let mut catalog = BTreeSet::new();
        for (id, number) in &self.anchors {
            if *id == 0 || *id > 9_007_199_254_740_991 || !canonical.insert(*number) {
                return Err("Invalid or conflicting retained canonical episode identity".into());
            }
            number.validate()?;
            let choice = self.numbering.get(id);
            if let Some(choice) = choice {
                choice.validate()?;
                if choice.catalog_id != *id {
                    return Err("Numbering choice identity mismatch".into());
                }
            }
            if !source.insert(choice.map_or(SourceNumber::SeasonEpisode(*number), |c| c.source))
                || !catalog.insert(choice.map_or(*number, |c| c.catalog))
            {
                return Err("Numbering choices must have unique catalog and source labels".into());
            }
        }
        if self
            .numbering
            .keys()
            .any(|id| !self.anchors.contains_key(id))
        {
            return Err("Numbering choice requires a retained catalog identity".into());
        }
        for ep in &self.plan.episodes {
            let number = EpisodeNumber {
                season: ep.season,
                episode: ep.episode,
            };
            if let Some(id) = ep.catalog_id {
                if self.anchors.get(&id) != Some(&number) {
                    return Err("Catalog plan changed a retained canonical episode identity".into());
                }
            } else if canonical.contains(&number) {
                return Err("Unresolved episode replaced a retained catalog identity".into());
            }
        }
        Ok(())
    }

    pub(super) fn normalize_catalog(&self, mut plan: Plan) -> Result<Plan> {
        plan.validate()?;
        if plan.request.tmdb_id != self.plan.request.tmdb_id {
            return Err("Catalog series identity changed".into());
        }
        let owners: BTreeMap<_, _> = self
            .anchors
            .iter()
            .map(|(id, number)| (*number, *id))
            .collect();
        for ep in &mut plan.episodes {
            let observed = EpisodeNumber {
                season: ep.season,
                episode: ep.episode,
            };
            if let Some(id) = ep.catalog_id
                && let Some(canonical) = self.anchors.get(&id)
            {
                let approved = self.numbering.get(&id).map_or(*canonical, |c| c.catalog);
                if observed != approved {
                    return Err("Catalog episode identity or numbering changed; an explicit mapping decision is required".into());
                }
                ep.season = canonical.season;
                ep.episode = canonical.episode;
            } else if owners.contains_key(&observed) {
                return Err("Catalog episode identity changed; an explicit mapping decision is required; retained library numbers cannot be reassigned".into());
            }
        }
        let scope = &self.plan.request;
        plan.episodes.retain(|ep| {
            (ep.season != 0 || self.include_specials)
                && (scope.season == 0 || scope.season == ep.season)
                && (scope.episode == 0 || scope.episode == ep.episode)
        });
        plan.request.season = scope.season;
        plan.request.episode = scope.episode;
        plan.episodes.sort_by_key(|ep| (ep.season, ep.episode));
        plan.validate()?;
        Ok(plan)
    }

    pub(super) fn accept_catalog(&mut self, plan: Plan) -> Result<()> {
        for (id, number) in initial_anchors(&plan) {
            if self.anchors.get(&id).is_some_and(|old| *old != number) {
                return Err("Canonical numbering cannot change".into());
            }
            self.anchors.insert(id, number);
        }
        self.plan = plan;
        self.validate()
    }

    pub(super) fn fetch_catalog(
        &self,
        engine: &Engine,
        full: bool,
        deadline: Instant,
    ) -> Result<Plan> {
        let mut request = self.plan.request.clone();
        let full = full
            || self
                .numbering
                .values()
                .any(|choice| self.anchors.get(&choice.catalog_id) != Some(&choice.catalog));
        if full {
            request.season = 0;
            request.episode = 0;
        }
        integrations::series_plan_before(
            &engine.config,
            &request,
            full || self.include_specials,
            deadline,
        )
    }
}

impl Engine {
    pub fn series_numbering(&self, id: &str, query: &NumberingRequest) -> Result<Value> {
        query.validate()?;
        if query.apply && self.read_only {
            return Err("Numbering changes require writable storage".into());
        }
        let _refresh = self
            .series_refresh_lock
            .try_lock()
            .map_err(|_| "A series refresh is already running")?;
        let deadline = Instant::now() + Duration::from_secs(90);
        let previous = lock(&self.series_store)?
            .get(id)
            .ok_or("Unknown monitored series")?;
        if query
            .changes
            .iter()
            .any(|choice| !previous.anchors.contains_key(&choice.catalog_id))
        {
            return Err("Numbering changes require an already known catalog identity".into());
        }
        let fetched = previous.fetch_catalog(self, true, deadline)?;
        let observed_numbers: BTreeMap<_, _> = fetched
            .episodes
            .iter()
            .filter_map(|ep| {
                ep.catalog_id.map(|id| {
                    (
                        id,
                        EpisodeNumber {
                            season: ep.season,
                            episode: ep.episode,
                        },
                    )
                })
            })
            .collect();
        let mut proposed = previous.clone();
        for choice in &query.changes {
            if !previous.anchors.contains_key(&choice.catalog_id) {
                return Err("Numbering changes require an already known catalog identity".into());
            }
            let observed = observed_numbers
                .get(&choice.catalog_id)
                .ok_or("Numbering choice is absent from the current catalog")?;
            if choice.catalog != *observed {
                return Err("Numbering choice does not match the current catalog labels".into());
            }
            proposed.numbering.insert(choice.catalog_id, *choice);
        }
        // The proposal may be unresolved, but no rejected proposal is persisted.
        let normalized = proposed
            .validate_numbering()
            .and_then(|()| proposed.normalize_catalog(fetched.clone()))
            .and_then(|plan| proposed.accept_catalog(plan));
        let mut fingerprint = Value::object();
        fingerprint.insert("previous", previous.to_json());
        fingerprint.insert("catalog_request", fetched.request.to_json());
        fingerprint.insert(
            "catalog_episodes",
            Value::Array(
                fetched
                    .episodes
                    .iter()
                    .map(super::Episode::to_json)
                    .collect(),
            ),
        );
        let mut choices = query.changes.clone();
        choices.sort_by_key(|choice| choice.catalog_id);
        fingerprint.insert(
            "changes",
            Value::Array(choices.iter().map(|c| c.to_json()).collect()),
        );
        let plan_id = super::hex(&sha256(json::stringify(&fingerprint).as_bytes()));
        let mut series = lock(&self.series_store)?;
        let current = series.get(id).ok_or("Unknown monitored series")?;
        if current != previous || self.stopped.load(Ordering::Acquire) || Instant::now() >= deadline
        {
            return Err(
                "Series or catalog changed during numbering preview; result discarded".into(),
            );
        }
        let mut report = Value::object();
        report.insert("series_id", id);
        report.insert("revision", previous.revision.to_string());
        report.insert("plan_id", plan_id.clone());
        report.insert("resolved", normalized.is_ok());
        report.insert("applied", false);
        report.insert(
            "changes",
            Value::Array(choices.iter().map(|c| c.to_json()).collect()),
        );
        report.insert(
            "issues",
            Value::Array(
                normalized
                    .as_ref()
                    .err()
                    .map(|error| Value::String(integrations::report_text(error, 2048)))
                    .into_iter()
                    .collect(),
            ),
        );
        let rows = fetched
            .episodes
            .iter()
            .filter(|ep| {
                ep.catalog_id
                    .is_some_and(|id| previous.anchors.contains_key(&id))
            })
            .map(|ep| {
                let id = ep.catalog_id.expect("filtered catalog identity");
                let mut row = Value::object();
                row.insert("catalog_id", Value::Number(id as f64));
                row.insert("title", integrations::report_text(&ep.title, 2048));
                row.insert("canonical", previous.anchors[&id].to_json());
                row.insert(
                    "catalog",
                    EpisodeNumber {
                        season: ep.season,
                        episode: ep.episode,
                    }
                    .to_json(),
                );
                row.insert(
                    "source",
                    proposed
                        .numbering
                        .get(&id)
                        .map_or(SourceNumber::SeasonEpisode(previous.anchors[&id]), |c| {
                            c.source
                        })
                        .to_json(),
                );
                row
            })
            .collect();
        report.insert("episodes", Value::Array(rows));
        if query.apply {
            if query.plan_id.as_ref() != Some(&plan_id) {
                return Err("Numbering preview is stale; preview again before applying".into());
            }
            normalized?;
            proposed.revision = previous
                .revision
                .checked_add(1)
                .ok_or("Series revision overflow")?;
            proposed.updated_at = store::now();
            proposed.checked_at = proposed.updated_at;
            proposed.next_check_at = 0;
            proposed.last_error = None;
            if Instant::now() >= deadline {
                return Err("Numbering decision exceeded its deadline; preview again".into());
            }
            series.save(proposed)?;
            report.insert("applied", true);
        }
        Ok(report)
    }
}
