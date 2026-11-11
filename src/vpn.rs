//! Private operator intent and immutable, revocable engine generations.
//! This control plane does not establish the isolation evidence of issue #224.
use crate::{
    Result,
    json::Value,
    net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream},
    torrent::transport::{Network, NetworkListener, Publication},
    tunnel::{MAX_PROFILES, Stats, Switch, TunnelState},
    tunnel_http::{HttpTunnel, Report},
    wireguard::profile::{self, Profile},
};
use forwarding::{Forwarder, Status as Forwarding};
use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const UNAVAILABLE: &str = "Torrent protection is unavailable or revoked";
const STORAGE: &str = "Cannot persist private VPN intent; torrent networking remains blocked";
const DISABLED: &str = "# mynou-vpn-v1 disabled\n";
const ENABLED: &str = "# mynou-vpn-v1 enabled\n";
const REVOKED: &str = "# mynou-vpn-v1 revoked\n";
/// Opt-in NAT-PMP forwarding to the profile's DNS gateway (owner decision, #223).
const FORWARDING: &str = "# mynou-forwarding natpmp\n";
/// Starts each profile of a list, with its name; a lone unnamed profile is
/// stored without it, as before lists existed.
const PROFILE: &str = "# mynou-profile";
const NAME_CHARS: usize = 64;
/// Retries on one profile of a list before the next one takes over: a few
/// minutes when its server stops answering.
const FAILOVER_ATTEMPTS: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Disabled,
    Enabled,
    Revoked,
}
/// One imported WireGuard profile, under the name it was imported with.
struct Entry {
    name: Option<String>,
    profile: Profile,
}
struct Intent {
    mode: Mode,
    forwarding: bool,
    /// In failover order: the first is tried first.
    profiles: Vec<Entry>,
}
/// A shown profile name: trimmed, without control characters, bounded.
fn profile_name(text: &str) -> Option<String> {
    let name: String = text
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_CHARS)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}
/// Today's lone profile, or a list where each profile follows its
/// `# mynou-profile [name]` line.
fn entries(text: &str) -> Result<Vec<Entry>> {
    let parse = |text: &str| profile::parse(text).map_err(|e| e.to_string());
    if !text.starts_with(PROFILE) {
        return Ok(vec![Entry {
            name: None,
            profile: parse(text)?,
        }]);
    }
    let mut entries = Vec::new();
    let mut current: Option<(Option<String>, String)> = None;
    for line in text.split_inclusive('\n') {
        let Some(marker) = line.strip_prefix(PROFILE) else {
            if let Some((_, text)) = current.as_mut() {
                text.push_str(line);
            }
            continue;
        };
        let name = match marker.trim_end_matches(['\r', '\n']) {
            "" => None,
            named => Some(
                named
                    .strip_prefix(' ')
                    .and_then(profile_name)
                    .ok_or("Invalid private VPN intent")?,
            ),
        };
        if let Some((name, text)) = current.replace((name, String::new())) {
            entries.push(Entry {
                name,
                profile: parse(&text)?,
            });
        }
    }
    if let Some((name, text)) = current {
        entries.push(Entry {
            name,
            profile: parse(&text)?,
        });
    }
    Ok(entries)
}
impl Intent {
    fn decode(text: &str) -> Result<Self> {
        let (mode, rest) = if let Some(rest) = text.strip_prefix(DISABLED) {
            (Mode::Disabled, rest)
        } else if let Some(rest) = text.strip_prefix(ENABLED) {
            (Mode::Enabled, rest)
        } else if let Some(rest) = text.strip_prefix(REVOKED) {
            (Mode::Revoked, rest)
        } else {
            return Err("Invalid private VPN intent".into());
        };
        let (forwarding, rest) = match rest.strip_prefix(FORWARDING) {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let profiles = if rest.is_empty() && mode == Mode::Disabled {
            Vec::new()
        } else {
            entries(rest)?
        };
        if (forwarding && profiles.is_empty()) || profiles.len() > MAX_PROFILES {
            return Err("Invalid private VPN intent".into());
        }
        Ok(Self {
            mode,
            forwarding,
            profiles,
        })
    }
    fn document(&self) -> String {
        let mut text = match self.mode {
            Mode::Disabled => DISABLED,
            Mode::Enabled => ENABLED,
            Mode::Revoked => REVOKED,
        }
        .to_owned();
        if self.forwarding {
            text.push_str(FORWARDING);
        }
        match self.profiles.as_slice() {
            [
                Entry {
                    name: None,
                    profile,
                },
            ] => text.push_str(&profile.private_document()),
            entries => {
                for entry in entries {
                    text.push_str(PROFILE);
                    if let Some(name) = &entry.name {
                        text.push(' ');
                        text.push_str(name);
                    }
                    text.push('\n');
                    text.push_str(&entry.profile.private_document());
                }
            }
        }
        text
    }
    /// The name shown for each profile, in order.
    fn names(&self) -> Vec<String> {
        self.profiles
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                entry
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Profile {}", index + 1))
            })
            .collect()
    }
}

/// One permanently retired gate per engine. The network is assigned once;
/// reconnects construct another engine rather than repointing outstanding work.
pub(crate) struct Gate {
    inner: OnceLock<Arc<dyn Network>>,
    /// The profile the tunnel generation behind `inner` runs on.
    profile: OnceLock<usize>,
    /// This generation's forwarded-port lease, when forwarding is enabled.
    forwarder: OnceLock<Forwarder>,
    retired: AtomicBool,
    fence: RwLock<()>,
}
impl Gate {
    fn new() -> Self {
        Self {
            inner: OnceLock::new(),
            profile: OnceLock::new(),
            forwarder: OnceLock::new(),
            retired: AtomicBool::new(false),
            fence: RwLock::new(()),
        }
    }
    fn network(&self) -> Result<&Arc<dyn Network>> {
        if self.retired.load(Ordering::Acquire) {
            return Err(UNAVAILABLE.into());
        }
        self.inner.get().ok_or_else(|| UNAVAILABLE.to_owned())
    }
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        if let Some(forwarder) = self.forwarder.get() {
            forwarder.stop();
        }
        let _fence = self.fence.write().unwrap_or_else(|e| e.into_inner());
    }
    fn assigned(&self) -> bool {
        self.inner.get().is_some()
    }
    /// The first lease attempt has an outcome, so the listener port is known.
    fn settled(&self, gateway: Option<Ipv4Addr>) -> bool {
        gateway.is_none()
            || self
                .forwarder
                .get()
                .is_some_and(|f| f.status() != Forwarding::Acquiring)
    }
}
struct Lease<'a> {
    _gate: std::sync::RwLockReadGuard<'a, ()>,
    _inner: Box<dyn Publication + 'a>,
}
impl Publication for Lease<'_> {}
impl Network for Gate {
    fn generation(&self) -> Result<Option<String>> {
        self.network()?.generation()
    }
    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        let guard = self.fence.read().map_err(|_| UNAVAILABLE)?;
        let inner = self.network()?.publication()?;
        Ok(Box::new(Lease {
            _gate: guard,
            _inner: inner,
        }))
    }
    fn connect(&self, a: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        self.network()?.connect(a, timeout)
    }
    fn resolve(&self, a: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        self.network()?.resolve(a, deadline)
    }
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        self.network()?.datagram(ipv6)
    }
    fn bind_datagram(&self, port: u16) -> Result<Box<dyn NetworkDatagram + Sync>> {
        self.network()?.bind_datagram(port)
    }
    fn listen(&self, a: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        self.network()?.listen(a)
    }
    fn http(&self) -> HttpClient {
        match self.network() {
            Ok(inner) => inner.http(),
            Err(_) => HttpClient::new().with_connector(Arc::new(Refused)),
        }
    }
}
/// Direct networking for work that does not go through the VPN while it is
/// on. It ends with the gate's generation, so nothing leaves while the VPN
/// changes.
pub(crate) struct Bypass {
    gate: Arc<Gate>,
    direct: Arc<dyn Network>,
}
impl Bypass {
    pub(crate) fn new(gate: Arc<Gate>) -> Self {
        Self {
            gate,
            direct: crate::torrent::transport::direct(),
        }
    }
    fn network(&self) -> Result<&Arc<dyn Network>> {
        if self.gate.retired.load(Ordering::Acquire) {
            return Err(UNAVAILABLE.into());
        }
        Ok(&self.direct)
    }
}
impl Network for Bypass {
    fn generation(&self) -> Result<Option<String>> {
        self.network()?.generation()
    }
    fn publication(&self) -> Result<Box<dyn Publication + '_>> {
        let guard = self.gate.fence.read().map_err(|_| UNAVAILABLE)?;
        let inner = self.network()?.publication()?;
        Ok(Box::new(Lease {
            _gate: guard,
            _inner: inner,
        }))
    }
    fn connect(&self, a: SocketAddr, timeout: Duration) -> Result<Box<dyn NetworkStream>> {
        self.network()?.connect(a, timeout)
    }
    fn resolve(&self, a: &str, deadline: Instant) -> Result<Vec<SocketAddr>> {
        self.network()?.resolve(a, deadline)
    }
    fn datagram(&self, ipv6: bool) -> Result<Box<dyn NetworkDatagram>> {
        self.network()?.datagram(ipv6)
    }
    fn bind_datagram(&self, port: u16) -> Result<Box<dyn NetworkDatagram + Sync>> {
        self.network()?.bind_datagram(port)
    }
    fn listen(&self, a: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        self.network()?.listen(a)
    }
    fn http(&self) -> HttpClient {
        match self.network() {
            Ok(inner) => inner.http(),
            Err(_) => HttpClient::new().with_connector(Arc::new(Refused)),
        }
    }
}
struct Refused;
impl HttpConnector for Refused {
    fn connect(&self, _: &str, _: u16, _: Instant) -> Result<Box<dyn NetworkStream>> {
        Err(UNAVAILABLE.into())
    }
}

struct State {
    document: String,
    mode: Mode,
    driver: Option<HttpTunnel>,
    /// The shown name of each profile, in failover order.
    names: Vec<String>,
    /// NAT-PMP gateway of each profile while forwarding is on: its first
    /// DNS server, which every parsed profile has.
    gateways: Vec<Ipv4Addr>,
    forwarding: bool,
    /// Whether each profile's tunnel carries IPv6.
    ipv6: Vec<bool>,
    error: Option<&'static str>,
    /// The last telemetry the tunnel answered, and when: a tunnel busy with
    /// traffic may miss one status deadline without anything being wrong.
    telemetry: Option<(Instant, Report)>,
}
impl State {
    /// Takes what is shown and used of `intent` besides its tunnel.
    fn describe(&mut self, intent: &Intent) {
        self.mode = intent.mode;
        self.forwarding = intent.forwarding;
        self.names = intent.names();
        self.ipv6 = intent.profiles.iter().map(|e| e.profile.ipv6()).collect();
        self.gateways = if intent.forwarding {
            intent
                .profiles
                .iter()
                .filter_map(|e| e.profile.dns().first().copied())
                .collect()
        } else {
            Vec::new()
        };
    }
}
/// How long a missed status query may reuse the last answer.
const TELEMETRY_GRACE: Duration = Duration::from_secs(10);
/// What a control call carries besides its action.
#[derive(Default)]
pub(crate) struct Edit {
    /// The profile `import` adds, and the name it is shown with.
    pub(crate) profile: Option<Profile>,
    pub(crate) name: Option<String>,
    /// The profile `remove` and `move` act on, and where `move` puts it.
    pub(crate) index: Option<usize>,
    pub(crate) to: Option<usize>,
}
/// Shared across automatic engine recovery, never across a change in operator intent.
pub(crate) struct Operator {
    path: PathBuf,
    state: Mutex<State>,
    changed: AtomicBool,
}
impl Operator {
    /// A store without `vpn.conf` has the VPN off: everything goes direct.
    pub(crate) fn load(directory: &Path, runtime: bool) -> Arc<Self> {
        let path = directory.join("vpn.conf");
        let missing = matches!(fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let loaded = if missing {
            Ok(DISABLED.to_owned())
        } else {
            profile::read_private_text(&path).map_err(|_| ())
        };
        let intent = loaded.as_ref().ok().and_then(|s| Intent::decode(s).ok());
        let mut state = State {
            document: String::new(),
            mode: Mode::Revoked,
            driver: None,
            names: Vec::new(),
            gateways: Vec::new(),
            forwarding: false,
            ipv6: Vec::new(),
            error: None,
            telemetry: None,
        };
        if let Some(intent) = intent {
            state.document = intent.document();
            state.describe(&intent);
            if intent.mode == Mode::Enabled && runtime {
                // A lone profile keeps every retry; in a list, a dead server
                // hands over to the next profile sooner.
                let several = intent.profiles.len() > 1;
                let profiles: Vec<_> = intent
                    .profiles
                    .into_iter()
                    .map(|entry| {
                        let mut config = entry.profile.into_tunnel_config();
                        if several {
                            config.reconnect_attempts = FAILOVER_ATTEMPTS;
                        }
                        config
                    })
                    .collect();
                state.driver = (!profiles.is_empty())
                    .then(|| HttpTunnel::start_profiles(profiles).ok())
                    .flatten();
                if state.driver.is_none() {
                    state.error = Some("Cannot start protected tunnel");
                }
            }
        } else {
            state.error = Some("Private VPN intent is unreadable or invalid");
        }
        Arc::new(Self {
            path,
            state: Mutex::new(state),
            changed: AtomicBool::new(false),
        })
    }
    pub(crate) fn gate(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::new());
        if let Ok(state) = self.state.lock()
            && state.mode == Mode::Disabled
            && state.error.is_none()
        {
            let _ = gate.inner.set(crate::torrent::transport::direct());
        }
        gate
    }
    /// Protected, and with forwarding enabled, the first lease attempt done:
    /// the torrent listener then opens on the granted port before announcing.
    pub(crate) fn ready(&self, gate: &Gate) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if !gate.assigned() {
            if state.error.is_some() || self.changed.load(Ordering::Acquire) {
                return false;
            }
            if let Some(driver) = &state.driver
                && let Ok((network, profile)) =
                    driver.network_profile(Instant::now() + Duration::from_millis(100))
            {
                let _ = gate.profile.set(profile);
                let _ = gate.inner.set(network);
            }
        }
        // Forwarding asks the gateway of the profile this generation runs on.
        let gateway = gate
            .profile
            .get()
            .and_then(|profile| state.gateways.get(*profile).copied())
            .filter(|_| state.mode == Mode::Enabled);
        drop(state);
        if gate.publication().is_err() {
            return false;
        }
        if let (Some(gateway), Some(network)) = (gateway, gate.inner.get()) {
            gate.forwarder.get_or_init(|| {
                Forwarder::start(network.clone(), gateway, forwarding::Timing::default())
            });
        }
        gate.settled(gateway)
    }
    pub(crate) fn needs_reload(&self, gate: &Gate) -> bool {
        self.changed.load(Ordering::Acquire)
            || gate.forwarder.get().is_some_and(Forwarder::stale)
            || (gate.assigned()
                && gate.publication().is_err()
                && self
                    .state
                    .lock()
                    .is_ok_and(|s| s.error.is_none() && s.mode == Mode::Enabled))
    }
    /// The VPN is on, or its intent cannot be read: traffic chosen for the
    /// VPN waits for the tunnel instead of going direct.
    pub(crate) fn protecting(&self) -> bool {
        self.state.lock().map_or(true, |state| {
            state.mode != Mode::Disabled || state.error.is_some()
        })
    }
    pub(crate) fn changed(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }
    pub(crate) fn report(&self, gate: &Gate) -> Value {
        let mut value = Value::object();
        value.insert("complete_isolation_verified", false);
        value.insert("forwarding", "disabled");
        value.insert("handshake_age_secs", Value::Null);
        value.insert("ipv6", false);
        let Ok(mut state) = self.state.try_lock() else {
            value.insert("state", "blocked");
            value.insert("reason", "VPN control is busy");
            return value;
        };
        forwarding_json(&mut value, &state, gate);
        value.insert("enabled", state.mode != Mode::Disabled);
        value.insert("profile_present", state.document.contains("[Interface]"));
        value.insert("profiles", profiles_json(&state.names, None));
        if let Some(error) = state.error {
            value.insert("state", "error");
            value.insert("reason", error);
            return value;
        }
        if self.changed() || gate.retired.load(Ordering::Acquire) {
            value.insert("state", "blocked");
            value.insert("reason", "Retiring previous engine generation");
            return value;
        }
        match state.mode {
            Mode::Disabled => {
                value.insert("state", "disabled");
            }
            Mode::Revoked => {
                value.insert("state", "blocked");
                value.insert("reason", "Profile revoked by operator");
            }
            Mode::Enabled => {
                if let Some(driver) = &state.driver {
                    let now = Instant::now();
                    let telemetry = match driver.report(now + Duration::from_millis(100)) {
                        Ok(telemetry) => {
                            state.telemetry = Some((now, telemetry.clone()));
                            Ok(telemetry)
                        }
                        Err(error) => state
                            .telemetry
                            .as_ref()
                            .filter(|(at, _)| now.duration_since(*at) < TELEMETRY_GRACE)
                            .map(|(_, telemetry)| telemetry.clone())
                            .ok_or(error),
                    };
                    match telemetry {
                        Ok(report) => {
                            let (status, counters, age) =
                                (report.status, report.stats, report.handshake_age);
                            value.insert(
                                "state",
                                match status.state {
                                    TunnelState::Connecting => "connecting",
                                    TunnelState::Protected
                                        if gate.assigned() && gate.generation().is_err() =>
                                    {
                                        "reconnecting"
                                    }
                                    TunnelState::Protected => "protected",
                                    TunnelState::Reconnecting => "reconnecting",
                                    TunnelState::Blocked => "blocked",
                                    TunnelState::Closed => "error",
                                },
                            );
                            if let Some(reason) = status.reason {
                                value.insert("reason", format!("{reason:?}"));
                            }
                            value.insert("attempt", status.attempt);
                            // IPv6 is active only in a protected tunnel that carries it.
                            value.insert(
                                "ipv6",
                                state.ipv6.get(status.profile) == Some(&true)
                                    && status.state == TunnelState::Protected,
                            );
                            if let Some(age) = age {
                                value.insert("handshake_age_secs", age.as_secs().to_string());
                            }
                            value.insert("counters", counters_json(counters));
                            value.insert(
                                "profiles",
                                profiles_json(&state.names, Some(status.profile)),
                            );
                            if let Some(name) = state.names.get(status.profile) {
                                value.insert("active_profile", name.as_str());
                            }
                            if let Some(wait) = report
                                .retry_in
                                .filter(|_| status.state == TunnelState::Reconnecting)
                            {
                                value.insert("retry_in_secs", Value::Number(wait.as_secs() as f64));
                            }
                            value.insert("switches", switches_json(&report.switches, &state.names));
                        }
                        Err(_) => {
                            value.insert("state", "error");
                            value.insert("reason", "Tunnel status unavailable");
                        }
                    }
                } else {
                    value.insert("state", "blocked");
                    value.insert(
                        "reason",
                        "Protected networking requires the running service",
                    );
                }
            }
        }
        value
    }
    /// Called only while this engine holds the journal's exclusive process lock.
    /// Retirement precedes persistence; any write failure keeps the gate closed.
    pub(crate) fn control(&self, gate: &Gate, action: &str, edit: Edit) -> Result<Value> {
        let mut state = self.state.lock().map_err(|_| UNAVAILABLE)?;
        if self.changed() {
            return Err("VPN generation is already retiring; retry after reload".into());
        }
        let mut intent = if state.error.is_none() {
            Intent::decode(&state.document)?
        } else {
            Intent {
                mode: Mode::Revoked,
                forwarding: false,
                profiles: Vec::new(),
            }
        };
        let count = intent.profiles.len();
        let position = |index: Option<usize>| {
            index
                .filter(|index| *index < count)
                .ok_or("No such VPN profile")
        };
        match action {
            // A new profile joins the end of the list.
            "import" => {
                let profile = edit.profile.ok_or("VPN import requires a profile")?;
                let document = profile.private_document();
                if intent
                    .profiles
                    .iter()
                    .any(|entry| entry.profile.private_document() == document)
                {
                    return Err("This VPN profile is already imported".into());
                }
                if count == MAX_PROFILES {
                    return Err(format!("Mynou keeps at most {MAX_PROFILES} VPN profiles"));
                }
                intent.profiles.push(Entry {
                    name: edit.name.as_deref().and_then(profile_name),
                    profile,
                });
                intent.mode = Mode::Enabled;
            }
            "remove" => {
                let index = position(edit.index)?;
                // Without a profile, protected traffic could only go direct.
                if count == 1 && intent.mode != Mode::Disabled {
                    return Err("Turn the VPN off before removing its last profile".into());
                }
                intent.profiles.remove(index);
                intent.forwarding &= !intent.profiles.is_empty();
            }
            "move" => {
                let (index, to) = (position(edit.index)?, position(edit.to)?);
                let entry = intent.profiles.remove(index);
                intent.profiles.insert(to, entry);
            }
            "enable" | "reconnect" => {
                if intent.profiles.is_empty() {
                    return Err("Import a VPN profile first".into());
                }
                intent.mode = Mode::Enabled;
            }
            "disable" => {
                intent.mode = Mode::Disabled;
            }
            "enable-forwarding" | "disable-forwarding" => {
                if intent.profiles.is_empty() {
                    return Err("Import a VPN profile first".into());
                }
                if intent.profiles.iter().any(|e| e.profile.dns().is_empty()) {
                    return Err("Forwarding uses the profile DNS server as gateway".into());
                }
                intent.forwarding = action == "enable-forwarding";
            }
            "revoke" => {
                if intent.profiles.is_empty() {
                    return Err("Import a VPN profile first".into());
                }
                intent.mode = Mode::Revoked;
            }
            _ => return Err("Unknown VPN action".into()),
        }
        // In a list, every profile keeps the name it is shown with now, so
        // moving one never renames another.
        if intent.profiles.len() > 1 {
            let names = intent.names();
            for (entry, name) in intent.profiles.iter_mut().zip(names) {
                entry.name.get_or_insert(name);
            }
        }
        let document = intent.document();
        if document.len() > profile::MAX_PROFILE_BYTES {
            return Err("These VPN profiles are too large to keep together".into());
        }
        gate.retire();
        if let Some(driver) = &state.driver {
            driver.cancel();
        }
        if persist(&self.path, document.as_bytes()).is_err() {
            state.error = Some(STORAGE);
            return Err(STORAGE.into());
        }
        state.describe(&intent);
        state.document = document;
        self.changed.store(true, Ordering::Release);
        drop(state);
        Ok(self.report(gate))
    }
}
/// Forwarding is reported apart from the tunnel state: a missing lease is
/// never a VPN failure. The port is what peers and trackers are told.
fn forwarding_json(value: &mut Value, state: &State, gate: &Gate) {
    if !state.forwarding {
        return;
    }
    let status = match gate.forwarder.get() {
        _ if state.gateways.is_empty() => Forwarding::Unavailable("Profile has no DNS gateway"),
        Some(forwarder) => forwarder.status(),
        None if state.mode == Mode::Enabled => Forwarding::Acquiring,
        None => Forwarding::Unavailable("Protection is not enabled"),
    };
    match status {
        Forwarding::Acquiring => value.insert("forwarding", "acquiring"),
        Forwarding::Active(port) => {
            value.insert("forwarding", "active");
            value.insert("forwarded_port", port.to_string());
        }
        Forwarding::Unavailable(reason) => {
            value.insert("forwarding", "unavailable");
            value.insert("forwarding_reason", reason);
        }
    }
}
/// The profiles in failover order; `active` marks the one in use or tried next.
fn profiles_json(names: &[String], active: Option<usize>) -> Value {
    Value::Array(
        names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let mut value = Value::object();
                value.insert("name", name.as_str());
                value.insert("active", active == Some(index));
                value
            })
            .collect(),
    )
}
/// The recent profile switches, newest first.
fn switches_json(switches: &[Switch], names: &[String]) -> Value {
    let name = |index: usize| names.get(index).map_or(Value::Null, |n| n.as_str().into());
    Value::Array(
        switches
            .iter()
            .rev()
            .map(|switch| {
                let at = switch
                    .at
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut value = Value::object();
                value.insert("at", Value::Number(at as f64));
                value.insert("from", name(switch.from));
                value.insert("to", name(switch.to));
                value.insert("reason", format!("{:?}", switch.reason));
                value
            })
            .collect(),
    )
}
fn counters_json(stats: Stats) -> Value {
    let mut value = Value::object();
    for (name, n) in [
        ("generations", stats.generations),
        ("received_datagrams", stats.received),
        ("sent_datagrams", stats.sent),
        ("rejected_datagrams", stats.rejected),
        ("send_errors", stats.send_errors),
        ("inner_dropped", stats.inner_dropped),
    ] {
        value.insert(name, n.to_string());
    }
    value
}
fn persist(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    crate::store::reject_symlinks(path).map_err(|_| STORAGE)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0)
    {
        return Err(STORAGE.into());
    }
    let parent = path.parent().ok_or(STORAGE)?;
    let suffix = crate::crypto::random_bytes::<16>().map_err(|_| STORAGE)?;
    let temporary = parent.join(format!(
        ".vpn-{}.tmp",
        suffix
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ));
    let mut created = false;
    let result = (|| {
        let mut file = crate::store::private_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| STORAGE)?;
        created = true;
        file.write_all(data)
            .and_then(|()| crate::store::sync_file(&file))
            .map_err(|_| STORAGE)?;
        fs::rename(&temporary, path).map_err(|_| STORAGE)?;
        crate::store::sync_directory(parent).map_err(|_| STORAGE)?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(temporary);
    }
    result
}

mod forwarding;
#[cfg(test)]
pub(crate) mod tests;

/// Builds a private request from a checked 0400/0600 import file. The returned
/// value contains credentials and is only for the authenticated control call.
pub fn import_request(path: &Path, structured: bool) -> Result<Value> {
    let profile = if structured {
        let text = profile::read_private_text(path).map_err(|e| e.to_string())?;
        let settings =
            crate::json::parse(&text).map_err(|_| "Invalid private WireGuard settings JSON")?;
        profile::from_settings(&settings)?
    } else {
        profile::read_private_file(path).map_err(|e| e.to_string())?
    };
    let mut body = Value::object();
    body.insert("action", "import");
    body.insert("profile", profile.private_document());
    Ok(body)
}

impl crate::engine::Engine {
    pub fn vpn_status(&self) -> Value {
        match (&self.vpn, &self.vpn_gate) {
            (Some(operator), Some(gate)) => operator.report(gate),
            _ => {
                let mut value = Value::object();
                value.insert("state", "external");
                value
            }
        }
    }
    pub fn vpn_control(&self, value: &Value) -> Result<Value> {
        if self.read_only {
            return Err("VPN controls require write access".into());
        }
        let fields = value.as_object().ok_or("VPN control must be an object")?;
        const FIELDS: [&str; 6] = ["action", "profile", "settings", "name", "index", "to"];
        if fields.keys().any(|key| !FIELDS.contains(&key.as_str())) {
            return Err("Unknown VPN control field".into());
        }
        let action = value
            .get("action")
            .and_then(Value::as_str)
            .ok_or("VPN action is required")?;
        let profile = match (action, value.get("profile"), value.get("settings")) {
            ("import", Some(Value::String(text)), None) => {
                Some(profile::parse(text).map_err(|e| e.to_string())?)
            }
            ("import", None, Some(settings)) => Some(profile::from_settings(settings)?),
            ("import", _, _) => {
                return Err("Supply exactly one WireGuard profile or settings object".into());
            }
            (_, None, None) => None,
            _ => return Err("Profile data is only accepted by import".into()),
        };
        let name = match (action, value.get("name")) {
            (_, None) => None,
            ("import", Some(Value::String(name))) => Some(name.clone()),
            _ => return Err("A profile name is only accepted by import".into()),
        };
        // Profile positions count from 0, in failover order.
        let position = |field: &str| match (action, value.get(field)) {
            (_, None) => Ok(None),
            ("remove" | "move", Some(index)) => index
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .map(Some)
                .ok_or("No such VPN profile"),
            _ => Err("A profile position is only accepted by remove and move"),
        };
        let edit = Edit {
            profile,
            name,
            index: position("index")?,
            to: position("to")?,
        };
        let (Some(operator), Some(gate)) = (&self.vpn, &self.vpn_gate) else {
            return Err("External transport has no operator VPN controls".into());
        };
        operator.control(gate, action, edit)
    }
    /// The VPN's state, the profile in use and the recent switches between
    /// profiles, for the statistics.
    pub(crate) fn vpn_failover(&self) -> Value {
        let status = self.vpn_status();
        let mut value = Value::object();
        for key in [
            "state",
            "reason",
            "active_profile",
            "retry_in_secs",
            "switches",
        ] {
            if let Some(field) = status.get(key) {
                value.insert(key, field.clone());
            }
        }
        value
    }
    pub(crate) fn vpn_ready(&self) -> bool {
        match (&self.vpn, &self.vpn_gate) {
            (Some(operator), Some(gate)) => operator.ready(gate),
            _ => true,
        }
    }
    pub(crate) fn vpn_needs_reload(&self) -> bool {
        match (&self.vpn, &self.vpn_gate) {
            (Some(operator), Some(gate)) => operator.needs_reload(gate),
            _ => false,
        }
    }
    /// The forwarded port the torrent listener must take, if one is held.
    pub(crate) fn forwarded_port(&self) -> Option<u16> {
        self.vpn_gate
            .as_ref()
            .filter(|_| self.torrents_protected)
            .and_then(|gate| gate.forwarder.get())
            .and_then(Forwarder::advertise)
    }
    pub(crate) fn retire_vpn_generation(&self) {
        if let Some(gate) = &self.vpn_gate {
            gate.retire();
        }
    }
}
