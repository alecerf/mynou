//! Private operator intent and immutable, revocable engine generations.
//! This control plane does not establish the isolation evidence of issue #224.
use crate::{
    Result,
    json::Value,
    net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream},
    torrent::transport::{Network, NetworkListener, Publication},
    tunnel::{Stats, TunnelState},
    tunnel_http::HttpTunnel,
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
    time::{Duration, Instant},
};

const UNAVAILABLE: &str = "Torrent protection is unavailable or revoked";
const STORAGE: &str = "Cannot persist private VPN intent; torrent networking remains blocked";
const DISABLED: &str = "# mynou-vpn-v1 disabled\n";
const ENABLED: &str = "# mynou-vpn-v1 enabled\n";
const REVOKED: &str = "# mynou-vpn-v1 revoked\n";
const UNCHOSEN: &str = "Choose a VPN profile or your own connection before torrents start";
/// Opt-in NAT-PMP forwarding to the profile's DNS gateway (owner decision, #223).
const FORWARDING: &str = "# mynou-forwarding natpmp\n";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Disabled,
    Enabled,
    Revoked,
    /// No `vpn.conf` yet: torrents wait until the operator imports a profile
    /// or chooses Direct. Never persisted; any control writes a real intent.
    Unchosen,
}
struct Intent {
    mode: Mode,
    forwarding: bool,
    profile: Option<Profile>,
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
        let profile = if rest.is_empty() && mode == Mode::Disabled {
            None
        } else {
            Some(profile::parse(rest).map_err(|e| e.to_string())?)
        };
        if forwarding && profile.is_none() {
            return Err("Invalid private VPN intent".into());
        }
        Ok(Self {
            mode,
            forwarding,
            profile,
        })
    }
    fn document(&self) -> String {
        let mut text = match self.mode {
            Mode::Disabled | Mode::Unchosen => DISABLED,
            Mode::Enabled => ENABLED,
            Mode::Revoked => REVOKED,
        }
        .to_owned();
        if self.forwarding {
            text.push_str(FORWARDING);
        }
        if let Some(profile) = &self.profile {
            text.push_str(&profile.private_document());
        }
        text
    }
}

/// One permanently retired gate per engine. The network is assigned once;
/// reconnects construct another engine rather than repointing outstanding work.
pub(crate) struct Gate {
    inner: OnceLock<Arc<dyn Network>>,
    /// This generation's forwarded-port lease, when forwarding is enabled.
    forwarder: OnceLock<Forwarder>,
    retired: AtomicBool,
    fence: RwLock<()>,
}
impl Gate {
    fn new() -> Self {
        Self {
            inner: OnceLock::new(),
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
    /// NAT-PMP gateway of an enabled forwarding intent: the profile's DNS.
    gateway: Option<Ipv4Addr>,
    forwarding: bool,
    /// The profile's tunnel carries IPv6.
    ipv6: bool,
    error: Option<&'static str>,
    /// The last telemetry the tunnel answered, and when: a tunnel busy with
    /// traffic may miss one status deadline without anything being wrong.
    telemetry: Option<(Instant, Telemetry)>,
}
type Telemetry = (
    crate::tunnel::Status,
    crate::tunnel::Stats,
    Option<Duration>,
);
/// How long a missed status query may reuse the last answer.
const TELEMETRY_GRACE: Duration = Duration::from_secs(10);
/// Shared across automatic engine recovery, never across a change in operator intent.
pub(crate) struct Operator {
    path: PathBuf,
    state: Mutex<State>,
    changed: AtomicBool,
}
impl Operator {
    pub(crate) fn load(directory: &Path, runtime: bool) -> Arc<Self> {
        Self::load_with(directory, runtime, false)
    }
    /// As the service loads it: a store that never chose waits for a choice
    /// instead of using Direct, so no torrent leaves before a profile exists.
    pub(crate) fn load_for_service(directory: &Path) -> Arc<Self> {
        Self::load_with(directory, true, true)
    }
    fn load_with(directory: &Path, runtime: bool, choose: bool) -> Arc<Self> {
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
            gateway: None,
            forwarding: false,
            ipv6: false,
            error: None,
            telemetry: None,
        };
        if missing && choose {
            state.mode = Mode::Unchosen;
        } else if let Some(intent) = intent {
            state.document = intent.document();
            state.mode = intent.mode;
            state.forwarding = intent.forwarding;
            state.ipv6 = intent.profile.as_ref().is_some_and(Profile::ipv6);
            if intent.forwarding {
                state.gateway = intent
                    .profile
                    .as_ref()
                    .and_then(|p| p.dns().first().copied());
            }
            if intent.mode == Mode::Enabled && runtime {
                state.driver = intent
                    .profile
                    .and_then(|p| HttpTunnel::start(p.into_tunnel_config()).ok());
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
        let gateway = state.gateway.filter(|_| state.mode == Mode::Enabled);
        if !gate.assigned() {
            if state.error.is_some() || self.changed.load(Ordering::Acquire) {
                return false;
            }
            if let Some(driver) = &state.driver
                && let Ok(network) = driver.network(Instant::now() + Duration::from_millis(100))
            {
                let _ = gate.inner.set(network);
            }
        }
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
        value.insert(
            "enabled",
            !matches!(state.mode, Mode::Disabled | Mode::Unchosen),
        );
        value.insert("profile_present", state.document.contains("[Interface]"));
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
            Mode::Unchosen => {
                value.insert("state", "unchosen");
                value.insert("reason", UNCHOSEN);
            }
            Mode::Enabled => {
                if let Some(driver) = &state.driver {
                    let now = Instant::now();
                    let telemetry = match driver.telemetry(now + Duration::from_millis(100)) {
                        Ok(telemetry) => {
                            state.telemetry = Some((now, telemetry));
                            Ok(telemetry)
                        }
                        Err(error) => state
                            .telemetry
                            .filter(|(at, _)| now.duration_since(*at) < TELEMETRY_GRACE)
                            .map(|(_, telemetry)| telemetry)
                            .ok_or(error),
                    };
                    match telemetry {
                        Ok((status, counters, age)) => {
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
                                state.ipv6 && status.state == TunnelState::Protected,
                            );
                            if let Some(age) = age {
                                value.insert("handshake_age_secs", age.as_secs().to_string());
                            }
                            value.insert("counters", counters_json(counters));
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
    pub(crate) fn control(
        &self,
        gate: &Gate,
        action: &str,
        profile: Option<Profile>,
    ) -> Result<Value> {
        let mut state = self.state.lock().map_err(|_| UNAVAILABLE)?;
        if self.changed() {
            return Err("VPN generation is already retiring; retry after reload".into());
        }
        let mut intent = if state.mode == Mode::Unchosen {
            Intent {
                mode: Mode::Unchosen,
                forwarding: false,
                profile: None,
            }
        } else if state.error.is_none() {
            Intent::decode(&state.document)?
        } else {
            Intent {
                mode: Mode::Revoked,
                forwarding: false,
                profile: None,
            }
        };
        match action {
            "import" => {
                intent.profile = Some(profile.ok_or("VPN import requires a profile")?);
                intent.mode = Mode::Enabled;
            }
            "enable" | "reconnect" => {
                if intent.profile.is_none() {
                    return Err("Import a VPN profile first".into());
                }
                intent.mode = Mode::Enabled;
            }
            "disable" => {
                intent.mode = Mode::Disabled;
            }
            "enable-forwarding" | "disable-forwarding" => {
                let Some(profile) = &intent.profile else {
                    return Err("Import a VPN profile first".into());
                };
                if profile.dns().is_empty() {
                    return Err("Forwarding uses the profile DNS server as gateway".into());
                }
                intent.forwarding = action == "enable-forwarding";
            }
            "revoke" => {
                if intent.profile.is_none() {
                    return Err("Import a VPN profile first".into());
                }
                intent.mode = Mode::Revoked;
            }
            _ => return Err("Unknown VPN action".into()),
        }
        gate.retire();
        if let Some(driver) = &state.driver {
            driver.cancel();
        }
        let document = intent.document();
        if persist(&self.path, document.as_bytes()).is_err() {
            state.error = Some(STORAGE);
            return Err(STORAGE.into());
        }
        state.mode = intent.mode;
        state.forwarding = intent.forwarding;
        state.ipv6 = intent.profile.as_ref().is_some_and(Profile::ipv6);
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
    let status = match (state.gateway, gate.forwarder.get()) {
        (None, _) => Forwarding::Unavailable("Profile has no DNS gateway"),
        (Some(_), Some(forwarder)) => forwarder.status(),
        (Some(_), None) if state.mode == Mode::Enabled => Forwarding::Acquiring,
        (Some(_), None) => Forwarding::Unavailable("Protection is not enabled"),
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
        if fields
            .keys()
            .any(|key| !["action", "profile", "settings"].contains(&key.as_str()))
        {
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
        let (Some(operator), Some(gate)) = (&self.vpn, &self.vpn_gate) else {
            return Err("External transport has no operator VPN controls".into());
        };
        operator.control(gate, action, profile)
    }
    /// No VPN mode was ever chosen, so torrents and startup wait for one.
    pub(crate) fn vpn_unchosen(&self) -> bool {
        self.vpn
            .as_ref()
            .and_then(|operator| operator.state.lock().ok())
            .is_some_and(|state| state.mode == Mode::Unchosen)
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
            .and_then(|gate| gate.forwarder.get())
            .and_then(Forwarder::advertise)
    }
    pub(crate) fn retire_vpn_generation(&self) {
        if let Some(gate) = &self.vpn_gate {
            gate.retire();
        }
    }
}
