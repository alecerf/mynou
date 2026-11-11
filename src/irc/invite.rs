//! Optional invite command for an invite-only channel: once identified, ask a
//! bot for an invitation, wait a bounded time for it, then join. The secret
//! part of the message is read for each connection, and never saved, logged
//! or reported.
use super::protocol::{Event, Message};
use crate::{Result, json::Value};

/// Invitations asked again after the channel refuses a join as invite-only.
pub const RETRIES: u32 = 3;
const PLACEHOLDER: &str = "{secret}";
/// Error once every retry is refused; the client reports why.
pub(crate) const NOT_INVITED: &str = "IRC: the invite-only channel refused every join";
/// Error when an invite-only channel refuses a source without invite command.
pub(crate) const COMMAND_REQUIRED: &str = "IRC: the channel is invite-only";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub nick: String,
    message: String,
    secret_env: String,
    pub timeout_secs: u64,
}
impl Settings {
    pub(crate) fn from_json(v: &Value) -> Result<Self> {
        super::only(v, &["nick", "message", "secret_env", "timeout_secs"])?;
        let nick = super::text(v, "nick")?;
        let message = super::text(v, "message")?;
        let secret_env = super::variable(v, "secret_env")?
            .ok_or("IRC: an invite command requires secret_env")?;
        let timeout_secs = super::count(v, "timeout_secs", 10, 1, 60)?;
        // With the longest nickname and secret, the command still fits one line.
        if !super::nickname(&nick)
            || message.len() > 200
            || message.chars().any(char::is_control)
            || message.matches(PLACEHOLDER).count() != 1
            || message.replacen(PLACEHOLDER, "", 1).contains(['{', '}'])
        {
            return Err(
                "IRC: an invite command needs a nickname and a message of up to 200 bytes with {secret} once".into(),
            );
        }
        Ok(Self {
            nick,
            message,
            secret_env,
            timeout_secs,
        })
    }
    pub(crate) fn command(&self) -> Result<String> {
        let secret = crate::config::lookup(&self.secret_env)
            .ok_or("IRC: invite credential is unavailable")?;
        self.command_for(&secret)
    }
    fn command_for(&self, secret: &str) -> Result<String> {
        if secret.is_empty()
            || secret.len() > 256
            || !secret.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err("IRC: invite credential is invalid".into());
        }
        Ok(format!(
            "PRIVMSG {} :{}",
            self.nick,
            self.message.replacen(PLACEHOLDER, secret, 1)
        ))
    }
}

/// Why a source failed with one of this module's errors: a code and a
/// sentence naming only its configuration, for its health.
pub(crate) fn failure(source: &super::Source, error: &str) -> Option<(&'static str, String)> {
    match (error, &source.invite) {
        (NOT_INVITED, Some(invite)) => Some((
            "invite_not_received",
            format!(
                "{} is invite-only and {} did not invite {} after {} invite commands",
                source.channel,
                invite.nick,
                source.nickname,
                RETRIES + 1
            ),
        )),
        (COMMAND_REQUIRED, None) => Some((
            "invite_command_required",
            format!(
                "{} is invite-only: this source needs an invite command",
                source.channel
            ),
        )),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Not identified yet, or no invite command.
    Idle,
    /// The invite command is sent; the bot's INVITE or the timeout comes next.
    Waiting,
    /// JOIN is sent.
    Joining,
    /// The channel refused the join; the invite command is sent again later.
    Backoff,
}
pub(crate) struct Negotiation {
    configured: bool,
    phase: Phase,
    retries: u32,
    reconnect: (u64, u64),
}
impl Negotiation {
    pub(crate) fn new(source: &super::Source) -> Self {
        Self {
            configured: source.invite.is_some(),
            phase: Phase::Idle,
            retries: 0,
            reconnect: (source.reconnect_min_secs, source.reconnect_max_secs),
        }
    }
    /// Registration and identification are complete: join, or first ask the
    /// bot for an invitation.
    pub(crate) fn identified(&mut self, event: Event) -> Event {
        if event != Event::Join {
            return event;
        }
        if self.configured && self.phase == Phase::Idle {
            self.phase = Phase::Waiting;
            return Event::Invite;
        }
        self.phase = Phase::Joining;
        Event::Join
    }
    /// An INVITE naming this nickname and the configured channel.
    pub(crate) fn invited(&mut self) -> Event {
        if self.configured && matches!(self.phase, Phase::Waiting | Phase::Backoff) {
            self.phase = Phase::Joining;
            return Event::Join;
        }
        Event::Ignore
    }
    /// The wait for an INVITE, or the delay before asking again, is over.
    pub(crate) fn elapsed(&mut self) -> Event {
        match self.phase {
            Phase::Waiting => {
                self.phase = Phase::Joining;
                Event::Join
            }
            Phase::Backoff => {
                self.phase = Phase::Waiting;
                Event::Invite
            }
            Phase::Idle | Phase::Joining => Event::Ignore,
        }
    }
    /// ERR_INVITEONLYCHAN for the configured channel.
    pub(crate) fn refused(&mut self) -> Result<Event> {
        if !self.configured {
            return Err(COMMAND_REQUIRED.into());
        }
        if self.phase != Phase::Joining {
            return Err("IRC: registration or channel rejected".into());
        }
        if self.retries == RETRIES {
            return Err(NOT_INVITED.into());
        }
        self.retries += 1;
        self.phase = Phase::Backoff;
        let (min, max) = self.reconnect;
        Ok(Event::Retry(
            min.saturating_mul(1 << self.retries.min(16)).min(max),
        ))
    }
}

/// Whether an INVITE names `nickname` and `channel`; anyone able to invite to
/// the channel may, as the join is attempted after the timeout anyway.
pub(crate) fn invitation(m: &Message, nickname: &str, channel: &str) -> bool {
    m.command == "INVITE"
        && m.params.len() == 2
        && m.params[0].eq_ignore_ascii_case(nickname)
        && m.params[1].eq_ignore_ascii_case(channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(extra: &str) -> Result<Settings> {
        Settings::from_json(
            &crate::json::parse(&format!(
                r#"{{"nick":"Bot","message":"invite {{secret}}","secret_env":"MYNOU_TEST_INVITE"{extra}}}"#
            ))
            .unwrap(),
        )
    }
    #[test]
    fn the_command_carries_the_secret_once_and_cannot_inject_commands() {
        let s = settings("").unwrap();
        assert_eq!(s.timeout_secs, 10);
        assert_eq!(s.command_for("k3y").unwrap(), "PRIVMSG Bot :invite k3y");
        let longest = Settings::from_json(
            &crate::json::parse(&format!(
                r#"{{"nick":"{}","message":"{}{{secret}}","secret_env":"MYNOU_TEST_INVITE"}}"#,
                "B".repeat(24),
                "m".repeat(192)
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(longest.command_for(&"k".repeat(256)).unwrap().len() <= 510);
        for bad in [
            String::new(),
            "k".repeat(257),
            "two words".into(),
            "k\r\nJOIN #other".into(),
            "k\0".into(),
            "k\u{7f}".into(),
            "clé".into(),
        ] {
            assert_eq!(
                s.command_for(&bad).unwrap_err(),
                "IRC: invite credential is invalid"
            );
        }
        // Only the variable's name is kept.
        assert!(!format!("{s:?}").contains("k3y"));
    }
    #[test]
    fn the_configuration_is_strict() {
        assert_eq!(settings(r#","timeout_secs":60"#).unwrap().timeout_secs, 60);
        for extra in [
            r#","timeout_secs":0"#,
            r#","timeout_secs":61"#,
            r#","secret":"private-value""#,
        ] {
            assert!(!settings(extra).unwrap_err().contains("private-value"));
        }
        for (nick, message, env) in [
            ("Bot", "invite", "MYNOU_TEST_INVITE"),
            ("Bot", "invite {secret} {secret}", "MYNOU_TEST_INVITE"),
            ("Bot", "invite {secret} {other}", "MYNOU_TEST_INVITE"),
            ("Bot", "invite {secret}\r\nQUIT", "MYNOU_TEST_INVITE"),
            ("Bot", "invite \u{1}{secret}", "MYNOU_TEST_INVITE"),
            ("", "invite {secret}", "MYNOU_TEST_INVITE"),
            ("Bot Two", "invite {secret}", "MYNOU_TEST_INVITE"),
            ("1Bot", "invite {secret}", "MYNOU_TEST_INVITE"),
            ("Bot", "invite {secret}", "lowercase"),
        ] {
            let mut v = Value::object();
            v.insert("nick", nick);
            v.insert("message", message);
            v.insert("secret_env", env);
            assert!(Settings::from_json(&v).is_err(), "{nick} {message} {env}");
        }
        let mut v = Value::object();
        v.insert("nick", "Bot");
        v.insert("message", format!("{}{{secret}}", "m".repeat(193)));
        v.insert("secret_env", "MYNOU_TEST_INVITE");
        assert!(Settings::from_json(&v).is_err());
        v.insert("message", "invite {secret}");
        v.insert("secret_env", Value::Null);
        assert!(Settings::from_json(&v).is_err());
    }
    #[test]
    fn an_invite_command_changes_neither_the_source_binding_nor_its_decisions() {
        let profiles = crate::selection::SelectionConfig::default();
        let parse = |s: &str| crate::json::parse(s).unwrap();
        let mut source = parse(
            r##"{"id":"local","url":"irc://127.0.0.1:1","channel":"#announces","sender":"announce!bot@fixture"}"##,
        );
        let irc = |source: &Value| {
            let mut v = Value::object();
            v.insert("sources", Value::Array(vec![source.clone()]));
            super::super::Settings::from_json(&v, &profiles).unwrap()
        };
        let plain = irc(&source);
        source.insert(
            "invite",
            parse(r#"{"nick":"Bot","message":"invite {secret}","secret_env":"MYNOU_TEST_INVITE"}"#),
        );
        let invited = irc(&source);
        assert_eq!(invited.sources[0].invite.as_ref().unwrap().nick, "Bot");
        assert_eq!(plain.sources[0].binding(), invited.sources[0].binding());
        assert_eq!(plain.fingerprint(&profiles), invited.fingerprint(&profiles));
    }
}
