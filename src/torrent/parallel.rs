//! A generation owns its peer workers, exclusive piece claims, and verified writes.

use super::{
    DownloadConfig, Jobs, Meta, PolicyRuntime, Source, TransferCounters, discovery, mkdir_private,
    persist_meta, transport::Network, update, verified_piece_bytes, verify_existing, wire,
    write_piece,
};
use crate::Result;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
pub(super) const CONNECTION_BUDGET: usize = 256;
const POLL: Duration = Duration::from_millis(20);
const RETRY: Duration = Duration::from_secs(2);
// Incoming peers waiting for a free slot, and how long one waits for it
// before its connection is closed.
const INBOUND: usize = 8;
const INBOUND_WAIT: Duration = Duration::from_secs(5);

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
    // Workers share one copy of the metadata. A worker that must store v2
    // hashes fetched from its peer takes its own, so then each one may hold one.
    let copied = (0..meta.count()).any(|index| super::wire::needs_hashes(meta, index));
    let footprint = meta
        .piece_length
        // Every claimed piece can be buffered, including queued results.
        .saturating_mul(wire::piece_claims(meta))
        // A message being read plus its copy, bounded once the pieces are known.
        .saturating_add(2 * super::wire::message_limit(Some(meta.count())))
        .saturating_add(if copied { metadata } else { 0 })
        .max(1);
    let budget = if copied {
        WORKER_MEMORY_BUDGET
    } else {
        WORKER_MEMORY_BUDGET.saturating_sub(metadata)
    };
    config
        .max_peers
        .min((CONNECTION_BUDGET / config.download_slots()).max(1))
        .min((budget / footprint).max(1))
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
    /// Another peer delivered the piece first.
    Abandoned {
        index: usize,
        availability: Option<Vec<u8>>,
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
    /// The peer fetches from this side: keep it even with nothing to offer.
    uploading: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    // None before handshake; Some(None) for unknown availability.
    availability: Option<Option<Vec<u8>>>,
    // The piece being fetched first, then the one queued behind it.
    claims: Vec<usize>,
    idle_since: Instant,
    received: u64,
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
    meta: Arc<Meta>,
    counters: Arc<TransferCounters>,
    download_gate: Arc<super::RateGate>,
    local_download_gate: Arc<super::RateGate>,
    network: Arc<dyn Network>,
    stop: Arc<AtomicBool>,
    corrupt: Arc<AtomicBool>,
    commands: Receiver<Command>,
    /// An incoming peer to answer instead of dialing `address`.
    inbound: Option<wire::Inbound>,
    serving: Option<wire::Serving>,
    uploading: Arc<AtomicBool>,
    /// Pieces verified through any peer.
    done: Arc<wire::Pieces>,
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

/// The rarest piece `wanted` accepts, within the most urgent priority it
/// accepts at all; ties keep `order`. No missing piece is rarer than `floor`.
fn rarest(
    order: &[usize],
    ranks: &[u8],
    rarity: &[u16],
    floor: u16,
    wanted: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for &index in order {
        if let Some(best) = best {
            if ranks[index] < ranks[best] {
                break;
            }
            if rarity[index] >= rarity[best] {
                continue;
            }
        }
        if wanted(index) {
            best = Some(index);
            if rarity[index] <= floor {
                break;
            }
        }
    }
    best
}

fn count_offers(rarity: &mut [u16], bits: &[u8], add: bool) {
    for (byte, bits) in bits.iter().enumerate().filter(|(_, bits)| **bits != 0) {
        for bit in (0..8).filter(|bit| bits & (0x80 >> bit) != 0) {
            if let Some(count) = rarity.get_mut(byte * 8 + bit) {
                *count = if add {
                    count.saturating_add(1)
                } else {
                    count.saturating_sub(1)
                };
            }
        }
    }
}

/// What the coordinator may assign to a peer: nothing while it chokes this
/// side, which it may do while fetching from it.
fn offered(peer: &wire::Peer) -> Option<Vec<u8>> {
    if peer.choked {
        return Some(Vec::new());
    }
    peer.availability().map(<[u8]>::to_vec)
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
        let inbound = task.inbound.take();
        let settings = wire::PeerSettings {
            meta: Some(&task.meta),
            port: task.port,
            pex: task.pex,
            v2_wire: task.v2_wire,
            counters: task.counters.clone(),
            download_gate: task.download_gate.clone(),
            local_download_gate: task.local_download_gate.clone(),
            network: &*task.network,
            serving: task.serving.clone(),
        };
        let mut peer = match inbound {
            Some(inbound) => wire::Peer::accept(inbound, &task.peer_id, settings, &task.stop)?,
            None => wire::Peer::connect_cancellable(
                task.address,
                &task.hash,
                &task.peer_id,
                settings,
                &task.stop,
            )?,
        };
        peer.watch(task.done.clone());
        peer.prepare_download(&task.meta, &task.stop)?;
        send(EventKind::Connected {
            availability: offered(&peer),
            peers: std::mem::take(&mut peer.discovered),
        })?;
        let mut keepalive = Instant::now();
        let mut queued = VecDeque::new();
        while !task.stop.load(Ordering::Acquire) {
            let next = match queued.pop_front() {
                Some(index) => Ok(Command::Piece(index)),
                None => match task.commands.try_recv() {
                    Ok(command) => Ok(command),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Err(mpsc::RecvTimeoutError::Disconnected)
                    }
                    // A peer fetching from this side is answered while idle.
                    Err(mpsc::TryRecvError::Empty) => {
                        let busy = peer.serve_between_pieces(&task.meta, &task.stop)?;
                        task.uploading.store(peer.uploading(), Ordering::Release);
                        // HAVEs and unchokes read here reach the coordinator.
                        if peer.take_fresh() {
                            send(EventKind::Connected {
                                availability: offered(&peer),
                                peers: std::mem::take(&mut peer.discovered),
                            })?;
                        }
                        task.commands.recv_timeout(if busy {
                            Duration::from_millis(1)
                        } else {
                            POLL
                        })
                    }
                },
            };
            let command = match next {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if keepalive.elapsed() >= Duration::from_secs(5) {
                        wire::write_all_deadline(
                            &mut *peer.stream,
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
            // The queued piece is taken only once the peer can request it.
            let mut upcoming = |ahead| {
                while queued.len() <= ahead {
                    let Ok(Command::Piece(next)) = task.commands.try_recv() else {
                        break;
                    };
                    queued.push_back(next);
                }
                queued.get(ahead).copied()
            };
            // Only a worker storing fetched v2 hashes copies the shared metadata.
            let fetched = if wire::needs_hashes(&task.meta, index) {
                peer.fetch_piece(
                    Arc::make_mut(&mut task.meta),
                    index,
                    &mut upcoming,
                    &task.stop,
                )
            } else {
                peer.fetch_known_piece(&task.meta, index, &mut upcoming, &task.stop)
            };
            let data = match fetched {
                Err(e) if e == wire::ABANDONED => {
                    send(EventKind::Abandoned {
                        index,
                        availability: offered(&peer),
                    })?;
                    keepalive = Instant::now();
                    continue;
                }
                fetched => fetched?,
            };
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
                availability: offered(&peer),
                peers: std::mem::take(&mut peer.discovered),
            })?;
            // The coordinator alone can issue the next command. Each worker
            // therefore owns at most piece_claims(meta) payloads, including
            // queued results.
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
            if peers.is_empty() {
                next_dht = next_dht.min(Instant::now() + discovery::DHT_RETRY);
            }
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
            let network = policy.network.clone();
            group.discovery = Some(thread::spawn(move || {
                discovery::dht_lookup(&*network, &hash, port, false, &cancel, &[])
                    .unwrap_or_default()
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
                network: &*policy.network,
                serving: None,
            },
            stop,
        )
        .and_then(|mut peer| peer.metadata(source.v1, source.v2, stop));
        match result {
            Ok(meta) => {
                let _publication = policy.network.publication()?;
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
    let (source, cached, counters, local_gate, upload_gate) =
        with_generation(jobs, stop, id, |job| {
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
                job.upload_gate.clone(),
            ))
        })?;
    let mut addresses = BTreeSet::new();
    super::merge_peers(
        &mut addresses,
        source.peer_addresses(&*policy.network, Instant::now() + Duration::from_secs(20))?,
    );
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
    let selected = with_generation(jobs, stop, id, |job| {
        super::selection::SelectionPlan::new(&meta, &job.control.file_selection)
    })?;
    let fresh = with_generation(jobs, stop, id, |_| {
        mkdir_private(&base)?;
        super::prepare_selected_files(&meta, &base, stop, Some(&selected.storage))
    })?;
    let mut have = verify_existing(&meta, &base, stop, &fresh);
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
    // Peers may fetch verified pieces from this side while it downloads.
    // Duplicate requests near the end stop once one copy is verified.
    let pieces = Arc::new(wire::Pieces::new(&have));
    let serving = config.seed.then(|| wire::Serving {
        pieces: pieces.clone(),
        base: base.clone(),
        upload_gate: policy.upload.clone(),
        local_upload_gate: upload_gate.clone(),
    });
    let limit = peer_limit(config, &meta);
    let claims = wire::piece_claims(&meta);
    // What new workers start from; renewed after the coordinator learns hashes.
    let mut shared = Arc::new(meta.clone());
    let mut shared_stale = false;
    // Piece priorities, recomputed when the order they produced changes.
    let mut ranked = Arc::new(Vec::new());
    let mut ranks = Vec::new();
    let mut rarity = vec![0u16; meta.count()];
    let mut counted: Vec<Option<Vec<u8>>> = vec![None; limit];
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
    let (inbound_sender, inbound) = mpsc::sync_channel(INBOUND);
    with_generation(jobs, stop, id, |job| {
        job.inbound = Some(inbound_sender);
        Ok(())
    })?;
    let mut waiting: Vec<(wire::Inbound, Instant)> = Vec::new();
    let mut last_update = Instant::now() - Duration::from_secs(1);
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if selected.complete(&have) {
            break;
        }
        refresh_peers(jobs, stop, id, &mut addresses)?;
        if let Some(handle) = group.discovery.as_ref()
            && handle.is_finished()
            && let Some(handle) = group.discovery.take()
            && let Ok(peers) = handle.join()
        {
            if peers.is_empty() {
                next_dht = next_dht.min(Instant::now() + discovery::DHT_RETRY);
            }
            super::merge_peers(&mut addresses, peers);
        }
        if config.dht && !meta.private && group.discovery.is_none() && Instant::now() >= next_dht {
            next_dht = Instant::now() + Duration::from_secs(60);
            let cancel = group.discovery_stop.clone();
            let port = config.listen_port;
            let network = policy.network.clone();
            group.discovery = Some(thread::spawn(move || {
                discovery::dht_lookup(&*network, &hash, port, true, &cancel, &[])
                    .unwrap_or_default()
            }));
        }
        // Incoming peers of this torrent take the next free slots, before any
        // address is dialed; one not given a slot in time is closed.
        while let Ok(peer) = inbound.try_recv() {
            if peer.hash == hash && waiting.len() < INBOUND {
                waiting.push((peer, Instant::now()));
            }
        }
        waiting.retain(|(_, since)| since.elapsed() < INBOUND_WAIT);
        // At most one connection and `claims` exclusive claims exist per slot.
        // A retired or panicking worker loses its claims when its slot is taken.
        for slot in 0..limit {
            if group.workers[slot].is_some() {
                continue;
            }
            let accepted = waiting.pop().map(|(peer, _)| peer);
            let address = match &accepted {
                Some(peer) => Some(peer.address),
                None => select_address(&addresses, &group.workers, &quarantine, &retries, cursor)
                    .inspect(|address| cursor = Some(*address)),
            };
            if let Some(address) = address {
                ticket = ticket
                    .checked_add(1)
                    .ok_or("Peer generation sequence exhausted")?;
                let (commands, command_receiver) = mpsc::sync_channel(claims);
                let cancel = Arc::new(AtomicBool::new(false));
                let corrupt = Arc::new(AtomicBool::new(false));
                let uploading = Arc::new(AtomicBool::new(false));
                let task = PeerTask {
                    slot,
                    ticket,
                    address,
                    hash,
                    peer_id: *peer_id,
                    port: config.listen_port,
                    pex: config.pex && !meta.private,
                    v2_wire: source.v1.is_none(),
                    meta: {
                        if shared_stale {
                            shared = Arc::new(meta.clone());
                            shared_stale = false;
                        }
                        shared.clone()
                    },
                    counters: counters.clone(),
                    download_gate: policy.download.clone(),
                    local_download_gate: local_gate.clone(),
                    network: policy.network.clone(),
                    stop: cancel.clone(),
                    commands: command_receiver,
                    inbound: accepted,
                    serving: serving.clone(),
                    uploading: uploading.clone(),
                    done: pieces.clone(),
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
                    uploading,
                    availability: None,
                    claims: Vec::new(),
                    idle_since: Instant::now(),
                    received: 0,
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
                    let _publication = policy.network.publication()?;
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
                    if worker.claims.first() != Some(&index) {
                        return Err("Peer returned an unclaimed piece".into());
                    }
                    worker.claims.remove(0);
                    worker.availability = Some(availability);
                    worker.idle_since = Instant::now();
                    // The worker verified the piece against the shared
                    // metadata; only hashes it fetched itself need a second
                    // check once merged here.
                    shared_stale |= !proofs.is_empty();
                    if merge_proofs(&mut meta, index, &proofs).is_err()
                        || (!proofs.is_empty() && !meta.verify(index, &data))
                    {
                        retire_peer(&mut group, event.slot, &mut quarantine, &mut retries, true);
                    } else if !have[index] {
                        worker.received += data.len() as u64;
                        let _publication = policy.network.publication()?;
                        commit_piece(
                            jobs, stop, id, &meta, &base, index, &data, &mut have, &selected,
                        )?;
                        pieces.add(index);
                        if !meta.private && config.pex {
                            super::merge_peers(&mut addresses, peers);
                        }
                    }
                }
                EventKind::Abandoned {
                    index,
                    availability,
                } => {
                    let worker = group.workers[event.slot]
                        .as_mut()
                        .ok_or("Peer worker unavailable")?;
                    if worker.claims.first() != Some(&index) || !have[index] {
                        return Err("Peer abandoned an unclaimed piece".into());
                    }
                    worker.claims.remove(0);
                    worker.availability = Some(availability);
                    worker.idle_since = Instant::now();
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
        let (order, fresh_ranks) = with_generation(jobs, stop, id, |job| {
            if job.piece_order.len() != meta.count() {
                job.piece_order = Arc::new(super::piece_order(&meta, &job.control.file_priorities));
            }
            let fresh = (!Arc::ptr_eq(&job.piece_order, &ranked))
                .then(|| super::piece_ranks(&meta, &job.control.file_priorities));
            Ok((job.piece_order.clone(), fresh))
        })?;
        if let Some(fresh) = fresh_ranks {
            ranks = fresh;
            ranked = order.clone();
        }
        // How many connected peers offer each piece, kept current by diffing
        // what each slot offered when last counted.
        for (worker, counted) in group.workers.iter().zip(&mut counted) {
            let offered = worker
                .as_ref()
                .and_then(|worker| worker.availability.as_ref())
                .and_then(Option::as_ref);
            if offered != counted.as_ref() {
                if let Some(old) = counted.take() {
                    count_offers(&mut rarity, &old, false);
                }
                if let Some(new) = offered {
                    count_offers(&mut rarity, new, true);
                    *counted = Some(new.clone());
                }
            }
        }
        let mut claimed = vec![0u8; meta.count()];
        // Pieces held by a peer that delivered nothing for a while.
        let mut stalled = vec![false; meta.count()];
        for worker in group.workers.iter().flatten() {
            for index in &worker.claims {
                claimed[*index] = claimed[*index].saturating_add(1);
                stalled[*index] |= worker.idle_since.elapsed() >= RETRY;
            }
        }
        let missing = |index: &usize| selected.pieces[*index] && !have[*index];
        // A second claim keeps a peer's requests flowing between pieces, but
        // near the end it would make other peers wait for a slow one.
        let workers = group.workers.iter().flatten().count();
        let plenty = (0..meta.count())
            .filter(|index| missing(index) && claimed[*index] == 0)
            .nth(claims * workers)
            .is_some();
        let floor = (0..meta.count())
            .filter(|index| missing(index) && claimed[*index] == 0)
            .map(|index| rarity[index])
            .min()
            .unwrap_or(0);
        for slot in 0..limit {
            let Some(worker) = group.workers[slot].as_mut() else {
                continue;
            };
            let Some(availability) = &worker.availability else {
                continue;
            };
            let offers = |index: usize| {
                availability.as_ref().is_none_or(|bits| {
                    index / 8 < bits.len() && bits[index / 8] & (0x80 >> (index % 8)) != 0
                })
            };
            let mut failed = false;
            while worker.claims.len() < if workers == 1 || plenty { claims } else { 1 } {
                let next = rarest(&order, &ranks, &rarity, floor, |index| {
                    missing(&index) && claimed[index] == 0 && offers(index)
                })
                // Endgame: once every missing piece is claimed, an idle peer
                // also fetches one held by a stalled peer. The first verified
                // copy makes the other peer cancel its requests.
                .or_else(|| {
                    (!plenty && worker.claims.is_empty())
                        .then(|| {
                            order.iter().copied().find(|index| {
                                missing(index)
                                    && claimed[*index] == 1
                                    && stalled[*index]
                                    && offers(*index)
                            })
                        })
                        .flatten()
                });
                let Some(index) = next else {
                    // Recycling an idle peer refreshes HAVE/PEX availability
                    // and allows later candidates to use this bounded slot.
                    failed = worker.claims.is_empty()
                        && worker.idle_since.elapsed() >= RETRY
                        && !worker.uploading.load(Ordering::Acquire)
                        && !selected.complete(&have);
                    break;
                };
                // The channel holds `claims` commands, so this never waits.
                if worker.commands.send(Command::Piece(index)).is_err() {
                    failed = true;
                    break;
                }
                worker.claims.push(index);
                claimed[index] += 1;
            }
            if failed {
                retire_peer(&mut group, slot, &mut quarantine, &mut retries, false);
            }
        }
        if last_update.elapsed() >= Duration::from_secs(1) {
            let pieces = meta.count().max(1) as f64;
            let live: Vec<_> = group
                .workers
                .iter()
                .flatten()
                .map(|worker| super::PeerView {
                    address: worker.address,
                    connected: worker.availability.is_some(),
                    progress: worker.availability.as_ref().and_then(|bits| {
                        bits.as_ref().map(|bits| {
                            let have: u32 = bits.iter().map(|byte| byte.count_ones()).sum();
                            (f64::from(have) / pieces).min(1.0)
                        })
                    }),
                    downloading: !worker.claims.is_empty(),
                    received_bytes: worker.received,
                })
                .collect();
            let known = addresses.len();
            with_generation(jobs, stop, id, |job| {
                job.live_peers = live;
                job.known_peers = known;
                Ok(())
            })?;
            update(
                (jobs, stop),
                id,
                Some(&meta),
                selected.progress(&have),
                false,
                if group
                    .workers
                    .iter()
                    .flatten()
                    .any(|worker| !worker.claims.is_empty())
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
    // No worker may retain a payload or proof mutation during publication.
    drop(group);
    let full = have.iter().all(|have| *have);
    if full {
        with_generation(jobs, stop, id, |_| {
            super::prepare_files(&meta, &base, stop).map(drop)
        })?;
    }
    super::authenticate_selected_v2(
        &mut meta,
        &base,
        stop,
        (!full).then_some(selected.files.as_slice()),
    )?;
    let _publication = policy.network.publication()?;
    with_generation(jobs, stop, id, |job| {
        super::sync_selected_files(
            &meta,
            &base,
            stop,
            (!full).then_some(selected.storage.as_slice()),
        )?;
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
        job.status.ready = full;
        job.status.selected_ready = true;
        job.status.available_files = meta
            .files
            .iter()
            .zip(&selected.files)
            .filter(|(file, wanted)| !file.padding && (full || **wanted))
            .map(|(file, _)| base.join(&file.path))
            .collect();
        job.status.message = if full {
            "Download verified and available"
        } else {
            "Selected files verified and available; torrent is incomplete"
        }
        .into();
        // The seed queue reads readiness from the index: save it now rather
        // than at the next periodic save, up to 30 seconds later.
        super::persist_index(job)
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_piece(
    jobs: &Jobs,
    stop: &AtomicBool,
    id: &str,
    meta: &Meta,
    base: &Path,
    index: usize,
    data: &[u8],
    have: &mut [bool],
    selected: &super::selection::SelectionPlan,
) -> Result<()> {
    if have[index] {
        return Err("Peer returned an already verified piece".into());
    }
    // Disk writes stay outside the state lock that status calls, inbound
    // peers and the persistence flush share. Files are only removed once
    // this download has stopped, so the generation is checked afterwards.
    write_piece(meta, base, index, data)?;
    with_generation(jobs, stop, id, |job| {
        have[index] = true;
        super::add_payload(&job.counters.verified, verified_piece_bytes(meta, index));
        job.status.progress = selected.progress(have);
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
            max_seeding: 0,
            max_peers: 256,
        }
    }

    #[test]
    fn the_rarest_piece_comes_first_within_the_most_urgent_priority() {
        // Piece 2 has the high priority, 0 and 1 the normal one, 3 the low.
        let order = [2, 0, 1, 3];
        let ranks = [1, 1, 2, 0];
        let mut rarity = vec![0; 4];
        count_offers(&mut rarity, &[0xf0], true);
        count_offers(&mut rarity, &[0xf0], true);
        count_offers(&mut rarity, &[0x80], true);
        count_offers(&mut rarity, &[0x20], true);
        count_offers(&mut rarity, &[0x20], false);
        assert_eq!(rarity, [3, 2, 2, 2]);
        assert_eq!(rarest(&order, &ranks, &rarity, 2, |_| true), Some(2));
        // Rarer within its priority, even after a more common piece.
        assert_eq!(rarest(&order, &ranks, &rarity, 2, |i| i != 2), Some(1));
        // A lower priority is never preferred for being rarer.
        assert_eq!(rarest(&order, &ranks, &[1, 9, 9, 0], 0, |i| i < 2), Some(0));
        // Ties keep the order.
        assert_eq!(rarest(&order, &ranks, &[2, 2, 2, 2], 2, |i| i < 2), Some(0));
        assert_eq!(rarest(&order, &ranks, &rarity, 2, |i| i == 3), Some(3));
        assert_eq!(rarest(&order, &ranks, &rarity, 2, |_| false), None);
    }

    #[test]
    fn connection_allocation_and_large_metadata_reduce_worker_count() {
        let small = metadata(8);
        assert_eq!(peer_limit(&configuration(1), &small), 256);
        assert_eq!(peer_limit(&configuration(32), &small), 8);
        assert_eq!(peer_limit(&configuration(256), &small), 1);
        // Bounded messages and the blocks requested ahead: a 4 MiB piece
        // includes two full payloads, even while results wait in the queue.
        let mut wide = metadata(8);
        wide.piece_length = 4 * 1024 * 1024;
        assert_eq!(peer_limit(&configuration(1), &wide), 15);
        let mut large = metadata(1_048_576);
        large.piece_length = 16 * 1024 * 1024;
        assert_eq!(peer_limit(&configuration(1), &large), 1);
        // Hashes known up front: metadata is shared, and the bound includes
        // all eight payloads even if a completed result is still queued.
        let mut known = metadata(24_000);
        known.v2 = None;
        known.pieces = vec![Some([0; 20]); 24_000];
        known.piece_length = 256 * 1024;
        assert_eq!(peer_limit(&configuration(1), &known), 59);
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
            uploading: Arc::default(),
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
            claims: vec![0],
            idle_since: Instant::now(),
            received: 0,
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
