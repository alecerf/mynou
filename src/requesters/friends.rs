//! Friends' watchlists through the owner's Plex connection.
//!
//! Plex's community API lists the owner's friends (`allFriendsV2`) and reads
//! each one's watchlist (`user(id:) { watchlist(first:, after:) }`) with the
//! owner's token alone: friends hand over nothing. Each item is then read
//! from Plex's metadata, the same description the owner's own watchlist
//! lists, so it becomes a film or a series by the same rules.
//!
//! A friend is a requester, `plex-<Plex user ID>`, off until the owner turns
//! it on: its switch is the account's policy. Once a friend is on, the
//! owner's own watchlist is read as a requester too, so a title on both lists
//! makes one request with both as requesters.
//!
//! The query shapes come from Plex Web's own queries and from real error
//! responses; the successful responses are synthetic fixtures until checked
//! against a live account (`tests/fixtures/plex`).
use super::{LedgerRead, MAX_ACCOUNTS, Policy, Record, plex_binding, plex_id, token_account};
use crate::{
    Result,
    config::{self, Config},
    engine::{Engine, lock},
    integrations::{
        self, PAGE_SIZE, PLEX_POLL_DEADLINE, PageMark, SweepCursor, WatchlistPage, integer, string,
    },
    json::{self, Value},
    store::{self, Request},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Friends Plex may list; a longer list is refused rather than cut.
pub const MAX_FRIENDS: usize = 200;
/// Plex accounts remembered: the friends listed now, former friends that
/// still have a requester, and the connected account.
const MAX_REGISTRY: usize = MAX_FRIENDS + MAX_ACCOUNTS + 1;
/// How often the friend list is read again.
pub const REFRESH_SECS: u64 = 600;
/// One community API response.
const MAX_RESPONSE: usize = 1 << 20;
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REASON: usize = 200;
const MAX_NAME: usize = 128;
/// Items whose description was read, so that sweeps after the first do not
/// ask again.
const ITEMS: usize = 4096;
const ITEM_TTL: Duration = Duration::from_secs(24 * 3600);
pub(crate) const NOT_CONNECTED: &str = "Plex is not connected";

const FRIENDS_QUERY: &str =
    "query MynouFriends { allFriendsV2 { user { id idRaw username displayName } } }";
const WATCHLIST_QUERY: &str = "query MynouWatchlist($uuid: ID = \"\", $first: PaginationInt!, $after: String) { user(id: $uuid) { watchlist(first: $first, after: $after) { nodes { id key type title year } pageInfo { hasNextPage endCursor } } } }";

/// A Plex account read with the owner's connection: the connected account
/// itself, or one of its friends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlexAccount {
    /// Plex's account number (`idRaw`), which the requester ID carries.
    pub user_id: u64,
    /// The community API's ID, which the watchlist query takes; empty for
    /// the connected account.
    pub uuid: String,
    pub username: String,
    pub display_name: String,
    /// The account behind the connection, rather than a friend.
    pub owner: bool,
    /// Plex lists it now: still a friend, or still the connected account.
    pub present: bool,
    /// Plex's reason when its watchlist could not be read, until a read
    /// succeeds.
    pub error: Option<String>,
}
impl PlexAccount {
    pub fn name(&self) -> &str {
        [&self.display_name, &self.username, &self.uuid]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or("", String::as_str)
    }
    fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("user_id", self.user_id.to_string());
        v.insert("uuid", self.uuid.clone());
        v.insert("username", self.username.clone());
        v.insert("display_name", self.display_name.clone());
        v.insert("owner", self.owner);
        v.insert("present", self.present);
        v.insert(
            "error",
            self.error.clone().map_or(Value::Null, Value::String),
        );
        v
    }
    fn from_json(v: &Value) -> Result<Self> {
        super::only(
            v,
            &[
                "user_id",
                "uuid",
                "username",
                "display_name",
                "owner",
                "present",
                "error",
            ],
        )?;
        let account = Self {
            user_id: super::integer(v, "user_id")?,
            uuid: super::text(v, "uuid")?,
            username: super::text(v, "username")?,
            display_name: super::text(v, "display_name")?,
            owner: super::boolean(v, "owner")?,
            present: super::boolean(v, "present")?,
            error: super::optional(v, "error")?,
        };
        if account.user_id == 0
            || (!account.uuid.is_empty() && !valid_uuid(&account.uuid))
            || (!account.owner && account.uuid.is_empty())
            || [&account.username, &account.display_name]
                .into_iter()
                .any(|s| clean(s, MAX_NAME) != *s)
            || account
                .error
                .as_ref()
                .is_some_and(|e| e.is_empty() || clean(e, MAX_REASON) != *e)
        {
            return Err("Requester: invalid Plex account".into());
        }
        Ok(account)
    }
}

/// The Plex accounts Mynou knows, by requester ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlexRegistry {
    pub accounts: BTreeMap<String, PlexAccount>,
    /// When Plex was last asked for the friend list; 0 before.
    pub checked_at: u64,
    /// Why that failed, until it succeeds.
    pub error: Option<String>,
}
impl PlexRegistry {
    pub(crate) fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert(
            "accounts",
            Value::Array(self.accounts.values().map(PlexAccount::to_json).collect()),
        );
        v.insert("checked_at", self.checked_at.to_string());
        v.insert(
            "error",
            self.error.clone().map_or(Value::Null, Value::String),
        );
        v
    }
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        super::only(v, &["accounts", "checked_at", "error"])?;
        let mut registry = Self {
            accounts: BTreeMap::new(),
            checked_at: super::integer(v, "checked_at")?,
            error: super::optional(v, "error")?,
        };
        let list = v
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or("Requester: invalid Plex accounts")?;
        if list.len() > MAX_REGISTRY {
            return Err("Requester: too many Plex accounts".into());
        }
        for item in list {
            let account = PlexAccount::from_json(item)?;
            if registry
                .accounts
                .insert(plex_id(account.user_id), account)
                .is_some()
            {
                return Err("Requester: duplicate Plex account".into());
            }
        }
        if registry.accounts.values().filter(|a| a.owner).count() > 1
            || registry
                .error
                .as_ref()
                .is_some_and(|e| e.is_empty() || clean(e, MAX_REASON) != *e)
        {
            return Err("Requester: invalid Plex accounts".into());
        }
        Ok(registry)
    }
    /// The connected account, once Plex said who it is.
    pub fn owner(&self) -> Option<(&String, &PlexAccount)> {
        self.accounts.iter().find(|(_, a)| a.owner && a.present)
    }
}

/// Printable text on one line, bounded.
fn clean(text: &str, limit: usize) -> String {
    text.split(char::is_control)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}
/// Plex's reason, one printable line without the owner's token.
pub(crate) fn reason(text: &str, token: &str) -> String {
    let text = if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "[token]")
    };
    clean(&text, MAX_REASON)
}
fn valid_uuid(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// The owner's token, from the Plex connection.
fn owner_token(config: &Config) -> Result<String> {
    if !config.plex.enabled {
        return Err(NOT_CONNECTED.into());
    }
    match &config.plex.token_override {
        Some(token) => Ok(token.clone()),
        None => config::secret(&config.plex.token_env).map_err(|_| NOT_CONNECTED.into()),
    }
}
pub(crate) fn connected(config: &Config) -> bool {
    owner_token(config).is_ok()
}
fn headers(config: &Config, token: &str, body: bool) -> Result<Vec<(String, String)>> {
    let mut headers = vec![
        ("Accept".to_owned(), "application/json".to_owned()),
        ("X-Plex-Token".to_owned(), token.to_owned()),
        (
            "X-Plex-Client-Identifier".to_owned(),
            crate::plex::client_id(&config.data_dir)?,
        ),
        ("X-Plex-Product".to_owned(), "Mynou".to_owned()),
        (
            "X-Plex-Version".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ];
    if body {
        headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
    }
    Ok(headers)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .map(|d| d.min(TIMEOUT))
        .ok_or_else(|| PLEX_POLL_DEADLINE.into())
}
/// Plex's reason with a refusal: the message of a JSON error, or a short
/// text body such as "Invalid token".
pub(crate) fn refusal(body: &[u8], token: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    let message = match json::parse(text) {
        Ok(value) => value
            .get("Error")
            .and_then(|e| string(e, "message"))
            .or_else(|| string(value.get("errors")?.as_array()?.first()?, "message"))?
            .to_owned(),
        Err(_) if !text.starts_with('<') && !text.contains('\n') => text.to_owned(),
        Err(_) => return None,
    };
    let line = reason(&message, token);
    (!line.is_empty()).then_some(line)
}

/// One query to Plex's community API with the owner's token: its `data`, or
/// Plex's reason when it refused the query, wholly or partly.
fn community(config: &Config, query: &str, variables: Value, deadline: Instant) -> Result<Value> {
    let token = owner_token(config)?;
    let timeout = remaining(deadline)?;
    let mut body = Value::object();
    body.insert("query", query);
    body.insert("variables", variables);
    let response = integrations::client()
        .without_redirects()
        .with_timeout(timeout)
        .with_max_body(MAX_RESPONSE)
        .request(
            "POST",
            &config.plex.community_url,
            &headers(config, &token, true)?,
            json::stringify(&body).as_bytes(),
        )
        .map_err(|_| {
            if Instant::now() >= deadline {
                PLEX_POLL_DEADLINE
            } else {
                "Plex: network request failed"
            }
        })?;
    if !(200..300).contains(&response.status) {
        return Err(match refusal(&response.body, &token) {
            Some(why) => format!("Plex: HTTP response {}: {why}", response.status),
            None => format!("Plex: HTTP response {}", response.status),
        });
    }
    let value = std::str::from_utf8(&response.body)
        .ok()
        .and_then(|text| json::parse(text).ok())
        .ok_or("Plex: invalid JSON response")?;
    if let Some(errors) = value.get("errors") {
        let errors = errors.as_array().ok_or("Plex: invalid JSON response")?;
        if let Some(first) = errors.first() {
            let why = string(first, "message")
                .map(|m| reason(m, &token))
                .filter(|m| !m.is_empty());
            return Err(format!(
                "Plex: {}",
                why.as_deref().unwrap_or("the query was refused")
            ));
        }
    }
    match value.get("data") {
        Some(data @ Value::Object(_)) => Ok(data.clone()),
        _ => Err("Plex: response without data".into()),
    }
}

/// A friend as Plex lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listed {
    pub user_id: u64,
    pub uuid: String,
    pub username: String,
    pub display_name: String,
}

fn optional_name(value: &Value, key: &str) -> Result<String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(clean(text, MAX_NAME)),
        Some(_) => Err(format!("Plex: invalid friend {key}")),
    }
}

/// The friends of an `allFriendsV2` answer. What Mynou needs is required;
/// fields it does not read are ignored.
pub(crate) fn parse_friends(data: &Value) -> Result<Vec<Listed>> {
    let list = data
        .get("allFriendsV2")
        .and_then(Value::as_array)
        .ok_or("Plex: invalid friends response")?;
    if list.len() > MAX_FRIENDS {
        return Err(format!("Plex: more than {MAX_FRIENDS} friends"));
    }
    let mut seen = BTreeSet::new();
    let mut friends = Vec::new();
    for entry in list {
        let user = entry
            .get("user")
            .filter(|u| u.as_object().is_some())
            .ok_or("Plex: invalid friend")?;
        let uuid = string(user, "id")
            .filter(|id| valid_uuid(id))
            .ok_or("Plex: invalid friend ID")?;
        let user_id = integer(user, "idRaw")
            .filter(|n| *n != 0)
            .ok_or("Plex: invalid friend account number")?;
        if !seen.insert(user_id) {
            return Err("Plex: a friend is listed twice".into());
        }
        friends.push(Listed {
            user_id,
            uuid: uuid.to_owned(),
            username: optional_name(user, "username")?,
            display_name: optional_name(user, "displayName")?,
        });
    }
    Ok(friends)
}

pub(crate) fn list_friends(config: &Config, deadline: Instant) -> Result<Vec<Listed>> {
    parse_friends(&community(
        config,
        FRIENDS_QUERY,
        Value::object(),
        deadline,
    )?)
}

/// The account behind the Plex connection: its number and name.
pub(crate) fn owner_identity(config: &Config, deadline: Instant) -> Result<(u64, String)> {
    let token = owner_token(config)?;
    let value = integrations::fetch_json_before(
        &config.plex.identity_url,
        &headers(config, &token, false)?,
        deadline,
        PLEX_POLL_DEADLINE,
    )
    .map_err(|e| reason(&e, &token))?;
    let id = integer(&value, "id")
        .filter(|n| *n != 0)
        .ok_or("Plex: invalid account")?;
    let name = string(&value, "username")
        .or_else(|| string(&value, "title"))
        .unwrap_or_default();
    Ok((id, clean(name, MAX_NAME)))
}

/// One page of a friend's watchlist as Plex returned it.
#[derive(Debug)]
pub(crate) struct Listing {
    pub nodes: Vec<Value>,
    pub has_next: bool,
    pub end: Option<String>,
}

/// A `user { watchlist }` answer. A missing user or watchlist is Plex
/// refusing it, never an empty list.
pub(crate) fn parse_watchlist(data: &Value) -> Result<Listing> {
    let invalid = || "Plex: invalid watchlist response".to_owned();
    let user = data.get("user").ok_or_else(invalid)?;
    if *user == Value::Null {
        return Err("Plex: this account is not available to you".into());
    }
    let list = user.get("watchlist").ok_or_else(invalid)?;
    if *list == Value::Null {
        return Err("Plex: this watchlist is not shared with you".into());
    }
    let nodes = list
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if nodes.len() > PAGE_SIZE {
        return Err(format!(
            "Plex: a watchlist page holds more than {PAGE_SIZE} items"
        ));
    }
    let info = list.get("pageInfo").ok_or_else(invalid)?;
    let has_next = info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let end = match info.get("endCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if integrations::valid_page_cursor(text) => Some(text.clone()),
        Some(_) => return Err(invalid()),
    };
    if has_next && (end.is_none() || nodes.is_empty()) {
        return Err("Plex: a watchlist page without a next cursor".into());
    }
    Ok(Listing {
        nodes: nodes.to_vec(),
        has_next,
        end,
    })
}

/// Stable identity of a watchlist entry, to notice a list that moved.
fn node_anchor(node: &Value) -> [u8; 32] {
    let mut hash = crate::crypto::Sha256::new();
    for field in ["type", "id", "key", "title"] {
        let text = string(node, field).unwrap_or_default();
        hash.update(&(text.len() as u64).to_be_bytes());
        hash.update(text.as_bytes());
    }
    hash.finalize()
}

/// The metadata ID of an entry: from its `/library/metadata/<id>` key, as
/// Plex Web opens it, else its ID.
pub(crate) fn item_id(node: &Value) -> Option<&str> {
    string(node, "key")
        .and_then(|key| key.strip_prefix("/library/metadata/"))
        .or_else(|| string(node, "id"))
        .filter(|id| {
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

fn items() -> &'static Mutex<BTreeMap<String, (Instant, Request)>> {
    static ITEMS_READ: OnceLock<Mutex<BTreeMap<String, (Instant, Request)>>> = OnceLock::new();
    ITEMS_READ.get_or_init(|| Mutex::new(BTreeMap::new()))
}
fn cached(key: &str) -> Option<Request> {
    let mut map = items().lock().ok()?;
    match map.get(key) {
        Some((at, request)) if at.elapsed() < ITEM_TTL => Some(request.clone()),
        Some(_) => {
            map.remove(key);
            None
        }
        None => None,
    }
}
fn remember(key: String, request: Request) {
    let Ok(mut map) = items().lock() else {
        return;
    };
    if map.len() >= ITEMS {
        map.retain(|_, (at, _)| at.elapsed() < ITEM_TTL);
    }
    if map.len() >= ITEMS
        && let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(key, _)| key.clone())
    {
        map.remove(&oldest);
    }
    map.insert(key, (Instant::now(), request));
}

/// The film or series an entry stands for, from Plex's description of the
/// item, as the owner's watchlist describes it; other kinds are skipped.
fn item_request(
    config: &Config,
    node: &Value,
    token: &str,
    deadline: Instant,
    requests: &mut u32,
) -> Result<Option<Request>> {
    let kind = match string(node, "type").map(str::to_ascii_lowercase).as_deref() {
        Some("movie") => "movie",
        Some("show") => "series",
        Some(_) => return Ok(None),
        None => return Err("Plex: a watchlist item has no type".into()),
    };
    let id = item_id(node).ok_or("Plex: a watchlist item has no ID")?;
    let key = format!("{}\0{id}", config.plex.metadata_url);
    if let Some(request) = cached(&key) {
        return Ok(Some(request));
    }
    *requests += 1;
    let url = format!("{}{id}?includeGuids=1", config.plex.metadata_url);
    let request = match integrations::fetch_json_before(
        &url,
        &headers(config, token, false)?,
        deadline,
        PLEX_POLL_DEADLINE,
    ) {
        Ok(value) => {
            let item = value
                .get("MediaContainer")
                .and_then(|c| c.get("Metadata"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .ok_or("Plex: a watchlist item has no metadata")?;
            let request = integrations::watchlist_request(item)?
                .ok_or("Plex: a watchlist item is neither a film nor a series")?;
            if request.kind != kind {
                return Err("Plex: a watchlist item and its metadata differ".into());
            }
            request
        }
        // Plex no longer describes the item: the title and year it listed
        // stand for it, the way an owner's item without a TMDB ID does.
        Err(error) if error.starts_with("Plex: HTTP response 404") => {
            let request = Request {
                kind: kind.into(),
                title: string(node, "title")
                    .map(|t| clean(t, 512))
                    .filter(|t| !t.is_empty())
                    .ok_or("Plex: a watchlist item has no title")?,
                year: u32::try_from(integer(node, "year").unwrap_or(0))
                    .map_err(|_| "Plex: invalid year")?,
                season: 0,
                episode: 0,
                tmdb_id: None,
                source_path: None,
                source_url: None,
                runtime_minutes: None,
                source_numbering: None,
                original_language: None,
            };
            request.validate()?;
            request
        }
        Err(error) => return Err(reason(&error, token)),
    };
    remember(key, request.clone());
    Ok(Some(request))
}

/// The next page of a friend's watchlist sweep, read with the owner's token,
/// 100 items a page. Each entry carries the cursor right after it, so that a
/// sweep may stop inside a page and resume there after a restart.
pub(crate) fn watchlist_page(
    config: &Config,
    uuid: &str,
    cursor: &SweepCursor,
    deadline: Instant,
) -> Result<WatchlistPage> {
    let token = owner_token(config)?;
    let start = cursor.page.as_ref().map_or(0, |page| page.start);
    let skip = cursor
        .position
        .checked_sub(start)
        .filter(|n| *n <= PAGE_SIZE as u64)
        .ok_or("Plex: invalid watchlist cursor")? as usize;
    let after = cursor.page.as_ref().map(|page| page.after.clone());
    let mut variables = Value::object();
    variables.insert("uuid", uuid);
    variables.insert("first", PAGE_SIZE as u32);
    variables.insert("after", after.clone().map_or(Value::Null, Value::String));
    let listing = parse_watchlist(&community(config, WATCHLIST_QUERY, variables, deadline)?)?;
    if listing.has_next && listing.end == after {
        return Err("Plex: pagination ignored by the server".into());
    }
    let mut requests = 1;
    // The entry before the cursor must still be there; otherwise the list
    // moved, and the sweep cannot prove what left it.
    let consistent = cursor.consistent
        && skip <= listing.nodes.len()
        && (skip == 0 || Some(node_anchor(&listing.nodes[skip - 1])) == cursor.anchor);
    let mut next = SweepCursor {
        consistent,
        ..cursor.clone()
    };
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, node) in listing.nodes.iter().enumerate().skip(skip) {
        let request = match item_request(config, node, &token, deadline, &mut requests) {
            Ok(request) => request,
            // The entries before it are kept; the failing one is read again.
            Err(_) if index > skip => {
                return Ok(WatchlistPage {
                    entries,
                    next,
                    complete: false,
                    requests,
                });
            }
            Err(error) => return Err(error),
        };
        next = SweepCursor {
            position: start + index as u64 + 1,
            total: None,
            anchor: Some(node_anchor(node)),
            consistent,
            page: cursor.page.clone(),
        };
        if let Some(request) = request
            && seen.insert(request.canonical_key())
        {
            entries.push((next.clone(), request));
        }
    }
    if listing.has_next {
        let end = start + listing.nodes.len() as u64;
        next = SweepCursor {
            position: end,
            total: None,
            anchor: listing.nodes.last().map(node_anchor),
            consistent,
            page: Some(PageMark {
                start: end,
                after: listing
                    .end
                    .ok_or("Plex: a watchlist page without a next cursor")?,
            }),
        };
    }
    Ok(WatchlistPage {
        entries,
        next,
        complete: !listing.has_next,
        requests,
    })
}

/// Whose watchlist a requester account reads.
#[derive(Clone, Debug)]
pub(crate) enum Watcher {
    /// A token account of the settings.
    Token(super::Account),
    /// The connected account's own watchlist.
    Owner(String),
    /// A friend's, through the owner's connection.
    Friend { id: String, uuid: String },
}
impl Watcher {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Token(account) => &account.id,
            Self::Owner(id) | Self::Friend { id, .. } => id,
        }
    }
    pub(crate) fn plex(&self) -> bool {
        !matches!(self, Self::Token(_))
    }
    /// The next page of the account's watchlist sweep.
    pub(crate) fn page(
        &self,
        config: &Config,
        cursor: &SweepCursor,
        deadline: Instant,
    ) -> Result<WatchlistPage> {
        match self {
            Self::Token(account) => {
                integrations::requester_watchlist_page(config, account, cursor, deadline)
            }
            Self::Owner(_) => {
                // Without a connection the owner's list would read as empty.
                let token = owner_token(config)?;
                integrations::watchlist_page_before(config, cursor, deadline)
                    .map_err(|e| reason(&e, &token))
            }
            Self::Friend { uuid, .. } => watchlist_page(config, uuid, cursor, deadline),
        }
    }
}

/// Whether any friend was ever turned on: the owner is then a requester.
fn following(ledger: &impl LedgerRead) -> bool {
    ledger.accounts().keys().any(|id| {
        super::plex_user(id).is_some() && !ledger.plex().accounts.get(id).is_some_and(|a| a.owner)
    })
}

/// Why a friend's watchlist is not read, when it is on.
fn idle_reason(config: &Config, account: &PlexAccount) -> Option<String> {
    if !connected(config) {
        Some(NOT_CONNECTED.into())
    } else if !account.present {
        Some(if account.owner {
            "No longer the connected Plex account".into()
        } else {
            "No longer your friend on Plex".into()
        })
    } else {
        token_account(config, account.user_id)
            .map(|a| format!("Also set up as the requester account {}", a.id))
    }
}

impl Engine {
    /// Reads the owner's friend list now, or when it is due once a friend is
    /// followed; then also the account behind the connection, whose own
    /// watchlist becomes a requester. Until a friend is followed, only
    /// Settings asks for the list. A failure is kept with the list, which
    /// stays as it was.
    pub(crate) fn refresh_plex(&self, force: bool, deadline: Instant) -> Result<()> {
        if self.read_only || !connected(&self.config) {
            return Ok(());
        }
        let (due, wants_owner) = {
            let ledger = lock(&self.requester_store)?;
            let following = following(&*ledger);
            if !force && !following {
                return Ok(());
            }
            let due = force || store::now().saturating_sub(ledger.plex.checked_at) >= REFRESH_SECS;
            let owner_known = ledger.plex.owner().is_some();
            (due, following && (due || !owner_known))
        };
        let friends = due.then(|| list_friends(&self.config, deadline));
        let owner = wants_owner.then(|| owner_identity(&self.config, deadline));
        let mut ledger = lock(&self.requester_store)?;
        let mut registry = ledger.plex.clone();
        let now = store::now();
        if let Some(friends) = &friends {
            registry.checked_at = now;
            match friends {
                Ok(listed) => {
                    registry.error = None;
                    let ids: BTreeSet<_> = listed.iter().map(|f| plex_id(f.user_id)).collect();
                    for friend in listed {
                        let entry = registry
                            .accounts
                            .entry(plex_id(friend.user_id))
                            .or_insert_with(|| PlexAccount {
                                user_id: friend.user_id,
                                uuid: String::new(),
                                username: String::new(),
                                display_name: String::new(),
                                owner: false,
                                present: true,
                                error: None,
                            });
                        // A new name keeps the same requester.
                        entry.uuid = friend.uuid.clone();
                        entry.username = friend.username.clone();
                        entry.display_name = friend.display_name.clone();
                        if !entry.owner {
                            entry.present = true;
                        }
                    }
                    for (id, entry) in registry.accounts.iter_mut() {
                        if !entry.owner && !ids.contains(id) {
                            entry.present = false;
                        }
                    }
                    // A former friend never followed leaves nothing behind.
                    let accounts = &ledger.state.accounts;
                    registry.accounts.retain(|id, entry| {
                        entry.owner || entry.present || accounts.contains_key(id)
                    });
                }
                Err(error) => registry.error = Some(clean(error, MAX_REASON)),
            }
        }
        let mut owner_id = None;
        match &owner {
            Some(Ok((user_id, name))) => {
                let id = plex_id(*user_id);
                for (other, entry) in registry.accounts.iter_mut() {
                    if entry.owner && *other != id {
                        // Another account was connected: its requester stays.
                        entry.owner = false;
                        entry.present = false;
                    }
                }
                let entry = registry
                    .accounts
                    .entry(id.clone())
                    .or_insert_with(|| PlexAccount {
                        user_id: *user_id,
                        uuid: String::new(),
                        username: String::new(),
                        display_name: String::new(),
                        owner: true,
                        present: true,
                        error: None,
                    });
                entry.owner = true;
                entry.present = true;
                entry.username = name.clone();
                owner_id = Some((id, *user_id));
            }
            Some(Err(error)) if registry.error.is_none() => {
                registry.error = Some(clean(error, MAX_REASON));
            }
            _ => {}
        }
        if registry.accounts.len() > MAX_REGISTRY {
            return Err("Requester: too many Plex accounts".into());
        }
        ledger.save_plex(registry)?;
        // The owner's own watchlist, read as a requester from now on, unless
        // a token account of the settings already reads it.
        if let Some((id, user_id)) = owner_id
            && !ledger.state.accounts.contains_key(&id)
            && token_account(&self.config, user_id).is_none()
        {
            if ledger.state.accounts.len() >= MAX_ACCOUNTS {
                return Err(format!(
                    "Requester: at most {MAX_ACCOUNTS} accounts; your own watchlist is not read"
                ));
            }
            let mut next = ledger.state.clone();
            next.accounts.insert(
                id.clone(),
                Record {
                    id,
                    binding: plex_binding(user_id),
                    policy: Policy {
                        // The owner's watchlist already made requests.
                        enabled: true,
                        ..Policy::initial(&self.config)
                    },
                    revision: 1,
                    cursor: 0,
                    snapshot: None,
                    polled_at: 0,
                    last_error: None,
                },
            );
            ledger.save_patch(next)?;
        }
        match (friends, owner) {
            (Some(Err(error)), _) | (_, Some(Err(error))) => Err(error),
            _ => Ok(()),
        }
    }

    /// Reads the friend list from Plex now: `POST /api/plex/friends`.
    pub fn refresh_plex_friends(&self) -> Result<Value> {
        if self.read_only {
            return Err("Plex friends need a running writable service".into());
        }
        if !connected(&self.config) {
            return Err(NOT_CONNECTED.into());
        }
        self.refresh_plex(true, Instant::now() + Duration::from_secs(20))?;
        let ledger = lock(&self.requester_store)?;
        self.plex_value(&ledger)
    }

    /// Turns a friend's requests on or off: `POST /api/plex/friends/<id>`.
    /// A friend turned off keeps the work already admitted for it, like a
    /// token account whose policy is turned off.
    pub fn plex_friend(&self, id: &str, enabled: bool) -> Result<Value> {
        if self.read_only {
            return Err("Plex friends need a running writable service".into());
        }
        let mut ledger = lock(&self.requester_store)?;
        let mut jobs = lock(&self.store)?;
        let friend = ledger
            .plex
            .accounts
            .get(id)
            .filter(|a| !a.owner)
            .cloned()
            .ok_or("Requester: unknown Plex friend")?;
        if enabled && let Some(why) = idle_reason(&self.config, &friend) {
            return Err(format!("Requester: {why}"));
        }
        let mut next = ledger.state.clone();
        let changed = match next.accounts.get_mut(id) {
            Some(record) if record.policy.enabled != enabled => {
                record.policy.enabled = enabled;
                // Unadmitted demands adopt this revision in bounded admission.
                record.revision = record
                    .revision
                    .checked_add(1)
                    .ok_or("Requester: policy revision overflow")?;
                true
            }
            Some(_) => false,
            None if enabled => {
                if next.accounts.len() >= MAX_ACCOUNTS {
                    return Err(format!("Requester: at most {MAX_ACCOUNTS} accounts"));
                }
                let policy = Policy {
                    enabled: true,
                    ..Policy::initial(&self.config)
                };
                policy.validate_config(&self.config)?;
                next.accounts.insert(
                    id.to_owned(),
                    Record {
                        id: id.to_owned(),
                        binding: plex_binding(friend.user_id),
                        policy,
                        revision: 1,
                        cursor: 0,
                        snapshot: None,
                        polled_at: 0,
                        last_error: None,
                    },
                );
                true
            }
            None => false,
        };
        if changed {
            ledger.save_patch(next)?;
            self.admit_requester_demand(&mut ledger, &mut jobs)?;
        }
        drop(jobs);
        self.friend_value(&ledger, id, &friend)
    }

    /// The accounts whose watchlists a pass reads: the token accounts, then
    /// the owner and the friends that are on, while Plex is connected.
    pub(crate) fn watchers(&self) -> Result<Vec<Watcher>> {
        let ledger = lock(&self.requester_store)?;
        let mut watchers: Vec<_> = self
            .config
            .requesters
            .accounts
            .iter()
            .cloned()
            .map(Watcher::Token)
            .collect();
        for (id, account) in &ledger.plex.accounts {
            let Some(record) = ledger.state.accounts.get(id) else {
                continue;
            };
            if idle_reason(&self.config, account).is_some() || !super::bound(&self.config, record) {
                continue;
            }
            if account.owner {
                watchers.push(Watcher::Owner(id.clone()));
            } else if record.policy.enabled {
                watchers.push(Watcher::Friend {
                    id: id.clone(),
                    uuid: account.uuid.clone(),
                });
            }
        }
        Ok(watchers)
    }

    /// Friends that are on but not read in this pass, with the reason.
    pub(crate) fn idle_friends(&self) -> Result<Vec<(String, String)>> {
        let ledger = lock(&self.requester_store)?;
        Ok(ledger
            .plex
            .accounts
            .iter()
            .filter(|(id, _)| {
                ledger
                    .state
                    .accounts
                    .get(*id)
                    .is_some_and(|r| r.policy.enabled)
            })
            .filter_map(|(id, account)| Some((id.clone(), idle_reason(&self.config, account)?)))
            .collect())
    }

    /// Keeps Plex's reason with the account whose watchlist failed, until a
    /// read succeeds.
    pub(crate) fn record_plex_error(&self, id: &str, error: Option<&str>) -> Result<()> {
        let mut ledger = lock(&self.requester_store)?;
        let mut registry = ledger.plex.clone();
        let Some(account) = registry.accounts.get_mut(id) else {
            return Ok(());
        };
        account.error = error
            .map(|e| clean(e, MAX_REASON))
            .filter(|e| !e.is_empty());
        ledger.save_plex(registry)
    }

    /// The Plex part of `GET /api/requesters`: the connection, the friend
    /// list and each friend's state and switch.
    pub(crate) fn plex_value(&self, ledger: &super::RequesterStore) -> Result<Value> {
        let mut v = Value::object();
        v.insert("connected", connected(&self.config));
        v.insert("checked_at", ledger.plex.checked_at.to_string());
        v.insert(
            "error",
            ledger.plex.error.clone().map_or(Value::Null, Value::String),
        );
        v.insert(
            "owner",
            ledger
                .plex
                .owner()
                .filter(|(id, _)| ledger.state.accounts.contains_key(*id))
                .map_or(Value::Null, |(id, _)| Value::String(id.clone())),
        );
        let mut friends = Vec::new();
        for (id, account) in ledger.plex.accounts.iter().filter(|(_, a)| !a.owner) {
            friends.push(self.friend_value(ledger, id, account)?);
        }
        v.insert("friends", Value::Array(friends));
        Ok(v)
    }

    fn friend_value(
        &self,
        ledger: &super::RequesterStore,
        id: &str,
        account: &PlexAccount,
    ) -> Result<Value> {
        let record = ledger.state.accounts.get(id);
        let enabled = record.is_some_and(|r| r.policy.enabled);
        let (state, error) = if !account.present {
            ("left", Some("No longer your friend on Plex".to_owned()))
        } else if let Some(other) = token_account(&self.config, account.user_id) {
            (
                "duplicate",
                Some(format!("Also set up as the requester account {}", other.id)),
            )
        } else if !enabled {
            ("off", None)
        } else if !connected(&self.config) {
            ("error", Some(NOT_CONNECTED.to_owned()))
        } else if let Some(error) = &account.error {
            ("error", Some(error.clone()))
        } else if record.is_some_and(|r| r.cursor == 0) {
            ("reading", None)
        } else {
            ("on", None)
        };
        // Titles of the friend's watchlist that Mynou keeps, removed ones aside.
        let titles = match record {
            Some(_) => ledger
                .count(id, None)?
                .saturating_sub(ledger.count(id, Some("removed"))?),
            None => 0,
        };
        let mut v = Value::object();
        v.insert("id", id);
        v.insert("name", account.name());
        v.insert("username", account.username.clone());
        v.insert("enabled", enabled);
        v.insert("state", state);
        v.insert("error", error.map_or(Value::Null, Value::String));
        v.insert("titles", Value::Number(titles as f64));
        v.insert(
            "polled_at",
            record.map_or_else(|| "0".to_owned(), |r| r.polled_at.to_string()),
        );
        Ok(v)
    }
}
