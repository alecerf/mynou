//! Required SASL PLAIN: bounded capability negotiation and transient credentials.
use super::protocol::{Event, Message};
use crate::{Result, json::Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub username_env: String,
    pub password_env: String,
    pub authorization_env: Option<String>,
}
impl Settings {
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        super::only(
            v,
            &[
                "mechanism",
                "username_env",
                "password_env",
                "authorization_env",
            ],
        )?;
        if super::text(v, "mechanism")? != "PLAIN" {
            return Err("IRC: SASL requires the PLAIN mechanism".into());
        }
        Ok(Self {
            username_env: super::variable(v, "username_env")?
                .ok_or("IRC: SASL requires username_env")?,
            password_env: super::variable(v, "password_env")?
                .ok_or("IRC: SASL requires password_env")?,
            authorization_env: super::variable(v, "authorization_env")?,
        })
    }
    pub(crate) fn configuration(&self) -> Value {
        let mut v = Value::object();
        v.insert("mechanism", "PLAIN");
        v.insert("username_env", self.username_env.clone());
        v.insert("password_env", self.password_env.clone());
        v.insert(
            "authorization_env",
            self.authorization_env
                .clone()
                .map_or(Value::Null, Value::from),
        );
        v
    }
    pub(crate) fn commands(&self) -> Result<Vec<String>> {
        let read = |name: &str| {
            std::env::var(name).map_err(|_| "IRC: SASL credential is unavailable".to_string())
        };
        let username = read(&self.username_env)?;
        let password = read(&self.password_env)?;
        let authorization = self.authorization_env.as_deref().map(read).transpose()?;
        if authorization.as_ref().is_some_and(String::is_empty) {
            return Err("IRC: SASL credential is invalid".into());
        }
        commands(&username, &password, authorization.as_deref().unwrap_or(""))
    }
}

fn commands(username: &str, password: &str, authorization: &str) -> Result<Vec<String>> {
    if username.is_empty()
        || password.is_empty()
        || [username, password, authorization]
            .iter()
            .any(|s| s.len() > 256 || s.chars().any(char::is_control))
    {
        return Err("IRC: SASL credential is invalid".into());
    }
    let mut payload = Vec::with_capacity(authorization.len() + username.len() + password.len() + 2);
    payload.extend_from_slice(authorization.as_bytes());
    payload.push(0);
    payload.extend_from_slice(username.as_bytes());
    payload.push(0);
    payload.extend_from_slice(password.as_bytes());
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(payload.len().div_ceil(3) * 4);
    for block in payload.chunks(3) {
        let a = block[0];
        let b = block.get(1).copied().unwrap_or(0);
        let c = block.get(2).copied().unwrap_or(0);
        for symbol in [
            ALPHABET[(a >> 2) as usize],
            ALPHABET[((a & 3) << 4 | b >> 4) as usize],
            if block.len() > 1 {
                ALPHABET[((b & 15) << 2 | c >> 6) as usize]
            } else {
                b'='
            },
            if block.len() > 2 {
                ALPHABET[(c & 63) as usize]
            } else {
                b'='
            },
        ] {
            encoded.push(symbol as char);
        }
    }
    let mut result = encoded
        .as_bytes()
        .chunks(400)
        .map(|chunk| format!("AUTHENTICATE {}", String::from_utf8_lossy(chunk)))
        .collect::<Vec<_>>();
    if encoded.len().is_multiple_of(400) {
        result.push("AUTHENTICATE +".into());
    }
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Disabled,
    Listing,
    Requested,
    Challenge,
    Result,
    Complete,
    Failed,
}
pub(crate) struct Negotiation {
    phase: Phase,
    capabilities: BTreeSet<String>,
    lines: usize,
    bytes: usize,
    plain: bool,
}
impl Negotiation {
    pub(crate) fn new(required: bool) -> Self {
        Self {
            phase: if required {
                Phase::Listing
            } else {
                Phase::Disabled
            },
            capabilities: BTreeSet::new(),
            lines: 0,
            bytes: 0,
            plain: false,
        }
    }
    pub(crate) fn receive(&mut self, m: &Message, nickname: &str) -> Result<Option<Event>> {
        if self.phase == Phase::Failed {
            return Err("IRC: SASL requires a new connection".into());
        }
        let result = self.advance(m, nickname);
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }
    fn advance(&mut self, m: &Message, nickname: &str) -> Result<Option<Event>> {
        if self.phase == Phase::Disabled {
            return Ok(None);
        }
        let control = matches!(
            m.command.as_str(),
            "CAP"
                | "AUTHENTICATE"
                | "001"
                | "900"
                | "901"
                | "902"
                | "903"
                | "904"
                | "905"
                | "906"
                | "907"
                | "908"
                | "421"
        );
        if control && m.prefix.as_ref().is_some_and(|p| p.contains(['!', '@'])) {
            return Ok(Some(Event::Ignore));
        }
        let addressed = m
            .params
            .first()
            .is_some_and(|p| p == "*" || p.eq_ignore_ascii_case(nickname));
        let targeted = m
            .params
            .first()
            .is_some_and(|p| p.eq_ignore_ascii_case(nickname));
        let event = match m.command.as_str() {
            "CAP" if addressed => match m.params.get(1).map(String::as_str) {
                Some("LS") if self.phase == Phase::Listing => {
                    let more = match m.params.as_slice() {
                        [_, _, _] => false,
                        [_, _, marker, _] if marker == "*" => true,
                        _ => return Err("IRC: invalid SASL capability framing".into()),
                    };
                    self.lines += 1;
                    let list = m.params.last().ok_or("IRC: missing SASL capabilities")?;
                    self.bytes += list.len();
                    if self.lines > 16 || self.bytes > 4096 {
                        return Err("IRC: SASL capabilities exceed bounds".into());
                    }
                    for token in list.split(' ').filter(|s| !s.is_empty()) {
                        let (name, value) = token
                            .split_once('=')
                            .map_or((token, None), |(n, v)| (n, Some(v)));
                        if name.is_empty()
                            || name.len() > 128
                            || token.len() > 256
                            || !name
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-./".contains(&b))
                            || !token.bytes().all(|b| (33..=126).contains(&b))
                            || self.capabilities.len() == 64
                            || !self.capabilities.insert(name.into())
                        {
                            return Err("IRC: invalid or duplicate SASL capabilities".into());
                        }
                        if name == "sasl" {
                            self.plain = match value {
                                None => true,
                                Some(v) => {
                                    let mut mechanisms = BTreeSet::new();
                                    for mechanism in v.split(',') {
                                        if mechanism.is_empty()
                                            || mechanism.len() > 32
                                            || !mechanism.bytes().all(|b| {
                                                b.is_ascii_uppercase()
                                                    || b.is_ascii_digit()
                                                    || b"-_".contains(&b)
                                            })
                                            || mechanisms.len() == 16
                                            || !mechanisms.insert(mechanism)
                                        {
                                            return Err(
                                                "IRC: invalid SASL mechanism advertisement".into(),
                                            );
                                        }
                                    }
                                    mechanisms.contains("PLAIN")
                                }
                            };
                        }
                    }
                    if more {
                        Event::Ignore
                    } else {
                        if !self.plain {
                            return Err("IRC: required SASL PLAIN is unavailable".into());
                        }
                        self.capabilities.clear();
                        self.phase = Phase::Requested;
                        Event::Reply("CAP REQ :sasl".into())
                    }
                }
                Some("ACK")
                    if self.phase == Phase::Requested
                        && m.params.len() == 3
                        && m.params[2] == "sasl" =>
                {
                    self.phase = Phase::Challenge;
                    Event::Reply("AUTHENTICATE PLAIN".into())
                }
                Some("NEW" | "LIST") => Event::Ignore,
                Some("DEL")
                    if m.params.len() == 3 && !m.params[2].split(' ').any(|s| s == "sasl") =>
                {
                    Event::Ignore
                }
                _ => return Err("IRC: required SASL capability rejected".into()),
            },
            "AUTHENTICATE" => {
                if self.phase != Phase::Challenge || m.params.as_slice() != ["+"] {
                    return Err("IRC: invalid SASL challenge or state".into());
                }
                self.phase = Phase::Result;
                Event::Authenticate
            }
            "903" if targeted => {
                if self.phase != Phase::Result {
                    return Err("IRC: unexpected SASL success".into());
                }
                self.phase = Phase::Complete;
                Event::Reply("CAP END".into())
            }
            "001" if targeted && self.phase != Phase::Complete => {
                return Err("IRC: required SASL did not complete".into());
            }
            "901" | "902" | "904" | "905" | "906" | "907" if targeted => {
                return Err("IRC: required SASL authentication failed".into());
            }
            "421"
                if targeted
                    && m.params
                        .get(1)
                        .is_some_and(|p| p == "CAP" || p == "AUTHENTICATE") =>
            {
                return Err("IRC: required SASL commands are unavailable".into());
            }
            _ => return Ok(None),
        };
        Ok(Some(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plain_vectors_and_exact_chunks_preserve_bytes_and_bound_secrets() {
        assert_eq!(
            commands("jilles", "sesame", "jilles").unwrap(),
            ["AUTHENTICATE amlsbGVzAGppbGxlcwBzZXNhbWU="]
        );
        assert_eq!(
            commands("user", "pass", "").unwrap(),
            ["AUTHENTICATE AHVzZXIAcGFzcw=="]
        );
        assert_eq!(commands("u", "p", "").unwrap(), ["AUTHENTICATE AHUAcA=="]);
        let chunks = commands(&"u".repeat(100), &"p".repeat(198), "").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 413);
        assert_eq!(chunks[1], "AUTHENTICATE +");
        let chunks = commands(&"u".repeat(256), &"p".repeat(256), &"a".repeat(256)).unwrap();
        assert_eq!(
            chunks.iter().map(String::len).collect::<Vec<_>>(),
            [413, 413, 241]
        );
        for (u, p, a) in [
            ("", "p", ""),
            ("u", "", ""),
            ("u\0x", "p", ""),
            ("u", "p\r\n", ""),
            ("u", "p", "a\u{7f}"),
        ] {
            assert_eq!(
                commands(u, p, a).unwrap_err(),
                "IRC: SASL credential is invalid"
            );
        }
        assert!(commands("u", &"p".repeat(257), "").is_err());
        assert!(commands("u", "p", &"a".repeat(257)).is_err());
    }
}
