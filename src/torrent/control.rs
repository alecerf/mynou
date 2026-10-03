//! Bounded transfer controls, verified persistence and aggregate payload limits.
use crate::crypto::sha256;
use crate::json::{self, Value};
use crate::Result;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_RATE: u64 = 1_073_741_824;
const MAX_SEED_TIME: u64 = 315_360_000;
const MAX_SEED_RATIO: u32 = 1_000_000;
const MAX_FILES: usize = 100_000;
const MAX_CONTROL_BYTES: usize = 4 * 1024 * 1024;
const CONTROL_MAGIC: &[u8; 8] = b"MYNOUC01";
const BLOCK_BYTES: u64 = 16 * 1024;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MAX_WAITERS: usize = 1_024;
const CANCELLATION_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferPolicy {
    pub download_limit_bps: u64,
    pub upload_limit_bps: u64,
    pub seed_ratio_milli: Option<u32>,
    pub seed_time_secs: Option<u64>,
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(entries.into_iter().map(|(key, value)| (key.into(), value)).collect())
}

fn strict_fields<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a BTreeMap<String, Value>> {
    let Value::Object(fields) = value else {
        return Err("Transfer controls must be a JSON object".into());
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unknown transfer control field".into());
    }
    Ok(fields)
}

fn required<'a>(fields: &'a BTreeMap<String, Value>, key: &str) -> Result<&'a Value> {
    fields.get(key).ok_or_else(|| format!("Missing transfer control field: {key}"))
}

fn numeric_u64(value: &Value, key: &str) -> Result<u64> {
    value.as_u64().ok_or_else(|| format!("Transfer control {key} must be a nonnegative JSON integer"))
}

fn exact_u64(value: &Value, key: &str) -> Result<u64> {
    let text = value.as_str().ok_or_else(|| format!("Transfer control {key} must be a decimal string"))?;
    decimal_u64(text, key)
}

fn decimal_u64(text: &str, key: &str) -> Result<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(format!("Invalid decimal transfer control: {key}"));
    }
    text.parse().map_err(|_| format!("Transfer control {key} exceeds u64"))
}

fn boolean(fields: &BTreeMap<String, Value>, key: &str) -> Result<bool> {
    required(fields, key)?.as_bool().ok_or_else(|| format!("Transfer control {key} must be a boolean"))
}

impl TransferPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.download_limit_bps > MAX_RATE || self.upload_limit_bps > MAX_RATE {
            return Err("Transfer payload rate must be between 0 and 1,073,741,824 bytes per second".into());
        }
        if self.seed_ratio_milli.is_some_and(|ratio| !(1..=MAX_SEED_RATIO).contains(&ratio)) {
            return Err("Seed ratio must be between 1 and 1,000,000 thousandths, or null".into());
        }
        if self.seed_time_secs.is_some_and(|seconds| !(1..=MAX_SEED_TIME).contains(&seconds)) {
            return Err("Seed time must be between 1 and 315,360,000 seconds, or null".into());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        object([
            ("download_limit_bps", Value::Number(self.download_limit_bps as f64)),
            ("upload_limit_bps", Value::Number(self.upload_limit_bps as f64)),
            ("seed_ratio_milli", self.seed_ratio_milli.map_or(Value::Null, |ratio| Value::Number(f64::from(ratio)))),
            ("seed_time_secs", self.seed_time_secs.map_or(Value::Null, |seconds| Value::Number(seconds as f64))),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = strict_fields(value, &["download_limit_bps", "upload_limit_bps", "seed_ratio_milli", "seed_time_secs"])?;
        let rate = |key| fields.get(key).map_or(Ok(0), |value| numeric_u64(value, key));
        let nullable = |key| match fields.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => numeric_u64(value, key).map(Some),
        };
        let policy = Self {
            download_limit_bps: rate("download_limit_bps")?,
            upload_limit_bps: rate("upload_limit_bps")?,
            seed_ratio_milli: nullable("seed_ratio_milli")?.map(u32::try_from).transpose()
                .map_err(|_| "Seed ratio exceeds u32")?,
            seed_time_secs: nullable("seed_time_secs")?,
        };
        policy.validate()?;
        Ok(policy)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilePriority {
    Low,
    #[default]
    Normal,
    High,
}

impl FilePriority {
    pub fn rank(self) -> u8 {
        match self { Self::Low => 0, Self::Normal => 1, Self::High => 2 }
    }

    pub fn to_json(self) -> Value {
        Value::String(match self { Self::Low => "low", Self::Normal => "normal", Self::High => "high" }.into())
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        match value.as_str() {
            Some("low") => Ok(Self::Low), Some("normal") => Ok(Self::Normal), Some("high") => Ok(Self::High),
            _ => Err("File priority must be low, normal or high".into()),
        }
    }
}

/// Historical counters do not attest to the presence or integrity of payload.
/// A native client must verify files independently when restoring this record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentControl {
    pub id: String,
    pub queue_order: u64,
    pub priority: i32,
    pub user_paused: bool,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
    pub seed_elapsed_secs: u64,
    pub seed_limited: bool,
    pub file_priorities: BTreeMap<usize, FilePriority>,
    pub policy: Option<TransferPolicy>,
}

impl TorrentControl {
    pub fn new(id: String, queue_order: u64) -> Result<Self> {
        let control = Self {
            id, queue_order, priority: 0, user_paused: false, downloaded_bytes: 0,
            uploaded_bytes: 0, seed_elapsed_secs: 0, seed_limited: false,
            file_priorities: BTreeMap::new(), policy: None,
        };
        control.validate()?;
        Ok(control)
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(self.id.len(), 40 | 64)
            || !self.id.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("Transfer control ID must be a lowercase torrent hash".into());
        }
        if self.queue_order == 0 || !(-1_000..=1_000).contains(&self.priority) {
            return Err("Transfer queue order must be positive and priority must be between -1,000 and 1,000".into());
        }
        if self.file_priorities.len() > MAX_FILES || self.file_priorities.keys().any(|index| *index >= MAX_FILES) {
            return Err("Transfer file priorities exceed the 100,000-file bound".into());
        }
        if let Some(policy) = &self.policy { policy.validate()?; }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        object([
            ("id", self.id.clone().into()),
            ("queue_order", self.queue_order.to_string().into()),
            ("priority", Value::Number(f64::from(self.priority))),
            ("user_paused", self.user_paused.into()),
            ("downloaded_bytes", self.downloaded_bytes.to_string().into()),
            ("uploaded_bytes", self.uploaded_bytes.to_string().into()),
            ("seed_elapsed_secs", self.seed_elapsed_secs.to_string().into()),
            ("seed_limited", self.seed_limited.into()),
            ("file_priorities", Value::Object(self.file_priorities.iter().map(|(index, priority)| (index.to_string(), priority.to_json())).collect())),
            ("policy", self.policy.as_ref().map_or(Value::Null, TransferPolicy::to_json)),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = strict_fields(value, &["id", "queue_order", "priority", "user_paused", "downloaded_bytes", "uploaded_bytes", "seed_elapsed_secs", "seed_limited", "file_priorities", "policy"])?;
        let priority = required(fields, "priority")?.as_i64().and_then(|priority| i32::try_from(priority).ok())
            .ok_or("Transfer queue priority must be a JSON integer")?;
        let Value::Object(priorities) = required(fields, "file_priorities")? else {
            return Err("Transfer file priorities must be an object keyed by file index".into());
        };
        if priorities.len() > MAX_FILES { return Err("Too many transfer file priorities".into()); }
        let mut file_priorities = BTreeMap::new();
        for (index, priority) in priorities {
            let index = decimal_u64(index, "file index")?;
            let index = usize::try_from(index).map_err(|_| "Transfer file index exceeds usize")?;
            if index >= MAX_FILES { return Err("Transfer file index exceeds the 100,000-file bound".into()); }
            file_priorities.insert(index, FilePriority::from_json(priority)?);
        }
        let control = Self {
            id: required(fields, "id")?.as_str().ok_or("Transfer control ID must be a string")?.into(),
            queue_order: exact_u64(required(fields, "queue_order")?, "queue_order")?,
            priority, user_paused: boolean(fields, "user_paused")?,
            downloaded_bytes: exact_u64(required(fields, "downloaded_bytes")?, "downloaded_bytes")?,
            uploaded_bytes: exact_u64(required(fields, "uploaded_bytes")?, "uploaded_bytes")?,
            seed_elapsed_secs: exact_u64(required(fields, "seed_elapsed_secs")?, "seed_elapsed_secs")?,
            seed_limited: boolean(fields, "seed_limited")?, file_priorities,
            policy: match required(fields, "policy")? {
                Value::Null => None, value => Some(TransferPolicy::from_json(value)?),
            },
        };
        control.validate()?;
        Ok(control)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let payload = json::stringify(&self.to_json()).into_bytes();
        if payload.len() > MAX_CONTROL_BYTES { return Err("Transfer control record exceeds 4 MiB".into()); }
        let mut encoded = Vec::with_capacity(payload.len() + 44);
        encoded.extend_from_slice(CONTROL_MAGIC);
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&sha256(&encoded));
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if !(44..=MAX_CONTROL_BYTES + 44).contains(&encoded.len()) || &encoded[..8] != CONTROL_MAGIC {
            return Err("Invalid transfer control record size or version".into());
        }
        let length = u32::from_le_bytes(encoded[8..12].try_into().map_err(|_| "Invalid transfer control length")?) as usize;
        if length > MAX_CONTROL_BYTES || length.checked_add(44) != Some(encoded.len()) {
            return Err("Invalid transfer control payload length".into());
        }
        if sha256(&encoded[..encoded.len() - 32]).as_slice() != &encoded[encoded.len() - 32..] {
            return Err("Transfer control checksum mismatch".into());
        }
        Self::from_json(&json::parse(std::str::from_utf8(&encoded[12..12 + length])
            .map_err(|_| "Transfer control payload is not UTF-8")?)?)
    }
}

/// One block of burst capacity is shared by every caller, independent of rate.
/// Credits use byte-nanoseconds so fractional bytes are never rounded away.
struct TokenBucket {
    rate: u64,
    credit: u128,
    at_nanos: u128,
}

impl TokenBucket {
    fn new(rate: u64) -> Self {
        Self { rate, credit: u128::from(BLOCK_BYTES) * NANOS_PER_SECOND, at_nanos: 0 }
    }

    fn advance(&mut self, now_nanos: u128) {
        let accrued = now_nanos.saturating_sub(self.at_nanos).saturating_mul(u128::from(self.rate));
        self.credit = self.credit.saturating_add(accrued).min(u128::from(BLOCK_BYTES) * NANOS_PER_SECOND);
        self.at_nanos = self.at_nanos.max(now_nanos);
    }

    fn take(&mut self, bytes: u64, now_nanos: u128) -> Option<Duration> {
        self.advance(now_nanos);
        if self.rate == 0 { return None; }
        let required = u128::from(bytes) * NANOS_PER_SECOND;
        if self.credit >= required {
            self.credit -= required;
            None
        } else {
            let wait = (required - self.credit).div_ceil(u128::from(self.rate));
            Some(Duration::from_nanos(wait as u64))
        }
    }
}


struct GateState {
    bucket: TokenBucket,
    next_ticket: u64,
    waiters: VecDeque<u64>,
}

/// A FIFO aggregate payload gate. Unlimited transfers avoid the mutex entirely.
/// Condition-variable waits release the mutex; cancellation is checked at most
/// every 100 ms while waiting for shared credits or another caller's turn.
pub struct RateGate {
    limit: AtomicU64,
    epoch: Instant,
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Default for RateGate {
    fn default() -> Self { Self::with_rate(0) }
}

impl RateGate {
    fn with_rate(limit: u64) -> Self {
        Self {
            limit: AtomicU64::new(limit), epoch: Instant::now(), changed: Condvar::new(),
            state: Mutex::new(GateState { bucket: TokenBucket::new(limit), next_ticket: 0, waiters: VecDeque::new() }),
        }
    }

    pub fn new(limit: u64) -> Result<Self> {
        if limit > MAX_RATE { return Err("Payload rate exceeds 1,073,741,824 bytes per second".into()); }
        Ok(Self::with_rate(limit))
    }

    pub fn limit(&self) -> u64 { self.limit.load(Ordering::Acquire) }

    pub fn set_limit(&self, limit: u64) -> Result<()> {
        if limit > MAX_RATE { return Err("Payload rate exceeds 1,073,741,824 bytes per second".into()); }
        let mut state = self.state.lock().map_err(|_| "Payload rate gate is unavailable after a panic")?;
        state.bucket.advance(self.epoch.elapsed().as_nanos());
        state.bucket.rate = limit;
        self.limit.store(limit, Ordering::Release);
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    pub fn reserve(&self, bytes: u64, cancel: &AtomicBool) -> Result<()> {
        if bytes > BLOCK_BYTES { return Err("A payload reservation cannot exceed one 16 KiB block".into()); }
        if cancel.load(Ordering::Acquire) { return Err("Payload reservation cancelled".into()); }
        if bytes == 0 || self.limit() == 0 { return Ok(()); }
        let mut state = self.state.lock().map_err(|_| "Payload rate gate is unavailable after a panic")?;
        if state.waiters.len() >= MAX_WAITERS { return Err("Payload rate gate waiter capacity reached".into()); }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.checked_add(1).ok_or("Payload rate gate ticket capacity reached")?;
        state.waiters.push_back(ticket);
        loop {
            if cancel.load(Ordering::Acquire) || self.limit() == 0 {
                state.waiters.retain(|waiting| *waiting != ticket);
                self.changed.notify_all();
                return if cancel.load(Ordering::Acquire) { Err("Payload reservation cancelled".into()) } else { Ok(()) };
            }
            let wait = if state.waiters.front() == Some(&ticket) {
                match state.bucket.take(bytes, self.epoch.elapsed().as_nanos()) {
                    None => {
                        state.waiters.pop_front();
                        self.changed.notify_all();
                        return Ok(());
                    }
                    Some(wait) => wait.min(CANCELLATION_INTERVAL),
                }
            } else { CANCELLATION_INTERVAL };
            state = self.changed.wait_timeout(state, wait)
                .map_err(|_| "Payload rate gate is unavailable after a panic")?.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn control() -> TorrentControl {
        TorrentControl::new("1".repeat(40), 1).unwrap()
    }

    #[test]
    fn policy_defaults_bounds_and_numeric_json_round_trip() {
        assert_eq!(TransferPolicy::from_json(&Value::object()).unwrap(), TransferPolicy::default());
        let policy = TransferPolicy {
            download_limit_bps: MAX_RATE, upload_limit_bps: 1,
            seed_ratio_milli: Some(MAX_SEED_RATIO), seed_time_secs: Some(MAX_SEED_TIME),
        };
        assert_eq!(TransferPolicy::from_json(&policy.to_json()).unwrap(), policy);
        for field in ["download_limit_bps", "upload_limit_bps", "seed_ratio_milli", "seed_time_secs"] {
            for invalid in [Value::Bool(true), Value::String("1".into()), Value::Number(-1.0), Value::Number(1.5)] {
                let mut value = Value::object();
                value.insert(field, invalid);
                assert!(TransferPolicy::from_json(&value).is_err(), "{field} must reject invalid JSON types");
            }
        }
        for (field, invalid) in [
            ("download_limit_bps", MAX_RATE + 1), ("upload_limit_bps", MAX_RATE + 1),
            ("seed_ratio_milli", 0), ("seed_ratio_milli", u64::from(MAX_SEED_RATIO) + 1),
            ("seed_time_secs", 0), ("seed_time_secs", MAX_SEED_TIME + 1),
        ] {
            let mut value = Value::object();
            value.insert(field, Value::Number(invalid as f64));
            assert!(TransferPolicy::from_json(&value).is_err(), "{field} bounds");
        }
        let mut unknown = Value::object();
        unknown.insert("bandwidth", 1_u32);
        assert!(TransferPolicy::from_json(&unknown).is_err());
    }

    #[test]
    fn durable_control_preserves_exact_accounting_and_user_intent() {
        let mut original = control();
        original.queue_order = u64::MAX;
        original.priority = -1_000;
        original.user_paused = true;
        original.seed_limited = true;
        original.downloaded_bytes = u64::MAX;
        original.uploaded_bytes = u64::MAX - 1;
        original.seed_elapsed_secs = u64::MAX - 2;
        original.file_priorities = BTreeMap::from([(0, FilePriority::High), (99_999, FilePriority::Low)]);
        original.policy = Some(TransferPolicy { upload_limit_bps: 32_768, seed_ratio_milli: Some(1_500), ..TransferPolicy::default() });
        let encoded = original.encode().unwrap();
        assert_eq!(TorrentControl::decode(&encoded).unwrap(), original);
        assert_eq!(TorrentControl::from_json(&original.to_json()).unwrap(), original);
        assert_eq!(original.to_json().get("downloaded_bytes").and_then(Value::as_str), Some("18446744073709551615"));
        assert!(!original.to_json().as_object().unwrap().contains_key("ready"), "accounting does not assert completion");
    }

    #[test]
    fn control_corruption_truncation_and_appended_data_are_rejected() {
        let encoded = control().encode().unwrap();
        for length in [0, 7, 43, encoded.len() - 1] {
            assert!(TorrentControl::decode(&encoded[..length]).is_err());
        }
        for offset in [0, 8, 12, encoded.len() - 1] {
            let mut damaged = encoded.clone();
            damaged[offset] ^= 1;
            assert!(TorrentControl::decode(&damaged).is_err());
        }
        let mut appended = encoded;
        appended.push(0);
        assert!(TorrentControl::decode(&appended).is_err());
        assert!(TorrentControl::decode(&vec![0; MAX_CONTROL_BYTES + 45]).is_err());
    }

    #[test]
    fn control_parser_rejects_aliases_invalid_id_and_noncanonical_counters() {
        for id in [String::new(), "A".repeat(40), "../unsafe".into(), "a".repeat(41)] {
            assert!(TorrentControl::new(id, 1).is_err());
        }
        assert!(TorrentControl::new("0".repeat(40), 1).is_ok());
        assert!(TorrentControl::new("a".repeat(64), 0).is_err());
        for counter in ["queue_order", "downloaded_bytes", "uploaded_bytes", "seed_elapsed_secs"] {
            for invalid in ["01", "+1", "-1", "", "18446744073709551616"] {
                let mut value = control().to_json();
                value.insert(counter, invalid);
                assert!(TorrentControl::from_json(&value).is_err());
            }
            let mut value = control().to_json();
            value.insert(counter, 1_u32);
            assert!(TorrentControl::from_json(&value).is_err());
        }
        for index in ["01", "+1", "100000", "18446744073709551615"] {
            let mut value = control().to_json();
            let mut priorities = Value::object();
            priorities.insert(index, "high");
            value.insert("file_priorities", priorities);
            assert!(TorrentControl::from_json(&value).is_err());
        }
        for priority in [-1_001.0, 1_001.0, 1.5] {
            let mut value = control().to_json();
            value.insert("priority", Value::Number(priority));
            assert!(TorrentControl::from_json(&value).is_err());
        }
        let mut unknown = control().to_json();
        unknown.insert("paused", false);
        assert!(TorrentControl::from_json(&unknown).is_err());
        assert_eq!(FilePriority::Normal, FilePriority::default());
        assert!(FilePriority::Low < FilePriority::Normal && FilePriority::Normal < FilePriority::High);
        assert!(FilePriority::from_json(&Value::Number(2.0)).is_err());
    }

    #[test]
    fn fake_time_bucket_shares_one_block_burst_and_exact_refill() {
        let mut bucket = TokenBucket::new(1_024);
        assert_eq!(bucket.take(BLOCK_BYTES, 0), None);
        assert_eq!(bucket.take(1_024, 0), Some(Duration::from_secs(1)));
        assert_eq!(bucket.take(1_024, 250_000_000), Some(Duration::from_millis(750)));
        assert_eq!(bucket.take(1_024, NANOS_PER_SECOND), None);
        // Another peer cannot consume the credits just spent by the first.
        assert_eq!(bucket.take(1_024, NANOS_PER_SECOND), Some(Duration::from_secs(1)));
        assert_eq!(bucket.take(BLOCK_BYTES, 100 * NANOS_PER_SECOND), None);
        assert_eq!(bucket.credit, 0);
    }

    #[test]
    fn fake_time_bucket_preserves_fractional_credit_and_handles_rate_changes() {
        let mut bucket = TokenBucket::new(3);
        assert_eq!(bucket.take(BLOCK_BYTES, 0), None);
        assert_eq!(bucket.take(1, 0), Some(Duration::from_nanos(333_333_334)));
        assert_eq!(bucket.take(1, 333_333_333), Some(Duration::from_nanos(1)));
        assert_eq!(bucket.take(1, 333_333_334), None);
        assert_eq!(bucket.credit, 2);
        bucket.advance(NANOS_PER_SECOND);
        bucket.rate = 6;
        assert_eq!(bucket.take(2, NANOS_PER_SECOND), None);
        assert_eq!(bucket.take(1, NANOS_PER_SECOND), Some(Duration::from_nanos(166_666_667)));
        assert_eq!(bucket.take(1, NANOS_PER_SECOND + 166_666_667), None);
    }

    #[test]
    fn fake_time_bucket_never_mints_credit_on_clock_rollback() {
        let mut bucket = TokenBucket::new(1_000_000_000);
        assert_eq!(bucket.take(BLOCK_BYTES, 0), None);
        assert_eq!(bucket.take(100, 100), None);
        assert_eq!(bucket.take(50, 50), Some(Duration::from_nanos(50)));
        assert_eq!(bucket.take(50, 150), None);
        assert_eq!(bucket.credit, 0);
    }

    #[test]
    fn cancelled_and_oversized_payload_reservations_never_wait() {
        let gate = RateGate::new(1).unwrap();
        assert!(gate.reserve(BLOCK_BYTES, &AtomicBool::new(true)).unwrap_err().contains("cancelled"));
        assert!(gate.reserve(BLOCK_BYTES + 1, &AtomicBool::new(false)).is_err());
        assert!(RateGate::new(MAX_RATE + 1).is_err());
        assert!(gate.set_limit(MAX_RATE + 1).is_err());
        assert_eq!(gate.limit(), 1);
        gate.set_limit(0).unwrap();
        gate.reserve(BLOCK_BYTES, &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn unlimited_payload_hot_path_does_not_acquire_a_mutex() {
        let gate = Arc::new(RateGate::default());
        let worker = gate.clone();
        assert!(std::thread::spawn(move || {
            let _guard = worker.state.lock().unwrap();
            panic!("intentional mutex poisoning for the unlimited-path regression");
        }).join().is_err());
        gate.reserve(BLOCK_BYTES, &AtomicBool::new(false)).unwrap();
        assert!(gate.set_limit(1).is_err(), "a poisoned limited gate must fail explicitly");
    }
}
