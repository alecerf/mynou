//! Connecting Plex in one click.
//!
//! The interface asks for a PIN, opens Plex's own sign-in page on it and asks
//! until Plex attaches a token, which goes straight to the secrets and never
//! back to the browser. The servers the account owns are listed from plex.tv;
//! choosing one finds an address of it that answers from this machine, and
//! its film and series sections.
use crate::{
    Result,
    config::{self, PathMapping},
    engine::Engine,
    json::{self, Value},
    net::HttpClient,
};
use std::path::Path;
use std::time::Duration;

const PLEX_TV: &str = "https://plex.tv/api/v2";
/// The identifier Mynou already sends to Plex servers and the watchlist.
const CLIENT: &str = "mynou-rust-std";
const TIMEOUT: Duration = Duration::from_secs(10);
/// A server address that answers takes well under this from the same network.
const PROBE: Duration = Duration::from_secs(3);
const MAX_ADDRESSES: usize = 8;

fn headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Accept".into(), "application/json".into()),
        ("X-Plex-Product".into(), "Mynou".into()),
        ("X-Plex-Version".into(), env!("CARGO_PKG_VERSION").into()),
        ("X-Plex-Client-Identifier".into(), CLIENT.into()),
    ];
    headers.extend(token.map(|token| ("X-Plex-Token".into(), token.into())));
    headers
}

fn fetch(method: &str, url: &str, token: Option<&str>, timeout: Duration) -> Result<Value> {
    let response = HttpClient::new()
        .with_timeout(timeout)
        .with_max_body(4 << 20)
        .request(method, url, &headers(token), &[])
        .map_err(|_| "Plex could not be reached")?;
    match response.status {
        200..=299 => {}
        401 | 403 => return Err("Plex refused the account: connect to Plex again".into()),
        status => return Err(format!("Plex: HTTP response {status}")),
    }
    json::parse(std::str::from_utf8(&response.body).map_err(|_| "Plex: invalid UTF-8")?)
        .map_err(|_| "Plex: invalid JSON response".into())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

/// The servers among an account's resources that it owns.
fn owned_servers(resources: &Value) -> Vec<&Value> {
    resources
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter(|resource| {
            resource.get("owned").and_then(Value::as_bool) == Some(true)
                && text(resource, "provides").is_some_and(|p| p.split(',').any(|p| p == "server"))
                && text(resource, "clientIdentifier").is_some()
        })
        .collect()
}

/// Where a server may answer, most likely first: this machine, then its
/// addresses on the local network over plain HTTP, which needs no plex.direct
/// name, then the addresses Plex published. Relays are never used.
fn addresses(server: &Value) -> Vec<String> {
    let mut urls = vec!["http://127.0.0.1:32400".to_owned()];
    let connections = server
        .get("connections")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let usable = connections
        .iter()
        .filter(|c| c.get("relay").and_then(Value::as_bool) != Some(true));
    let (local, remote): (Vec<_>, Vec<_>) =
        usable.partition(|c| c.get("local").and_then(Value::as_bool) == Some(true));
    for connection in &local {
        if let (Some(address), Some(port)) = (
            text(connection, "address").filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok()),
            connection.get("port").and_then(Value::as_u64),
        ) {
            urls.push(format!("http://{address}:{port}"));
        }
    }
    for connection in local.iter().chain(&remote) {
        urls.extend(text(connection, "uri").map(String::from));
    }
    let mut seen = std::collections::BTreeSet::new();
    urls.retain(|url| crate::net::parse_url(url).is_ok() && seen.insert(url.clone()));
    urls.truncate(MAX_ADDRESSES);
    urls
}

#[derive(Debug, PartialEq)]
struct Section {
    key: String,
    title: String,
    kind: String,
    locations: Vec<String>,
}

fn sections(value: &Value) -> Vec<Section> {
    let directories = value
        .get("MediaContainer")
        .and_then(|c| c.get("Directory"))
        .and_then(Value::as_array)
        .unwrap_or_default();
    directories
        .iter()
        .filter_map(|directory| {
            Some(Section {
                key: text(directory, "key")
                    .filter(|k| k.bytes().all(|b| b.is_ascii_digit()))?
                    .into(),
                title: text(directory, "title").unwrap_or_default().into(),
                kind: text(directory, "type")?.into(),
                locations: directory
                    .get("Location")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|location| text(location, "path").map(String::from))
                    .collect(),
            })
        })
        .collect()
}

/// A folder of this machine as Plex names it, through the longest mapping.
fn as_plex_sees(folder: &Path, mappings: &[PathMapping]) -> String {
    let folder = folder.to_string_lossy();
    let under = |prefix: &str| {
        folder == prefix || prefix == "/" || folder.starts_with(&format!("{prefix}/"))
    };
    match mappings
        .iter()
        .filter(|m| under(&m.mynou_prefix))
        .max_by_key(|m| m.mynou_prefix.len())
    {
        Some(m) => {
            let rest = folder[m.mynou_prefix.len().min(folder.len())..].trim_start_matches('/');
            let base = m.plex_prefix.trim_end_matches('/');
            if rest.is_empty() {
                m.plex_prefix.clone()
            } else {
                format!("{base}/{rest}")
            }
        }
        None => folder.into_owned(),
    }
}

/// The section of `kind` that holds `folder`, else the one already chosen,
/// else the first of that kind.
fn pick<'a>(
    sections: &'a [Section],
    kind: &str,
    folder: &str,
    current: &str,
) -> Option<&'a Section> {
    let nested =
        |a: &str, b: &str| a == b || a.starts_with(&format!("{}/", b.trim_end_matches('/')));
    let of_kind = || sections.iter().filter(move |s| s.kind == kind);
    of_kind()
        .find(|s| {
            s.locations
                .iter()
                .any(|l| nested(folder, l) || nested(l, folder))
        })
        .or_else(|| of_kind().find(|s| s.key == current))
        .or_else(|| of_kind().next())
}

impl Engine {
    fn plex_token_name(&self) -> Result<String> {
        let document = self.current_settings()?;
        Ok(document
            .get("plex")
            .and_then(|plex| text(plex, "token_env"))
            .unwrap_or(&self.config.plex.token_env)
            .to_owned())
    }

    fn plex_token(&self) -> Result<String> {
        config::secret(&self.plex_token_name()?).map_err(|_| "Connect to Plex first".into())
    }

    /// Starts a sign-in: a PIN, and the Plex page where the owner confirms it.
    pub fn plex_pin(&self) -> Result<Value> {
        let pin = fetch(
            "POST",
            &format!("{PLEX_TV}/pins?strong=true"),
            None,
            TIMEOUT,
        )?;
        let id = pin
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("Plex: invalid PIN")?;
        let code = text(&pin, "code")
            .filter(|code| !code.is_empty() && code.bytes().all(|b| b.is_ascii_alphanumeric()))
            .ok_or("Plex: invalid PIN")?;
        let mut value = Value::object();
        value.insert("id", Value::Number(id as f64));
        value.insert(
            "url",
            format!("https://app.plex.tv/auth#?clientID={CLIENT}&code={code}&context%5Bdevice%5D%5Bproduct%5D=Mynou"),
        );
        Ok(value)
    }

    /// Whether the owner confirmed the PIN; once they have, the account token
    /// is saved as the Plex token.
    pub fn plex_pin_state(&self, id: u64) -> Result<Value> {
        let pin = fetch("GET", &format!("{PLEX_TV}/pins/{id}"), None, TIMEOUT).map_err(|e| {
            if e.ends_with(" 404") {
                "The Plex sign-in expired: try again".into()
            } else {
                e
            }
        })?;
        let token = text(&pin, "authToken").filter(|token| !token.is_empty());
        if let Some(token) = token {
            let mut secret = Value::object();
            secret.insert("name", self.plex_token_name()?);
            secret.insert("value", token);
            self.update_secret(&secret)?;
        }
        let mut value = Value::object();
        value.insert("signed_in", token.is_some());
        Ok(value)
    }

    /// The Plex Media Servers the account owns, and which one Mynou uses.
    pub fn plex_servers(&self) -> Result<Value> {
        let token = self.plex_token()?;
        let resources = fetch(
            "GET",
            &format!("{PLEX_TV}/resources?includeHttps=1&includeIPv6=1"),
            Some(&token),
            TIMEOUT,
        )?;
        let url = self.config.plex.url.as_str();
        let servers = owned_servers(&resources)
            .into_iter()
            .map(|server| {
                let mut entry = Value::object();
                entry.insert("id", text(server, "clientIdentifier").unwrap_or_default());
                entry.insert("name", text(server, "name").unwrap_or("Plex Media Server"));
                entry.insert(
                    "current",
                    self.config.plex.enabled && addresses(server).iter().skip(1).any(|a| a == url),
                );
                entry
            })
            .collect();
        let mut value = Value::object();
        value.insert("servers", Value::Array(servers));
        Ok(value)
    }

    /// Connects Mynou to one of those servers: an address that answers from
    /// here and its film and series sections, saved with the settings.
    pub fn plex_choose(&self, body: &Value) -> Result<Value> {
        let id = text(body, "id").ok_or("id is required")?;
        let token = self.plex_token()?;
        let resources = fetch(
            "GET",
            &format!("{PLEX_TV}/resources?includeHttps=1&includeIPv6=1"),
            Some(&token),
            TIMEOUT,
        )?;
        let server = owned_servers(&resources)
            .into_iter()
            .find(|server| text(server, "clientIdentifier") == Some(id))
            .ok_or("This Plex server is no longer on the account")?;
        let name = text(server, "name").unwrap_or("Plex Media Server");
        // An address counts only when the server there is the one chosen.
        let url = addresses(server)
            .into_iter()
            .find(|url| {
                fetch("GET", &format!("{url}/identity"), Some(&token), PROBE).is_ok_and(
                    |identity| {
                        identity
                            .get("MediaContainer")
                            .and_then(|c| text(c, "machineIdentifier"))
                            == Some(id)
                    },
                )
            })
            .ok_or_else(|| {
                format!("{name} does not answer from the Mac running Mynou: check that it is on")
            })?;
        let found = sections(&fetch(
            "GET",
            &format!("{url}/library/sections"),
            Some(&token),
            TIMEOUT,
        )?);
        let mut document = self.current_settings()?;
        if document.get("plex").and_then(Value::as_object).is_none() {
            document.insert("plex", Value::object());
        }
        let plex = document
            .get_mut("plex")
            .ok_or("settings: plex is missing")?;
        let mappings = &self.config.plex.path_mappings;
        let mut value = Value::object();
        for (field, kind, folder) in [
            ("movies_section", "movie", &self.config.movies_root),
            ("series_section", "show", &self.config.series_root),
        ] {
            let current = text(plex, field).unwrap_or_default().to_owned();
            if let Some(section) = pick(&found, kind, &as_plex_sees(folder, mappings), &current) {
                plex.insert(field, section.key.as_str());
                value.insert(field, section.title.as_str());
            }
        }
        plex.insert("url", url.as_str());
        plex.insert("enabled", true);
        let mut settings = Value::object();
        settings.insert("settings", document);
        value.insert(
            "state",
            self.update_settings(&settings)?
                .get("state")
                .cloned()
                .unwrap_or(Value::Null),
        );
        value.insert("name", name);
        Ok(value)
    }

    /// The film and series sections of the server Mynou uses, to choose from.
    pub fn plex_sections(&self) -> Result<Value> {
        let token = self.plex_token()?;
        let found = sections(&fetch(
            "GET",
            &format!("{}/library/sections", self.config.plex.url),
            Some(&token),
            TIMEOUT,
        )?);
        let sections = found
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "movie" | "show"))
            .map(|s| {
                let mut entry = Value::object();
                entry.insert("key", s.key.as_str());
                entry.insert("title", s.title.as_str());
                entry.insert("type", s.kind.as_str());
                entry
            })
            .collect();
        let mut value = Value::object();
        value.insert("sections", Value::Array(sections));
        Ok(value)
    }

    /// Forgets the Plex token and stops using Plex.
    pub fn plex_disconnect(&self) -> Result<Value> {
        let mut secret = Value::object();
        secret.insert("name", self.plex_token_name()?);
        secret.insert("value", Value::Null);
        self.update_secret(&secret)?;
        let mut document = self.current_settings()?;
        if let Some(plex) = document.get_mut("plex").filter(|p| p.as_object().is_some()) {
            plex.insert("enabled", false);
        }
        let mut settings = Value::object();
        settings.insert("settings", document);
        self.update_settings(&settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_come_first_and_relays_never() {
        let server = json::parse(r#"{"connections":[
            {"protocol":"https","address":"203.0.113.9","port":32400,"uri":"https://203-0-113-9.abc.plex.direct:32400","local":false,"relay":false},
            {"protocol":"https","address":"192.168.1.20","port":32400,"uri":"https://192-168-1-20.abc.plex.direct:32400","local":true,"relay":false},
            {"protocol":"https","address":"10.0.0.1","port":8443,"uri":"https://10-0-0-1.abc.plex.direct:8443","local":false,"relay":true}
        ]}"#).unwrap();
        assert_eq!(
            addresses(&server),
            [
                "http://127.0.0.1:32400",
                "http://192.168.1.20:32400",
                "https://192-168-1-20.abc.plex.direct:32400",
                "https://203-0-113-9.abc.plex.direct:32400",
            ]
        );
    }

    #[test]
    fn sections_follow_the_library_folders() {
        let found = sections(
            &json::parse(
                r#"{"MediaContainer":{"Directory":[
            {"key":"1","title":"Films","type":"movie","Location":[{"path":"/data/Films"}]},
            {"key":"4","title":"Kids","type":"movie","Location":[{"path":"/data/Kids"}]},
            {"key":"2","title":"Series","type":"show","Location":[{"path":"/data/TV"}]},
            {"key":"x","title":"Bad","type":"movie"}
        ]}}"#,
            )
            .unwrap(),
        );
        assert_eq!(found.len(), 3);
        let mappings = [PathMapping {
            mynou_prefix: "/Volumes/Media".into(),
            plex_prefix: "/data".into(),
        }];
        let kids = as_plex_sees(Path::new("/Volumes/Media/Kids"), &mappings);
        assert_eq!(kids, "/data/Kids");
        assert_eq!(pick(&found, "movie", &kids, "1").unwrap().key, "4");
        // Without a match, the section already chosen stays, else the first.
        assert_eq!(pick(&found, "movie", "/elsewhere", "4").unwrap().key, "4");
        assert_eq!(pick(&found, "movie", "/elsewhere", "9").unwrap().key, "1");
        assert_eq!(pick(&found, "show", "/data/TV/Shows", "").unwrap().key, "2");
        assert!(pick(&found, "artist", "/", "").is_none());
    }
}
