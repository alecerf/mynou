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
const LANGUAGES: &[&str] = &["en", "fr", "de", "es", "it", "ja", "ko", "zh", "pt", "ru"];

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
    pub fn from_json(value: &Value) -> Result<Self> {
        check_keys(value, &["profiles", "movie_profile", "episode_profile"])?;
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// Ordered from most preferred to least preferred; an empty list is unrestricted.
    pub resolutions: Vec<u32>,
    /// Reaching this resolution or an earlier preference stops future upgrades.
    pub cutoff_resolution: Option<u32>,
    pub sources: Vec<String>,
    pub codecs: Vec<String>,
    pub languages: Vec<String>,
    pub allow_unknown_resolution: bool,
    pub allow_unknown_source: bool,
    pub allow_unknown_codec: bool,
    pub allow_unknown_language: bool,
    pub required_terms: Vec<String>,
    pub blocked_terms: Vec<String>,
    pub score_rules: Vec<ScoreRule>,
    pub minimum_score: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScoreRule {
    /// Every token phrase must occur for this rule to contribute its score.
    pub terms: Vec<String>,
    pub score: i32,
}

impl Profile {
    pub fn from_json(value: &Value) -> Result<Self> {
        check_keys(
            value,
            &[
                "resolutions",
                "cutoff_resolution",
                "sources",
                "codecs",
                "languages",
                "allow_unknown_resolution",
                "allow_unknown_source",
                "allow_unknown_codec",
                "allow_unknown_language",
                "required_terms",
                "blocked_terms",
                "score_rules",
                "minimum_score",
            ],
        )?;
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
            check_keys(rule, &["terms", "score"])?;
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
            resolutions,
            cutoff_resolution,
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
        object([
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
        ])
    }

    pub fn cutoff_reached(&self, assessment: &Assessment) -> bool {
        if !assessment.accepted {
            return false;
        }
        let cutoff = self
            .cutoff_resolution
            .and_then(|cutoff| self.resolutions.iter().position(|value| *value == cutoff));
        let current = assessment
            .attributes
            .resolution
            .and_then(|resolution| self.resolutions.iter().position(|value| *value == resolution));
        match (current, cutoff) {
            (Some(current), Some(cutoff)) => current <= cutoff,
            _ => false,
        }
    }

    pub fn assess(&self, release_title: &str, media_title: &str) -> Assessment {
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
        let language = if self.languages.is_empty() {
            0
        } else if attributes.languages.is_empty() {
            if !self.allow_unknown_language {
                reasons.push("Language is unknown".into());
            }
            0
        } else {
            let rank = self
                .languages
                .iter()
                .position(|language| attributes.languages.contains(language));
            if let Some(index) = rank {
                (self.languages.len() - index) as u32
            } else {
                reasons
                    .push("No explicitly marked audio language is allowed by the profile".into());
                0
            }
        };
        Assessment {
            accepted: reasons.is_empty(),
            reasons,
            rank: Rank {
                custom_score,
                resolution,
                source,
                codec,
                language,
            },
            attributes,
        }
    }
}

/// Lexicographic priority: custom score, resolution, source, codec, then language.
/// Larger values win. Empty preference lists contribute zero, preserving seed ordering.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    pub custom_score: i32,
    pub resolution: u32,
    pub source: u32,
    pub codec: u32,
    pub language: u32,
}

impl Rank {
    pub fn to_json(&self) -> Value {
        object([
            ("custom_score", number(self.custom_score)),
            ("resolution", self.resolution.into()),
            ("source", self.source.into()),
            ("codec", self.codec.into()),
            ("language", self.language.into()),
        ])
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    pub resolution: Option<u32>,
    pub source: Option<String>,
    pub codec: Option<String>,
    pub languages: Vec<String>,
    pub issues: Vec<String>,
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
}

impl Assessment {
    pub fn to_json(&self) -> Value {
        object([
            ("accepted", self.accepted.into()),
            ("reasons", strings(&self.reasons)),
            ("rank", self.rank.to_json()),
            ("attributes", self.attributes.to_json()),
        ])
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

fn check_keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let map = value.as_object().ok_or("Selection: expected an object")?;
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Selection: unknown field {key}"));
        }
    }
    Ok(())
}
fn validate_profile_name(name: &str) -> Result<()> {
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
        if let Some(language) = language_marker(token) {
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
            if explicitly_audio || !subtitle_context {
                languages.insert(language);
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
fn subtitle_language_positions(tokens: &[String]) -> Vec<bool> {
    let mut positions = vec![false; tokens.len()];
    for (index, token) in tokens.iter().enumerate() {
        if !is_subtitle_marker(token) {
            continue;
        }
        // Subtitle labels scope a contiguous language list in either direction.
        // Audio and other release markers form boundaries.
        for (next, token) in tokens.iter().enumerate().skip(index + 1) {
            if language_marker(token).is_none() {
                break;
            }
            positions[next] = true;
        }
        for (previous, token) in tokens[..index].iter().enumerate().rev() {
            if language_marker(token).is_none() {
                break;
            }
            positions[previous] = true;
        }
    }
    positions
}

fn language_marker(token: &str) -> Option<&'static str> {
    // VOSTFR, SUBFR, MULTI and DUAL provide no evidence of an audio language.
    if token.starts_with("vost")
        || token.starts_with("sub")
        || token.ends_with("subs")
        || token.ends_with("sdh")
    {
        return None;
    }
    let token = token
        .strip_prefix("audio")
        .or_else(|| token.strip_suffix("audio"))
        .unwrap_or(token);
    match token {
        "en" | "eng" | "english" => Some("en"),
        "fr" | "fre" | "fra" | "french" | "truefrench" | "vff" | "vfq" | "vfi" => Some("fr"),
        "de" | "ger" | "deu" | "german" => Some("de"),
        "es" | "spa" | "esp" | "spanish" => Some("es"),
        "it" | "ita" | "italian" => Some("it"),
        "ja" | "jpn" | "jap" | "japanese" => Some("ja"),
        "ko" | "kor" | "korean" => Some("ko"),
        "zh" | "chi" | "zho" | "chinese" | "mandarin" | "cantonese" => Some("zh"),
        "pt" | "por" | "portuguese" => Some("pt"),
        "ru" | "rus" | "russian" => Some("ru"),
        _ => None,
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
            assert!(assessment.attributes.languages.is_empty(), "{marker}");
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
        assert!(!profile.assess("Example 1080p FRE", "Example").accepted);
        let strict = parse_profile(r#"{"resolutions":[1080]}"#);
        assert!(!strict.assess("Example 2026", "Example").accepted);
    }

    #[test]
    fn ordered_preferences_and_custom_scores_have_explicit_priority() {
        let profile = parse_profile(
            r#"{"resolutions":[1080,2160],"sources":["web-dl","bluray"],"codecs":["h265","h264"],"languages":["en","fr"],"score_rules":[{"terms":["proper","release group"],"score":10}]}"#,
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
        assert!(scored.rank > preferred.rank);
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
            r#"{"unknown":true}"#,
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
        for invalid in [
            r#"{"profiles":{}}"#,
            r#"{"movie_profile":"missing"}"#,
            r#"{"extra":true}"#,
        ] {
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
