//! Metadata-only discovery: no transfer queue, payload request, or disk write.
use super::{RateGate, Source, TransferCounters, discovery, metainfo::Meta, wire};
use crate::Result;
use std::{
    collections::BTreeSet,
    net::TcpListener,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

pub const MAX_INSPECTED_FILES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataFile {
    pub path: String,
    pub length: u64,
    pub padding: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentMetadata {
    /// The source's authenticated hash, retained even when a hybrid has another alias.
    pub id: String,
    /// All authenticated v1/v2 aliases, for exclusive physical-file ownership.
    pub aliases: Vec<String>,
    pub files: Vec<MetadataFile>,
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "Torrent metadata deadline exceeded".into())
}

fn report(meta: Meta, id: String, deadline: Instant) -> Result<TorrentMetadata> {
    remaining(deadline)?;
    if meta.files.len() > MAX_INSPECTED_FILES {
        return Err("Pack metadata exceeds 1024 files".into());
    }
    let mut bytes = 0_usize;
    let aliases = meta
        .v1
        .iter()
        .map(|h| super::hex(h))
        .chain(meta.v2.iter().map(|h| super::hex(h)))
        .collect();
    let mut files = Vec::with_capacity(meta.files.len());
    for file in meta.files {
        let path = file
            .path
            .to_str()
            .ok_or("Pack metadata path is not UTF-8")?;
        bytes = bytes.saturating_add(path.len());
        if bytes > 1_048_576 {
            return Err("Pack metadata paths exceed 1 MiB".into());
        }
        files.push(MetadataFile {
            path: path.into(),
            length: file.length,
            padding: file.padding,
        });
    }
    remaining(deadline)?;
    Ok(TorrentMetadata { id, aliases, files })
}

/// Inspect a local/HTTP torrent or authenticate magnet metadata from bounded peers.
/// Tracker lookups use stopped events, zero counters, and an ephemeral port; they
/// never advertise payload or completion. Trackers may decline metadata-only queries.
/// Unknown privacy never enables DHT or PEX discovery in this inspection path.
pub fn inspect_metadata(source: &str, deadline: Instant) -> Result<TorrentMetadata> {
    remaining(deadline)?;
    let mut source = Source::parse_with_deadline(source, Some(deadline))
        .map_err(|_| "Torrent source metadata could not be read or authenticated")?;
    let id = source.id();
    if let Some(meta) = source.meta.take() {
        return report(meta, id, deadline);
    }
    let peer_id = crate::crypto::random_bytes::<20>()?;
    let wire_hash = source.wire_hash();
    let counters = Arc::new(TransferCounters::default());
    let gate = Arc::new(RateGate::new(0)?);
    let stop = AtomicBool::new(false);
    let mut peers: BTreeSet<_> = source.peers.iter().copied().collect();
    let mut tried = BTreeSet::new();
    let mut trackers = source.trackers.iter().take(4);
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|_| "Cannot reserve the metadata inspection port")?;
    let port = listener
        .local_addr()
        .map_err(|_| "Cannot read the metadata inspection port")?
        .port();
    loop {
        remaining(deadline)?;
        if tried.len() >= 8 {
            break;
        }
        if let Some(address) = peers
            .iter()
            .find(|address| !tried.contains(*address))
            .copied()
        {
            tried.insert(address);
            let attempt_end = deadline.min(Instant::now() + Duration::from_secs(5));
            let result = wire::Peer::connect_before(
                address,
                &wire_hash,
                &peer_id,
                wire::PeerSettings {
                    meta: None,
                    port,
                    pex: false,
                    v2_wire: source.v1.is_none(),
                    counters: counters.clone(),
                    download_gate: gate.clone(),
                    local_download_gate: gate.clone(),
                },
                attempt_end,
            )
            .and_then(|mut peer| {
                peer.metadata_before(source.v1, source.v2, &stop, Some(attempt_end))
            });
            if let Ok(meta) = result {
                return report(meta, id, deadline);
            }
        } else if let Some(url) = trackers.next() {
            let request = discovery::TrackerRequest {
                hash: &wire_hash,
                peer_id: &peer_id,
                port,
                left: 1,
                uploaded: 0,
                downloaded: 0,
                event: discovery::TrackerEvent::Stopped,
            };
            if let Ok(reply) = discovery::tracker_before(
                url,
                &request,
                deadline.min(Instant::now() + Duration::from_secs(5)),
            ) {
                peers.extend(reply.peers.into_iter().take(256));
            }
        } else {
            break;
        }
    }
    Err("No peer authenticated pack metadata within the inspection limits".into())
}
