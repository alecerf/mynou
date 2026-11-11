//! Bounded release-title policies. Title markers are hints, not verified media tracks.
use std::collections::{BTreeMap, BTreeSet};

use crate::{Result, json::Value};

const MAX_PROFILES: usize = 64;
const MAX_ENTRIES: usize = 32;
const MAX_TERM_BYTES: usize = 128;
const MAX_TITLE_BYTES: usize = 4096;
const MAX_SCORE: i32 = 100_000;
const RESOLUTIONS: &[u32] = &[480, 576, 720, 1080, 2160];
const SOURCES: &[&str] = &["cam", "dvd", "hdtv", "webrip", "web-dl", "bluray", "remux"];
const CODECS: &[&str] = &["h264", "h265", "av1", "vp9"];
/// `vostfr` is the original audio with French subtitles; `original` is the
/// title's own language, which TMDB gives title by title.
const LANGUAGES: &[&str] = &[
    "en", "fr", "vff", "vfq", "vfi", "vostfr", "de", "es", "it", "ja", "ko", "zh", "pt", "ru",
    ORIGINAL,
];
/// The profile language resolved per title from TMDB's original language.
pub const ORIGINAL: &str = "original";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionConfig {
    pub profiles: BTreeMap<String, Profile>,
    pub movie_profile: String,
    pub episode_profile: String,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::from([("any".into(), Profile::default())]),
            movie_profile: "any".into(),
            episode_profile: "any".into(),
        }
    }
}

impl SelectionConfig {
    /// Presets for newly generated configuration files.
    pub fn fresh() -> Self {
        let mut profiles = BTreeMap::new();
        for (name, label, resolutions, cutoff_source, max_mb_per_hour) in [
            (
                "compact-1080p",
                "Compact 1080p",
                vec![1080, 720],
                "web-dl",
                Some(2500),
            ),
            (
                "balanced-1080p",
                "Balanced 1080p",
                vec![1080, 720],
                "bluray",
                Some(5000),
            ),
            ("cinema-4k", "Cinema 4K", vec![2160, 1080], "remux", None),
        ] {
            let cinema = name == "cinema-4k";
            profiles.insert(
                name.into(),
                Profile {
                    label: Some(label.into()),
                    cutoff_resolution: Some(resolutions[0]),
                    resolutions,
                    cutoff_source: Some(cutoff_source.into()),
                    sources: if cinema {
                        vec!["remux", "bluray", "web-dl", "webrip", "hdtv", "dvd"]
                    } else {
                        vec!["bluray", "web-dl", "webrip", "hdtv", "dvd"]
                    }
                    .into_iter()
                    .map(String::from)
                    .collect(),
                    codecs: CODECS.iter().map(|s| (*s).into()).collect(),
                    language_mode: Some(LanguageMode::Any),
                    allow_unknown_source: true,
                    allow_unknown_codec: true,
                    blocked_terms: vec!["3d".into()],
                    max_mb_per_hour,
                    score_rules: if cinema {
                        vec![
                            ScoreRule {
                                terms: vec!["hdr".into()],
                                score: 10,
                            },
                            ScoreRule {
                                terms: vec!["dolby vision".into()],
                                score: 10,
                            },
                        ]
                    } else {
                        Vec::new()
                    },
                    ..Profile::default()
                },
            );
        }
        Self {
            profiles,
            movie_profile: "balanced-1080p".into(),
            episode_profile: "balanced-1080p".into(),
        }
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        expect_object(value)?;
        let mut config = Self::default();
        if let Some(value) = value.get("profiles") {
            let profiles = value
                .as_object()
                .ok_or("Selection: profiles must be an object")?;
            if profiles.is_empty() || profiles.len() > MAX_PROFILES {
                return Err("Selection: profiles must contain between 1 and 64 entries".into());
            }
            config.profiles.clear();
            for (name, value) in profiles {
                validate_profile_name(name)?;
                config
                    .profiles
                    .insert(name.clone(), Profile::from_json(value)?);
            }
        }
        config.movie_profile = profile_name(value, "movie_profile", "any")?;
        config.episode_profile = profile_name(value, "episode_profile", "any")?;
        config.profile("movie")?;
        config.profile("episode")?;
        Ok(config)
    }

    pub fn to_json(&self) -> Value {
        object([
            (
                "profiles",
                Value::Object(
                    self.profiles
                        .iter()
                        .map(|(name, profile)| (name.clone(), profile.to_json()))
                        .collect(),
                ),
            ),
            ("movie_profile", self.movie_profile.clone().into()),
            ("episode_profile", self.episode_profile.clone().into()),
        ])
    }

    pub fn profile(&self, kind: &str) -> Result<(&str, &Profile)> {
        let name = match kind {
            "movie" => &self.movie_profile,
            "episode" => &self.episode_profile,
            _ => return Err(format!("Selection: unsupported media kind {kind}")),
        };
        self.profiles
            .get(name)
            .map(|profile| (name.as_str(), profile))
            .ok_or_else(|| format!("Selection: unknown profile {name}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageMode {
    Any,
    Prefer,
    Require,
}

impl LanguageMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Prefer => "prefer",
            Self::Require => "require",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub label: Option<String>,
    pub cutoff_source: Option<String>,
    /// None derives the mode from the legacy language fields.
    pub language_mode: Option<LanguageMode>,
    pub min_mb_per_hour: Option<u32>,
    pub max_mb_per_hour: Option<u32>,
    /// Ordered from most preferred to least preferred; an empty list is unrestricted.
    pub resolutions: Vec<u32>,
    /// The upgrade target also requires the cutoff source and preferred language.
    pub cutoff_resolution: Option<u32>,
    pub sources: Vec<String>,
    pub codecs: Vec<String>,
    /// Every language a release must have; the first ones count most when a
    /// release has only some of them. `fr` matches any French audio; variants
    /// match exactly; `original` is the title's own audio. The language
    /// decides before the quality.
    pub languages: Vec<String>,
    pub allow_unknown_resolution: bool,
    pub allow_unknown_source: bool,
    pub allow_unknown_codec: bool,
    pub allow_unknown_language: bool,
    pub required_terms: Vec<String>,
    pub blocked_terms: Vec<String>,
    pub score_rules: Vec<ScoreRule>,
    pub minimum_score: i32,
    pub min_size_mb: Option<u32>,
    pub max_size_mb: Option<u32>,
    pub allow_unknown_size: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            label: None,
            cutoff_source: None,
            language_mode: None,
            min_mb_per_hour: None,
            max_mb_per_hour: None,
            resolutions: Vec::new(),
            cutoff_resolution: None,
            sources: Vec::new(),
            codecs: Vec::new(),
            languages: Vec::new(),
            allow_unknown_resolution: false,
            allow_unknown_source: false,
            allow_unknown_codec: false,
            allow_unknown_language: false,
            required_terms: Vec::new(),
            blocked_terms: Vec::new(),
            score_rules: Vec::new(),
            minimum_score: 0,
            min_size_mb: None,
            max_size_mb: None,
            allow_unknown_size: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoreRule {
    /// Every token phrase must occur for this rule to contribute its score.
    pub terms: Vec<String>,
    pub score: i32,
}

impl Profile {
    pub fn from_json(value: &Value) -> Result<Self> {
        expect_object(value)?;
        let size_limit = |field: &str| -> Result<Option<u32>> {
            match value.get(field) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .filter(|v| (1..=1_000_000).contains(v))
                    .map(|v| Some(v as u32))
                    .ok_or_else(|| {
                        format!("Selection: {field} must be null or an integer from 1 to 1000000")
                    }),
            }
        };
        let min_size_mb = size_limit("min_size_mb")?;
        let max_size_mb = size_limit("max_size_mb")?;
        if min_size_mb
            .zip(max_size_mb)
            .is_some_and(|(min, max)| min > max)
        {
            return Err("Selection: min_size_mb must not exceed max_size_mb".into());
        }
        let min_mb_per_hour = size_limit("min_mb_per_hour")?;
        let max_mb_per_hour = size_limit("max_mb_per_hour")?;
        if min_mb_per_hour
            .zip(max_mb_per_hour)
            .is_some_and(|(min, max)| min > max)
        {
            return Err("Selection: min_mb_per_hour must not exceed max_mb_per_hour".into());
        }
        let label = match value.get("label") {
            None => None,
            Some(Value::String(label))
                if (1..=64).contains(&label.chars().count())
                    && !label.trim().is_empty()
                    && !label.chars().any(char::is_control) =>
            {
                Some(label.clone())
            }
            _ => {
                return Err(
                    "Selection: label must contain 1 to 64 characters without controls".into(),
                );
            }
        };
        let cutoff_source = match value.get("cutoff_source") {
            None | Some(Value::Null) => None,
            Some(Value::String(source)) if SOURCES.contains(&source.as_str()) => {
                Some(source.clone())
            }
            _ => return Err("Selection: unsupported cutoff source".into()),
        };
        let language_mode = match value.get("language_mode") {
            None => None,
            Some(Value::String(mode)) => Some(match mode.as_str() {
                "any" => LanguageMode::Any,
                "prefer" => LanguageMode::Prefer,
                "require" => LanguageMode::Require,
                _ => return Err("Selection: unsupported language mode".into()),
            }),
            _ => return Err("Selection: language_mode must be any, prefer or require".into()),
        };
        let mut resolutions = Vec::new();
        for value in list(value, "resolutions")? {
            let resolution = value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| RESOLUTIONS.contains(v))
                .ok_or("Selection: unsupported resolution")?;
            if resolutions.contains(&resolution) {
                return Err("Selection: duplicate resolution".into());
            }
            resolutions.push(resolution);
        }
        let cutoff_resolution = match value.get("cutoff_resolution") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let cutoff = value
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| RESOLUTIONS.contains(v))
                    .ok_or("Selection: unsupported cutoff resolution")?;
                if !resolutions.contains(&cutoff) {
                    return Err(
                        "Selection: cutoff resolution must appear in the ordered resolutions list"
                            .into(),
                    );
                }
                Some(cutoff)
            }
        };
        let mut score_rules = Vec::new();
        let mut seen_rules = BTreeSet::new();
        for rule in list(value, "score_rules")? {
            expect_object(rule)?;
            let terms = terms(rule, "terms")?;
            if terms.is_empty() {
                return Err("Selection: score rules must contain at least one term".into());
            }
            let score = signed(rule, "score", None)?;
            let mut identity: Vec<_> = terms.iter().map(|term| tokens(term)).collect();
            identity.sort();
            if !seen_rules.insert(identity) {
                return Err("Selection: duplicate score rule terms".into());
            }
            score_rules.push(ScoreRule { terms, score });
        }
        Ok(Self {
            label,
            cutoff_source,
            language_mode,
            min_mb_per_hour,
            max_mb_per_hour,
            resolutions,
            cutoff_resolution,
            min_size_mb,
            max_size_mb,
            allow_unknown_size: match value.get("allow_unknown_size") {
                None => true,
                Some(v) => v
                    .as_bool()
                    .ok_or("Selection: allow_unknown_size must be a boolean")?,
            },
            sources: choices(value, "sources", SOURCES)?,
            codecs: choices(value, "codecs", CODECS)?,
            languages: choices(value, "languages", LANGUAGES)?,
            allow_unknown_resolution: boolean(value, "allow_unknown_resolution")?,
            allow_unknown_source: boolean(value, "allow_unknown_source")?,
            allow_unknown_codec: boolean(value, "allow_unknown_codec")?,
            allow_unknown_language: boolean(value, "allow_unknown_language")?,
            required_terms: terms(value, "required_terms")?,
            blocked_terms: terms(value, "blocked_terms")?,
            score_rules,
            minimum_score: signed(value, "minimum_score", Some(0))?,
        })
    }

    pub fn to_json(&self) -> Value {
        let mut value = object([
            (
                "resolutions",
                Value::Array(self.resolutions.iter().copied().map(Value::from).collect()),
            ),
            (
                "cutoff_resolution",
                self.cutoff_resolution
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            ),
            (
                "cutoff_source",
                self.cutoff_source
                    .clone()
                    .map_or(Value::Null, Value::String),
            ),
            (
                "min_mb_per_hour",
                self.min_mb_per_hour.map_or(Value::Null, Value::from),
            ),
            (
                "max_mb_per_hour",
                self.max_mb_per_hour.map_or(Value::Null, Value::from),
            ),
            ("sources", strings(&self.sources)),
            ("codecs", strings(&self.codecs)),
            ("languages", strings(&self.languages)),
            (
                "allow_unknown_resolution",
                self.allow_unknown_resolution.into(),
            ),
            ("allow_unknown_source", self.allow_unknown_source.into()),
            ("allow_unknown_codec", self.allow_unknown_codec.into()),
            ("allow_unknown_language", self.allow_unknown_language.into()),
            ("required_terms", strings(&self.required_terms)),
            ("blocked_terms", strings(&self.blocked_terms)),
            (
                "score_rules",
                Value::Array(
                    self.score_rules
                        .iter()
                        .map(|rule| {
                            object([
                                ("terms", strings(&rule.terms)),
                                ("score", number(rule.score)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("minimum_score", number(self.minimum_score)),
            (
                "min_size_mb",
                self.min_size_mb.map_or(Value::Null, Value::from),
            ),
            (
                "max_size_mb",
                self.max_size_mb.map_or(Value::Null, Value::from),
            ),
            ("allow_unknown_size", self.allow_unknown_size.into()),
        ]);
        if let Some(label) = &self.label {
            value.insert("label", label.clone());
        }
        if let Some(mode) = self.language_mode {
            value.insert("language_mode", mode.as_str());
        }
        value
    }

    pub fn effective_language_mode(&self) -> LanguageMode {
        self.language_mode.unwrap_or({
            if self.languages.is_empty() {
                LanguageMode::Any
            } else if self.allow_unknown_language {
                LanguageMode::Prefer
            } else {
                LanguageMode::Require
            }
        })
    }

    /// Whether the title's original language decides: `original` is among
    /// the languages and they count.
    pub fn uses_original_language(&self) -> bool {
        self.effective_language_mode() != LanguageMode::Any
            && self.languages.iter().any(|language| language == ORIGINAL)
    }

    pub fn cutoff_reached(&self, assessment: &Assessment) -> bool {
        if !assessment.accepted {
            return false;
        }
        let cutoff = self
            .cutoff_resolution
            .and_then(|cutoff| self.resolutions.iter().position(|value| *value == cutoff));
        let current = assessment.attributes.resolution.and_then(|resolution| {
            self.resolutions
                .iter()
                .position(|value| *value == resolution)
        });
        // Upgrades go on until a release has every language of the profile.
        if self.effective_language_mode() != LanguageMode::Any
            && !self.languages.iter().all(|target| {
                if target == ORIGINAL {
                    assessment.original.as_ref().is_some_and(|o| o.audio)
                } else {
                    matches_language(target, &assessment.attributes.languages)
                }
            })
        {
            return false;
        }
        match (current, cutoff) {
            (Some(current), Some(cutoff)) => {
                current < cutoff
                    || (current == cutoff
                        && self.cutoff_source.as_ref().is_none_or(|target| {
                            let Some(source) = assessment.attributes.source.as_ref() else {
                                return false;
                            };
                            if let Some(target) =
                                self.sources.iter().position(|value| value == target)
                            {
                                self.sources
                                    .iter()
                                    .position(|value| value == source)
                                    .is_some_and(|current| current <= target)
                            } else {
                                let source_rank = |source: &str| {
                                    SOURCES.iter().position(|value| *value == source)
                                };
                                source_rank(source)
                                    .zip(source_rank(target))
                                    .is_some_and(|(current, target)| current >= target)
                            }
                        }))
            }
            _ => false,
        }
    }

    /// Without the title's facts, as for an announcement not identified yet:
    /// `original` is left to the check made once the title is known.
    pub fn assess(&self, release_title: &str, media_title: &str) -> Assessment {
        self.assess_with_size(release_title, media_title, None, Some(1))
    }

    /// Limits are decimal MB per movie or episode. `original` is not judged.
    pub fn assess_with_size(
        &self,
        release_title: &str,
        media_title: &str,
        size: Option<u64>,
        episodes: Option<u32>,
    ) -> Assessment {
        self.assess_release(
            release_title,
            media_title,
            size,
            episodes,
            None,
            Original::Unjudged,
        )
    }

    /// A release of a title whose original language is `original_language`,
    /// without its size; see [`Self::assess_with_runtime`].
    pub fn assess_for(
        &self,
        release_title: &str,
        media_title: &str,
        original_language: Option<&str>,
    ) -> Assessment {
        self.assess_with_runtime(
            release_title,
            media_title,
            None,
            Some(1),
            None,
            original_language,
        )
    }

    /// Runtime is the total duration covered by the release.
    /// `original_language` is TMDB's code for the title's own language; when
    /// it is unknown, `original` counts only VO, MULTi and VOSTFR.
    pub fn assess_with_runtime(
        &self,
        release_title: &str,
        media_title: &str,
        size: Option<u64>,
        episodes: Option<u32>,
        runtime_minutes: Option<u32>,
        original_language: Option<&str>,
    ) -> Assessment {
        self.assess_release(
            release_title,
            media_title,
            size,
            episodes,
            runtime_minutes,
            Original::Title(original_language),
        )
    }

    fn assess_release(
        &self,
        release_title: &str,
        media_title: &str,
        size: Option<u64>,
        episodes: Option<u32>,
        runtime_minutes: Option<u32>,
        original: Original<'_>,
    ) -> Assessment {
        let mut assessment = self.assess_title(release_title, media_title, original);
        let hourly = runtime_minutes
            .filter(|minutes| *minutes > 0)
            .filter(|_| self.min_mb_per_hour.is_some() || self.max_mb_per_hour.is_some());
        let (minimum, maximum, units, scale) = if let Some(minutes) = hourly {
            (
                self.min_mb_per_hour,
                self.max_mb_per_hour,
                Some(minutes),
                60_u128,
            )
        } else {
            (
                self.min_size_mb,
                self.max_size_mb,
                episodes.filter(|count| *count > 0),
                1,
            )
        };
        let size = size.filter(|size| *size > 0);
        if !self.allow_unknown_size && size.is_none() {
            assessment.reasons.push("Release size is unknown".into());
        }
        if episodes.filter(|n| *n > 0).is_none() && maximum.is_some() && !self.allow_unknown_size {
            assessment
                .reasons
                .push("Pack episode count is unknown".into());
        }
        match units {
            None if maximum.is_some() && !self.allow_unknown_size => {}
            None if minimum.is_some() || maximum.is_some() => {
                assessment
                    .attributes
                    .issues
                    .push("Pack episode count is unknown; size limits were not applied".into());
            }
            Some(units) => {
                if let Some(size) = size {
                    let size = u128::from(size) * scale;
                    let bytes = |mb: u32| u128::from(mb) * 1_000_000 * u128::from(units);
                    if minimum.is_some_and(|min| size < bytes(min)) {
                        assessment.reasons.push(
                            if hourly.is_some() {
                                "Release is smaller than the profile minimum MB per hour"
                            } else {
                                "Release is smaller than the profile minimum size"
                            }
                            .into(),
                        );
                    }
                    if maximum.is_some_and(|max| size > bytes(max)) {
                        assessment.reasons.push(
                            if hourly.is_some() {
                                "Release is larger than the profile maximum MB per hour"
                            } else {
                                "Release is larger than the profile maximum size"
                            }
                            .into(),
                        );
                    }
                }
            }
            _ => {}
        }
        assessment.accepted = assessment.reasons.is_empty();
        assessment
    }

    /// Revisions can improve the same group's release even at the cutoff.
    pub fn is_upgrade(
        &self,
        candidate: &Assessment,
        current: &Assessment,
        candidate_title: &str,
        current_title: &str,
    ) -> bool {
        if !candidate.accepted {
            return false;
        }
        if !current.accepted {
            return true;
        }
        if !self.cutoff_reached(current) {
            return candidate.rank > current.rank;
        }
        let group = |title: &str| title.rsplit_once('-').map(|(_, group)| tokens(group));
        candidate.rank.revision > current.rank.revision
            && candidate.rank > current.rank
            && candidate.attributes.resolution == current.attributes.resolution
            && candidate.attributes.source == current.attributes.source
            && group(candidate_title).is_some_and(|group_name| {
                !group_name.is_empty() && Some(group_name) == group(current_title)
            })
    }

    fn assess_title(
        &self,
        release_title: &str,
        media_title: &str,
        original: Original<'_>,
    ) -> Assessment {
        let (attributes, suffix, invalid) = parse_attributes(release_title, media_title);
        let mut reasons = Vec::new();
        if invalid.global {
            reasons.extend(attributes.issues.iter().cloned());
        }
        if !self.resolutions.is_empty() && invalid.resolution {
            reasons.push("Malformed, unsupported or conflicting resolution markers".into());
        }
        if !self.sources.is_empty() && invalid.source {
            reasons.push("Conflicting source markers".into());
        }
        if !self.codecs.is_empty() && invalid.codec {
            reasons.push("Malformed, unsupported or conflicting codec markers".into());
        }
        for term in &self.required_terms {
            if !contains_phrase(&suffix, &tokens(term)) {
                reasons.push("A required token phrase is missing".into());
            }
        }
        for term in &self.blocked_terms {
            if contains_phrase(&suffix, &tokens(term)) {
                reasons.push("A blocked token phrase is present".into());
            }
        }
        let custom_score = self.score_rules.iter().fold(0_i32, |score, rule| {
            if rule
                .terms
                .iter()
                .all(|term| contains_phrase(&suffix, &tokens(term)))
            {
                score.saturating_add(rule.score)
            } else {
                score
            }
        });
        if custom_score < self.minimum_score {
            reasons.push(format!(
                "Custom score {custom_score} is below minimum {}",
                self.minimum_score
            ));
        }
        let resolution = preference(
            &self.resolutions,
            attributes.resolution.as_ref(),
            self.allow_unknown_resolution,
            "resolution",
            &mut reasons,
        );
        let source = preference(
            &self.sources,
            attributes.source.as_ref(),
            self.allow_unknown_source,
            "source",
            &mut reasons,
        );
        let codec = preference(
            &self.codecs,
            attributes.codec.as_ref(),
            self.allow_unknown_codec,
            "codec",
            &mut reasons,
        );
        let mode = self.effective_language_mode();
        let mut original_audio = None;
        let language = if mode == LanguageMode::Any {
            0
        } else {
            if let Original::Title(code) = original
                && self.languages.iter().any(|language| language == ORIGINAL)
            {
                original_audio = Some(OriginalAudio::of(&attributes, code));
            }
            let matched: Vec<bool> = self
                .languages
                .iter()
                .map(|language| {
                    if language == ORIGINAL {
                        original_audio.as_ref().is_none_or(|o| o.audio)
                    } else {
                        matches_language(language, &attributes.languages)
                    }
                })
                .collect();
            let count = matched.iter().filter(|matched| **matched).count();
            if mode == LanguageMode::Require && count < matched.len() {
                // An untagged release passes when the profile accepts those.
                if let Some(o) = original_audio.as_ref().filter(|o| !o.audio)
                    && !(attributes.languages.is_empty() && self.allow_unknown_language)
                {
                    reasons.push(o.explanation.clone());
                }
                let others_missing = matched
                    .iter()
                    .zip(&self.languages)
                    .any(|(matched, language)| !matched && language != ORIGINAL);
                if !others_missing {
                    // Only the original audio is missing; said above.
                } else if attributes.languages.is_empty() {
                    if !self.allow_unknown_language {
                        reasons.push("Language is unknown".into());
                    }
                } else if count == 0 {
                    reasons.push("No explicitly marked language is allowed by the profile".into());
                } else {
                    reasons.push("The release lacks some of the profile's languages".into());
                }
            }
            language_rank(&matched)
        };
        let episode_title = episode_title_positions(&suffix);
        let revision = suffix
            .iter()
            .enumerate()
            .filter(|(index, _)| !episode_title[*index])
            .map(|(_, token)| match token.as_str() {
                "proper" | "repack" | "real" => 1,
                _ => 0,
            })
            .sum();
        Assessment {
            accepted: reasons.is_empty(),
            reasons,
            rank: Rank {
                revision,
                custom_score,
                resolution,
                source,
                codec,
                language,
            },
            attributes,
            original: original_audio,
        }
    }
}

/// What is known of a title's original language when its release is assessed.
#[derive(Clone, Copy)]
enum Original<'a> {
    /// TMDB's code for it, such as `en`; `None` when TMDB did not give it.
    Title(Option<&'a str>),
    /// No title yet: `original` waits for the check made once it is known.
    Unjudged,
}

/// Whether a release holds its title's original audio, and why, in words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginalAudio {
    /// TMDB's code for the title's original language; `None` when unknown.
    pub language: Option<String>,
    pub audio: bool,
    /// Such as "Original language: English — VFF only, no original audio".
    pub explanation: String,
}

impl OriginalAudio {
    /// VO, MULTi and VOSTFR hold the original audio whatever it is. Without
    /// any of them, the original language decides: a release without a
    /// language tag counts unless the title is French, and French audio
    /// counts only for a French title.
    fn of(attributes: &Attributes, language: Option<&str>) -> Self {
        let tags = attributes
            .languages
            .iter()
            .map(|tag| tag.to_uppercase())
            .collect::<Vec<_>>()
            .join(", ");
        let (audio, why) = if attributes.multi {
            (true, "MULTi, with the original audio".to_owned())
        } else if attributes.vo {
            (true, "VO, the original audio".to_owned())
        } else if attributes.languages.iter().any(|tag| tag == "vostfr") {
            (
                true,
                "VOSTFR, the original audio with French subtitles".to_owned(),
            )
        } else if let Some(code) = language {
            let french = |tag: &String| matches!(tag.as_str(), "fr" | "vff" | "vfq" | "vfi");
            if let Some(tag) = attributes
                .languages
                .iter()
                .find(|tag| *tag == release_language(code) || (code == "fr" && french(tag)))
            {
                (true, format!("{}, the original audio", tag.to_uppercase()))
            } else if attributes.languages.is_empty() && code == "fr" {
                (false, "no language tag, no sign of French audio".to_owned())
            } else if attributes.languages.is_empty() {
                (
                    true,
                    "no language tag, taken as the original audio".to_owned(),
                )
            } else {
                (false, format!("{tags} only, no original audio"))
            }
        } else if attributes.languages.is_empty() {
            (
                false,
                "no language tag; only VO, MULTi or VOSTFR count".to_owned(),
            )
        } else {
            (
                false,
                format!("{tags} only; only VO, MULTi or VOSTFR count"),
            )
        };
        let head = language.map_or_else(
            || "Original language unknown".to_owned(),
            |code| format!("Original language: {}", language_name(code)),
        );
        Self {
            language: language.map(str::to_owned),
            audio,
            explanation: format!("{head} — {why}"),
        }
    }

    pub fn to_json(&self) -> Value {
        object([
            (
                "language",
                self.language.clone().map_or(Value::Null, Value::String),
            ),
            (
                "name",
                self.language
                    .as_deref()
                    .map_or(Value::Null, |code| language_name(code).into()),
            ),
            ("audio", self.audio.into()),
            ("explanation", self.explanation.clone().into()),
        ])
    }
}

/// A TMDB original language code: two or three lowercase ASCII letters.
pub fn valid_language_code(code: &str) -> bool {
    (2..=3).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_lowercase())
}

/// The release-name language of a TMDB code: TMDB's `cn` (Cantonese) is
/// tagged as Chinese.
fn release_language(code: &str) -> &str {
    if code == "cn" { "zh" } else { code }
}

/// The English name of a TMDB original language code; the code itself,
/// upper case, for a language without one here.
pub fn language_name(code: &str) -> String {
    match code {
        "en" => "English",
        "fr" => "French",
        "de" => "German",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" => "Portuguese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "cn" => "Cantonese",
        "ru" => "Russian",
        "hi" => "Hindi",
        "ta" => "Tamil",
        "te" => "Telugu",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "da" => "Danish",
        "no" | "nb" => "Norwegian",
        "fi" => "Finnish",
        "is" => "Icelandic",
        "pl" => "Polish",
        "cs" => "Czech",
        "hu" => "Hungarian",
        "ro" => "Romanian",
        "el" => "Greek",
        "tr" => "Turkish",
        "uk" => "Ukrainian",
        "ar" => "Arabic",
        "he" => "Hebrew",
        "fa" => "Persian",
        "th" => "Thai",
        "id" => "Indonesian",
        "xx" => "No language",
        _ => return code.to_uppercase(),
    }
    .into()
}

/// Larger values win: the language first, then the quality, then the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rank {
    pub custom_score: i32,
    pub resolution: u32,
    pub source: u32,
    pub codec: u32,
    pub language: u32,
    pub revision: u32,
}

impl Rank {
    pub fn order(&self) -> [&'static str; 6] {
        [
            "language",
            "resolution",
            "source",
            "custom_score",
            "revision",
            "codec",
        ]
    }

    pub(crate) fn ordered_values(&self) -> [u32; 6] {
        let score = self.custom_score.cast_unsigned() ^ 0x8000_0000;
        [
            self.language,
            self.resolution,
            self.source,
            score,
            self.revision,
            self.codec,
        ]
    }

    pub fn to_json(&self) -> Value {
        object([
            ("custom_score", number(self.custom_score)),
            ("resolution", self.resolution.into()),
            ("source", self.source.into()),
            ("codec", self.codec.into()),
            ("language", self.language.into()),
            ("revision", self.revision.into()),
            (
                "order",
                Value::Array(self.order().into_iter().map(Value::from).collect()),
            ),
        ])
    }
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ordered_values().cmp(&other.ordered_values())
    }
}
impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    pub resolution: Option<u32>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub languages: Vec<String>,
    pub issues: Vec<String>,
    /// Tagged MULTi: the original audio beside another one.
    pub multi: bool,
    /// Tagged VO: the original audio.
    pub vo: bool,
}

impl Attributes {
    pub fn to_json(&self) -> Value {
        object([
            (
                "resolution",
                self.resolution.map(Value::from).unwrap_or(Value::Null),
            ),
            (
                "source",
                self.source.clone().map(Value::from).unwrap_or(Value::Null),
            ),
            (
                "codec",
                self.codec.clone().map(Value::from).unwrap_or(Value::Null),
            ),
            ("languages", strings(&self.languages)),
            ("issues", strings(&self.issues)),
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    pub accepted: bool,
    pub reasons: Vec<String>,
    pub rank: Rank,
    pub attributes: Attributes,
    /// Whether the release holds the title's original audio, when the
    /// profile asks for it and the title is known.
    pub original: Option<OriginalAudio>,
}

impl Assessment {
    pub fn to_json(&self) -> Value {
        let mut value = object([
            ("accepted", self.accepted.into()),
            ("reasons", strings(&self.reasons)),
            ("rank", self.rank.to_json()),
            ("attributes", self.attributes.to_json()),
        ]);
        if let Some(original) = &self.original {
            value.insert("original", original.to_json());
        }
        value
    }
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}
fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}
fn number(value: i32) -> Value {
    Value::Number(f64::from(value))
}

/// Fields Mynou does not know are ignored, so older or newer documents load.
fn expect_object(value: &Value) -> Result<()> {
    value.as_object().ok_or("Selection: expected an object")?;
    Ok(())
}
pub(crate) fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        Err("Selection: profile names must contain 1 to 64 ASCII letters, digits, hyphens or underscores".into())
    } else {
        Ok(())
    }
}
fn profile_name(value: &Value, field: &str, default: &str) -> Result<String> {
    let name = match value.get(field) {
        None => default,
        Some(Value::String(name)) => name,
        _ => return Err(format!("Selection: {field} must be a string")),
    };
    validate_profile_name(name)?;
    Ok(name.into())
}
fn list<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    let values = match value.get(field) {
        None => return Ok(&[]),
        Some(value) => value
            .as_array()
            .ok_or_else(|| format!("Selection: {field} must be an array"))?,
    };
    if values.len() > MAX_ENTRIES {
        return Err(format!("Selection: {field} exceeds 32 entries"));
    }
    Ok(values)
}
fn boolean(value: &Value, field: &str) -> Result<bool> {
    match value.get(field) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("Selection: {field} must be a boolean")),
    }
}
fn signed(value: &Value, field: &str, default: Option<i32>) -> Result<i32> {
    let score = match value.get(field) {
        None => return default.ok_or_else(|| format!("Selection: missing field {field}")),
        Some(value) => value
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| format!("Selection: {field} must be an integer"))?,
    };
    if score.abs_diff(0) > MAX_SCORE as u32 {
        Err(format!(
            "Selection: {field} must be between -100000 and 100000"
        ))
    } else {
        Ok(score)
    }
}
fn choices(value: &Value, field: &str, allowed: &[&str]) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for value in list(value, field)? {
        let choice = value
            .as_str()
            .filter(|choice| allowed.contains(choice))
            .ok_or_else(|| format!("Selection: unsupported {field} value"))?;
        if result.iter().any(|v| v == choice) {
            return Err(format!("Selection: duplicate {field} value"));
        }
        result.push(choice.into());
    }
    Ok(result)
}
fn terms(value: &Value, field: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for value in list(value, field)? {
        let term = value
            .as_str()
            .ok_or_else(|| format!("Selection: {field} terms must be strings"))?;
        if term.is_empty()
            || term.len() > MAX_TERM_BYTES
            || term.chars().any(char::is_control)
            || term.trim() != term
        {
            return Err(format!(
                "Selection: {field} terms must contain 1 to 128 bytes without controls or outer whitespace"
            ));
        }
        let normalized = tokens(term);
        if normalized.is_empty() || !seen.insert(normalized) {
            return Err(format!(
                "Selection: {field} contains an empty or duplicate token phrase"
            ));
        }
        result.push(term.into());
    }
    Ok(result)
}
fn preference<T: PartialEq>(
    allowed: &[T],
    actual: Option<&T>,
    allow_unknown: bool,
    name: &str,
    reasons: &mut Vec<String>,
) -> u32 {
    if allowed.is_empty() {
        return 0;
    }
    match actual {
        Some(actual) => match allowed.iter().position(|v| v == actual) {
            Some(index) => (allowed.len() - index) as u32,
            None => {
                reasons.push(format!("Marked {name} is not allowed by the profile"));
                0
            }
        },
        None => {
            if !allow_unknown {
                reasons.push(format!("{} is unknown", capitalize(name)));
            }
            0
        }
    }
}
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// How well a release covers the profile's languages: how many it has, then
/// which, the first ones counting most.
fn language_rank(matched: &[bool]) -> u32 {
    let count = matched.iter().filter(|matched| **matched).count() as u32;
    let which = matched
        .iter()
        .fold(0_u32, |bits, matched| (bits << 1) | u32::from(*matched));
    (count << 16) | which
}

/// Scene-language matching is directional: generic French is a profile wildcard,
/// but a generic French release tag cannot establish a specific dubbing variant.
fn matches_language(preferred: &str, actual: &[String]) -> bool {
    actual.iter().any(|language| {
        language == preferred
            || (preferred == "fr" && matches!(language.as_str(), "vff" | "vfq" | "vfi"))
    })
}

/// Punctuation separates tokens; a phrase never matches part of another word.
pub(crate) fn tokens(text: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ï' | 'î' | 'í' | 'ì' => 'i',
            'ô' | 'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            c => c,
        };
        normalized.push(if c.is_alphanumeric() { c } else { ' ' });
    }
    normalized.split_whitespace().map(str::to_owned).collect()
}
fn contains_phrase(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[derive(Default)]
struct InvalidMarkers {
    global: bool,
    resolution: bool,
    source: bool,
    codec: bool,
}

fn parse_attributes(
    release_title: &str,
    media_title: &str,
) -> (Attributes, Vec<String>, InvalidMarkers) {
    let mut attributes = Attributes::default();
    let mut invalid = InvalidMarkers::default();
    if release_title.len() > MAX_TITLE_BYTES || media_title.len() > MAX_TITLE_BYTES {
        attributes
            .issues
            .push("Release or media title exceeds 4096 bytes".into());
        invalid.global = true;
        return (attributes, Vec::new(), invalid);
    }
    let release = tokens(release_title);
    let media = tokens(media_title);
    if media.is_empty() || !release.starts_with(&media) {
        attributes
            .issues
            .push("Release title does not begin with the media title".into());
        invalid.global = true;
        return (attributes, Vec::new(), invalid);
    }
    let suffix = release[media.len()..].to_vec();
    let mut resolutions = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut codecs = BTreeSet::new();
    let mut languages = BTreeSet::new();
    let subtitle_languages = subtitle_language_positions(&suffix);
    let episode_title = episode_title_positions(&suffix);
    for (index, token) in suffix.iter().enumerate() {
        let next = suffix.get(index + 1).map(String::as_str).unwrap_or("");
        let issues_before = attributes.issues.len();
        if let Some(resolution) = resolution_marker(token, &mut attributes.issues) {
            resolutions.insert(resolution);
        }
        invalid.resolution |= attributes.issues.len() > issues_before;
        if let Some(source) = source_marker(token, next) {
            sources.insert(source);
        }
        let issues_before = attributes.issues.len();
        if let Some(codec) = codec_marker(token, next, &mut attributes.issues) {
            codecs.insert(codec);
        }
        invalid.codec |= attributes.issues.len() > issues_before;
        // VOSTFR, SUBFRENCH or French subtitles: the original audio with
        // French subtitles.
        if !episode_title[index]
            && (matches!(
                token.as_str(),
                "vostfr" | "vostfre" | "subfrench" | "subfr" | "subfre" | "stfr"
            ) || (subtitle_languages[index] && language_markers(token) == ["fr"]))
        {
            languages.insert("vostfr");
        }
        let marked_languages = language_markers(token);
        if !marked_languages.is_empty() {
            let previous = index
                .checked_sub(1)
                .and_then(|i| suffix.get(i))
                .map(String::as_str)
                .unwrap_or("");
            let explicitly_audio = previous == "audio"
                || next == "audio"
                || token.starts_with("audio")
                || token.ends_with("audio");
            let subtitle_context = subtitle_languages[index];
            // A two-letter code is also a word ("It", "de", "en"): in the
            // words of an episode title, it is no language.
            let word = token.len() == 2 && episode_title[index];
            if explicitly_audio || (!subtitle_context && !word) {
                languages.extend(marked_languages.iter().copied());
            }
        }
    }
    // Blu-ray remux is a single, more specific source, not two contradictory sources.
    if sources.contains("remux") {
        sources.remove("bluray");
    }
    invalid.resolution |= resolutions.len() > 1;
    invalid.source = sources.len() > 1;
    invalid.codec |= codecs.len() > 1;
    attributes.resolution = unique_marker(resolutions, "resolution", &mut attributes.issues);
    attributes.source = unique_marker(sources, "source", &mut attributes.issues).map(str::to_owned);
    attributes.codec = unique_marker(codecs, "codec", &mut attributes.issues).map(str::to_owned);
    // A generic French tag alongside an explicit dub adds no extra audio track.
    if languages
        .iter()
        .any(|language| matches!(*language, "vff" | "vfq" | "vfi"))
    {
        languages.remove("fr");
    }
    let tagged = |tag: &str| {
        suffix
            .iter()
            .enumerate()
            .any(|(index, token)| token == tag && !episode_title[index])
    };
    attributes.multi = tagged("multi");
    attributes.vo = tagged("vo");
    // MULTi holds the original audio beside the French one, with French
    // subtitles in practice: it counts as VOSTFR too.
    if attributes.multi
        && languages
            .iter()
            .any(|language| matches!(*language, "fr" | "vff" | "vfq" | "vfi"))
    {
        languages.insert("vostfr");
    }
    attributes.languages = languages.into_iter().map(str::to_owned).collect();
    (attributes, suffix, invalid)
}
fn unique_marker<T>(values: BTreeSet<T>, name: &str, issues: &mut Vec<String>) -> Option<T> {
    if values.len() > 1 {
        issues.push(format!("Conflicting {name} markers in release title"));
        None
    } else {
        values.into_iter().next()
    }
}
fn resolution_marker(token: &str, issues: &mut Vec<String>) -> Option<u32> {
    if matches!(token, "4k" | "uhd") {
        return Some(2160);
    }
    let value = if let Some(number) = token.strip_suffix('p').or_else(|| token.strip_suffix('i')) {
        if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
            Some(number)
        } else {
            if token.starts_with(|c: char| c.is_ascii_digit()) {
                issues.push("Malformed resolution marker in release title".into());
            }
            None
        }
    } else if let Some((width, height)) = token.split_once('x') {
        if !width.is_empty() && width.bytes().all(|b| b.is_ascii_digit()) {
            // Small numeric pairs are episode identifiers such as 2x03, not video dimensions.
            if width.parse::<u32>().is_ok_and(|width| width < 320)
                && height.parse::<u32>().is_ok_and(|height| height <= 99999)
            {
                return None;
            }
            let valid_width = width
                .parse::<u32>()
                .is_ok_and(|width| (320..=16384).contains(&width));
            if valid_width && !height.is_empty() && height.bytes().all(|b| b.is_ascii_digit()) {
                Some(height)
            } else {
                issues.push("Malformed resolution marker in release title".into());
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    if let Some(value) = value {
        let parsed = value.parse::<u32>().ok();
        if let Some(resolution) = parsed.filter(|v| RESOLUTIONS.contains(v)) {
            Some(resolution)
        } else {
            issues.push("Unsupported resolution marker in release title".into());
            None
        }
    } else {
        None
    }
}
fn source_marker(token: &str, next: &str) -> Option<&'static str> {
    match token {
        "cam" | "camrip" | "hdcam" | "telesync" | "ts" => Some("cam"),
        "dvd" | "dvdrip" | "dvdr" => Some("dvd"),
        "hdtv" | "pdtv" => Some("hdtv"),
        "webrip" => Some("webrip"),
        "webdl" => Some("web-dl"),
        "web" if next == "dl" => Some("web-dl"),
        "web" if next == "rip" => Some("webrip"),
        "bluray" | "bdrip" | "brrip" => Some("bluray"),
        "blu" if next == "ray" => Some("bluray"),
        "remux" | "bdremux" => Some("remux"),
        _ => None,
    }
}
fn codec_marker(token: &str, next: &str, issues: &mut Vec<String>) -> Option<&'static str> {
    match token {
        "h264" | "x264" | "avc" | "h26410bit" | "x26410bit" => Some("h264"),
        "h265" | "x265" | "hevc" | "h26510bit" | "x26510bit" | "h26512bit" | "x26512bit" => {
            Some("h265")
        }
        "h" if next == "264" => Some("h264"),
        "h" if next == "265" => Some("h265"),
        "av1" => Some("av1"),
        "av" if next == "1" => Some("av1"),
        "vp9" => Some("vp9"),
        "vp" if next == "9" => Some("vp9"),
        "h" if !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit()) => {
            issues.push("Unsupported codec marker in release title".into());
            None
        }
        _ => {
            if let Some(rest) = token.strip_prefix('h').or_else(|| token.strip_prefix('x')) {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    issues.push("Unsupported codec marker in release title".into());
                } else if rest.starts_with("264") || rest.starts_with("265") {
                    issues.push("Malformed codec marker in release title".into());
                }
            }
            None
        }
    }
}
fn is_subtitle_marker(token: &str) -> bool {
    matches!(
        token,
        "sub" | "subs" | "subtitle" | "subtitles" | "vost" | "sdh" | "cc"
    )
}
/// The positions of an episode title's words: after the episode number and
/// before the first release marker. Only a title with a word that is no
/// language code counts, so that a release such as
/// `Show S01E05 DE 1080p` keeps its language; a film has no such title.
fn episode_title_positions(tokens: &[String]) -> Vec<bool> {
    let mut positions = vec![false; tokens.len()];
    let Some(start) = tokens
        .iter()
        .position(|token| crate::numbering::episode_marker(token).is_some())
    else {
        return positions;
    };
    let mut scratch = Vec::new();
    let end = (start + 1..tokens.len())
        .find(|&index| {
            let token = &tokens[index];
            let next = tokens.get(index + 1).map(String::as_str).unwrap_or("");
            resolution_marker(token, &mut scratch).is_some()
                || source_marker(token, next).is_some()
                || codec_marker(token, next, &mut scratch).is_some()
                || release_tag(token)
                || is_subtitle_marker(token)
        })
        .unwrap_or(tokens.len());
    let title = start + 1..end;
    // A single letter is a piece of a split marker ("E-AC3"), not a word.
    if tokens[title.clone()].iter().any(|token| {
        token.len() >= 2
            && token.bytes().any(|byte| byte.is_ascii_alphabetic())
            && language_markers(token).is_empty()
    }) {
        positions[title].fill(true);
    }
    positions
}

/// Release markers other than resolution, source and codec that end an
/// episode title: audio formats with their channels, and edition, repack,
/// dynamic range and bit depth tags.
fn release_tag(token: &str) -> bool {
    const AUDIO: [&str; 12] = [
        "aac", "ac3", "eac3", "ddp", "dd", "dts", "truehd", "atmos", "opus", "flac", "pcm", "lpcm",
    ];
    // Words that also start episode titles ("Real", "Complete") are left out.
    const TAGS: [&str; 19] = [
        "proper",
        "repack",
        "rerip",
        "internal",
        "multi",
        "dual",
        "dl",
        "hdr",
        "hdr10",
        "dv",
        "dovi",
        "sdr",
        "10bit",
        "8bit",
        "remastered",
        "uncut",
        "imax",
        "dubbed",
        "vo",
    ];
    TAGS.contains(&token)
        || AUDIO.iter().any(|codec| {
            token
                .strip_prefix(codec)
                .is_some_and(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn subtitle_language_positions(tokens: &[String]) -> Vec<bool> {
    let mut positions = vec![false; tokens.len()];
    for (index, token) in tokens.iter().enumerate() {
        if !is_subtitle_marker(token) {
            continue;
        }
        // Subtitle labels scope a contiguous language list in either direction.
        // Audio and other release markers form boundaries.
        for (next, token) in tokens.iter().enumerate().skip(index + 1) {
            if language_markers(token).is_empty() {
                break;
            }
            positions[next] = true;
        }
        for (previous, token) in tokens[..index].iter().enumerate().rev() {
            if language_markers(token).is_empty() {
                break;
            }
            positions[previous] = true;
        }
    }
    positions
}

fn language_markers(token: &str) -> &'static [&'static str] {
    // VOSTFR, SUBFR, MULTI and DUAL provide no evidence of an audio language.
    if token.starts_with("vost")
        || token.starts_with("sub")
        || token.ends_with("subs")
        || token.ends_with("sdh")
    {
        return &[];
    }
    let token = token
        .strip_prefix("audio")
        .or_else(|| token.strip_suffix("audio"))
        .unwrap_or(token);
    match token {
        "en" | "eng" | "english" => &["en"],
        "fr" | "fre" | "fra" | "french" | "vf" => &["fr"],
        "vff" | "truefrench" => &["vff"],
        "vfq" => &["vfq"],
        "vfi" => &["vfi"],
        "vf2" => &["vff", "vfq"],
        "de" | "ger" | "deu" | "german" => &["de"],
        "es" | "spa" | "esp" | "spanish" => &["es"],
        "it" | "ita" | "italian" => &["it"],
        "ja" | "jpn" | "jap" | "japanese" => &["ja"],
        "ko" | "kor" | "korean" => &["ko"],
        "zh" | "chi" | "zho" | "chinese" | "mandarin" | "cantonese" => &["zh"],
        "pt" | "por" | "portuguese" => &["pt"],
        "ru" | "rus" | "russian" => &["ru"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn parse_profile(json: &str) -> Profile {
        Profile::from_json(&json::parse(json).unwrap()).unwrap()
    }

    #[test]
    fn french_scene_tag_parsing_and_deduplication() {
        for (tags, expected) in [
            ("VFF", vec!["vff"]),
            ("TRUEFRENCH", vec!["vff"]),
            ("VFQ", vec!["vfq"]),
            ("VFI", vec!["vfi"]),
            ("FRENCH", vec!["fr"]),
            ("FR", vec!["fr"]),
            ("FRE", vec!["fr"]),
            ("FRA", vec!["fr"]),
            ("VF", vec!["fr"]),
            ("VF2", vec!["vff", "vfq"]),
            ("MULTi.VFF", vec!["vff", "vostfr"]),
            ("MULTI.VF2", vec!["vff", "vfq", "vostfr"]),
            ("MULTi.TRUEFRENCH", vec!["vff", "vostfr"]),
            ("FRENCH.VFF", vec!["vff"]),
            ("VFF.TRUEFRENCH.VFF", vec!["vff"]),
            ("VF2.VFQ.VFF.FRENCH", vec!["vff", "vfq"]),
            ("VF.VFI", vec!["vfi"]),
            ("MULTi", vec![]),
            ("DUAL", vec![]),
            ("VOSTFR", vec!["vostfr"]),
            ("VOST.FR", vec!["vostfr"]),
            ("SUBFRENCH", vec!["vostfr"]),
            ("SUBS.VF2", vec![]),
            ("VFF.VFQ.SUBS", vec![]),
            ("VFF.Audio", vec!["vff"]),
            ("AudioVF2", vec!["vff", "vfq"]),
            ("VOSTFR.VFF", vec!["vff", "vostfr"]),
        ] {
            let assessment = Profile::default().assess(&format!("Example 1080p {tags}"), "Example");
            assert_eq!(assessment.attributes.languages, expected, "{tags}");
        }
    }

    #[test]
    fn french_profile_matching_is_directional_in_every_mode() {
        for preferred in ["fr", "vff", "vfq", "vfi"] {
            for (tag, languages) in [
                ("FRENCH", vec!["fr"]),
                ("VFF", vec!["vff"]),
                ("VFQ", vec!["vfq"]),
                ("VFI", vec!["vfi"]),
                ("VF2", vec!["vff", "vfq"]),
                ("EN", vec!["en"]),
                ("MULTI", vec![]),
            ] {
                let matches = languages.contains(&preferred)
                    || (preferred == "fr" && !languages.is_empty() && tag != "EN");
                for mode in ["require", "prefer", "any"] {
                    let p = parse_profile(&format!(
                        r#"{{"languages":["{preferred}"],"language_mode":"{mode}"}}"#
                    ));
                    let assessment = p.assess(&format!("Example 1080p {tag}"), "Example");
                    assert_eq!(
                        assessment.accepted,
                        mode != "require" || matches,
                        "{preferred} / {tag} / {mode}"
                    );
                    assert_eq!(
                        assessment.rank.language,
                        if mode != "any" && matches {
                            language_rank(&[true])
                        } else {
                            0
                        },
                        "{preferred} / {tag} / {mode}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_language_is_needed_and_decides_before_the_quality() {
        let p = parse_profile(
            r#"{"resolutions":[2160,1080,720],"cutoff_resolution":1080,"languages":["vff","vostfr"],"language_mode":"prefer"}"#,
        );
        let assess = |title: &str| p.assess(title, "Example");
        // MULTi.VFF holds both; a 4K in VFF alone loses to it.
        let both = "Example 1080p MULTi VFF";
        let uhd = "Example 2160p VFF";
        assert!(assess(both).rank > assess(uhd).rank);
        assert!(p.cutoff_reached(&assess(both)));
        assert!(!p.cutoff_reached(&assess(uhd)));
        assert!(p.is_upgrade(&assess(both), &assess(uhd), both, uhd));
        assert!(!p.is_upgrade(&assess(uhd), &assess(both), uhd, both));
        let uhd_both = "Example 2160p MULTi VFF";
        assert!(assess(uhd_both).rank > assess(both).rank);
        for title in ["Example 1080p VFF VOSTFR", "Example 720p VF2 VOSTFR"] {
            assert_eq!(
                assess(title).rank.language,
                assess(both).rank.language,
                "{title}"
            );
        }
        // With only some of them, the first ones count most.
        assert!(assess("Example 720p VFF").rank > assess("Example 2160p VOSTFR").rank);
        assert!(assess("Example 720p VOSTFR").rank > assess("Example 2160p EN").rank);
        assert!(assess("Example 2160p EN").accepted);

        let mut strict = p.clone();
        strict.language_mode = Some(LanguageMode::Require);
        assert!(strict.assess(both, "Example").accepted);
        let partial = strict.assess(uhd, "Example");
        assert!(!partial.accepted);
        assert_eq!(
            partial.reasons,
            ["The release lacks some of the profile's languages"]
        );

        // French matches every dub; a variant needs its own tag.
        let wildcard = parse_profile(
            r#"{"resolutions":[1080],"cutoff_resolution":1080,"languages":["fr","vff"]}"#,
        );
        for (tag, complete) in [
            ("FR", false),
            ("VFF", true),
            ("VFQ", false),
            ("VFI", false),
            ("VF2", true),
        ] {
            let assessment = wildcard.assess(&format!("Example 1080p {tag}"), "Example");
            assert_eq!(wildcard.cutoff_reached(&assessment), complete, "{tag}");
        }
    }

    #[test]
    fn original_language_is_resolved_per_title() {
        let p = parse_profile(r#"{"languages":["original"],"language_mode":"require"}"#);
        assert_eq!(Profile::from_json(&p.to_json()).unwrap(), p);
        assert!(p.uses_original_language());
        let english = |title: &str| p.assess_for(title, "Example", Some("en"));
        let french = |title: &str| p.assess_for(title, "Example", Some("fr"));
        // A French dub alone is no English film's original audio; it is a
        // French film's.
        for tag in ["FRENCH", "VFF", "VFQ", "TRUEFRENCH", "VF2", "VFI"] {
            let title = format!("Example 2024 1080p {tag}");
            assert!(!english(&title).accepted, "{tag}");
            assert!(french(&title).accepted, "{tag}");
        }
        // VO, MULTi and VOSTFR hold it whatever it is.
        for tag in [
            "MULTi",
            "MULTi.VFF",
            "MULTi.FRENCH",
            "VO",
            "VFF.VO",
            "VOSTFR",
            "SUBFRENCH",
        ] {
            let title = format!("Example 2024 1080p {tag}");
            for assessment in [english(&title), french(&title)] {
                assert!(assessment.accepted, "{tag}");
                assert!(assessment.original.unwrap().audio, "{tag}");
            }
        }
        // A release without a language tag is the original audio unless the
        // title is French.
        assert!(english("Example 2024 1080p").accepted);
        assert!(!french("Example 2024 1080p").accepted);
        // The title's own language, as release names tag it.
        assert!(english("Example 2024 1080p ENGLISH").accepted);
        assert!(!french("Example 2024 1080p ENGLISH").accepted);
        assert!(!english("Example 2024 1080p GERMAN").accepted);
        assert!(
            p.assess_for("Example 1080p JAPANESE", "Example", Some("ja"))
                .accepted
        );
        assert!(
            p.assess_for("Example 1080p CHINESE", "Example", Some("cn"))
                .accepted
        );

        // Each decision says why.
        let refused = english("Example 2024 1080p VFF");
        let why = "Original language: English — VFF only, no original audio";
        assert_eq!(refused.reasons, [why]);
        assert_eq!(refused.original.as_ref().unwrap().explanation, why);
        let json = refused.to_json();
        let original = json.get("original").unwrap();
        assert_eq!(original.get("language").and_then(Value::as_str), Some("en"));
        assert_eq!(
            original.get("name").and_then(Value::as_str),
            Some("English")
        );
        assert_eq!(original.get("audio").and_then(Value::as_bool), Some(false));
        assert_eq!(
            original.get("explanation").and_then(Value::as_str),
            Some(why)
        );
        for (assessment, explanation) in [
            (
                french("Example 2024 1080p FRENCH"),
                "Original language: French — FR, the original audio",
            ),
            (
                english("Example 2024 1080p MULTi VFF"),
                "Original language: English — MULTi, with the original audio",
            ),
            (
                english("Example 2024 1080p VOSTFR"),
                "Original language: English — VOSTFR, the original audio with French subtitles",
            ),
            (
                english("Example 2024 1080p"),
                "Original language: English — no language tag, taken as the original audio",
            ),
            (
                english("Example 2024 1080p EN"),
                "Original language: English — EN, the original audio",
            ),
        ] {
            assert!(assessment.reasons.is_empty(), "{explanation}");
            assert_eq!(assessment.original.unwrap().explanation, explanation);
        }
        assert_eq!(
            french("Example 2024 1080p").reasons,
            ["Original language: French — no language tag, no sign of French audio"]
        );
        assert_eq!(
            english("Example 2024 1080p GERMAN VFF").reasons,
            ["Original language: English — DE, VFF only, no original audio"]
        );

        // Unknown, it never lets a French dub alone or an untagged release
        // pass; VO, MULTi and VOSTFR still count.
        let unknown = |title: &str| p.assess_for(title, "Example", None);
        assert_eq!(
            unknown("Example 2024 1080p VFF").reasons,
            ["Original language unknown — VFF only; only VO, MULTi or VOSTFR count"]
        );
        assert_eq!(
            unknown("Example 2024 1080p").reasons,
            ["Original language unknown — no language tag; only VO, MULTi or VOSTFR count"]
        );
        assert!(unknown("Example 2024 1080p MULTi VFF").accepted);
        assert!(unknown("Example 2024 1080p VOSTFR").accepted);
        let json = unknown("Example 2024 1080p VFF").to_json();
        assert_eq!(
            json.get("original").unwrap().get("name"),
            Some(&Value::Null)
        );

        // Before the title is known (an announcement), the title's own check decides.
        let early = p.assess("Example 2024 1080p VFF", "Example");
        assert!(early.accepted);
        assert!(early.original.is_none());

        // Untagged releases pass anyway when the profile accepts them.
        let lenient = parse_profile(
            r#"{"languages":["original"],"language_mode":"require","allow_unknown_language":true}"#,
        );
        assert!(
            lenient
                .assess_for("Example 2024 1080p", "Example", Some("fr"))
                .accepted
        );
        assert!(
            !lenient
                .assess_for("Example 2024 1080p VFF", "Example", Some("en"))
                .accepted
        );
        // Languages that do not count leave it out.
        let any = parse_profile(r#"{"languages":["original"],"language_mode":"any"}"#);
        assert!(!any.uses_original_language());
        let assessment = any.assess_for("Example 2024 1080p VFF", "Example", Some("en"));
        assert!(assessment.accepted && assessment.original.is_none());
    }

    #[test]
    fn original_language_keeps_every_language_needed_and_ranks_first() {
        // VFF and the original audio: an English film needs MULTi with VFF,
        // or VFF with VO; VFF alone is complete for a French film.
        let p = parse_profile(
            r#"{"resolutions":[2160,1080],"cutoff_resolution":1080,"languages":["vff","original"],"language_mode":"require"}"#,
        );
        let english = |title: &str| p.assess_for(title, "Example", Some("en"));
        let french = |title: &str| p.assess_for(title, "Example", Some("fr"));
        for title in ["Example 1080p MULTi VFF", "Example 1080p VFF VO"] {
            assert!(english(title).accepted, "{title}");
            assert!(p.cutoff_reached(&english(title)), "{title}");
        }
        assert_eq!(
            english("Example 1080p VFF").reasons,
            ["Original language: English — VFF only, no original audio"]
        );
        assert_eq!(
            english("Example 1080p MULTi").reasons,
            ["Language is unknown"]
        );
        assert_eq!(
            english("Example 1080p VOSTFR").reasons,
            ["The release lacks some of the profile's languages"]
        );
        assert_eq!(
            english("Example 1080p GERMAN").reasons,
            [
                "Original language: English — DE only, no original audio",
                "No explicitly marked language is allowed by the profile"
            ]
        );
        assert!(french("Example 1080p VFF").accepted);
        assert!(p.cutoff_reached(&french("Example 1080p VFF")));
        assert!(!french("Example 1080p FRENCH").accepted);

        // Accepted until a complete one: the language still ranks before
        // the quality, and upgrades go on until the original audio is there.
        let mut prefer = p.clone();
        prefer.language_mode = Some(LanguageMode::Prefer);
        let uhd = "Example 2160p VFF";
        let multi = "Example 1080p MULTi VFF";
        let partial = prefer.assess_for(uhd, "Example", Some("en"));
        let complete = prefer.assess_for(multi, "Example", Some("en"));
        assert!(partial.accepted && partial.reasons.is_empty());
        assert_eq!(
            partial.original.as_ref().unwrap().explanation,
            "Original language: English — VFF only, no original audio"
        );
        assert!(complete.rank > partial.rank);
        assert!(!prefer.cutoff_reached(&partial));
        assert!(prefer.cutoff_reached(&complete));
        assert!(prefer.is_upgrade(&complete, &partial, multi, uhd));
        assert!(!prefer.is_upgrade(&partial, &complete, uhd, multi));
        // For a French film, the 4K in VFF is complete and wins.
        let french_uhd = prefer.assess_for(uhd, "Example", Some("fr"));
        assert!(french_uhd.rank > prefer.assess_for(multi, "Example", Some("fr")).rank);
        assert!(prefer.cutoff_reached(&french_uhd));
    }

    #[test]
    fn french_variant_profiles_round_trip() {
        let p = parse_profile(
            r#"{"languages":["vff","vfi","vfq","vostfr","fr","en","de","es","it","ja","ko","zh","pt","ru"],"language_mode":"require"}"#,
        );
        assert_eq!(Profile::from_json(&p.to_json()).unwrap(), p);
        let config = SelectionConfig {
            movie_profile: "french".into(),
            episode_profile: "french".into(),
            profiles: BTreeMap::from([("french".into(), p)]),
        };
        assert_eq!(
            SelectionConfig::from_json(&config.to_json()).unwrap(),
            config
        );
    }

    #[test]
    fn language_modes_and_legacy_derivation() {
        for (mode, unknown, english, french) in [
            ("any", true, true, true),
            ("prefer", true, true, true),
            ("require", false, false, true),
        ] {
            let p = parse_profile(&format!(
                r#"{{"languages":["fr"],"language_mode":"{mode}"}}"#
            ));
            assert_eq!(p.assess("Example 1080p", "Example").accepted, unknown);
            assert_eq!(
                p.assess("Example 1080p English", "Example").accepted,
                english
            );
            assert_eq!(p.assess("Example 1080p French", "Example").accepted, french);
            assert_eq!(p.assess("Example English", "Example").rank.language, 0);
            assert_eq!(Profile::from_json(&p.to_json()).unwrap(), p);
        }
        let prefer = parse_profile(r#"{"languages":["fr"],"allow_unknown_language":true}"#);
        assert_eq!(prefer.effective_language_mode(), LanguageMode::Prefer);
        assert!(prefer.assess("Example 1080p English", "Example").accepted);
        assert!(
            prefer.assess("Example French", "Example").rank
                > prefer.assess("Example English", "Example").rank
        );
        let require = parse_profile(
            r#"{"languages":["fr"],"language_mode":"require","allow_unknown_language":true}"#,
        );
        assert!(require.assess("Example 1080p", "Example").accepted);
        assert!(!require.assess("Example 1080p English", "Example").accepted);
        assert_eq!(
            Profile::default().effective_language_mode(),
            LanguageMode::Any
        );
    }

    #[test]
    fn rank_orders_language_quality_score_revision_and_codec() {
        let mut p = parse_profile(
            r#"{"resolutions":[1080,720],"sources":["bluray","web-dl"],"languages":["fr","en"],"language_mode":"prefer","codecs":["h265","h264"],"score_rules":[{"terms":["bonus"],"score":100}]}"#,
        );
        let high = "Example 1080p Bluray EN H264";
        let french = "Example 720p WEB-DL FR H265 bonus PROPER";
        // The French release has the language, which comes before quality.
        assert!(p.assess(high, "Example").rank < p.assess(french, "Example").rank);
        assert_eq!(p.assess(high, "Example").rank.order()[0], "language");
        p.language_mode = Some(LanguageMode::Any);
        assert!(p.assess(high, "Example").rank > p.assess(french, "Example").rank);
        let proper = p.assess("Example 1080p Bluray EN H264 PROPER", "Example");
        let codec = p.assess("Example 1080p Bluray EN H265", "Example");
        assert!(proper.rank > codec.rank);
        assert_eq!(
            proper
                .rank
                .to_json()
                .get("revision")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn cutoff_source_and_every_language_must_be_reached() {
        let mut p = parse_profile(
            r#"{"resolutions":[2160,1080,720],"cutoff_resolution":1080,"cutoff_source":"bluray","languages":["fr"],"language_mode":"prefer"}"#,
        );
        for (title, reached) in [
            ("Example 720p Remux FR", false),
            ("Example 1080p WEB-DL FR", false),
            ("Example 1080p Bluray EN", false),
            ("Example 1080p Bluray FR", true),
            ("Example 2160p WEB-DL FR", true),
            ("Example 1080p FR", false),
        ] {
            assert_eq!(
                p.cutoff_reached(&p.assess(title, "Example")),
                reached,
                "{title}"
            );
        }
        p.sources = vec!["web-dl".into(), "bluray".into()];
        assert!(p.cutoff_reached(&p.assess("Example 1080p WEB-DL FR", "Example")));
        p.cutoff_source = None;
        p.sources.clear();
        assert!(p.cutoff_reached(&p.assess("Example 1080p FR", "Example")));
        p.language_mode = Some(LanguageMode::Any);
        assert!(p.cutoff_reached(&p.assess("Example 1080p EN", "Example")));
    }

    #[test]
    fn hourly_limits_use_total_runtime_and_fall_back_to_absolute_sizes() {
        let p = parse_profile(
            r#"{"min_mb_per_hour":100,"max_mb_per_hour":200,"min_size_mb":10,"max_size_mb":20,"allow_unknown_size":false}"#,
        );
        let assess = |size, count, runtime| {
            p.assess_with_runtime("Example", "Example", size, count, runtime, None)
        };
        assert!(assess(Some(200_000_000), Some(1), Some(120)).accepted);
        assert!(assess(Some(400_000_000), Some(4), Some(120)).accepted);
        assert!(!assess(Some(400_000_001), Some(4), Some(120)).accepted);
        assert!(!assess(Some(199_999_999), Some(4), Some(120)).accepted);
        assert!(assess(Some(80_000_000), Some(4), None).accepted);
        assert!(!assess(Some(80_000_001), Some(4), None).accepted);
        assert!(!assess(None, Some(1), Some(120)).accepted);
        assert!(!assess(Some(200_000_000), None, Some(120)).accepted);
        assert!(!assess(Some(u64::MAX), Some(u32::MAX), Some(u32::MAX)).accepted);
        let permissive = parse_profile(r#"{"max_size_mb":20}"#);
        assert!(
            permissive
                .assess_with_size("Example", "Example", None, None)
                .accepted
        );
    }

    #[test]
    fn proper_repack_and_real_upgrade_plain_releases_at_cutoff() {
        let p = parse_profile(r#"{"resolutions":[1080,720],"cutoff_resolution":1080}"#);
        let plain = "Example 1080p-GROUP";
        let current = p.assess(plain, "Example");
        for marker in ["PROPER", "REPACK", "REAL"] {
            let title = format!("Example 1080p {marker}-GROUP");
            let revision = p.assess(&title, "Example");
            assert!(p.is_upgrade(&revision, &current, &title, plain));
            assert!(!p.is_upgrade(&current, &revision, plain, &title));
        }
        let scored = parse_profile(
            r#"{"resolutions":[1080],"cutoff_resolution":1080,"codecs":["h265","h264"],"score_rules":[{"terms":["proper"],"score":10}]}"#,
        );
        let plain_codec = "Example 1080p H265-GROUP";
        let proper_codec = "Example 1080p H264 PROPER-GROUP";
        assert!(scored.is_upgrade(
            &scored.assess(proper_codec, "Example"),
            &scored.assess(plain_codec, "Example"),
            proper_codec,
            plain_codec
        ));
        let other = "Example 1080p PROPER-OTHER";
        assert!(!p.is_upgrade(&p.assess(other, "Example"), &current, other, plain));
        assert_eq!(
            p.assess("Example S01E01 Real Life 1080p", "Example")
                .rank
                .revision,
            0
        );
        assert_eq!(
            p.assess("Example 1080p REAL PROPER", "Example")
                .rank
                .revision,
            2
        );
    }

    #[test]
    fn new_profile_fields_are_validated_and_presets_round_trip() {
        for invalid in [
            r#"{"label":""}"#,
            r#"{"label":null}"#,
            r#"{"cutoff_source":"web"}"#,
            r#"{"language_mode":"sometimes"}"#,
            r#"{"min_mb_per_hour":0}"#,
            r#"{"max_mb_per_hour":1000001}"#,
            r#"{"min_mb_per_hour":20,"max_mb_per_hour":10}"#,
        ] {
            assert!(
                Profile::from_json(&json::parse(invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
        assert!(Profile::from_json(&object([("label", "x".repeat(65).into())])).is_err());
        let fresh = SelectionConfig::fresh();
        assert_eq!(SelectionConfig::from_json(&fresh.to_json()).unwrap(), fresh);
        assert_eq!(fresh.movie_profile, "balanced-1080p");
        assert_eq!(fresh.profiles["compact-1080p"].max_mb_per_hour, Some(2500));
        assert!(
            fresh
                .profiles
                .values()
                .all(|p| p.max_size_mb.is_none() && p.blocked_terms == ["3d"])
        );
        assert_eq!(
            SelectionConfig::from_json(&json::parse("{}").unwrap()).unwrap(),
            SelectionConfig::default()
        );
    }

    #[test]
    fn size_limits_are_decimal_per_episode_and_unknown_pack_counts_are_explained() {
        let profile = Profile::from_json(
            &crate::json::parse(
                r#"{"min_size_mb":100,"max_size_mb":200,"allow_unknown_size":false}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let assess = |size, count| profile.assess_with_size("Example 2026", "Example", size, count);
        assert!(assess(Some(100_000_000), Some(1)).accepted);
        assert!(assess(Some(200_000_000), Some(1)).accepted);
        assert_eq!(
            assess(Some(99_999_999), Some(1)).reasons,
            ["Release is smaller than the profile minimum size"]
        );
        assert_eq!(
            assess(Some(200_000_001), Some(1)).reasons,
            ["Release is larger than the profile maximum size"]
        );
        assert_eq!(assess(None, Some(1)).reasons, ["Release size is unknown"]);
        assert!(
            Profile::default()
                .assess_with_size("Example", "Example", None, Some(1))
                .accepted
        );
        assert!(assess(Some(600_000_000), Some(3)).accepted);
        assert!(!assess(Some(299_999_999), Some(3)).accepted);
        assert!(!assess(Some(600_000_001), Some(3)).accepted);
        let unknown = assess(None, None);
        assert!(!unknown.accepted);
        assert!(
            unknown
                .reasons
                .iter()
                .any(|s| s.contains("episode count is unknown"))
        );
        assert!(!assess(Some(1), None).accepted);
        assert_eq!(
            assess(Some(150_000_000), Some(1)).rank,
            assess(Some(180_000_000), Some(1)).rank
        );
    }

    #[test]
    fn unrestricted_profile_preserves_seed_priority() {
        let config = SelectionConfig::default();
        let (_, profile) = config.profile("movie").unwrap();
        let high = profile.assess("Example.2026.2160p.Remux.HEVC.English", "Example");
        let low = profile.assess("Example.2026.720p.WEB-DL.H264", "Example");
        assert!(high.accepted && low.accepted);
        assert_eq!(high.rank, Rank::default());
        assert_eq!(high.rank, low.rank);
        assert!(profile.assess("Example 2026", "Example").accepted);
        assert_eq!(
            SelectionConfig::from_json(&config.to_json()).unwrap(),
            config
        );
    }

    #[test]
    fn title_words_are_never_attribute_markers_or_scoring_terms() {
        let assessment =
            Profile::default().assess("French WEB H264 1080p.2026", "French WEB H264 1080p");
        assert!(assessment.accepted);
        assert_eq!(assessment.attributes, Attributes::default());
        let profile = parse_profile(r#"{"required_terms":["french"]}"#);
        assert!(
            !profile
                .assess("French Connection.2026.1080p", "French Connection")
                .accepted
        );
        assert!(
            !Profile::default()
                .assess("Another French Film.1080p", "French Film")
                .accepted
        );
        assert!(
            Profile::default()
                .assess("Cafe.Night.2026.1080p", "Café Night")
                .accepted
        );
        let accented = parse_profile(r#"{"required_terms":["déjà vu"]}"#);
        assert!(accented.assess("Example deja.vu", "Example").accepted);
        assert!(!accented.assess("Example deja other vu", "Example").accepted);
    }

    #[test]
    fn common_markers_and_punctuation_are_normalized() {
        let assessment = Profile::default().assess(
            "Example 2026 1920x1080 Blu-Ray REMUX H.265 ENG-FRE",
            "Example",
        );
        assert!(assessment.accepted, "{:?}", assessment.reasons);
        assert_eq!(assessment.attributes.resolution, Some(1080));
        assert_eq!(assessment.attributes.source.as_deref(), Some("remux"));
        assert_eq!(assessment.attributes.codec.as_deref(), Some("h265"));
        assert_eq!(assessment.attributes.languages, ["en", "fr"]);
        for marker in ["x264", "AVC", "H.264"] {
            assert_eq!(
                Profile::default()
                    .assess(&format!("Example {marker}"), "Example")
                    .attributes
                    .codec
                    .as_deref(),
                Some("h264")
            );
        }
        let episode = Profile::default().assess("Example 2026 2x03 1080p WEB-DL", "Example");
        assert!(episode.accepted, "{:?}", episode.reasons);
        assert_eq!(episode.attributes.resolution, Some(1080));
    }

    #[test]
    fn subtitle_and_multiple_audio_labels_do_not_invent_languages() {
        let french = parse_profile(r#"{"languages":["fr"]}"#);
        for marker in [
            "MULTI",
            "DUAL",
            "VOSTFR",
            "VOST.FR",
            "French Subs",
            "Subs French",
            "FrenchSDH",
            "SUBFR",
            "SUBS.EN.FR",
            "VOST.ENG.FRE",
            "EN.FR.SUBS",
        ] {
            let assessment = french.assess(&format!("Example 1080p {marker}"), "Example");
            assert!(!assessment.accepted, "{marker}");
            // French subtitles are VOSTFR, never French audio.
            assert!(
                assessment
                    .attributes
                    .languages
                    .iter()
                    .all(|language| language == "vostfr"),
                "{marker}"
            );
        }
        assert!(
            french
                .assess("Example 1080p MULTI ENG-FRE", "Example")
                .accepted
        );
        assert!(french.assess("Example French Audio", "Example").accepted);
        assert!(
            french
                .assess("Example SUBS ENG AUDIO FRE", "Example")
                .accepted
        );
        assert!(!french.assess("Example ENG VOSTFR", "Example").accepted);
        // A word of an episode title is no language; after the markers it is.
        let english = parse_profile(
            r#"{"languages":["en"],"language_mode":"require","allow_unknown_language":true}"#,
        );
        assert!(
            english
                .assess("Example S01E05 Fake It 1080p WEB-DL", "Example")
                .accepted
        );
        assert!(
            !english
                .assess("Example S01E05 1080p WEB-DL IT", "Example")
                .accepted
        );
        // A film has no episode title, and a title of language codes alone
        // is the release's language.
        for german in [
            "Example 2024 DE 1080p WEB-DL",
            "Example S01E05 DE 1080p",
            "Example S01E05 DE AAC 1080p WEB-DL",
            "Example S01E05 DE PROPER 1080p",
            "Example S01E05 DE DDP5 1 1080p",
            "Example.S01E05.DE.E-AC3.5.1.1080p.WEB-DL",
        ] {
            assert!(!english.assess(german, "Example").accepted, "{german}");
        }
    }

    #[test]
    fn conflicting_and_malformed_markers_are_rejected() {
        let constrained = parse_profile(
            r#"{"resolutions":[1080,720],"sources":["web-dl","webrip"],"codecs":["h264","h265"],"allow_unknown_resolution":true,"allow_unknown_source":true,"allow_unknown_codec":true}"#,
        );
        for suffix in [
            "1080p 720p",
            "WEB-DL WEBRip",
            "x264 x265",
            "4320p",
            "1920x",
            "1080p720p",
            "H.266",
            "x264x265",
        ] {
            let assessment = constrained.assess(&format!("Example {suffix}"), "Example");
            assert!(!assessment.accepted, "{suffix}");
            assert!(!assessment.attributes.issues.is_empty());
            let unrestricted = Profile::default().assess(&format!("Example {suffix}"), "Example");
            assert!(unrestricted.accepted, "{suffix}");
            assert_eq!(unrestricted.rank, Rank::default());
            assert!(!unrestricted.attributes.issues.is_empty());
        }
        let resolution_only = parse_profile(r#"{"resolutions":[1080]}"#);
        assert!(
            resolution_only
                .assess("Example 1080p H.266", "Example")
                .accepted
        );
    }

    #[test]
    fn unknown_allowance_does_not_allow_an_explicitly_disallowed_marker() {
        let profile = parse_profile(
            r#"{"resolutions":[1080],"allow_unknown_resolution":true,"languages":["en"],"allow_unknown_language":true}"#,
        );
        assert!(profile.assess("Example 2026", "Example").accepted);
        assert!(!profile.assess("Example 720p", "Example").accepted);
        assert!(profile.assess("Example 1080p FRE", "Example").accepted);
        let strict = parse_profile(r#"{"resolutions":[1080]}"#);
        assert!(!strict.assess("Example 2026", "Example").accepted);
    }

    #[test]
    fn ordered_preferences_and_custom_scores_have_explicit_priority() {
        let profile = parse_profile(
            r#"{"resolutions":[1080,2160],"sources":["web-dl","bluray"],"codecs":["h265","h264"],"score_rules":[{"terms":["proper","release group"],"score":10}]}"#,
        );
        let preferred = profile.assess("Example 1080p WEB-DL HEVC ENG", "Example");
        let other = profile.assess("Example 2160p BluRay AVC FRE", "Example");
        assert!(preferred.accepted && other.accepted);
        assert!(preferred.rank > other.rank);
        let scored = profile.assess(
            "Example 2160p BluRay AVC FRE PROPER release.group",
            "Example",
        );
        assert_eq!(scored.rank.custom_score, 10);
        assert!(scored.rank < preferred.rank);
        assert_eq!(
            profile
                .assess(
                    "Example 2160p BluRay AVC FRE improper release.group",
                    "Example"
                )
                .rank
                .custom_score,
            0
        );
        assert_eq!(
            profile
                .assess("Example 2160p BluRay AVC FRE proper other.group", "Example")
                .rank
                .custom_score,
            0
        );
    }

    #[test]
    fn phrase_filters_and_score_thresholds_are_token_based() {
        let profile = parse_profile(
            r#"{"required_terms":["release group"],"blocked_terms":["cam"],"score_rules":[{"terms":["proper"],"score":5},{"terms":["repack"],"score":-3}],"minimum_score":4}"#,
        );
        assert!(
            profile
                .assess("Example Release.Group Proper camera", "Example")
                .accepted
        );
        assert!(
            !profile
                .assess("Example Release.Group Proper CAM", "Example")
                .accepted
        );
        assert!(
            !profile
                .assess("Example Release.Group Proper Repack", "Example")
                .accepted
        );
        assert!(
            !profile
                .assess("Example Release Other Group Proper", "Example")
                .accepted
        );
    }

    #[test]
    fn invalid_and_oversized_configuration_is_rejected() {
        for invalid in [
            r#"{"resolutions":[1080,1080]}"#,
            r#"{"resolutions":[1080.5]}"#,
            r#"{"resolutions":[4320]}"#,
            r#"{"sources":["WEB-DL"]}"#,
            r#"{"codecs":["h266"]}"#,
            r#"{"languages":["english"]}"#,
            r#"{"allow_unknown_language":1}"#,
            r#"{"required_terms":[""]}"#,
            r#"{"blocked_terms":[" "]}"#,
            r#"{"required_terms":["WEB-DL","web dl"]}"#,
            r#"{"score_rules":[{"terms":[],"score":1}]}"#,
            r#"{"score_rules":[{"terms":["proper"]}]}"#,
            r#"{"score_rules":[{"terms":["proper"],"score":100001}]}"#,
            r#"{"score_rules":[{"terms":["a","b"],"score":1},{"terms":["B","A"],"score":2}]}"#,
            r#"{"minimum_score":-2147483648}"#,
        ] {
            assert!(
                Profile::from_json(&json::parse(invalid).unwrap()).is_err(),
                "{invalid}"
            );
        }
        assert!(
            Profile::from_json(&object([(
                "required_terms",
                Value::Array(vec!["a".into(); 33])
            )]))
            .is_err()
        );
        assert!(
            Profile::from_json(&object([(
                "required_terms",
                Value::Array(vec!["a".repeat(129).into()])
            )]))
            .is_err()
        );
        for invalid in [r#"{"profiles":{}}"#, r#"{"movie_profile":"missing"}"#] {
            assert!(SelectionConfig::from_json(&json::parse(invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn explicit_profile_references_round_trip_and_are_validated() {
        let value = json::parse(r#"{"profiles":{"hd":{"resolutions":[1080,720]}},"movie_profile":"hd","episode_profile":"hd"}"#).unwrap();
        let config = SelectionConfig::from_json(&value).unwrap();
        assert_eq!(config.profile("movie").unwrap().0, "hd");
        assert_eq!(
            SelectionConfig::from_json(&config.to_json()).unwrap(),
            config
        );
        assert!(config.profile("show").is_err());
        let oversized = Value::Object(
            (0..65)
                .map(|i| (format!("profile-{i}"), Value::object()))
                .collect(),
        );
        assert!(SelectionConfig::from_json(&object([("profiles", oversized)])).is_err());
    }

    #[test]
    fn oversized_titles_are_rejected_without_scanning_markers() {
        let assessment = Profile::default().assess(&"x".repeat(4097), "Example");
        assert!(!assessment.accepted);
        assert_eq!(assessment.attributes.resolution, None);
    }
}
