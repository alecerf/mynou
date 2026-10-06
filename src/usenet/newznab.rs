//! Native typed Newznab metadata and authenticated NZB transport. No queue admission.
use super::{Server, nzb::Nzb};
use crate::{
    Result,
    config::{Config, Source},
    json::Value,
    net,
    requesters::{valid_digest, valid_id},
    xml::{Element, parse_xml},
};
const NAMESPACE: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";
const MAX_ITEMS: usize = 4096;
#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub server_id: String,
    pub minimum_bytes: u64,
    pub maximum_bytes: u64,
}
impl Options {
    pub fn parse(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["server_id", "minimum_bytes", "maximum_bytes"])?;
        let server_id = v.get("server_id").and_then(Value::as_str).filter(|s| valid_id(s))
            .ok_or("Newznab: explicit server ID is required")?.to_owned();
        let get = |key: &str, default: u64| v.get(key).map_or(Ok(default), |v| {
            v.as_u64().filter(|n| *n > 0 && *n <= 1 << 40).ok_or("Newznab: invalid size policy")
        });
        let minimum_bytes = get("minimum_bytes", 1)?;
        let maximum_bytes = get("maximum_bytes", 64 << 30)?;
        if minimum_bytes > maximum_bytes {
            return Err("Newznab: invalid size interval".into());
        }
        Ok(Self { server_id, minimum_bytes, maximum_bytes })
    }
    pub fn json(&self) -> Value {
        let mut v = Value::object();
        v.insert("server_id", self.server_id.clone());
        v.insert("minimum_bytes", self.minimum_bytes.to_string());
        v.insert("maximum_bytes", self.maximum_bytes.to_string());
        v
    }
}
/// An advertisement bound to one indexer configuration and NNTP provider.
/// This identifies metadata; it is not evidence of acquired or verified content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub indexer_id: String,
    pub indexer_binding: String,
    pub server_id: String,
    pub server_binding: String,
    pub advertised_bytes: u64,
    pub password_protected: bool,
}
impl Target {
    fn new(source: &Source, server: &Server, size: u64, password: bool) -> Self {
        Self {
            indexer_id: source.options.identity(source),
            indexer_binding: source.options.binding(source),
            server_id: server.id().into(),
            server_binding: server.binding(),
            advertised_bytes: size,
            password_protected: password,
        }
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("indexer_id", self.indexer_id.clone());
        v.insert("indexer_binding", self.indexer_binding.clone());
        v.insert("server_id", self.server_id.clone());
        v.insert("server_binding", self.server_binding.clone());
        v.insert("advertised_bytes", self.advertised_bytes.to_string());
        v.insert("password_protected", self.password_protected);
        v
    }
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["indexer_id", "indexer_binding", "server_id", "server_binding", "advertised_bytes", "password_protected"])?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned).ok_or("Newznab: invalid captured identity");
        let t = Self {
            indexer_id: s("indexer_id")?,
            indexer_binding: s("indexer_binding")?,
            server_id: s("server_id")?,
            server_binding: s("server_binding")?,
            advertised_bytes: v.get("advertised_bytes").and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())).filter(|n| *n > 0 && *n <= 1 << 40).ok_or("Newznab: invalid captured size")?,
            password_protected: v.get("password_protected").and_then(Value::as_bool).ok_or("Newznab: invalid captured password flag")?,
        };
        if !valid_id(&t.indexer_id) || !valid_id(&t.server_id) || !valid_digest(&t.indexer_binding) || !valid_digest(&t.server_binding) {
            return Err("Newznab: invalid captured binding".into());
        }
        Ok(t)
    }
    pub(crate) fn allocation_bytes(&self) -> usize {
        self.indexer_id.capacity() + self.indexer_binding.capacity() + self.server_id.capacity() + self.server_binding.capacity()
    }
    pub(crate) fn configured<'a>(&self, config: &'a Config) -> Result<&'a Source> {
        let source = config.sources.iter().find(|s| {
            s.kind == "newznab" && s.options.identity(s) == self.indexer_id && s.options.binding(s) == self.indexer_binding
        }).ok_or("Newznab: captured indexer is unavailable or changed")?;
        let options = source.options.newznab.as_ref().ok_or("Newznab: missing source settings")?;
        if options.server_id != self.server_id || self.advertised_bytes < options.minimum_bytes
            || self.advertised_bytes > options.maximum_bytes || self.password_protected
            || !config.usenet.servers.iter().any(|s| s.id() == self.server_id && s.binding() == self.server_binding)
        {
            return Err("Newznab: captured provider or release policy is unavailable".into());
        }
        Ok(source)
    }
}
pub(crate) struct Advertisement {
    pub title: String,
    pub url: String,
    pub target: Target,
}
fn one<'a>(e: &'a Element, name: &str) -> Result<Option<&'a Element>> {
    let mut children = e.children.iter().filter(|c| c.local() == name);
    let first = children.next();
    if children.next().is_some() {
        return Err("Newznab: ambiguous item metadata".into());
    }
    Ok(first)
}
fn size(s: &str) -> Result<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Newznab: invalid advertised size".into());
    }
    s.parse::<u64>().ok().filter(|n| *n > 0 && *n <= 1 << 40)
        .ok_or_else(|| "Newznab: invalid advertised size".into())
}
pub(crate) fn download_url(url: &str, base: &str) -> Result<String> {
    if url.is_empty() || url.len() > 8192 || url.chars().any(char::is_control) {
        return Err("Newznab: invalid download reference".into());
    }
    let origin = net::parse_url(base)?.origin();
    let url = if url.starts_with('/') && !url.starts_with("//") {
        format!("{origin}{url}")
    } else {
        url.to_owned()
    };
    if net::parse_url(&url)?.origin() != origin {
        return Err("Newznab: download reference changed origin".into());
    }
    Ok(url)
}
fn item(e: &Element, root: &Element, channel: &Element, source: &Source, server: &Server) -> Result<Advertisement> {
    let title = one(e, "title")?.filter(|e| e.children.is_empty()).map(|e| e.text.trim())
        .filter(|s| !s.is_empty() && s.len() <= 2048).ok_or("Newznab: missing title")?;
    let enclosure = one(e, "enclosure")?.ok_or("Newznab: missing NZB enclosure")?;
    if !enclosure.children.is_empty() || !enclosure.text.trim().is_empty()
        || !enclosure.attrs.get("type").is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "application/x-nzb" | "application/x-nzb+xml"))
    {
        return Err("Newznab: unsupported enclosure type".into());
    }
    let url = download_url(enclosure.attrs.get("url").ok_or("Newznab: missing download reference")?, &source.url)?;
    let mut advertised = enclosure.attrs.get("length").map(|s| size(s)).transpose()?;
    let mut size_attr = false;
    let mut password = None;
    for a in e.children.iter().filter(|a| a.local() == "attr") {
        let key = a.attrs.get("name").map(String::as_str).unwrap_or("");
        if !matches!(key, "size" | "password" | "passworded") {
            continue;
        }
        let prefix = a.name.split_once(':').map(|(p,_)| format!("xmlns:{p}")).ok_or("Newznab: metadata namespace is required")?;
        let namespace = [a, e, channel, root].iter().find_map(|e| e.attrs.get(&prefix));
        if namespace.map(String::as_str) != Some(NAMESPACE) || !a.children.is_empty() || !a.text.trim().is_empty() {
            return Err("Newznab: invalid metadata namespace or structure".into());
        }
        let value = a.attrs.get("value").ok_or("Newznab: missing metadata value")?;
        if key == "size" {
            let n = size(value)?;
            if size_attr || advertised.is_some_and(|s| s != n) {
                return Err("Newznab: conflicting advertised size".into());
            }
            size_attr = true;
            advertised = Some(n);
        } else {
            let flag = match value.as_str() {
                "0" => false,
                "1" => true,
                _ => return Err("Newznab: invalid password advertisement".into()),
            };
            if password.replace(flag).is_some() {
                return Err("Newznab: ambiguous password advertisement".into());
            }
        }
    }
    Ok(Advertisement {
        title: title.into(),
        url,
        target: Target::new(source, server, advertised.ok_or("Newznab: missing advertised size")?, password.unwrap_or(false)),
    })
}
pub(crate) fn parse_feed(text: &str, source: &Source, config: &Config) -> Result<Vec<Advertisement>> {
    let root = parse_xml(text).map_err(|_| "Newznab: invalid or unsupported XML")?;
    if root.local() == "error" {
        return Err("Newznab: indexer service error".into());
    }
    if root.name != "rss" {
        return Err("Newznab: expected RSS metadata".into());
    }
    let channel = one(&root, "channel")?.ok_or("Newznab: missing channel")?;
    let options = source.options.newznab.as_ref().ok_or("Newznab: missing source settings")?;
    let server = config.usenet.servers.iter().find(|s| s.id() == options.server_id).ok_or("Newznab: provider is not configured")?;
    let mut out = Vec::new();
    for (i, e) in channel.children.iter().filter(|e| e.local() == "item").enumerate() {
        if i >= MAX_ITEMS {
            return Err("Newznab: advertised item limit exceeded".into());
        }
        // A malformed advertisement cannot impersonate another valid item.
        if let Ok(a) = item(e, &root, channel, source, server) {
            out.push(a);
        }
    }
    Ok(out)
}
/// Fetch a bound original NZB document with the native indexer's authentication,
/// origin, rate, policy and absolute-deadline gates. Never creates a transfer.
pub fn fetch_document(config: &Config, target: &Target, url: &str, deadline: std::time::Instant) -> Result<Vec<u8>> {
    Target::from_json(&target.to_json())?;
    let source = target.configured(config)?;
    let url = download_url(url, &source.url)?;
    let url = crate::integrations::newznab_document_url(source, &url)?;
    let response = crate::indexers::fetch(source, &url, "application/x-nzb, application/xml", deadline)?;
    let parsed = Nzb::parse(&response.body);
    crate::indexers::parsed(source, parsed.is_ok());
    parsed?;
    Ok(response.body)
}
