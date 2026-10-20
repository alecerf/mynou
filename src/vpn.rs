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
use std::{
    fs,
    io::Write,
    net::SocketAddr,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Disabled,
    Enabled,
    Revoked,
}
struct Intent {
    mode: Mode,
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
        let profile = if rest.is_empty() && mode == Mode::Disabled {
            None
        } else {
            Some(profile::parse(rest).map_err(|e| e.to_string())?)
        };
        Ok(Self { mode, profile })
    }
    fn document(&self) -> String {
        let mut text = match self.mode {
            Mode::Disabled => DISABLED,
            Mode::Enabled => ENABLED,
            Mode::Revoked => REVOKED,
        }
        .to_owned();
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
    retired: AtomicBool,
    fence: RwLock<()>,
}
impl Gate {
    fn new() -> Self {
        Self {
            inner: OnceLock::new(),
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
        let _fence = self.fence.write().unwrap_or_else(|e| e.into_inner());
    }
    fn assigned(&self) -> bool {
        self.inner.get().is_some()
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
    error: Option<&'static str>,
}
/// Shared across automatic engine recovery, never across a change in operator intent.
pub(crate) struct Operator {
    path: PathBuf,
    state: Mutex<State>,
    changed: AtomicBool,
}
impl Operator {
    pub(crate) fn load(directory: &Path, runtime: bool) -> Arc<Self> {
        let path = directory.join("vpn.conf");
        let loaded = match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DISABLED.to_owned()),
            _ => profile::read_private_text(&path).map_err(|_| ()),
        };
        let intent = loaded.as_ref().ok().and_then(|s| Intent::decode(s).ok());
        let mut state = State {
            document: String::new(),
            mode: Mode::Revoked,
            driver: None,
            error: None,
        };
        if let Some(intent) = intent {
            state.document = intent.document();
            state.mode = intent.mode;
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
    pub(crate) fn ready(&self, gate: &Gate) -> bool {
        if gate.assigned() {
            return gate.publication().is_ok();
        }
        let Ok(state) = self.state.lock() else {
            return false;
        };
        if state.error.is_some() || self.changed.load(Ordering::Acquire) {
            return false;
        }
        if let Some(driver) = &state.driver
            && let Ok(network) = driver.network(Instant::now() + Duration::from_millis(100))
        {
            let _ = gate.inner.set(network);
        }
        gate.publication().is_ok()
    }
    pub(crate) fn needs_reload(&self, gate: &Gate) -> bool {
        self.changed.load(Ordering::Acquire)
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
        value.insert("forwarding", "unsupported");
        value.insert("handshake_age_secs", Value::Null);
        let Ok(state) = self.state.try_lock() else {
            value.insert("state", "blocked");
            value.insert("reason", "VPN control is busy");
            return value;
        };
        value.insert("enabled", state.mode != Mode::Disabled);
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
            Mode::Enabled => {
                if let Some(driver) = &state.driver {
                    match driver.telemetry(Instant::now() + Duration::from_millis(100)) {
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
        let mut intent = if state.error.is_none() {
            Intent::decode(&state.document)?
        } else {
            Intent {
                mode: Mode::Revoked,
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
        state.document = document;
        self.changed.store(true, Ordering::Release);
        drop(state);
        Ok(self.report(gate))
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
    let result = (|| {
        let mut file = crate::store::private_options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| STORAGE)?;
        file.write_all(data)
            .and_then(|()| crate::store::sync_file(&file))
            .map_err(|_| STORAGE)?;
        fs::rename(&temporary, path).map_err(|_| STORAGE)?;
        crate::store::sync_directory(parent).map_err(|_| STORAGE)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests;

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
    pub(crate) fn retire_vpn_generation(&self) {
        if let Some(gate) = &self.vpn_gate {
            gate.retire();
        }
    }
}
