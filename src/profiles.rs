//! Durable per-title release profiles in `profiles.idx`, for #338.
//!
//! A binding names the selection profile of one movie or one tracked series by
//! its TMDB identity. It stores only the profile name: the profile itself comes
//! from the current configuration, so an unknown name fails selection instead
//! of silently selecting a default. Precedence is the title binding, then the
//! requester's captured profile, then the global movie or episode profile.
//!
//! The store has its own lock, taken last: callers may hold requester or job
//! locks while reading a binding, and nothing else is locked while it is held.
use crate::{
    Result,
    config::Config,
    crypto::random_bytes,
    engine::{Engine, lock},
    inventory::{Change, Index, Key, MAX_PAGE_ROWS, Query},
    json::{self, Value},
    numbering::only,
    requesters::Capture,
    selection::Profile,
    store::{Request, reject_symlinks, sync_directory},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Bindings retained in one store; see docs/limits.md.
pub const MAX_BINDINGS: u64 = 100_000;
/// Titles in one reviewed import plan, below the inventory transaction bound.
pub const MAX_IMPORT_TITLES: usize = 5_000;
const MAX_SOURCE_PROFILES: usize = 256;
const MAX_SOURCE_NAME_BYTES: usize = 128;
const INDEX: &str = "profiles.idx";
const STAGING: &str = "profiles.idx.staging";
const FORMAT: &[u8] = b"mynou-title-profiles/1";
const HEADER: Key = [0; 64];
const T_BINDING: u8 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Movie,
    Series,
}

/// The catalog identity a binding applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Title {
    pub kind: Kind,
    pub tmdb_id: u64,
}

impl Title {
    /// Movies bind by their own identity; episodes and season packs by their series.
    pub fn of(request: &Request) -> Option<Self> {
        let kind = match request.kind.as_str() {
            "movie" => Kind::Movie,
            "episode" | "series" => Kind::Series,
            _ => return None,
        };
        request
            .tmdb_id
            .filter(|id| valid_tmdb(*id))
            .map(|tmdb_id| Self { kind, tmdb_id })
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        let kind = match v.get("kind").and_then(Value::as_str) {
            Some("movie") => Kind::Movie,
            Some("series") => Kind::Series,
            _ => return Err("Title profile: kind must be movie or series".into()),
        };
        let tmdb_id = v
            .get("tmdb_id")
            .and_then(Value::as_u64)
            .filter(|id| valid_tmdb(*id))
            .ok_or("Title profile: invalid TMDB identity")?;
        Ok(Self { kind, tmdb_id })
    }
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Movie => "movie",
            Kind::Series => "series",
        }
    }
    fn insert(&self, v: &mut Value) {
        v.insert("kind", self.kind_name());
        v.insert("tmdb_id", Value::Number(self.tmdb_id as f64));
    }
    fn key(&self) -> Key {
        let mut key = [0; 64];
        key[0] = T_BINDING;
        key[1] = match self.kind {
            Kind::Movie => 1,
            Kind::Series => 2,
        };
        key[2..10].copy_from_slice(&self.tmdb_id.to_be_bytes());
        key
    }
    fn from_key(key: &Key) -> Result<Self> {
        let kind = match (key[0], key[1]) {
            (T_BINDING, 1) => Kind::Movie,
            (T_BINDING, 2) => Kind::Series,
            _ => return Err("Title profile storage: invalid binding key".into()),
        };
        let mut id = [0; 8];
        id.copy_from_slice(&key[2..10]);
        let tmdb_id = u64::from_be_bytes(id);
        if !valid_tmdb(tmdb_id) || key[10..].iter().any(|b| *b != 0) {
            return Err("Title profile storage: invalid binding key".into());
        }
        Ok(Self { kind, tmdb_id })
    }
}

fn valid_tmdb(id: u64) -> bool {
    id != 0 && id <= 9_007_199_254_740_991
}

fn valid_name(name: &str) -> bool {
    crate::selection::validate_profile_name(name).is_ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub title: Title,
    pub profile: String,
    /// `operator` for an explicit change, `migration` for an imported assignment.
    pub origin: String,
    pub updated_at: u64,
}

impl Binding {
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        self.title.insert(&mut v);
        v.insert("profile", self.profile.clone());
        v.insert("origin", self.origin.clone());
        v.insert("updated_at", self.updated_at.to_string());
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        only(v, &["kind", "tmdb_id", "profile", "origin", "updated_at"])
            .map_err(|_| "Title profile storage: unknown binding field")?;
        let binding = Self {
            title: Title::from_json(v)?,
            profile: v
                .get("profile")
                .and_then(Value::as_str)
                .filter(|name| valid_name(name))
                .ok_or("Title profile storage: invalid profile name")?
                .into(),
            origin: v
                .get("origin")
                .and_then(Value::as_str)
                .filter(|o| matches!(*o, "operator" | "migration"))
                .ok_or("Title profile storage: invalid origin")?
                .into(),
            updated_at: v
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(|t| t.parse().ok())
                .ok_or("Title profile storage: invalid timestamp")?,
        };
        Ok(binding)
    }
}

/// The selected profile for a request kind under a binding and requester capture.
pub(crate) fn resolve<'a>(
    config: &'a Config,
    kind: &str,
    capture: Option<&'a Capture>,
    binding: Option<&'a str>,
) -> Result<(&'a str, &'a Profile)> {
    let Some(name) = binding else {
        return match capture {
            Some(c) => Ok((&c.profile_name, &c.profile)),
            None => config
                .selection
                .profile(if kind == "series" { "episode" } else { kind }),
        };
    };
    config
        .selection
        .profiles
        .get_key_value(name)
        .map(|(name, profile)| (name.as_str(), profile))
        .ok_or_else(|| format!("Selection: unknown profile {name}"))
}

/// Points a request kind's profile slot at the bound name.
pub fn bind(config: &mut Config, kind: &str, name: &str) {
    if kind == "movie" {
        config.selection.movie_profile = name.into();
    } else {
        config.selection.episode_profile = name.into();
    }
}

pub struct TitleProfiles {
    directory: PathBuf,
    index: Option<Index>,
    read_only: bool,
}

fn exists(path: &Path) -> Result<bool> {
    reject_symlinks(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Title profile storage: cannot inspect storage path".into()),
    }
}

impl TitleProfiles {
    /// Opens `profiles.idx` when present; the first write creates it.
    pub fn open(directory: &Path, read_only: bool) -> Result<Self> {
        let index = if exists(&directory.join(INDEX))? {
            let mut index = Index::open(&directory.join(INDEX), read_only)?;
            if index.get(&HEADER)?.as_deref() != Some(FORMAT) {
                return Err("Title profile storage: unsupported format".into());
            }
            Some(index)
        } else {
            None
        };
        Ok(Self {
            directory: directory.to_owned(),
            index,
            read_only,
        })
    }

    pub fn get(&mut self, title: &Title) -> Result<Option<Binding>> {
        let Some(index) = &mut self.index else {
            return Ok(None);
        };
        index
            .get(&title.key())?
            .map(|bytes| decode(title, &bytes))
            .transpose()
    }

    /// Changes on every commit; zero before the store exists.
    pub fn sequence(&self) -> Result<u64> {
        self.index.as_ref().map_or(Ok(0), Index::sequence)
    }

    pub fn count(&self) -> Result<u64> {
        self.index
            .as_ref()
            .map_or(Ok(0), |index| Ok(index.len()?.saturating_sub(1)))
    }

    /// Bindings in kind and TMDB order after the exclusive cursor.
    pub fn page(
        &mut self,
        after: Option<&Title>,
        limit: usize,
    ) -> Result<(Vec<Binding>, Option<Title>)> {
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err("Title profile: page limit must be between 1 and 200".into());
        }
        let Some(index) = &mut self.index else {
            return Ok((Vec::new(), None));
        };
        let mut start = [0; 64];
        start[0] = T_BINDING;
        let mut end = [0; 64];
        end[0] = T_BINDING + 1;
        let page = index.page(&Query {
            start,
            end: Some(end),
            after: after.map(Title::key),
            limit,
            sequence: None,
        })?;
        let rows = page
            .entries
            .iter()
            .map(|(key, bytes)| decode(&Title::from_key(key)?, bytes))
            .collect::<Result<Vec<_>>>()?;
        let next = page.next.as_ref().map(Title::from_key).transpose()?;
        Ok((rows, next))
    }

    /// Replaces or removes bindings in one recoverable commit.
    pub(crate) fn commit(&mut self, changes: &[(Title, Option<Binding>)]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut added = 0_u64;
        let mut removed = 0_u64;
        let mut batch = Vec::with_capacity(changes.len());
        for (title, binding) in changes {
            let present = self.get(title)?.is_some();
            match binding {
                Some(binding) => {
                    added += u64::from(!present);
                    batch.push(Change::Put(
                        title.key(),
                        json::stringify(&binding.to_json()).into_bytes(),
                    ));
                }
                None if present => {
                    removed += 1;
                    batch.push(Change::Delete(title.key()));
                }
                None => {}
            }
        }
        if batch.is_empty() {
            return Ok(());
        }
        if (self.count()? + added).saturating_sub(removed) > MAX_BINDINGS {
            return Err(format!(
                "Title profile: more than {MAX_BINDINGS} bindings; nothing was saved"
            ));
        }
        self.writable()?.commit(batch)?;
        Ok(())
    }

    fn writable(&mut self) -> Result<&mut Index> {
        if self.read_only {
            return Err("Title profile storage is open read-only; the change was not saved".into());
        }
        if self.index.is_none() {
            self.index = Some(self.create()?);
        }
        self.index
            .as_mut()
            .ok_or_else(|| "Title profile storage is unavailable".into())
    }

    /// Publishes a complete new index atomically; unfinished attempts are kept.
    fn create(&self) -> Result<Index> {
        let staging = self.directory.join(STAGING);
        if exists(&staging)? {
            let preserved = self.directory.join(format!(
                "profiles.idx.abandoned-{}",
                crate::requesters::digest(&random_bytes::<16>()?)
            ));
            fs::rename(&staging, &preserved)
                .map_err(|_| "Title profile storage: cannot preserve unfinished staging data")?;
        }
        let mut index = Index::create(&staging)?;
        index.commit(vec![Change::Put(HEADER, FORMAT.to_vec())])?;
        drop(index);
        if exists(&self.directory.join(INDEX))? {
            return Err("Title profile storage: index appeared before promotion".into());
        }
        fs::rename(&staging, self.directory.join(INDEX)).map_err(|error| {
            crate::store::migration_io("Title profile storage", "write", &error)
        })?;
        sync_directory(&self.directory)
            .map_err(|error| crate::store::migration_sync("Title profile storage", &error))?;
        Index::open(&self.directory.join(INDEX), false)
    }
}

fn decode(title: &Title, bytes: &[u8]) -> Result<Binding> {
    let binding = Binding::from_json(&json::parse(
        std::str::from_utf8(bytes).map_err(|_| "Title profile storage: invalid UTF-8")?,
    )?)?;
    if binding.title != *title {
        return Err("Title profile storage: binding differs from its key".into());
    }
    Ok(binding)
}

/// One reviewed change: preview first, then apply the returned `plan_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub action: String,
    pub title: Option<Title>,
    pub profile: Option<String>,
    pub import: Option<Import>,
    pub apply: bool,
    pub plan_id: Option<String>,
}

/// Source-manager profile names mapped to Mynou profiles, and per-title assignments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// `None` leaves every title of that source profile explicitly unresolved.
    pub profiles: BTreeMap<String, Option<String>>,
    pub titles: Vec<(Title, String)>,
}

impl ControlRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(
            v,
            &[
                "action", "kind", "tmdb_id", "profile", "import", "apply", "plan_id",
            ],
        )
        .map_err(|_| "Title profile: unknown control field")?;
        let action = v
            .get("action")
            .and_then(Value::as_str)
            .ok_or("Title profile: missing action")?
            .to_owned();
        let title = if v.get("kind").is_some() || v.get("tmdb_id").is_some() {
            Some(Title::from_json(v)?)
        } else {
            None
        };
        let profile = match v.get("profile") {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) if valid_name(name) => Some(name.clone()),
            Some(_) => return Err("Title profile: invalid profile name".into()),
        };
        let import = v.get("import").map(Import::from_json).transpose()?;
        let apply = match v.get("apply") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or("Title profile: apply must be a boolean")?,
        };
        let plan_id = match v.get("plan_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id))
                if id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Some(id.clone())
            }
            Some(_) => return Err("Title profile: invalid plan ID".into()),
        };
        let request = Self {
            action,
            title,
            profile,
            import,
            apply,
            plan_id,
        };
        let valid = match request.action.as_str() {
            "set" => {
                request.title.is_some() && request.profile.is_some() && request.import.is_none()
            }
            "clear" => {
                request.title.is_some() && request.profile.is_none() && request.import.is_none()
            }
            "import" => {
                request.title.is_none() && request.profile.is_none() && request.import.is_some()
            }
            _ => false,
        };
        if !valid || request.apply != request.plan_id.is_some() {
            return Err("Title profile: invalid control scope or guard".into());
        }
        Ok(request)
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("action", self.action.clone());
        if let Some(title) = &self.title {
            title.insert(&mut v);
        }
        if let Some(profile) = &self.profile {
            v.insert("profile", profile.clone());
        }
        if let Some(import) = &self.import {
            v.insert("import", import.to_json());
        }
        v.insert("apply", self.apply);
        if let Some(id) = &self.plan_id {
            v.insert("plan_id", id.clone());
        }
        v
    }
}

impl Import {
    pub fn from_json(v: &Value) -> Result<Self> {
        only(v, &["profiles", "titles"]).map_err(|_| "Title profile import: unknown field")?;
        let source = v
            .get("profiles")
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty() && m.len() <= MAX_SOURCE_PROFILES)
            .ok_or("Title profile import: profiles must map 1 to 256 source names")?;
        let mut profiles = BTreeMap::new();
        for (name, target) in source {
            if !valid_source_name(name) {
                return Err("Title profile import: invalid source profile name".into());
            }
            let target = match target {
                Value::Null => None,
                Value::String(t) if valid_name(t) => Some(t.clone()),
                _ => return Err("Title profile import: invalid target profile name".into()),
            };
            profiles.insert(name.clone(), target);
        }
        let rows = v
            .get("titles")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() <= MAX_IMPORT_TITLES)
            .ok_or("Title profile import: titles must list at most 5000 assignments")?;
        let mut titles = Vec::with_capacity(rows.len());
        for row in rows {
            only(row, &["kind", "tmdb_id", "profile"])
                .map_err(|_| "Title profile import: unknown title field")?;
            let profile = row
                .get("profile")
                .and_then(Value::as_str)
                .filter(|name| valid_source_name(name))
                .ok_or("Title profile import: invalid source profile name")?;
            titles.push((Title::from_json(row)?, profile.to_owned()));
        }
        Ok(Self { profiles, titles })
    }
    fn to_json(&self) -> Value {
        let mut profiles = Value::object();
        for (name, target) in &self.profiles {
            profiles.insert(
                name.clone(),
                target.clone().map_or(Value::Null, Value::String),
            );
        }
        let titles = self
            .titles
            .iter()
            .map(|(title, profile)| {
                let mut v = Value::object();
                title.insert(&mut v);
                v.insert("profile", profile.clone());
                v
            })
            .collect();
        let mut v = Value::object();
        v.insert("profiles", profiles);
        v.insert("titles", Value::Array(titles));
        v
    }
}

fn valid_source_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_SOURCE_NAME_BYTES && !name.chars().any(char::is_control)
}

/// What a plan proposes for one title, and why an assignment stays unresolved.
enum Outcome {
    Change(Title, Option<String>),
    Unresolved(Title, String, &'static str),
}

fn plan(config: &Config, request: &ControlRequest) -> Vec<Outcome> {
    let known = |name: &str| config.selection.profiles.contains_key(name);
    match (request.action.as_str(), &request.title, &request.import) {
        ("set", Some(title), _) => {
            let name = request.profile.clone().unwrap_or_default();
            if known(&name) {
                vec![Outcome::Change(*title, Some(name))]
            } else {
                vec![Outcome::Unresolved(*title, name, "unknown_profile")]
            }
        }
        ("clear", Some(title), _) => vec![Outcome::Change(*title, None)],
        ("import", _, Some(import)) => {
            let mut assigned: BTreeMap<Title, BTreeSet<&str>> = BTreeMap::new();
            for (title, source) in &import.titles {
                assigned.entry(*title).or_default().insert(source);
            }
            assigned
                .into_iter()
                .map(|(title, sources)| {
                    let source = sources.iter().next().copied().unwrap_or_default();
                    if sources.len() > 1 {
                        return Outcome::Unresolved(
                            title,
                            source.into(),
                            "conflicting_assignments",
                        );
                    }
                    match import.profiles.get(source) {
                        None => {
                            Outcome::Unresolved(title, source.into(), "unmapped_source_profile")
                        }
                        Some(None) => Outcome::Unresolved(
                            title,
                            source.into(),
                            "source_profile_left_unresolved",
                        ),
                        Some(Some(name)) if !known(name) => {
                            Outcome::Unresolved(title, source.into(), "unknown_profile")
                        }
                        Some(Some(name)) => Outcome::Change(title, Some(name.clone())),
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

impl Engine {
    /// The binding that applies to a request, if it has a catalog identity.
    pub(crate) fn title_binding(&self, request: &Request) -> Result<Option<String>> {
        let Some(title) = Title::of(request) else {
            return Ok(None);
        };
        Ok(lock(&self.title_profiles)?.get(&title)?.map(|b| b.profile))
    }

    /// A configuration whose kind profile follows the request's title binding.
    pub(crate) fn configuration_for_request(&self, request: &Request) -> Result<Config> {
        let mut config = self.config.clone();
        if let Some(name) = self.title_binding(request)? {
            bind(&mut config, &request.kind, &name);
        }
        Ok(config)
    }

    pub fn title_profiles(&self, after: Option<&Title>, limit: usize) -> Result<Value> {
        list(
            &self.config,
            &mut *lock(&self.title_profiles)?,
            after,
            limit,
        )
    }

    pub fn title_profile_control(&self, request: &ControlRequest) -> Result<Value> {
        if request.apply && self.read_only {
            return Err("Read-only preview cannot change title profiles".into());
        }
        control(&self.config, &mut *lock(&self.title_profiles)?, request)
    }

    /// Search preview under the title binding of the request, without recording it.
    pub fn search_preview(
        &self,
        request: &Request,
        page: &crate::integrations::SearchPage,
    ) -> Result<Value> {
        let binding = self.title_binding(request)?;
        preview(&self.config, request, binding, page)
    }
}

/// Search preview with an explicit binding; it records nothing.
pub fn preview(
    config: &Config,
    request: &Request,
    binding: Option<String>,
    page: &crate::integrations::SearchPage,
) -> Result<Value> {
    let mut config = config.clone();
    if let Some(name) = &binding {
        bind(&mut config, &request.kind, name);
    }
    let mut report = crate::integrations::search_report_page(&config, request, page)?;
    report.insert("title_profile", binding.map_or(Value::Null, Value::String));
    Ok(report)
}

/// The binding of a request read from a store opened without the service.
pub fn stored_binding(config: &Config, request: &Request) -> Result<Option<String>> {
    let Some(title) = Title::of(request) else {
        return Ok(None);
    };
    Ok(TitleProfiles::open(&config.store_dir, true)?
        .get(&title)?
        .map(|b| b.profile))
}

pub fn list(
    config: &Config,
    store: &mut TitleProfiles,
    after: Option<&Title>,
    limit: usize,
) -> Result<Value> {
    let (rows, next) = store.page(after, limit)?;
    let mut v = Value::object();
    v.insert("total", Value::Number(store.count()? as f64));
    v.insert("sequence", store.sequence()?.to_string());
    v.insert("movie_profile", config.selection.movie_profile.clone());
    v.insert("episode_profile", config.selection.episode_profile.clone());
    v.insert(
        "bindings",
        Value::Array(
            rows.iter()
                .map(|b| {
                    let mut v = b.to_json();
                    v.insert(
                        "resolved",
                        config.selection.profiles.contains_key(&b.profile),
                    );
                    v
                })
                .collect(),
        ),
    );
    v.insert(
        "next",
        next.map_or(Value::Null, |t| {
            Value::String(format!("{}:{}", t.kind_name(), t.tmdb_id))
        }),
    );
    Ok(v)
}

/// Previews a reviewed change, or applies it when the plan is unchanged.
pub fn control(
    config: &Config,
    store: &mut TitleProfiles,
    request: &ControlRequest,
) -> Result<Value> {
    let outcomes = plan(config, request);
    let at = crate::store::now();
    let origin = if request.action == "import" {
        "migration"
    } else {
        "operator"
    };
    let mut changes = Vec::new();
    let mut rows = Vec::new();
    let mut unresolved = Vec::new();
    let mut counts = [0_u64; 4];
    for outcome in &outcomes {
        match outcome {
            Outcome::Change(title, proposed) => {
                let current = store.get(title)?.map(|b| b.profile);
                let action = match (&current, proposed) {
                    (a, b) if a == b => "unchanged",
                    (None, Some(_)) => "bind",
                    (Some(_), Some(_)) => "rebind",
                    _ => "clear",
                };
                counts[match action {
                    "unchanged" => 0,
                    "bind" => 1,
                    "rebind" => 2,
                    _ => 3,
                }] += 1;
                let mut row = Value::object();
                title.insert(&mut row);
                row.insert(
                    "current",
                    current.clone().map_or(Value::Null, Value::String),
                );
                row.insert(
                    "proposed",
                    proposed.clone().map_or(Value::Null, Value::String),
                );
                row.insert("action", action);
                if rows.len() < MAX_PAGE_ROWS {
                    rows.push(row);
                }
                if action != "unchanged" {
                    changes.push((
                        *title,
                        proposed.as_ref().map(|profile| Binding {
                            title: *title,
                            profile: profile.clone(),
                            origin: origin.into(),
                            updated_at: at,
                        }),
                    ));
                }
            }
            Outcome::Unresolved(title, source, reason) => {
                let mut row = Value::object();
                title.insert(&mut row);
                row.insert(
                    "source_profile",
                    crate::integrations::report_text(source, 128),
                );
                row.insert("reason", *reason);
                row.insert(
                    "current",
                    store
                        .get(title)?
                        .map_or(Value::Null, |b| Value::String(b.profile)),
                );
                if unresolved.len() < MAX_PAGE_ROWS {
                    unresolved.push(row);
                }
            }
        }
    }
    // The guard covers the request, the stored bindings and the profiles it names.
    let mut guard = request.to_json();
    guard.insert("apply", false);
    guard.insert("plan_id", Value::Null);
    guard.insert("sequence", store.sequence()?.to_string());
    guard.insert("selection", config.selection.to_json());
    let plan_id = crate::requesters::digest(json::stringify(&guard).as_bytes());
    let unresolved_count = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Unresolved(..)))
        .count();
    if request.action != "import" && unresolved_count > 0 {
        return Err("Title profile: the selected profile is not configured".into());
    }
    if request.apply {
        if request.plan_id.as_deref() != Some(&plan_id) {
            return Err("Title profile: plan changed; preview again before applying".into());
        }
        store.commit(&changes)?;
    }
    let mut v = Value::object();
    v.insert("action", request.action.clone());
    v.insert("apply", request.apply);
    v.insert("applied", request.apply && !changes.is_empty());
    v.insert("plan_id", plan_id);
    for (name, count) in ["unchanged", "bind", "rebind", "clear"].iter().zip(counts) {
        v.insert(*name, Value::Number(count as f64));
    }
    v.insert("unresolved_count", Value::Number(unresolved_count as f64));
    v.insert("changes", Value::Array(rows));
    v.insert("unresolved", Value::Array(unresolved));
    v.insert(
        "effect",
        "Applies to work that has not selected a release and to later upgrade checks; \
             selected, downloading and imported releases are kept.",
    );
    Ok(v)
}

/// Parses `after=kind:tmdb_id&limit=N`; a page holds 100 bindings by default.
pub fn page(query: &str) -> Result<(Option<Title>, usize)> {
    let mut after = None;
    let mut limit = 100;
    for field in query.split('&').filter(|f| !f.is_empty()) {
        match field.split_once('=') {
            Some(("after", text)) if after.is_none() => {
                let (kind, id) = text
                    .split_once(':')
                    .ok_or("Title profile: invalid cursor")?;
                let mut v = Value::object();
                v.insert("kind", kind);
                v.insert(
                    "tmdb_id",
                    Value::Number(
                        id.parse::<u64>()
                            .map_err(|_| "Title profile: invalid cursor")?
                            as f64,
                    ),
                );
                after = Some(Title::from_json(&v)?);
            }
            Some(("limit", text)) => {
                limit = text
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_PAGE_ROWS).contains(n))
                    .ok_or("Title profile: page limit must be between 1 and 200")?;
            }
            _ => return Err("Title profile: invalid page query".into()),
        }
    }
    Ok((after, limit))
}

#[cfg(test)]
mod tests;
