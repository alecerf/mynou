//! A generation owns its peer workers, exclusive piece claims, and verified writes.

use super::{
    DownloadConfig, Jobs, Meta, PolicyRuntime, Source, TransferCounters, authenticate_v2,
    discovery, mkdir_private, persist_meta, prepare_files, read_piece, update,
    verified_piece_bytes, wire, write_piece,
};
use crate::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// This bounds the additional worker-owned metadata and payload buffers. The
// canonical metadata, application state, and operating-system socket buffers
// are separate. A single large metadata object is still permitted at limit one.
const WORKER_MEMORY_BUDGET: usize = 128 * 1024 * 1024;
const WIRE_MEMORY_ALLOWANCE: usize = 2 * (super::MAX_META + 1024);
const CONNECTION_BUDGET: usize = 64;
const POLL: Duration = Duration::from_millis(20);
const RETRY: Duration = Duration::from_secs(2);

fn peer_limit(config: &DownloadConfig, meta: &Meta) -> usize {
    let strings = meta
        .files
        .iter()
        .map(|file| file.path.as_os_str().len())
        .chain(meta.trackers.iter().map(String::len))
        .fold(0usize, usize::saturating_add);
    let metadata = meta
        .info
        .len()
        .saturating_add(meta.encoded.len())
        .saturating_add(
            meta.pieces
                .len()
                .saturating_mul(size_of::<Option<[u8; 20]>>()),
        )
        .saturating_add(
            meta.v2_pieces
                .len()
                .saturating_mul(size_of::<Option<[u8; 32]>>()),
        )
        .saturating_add(
            meta.files
                .len()
                .saturating_mul(size_of::<super::metainfo::MediaFile>()),
        )
        .saturating_add(meta.trackers.len().saturating_mul(size_of::<String>()))
        .saturating_add(strings);
    let footprint = metadata
        .saturating_add(meta.piece_length)
        .saturating_add(WIRE_MEMORY_ALLOWANCE)
        .max(1);
    config
        .max_peers
        .min((CONNECTION_BUDGET / config.max_active).max(1))
        .min((WORKER_MEMORY_BUDGET / footprint).max(1))
}

fn current_job<'a>(
    map: &'a mut BTreeMap<String, super::Job>,
    stop: &AtomicBool,
    id: &str,
) -> Result<&'a mut super::Job> {
    let job = map.get_mut(id).ok_or("Download no longer exists")?;
    if stop.load(Ordering::Acquire)
        || job.paused
        || job.control.user_paused
        || !std::ptr::eq(&*job.cancel, stop)
    {
        return Err("Download interrupted".into());
    }
    Ok(job)
}

// Pause and generation retirement use the same mutex. They cannot return while
// a verified file write or metadata publication by the retired generation runs.
fn with_generation<T>(
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    operation: impl FnOnce(&mut super::Job) -> Result<T>,
) -> Result<T> {
    let mut map = jobs
        .lock()
        .map_err(|_| "BitTorrent state lock is poisoned")?;
    operation(current_job(&mut map, stop, id)?)
}

enum Command {
    Piece(usize),
}

enum EventKind {
    Connected {
        availability: Option<Vec<u8>>,
        peers: Vec<SocketAddr>,
    },
    Piece {
        index: usize,
        data: Vec<u8>,
        proofs: Vec<(usize, [u8; 32])>,
        availability: Option<Vec<u8>>,
        peers: Vec<SocketAddr>,
    },
    Failed {
        corrupt: bool,
    },
}

struct Event {
    slot: usize,
    ticket: u64,
    kind: EventKind,
}

struct Worker {
    address: SocketAddr,
    ticket: u64,
    commands: SyncSender<Command>,
    stop: Arc<AtomicBool>,
    corrupt: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    // None before handshake; Some(None) for unknown availability.
    availability: Option<Option<Vec<u8>>>,
    claim: Option<usize>,
    idle_since: Instant,
}

struct Group {
    workers: Vec<Option<Worker>>,
    events: Option<Receiver<Event>>,
    discovery: Option<JoinHandle<Vec<SocketAddr>>>,
    discovery_stop: Arc<AtomicBool>,
}

impl Group {
    fn retire(&mut self, slot: usize) -> Option<(SocketAddr, bool)> {
        let worker = self.workers[slot].take()?;
        worker.stop.store(true, Ordering::Release);
        let address = worker.address;
        drop(worker.commands);
        let _ = worker.handle.join();
        Some((address, worker.corrupt.load(Ordering::Acquire)))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        // Signal everyone before joining anyone, including a worker whose
        // bounded result send is waiting for its receiver to make progress.
        self.discovery_stop.store(true, Ordering::Release);
        for worker in self.workers.iter().flatten() {
            worker.stop.store(true, Ordering::Release);
        }
        drop(self.events.take());
        for slot in 0..self.workers.len() {
            let _ = self.retire(slot);
        }
        if let Some(handle) = self.discovery.take() {
            let _ = handle.join();
        }
    }
}

fn proof_pair(meta: &Meta, piece: usize) -> Vec<usize> {
    let Some(file) = meta.v2_file(piece) else {
        return Vec::new();
    };
    let start = (file.offset / meta.piece_length as u64) as usize;
    let count = file.length.div_ceil(meta.piece_length as u64) as usize;
    let first = start + ((piece - start) & !1);
    (first..(first + 2).min(start + count)).collect()
}

fn merge_proofs(meta: &mut Meta, piece: usize, proofs: &[(usize, [u8; 32])]) -> Result<()> {
    let allowed = proof_pair(meta, piece);
    if proofs.len() > 2
        || proofs.iter().any(|(index, hash)| {
            !allowed.contains(index)
                || meta.v2_pieces[*index].is_some_and(|expected| expected != *hash)
        })
    {
        return Err("Conflicting authenticated v2 piece proof".into());
    }
    for (index, hash) in proofs {
        meta.v2_pieces[*index] = Some(*hash);
    }
    Ok(())
}

struct PeerTask {
    slot: usize,
    ticket: u64,
    address: SocketAddr,
    hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    pex: bool,
    v2_wire: bool,
    meta: Meta,
    counters: Arc<TransferCounters>,
    download_gate: Arc<super::RateGate>,
    local_download_gate: Arc<super::RateGate>,
    stop: Arc<AtomicBool>,
    corrupt: Arc<AtomicBool>,
    commands: Receiver<Command>,
    events: SyncSender<Event>,
}

fn send_event(events: &SyncSender<Event>, stop: &AtomicBool, mut event: Event) -> Result<()> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("Peer coordinator stopped".to_owned());
        }
        match events.try_send(event) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err("Peer coordinator stopped".to_owned());
            }
            Err(mpsc::TrySendError::Full(pending)) => event = pending,
        }
        thread::sleep(POLL);
    }
}

fn peer_worker(mut task: PeerTask) {
    let send = |kind| {
        send_event(
            &task.events,
            &task.stop,
            Event {
                slot: task.slot,
                ticket: task.ticket,
                kind,
            },
        )
    };
    let outcome = (|| -> Result<()> {
        let mut peer = wire::Peer::connect_cancellable(
            task.address,
            &task.hash,
            &task.peer_id,
            wire::PeerSettings {
                meta: Some(&task.meta),
                port: task.port,
                pex: task.pex,
                v2_wire: task.v2_wire,
                counters: task.counters.clone(),
                download_gate: task.download_gate.clone(),
                local_download_gate: task.local_download_gate.clone(),
            },
            &task.stop,
        )?;
        peer.prepare_download(&task.meta, &task.stop)?;
        send(EventKind::Connected {
            availability: peer.availability().map(<[u8]>::to_vec),
            peers: std::mem::take(&mut peer.discovered),
        })?;
        let mut keepalive = Instant::now();
        while !task.stop.load(Ordering::Acquire) {
            let command = match task.commands.recv_timeout(POLL) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if keepalive.elapsed() >= Duration::from_secs(5) {
                        wire::write_all_deadline(
                            &mut peer.stream,
                            &[0; 4],
                            Some(&task.stop),
                            Instant::now() + Duration::from_secs(5),
                        )?;
                        keepalive = Instant::now();
                    }
                    continue;
                }
            };
            let Command::Piece(index) = command;
            let pair = proof_pair(&task.meta, index);
            let before: Vec<_> = pair
                .iter()
                .map(|index| task.meta.v2_pieces[*index])
                .collect();
            let data = peer.fetch_piece(&mut task.meta, index, &task.stop)?;
            let proofs = pair
                .into_iter()
                .zip(before)
                .filter_map(|(index, before)| {
                    task.meta.v2_pieces[index]
                        .filter(|hash| Some(*hash) != before)
                        .map(|hash| (index, hash))
                })
                .collect();
            send(EventKind::Piece {
                index,
                data,
                proofs,
                availability: peer.availability().map(<[u8]>::to_vec),
                peers: std::mem::take(&mut peer.discovered),
            })?;
            // The coordinator alone can issue the next command. Each worker
            // therefore owns at most one payload, including queued results.
            keepalive = Instant::now();
        }
        Ok(())
    })();
    if let Err(error) = outcome
        && !task.stop.load(Ordering::Acquire)
    {
        let corrupt = [
            "hash",
            "proof",
            "padding",
            "Invalid",
            "Unsolicited",
            "inconsistent",
            "Bitfield",
            "Have index",
            "repeated bitfield",
        ]
        .iter()
        .any(|marker| error.contains(marker));
        task.corrupt.store(corrupt, Ordering::Release);
        let _ = send(EventKind::Failed { corrupt });
    }
}

fn retire_peer(
    group: &mut Group,
    slot: usize,
    quarantine: &mut BTreeSet<SocketAddr>,
    retries: &mut BTreeMap<SocketAddr, Instant>,
    corrupt_response: bool,
) {
    if let Some((address, corrupt)) = group.retire(slot) {
        if corrupt_response || corrupt {
            quarantine.insert(address);
        } else {
            retries.insert(address, Instant::now() + RETRY);
        }
    }
}

fn select_address(
    addresses: &BTreeSet<SocketAddr>,
    workers: &[Option<Worker>],
    quarantine: &BTreeSet<SocketAddr>,
    retries: &BTreeMap<SocketAddr, Instant>,
    cursor: Option<SocketAddr>,
) -> Option<SocketAddr> {
    let eligible = |address: &&SocketAddr| {
        !quarantine.contains(address)
            && !workers
                .iter()
                .flatten()
                .any(|worker| worker.address == **address)
            && retries
                .get(address)
                .is_none_or(|retry| *retry <= Instant::now())
    };
    addresses
        .iter()
        .filter(eligible)
        .find(|address| cursor.is_none_or(|cursor| **address > cursor))
        .or_else(|| addresses.iter().find(eligible))
        .copied()
}

fn refresh_peers(
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    addresses: &mut BTreeSet<SocketAddr>,
) -> Result<()> {
    with_generation(jobs, stop, id, |job| {
        super::merge_peers(addresses, job.tracker_peers.iter().copied());
        job.peers = addresses.iter().copied().collect();
        Ok(())
    })
}

// Metadata discovery has one owner so a newly authenticated private flag
// cannot race another worker's discovery or PEX publication.
struct MetadataContext<'a> {
    config: &'a DownloadConfig,
    jobs: &'a Jobs,
    stop: &'a AtomicBool,
    id: &'a str,
    peer_id: &'a [u8; 20],
    policy: &'a PolicyRuntime,
    source: &'a Source,
    counters: &'a Arc<TransferCounters>,
}

fn metadata(context: MetadataContext<'_>, addresses: &mut BTreeSet<SocketAddr>) -> Result<Meta> {
    let MetadataContext {
        config,
        jobs,
        stop,
        id,
        peer_id,
        policy,
        source,
        counters,
    } = context;
    let mut group = Group {
        workers: Vec::new(),
        events: None,
        discovery: None,
        discovery_stop: Arc::new(AtomicBool::new(false)),
    };
    let mut cursor = None;
    let mut retries = BTreeMap::new();
    let mut quarantine = BTreeSet::new();
    let hash = source.wire_hash();
    let mut next_dht = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        refresh_peers(jobs, stop, id, addresses)?;
        if group
            .discovery
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
            && let Some(handle) = group.discovery.take()
            && let Ok(peers) = handle.join()
        {
            super::merge_peers(addresses, peers);
        }
        // Known addresses are attempted immediately; no DHT lookup occupies
        // this coordinator. Unknown privacy may query only without hints.
        if config.dht
            && source.trackers.is_empty()
            && source.peers.is_empty()
            && group.discovery.is_none()
            && Instant::now() >= next_dht
        {
            next_dht = Instant::now() + Duration::from_secs(60);
            let cancel = group.discovery_stop.clone();
            let port = config.listen_port;
            group.discovery = Some(thread::spawn(move || {
                discovery::dht_lookup(&hash, port, false, &cancel, &[]).unwrap_or_default()
            }));
        }
        let Some(address) = select_address(addresses, &[], &quarantine, &retries, cursor) else {
            thread::sleep(POLL);
            continue;
        };
        cursor = Some(address);
        let gate = with_generation(jobs, stop, id, |job| Ok(job.download_gate.clone()))?;
        let result = wire::Peer::connect_cancellable(
            address,
            &hash,
            peer_id,
            wire::PeerSettings {
                meta: None,
                port: config.listen_port,
                pex: false,
                v2_wire: source.v1.is_none(),
                counters: counters.clone(),
                download_gate: policy.download.clone(),
                local_download_gate: gate,
            },
            stop,
        )
        .and_then(|mut peer| peer.metadata(source.v1, source.v2, stop));
        match result {
            Ok(meta) => {
                group.discovery_stop.store(true, Ordering::Release);
                if meta.private && source.trackers.is_empty() && source.peers.is_empty() {
                    addresses.clear();
                    // A peer found through unknown-metadata DHT does not become
                    // an accepted direct hint for an authenticated private swarm.
                }
                with_generation(jobs, stop, id, |_| persist_meta(config, id, &meta))?;
                update(
                    (jobs, stop),
                    id,
                    Some(&meta),
                    0.0,
                    false,
                    "Torrent metadata authenticated",
                    config,
                )?;
                return Ok(meta);
            }
            Err(error) => {
                if error.contains("hash") || error.contains("authenticated") {
                    quarantine.insert(address);
                } else {
                    retries.insert(address, Instant::now() + RETRY);
                }
            }
        }
    }
}

pub(super) fn download(
    config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    peer_id: &[u8; 20],
    policy: &PolicyRuntime,
) -> Result<()> {
    let (source, cached, counters, local_gate) = with_generation(jobs, stop, id, |job| {
        Ok((
            Source {
                original: job.source.original.clone(),
                v1: job.source.v1,
                v2: job.source.v2,
                trackers: job.source.trackers.clone(),
                peers: job.source.peers.clone(),
                meta: None,
            },
            job.meta.as_ref().map(|meta| (**meta).clone()),
            job.counters.clone(),
            job.download_gate.clone(),
        ))
    })?;
    let mut addresses = BTreeSet::new();
    super::merge_peers(&mut addresses, source.peers.iter().copied());
    let mut meta = match cached {
        Some(meta) => meta,
        None => metadata(
            MetadataContext {
                config,
                jobs,
                stop,
                id,
                peer_id,
                policy,
                source: &source,
                counters: &counters,
            },
            &mut addresses,
        )?,
    };
    let base = config.data_dir.join(id);
    with_generation(jobs, stop, id, |_| {
        mkdir_private(&base)?;
        prepare_files(&meta, &base, stop)
    })?;
    let mut have: Vec<_> = (0..meta.count())
        .map(|index| {
            !stop.load(Ordering::Acquire)
                && (meta.pieces[index].is_some() || meta.v2_pieces[index].is_some())
                && read_piece(&meta, &base, index).is_ok_and(|data| meta.verify(index, &data))
        })
        .collect();
    with_generation(jobs, stop, id, |_| {
        counters.verified.store(
            have.iter()
                .enumerate()
                .filter(|(_, have)| **have)
                .map(|(index, _)| verified_piece_bytes(&meta, index))
                .sum(),
            Ordering::Relaxed,
        );
        Ok(())
    })?;
    let limit = peer_limit(config, &meta);
    let (events, receiver) = mpsc::sync_channel(limit);
    let mut group = Group {
        workers: (0..limit).map(|_| None).collect(),
        events: Some(receiver),
        discovery: None,
        discovery_stop: Arc::new(AtomicBool::new(false)),
    };
    let mut cursor = None;
    let mut ticket = 0u64;
    let mut retries = BTreeMap::new();
    let mut quarantine = BTreeSet::new();
    let mut next_dht = Instant::now();
    let hash = source.wire_hash();
    let mut last_update = Instant::now() - Duration::from_secs(1);
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if have.iter().all(|have| *have) {
            break;
        }
        refresh_peers(jobs, stop, id, &mut addresses)?;
        if let Some(handle) = group.discovery.as_ref()
            && handle.is_finished()
            && let Some(handle) = group.discovery.take()
            && let Ok(peers) = handle.join()
        {
            super::merge_peers(&mut addresses, peers);
        }
        if config.dht && !meta.private && group.discovery.is_none() && Instant::now() >= next_dht {
            next_dht = Instant::now() + Duration::from_secs(60);
            let cancel = group.discovery_stop.clone();
            let port = config.listen_port;
            group.discovery = Some(thread::spawn(move || {
                discovery::dht_lookup(&hash, port, true, &cancel, &[]).unwrap_or_default()
            }));
        }
        // At most one connection and one exclusive claim exist per slot. A
        // retired or panicking worker loses its claim when its slot is taken.
        for slot in 0..limit {
            if group.workers[slot].is_none()
                && let Some(address) =
                    select_address(&addresses, &group.workers, &quarantine, &retries, cursor)
            {
                cursor = Some(address);
                ticket = ticket
                    .checked_add(1)
                    .ok_or("Peer generation sequence exhausted")?;
                let (commands, command_receiver) = mpsc::sync_channel(1);
                let cancel = Arc::new(AtomicBool::new(false));
                let corrupt = Arc::new(AtomicBool::new(false));
                let task = PeerTask {
                    slot,
                    ticket,
                    address,
                    hash,
                    peer_id: *peer_id,
                    port: config.listen_port,
                    pex: config.pex && !meta.private,
                    v2_wire: source.v1.is_none(),
                    meta: meta.clone(),
                    counters: counters.clone(),
                    download_gate: policy.download.clone(),
                    local_download_gate: local_gate.clone(),
                    stop: cancel.clone(),
                    commands: command_receiver,
                    events: events.clone(),
                    corrupt: corrupt.clone(),
                };
                group.workers[slot] = Some(Worker {
                    address,
                    ticket,
                    commands,
                    stop: cancel,
                    handle: thread::spawn(move || peer_worker(task)),
                    corrupt,
                    availability: None,
                    claim: None,
                    idle_since: Instant::now(),
                });
            }
        }
        let event = group
            .events
            .as_ref()
            .ok_or("Peer result receiver unavailable")?
            .recv_timeout(POLL);
        if let Ok(event) = event
            && group.workers[event.slot]
                .as_ref()
                .is_some_and(|worker| worker.ticket == event.ticket)
        {
            match event.kind {
                EventKind::Connected {
                    availability,
                    peers,
                } => {
                    let worker = group.workers[event.slot]
                        .as_mut()
                        .ok_or("Peer worker unavailable")?;
                    worker.availability = Some(availability);
                    worker.idle_since = Instant::now();
                    if !meta.private && config.pex {
                        super::merge_peers(&mut addresses, peers);
                    }
                }
                EventKind::Piece {
                    index,
                    data,
                    proofs,
                    availability,
                    peers,
                } => {
                    let worker = group.workers[event.slot]
                        .as_mut()
                        .ok_or("Peer worker unavailable")?;
                    if worker.claim != Some(index) {
                        return Err("Peer returned an unclaimed piece".into());
                    }
                    worker.claim = None;
                    worker.availability = Some(availability);
                    worker.idle_since = Instant::now();
                    if merge_proofs(&mut meta, index, &proofs).is_err()
                        || !meta.verify(index, &data)
                    {
                        retire_peer(&mut group, event.slot, &mut quarantine, &mut retries, true);
                    } else {
                        commit_piece(config, jobs, stop, id, &meta, &base, index, &data, &have)?;
                        have[index] = true;
                        if !meta.private && config.pex {
                            super::merge_peers(&mut addresses, peers);
                        }
                    }
                }
                EventKind::Failed { corrupt } => {
                    retire_peer(
                        &mut group,
                        event.slot,
                        &mut quarantine,
                        &mut retries,
                        corrupt,
                    );
                }
            }
        }
        for slot in 0..limit {
            if group.workers[slot]
                .as_ref()
                .is_some_and(|worker| worker.handle.is_finished())
            {
                retire_peer(&mut group, slot, &mut quarantine, &mut retries, false);
            }
        }
        let order = with_generation(jobs, stop, id, |job| {
            if job.piece_order.len() != meta.count() {
                job.piece_order = Arc::new(super::piece_order(&meta, &job.control.file_priorities));
            }
            Ok(job.piece_order.clone())
        })?;
        let mut claimed: BTreeSet<_> = group
            .workers
            .iter()
            .flatten()
            .filter_map(|worker| worker.claim)
            .collect();
        for slot in 0..limit {
            let Some(worker) = group.workers[slot].as_mut() else {
                continue;
            };
            let Some(availability) = &worker.availability else {
                continue;
            };
            if worker.claim.is_some() {
                continue;
            }
            let next = order.iter().copied().find(|index| {
                !have[*index]
                    && !claimed.contains(index)
                    && availability.as_ref().is_none_or(|bits| {
                        *index / 8 < bits.len() && bits[*index / 8] & (0x80 >> (*index % 8)) != 0
                    })
            });
            if let Some(index) = next {
                if worker.commands.send(Command::Piece(index)).is_ok() {
                    worker.claim = Some(index);
                    claimed.insert(index);
                } else {
                    retire_peer(&mut group, slot, &mut quarantine, &mut retries, false);
                }
            } else if worker.idle_since.elapsed() >= RETRY && !have.iter().all(|have| *have) {
                // Recycling an idle peer refreshes HAVE/PEX availability and
                // allows later candidates to use this bounded connection slot.
                retire_peer(&mut group, slot, &mut quarantine, &mut retries, false);
            }
        }
        if last_update.elapsed() >= Duration::from_secs(1) {
            let done = have.iter().filter(|have| **have).count();
            update(
                (jobs, stop),
                id,
                Some(&meta),
                done as f64 / meta.count() as f64,
                false,
                if group
                    .workers
                    .iter()
                    .flatten()
                    .any(|worker| worker.claim.is_some())
                {
                    "Download in progress"
                } else {
                    "Searching for available peers"
                },
                config,
            )?;
            last_update = Instant::now();
        }
    }
    // No worker can retain a result or proof mutation when the complete torrent
    // is authenticated and published, including when an error unwinds the loop.
    drop(group);
    authenticate_v2(&mut meta, &base, stop)?;
    with_generation(jobs, stop, id, |job| {
        super::sync_files(&meta, &base, stop)?;
        persist_meta(config, id, &meta)?;
        job.meta = Some(Arc::new(meta.clone()));
        job.source.v1 = meta.v1;
        job.source.v2 = meta.v2;
        job.piece_order = Arc::new(super::piece_order(&meta, &job.control.file_priorities));
        job.status.files = meta
            .files
            .iter()
            .filter(|file| !file.padding)
            .map(|file| base.join(&file.path))
            .collect();
        job.status.progress = 1.0;
        job.status.ready = true;
        job.status.message = "Download verified and available".into();
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_piece(
    _config: &DownloadConfig,
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    meta: &Meta,
    base: &Path,
    index: usize,
    data: &[u8],
    have: &[bool],
) -> Result<()> {
    with_generation(jobs, stop, id, |job| {
        if have[index] {
            return Err("Peer returned an already verified piece".into());
        }
        write_piece(meta, base, index, data)?;
        super::add_payload(&job.counters.verified, verified_piece_bytes(meta, index));
        job.status.progress =
            (have.iter().filter(|have| **have).count() + 1) as f64 / meta.count() as f64;
        job.status.message = "Download in progress".into();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(count: usize) -> Meta {
        Meta {
            info: Vec::new(),
            encoded: Vec::new(),
            v1: None,
            v2: Some([1; 32]),
            private: true,
            piece_length: super::super::BLOCK,
            pieces: vec![None; count],
            v2_pieces: vec![None; count],
            files: Vec::new(),
            total: (count * super::super::BLOCK) as u64,
            trackers: Vec::new(),
        }
    }

    fn configuration(active: usize) -> DownloadConfig {
        DownloadConfig {
            data_dir: "data".into(),
            state_dir: "state".into(),
            listen_port: 0,
            seed: false,
            dht: false,
            pex: false,
            max_active: active,
            max_peers: 8,
        }
    }

    #[test]
    fn connection_allocation_and_large_metadata_reduce_worker_count() {
        let small = metadata(8);
        assert_eq!(peer_limit(&configuration(1), &small), 7);
        assert_eq!(peer_limit(&configuration(32), &small), 2);
        assert_eq!(peer_limit(&configuration(64), &small), 1);
        let mut large = metadata(1_048_576);
        large.piece_length = 16 * 1024 * 1024;
        assert_eq!(peer_limit(&configuration(1), &large), 1);
    }

    #[test]
    fn sparse_v2_leaf_pairs_respect_a_file_start_at_an_odd_piece_index() {
        let mut meta = metadata(5);
        meta.files.push(super::super::metainfo::MediaFile {
            path: "second.bin".into(),
            offset: super::super::BLOCK as u64,
            length: (4 * super::super::BLOCK) as u64,
            padding: false,
            root: Some([2; 32]),
        });
        merge_proofs(&mut meta, 2, &[(1, [3; 32]), (2, [4; 32])]).unwrap();
        assert_eq!(meta.v2_pieces[1], Some([3; 32]));
        assert_eq!(meta.v2_pieces[2], Some([4; 32]));
        let before = meta.v2_pieces.clone();
        assert!(merge_proofs(&mut meta, 2, &[(2, [5; 32])]).is_err());
        assert!(merge_proofs(&mut meta, 2, &[(3, [6; 32])]).is_err());
        assert_eq!(meta.v2_pieces, before);
    }

    #[test]
    fn retiring_a_peer_does_not_wait_forever_on_a_full_result_queue() {
        let (events, receiver) = mpsc::sync_channel(1);
        events
            .send(Event {
                slot: 0,
                ticket: 1,
                kind: EventKind::Failed { corrupt: false },
            })
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (commands, _receiver) = mpsc::sync_channel(1);
        let worker = Worker {
            address: "127.0.0.1:1".parse().unwrap(),
            ticket: 1,
            commands,
            stop,
            corrupt: Arc::new(AtomicBool::new(true)),
            handle: thread::spawn(move || {
                assert!(
                    send_event(
                        &events,
                        &worker_stop,
                        Event {
                            slot: 0,
                            ticket: 1,
                            kind: EventKind::Failed { corrupt: true }
                        }
                    )
                    .is_err()
                );
            }),
            availability: None,
            claim: Some(0),
            idle_since: Instant::now(),
        };
        let mut group = Group {
            workers: vec![Some(worker)],
            events: Some(receiver),
            discovery: None,
            discovery_stop: Arc::new(AtomicBool::new(false)),
        };
        let (finished, result) = mpsc::channel();
        let retire = thread::spawn(move || {
            finished.send(group.retire(0)).unwrap();
        });
        let (_, corrupt) = result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(
            corrupt,
            "Exited workers retain their corrupt-peer classification"
        );
        retire.join().unwrap();
    }
}
