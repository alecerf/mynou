//! Bounded transfer controls, verified persistence and aggregate payload limits.
use crate::Result;
use crate::crypto::sha256;
use crate::json::{self, Value};
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
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
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
    fields
        .get(key)
        .ok_or_else(|| format!("Missing transfer control field: {key}"))
}

fn numeric_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| format!("Transfer control {key} must be a nonnegative JSON integer"))
}

fn exact_u64(value: &Value, key: &str) -> Result<u64> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("Transfer control {key} must be a decimal string"))?;
    decimal_u64(text, key)
}

fn decimal_u64(text: &str, key: &str) -> Result<u64> {
    if text.is_empty()
        || text.len() > 20
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(format!("Invalid decimal transfer control: {key}"));
    }
    text.parse()
        .map_err(|_| format!("Transfer control {key} exceeds u64"))
}

fn boolean(fields: &BTreeMap<String, Value>, key: &str) -> Result<bool> {
    required(fields, key)?
        .as_bool()
        .ok_or_else(|| format!("Transfer control {key} must be a boolean"))
}

impl TransferPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.download_limit_bps > MAX_RATE || self.upload_limit_bps > MAX_RATE {
            return Err(
                "Transfer payload rate must be between 0 and 1,073,741,824 bytes per second".into(),
            );
        }
        if self
            .seed_ratio_milli
            .is_some_and(|ratio| !(1..=MAX_SEED_RATIO).contains(&ratio))
        {
            return Err("Seed ratio must be between 1 and 1,000,000 thousandths, or null".into());
        }
        if self
            .seed_time_secs
            .is_some_and(|seconds| !(1..=MAX_SEED_TIME).contains(&seconds))
        {
            return Err("Seed time must be between 1 and 315,360,000 seconds, or null".into());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        object([
            (
                "download_limit_bps",
                Value::Number(self.download_limit_bps as f64),
            ),
            (
                "upload_limit_bps",
                Value::Number(self.upload_limit_bps as f64),
            ),
            (
                "seed_ratio_milli",
                self.seed_ratio_milli
                    .map_or(Value::Null, |ratio| Value::Number(f64::from(ratio))),
            ),
            (
                "seed_time_secs",
                self.seed_time_secs
                    .map_or(Value::Null, |seconds| Value::Number(seconds as f64)),
            ),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = strict_fields(
            value,
            &[
                "download_limit_bps",
                "upload_limit_bps",
                "seed_ratio_milli",
                "seed_time_secs",
            ],
        )?;
        let rate = |key| {
            fields
                .get(key)
                .map_or(Ok(0), |value| numeric_u64(value, key))
        };
        let nullable = |key| match fields.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => numeric_u64(value, key).map(Some),
        };
        let policy = Self {
            download_limit_bps: rate("download_limit_bps")?,
            upload_limit_bps: rate("upload_limit_bps")?,
            seed_ratio_milli: nullable("seed_ratio_milli")?
                .map(u32::try_from)
                .transpose()
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
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::High => 2,
        }
    }

    pub fn to_json(self) -> Value {
        Value::String(
            match self {
                Self::Low => "low",
                Self::Normal => "normal",
                Self::High => "high",
            }
            .into(),
        )
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        match value.as_str() {
            Some("low") => Ok(Self::Low),
            Some("normal") => Ok(Self::Normal),
            Some("high") => Ok(Self::High),
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
            id,
            queue_order,
            priority: 0,
            user_paused: false,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            seed_elapsed_secs: 0,
            seed_limited: false,
            file_priorities: BTreeMap::new(),
            policy: None,
        };
        control.validate()?;
        Ok(control)
    }

    pub fn validate(&self) -> Result<()> {
        if !matches!(self.id.len(), 40 | 64)
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("Transfer control ID must be a lowercase torrent hash".into());
        }
        if self.queue_order == 0 || !(-1_000..=1_000).contains(&self.priority) {
            return Err("Transfer queue order must be positive and priority must be between -1,000 and 1,000".into());
        }
        if self.file_priorities.len() > MAX_FILES
            || self.file_priorities.keys().any(|index| *index >= MAX_FILES)
        {
            return Err("Transfer file priorities exceed the 100,000-file bound".into());
        }
        if let Some(policy) = &self.policy {
            policy.validate()?;
        }
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
            (
                "seed_elapsed_secs",
                self.seed_elapsed_secs.to_string().into(),
            ),
            ("seed_limited", self.seed_limited.into()),
            (
                "file_priorities",
                Value::Object(
                    self.file_priorities
                        .iter()
                        .map(|(index, priority)| (index.to_string(), priority.to_json()))
                        .collect(),
                ),
            ),
            (
                "policy",
                self.policy
                    .as_ref()
                    .map_or(Value::Null, TransferPolicy::to_json),
            ),
        ])
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = strict_fields(
            value,
            &[
                "id",
                "queue_order",
                "priority",
                "user_paused",
                "downloaded_bytes",
                "uploaded_bytes",
                "seed_elapsed_secs",
                "seed_limited",
                "file_priorities",
                "policy",
            ],
        )?;
        let priority = required(fields, "priority")?
            .as_i64()
            .and_then(|priority| i32::try_from(priority).ok())
            .ok_or("Transfer queue priority must be a JSON integer")?;
        let Value::Object(priorities) = required(fields, "file_priorities")? else {
            return Err("Transfer file priorities must be an object keyed by file index".into());
        };
        if priorities.len() > MAX_FILES {
            return Err("Too many transfer file priorities".into());
        }
        let mut file_priorities = BTreeMap::new();
        for (index, priority) in priorities {
            let index = decimal_u64(index, "file index")?;
            let index = usize::try_from(index).map_err(|_| "Transfer file index exceeds usize")?;
            if index >= MAX_FILES {
                return Err("Transfer file index exceeds the 100,000-file bound".into());
            }
            file_priorities.insert(index, FilePriority::from_json(priority)?);
        }
        let control = Self {
            id: required(fields, "id")?
                .as_str()
                .ok_or("Transfer control ID must be a string")?
                .into(),
            queue_order: exact_u64(required(fields, "queue_order")?, "queue_order")?,
            priority,
            user_paused: boolean(fields, "user_paused")?,
            downloaded_bytes: exact_u64(required(fields, "downloaded_bytes")?, "downloaded_bytes")?,
            uploaded_bytes: exact_u64(required(fields, "uploaded_bytes")?, "uploaded_bytes")?,
            seed_elapsed_secs: exact_u64(
                required(fields, "seed_elapsed_secs")?,
                "seed_elapsed_secs",
            )?,
            seed_limited: boolean(fields, "seed_limited")?,
            file_priorities,
            policy: match required(fields, "policy")? {
                Value::Null => None,
                value => Some(TransferPolicy::from_json(value)?),
            },
        };
        control.validate()?;
        Ok(control)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let payload = json::stringify(&self.to_json()).into_bytes();
        if payload.len() > MAX_CONTROL_BYTES {
            return Err("Transfer control record exceeds 4 MiB".into());
        }
        let mut encoded = Vec::with_capacity(payload.len() + 44);
        encoded.extend_from_slice(CONTROL_MAGIC);
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&sha256(&encoded));
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        if !(44..=MAX_CONTROL_BYTES + 44).contains(&encoded.len()) || &encoded[..8] != CONTROL_MAGIC
        {
            return Err("Invalid transfer control record size or version".into());
        }
        let length = u32::from_le_bytes(
            encoded[8..12]
                .try_into()
                .map_err(|_| "Invalid transfer control length")?,
        ) as usize;
        if length > MAX_CONTROL_BYTES || length.checked_add(44) != Some(encoded.len()) {
            return Err("Invalid transfer control payload length".into());
        }
        if sha256(&encoded[..encoded.len() - 32]).as_slice() != &encoded[encoded.len() - 32..] {
            return Err("Transfer control checksum mismatch".into());
        }
        Self::from_json(&json::parse(
            std::str::from_utf8(&encoded[12..12 + length])
                .map_err(|_| "Transfer control payload is not UTF-8")?,
        )?)
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
        Self {
            rate,
            credit: u128::from(BLOCK_BYTES) * NANOS_PER_SECOND,
            at_nanos: 0,
        }
    }

    fn advance(&mut self, now_nanos: u128) {
        let accrued = now_nanos
            .saturating_sub(self.at_nanos)
            .saturating_mul(u128::from(self.rate));
        self.credit = self
            .credit
            .saturating_add(accrued)
            .min(u128::from(BLOCK_BYTES) * NANOS_PER_SECOND);
        self.at_nanos = self.at_nanos.max(now_nanos);
    }

    fn wait_for(&mut self, bytes: u64, now_nanos: u128) -> Option<Duration> {
        self.advance(now_nanos);
        if self.rate == 0 {
            return None;
        }
        let required = u128::from(bytes) * NANOS_PER_SECOND;
        if self.credit >= required {
            None
        } else {
            let wait = (required - self.credit).div_ceil(u128::from(self.rate));
            Some(Duration::from_nanos(wait as u64))
        }
    }

    fn consume(&mut self, bytes: u64) {
        if self.rate != 0 {
            self.credit -= u128::from(bytes) * NANOS_PER_SECOND;
        }
    }

    fn take(&mut self, bytes: u64, now_nanos: u128) -> Option<Duration> {
        let wait = self.wait_for(bytes, now_nanos);
        if wait.is_none() {
            self.consume(bytes);
        }
        wait
    }

    fn take_pair(
        first: &mut Self,
        second: &mut Self,
        bytes: u64,
        first_nanos: u128,
        second_nanos: u128,
    ) -> Option<Duration> {
        let first_wait = first.wait_for(bytes, first_nanos);
        let second_wait = second.wait_for(bytes, second_nanos);
        match (first_wait, second_wait) {
            (None, None) => {
                first.consume(bytes);
                second.consume(bytes);
                None
            }
            (Some(wait), None) | (None, Some(wait)) => Some(wait),
            (Some(first), Some(second)) => Some(first.max(second)),
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

struct GateWaiter<'a> {
    gate: &'a RateGate,
    ticket: u64,
    active: bool,
}

impl GateWaiter<'_> {
    fn finish(&mut self, state: &mut GateState) {
        state.waiters.retain(|waiting| *waiting != self.ticket);
        self.active = false;
    }
}

impl Drop for GateWaiter<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // Recover a poisoned mutex only to remove this reservation's ticket.
        // Each cleanup holds one mutex, so either gate can fail independently.
        let gate = self.gate;
        let mut state = gate.state.lock().unwrap_or_else(|error| error.into_inner());
        self.finish(&mut state);
        drop(state);
        gate.changed.notify_all();
    }
}

impl Default for RateGate {
    fn default() -> Self {
        Self::with_rate(0)
    }
}

impl RateGate {
    fn with_rate(limit: u64) -> Self {
        Self {
            limit: AtomicU64::new(limit),
            epoch: Instant::now(),
            changed: Condvar::new(),
            state: Mutex::new(GateState {
                bucket: TokenBucket::new(limit),
                next_ticket: 0,
                waiters: VecDeque::new(),
            }),
        }
    }

    pub fn new(limit: u64) -> Result<Self> {
        if limit > MAX_RATE {
            return Err("Payload rate exceeds 1,073,741,824 bytes per second".into());
        }
        Ok(Self::with_rate(limit))
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Acquire)
    }

    pub fn set_limit(&self, limit: u64) -> Result<()> {
        if limit > MAX_RATE {
            return Err("Payload rate exceeds 1,073,741,824 bytes per second".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Payload rate gate is unavailable after a panic")?;
        state.bucket.advance(self.epoch.elapsed().as_nanos());
        state.bucket.rate = limit;
        self.limit.store(limit, Ordering::Release);
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    #[cfg(test)]
    pub fn reserve(&self, bytes: u64, cancel: &AtomicBool) -> Result<()> {
        if bytes > BLOCK_BYTES {
            return Err("A payload reservation cannot exceed one 16 KiB block".into());
        }
        if cancel.load(Ordering::Acquire) {
            return Err("Payload reservation cancelled".into());
        }
        if bytes == 0 || self.limit() == 0 {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Payload rate gate is unavailable after a panic")?;
        if state.waiters.len() >= MAX_WAITERS {
            return Err("Payload rate gate waiter capacity reached".into());
        }
        let ticket = state.next_ticket;
        state.next_ticket = state
            .next_ticket
            .checked_add(1)
            .ok_or("Payload rate gate ticket capacity reached")?;
        state.waiters.push_back(ticket);
        loop {
            if cancel.load(Ordering::Acquire) || self.limit() == 0 {
                state.waiters.retain(|waiting| *waiting != ticket);
                self.changed.notify_all();
                return if cancel.load(Ordering::Acquire) {
                    Err("Payload reservation cancelled".into())
                } else {
                    Ok(())
                };
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
            } else {
                CANCELLATION_INTERVAL
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .map_err(|_| "Payload rate gate is unavailable after a panic")?
                .0;
        }
    }

    /// Reserve one payload block against both gates without banking credits in
    /// either gate while waiting for the other. An unlimited gate keeps its FIFO
    /// ticket while the other gate waits, so enabling its limit applies to this
    /// reservation. Both-unlimited calls admit immediately without locking.
    #[cfg(test)]
    pub fn reserve_pair(
        first: &Self,
        second: &Self,
        bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        Self::reserve_pair_with(first, second, bytes, cancel, || Ok(()))
    }

    /// Run bounded caller work after each wait, with both gate mutexes released.
    /// Callback errors and cancellation remove both FIFO tickets before return.
    pub fn reserve_pair_with(
        first: &Self,
        second: &Self,
        bytes: u64,
        cancel: &AtomicBool,
        mut while_waiting: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if bytes > BLOCK_BYTES {
            return Err("A payload reservation cannot exceed one 16 KiB block".into());
        }
        if cancel.load(Ordering::Acquire) {
            return Err("Payload reservation cancelled".into());
        }
        if bytes == 0 || (first.limit() == 0 && second.limit() == 0) {
            return Ok(());
        }
        if std::ptr::eq(first, second) {
            return first.reserve_with(bytes, cancel, while_waiting);
        }
        // Every pair acquires locks in address order, including reversed calls.
        let (first, second) = if std::ptr::from_ref(first) < std::ptr::from_ref(second) {
            (first, second)
        } else {
            (second, first)
        };
        let (mut first_waiter, mut second_waiter) = {
            let mut first_state = first
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            let mut second_state = second
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            if first_state.waiters.len() >= MAX_WAITERS || second_state.waiters.len() >= MAX_WAITERS
            {
                return Err("Payload rate gate waiter capacity reached".into());
            }
            let first_ticket = first_state.next_ticket;
            let second_ticket = second_state.next_ticket;
            let first_next = first_ticket
                .checked_add(1)
                .ok_or("Payload rate gate ticket capacity reached")?;
            let second_next = second_ticket
                .checked_add(1)
                .ok_or("Payload rate gate ticket capacity reached")?;
            // Preflight both queues before changing either. Atomic registration
            // gives overlapping pairs the same order in their shared queues.
            first_state.next_ticket = first_next;
            second_state.next_ticket = second_next;
            first_state.waiters.push_back(first_ticket);
            second_state.waiters.push_back(second_ticket);
            (
                GateWaiter {
                    gate: first,
                    ticket: first_ticket,
                    active: true,
                },
                GateWaiter {
                    gate: second,
                    ticket: second_ticket,
                    active: true,
                },
            )
        };
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err("Payload reservation cancelled".into());
            }
            let mut first_state = first
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            let mut second_state = second
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            if cancel.load(Ordering::Acquire) {
                return Err("Payload reservation cancelled".into());
            }
            let first_ready = first_state.bucket.rate == 0
                || first_state.waiters.front() == Some(&first_waiter.ticket);
            let second_ready = second_state.bucket.rate == 0
                || second_state.waiters.front() == Some(&second_waiter.ticket);
            let wait = if first_ready && second_ready {
                match TokenBucket::take_pair(
                    &mut first_state.bucket,
                    &mut second_state.bucket,
                    bytes,
                    first.epoch.elapsed().as_nanos(),
                    second.epoch.elapsed().as_nanos(),
                ) {
                    None => {
                        first_waiter.finish(&mut first_state);
                        second_waiter.finish(&mut second_state);
                        drop(second_state);
                        drop(first_state);
                        first.changed.notify_all();
                        second.changed.notify_all();
                        return Ok(());
                    }
                    Some(wait) => wait.min(CANCELLATION_INTERVAL),
                }
            } else {
                CANCELLATION_INTERVAL
            };
            // The condvar releases the first mutex while sleeping. The second
            // is dropped first; updates on its condvar are seen within 100 ms.
            drop(second_state);
            let (first_state, _) = first
                .changed
                .wait_timeout(first_state, wait)
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            drop(first_state);
            if cancel.load(Ordering::Acquire) {
                return Err("Payload reservation cancelled".into());
            }
            while_waiting()?;
        }
    }

    fn reserve_with(
        &self,
        bytes: u64,
        cancel: &AtomicBool,
        mut while_waiting: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut waiter = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            if state.waiters.len() >= MAX_WAITERS {
                return Err("Payload rate gate waiter capacity reached".into());
            }
            let ticket = state.next_ticket;
            state.next_ticket = ticket
                .checked_add(1)
                .ok_or("Payload rate gate ticket capacity reached")?;
            state.waiters.push_back(ticket);
            GateWaiter {
                gate: self,
                ticket,
                active: true,
            }
        };
        loop {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            if cancel.load(Ordering::Acquire) {
                return Err("Payload reservation cancelled".into());
            }
            if state.bucket.rate == 0 {
                waiter.finish(&mut state);
                drop(state);
                self.changed.notify_all();
                return Ok(());
            }
            let wait = if state.waiters.front() == Some(&waiter.ticket) {
                match state.bucket.take(bytes, self.epoch.elapsed().as_nanos()) {
                    None => {
                        waiter.finish(&mut state);
                        drop(state);
                        self.changed.notify_all();
                        return Ok(());
                    }
                    Some(wait) => wait.min(CANCELLATION_INTERVAL),
                }
            } else {
                CANCELLATION_INTERVAL
            };
            let (state, _) = self
                .changed
                .wait_timeout(state, wait)
                .map_err(|_| "Payload rate gate is unavailable after a panic")?;
            drop(state);
            if cancel.load(Ordering::Acquire) {
                return Err("Payload reservation cancelled".into());
            }
            while_waiting()?;
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
        assert_eq!(
            TransferPolicy::from_json(&Value::object()).unwrap(),
            TransferPolicy::default()
        );
        let policy = TransferPolicy {
            download_limit_bps: MAX_RATE,
            upload_limit_bps: 1,
            seed_ratio_milli: Some(MAX_SEED_RATIO),
            seed_time_secs: Some(MAX_SEED_TIME),
        };
        assert_eq!(
            TransferPolicy::from_json(&policy.to_json()).unwrap(),
            policy
        );
        for field in [
            "download_limit_bps",
            "upload_limit_bps",
            "seed_ratio_milli",
            "seed_time_secs",
        ] {
            for invalid in [
                Value::Bool(true),
                Value::String("1".into()),
                Value::Number(-1.0),
                Value::Number(1.5),
            ] {
                let mut value = Value::object();
                value.insert(field, invalid);
                assert!(
                    TransferPolicy::from_json(&value).is_err(),
                    "{field} must reject invalid JSON types"
                );
            }
        }
        for (field, invalid) in [
            ("download_limit_bps", MAX_RATE + 1),
            ("upload_limit_bps", MAX_RATE + 1),
            ("seed_ratio_milli", 0),
            ("seed_ratio_milli", u64::from(MAX_SEED_RATIO) + 1),
            ("seed_time_secs", 0),
            ("seed_time_secs", MAX_SEED_TIME + 1),
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
        original.file_priorities =
            BTreeMap::from([(0, FilePriority::High), (99_999, FilePriority::Low)]);
        original.policy = Some(TransferPolicy {
            upload_limit_bps: 32_768,
            seed_ratio_milli: Some(1_500),
            ..TransferPolicy::default()
        });
        let encoded = original.encode().unwrap();
        assert_eq!(TorrentControl::decode(&encoded).unwrap(), original);
        assert_eq!(
            TorrentControl::from_json(&original.to_json()).unwrap(),
            original
        );
        assert_eq!(
            original
                .to_json()
                .get("downloaded_bytes")
                .and_then(Value::as_str),
            Some("18446744073709551615")
        );
        assert!(
            !original
                .to_json()
                .as_object()
                .unwrap()
                .contains_key("ready"),
            "accounting does not assert completion"
        );
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
        for id in [
            String::new(),
            "A".repeat(40),
            "../unsafe".into(),
            "a".repeat(41),
        ] {
            assert!(TorrentControl::new(id, 1).is_err());
        }
        assert!(TorrentControl::new("0".repeat(40), 1).is_ok());
        assert!(TorrentControl::new("a".repeat(64), 0).is_err());
        for counter in [
            "queue_order",
            "downloaded_bytes",
            "uploaded_bytes",
            "seed_elapsed_secs",
        ] {
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
        assert!(
            FilePriority::Low < FilePriority::Normal && FilePriority::Normal < FilePriority::High
        );
        assert!(FilePriority::from_json(&Value::Number(2.0)).is_err());
    }

    #[test]
    fn fake_time_bucket_shares_one_block_burst_and_exact_refill() {
        let mut bucket = TokenBucket::new(1_024);
        assert_eq!(bucket.take(BLOCK_BYTES, 0), None);
        assert_eq!(bucket.take(1_024, 0), Some(Duration::from_secs(1)));
        assert_eq!(
            bucket.take(1_024, 250_000_000),
            Some(Duration::from_millis(750))
        );
        assert_eq!(bucket.take(1_024, NANOS_PER_SECOND), None);
        // Another peer cannot consume the credits just spent by the first.
        assert_eq!(
            bucket.take(1_024, NANOS_PER_SECOND),
            Some(Duration::from_secs(1))
        );
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
        assert_eq!(
            bucket.take(1, NANOS_PER_SECOND),
            Some(Duration::from_nanos(166_666_667))
        );
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
    fn fake_time_pair_wait_does_not_spend_the_other_bucket() {
        let mut first = TokenBucket::new(1_024);
        let mut second = TokenBucket::new(1_024);
        assert_eq!(second.take(BLOCK_BYTES, 0), None);
        let burst = u128::from(BLOCK_BYTES) * NANOS_PER_SECOND;
        assert_eq!(
            TokenBucket::take_pair(&mut first, &mut second, 1_024, 0, 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(first.credit, burst);
        assert_eq!(second.credit, 0);
        assert_eq!(
            TokenBucket::take_pair(&mut second, &mut first, 1_024, 0, 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            first.credit, burst,
            "neither parameter order may spend the ready bucket"
        );
        assert_eq!(
            TokenBucket::take_pair(
                &mut first,
                &mut second,
                1_024,
                NANOS_PER_SECOND,
                NANOS_PER_SECOND
            ),
            None
        );
        assert_eq!(
            first.credit,
            u128::from(BLOCK_BYTES - 1_024) * NANOS_PER_SECOND
        );
        assert_eq!(second.credit, 0);
    }

    #[test]
    fn fake_time_pair_backlog_cannot_bank_local_credits_for_a_rapid_release() {
        let mut aggregate = TokenBucket::new(BLOCK_BYTES);
        let mut local = TokenBucket::new(1_024);
        let burst = u128::from(BLOCK_BYTES) * NANOS_PER_SECOND;
        assert_eq!(aggregate.take(BLOCK_BYTES, 0), None);
        // Other torrents keep the aggregate bucket empty while 32 local upload
        // callers are queued. Every failed joint probe must preserve local credit.
        for second in 1_u128..=32 {
            let now = second * NANOS_PER_SECOND;
            assert_eq!(aggregate.take(BLOCK_BYTES, now), None);
            for _ in 0..32 {
                assert_eq!(
                    TokenBucket::take_pair(&mut aggregate, &mut local, BLOCK_BYTES, now, now),
                    Some(Duration::from_secs(1))
                );
                assert_eq!(local.credit, burst);
            }
        }
        let release = 33 * NANOS_PER_SECOND;
        assert_eq!(
            TokenBucket::take_pair(&mut aggregate, &mut local, BLOCK_BYTES, release, release),
            None
        );
        assert_eq!(local.credit, 0);
        aggregate.rate = MAX_RATE;
        // The aggregate queue clears and credits refill quickly, but only one
        // shared local burst was admitted. The queued callers still need refill.
        for millisecond in 1_u128..=32 {
            let now = release + millisecond * 1_000_000;
            assert!(
                TokenBucket::take_pair(&mut aggregate, &mut local, BLOCK_BYTES, now, now).is_some()
            );
            assert_eq!(
                local.credit,
                u128::from(1_024_u64) * millisecond * 1_000_000
            );
        }
        let local_refilled = release + 16 * NANOS_PER_SECOND;
        assert_eq!(
            TokenBucket::take_pair(
                &mut aggregate,
                &mut local,
                BLOCK_BYTES,
                local_refilled,
                local_refilled
            ),
            None
        );
        assert_eq!(local.credit, 0);
    }

    #[test]
    fn paired_reservations_validate_and_charge_an_identical_gate_once() {
        let gate = RateGate::new(1).unwrap();
        let cancel = AtomicBool::new(false);
        RateGate::reserve_pair(&gate, &gate, BLOCK_BYTES, &cancel).unwrap();
        let state = gate.state.lock().unwrap();
        assert_eq!(state.bucket.credit, 0);
        assert!(state.waiters.is_empty());
        drop(state);
        assert!(RateGate::reserve_pair(&gate, &gate, BLOCK_BYTES + 1, &cancel).is_err());
        assert!(
            RateGate::reserve_pair(&gate, &gate, 1, &AtomicBool::new(true))
                .unwrap_err()
                .contains("cancelled")
        );
    }

    #[test]
    fn paired_queue_capacity_and_ticket_overflow_leave_both_queues_unchanged() {
        let first = RateGate::new(1).unwrap();
        let second = RateGate::new(1).unwrap();
        {
            let mut state = second.state.lock().unwrap();
            state.waiters.extend(0..MAX_WAITERS as u64);
            state.next_ticket = MAX_WAITERS as u64;
        }
        assert!(
            RateGate::reserve_pair(&first, &second, 1, &AtomicBool::new(false))
                .unwrap_err()
                .contains("waiter capacity")
        );
        assert!(first.state.lock().unwrap().waiters.is_empty());
        {
            let mut state = second.state.lock().unwrap();
            assert_eq!(state.waiters.len(), MAX_WAITERS);
            state.waiters.clear();
            state.next_ticket = u64::MAX;
        }
        assert!(
            RateGate::reserve_pair(&second, &first, 1, &AtomicBool::new(false))
                .unwrap_err()
                .contains("ticket capacity")
        );
        let state = first.state.lock().unwrap();
        assert!(state.waiters.is_empty());
        assert_eq!(state.next_ticket, 0);
        assert!(second.state.lock().unwrap().waiters.is_empty());
    }

    #[test]
    fn paired_callback_runs_without_locks_and_errors_clean_both_queues() {
        let first = RateGate::new(1).unwrap();
        let second = RateGate::new(1).unwrap();
        first.reserve(BLOCK_BYTES, &AtomicBool::new(false)).unwrap();
        let error = RateGate::reserve_pair_with(
            &first,
            &second,
            BLOCK_BYTES,
            &AtomicBool::new(false),
            || {
                assert!(first.state.try_lock().is_ok());
                assert!(second.state.try_lock().is_ok());
                Err("Caller keepalive failed".into())
            },
        )
        .unwrap_err();
        assert_eq!(error, "Caller keepalive failed");
        assert!(first.state.lock().unwrap().waiters.is_empty());
        let state = second.state.lock().unwrap();
        assert!(state.waiters.is_empty());
        assert_eq!(
            state.bucket.credit,
            u128::from(BLOCK_BYTES) * NANOS_PER_SECOND
        );
    }

    #[test]
    fn paired_wait_applies_a_new_limit_to_an_initially_unlimited_gate() {
        let aggregate = RateGate::new(1).unwrap();
        let local = RateGate::default();
        aggregate
            .reserve(BLOCK_BYTES, &AtomicBool::new(false))
            .unwrap();
        let cancel = AtomicBool::new(false);
        let mut callbacks = 0;
        let error = RateGate::reserve_pair_with(&aggregate, &local, BLOCK_BYTES, &cancel, || {
            callbacks += 1;
            if callbacks == 1 {
                assert_eq!(local.state.lock().unwrap().waiters.len(), 1);
                local.set_limit(1).unwrap();
                // Simulate an empty local burst so the newly enabled gate must
                // keep this pair waiting after the aggregate cap is removed.
                local.state.lock().unwrap().bucket.credit = 0;
                aggregate.set_limit(0).unwrap();
            } else {
                cancel.store(true, Ordering::Release);
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("cancelled"));
        assert_eq!(
            callbacks, 2,
            "the new local cap must keep this reservation waiting"
        );
        assert!(aggregate.state.lock().unwrap().waiters.is_empty());
        assert!(local.state.lock().unwrap().waiters.is_empty());
    }

    #[test]
    fn paired_wait_cancellation_removes_both_fifo_tickets() {
        let first = Arc::new(RateGate::new(1).unwrap());
        let second = Arc::new(RateGate::new(1).unwrap());
        let cancel = Arc::new(AtomicBool::new(false));
        first.reserve(BLOCK_BYTES, &cancel).unwrap();
        second.reserve(BLOCK_BYTES, &cancel).unwrap();
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker_first = first.clone();
        let worker_second = second.clone();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            let mut waiting_tx = Some(waiting_tx);
            let result = RateGate::reserve_pair_with(
                &worker_second,
                &worker_first,
                BLOCK_BYTES,
                &worker_cancel,
                || {
                    if let Some(sender) = waiting_tx.take() {
                        let _ = sender.send(());
                    }
                    Ok(())
                },
            );
            let _ = result_tx.send(result);
        });
        let entered = waiting_rx.recv_timeout(Duration::from_secs(2));
        cancel.store(true, Ordering::Release);
        let result = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("paired cancellation must finish within the bounded wait");
        worker.join().unwrap();
        entered.expect("paired worker must begin a bounded wait");
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(first.state.lock().unwrap().waiters.is_empty());
        assert!(second.state.lock().unwrap().waiters.is_empty());
    }

    #[test]
    fn cancelled_and_oversized_payload_reservations_never_wait() {
        let gate = RateGate::new(1).unwrap();
        assert!(
            gate.reserve(BLOCK_BYTES, &AtomicBool::new(true))
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(
            gate.reserve(BLOCK_BYTES + 1, &AtomicBool::new(false))
                .is_err()
        );
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
        assert!(
            std::thread::spawn(move || {
                let _guard = worker.state.lock().unwrap();
                panic!("intentional mutex poisoning for the unlimited-path regression");
            })
            .join()
            .is_err()
        );
        gate.reserve(BLOCK_BYTES, &AtomicBool::new(false)).unwrap();
        assert!(
            gate.set_limit(1).is_err(),
            "a poisoned limited gate must fail explicitly"
        );
    }

    #[test]
    fn unlimited_pair_hot_path_does_not_acquire_either_mutex() {
        let first = Arc::new(RateGate::default());
        let second = Arc::new(RateGate::default());
        for gate in [&first, &second] {
            let gate = gate.clone();
            assert!(
                std::thread::spawn(move || {
                    let _guard = gate.state.lock().unwrap();
                    panic!("intentional mutex poisoning for the unlimited pair regression");
                })
                .join()
                .is_err()
            );
        }
        RateGate::reserve_pair(&first, &second, BLOCK_BYTES, &AtomicBool::new(false)).unwrap();
        RateGate::reserve_pair_with(
            &first,
            &second,
            BLOCK_BYTES,
            &AtomicBool::new(false),
            || panic!("unlimited reservations never wait"),
        )
        .unwrap();
    }
}
