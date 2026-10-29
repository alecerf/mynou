//! Network access. On loopback, Mynou answers this machine without sign-in.
//! Listening on another address also needs the owner's password: clients on
//! the network sign in once and keep a signed session cookie. The owner may
//! instead open Mynou to the local network: computers at a private address
//! then need no sign-in, and any other still does. Only a PBKDF2
//! hash of the password and the random key that signs sessions are stored,
//! and changing the password replaces the key, which ends every session.
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
/// Failed sign-ins allowed per address, and in all, in each window.
const WINDOW: Duration = Duration::from_secs(15 * 60);
const FAILURES_PER_PEER: u32 = 5;
const FAILURES_IN_ALL: u32 = 30;
const MAX_PEERS: usize = 256;
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

/// The stored password hash and the key that signs sessions.
#[derive(Clone)]
pub struct Credentials {
    iterations: u32,
    salt: [u8; 16],
    hash: [u8; 32],
    key: [u8; 32],
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
        })
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

    fn valid_session(&self, value: &str, now: u64) -> bool {
        let Some((claim, signature)) = value.rsplit_once('.') else {
            return false;
        };
        let Some((expiry, _)) = claim.split_once('.') else {
            return false;
        };
        expiry.parse::<u64>().is_ok_and(|expiry| expiry > now)
            && constant_time_eq(signature.as_bytes(), self.signature(claim).as_bytes())
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
    all: Option<(Instant, u32)>,
}

impl Failures {
    fn retry_after(&mut self, peer: IpAddr, now: Instant) -> Option<Duration> {
        self.by_peer.retain(|_, (start, _)| now - *start < WINDOW);
        if self.all.is_some_and(|(start, _)| now - start >= WINDOW) {
            self.all = None;
        }
        [self.by_peer.get(&peer).copied(), self.all]
            .into_iter()
            .zip([FAILURES_PER_PEER, FAILURES_IN_ALL])
            .filter_map(|(window, limit)| window.filter(|(_, n)| *n >= limit))
            .map(|(start, _)| WINDOW.saturating_sub(now - start))
            .max()
    }

    fn fail(&mut self, peer: IpAddr, now: Instant) {
        let all = self.all.get_or_insert((now, 0));
        all.1 += 1;
        if self.by_peer.len() < MAX_PEERS || self.by_peer.contains_key(&peer) {
            self.by_peer.entry(peer).or_insert((now, 0)).1 += 1;
        }
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
            failures.by_peer.remove(&peer);
            return credentials.session(now()).map(SignIn::Session);
        }
        failures.fail(peer, Instant::now());
        Ok(SignIn::Wrong)
    }

    /// Whether the request carries a valid session cookie.
    pub fn signed_in(&self, cookie_header: Option<&str>) -> bool {
        let Some(value) = cookie_header.and_then(|header| {
            header.split(';').find_map(|pair| {
                pair.trim()
                    .strip_prefix(COOKIE)
                    .and_then(|rest| rest.strip_prefix('='))
            })
        }) else {
            return false;
        };
        self.credentials
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|credentials| credentials.valid_session(value, now()))
    }
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
        access.replace(Some(self::credentials()));
        assert!(!access.signed_in(Some(&format!("{COOKIE}={cookie}"))));
        access.replace(None);
        assert!(matches!(
            access.sign_in(IpAddr::from([192, 0, 2, 10]), "correct horse battery"),
            Ok(SignIn::Unset)
        ));
    }

    #[test]
    fn failed_sign_ins_are_limited_per_address_and_in_all() {
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
        for n in 0..FAILURES_IN_ALL {
            failures.fail(IpAddr::from([198, 51, 100, n as u8]), now);
        }
        assert!(
            failures
                .retry_after(IpAddr::from([203, 0, 113, 1]), now)
                .is_some()
        );
        assert!(failures.retry_after(peer, now + WINDOW).is_none());
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
