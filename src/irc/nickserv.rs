//! Required service identification with exact trusted account confirmation.
use super::protocol::{Event, Message};
use crate::{Result, json::Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    account: String,
    password_env: String,
    service: String,
    sender: String,
    success_notice: String,
    failure_notices: Vec<String>,
}
impl Settings {
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        super::only(
            v,
            &[
                "account",
                "password_env",
                "service",
                "sender",
                "success_notice",
                "failure_notices",
            ],
        )?;
        let account = super::text(v, "account")?;
        let password_env =
            super::variable(v, "password_env")?.ok_or("IRC: NickServ requires password_env")?;
        let service = super::text(v, "service")?;
        let sender = super::text(v, "sender")?;
        let success_notice = super::text(v, "success_notice")?;
        let notices = v
            .get("failure_notices")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty() && v.len() <= 8)
            .ok_or("IRC: NickServ requires bounded failure notices")?;
        let valid_notice =
            |s: &str| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control);
        let mut failure_notices = Vec::new();
        for notice in notices {
            let s = notice
                .as_str()
                .filter(|s| valid_notice(s))
                .ok_or("IRC: invalid NickServ failure notice")?;
            if s.contains(['{', '}']) || failure_notices.iter().any(|v| v == s) {
                return Err("IRC: ambiguous NickServ failure notices".into());
            }
            failure_notices.push(s.to_owned());
        }
        let (nick, identity) = sender
            .split_once('!')
            .and_then(|(n, p)| p.split_once('@').map(|(u, h)| (n, (u, h))))
            .ok_or("IRC: NickServ requires an exact service sender")?;
        let remainder = success_notice.replacen("{account}", "", 1);
        if account.is_empty()
            || account.len() > 64
            || !account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || !super::nickname(&service)
            || nick != service
            || sender.len() > 128
            || identity.0.is_empty()
            || identity.1.is_empty()
            || !identity
                .0
                .bytes()
                .chain(identity.1.bytes())
                .all(|b| b.is_ascii_alphanumeric() || b"._-~:/".contains(&b))
            || !valid_notice(&success_notice)
            || !success_notice.contains("{account}")
            || remainder.contains(['{', '}'])
            || failure_notices.contains(&success_notice.replace("{account}", &account))
        {
            return Err("IRC: invalid NickServ identity or confirmation".into());
        }
        Ok(Self {
            account,
            password_env,
            service,
            sender,
            success_notice,
            failure_notices,
        })
    }
    pub(crate) fn configuration(&self) -> Value {
        let mut v = Value::object();
        for (k, s) in [
            ("account", &self.account),
            ("password_env", &self.password_env),
            ("service", &self.service),
            ("sender", &self.sender),
            ("success_notice", &self.success_notice),
        ] {
            v.insert(k, s.clone());
        }
        v.insert(
            "failure_notices",
            Value::Array(
                self.failure_notices
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        v
    }
    pub(crate) fn command(&self) -> Result<String> {
        let password = std::env::var(&self.password_env)
            .map_err(|_| "IRC: NickServ credential is unavailable")?;
        if password.is_empty()
            || password.len() > 256
            || !password.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err("IRC: NickServ credential is invalid".into());
        }
        Ok(format!(
            "PRIVMSG {} :IDENTIFY {} {}",
            self.service, self.account, password
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Disabled,
    Waiting,
    Identifying,
    Complete,
    Failed,
}
pub(crate) struct Negotiation {
    settings: Option<Settings>,
    phase: Phase,
}
impl Negotiation {
    pub(crate) fn new(settings: Option<Settings>) -> Self {
        let phase = if settings.is_some() {
            Phase::Waiting
        } else {
            Phase::Disabled
        };
        Self { settings, phase }
    }
    pub(crate) fn fail(&mut self) {
        if self.phase != Phase::Disabled {
            self.phase = Phase::Failed;
        }
    }
    pub(crate) fn ready(&self) -> bool {
        matches!(self.phase, Phase::Disabled | Phase::Complete)
    }
    pub(crate) fn begin(&mut self) -> Result<Event> {
        match self.phase {
            Phase::Disabled => Ok(Event::Join),
            Phase::Waiting => {
                self.phase = Phase::Identifying;
                Ok(Event::Identify)
            }
            _ => Err("IRC: unexpected NickServ registration state".into()),
        }
    }
    pub(crate) fn receive(&mut self, m: &Message, nickname: &str) -> Result<Option<Event>> {
        if self.phase == Phase::Failed {
            return Err("IRC: NickServ requires a new connection".into());
        }
        let Some(s) = &self.settings else {
            return Ok(None);
        };
        if m.command != "NOTICE"
            || m.params.len() != 2
            || !m.params[0].eq_ignore_ascii_case(nickname)
            || m.prefix.as_deref() != Some(s.sender.as_str())
        {
            return Ok(None);
        }
        let body = super::format::formatting(&m.params[1])?;
        if s.failure_notices.contains(&body) {
            return Err("IRC: required NickServ identification failed".into());
        }
        if body == s.success_notice.replace("{account}", &s.account) {
            if self.phase == Phase::Identifying {
                self.phase = Phase::Complete;
                return Ok(Some(Event::Join));
            }
            if self.phase != Phase::Complete {
                return Err("IRC: unexpected NickServ confirmation".into());
            }
        }
        Ok(Some(Event::Ignore))
    }
}
