//! Offline parser for the first supported WireGuard profile subset.
//! No file access, host resolution, sockets, persistence or protection activation.

use crate::{
    netstack::{Ipv4Config, MAX_MTU, MAX_RESOLVERS, MIN_MTU, TcpConfig},
    tunnel::{Endpoint, TunnelConfig},
};
use std::{fmt, net::Ipv4Addr, net::SocketAddrV4, time::Duration};

pub const MAX_PROFILE_BYTES: usize = 16_384;
pub const MAX_PROFILE_LINES: usize = 256;
pub const MAX_LINE_BYTES: usize = 1024;

/// Field names are static parser-owned strings; diagnostics never retain input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    ProfileLimit,
    LineLimit,
    Syntax,
    UnsupportedSection,
    DuplicateSection,
    UnsupportedDirective,
    MissingField(&'static str),
    DuplicateField(&'static str),
    InvalidValue(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileError {
    /// One-based line, or zero for a whole-profile or missing-field error.
    pub line: usize,
    pub kind: ErrorKind,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireGuard profile (line {}): ", self.line)?;
        match self.kind {
            ErrorKind::ProfileLimit => f.write_str("profile size or line count exceeds limit"),
            ErrorKind::LineLimit => f.write_str("line exceeds limit"),
            ErrorKind::Syntax => f.write_str("invalid syntax or non-ASCII/control character"),
            ErrorKind::UnsupportedSection => f.write_str("unsupported section or section order"),
            ErrorKind::DuplicateSection => {
                f.write_str("duplicate section; only one peer is supported")
            }
            ErrorKind::UnsupportedDirective => {
                f.write_str("unsupported directive; hooks and scripts are forbidden")
            }
            ErrorKind::MissingField(field) => write!(f, "missing {field}"),
            ErrorKind::DuplicateField(field) => write!(f, "duplicate {field}"),
            ErrorKind::InvalidValue(field) => write!(f, "invalid or unsupported {field}"),
        }
    }
}

impl std::error::Error for ProfileError {}

/// Owns private key/PSK material. Deliberately no Debug, Clone or serialization.
/// Safe Rust does not guarantee key memory zeroization after disposal.
pub struct Profile {
    secret: [u8; 32],
    public: [u8; 32],
    psk: Option<[u8; 32]>,
    ip: Ipv4Config,
    prefix: u8,
    dns: Vec<Ipv4Addr>,
    mtu: usize,
    endpoint: SocketAddrV4,
    keepalive: Option<Duration>,
}

impl Profile {
    /// Retained interface CIDR prefix; AllowedIPs independently supplies the route.
    pub fn address_prefix(&self) -> u8 {
        self.prefix
    }

    /// Only constructs configuration: does not start a driver or resolve names.
    pub fn into_tunnel_config(self) -> TunnelConfig {
        let ip = self.ip;
        let mut config = TunnelConfig::new(
            self.secret,
            self.public,
            Endpoint::Address(self.endpoint.into()),
            ip,
        );
        config.psk = self.psk;
        config.mtu = self.mtu;
        config.tcp = TcpConfig::new(ip, self.mtu);
        config.keepalive = self.keepalive;
        config.with_dns(self.dns)
    }
}

const FIELDS: [&str; 9] = [
    "PrivateKey",
    "Address",
    "DNS",
    "MTU",
    "PublicKey",
    "PresharedKey",
    "AllowedIPs",
    "Endpoint",
    "PersistentKeepalive",
];

fn error(line: usize, kind: ErrorKind) -> ProfileError {
    ProfileError { line, kind }
}

fn ipv4(value: &str) -> Option<Ipv4Addr> {
    let address: Ipv4Addr = value.parse().ok()?;
    let first = address.octets()[0];
    (first != 0 && first != 127 && first < 224).then_some(address)
}

/// Canonical standard base64, exactly 32 bytes (one final '='; no ignored bytes).
fn key(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 44 || bytes[43] != b'=' {
        return None;
    }
    let digit = |byte| match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = [0; 32];
    for i in 0..10 {
        let offset = 4 * i;
        let a = digit(bytes[offset])?;
        let b = digit(bytes[offset + 1])?;
        let c = digit(bytes[offset + 2])?;
        let d = digit(bytes[offset + 3])?;
        out[i * 3] = a << 2 | b >> 4;
        out[i * 3 + 1] = b << 4 | c >> 2;
        out[i * 3 + 2] = c << 6 | d;
    }
    let a = digit(bytes[40])?;
    let b = digit(bytes[41])?;
    let c = digit(bytes[42])?;
    if c & 3 != 0 {
        return None;
    }
    out[30] = a << 2 | b >> 4;
    out[31] = b << 4 | c >> 2;
    Some(out)
}

/// Decimal values have no sign, whitespace, separators or implicit bases.
fn decimal(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Exactly one `[Interface]` followed by one `[Peer]`. Comments start with '#'.
/// LF/CRLF and horizontal ASCII whitespace are accepted; values are not quoted.
pub fn parse(input: &str) -> Result<Profile, ProfileError> {
    if input.len() > MAX_PROFILE_BYTES {
        return Err(error(0, ErrorKind::ProfileLimit));
    }
    let mut section = 0;
    let mut fields = [None; 9];
    for (index, raw) in input.split_inclusive('\n').enumerate() {
        let line = index + 1;
        if line > MAX_PROFILE_LINES {
            return Err(error(0, ErrorKind::ProfileLimit));
        }
        let raw = match raw.strip_suffix('\n') {
            Some(line) => line.strip_suffix('\r').unwrap_or(line),
            None => raw,
        };
        if raw.len() > MAX_LINE_BYTES {
            return Err(error(line, ErrorKind::LineLimit));
        }
        if !raw
            .bytes()
            .all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
        {
            return Err(error(line, ErrorKind::Syntax));
        }
        let text = raw.split('#').next().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with('[') {
            section = match (text, section) {
                ("[Interface]", 0) => 1,
                ("[Peer]", 1) => 2,
                ("[Interface]", 1 | 2) | ("[Peer]", 2) => {
                    return Err(error(line, ErrorKind::DuplicateSection));
                }
                _ => return Err(error(line, ErrorKind::UnsupportedSection)),
            };
            continue;
        }
        let (name, value) = text.split_once('=').ok_or(error(line, ErrorKind::Syntax))?;
        let name = name.trim();
        let Some(field) = FIELDS.iter().position(|known| *known == name) else {
            return Err(error(line, ErrorKind::UnsupportedDirective));
        };
        if section == 0 || (field < 4) != (section == 1) {
            return Err(error(line, ErrorKind::UnsupportedDirective));
        }
        if fields[field].replace((line, value.trim())).is_some() {
            return Err(error(line, ErrorKind::DuplicateField(FIELDS[field])));
        }
    }
    let required =
        |index: usize| fields[index].ok_or(error(0, ErrorKind::MissingField(FIELDS[index])));
    let invalid = |index: usize| {
        error(
            fields[index].map_or(0, |v| v.0),
            ErrorKind::InvalidValue(FIELDS[index]),
        )
    };
    let secret = key(required(0)?.1)
        .filter(|key| *key != [0; 32])
        .ok_or_else(|| invalid(0))?;
    let (address, prefix) = required(1)?.1.split_once('/').ok_or_else(|| invalid(1))?;
    let address = ipv4(address).ok_or_else(|| invalid(1))?;
    let ip = Ipv4Config::new(address, Ipv4Addr::UNSPECIFIED, 0).map_err(|_| invalid(1))?;
    let prefix = decimal(prefix)
        .filter(|n| *n <= 32)
        .ok_or_else(|| invalid(1))? as u8;
    let mut dns = Vec::new();
    for value in required(2)?.1.split(',') {
        if dns.len() == MAX_RESOLVERS {
            return Err(invalid(2));
        }
        let resolver = ipv4(value.trim()).ok_or_else(|| invalid(2))?;
        if dns.contains(&resolver) {
            return Err(invalid(2));
        }
        dns.push(resolver);
    }
    let mtu = match fields[3] {
        Some((_, value)) => decimal(value)
            .filter(|n| (MIN_MTU as u32..=MAX_MTU as u32).contains(n))
            .ok_or_else(|| invalid(3))? as usize,
        None => MAX_MTU,
    };
    let public = key(required(4)?.1).ok_or_else(|| invalid(4))?;
    // Reject unusable low-order peer keys now, before a driver can be created.
    crate::crypto::x25519(secret, public).map_err(|_| invalid(4))?;
    if crate::crypto::constant_time_eq(&crate::crypto::x25519_public_key(secret), &public) {
        return Err(invalid(4));
    }
    let psk = fields[5]
        .map(|(_, value)| key(value).ok_or_else(|| invalid(5)))
        .transpose()?;
    if required(6)?.1 != "0.0.0.0/0" {
        return Err(invalid(6));
    }
    let endpoint: SocketAddrV4 = required(7)?.1.parse().map_err(|_| invalid(7))?;
    if endpoint.port() == 0 || ipv4(&endpoint.ip().to_string()).is_none() {
        return Err(invalid(7));
    }
    let keepalive = match fields[8] {
        Some((_, value)) => match decimal(value)
            .filter(|n| *n <= 65535)
            .ok_or_else(|| invalid(8))?
        {
            0 => None,
            seconds => Some(Duration::from_secs(u64::from(seconds))),
        },
        None => None,
    };
    Ok(Profile {
        secret,
        public,
        psk,
        ip,
        prefix,
        dns,
        mtu,
        endpoint,
        keepalive,
    })
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
