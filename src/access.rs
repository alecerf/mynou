//! Network access. On loopback, Mynou answers this machine without sign-in.
//! Listening on another address also needs the owner's password: clients on
//! the network sign in once and keep a signed session cookie. The owner may
//! instead open Mynou to the local network: computers at a private address
//! then need no sign-in, and any other still does. Only a PBKDF2
//! hash of the password, the random key that signs sessions and the sessions
//! signed out before they expired are stored; changing the password replaces
//! the key, which ends every session.
//!
//! Every request, signed in or not, must name an allowed host, so a domain
//! rebound to this machine is refused, and a page of another site cannot
//! send it an `Origin` of its own.
use crate::{
    Result,
    crypto::{constant_time_eq, hmac_sha256, pbkdf2_sha256, random_bytes},
    json::Value,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr, UdpSocket},
    path::Path,
    sync::{Mutex, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const COOKIE: &str = "mynou_session";
pub const SESSION_SECS: u64 = 30 * 86_400;
const ITERATIONS: u32 = 600_000;
const MAX_ITERATIONS: u32 = 10_000_000;
const MIN_PASSWORD_CHARS: usize = 10;
const MAX_PASSWORD_BYTES: usize = 1024;
/// Failed sign-ins allowed per address in each window. There is no limit in
/// all, so that another machine cannot lock the owner out; an IPv6 /64 counts
/// as one address, so that a machine cannot rotate around the limit.
const WINDOW: Duration = Duration::from_secs(15 * 60);
const FAILURES_PER_PEER: u32 = 5;
/// Addresses remembered at once; beyond them, new addresses wait.
const MAX_PEERS: usize = 1024;
/// Signed-out sessions remembered until they expire. One more sign-out
/// renews the session key instead, which ends every session.
const MAX_REVOKED: usize = 256;
pub const MAX_ALLOWED_HOSTS: usize = 32;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; N];
    for (n, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * n..2 * n + 2], 16).ok()?;
    }
    Some(out)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn check_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS
        || password.len() > MAX_PASSWORD_BYTES
        || password.chars().any(char::is_control)
    {
        return Err(format!(
            "The password needs at least {MIN_PASSWORD_CHARS} characters, at most {MAX_PASSWORD_BYTES} bytes and no control characters"
        ));
    }
    Ok(())
}

/// The stored password hash, the key that signs sessions and the nonces of
/// sessions signed out before their expiry.
#[derive(Clone)]
pub struct Credentials {
    iterations: u32,
    salt: [u8; 16],
    hash: [u8; 32],
    key: [u8; 32],
    revoked: BTreeMap<[u8; 16], u64>,
}

impl Credentials {
    pub fn new(password: &str) -> Result<Self> {
        Self::with_iterations(password, ITERATIONS)
    }

    pub(crate) fn with_iterations(password: &str, iterations: u32) -> Result<Self> {
        check_password(password)?;
        let salt = random_bytes()?;
        Ok(Self {
            iterations,
            salt,
            hash: pbkdf2_sha256(password.as_bytes(), &salt, iterations),
            key: random_bytes()?,
            revoked: BTreeMap::new(),
        })
    }

    /// The same password with a new session key, which ends every session.
    pub fn renewed(&self) -> Result<Self> {
        Ok(Self {
            key: random_bytes()?,
            revoked: BTreeMap::new(),
            ..self.clone()
        })
    }

    /// These credentials with the session `value` signed out, or `None` when
    /// it is not a valid session. Expired nonces are forgotten; when
    /// `MAX_REVOKED` are still remembered, the key is renewed instead.
    pub fn signed_out(&self, value: &str, now: u64) -> Result<Option<Self>> {
        let Some((nonce, expiry)) = self.session_nonce(value, now) else {
            return Ok(None);
        };
        let mut next = self.clone();
        next.revoked.retain(|_, until| *until > now);
        if next.revoked.len() >= MAX_REVOKED {
            return self.renewed().map(Some);
        }
        next.revoked.insert(nonce, expiry);
        Ok(Some(next))
    }

    pub fn verify(&self, password: &str) -> bool {
        password.len() <= MAX_PASSWORD_BYTES
            && constant_time_eq(
                &pbkdf2_sha256(password.as_bytes(), &self.salt, self.iterations),
                &self.hash,
            )
    }

    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert(
            "password",
            format!(
                "pbkdf2-sha256${}${}${}",
                self.iterations,
                hex(&self.salt),
                hex(&self.hash)
            ),
        );
        value.insert("session_key", hex(&self.key));
        if !self.revoked.is_empty() {
            value.insert(
                "signed_out",
                Value::Array(
                    self.revoked
                        .iter()
                        .map(|(nonce, expiry)| Value::String(format!("{expiry}.{}", hex(nonce))))
                        .collect(),
                ),
            );
        }
        value
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let invalid = || "The stored network access password is invalid".to_owned();
        let password = value.get("password").and_then(Value::as_str);
        let mut parts = password.ok_or_else(invalid)?.split('$');
        let (Some("pbkdf2-sha256"), Some(iterations), Some(salt), Some(hash), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(invalid());
        };
        let iterations = iterations
            .parse()
            .ok()
            .filter(|n| (1..=MAX_ITERATIONS).contains(n))
            .ok_or_else(invalid)?;
        Ok(Self {
            iterations,
            salt: unhex(salt).ok_or_else(invalid)?,
            hash: unhex(hash).ok_or_else(invalid)?,
            key: value
                .get("session_key")
                .and_then(Value::as_str)
                .and_then(unhex)
                .ok_or_else(invalid)?,
            revoked: match value.get("signed_out") {
                None => BTreeMap::new(),
                Some(Value::Array(items)) if items.len() <= MAX_REVOKED => items
                    .iter()
                    .map(|item| {
                        let (expiry, nonce) = item.as_str()?.split_once('.')?;
                        Some((unhex(nonce)?, expiry.parse().ok()?))
                    })
                    .collect::<Option<_>>()
                    .ok_or_else(invalid)?,
                Some(_) => return Err(invalid()),
            },
        })
    }

    fn signature(&self, claim: &str) -> String {
        hex(&hmac_sha256(
            &self.key,
            format!("mynou-session:{claim}").as_bytes(),
        ))
    }

    /// A new session cookie value: expiry, nonce and their signature.
    fn session(&self, now: u64) -> Result<String> {
        let claim = format!("{}.{}", now + SESSION_SECS, hex(&random_bytes::<16>()?));
        Ok(format!("{claim}.{}", self.signature(&claim)))
    }

    /// The nonce and expiry of a session this key signed and that has not
    /// expired, signed out or not.
    fn session_nonce(&self, value: &str, now: u64) -> Option<([u8; 16], u64)> {
        let (claim, signature) = value.rsplit_once('.')?;
        let (expiry, nonce) = claim.split_once('.')?;
        let expiry = expiry.parse::<u64>().ok().filter(|expiry| *expiry > now)?;
        constant_time_eq(signature.as_bytes(), self.signature(claim).as_bytes()).then_some(())?;
        Some((unhex(nonce)?, expiry))
    }

    fn valid_session(&self, value: &str, now: u64) -> bool {
        self.session_nonce(value, now)
            .is_some_and(|(nonce, _)| !self.revoked.contains_key(&nonce))
    }
}

/// The credentials saved in a stopped installation's store, if any.
pub fn stored(store_dir: &Path) -> Result<Option<Credentials>> {
    if !crate::store::Store::directory_exists(store_dir)? {
        return Ok(None);
    }
    crate::store::Store::open_read_only(store_dir)?.access()
}

/// Refuses a listener other machines can reach until a password is set or
/// the owner opened the local network.
pub fn require(listen: &str, password_set: bool, local_network_open: bool) -> Result<()> {
    if !password_set && !local_network_open && !loopback(listen) {
        return Err(format!(
            "Mynou does not listen on {listen} without a password: the API would be open to the network. Set a network access password in Settings, Network, from this Mac, open Mynou to the local network, or listen on 127.0.0.1"
        ));
    }
    Ok(())
}

/// Whether a peer is on the local network: a private or link-local address,
/// which the internet cannot reach this machine from. A router forwarding a
/// port keeps the public address of whoever connects.
pub fn local_network(peer: IpAddr) -> bool {
    match peer.to_canonical() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        // Unique local fc00::/7 and link-local fe80::/10.
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    }
}

/// Whether a listen address is loopback only (anything unparsable counts as
/// exposed).
pub fn loopback(listen: &str) -> bool {
    listen
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().to_canonical().is_loopback())
}

pub enum SignIn {
    Session(String),
    Wrong,
    Throttled(Duration),
    Unset,
}

#[derive(Default)]
struct Failures {
    by_peer: BTreeMap<IpAddr, (Instant, u32)>,
}

/// The address failures count against: an IPv6 address by its /64.
fn counted(peer: IpAddr) -> IpAddr {
    match peer.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !u128::from(u64::MAX)).into()),
        v4 => v4,
    }
}

impl Failures {
    fn retry_after(&mut self, peer: IpAddr, now: Instant) -> Option<Duration> {
        self.by_peer.retain(|_, (start, _)| now - *start < WINDOW);
        let peer = counted(peer);
        match self.by_peer.get(&peer) {
            Some((start, n)) if *n >= FAILURES_PER_PEER => {
                Some(WINDOW.saturating_sub(now - *start))
            }
            None if self.by_peer.len() >= MAX_PEERS => self
                .by_peer
                .values()
                .map(|(start, _)| WINDOW.saturating_sub(now - *start))
                .min(),
            _ => None,
        }
    }

    fn fail(&mut self, peer: IpAddr, now: Instant) {
        self.by_peer.entry(counted(peer)).or_insert((now, 0)).1 += 1;
    }

    fn forget(&mut self, peer: IpAddr) {
        self.by_peer.remove(&counted(peer));
    }
}

/// The credentials in force, shared by the startup and full listeners, and
/// the record of failed sign-ins.
#[derive(Default)]
pub struct Access {
    credentials: RwLock<Option<Credentials>>,
    failures: Mutex<Failures>,
}

impl Access {
    pub fn new(credentials: Option<Credentials>) -> Self {
        Self {
            credentials: RwLock::new(credentials),
            failures: Mutex::default(),
        }
    }

    pub fn is_set(&self) -> bool {
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn current(&self) -> Option<Credentials> {
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn replace(&self, credentials: Option<Credentials>) {
        *self.credentials.write().unwrap_or_else(|e| e.into_inner()) = credentials;
    }

    /// Checks a password from `peer`, within the failure limits.
    pub fn sign_in(&self, peer: IpAddr, password: &str) -> Result<SignIn> {
        let peer = peer.to_canonical();
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(wait) = failures.retry_after(peer, Instant::now()) {
            return Ok(SignIn::Throttled(wait));
        }
        let Some(credentials) = self
            .credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return Ok(SignIn::Unset);
        };
        // Attempts are checked one at a time, which also bounds their cost.
        if credentials.verify(password) {
            failures.forget(peer);
            return credentials.session(now()).map(SignIn::Session);
        }
        failures.fail(peer, Instant::now());
        Ok(SignIn::Wrong)
    }

    /// Whether the request carries a valid session cookie.
    pub fn signed_in(&self, cookie_header: Option<&str>) -> bool {
        let Some(value) = session_cookie(cookie_header) else {
            return false;
        };
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|credentials| credentials.valid_session(value, now()))
    }

    /// The credentials with the request's session signed out, when it carries
    /// a valid one. The caller saves them, then puts them in force with
    /// `replace`.
    pub fn sign_out(&self, cookie_header: Option<&str>) -> Result<Option<Credentials>> {
        let (Some(value), Some(credentials)) = (session_cookie(cookie_header), self.current())
        else {
            return Ok(None);
        };
        credentials.signed_out(value, now())
    }
}

/// The value of the session cookie in a `Cookie` header.
fn session_cookie(cookie_header: Option<&str>) -> Option<&str> {
    cookie_header?.split(';').find_map(|pair| {
        pair.trim()
            .strip_prefix(COOKIE)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

/// Whether a proxy forwarded the request. Its peer is then the proxy, not
/// the client, so neither locality nor the local network can be trusted.
pub fn proxied(headers: &BTreeMap<String, String>) -> bool {
    [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-real-ip",
        "via",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

/// Whether a connection comes from this machine: from loopback, or from the
/// very address it reached, which a remote peer cannot complete a TCP
/// handshake from.
pub fn local_peer(peer: SocketAddr, local: SocketAddr) -> bool {
    let peer = peer.ip().to_canonical();
    peer.is_loopback() || peer == local.ip().to_canonical()
}

/// The name of a `Host` header, without its port.
fn host_name(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
}

/// Whether `Host` may address this listener. Loopback names always may. A
/// listener reachable from the network also takes its own address, any
/// address of this machine when it listens on all of them, this Mac's
/// `NAME.local` and the names the owner allowed.
pub fn allowed_host(host: &str, listener: SocketAddr, allowed: &[String]) -> bool {
    let name = host_name(host);
    if matches!(name, "127.0.0.1" | "[::1]") || name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let listening = listener.ip().to_canonical();
    if listening.is_loopback() || name.is_empty() {
        return false;
    }
    let literal = match name.strip_prefix('[') {
        Some(rest) => match rest.strip_suffix(']').map(str::parse::<std::net::Ipv6Addr>) {
            Some(Ok(ip)) => Some(IpAddr::V6(ip)),
            _ => return false,
        },
        None => name.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4),
    };
    if let Some(ip) = literal {
        let ip = ip.to_canonical();
        return !ip.is_unspecified()
            && (ip == listening || listening.is_unspecified() && local_address(ip));
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    local_name().is_some_and(|local| name.eq_ignore_ascii_case(&format!("{local}.local")))
        || allowed
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// Whether `ip` is assigned to this machine: only then can a socket bind it.
fn local_address(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && UdpSocket::bind((ip, 0)).is_ok()
}

/// The address this machine reaches its network from. Connecting a UDP
/// socket sends nothing; it only picks the route.
pub fn network_address() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

const PREFERENCES: &str = "/Library/Preferences/SystemConfiguration/preferences.plist";
static LOCAL_NAME: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

/// This Mac's Bonjour name (`NAME` in `NAME.local`), read from the system
/// preferences at most once a minute.
pub fn local_name() -> Option<String> {
    let mut cached = LOCAL_NAME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((read, name)) = cached.as_ref()
        && read.elapsed() < Duration::from_secs(60)
    {
        return name.clone();
    }
    let name = std::fs::read_to_string(PREFERENCES)
        .ok()
        .and_then(|text| bonjour_name(&text));
    *cached = Some((Instant::now(), name.clone()));
    name
}

fn bonjour_name(plist: &str) -> Option<String> {
    let (_, rest) = plist.split_once("<key>LocalHostName</key>")?;
    let rest = rest.trim_start().strip_prefix("<string>")?;
    let (name, _) = rest.split_once("</string>")?;
    (!name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then(|| name.to_owned())
}

/// Validates one owner-allowed host name: a DNS name or an IP literal, IPv6
/// in brackets, without a port.
pub fn check_allowed_host(name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase();
    let ip = name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok())
        || name.parse::<std::net::Ipv4Addr>().is_ok();
    let dns = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !(ip || dns) {
        return Err(format!(
            "Configuration: access.allowed_hosts takes host names or IP addresses without a port, not {name:?}"
        ));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials::with_iterations("correct horse battery", 2).unwrap()
    }

    #[test]
    fn only_the_password_hash_and_session_key_are_stored() {
        let saved = credentials();
        let json = crate::json::stringify(&saved.to_json());
        assert!(!json.contains("correct horse"), "{json}");
        let loaded = Credentials::from_json(&saved.to_json()).unwrap();
        assert!(loaded.verify("correct horse battery"));
        assert!(!loaded.verify("correct horse batterY"));
        assert!(!loaded.verify(""));
        let mut tampered = saved.to_json();
        tampered.insert("password", "pbkdf2-sha256$0$00$00");
        assert!(Credentials::from_json(&tampered).is_err());
        assert!(Credentials::with_iterations("too short", 2).is_err());
        assert!(Credentials::with_iterations("ten chars\n!", 2).is_err());
    }

    #[test]
    fn sessions_are_signed_expire_and_end_with_a_new_password() {
        let credentials = credentials();
        let session = credentials.session(1000).unwrap();
        assert!(credentials.valid_session(&session, 1000));
        assert!(!credentials.valid_session(&session, 1000 + SESSION_SECS));
        let (claim, _) = session.rsplit_once('.').unwrap();
        let later = claim.replacen(&(1000 + SESSION_SECS).to_string(), "99999999999", 1);
        assert!(!credentials.valid_session(&format!("{later}.{}", "0".repeat(64)), 1000));
        assert!(!credentials.valid_session("", 1000));
        assert!(!credentials.valid_session("1.2", 1000));
        assert!(!self::credentials().valid_session(&session, 1000));

        let access = Access::new(Some(credentials.clone()));
        let SignIn::Session(cookie) = access
            .sign_in(IpAddr::from([192, 0, 2, 10]), "correct horse battery")
            .unwrap()
        else {
            panic!("the password was refused");
        };
        assert!(access.signed_in(Some(&format!("theme=dark; {COOKIE}={cookie}"))));
        assert!(!access.signed_in(Some(&format!("x{COOKIE}={cookie}"))));
        assert!(!access.signed_in(None));
        // Signing out ends this session only, and survives a reload.
        let SignIn::Session(other) = access
            .sign_in(IpAddr::from([192, 0, 2, 11]), "correct horse battery")
            .unwrap()
        else {
            panic!("the password was refused");
        };
        let header = format!("{COOKIE}={other}");
        let next = access.sign_out(Some(&header)).unwrap().unwrap();
        let next = Credentials::from_json(&next.to_json()).unwrap();
        access.replace(Some(next));
        assert!(!access.signed_in(Some(&header)));
        assert!(access.sign_out(Some(&header)).unwrap().is_some());
        assert!(access.sign_out(Some("other=1")).unwrap().is_none());
        assert!(access.signed_in(Some(&format!("{COOKIE}={cookie}"))));
        // Past the limit, a sign-out renews the key and ends every session.
        let mut full = access.current().unwrap();
        for n in 0..MAX_REVOKED {
            full.revoked.insert([n as u8; 16], u64::MAX);
        }
        let renewed = full.signed_out(&cookie, now()).unwrap().unwrap();
        assert!(renewed.revoked.is_empty() && renewed.key != full.key);
        // Signing out everywhere keeps the password and ends the session.
        let renewed = access.current().unwrap().renewed().unwrap();
        assert!(renewed.verify("correct horse battery"));
        access.replace(Some(renewed));
        assert!(!access.signed_in(Some(&format!("{COOKIE}={cookie}"))));
        access.replace(None);
        assert!(matches!(
            access.sign_in(IpAddr::from([192, 0, 2, 10]), "correct horse battery"),
            Ok(SignIn::Unset)
        ));
    }

    #[test]
    fn failed_sign_ins_are_limited_per_address_only() {
        let access = Access::new(Some(credentials()));
        let peer = IpAddr::from([192, 0, 2, 10]);
        for _ in 0..FAILURES_PER_PEER {
            assert!(matches!(
                access.sign_in(peer, "wrong password"),
                Ok(SignIn::Wrong)
            ));
        }
        // Even the right password waits once the limit is reached.
        assert!(matches!(
            access.sign_in(peer, "correct horse battery"),
            Ok(SignIn::Throttled(wait)) if wait > Duration::from_secs(60)
        ));
        let other = IpAddr::from([192, 0, 2, 11]);
        assert!(matches!(
            access.sign_in(other, "correct horse battery"),
            Ok(SignIn::Session(_))
        ));
        let mut failures = Failures::default();
        let now = Instant::now();
        // Many failing addresses do not lock out another one.
        for n in 0..=250 {
            for _ in 0..FAILURES_PER_PEER {
                failures.fail(IpAddr::from([198, 51, 100, n]), now);
            }
        }
        let owner = IpAddr::from([203, 0, 113, 1]);
        assert!(failures.retry_after(owner, now).is_none());
        // An IPv6 machine cannot rotate addresses within its /64.
        for n in 0..FAILURES_PER_PEER {
            let address = std::net::Ipv6Addr::new(0xfd00, 0, 0, 7, 0, 0, 0, n as u16 + 1);
            assert!(failures.retry_after(address.into(), now).is_none());
            failures.fail(address.into(), now);
        }
        let rotated = std::net::Ipv6Addr::new(0xfd00, 0, 0, 7, 0xa, 0xb, 0xc, 0xd);
        assert!(failures.retry_after(rotated.into(), now).is_some());
        let neighbour = std::net::Ipv6Addr::new(0xfd00, 0, 0, 8, 0, 0, 0, 1);
        assert!(failures.retry_after(neighbour.into(), now).is_none());
        // Once the table is full, only remembered addresses sign in.
        for n in 0..MAX_PEERS as u32 {
            failures.fail(IpAddr::from((10 << 24 | n).to_be_bytes()), now);
        }
        assert!(failures.retry_after(owner, now).is_some());
        assert!(failures.retry_after(owner, now + WINDOW).is_none());
    }

    #[test]
    fn hosts_are_loopback_names_or_names_of_an_exposed_listener() {
        let loopback: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let everywhere: SocketAddr = "0.0.0.0:80".parse().unwrap();
        let lan: SocketAddr = "192.0.2.10:8787".parse().unwrap();
        let allowed = vec!["mynou.example".to_owned()];
        for host in ["127.0.0.1:8787", "localhost", "LOCALHOST:80", "[::1]:8787"] {
            assert!(allowed_host(host, loopback, &[]), "{host}");
            assert!(allowed_host(host, everywhere, &allowed), "{host}");
        }
        for host in ["192.0.2.10:8787", "mynou.example", "evil.example"] {
            assert!(!allowed_host(host, loopback, &allowed), "{host}");
        }
        assert!(allowed_host("192.0.2.10:8787", lan, &[]));
        assert!(allowed_host("Mynou.Example.:8787", lan, &allowed));
        for host in [
            "",
            ":80",
            "evil.example",
            "192.0.2.11:8787",
            "mynou.example.evil.example",
            "[::1",
            "[192.0.2.10]",
            "192.0.2.010",
        ] {
            assert!(!allowed_host(host, lan, &allowed), "{host}");
        }
        // On every address, any address of this machine, never another's.
        assert!(
            allowed_host("127.0.0.2", everywhere, &[])
                == local_address("127.0.0.2".parse().unwrap())
        );
        assert!(!allowed_host("192.0.2.77", everywhere, &[]));
        assert!(!allowed_host("0.0.0.0", everywhere, &[]));
        if let Some(ip) = network_address() {
            let host = match ip {
                IpAddr::V4(ip) => ip.to_string(),
                IpAddr::V6(ip) => format!("[{ip}]"),
            };
            assert!(allowed_host(&host, everywhere, &[]), "{host}");
        }
    }

    #[test]
    fn a_peer_is_local_from_loopback_or_from_the_address_it_reached() {
        let local: SocketAddr = "192.0.2.10:80".parse().unwrap();
        for peer in [
            "127.0.0.1:5000",
            "[::1]:5000",
            "[::ffff:127.0.0.1]:5000",
            "192.0.2.10:5000",
            "[::ffff:192.0.2.10]:5000",
        ] {
            assert!(local_peer(peer.parse().unwrap(), local), "{peer}");
        }
        assert!(!local_peer("192.0.2.11:5000".parse().unwrap(), local));
    }

    #[test]
    fn only_private_and_link_local_peers_are_on_the_local_network() {
        for peer in [
            "10.0.0.5",
            "172.16.3.4",
            "172.31.255.1",
            "192.168.1.20",
            "169.254.9.9",
            "::ffff:192.168.1.20",
            "fd12:3456::1",
            "fe80::1",
            "127.0.0.1",
        ] {
            assert!(local_network(peer.parse().unwrap()), "{peer}");
        }
        for peer in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!local_network(peer.parse().unwrap()), "{peer}");
        }
    }

    #[test]
    fn listeners_and_names() {
        assert!(require("127.0.0.1:8787", false, false).is_ok());
        assert!(require("[::1]:8787", false, false).is_ok());
        assert!(require("0.0.0.0:80", false, false).is_err());
        assert!(require("[::]:80", false, false).is_err());
        assert!(require("0.0.0.0:80", true, false).is_ok());
        assert!(require("0.0.0.0:80", false, true).is_ok());
        assert_eq!(
            bonjour_name("<dict>\n\t<key>LocalHostName</key>\n\t<string>Studio-Mac</string>"),
            Some("Studio-Mac".into())
        );
        assert_eq!(
            bonjour_name("<key>LocalHostName</key><string>a b</string>"),
            None
        );
        assert_eq!(check_allowed_host(" NAS.Local ").unwrap(), "nas.local");
        assert_eq!(check_allowed_host("[fe80::1]").unwrap(), "[fe80::1]");
        for bad in ["", "nas.local:80", "-nas", "a..b", "http://nas", "[::1"] {
            assert!(check_allowed_host(bad).is_err(), "{bad}");
        }
    }
}
