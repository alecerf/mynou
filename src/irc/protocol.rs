//! Incremental bounded IRC framing; unknown commands and tags grant no authority.
use crate::Result;
pub const MAX_LINE: usize = 8192;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}
impl Message {
    pub fn parse(line: &str) -> Result<Self> {
        if line.is_empty() || line.len() > MAX_LINE - 2 || line.contains(['\r', '\n', '\0']) {
            return Err("IRC: invalid protocol line".into());
        }
        let mut rest = line;
        if let Some(tags) = rest.strip_prefix('@') {
            let (tags, tail) = tags.split_once(' ').ok_or("IRC: invalid message tags")?;
            if tags.chars().any(char::is_control) {
                return Err("IRC: invalid message tag controls".into());
            }
            let mut keys = std::collections::BTreeSet::new();
            for tag in tags.split(';') {
                let key = tag.split('=').next().unwrap_or("");
                if keys.len() >= 32
                    || key.is_empty()
                    || key.len() > 128
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-./+".contains(&b))
                    || !keys.insert(key)
                {
                    return Err("IRC: invalid or duplicate message tags".into());
                }
            }
            rest = tail.trim_start_matches(' ');
        }
        let prefix = if let Some(p) = rest.strip_prefix(':') {
            let (p, tail) = p.split_once(' ').ok_or("IRC: invalid sender prefix")?;
            if p.is_empty() || p.len() > 128 || !p.is_ascii() || p.chars().any(char::is_control) {
                return Err("IRC: invalid sender prefix".into());
            }
            rest = tail.trim_start_matches(' ');
            Some(p.into())
        } else {
            None
        };
        let (command, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        if command.is_empty()
            || command.len() > 16
            || !command.bytes().all(|b| b.is_ascii_uppercase())
                && !(command.len() == 3 && command.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err("IRC: invalid command".into());
        }
        let mut params: Vec<String> = Vec::new();
        rest = tail.trim_start_matches(' ');
        while !rest.is_empty() {
            if params.len() == 15 {
                return Err("IRC: too many command parameters".into());
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                params.push(trailing.into());
                break;
            }
            let (p, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            params.push(p.into());
            rest = tail.trim_start_matches(' ');
        }
        if params.iter().enumerate().any(|(i, p)| {
            p.chars().any(|c| {
                c.is_control()
                    && !(i == 1
                        && params.len() == 2
                        && matches!(command, "NOTICE" | "PRIVMSG")
                        && super::format::style(c))
            })
        }) {
            return Err("IRC: invalid command controls".into());
        }
        Ok(Self {
            prefix,
            command: command.into(),
            params,
        })
    }
}
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    failed: bool,
}
impl Decoder {
    pub fn has_partial(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Message>> {
        if self.failed {
            return Err("IRC: decoder requires a new connection".into());
        }
        let result = (|| {
            if bytes.len() > 4096 {
                return Err("IRC: input chunk exceeds bounds".into());
            }
            let mut messages = Vec::new();
            for &b in bytes {
                if self.pending.len() == MAX_LINE {
                    return Err("IRC: protocol line exceeds bounds".into());
                }
                self.pending.push(b);
                if b == b'\n' {
                    if messages.len() == 128 || !self.pending.ends_with(b"\r\n") {
                        return Err("IRC: invalid line framing or message batch".into());
                    }
                    let line = std::str::from_utf8(&self.pending[..self.pending.len() - 2])
                        .map_err(|_| "IRC: protocol line is not UTF-8")?;
                    messages.push(Message::parse(line)?);
                    self.pending.clear();
                }
            }
            Ok(messages)
        })();
        if result.is_err() {
            self.failed = true;
            self.pending.clear();
        }
        result
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Ignore,
    Reply(String),
    Authenticate,
    Join,
    Joined,
    Announcement(String),
}
pub struct Protocol {
    source: super::Source,
    registered: bool,
    joined: bool,
    authentication: super::sasl::Negotiation,
}
impl Protocol {
    pub fn new(source: super::Source) -> Self {
        let authentication = super::sasl::Negotiation::new(source.sasl.is_some());
        Self {
            source,
            registered: false,
            joined: false,
            authentication,
        }
    }
    pub fn receive(&mut self, m: &Message) -> Result<Event> {
        if let Some(event) = self.authentication.receive(m, &self.source.nickname)? {
            return Ok(event);
        }
        match m.command.as_str() {
            "PING" if (1..=2).contains(&m.params.len()) => Ok(Event::Reply(format!(
                "PONG :{}",
                m.params.last().ok_or("IRC: missing ping token")?
            ))),
            "001"
                if !self.registered
                    && m.params
                        .first()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&self.source.nickname)) =>
            {
                self.registered = true;
                Ok(Event::Join)
            }
            "366"
                if self.registered
                    && m.params.len() >= 2
                    && m.params[0].eq_ignore_ascii_case(&self.source.nickname)
                    && m.params[1].eq_ignore_ascii_case(&self.source.channel) =>
            {
                self.joined = true;
                Ok(Event::Joined)
            }
            "JOIN"
                if self.registered
                    && m.params
                        .first()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&self.source.channel))
                    && self.own_sender(m) =>
            {
                self.joined = true;
                Ok(Event::Joined)
            }
            "KICK"
                if m.params.len() >= 2
                    && m.params[0].eq_ignore_ascii_case(&self.source.channel)
                    && m.params[1].eq_ignore_ascii_case(&self.source.nickname) =>
            {
                Err("IRC: channel membership ended".into())
            }
            "PART"
                if self.own_sender(m)
                    && m.params
                        .first()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&self.source.channel)) =>
            {
                Err("IRC: channel membership ended".into())
            }
            "ERROR" | "432" | "433" | "451" | "464" | "465" | "471" | "473" | "474" | "475" => {
                Err("IRC: registration or channel rejected".into())
            }
            "NOTICE" | "PRIVMSG"
                if self.joined
                    && m.params.len() == 2
                    && m.params[0].eq_ignore_ascii_case(&self.source.channel)
                    && m.prefix.as_deref() == Some(self.source.sender.as_str()) =>
            {
                Ok(self
                    .source
                    .wire_payload(&m.params[1])?
                    .map_or(Event::Ignore, Event::Announcement))
            }
            _ => Ok(Event::Ignore),
        }
    }
    fn own_sender(&self, m: &Message) -> bool {
        m.prefix
            .as_deref()
            .and_then(|p| p.split('!').next())
            .is_some_and(|p| p.eq_ignore_ascii_case(&self.source.nickname))
    }
}
