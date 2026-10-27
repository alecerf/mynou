//! Offline parser for the first supported WireGuard profile subset.
//! Parsing is pure; optional private-file loading performs bounded read-only I/O.
//! No host resolution, sockets, persistence or protection activation.

use crate::{
    netstack::{Ipv4Config, MAX_MTU, MAX_RESOLVERS, MIN_MTU},
    tunnel::{Endpoint, TunnelConfig},
};
use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr, SocketAddrV4},
    time::Duration,
};

#[path = "profile_file.rs"]
mod file;
pub(crate) use file::read_private_text;
pub use file::{ImportError, read_private_file};

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
    /// IPv6 settings of the profile, kept for its private document: the tunnel
    /// itself carries IPv4 only, and nothing falls back to the host's IPv6.
    ipv6: Ipv6Settings,
    mtu: usize,
    endpoint: SocketAddrV4,
    keepalive: Option<Duration>,
}

impl Profile {
    /// Inner DNS servers; the first is also the NAT-PMP forwarding gateway.
    pub fn dns(&self) -> &[Ipv4Addr] {
        &self.dns
    }

    /// Retained interface CIDR prefix; AllowedIPs independently supplies the route.
    pub fn address_prefix(&self) -> u8 {
        self.prefix
    }

    /// Private persistence representation. Contains keys; never log or return it.
    pub(crate) fn private_document(&self) -> String {
        fn key(bytes: &[u8]) -> String {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut text = String::new();
            for chunk in bytes.chunks(3) {
                let n = (u32::from(chunk[0]) << 16)
                    | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                    | u32::from(*chunk.get(2).unwrap_or(&0));
                for shift in [18, 12] {
                    text.push(ALPHABET[((n >> shift) & 63) as usize] as char);
                }
                text.push(if chunk.len() > 1 {
                    ALPHABET[((n >> 6) & 63) as usize] as char
                } else {
                    '='
                });
                text.push(if chunk.len() > 2 {
                    ALPHABET[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            text
        }
        let mut address = format!("{}/{}", self.ip.local(), self.prefix);
        if let Some((ip, prefix)) = self.ipv6.address {
            address.push_str(&format!(", {ip}/{prefix}"));
        }
        let dns = self
            .dns
            .iter()
            .map(ToString::to_string)
            .chain(self.ipv6.dns.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(", ");
        let route = if self.ipv6.route {
            "0.0.0.0/0, ::/0"
        } else {
            "0.0.0.0/0"
        };
        let mut text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = {address}\nDNS = {dns}\nMTU = {}\n[Peer]\nPublicKey = {}\nAllowedIPs = {route}\nEndpoint = {}\n",
            key(&self.secret),
            self.mtu,
            key(&self.public),
            self.endpoint
        );
        if let Some(psk) = self.psk {
            text.push_str(&format!("PresharedKey = {}\n", key(&psk)));
        }
        if let Some(interval) = self.keepalive {
            text.push_str(&format!("PersistentKeepalive = {}\n", interval.as_secs()));
        }
        text
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
        // Keep the tunnel's TCP bounds; only the profile's MTU changes.
        config.tcp.mtu = self.mtu;
        config.keepalive = self.keepalive;
        config.with_dns(self.dns)
    }
}

#[derive(Default)]
struct Ipv6Settings {
    address: Option<(Ipv6Addr, u8)>,
    dns: Vec<Ipv6Addr>,
    route: bool,
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

/// A unicast IPv6 address: not unspecified, loopback or multicast.
fn ipv6(value: &str) -> Option<Ipv6Addr> {
    let address: Ipv6Addr = value.parse().ok()?;
    (!address.is_unspecified() && !address.is_loopback() && !address.is_multicast())
        .then_some(address)
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
    // One IPv4 address, and at most one IPv6 address beside it.
    let mut ipv4_address = None;
    let mut ipv6_settings = Ipv6Settings::default();
    for value in required(1)?.1.split(',') {
        let (address, prefix) = value.trim().split_once('/').ok_or_else(|| invalid(1))?;
        let prefix = decimal(prefix).ok_or_else(|| invalid(1))?;
        if let Some(address) = ipv4(address) {
            if prefix > 32 || ipv4_address.replace((address, prefix as u8)).is_some() {
                return Err(invalid(1));
            }
        } else {
            let address = ipv6(address).ok_or_else(|| invalid(1))?;
            if prefix > 128
                || ipv6_settings
                    .address
                    .replace((address, prefix as u8))
                    .is_some()
            {
                return Err(invalid(1));
            }
        }
    }
    let (address, prefix) = ipv4_address.ok_or_else(|| invalid(1))?;
    let ip = Ipv4Config::new(address, Ipv4Addr::UNSPECIFIED, 0).map_err(|_| invalid(1))?;
    // IPv4 resolvers answer inside the tunnel; IPv6 ones are kept for later.
    let mut dns = Vec::new();
    for value in required(2)?.1.split(',') {
        if dns.len() + ipv6_settings.dns.len() == MAX_RESOLVERS {
            return Err(invalid(2));
        }
        let value = value.trim();
        if let Some(resolver) = ipv4(value) {
            if dns.contains(&resolver) {
                return Err(invalid(2));
            }
            dns.push(resolver);
        } else {
            let resolver = ipv6(value).ok_or_else(|| invalid(2))?;
            if ipv6_settings.dns.contains(&resolver) {
                return Err(invalid(2));
            }
            ipv6_settings.dns.push(resolver);
        }
    }
    if dns.is_empty() {
        return Err(invalid(2));
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
    // The full IPv4 route, optionally with the full IPv6 route.
    let mut routes: Vec<_> = required(6)?.1.split(',').map(str::trim).collect();
    routes.sort_unstable();
    ipv6_settings.route = match routes.as_slice() {
        ["0.0.0.0/0"] => false,
        ["0.0.0.0/0", "::/0"] => true,
        _ => return Err(invalid(6)),
    };
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
        ipv6: ipv6_settings,
        mtu,
        endpoint,
        keepalive,
    })
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;

/// Equivalent structured settings use the supported WireGuard directive names.
/// Every value is a single string; no raw value or unknown key enters errors.
pub fn from_settings(value: &crate::json::Value) -> Result<Profile, String> {
    let fields = value
        .as_object()
        .ok_or("WireGuard settings must be an object")?;
    if fields.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return Err("WireGuard settings contain an unsupported directive".into());
    }
    let mut text = String::from("[Interface]\n");
    for (index, field) in FIELDS.iter().enumerate() {
        if index == 4 {
            text.push_str("[Peer]\n");
        }
        if let Some(value) = fields.get(*field) {
            let value = value
                .as_str()
                .filter(|v| {
                    v.len() <= MAX_LINE_BYTES - 32
                        && v.bytes().all(|b| (b' '..=b'~').contains(&b))
                        && !v.contains('#')
                })
                .ok_or("Invalid WireGuard structured setting")?;
            text.push_str(&format!("{field} = {value}\n"));
        }
    }
    parse(&text).map_err(|error| error.to_string())
}
