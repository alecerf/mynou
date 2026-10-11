//! Declarative field mapping for `json` sources. Any indexer that answers a
//! search with a JSON list can be read from configuration alone: no provider
//! code, no scripts and no helper programs.
use super::{text, token};
use crate::{Result, json::Value, numbering};

const MAX_DEPTH: usize = 8;
const MAX_ALTERNATIVES: usize = 4;
const MAX_FIXED: usize = 8;
const MAX_VALUE: usize = 256;
/// Request roles that can be renamed or omitted, with the name used by default.
const ROLES: [(&str, &str); 7] = [
    ("query", "q"),
    ("kind", "kind"),
    ("year", "year"),
    ("season", "season"),
    ("episode", "episode"),
    ("absolute", "absolute"),
    ("api_key", "apikey"),
];
const LEGACY_URLS: [&str; 5] = ["download_url", "magnet", "magnet_url", "url", "link"];

/// A dot-separated path through objects, where a numeric part indexes an array.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Path(Vec<String>);

impl Path {
    fn parse(text: &str) -> Result<Self> {
        let parts: Vec<String> = text.split('.').map(str::to_owned).collect();
        if parts.len() > MAX_DEPTH || !parts.iter().all(|part| token(part)) {
            return Err("Indexer: invalid mapping path".into());
        }
        Ok(Self(parts))
    }
    fn find<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        self.0.iter().try_fold(root, |value, part| match value {
            Value::Array(items) => items.get(part.parse::<usize>().ok()?),
            _ => value.get(part),
        })
    }
}

fn paths(v: &Value, key: &str, limit: usize) -> Result<Vec<Path>> {
    match v.get(key) {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![Path::parse(s)?]),
        Some(Value::Array(list)) if !list.is_empty() && list.len() <= limit => list
            .iter()
            .map(|item| Path::parse(item.as_str().ok_or("Indexer: invalid mapping path")?))
            .collect(),
        _ => Err("Indexer: invalid mapping path".into()),
    }
}

fn parameter(s: &str) -> bool {
    s.is_empty()
        || (s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')))
}

fn plain(s: &str) -> bool {
    s.len() <= MAX_VALUE && !s.chars().any(char::is_control)
}

fn count(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

/// How a `json` source is queried and read. The default reproduces the built-in
/// behavior, so sources without a `mapping` section are unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mapping {
    results: Option<Path>,
    title: Option<Path>,
    urls: Vec<Path>,
    seeders: Option<Path>,
    names: Vec<(String, String)>,
    kinds: Vec<(String, String)>,
    fixed: Vec<(String, String)>,
    canonical: String,
}

impl Mapping {
    pub(crate) fn from_json(source: &Value) -> Result<Self> {
        let Some(v) = source.get("mapping") else {
            return Ok(Self::default());
        };
        numbering::only(
            v,
            &[
                "results",
                "title",
                "url",
                "seeders",
                "params",
                "kind_values",
                "fixed_params",
            ],
        )?;
        if source.get("kind").and_then(Value::as_str).unwrap_or("json") != "json" {
            return Err("Indexer: a mapping needs a json source".into());
        }
        let single = |key| -> Result<Option<Path>> { Ok(paths(v, key, 1)?.pop()) };
        let mut mapping = Self {
            results: single("results")?,
            title: single("title")?,
            urls: paths(v, "url", MAX_ALTERNATIVES)?,
            seeders: single("seeders")?,
            ..Self::default()
        };
        if let Some(params) = v.get("params") {
            let roles: Vec<&str> = ROLES.iter().map(|(role, _)| *role).collect();
            numbering::only(params, &roles)?;
            for (role, _) in ROLES {
                if params.get(role).is_some() {
                    let name = text(params, role)?;
                    if !parameter(&name) {
                        return Err("Indexer: invalid mapping parameter name".into());
                    }
                    mapping.names.push((role.into(), name));
                }
            }
        }
        if let Some(kinds) = v.get("kind_values") {
            numbering::only(kinds, &["movie", "episode", "series"])?;
            for kind in ["movie", "episode", "series"] {
                if kinds.get(kind).is_some() {
                    let value = text(kinds, kind)?;
                    if !plain(&value) {
                        return Err("Indexer: invalid mapping kind value".into());
                    }
                    mapping.kinds.push((kind.into(), value));
                }
            }
        }
        if let Some(fixed) = v.get("fixed_params") {
            let map = fixed
                .as_object()
                .filter(|map| map.len() <= MAX_FIXED)
                .ok_or("Indexer: invalid fixed parameters")?;
            for (name, value) in map {
                let value = value.as_str().ok_or("Indexer: invalid fixed parameters")?;
                if name.is_empty() || !parameter(name) || !plain(value) {
                    return Err("Indexer: invalid fixed parameters".into());
                }
                mapping.fixed.push((name.clone(), value.to_owned()));
            }
        }
        let mut used: Vec<&str> = ROLES
            .iter()
            .filter_map(|(role, _)| mapping.parameter(role))
            .collect();
        used.extend(mapping.fixed.iter().map(|(name, _)| name.as_str()));
        let total = used.len();
        used.sort_unstable();
        used.dedup();
        if used.len() != total {
            return Err("Indexer: mapping parameter names must be distinct".into());
        }
        mapping.canonical = crate::json::stringify(v);
        Ok(mapping)
    }

    /// Stable text bound into the source identity, empty without a mapping.
    pub(crate) fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Query parameter name for a request role, or `None` when it is omitted.
    pub(crate) fn parameter(&self, role: &str) -> Option<&str> {
        let name = self
            .names
            .iter()
            .find(|(r, _)| r == role)
            .map(|(_, name)| name.as_str())
            .or_else(|| {
                ROLES
                    .iter()
                    .find(|(r, _)| *r == role)
                    .map(|(_, name)| *name)
            })?;
        (!name.is_empty()).then_some(name)
    }

    /// The value sent for a request kind (`movie`, `episode` or `series`).
    pub(crate) fn kind_value<'a>(&'a self, kind: &'a str) -> &'a str {
        self.kinds
            .iter()
            .find(|(k, _)| k == kind)
            .map_or(kind, |(_, value)| value.as_str())
    }

    pub(crate) fn fixed(&self) -> &[(String, String)] {
        &self.fixed
    }

    pub(crate) fn list<'a>(&self, root: &'a Value) -> Option<&'a [Value]> {
        match &self.results {
            Some(path) => path.find(root)?.as_array(),
            None => root
                .as_array()
                .or_else(|| root.get("results").and_then(Value::as_array))
                .or_else(|| root.get("items").and_then(Value::as_array)),
        }
    }

    pub(crate) fn title<'a>(&self, item: &'a Value) -> Option<&'a str> {
        match &self.title {
            Some(path) => path.find(item)?.as_str(),
            None => item.get("title")?.as_str(),
        }
    }

    pub(crate) fn url<'a>(&self, item: &'a Value) -> Option<&'a str> {
        if self.urls.is_empty() {
            LEGACY_URLS.iter().find_map(|key| item.get(key)?.as_str())
        } else {
            self.urls.iter().find_map(|path| path.find(item)?.as_str())
        }
    }

    pub(crate) fn seeders(&self, item: &Value) -> u64 {
        match &self.seeders {
            Some(path) => path.find(item).and_then(count),
            None => item
                .get("seeders")
                .and_then(count)
                .or_else(|| item.get("seeders_count").and_then(count)),
        }
        .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn source(mapping: &str) -> Value {
        json::parse(&format!(
            r#"{{"kind":"json","url":"https://x.test/api","mapping":{mapping}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn absent_mapping_keeps_the_built_in_reader() {
        let none = Mapping::from_json(&json::parse(r#"{"kind":"json"}"#).unwrap()).unwrap();
        assert_eq!(none, Mapping::default());
        assert_eq!(none.canonical(), "");
        assert_eq!(none.parameter("query"), Some("q"));
        assert_eq!(none.parameter("api_key"), Some("apikey"));
        let page =
            json::parse(r#"{"results":[{"title":"A","magnet":"m","seeders_count":"7"}]}"#).unwrap();
        let item = &none.list(&page).unwrap()[0];
        assert_eq!(none.title(item), Some("A"));
        assert_eq!(none.url(item), Some("m"));
        assert_eq!(none.seeders(item), 7);
    }

    #[test]
    fn paths_follow_objects_and_array_indexes() {
        let mapping = Mapping::from_json(&source(
            r#"{"results":"data.torrents","title":"meta.name","url":["links.0","magnet"],"seeders":"stats.up"}"#,
        ))
        .unwrap();
        let page = json::parse(
            r#"{"data":{"torrents":[{"meta":{"name":"N"},"links":["https://x.test/d/1"],"stats":{"up":12}}]}}"#,
        )
        .unwrap();
        let item = &mapping.list(&page).unwrap()[0];
        assert_eq!(mapping.title(item), Some("N"));
        assert_eq!(mapping.url(item), Some("https://x.test/d/1"));
        assert_eq!(mapping.seeders(item), 12);
        assert!(
            mapping
                .list(&json::parse(r#"{"data":[]}"#).unwrap())
                .is_none()
        );
        assert!(!mapping.canonical().is_empty());
    }

    #[test]
    fn parameters_are_renamed_omitted_and_fixed() {
        let mapping = Mapping::from_json(&source(
            r#"{"params":{"query":"search","year":"","api_key":"key"},"kind_values":{"movie":"2000"},"fixed_params":{"limit":"100"}}"#,
        ))
        .unwrap();
        assert_eq!(mapping.parameter("query"), Some("search"));
        assert_eq!(mapping.parameter("year"), None);
        assert_eq!(mapping.parameter("api_key"), Some("key"));
        assert_eq!(mapping.parameter("season"), Some("season"));
        assert_eq!(mapping.kind_value("movie"), "2000");
        assert_eq!(mapping.kind_value("episode"), "episode");
        assert_eq!(mapping.fixed(), [("limit".to_owned(), "100".to_owned())]);
    }

    #[test]
    fn invalid_mappings_are_rejected() {
        for mapping in [
            r#"{"unknown":"x"}"#,
            r#"{"title":""}"#,
            r#"{"title":"a..b"}"#,
            r#"{"title":"a.b.c.d.e.f.g.h.i"}"#,
            r#"{"url":[]}"#,
            r#"{"url":["a","b","c","d","e"]}"#,
            r#"{"results":["a","b"]}"#,
            r#"{"params":{"unknown":"x"}}"#,
            r#"{"params":{"query":"a b"}}"#,
            r#"{"params":{"query":"same","year":"same"}}"#,
            r#"{"fixed_params":{"q":"x"}}"#,
            r#"{"fixed_params":{"cat":1}}"#,
            r#"{"fixed_params":{"cat":"a\nb"}}"#,
            r#"{"kind_values":{"album":"x"}}"#,
        ] {
            assert!(Mapping::from_json(&source(mapping)).is_err(), "{mapping}");
        }
        let torznab = json::parse(r#"{"kind":"torznab","mapping":{}}"#).unwrap();
        assert!(Mapping::from_json(&torznab).is_err());
    }
}
