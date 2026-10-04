use super::{
    discovery::{value_dict, value_field},
    metainfo::{BLOCK, MAX_META, Meta, pair, zero_hash},
};
use crate::{Result, bencode::Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const EXT_METADATA: u8 = 1;
pub const EXT_PEX: u8 = 2;
pub struct PeerSettings<'a> {
    pub meta: Option<&'a Meta>,
    pub port: u16,
    pub pex: bool,
    pub v2_wire: bool,
    pub counters: Arc<super::TransferCounters>,
    pub download_gate: Arc<super::RateGate>,
    pub local_download_gate: Arc<super::RateGate>,
}
pub struct Peer {
    pub stream: TcpStream,
    pub metadata_id: Option<u8>,
    pub pex_id: Option<u8>,
    pub metadata_size: Option<usize>,
    pub bitfield: Vec<u8>,
    pub choked: bool,
    pub extensions: bool,
    pub discovered: Vec<SocketAddr>,
    pub pex_allowed: bool,
    pub v2_wire: bool,
    counters: Arc<super::TransferCounters>,
    download_gate: Arc<super::RateGate>,
    local_download_gate: Arc<super::RateGate>,
    piece_count: Option<usize>,
    bitfield_received: bool,
    availability_known: bool,
}

#[cfg(test)]
pub fn handshake(
    stream: &mut TcpStream,
    hash: &[u8; 20],
    id: &[u8; 20],
    outgoing: bool,
) -> Result<bool> {
    handshake_deadline(
        stream,
        hash,
        id,
        outgoing,
        None,
        Instant::now() + Duration::from_secs(5),
    )
}

fn handshake_deadline(
    stream: &mut TcpStream,
    hash: &[u8; 20],
    id: &[u8; 20],
    outgoing: bool,
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<bool> {
    let mut packet = [0u8; 68];
    packet[0] = 19;
    packet[1..20].copy_from_slice(b"BitTorrent protocol");
    packet[25] = 0x10;
    packet[27] = 0;
    packet[28..48].copy_from_slice(hash);
    packet[48..].copy_from_slice(id);
    let mut response = [0; 68];
    if outgoing {
        write_all_deadline(stream, &packet, stop, deadline)?;
    }
    read_exact_deadline(stream, &mut response, stop, deadline)?;
    if response[0] != 19 || &response[1..20] != b"BitTorrent protocol" || response[28..48] != *hash
    {
        return Err("Invalid peer handshake".into());
    }
    if !outgoing {
        write_all_deadline(stream, &packet, stop, deadline)?;
    }
    Ok(response[25] & 0x10 != 0)
}

pub fn read_exact_deadline(
    stream: &mut TcpStream,
    mut buffer: &mut [u8],
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| "Could not configure TCP reads")?;
    while !buffer.is_empty() {
        if stop.is_some_and(|s| s.load(Ordering::Acquire)) {
            return Err("Download interrupted".into());
        }
        if Instant::now() >= deadline {
            return Err("Peer exceeded read deadline".into());
        }
        match stream.read(buffer) {
            Ok(0) => return Err("Peer connection closed".into()),
            Ok(n) => {
                let (_, remaining) = buffer.split_at_mut(n);
                buffer = remaining;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err("Peer read interrupted".into()),
        }
    }
    Ok(())
}
pub fn write_all_deadline(
    stream: &mut TcpStream,
    mut buffer: &[u8],
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<()> {
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| "Could not configure TCP writes")?;
    while !buffer.is_empty() {
        if stop.is_some_and(|s| s.load(Ordering::Acquire)) {
            return Err("Download interrupted".into());
        }
        if Instant::now() >= deadline {
            return Err("Peer exceeded write deadline".into());
        }
        match stream.write(buffer) {
            Ok(0) => return Err("Peer connection closed".into()),
            Ok(n) => buffer = &buffer[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err("Peer write interrupted".into()),
        }
    }
    Ok(())
}
pub fn write_message(
    stream: &mut TcpStream,
    id: u8,
    payload: &[u8],
    stop: Option<&AtomicBool>,
) -> Result<()> {
    write_message_deadline(
        stream,
        id,
        payload,
        stop,
        Instant::now() + Duration::from_secs(15),
    )
}

fn write_message_deadline(
    stream: &mut TcpStream,
    id: u8,
    payload: &[u8],
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<()> {
    let length = u32::try_from(payload.len().checked_add(1).ok_or("Message is too large")?)
        .map_err(|_| "Message is too large")?;
    write_all_deadline(stream, &length.to_be_bytes(), stop, deadline)?;
    write_all_deadline(stream, &[id], stop, deadline)?;
    write_all_deadline(stream, payload, stop, deadline)
}
/// Count payload bytes only after the socket accepts them, including a partial
/// write before a later network failure. Protocol headers are never charged.
pub fn write_payload_message(
    stream: &mut TcpStream,
    header: &[u8],
    mut payload: &[u8],
    stop: &AtomicBool,
    uploaded: &std::sync::atomic::AtomicU64,
) -> Result<()> {
    let length = header
        .len()
        .checked_add(payload.len())
        .and_then(|n| n.checked_add(1))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or("Message is too large")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    write_all_deadline(stream, &length.to_be_bytes(), Some(stop), deadline)?;
    write_all_deadline(stream, &[7], Some(stop), deadline)?;
    write_all_deadline(stream, header, Some(stop), deadline)?;
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| "Could not configure TCP writes")?;
    while !payload.is_empty() {
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        if Instant::now() >= deadline {
            return Err("Peer exceeded write deadline".into());
        }
        match stream.write(payload) {
            Ok(0) => return Err("Peer connection closed".into()),
            Ok(n) => {
                super::add_payload(uploaded, n as u64);
                payload = &payload[n..];
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err("Peer write interrupted".into()),
        }
    }
    Ok(())
}
pub fn reserve_payload(
    stream: &mut TcpStream,
    aggregate: &super::RateGate,
    local: &super::RateGate,
    bytes: u64,
    stop: &AtomicBool,
) -> Result<()> {
    let mut keepalive = Instant::now();
    super::RateGate::reserve_pair_with(aggregate, local, bytes, stop, || {
        if keepalive.elapsed() >= Duration::from_secs(5) {
            write_all_deadline(
                stream,
                &[0; 4],
                Some(stop),
                Instant::now() + Duration::from_secs(5),
            )?;
            keepalive = Instant::now();
        }
        Ok(())
    })
}
pub fn read_message(stream: &mut TcpStream, stop: Option<&AtomicBool>) -> Result<(u8, Vec<u8>)> {
    read_message_deadline(stream, stop, Instant::now() + Duration::from_secs(15))
}

fn read_message_deadline(
    stream: &mut TcpStream,
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<(u8, Vec<u8>)> {
    let mut length = [0; 4];
    read_exact_deadline(stream, &mut length, stop, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        return Ok((255, Vec::new()));
    }
    if length > MAX_META + 1024 {
        return Err("Peer message is too large".into());
    }
    let mut payload = vec![0; length];
    read_exact_deadline(stream, &mut payload, stop, deadline)?;
    Ok((payload[0], payload[1..].to_vec()))
}
pub fn extended_handshake(meta: Option<&Meta>, port: u16, pex: bool) -> Vec<u8> {
    let mut extensions = BTreeMap::new();
    extensions.insert(b"ut_metadata".to_vec(), Value::Int(i64::from(EXT_METADATA)));
    if pex && meta.is_some_and(|m| !m.private) {
        extensions.insert(b"ut_pex".to_vec(), Value::Int(i64::from(EXT_PEX)));
    }
    let mut fields = BTreeMap::new();
    fields.insert(b"m".to_vec(), Value::Dict(extensions));
    fields.insert(
        b"v".to_vec(),
        Value::Bytes(format!("Mynou/{}", env!("CARGO_PKG_VERSION")).into_bytes()),
    );
    fields.insert(b"reqq".to_vec(), Value::Int(16));
    fields.insert(b"p".to_vec(), Value::Int(i64::from(port)));
    if let Some(meta) = meta {
        fields.insert(
            b"metadata_size".to_vec(),
            Value::Int(meta.info.len() as i64),
        );
    }
    let mut out = vec![0];
    out.extend(crate::bencode::encode(&Value::Dict(fields)));
    out
}

fn valid_spare_bits(bitfield: &[u8], count: usize) -> bool {
    count.is_multiple_of(8)
        || bitfield
            .get(count / 8)
            .is_none_or(|byte| byte & ((1u8 << (8 - count % 8)) - 1) == 0)
}

fn validate_bitfield(bitfield: &[u8], count: Option<usize>) -> Result<()> {
    if bitfield.len() > 1024 * 1024 {
        return Err("Bitfield exceeds limits".into());
    }
    if let Some(count) = count
        && (bitfield.len() != count.div_ceil(8) || !valid_spare_bits(bitfield, count))
    {
        return Err("Bitfield does not match metadata piece count".into());
    }
    Ok(())
}

impl Peer {
    pub fn connect(
        address: SocketAddr,
        hash: &[u8; 20],
        id: &[u8; 20],
        settings: PeerSettings<'_>,
    ) -> Result<Self> {
        Self::connect_with_stop(address, hash, id, settings, None, None)
    }

    pub fn connect_cancellable(
        address: SocketAddr,
        hash: &[u8; 20],
        id: &[u8; 20],
        settings: PeerSettings<'_>,
        stop: &AtomicBool,
    ) -> Result<Self> {
        Self::connect_with_stop(address, hash, id, settings, Some(stop), None)
    }

    pub fn connect_before(
        address: SocketAddr,
        hash: &[u8; 20],
        id: &[u8; 20],
        settings: PeerSettings<'_>,
        deadline: Instant,
    ) -> Result<Self> {
        Self::connect_with_stop(address, hash, id, settings, None, Some(deadline))
    }

    fn connect_with_stop(
        address: SocketAddr,
        hash: &[u8; 20],
        id: &[u8; 20],
        settings: PeerSettings<'_>,
        stop: Option<&AtomicBool>,
        end: Option<Instant>,
    ) -> Result<Self> {
        let PeerSettings {
            meta,
            port,
            pex,
            v2_wire,
            counters,
            download_gate,
            local_download_gate,
        } = settings;
        // Standard-library connects cannot be cancelled in flight. One
        // bounded attempt avoids resetting the handshake on slower networks;
        // the parallel scheduler isolates this wait from established peers.
        if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err("Download interrupted".into());
        }
        let connect_timeout = if stop.is_some() {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(2)
        };
        let connect_timeout = match end {
            Some(deadline) => connect_timeout.min(
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or("Torrent metadata deadline exceeded")?,
            ),
            None => connect_timeout,
        };
        let connection = TcpStream::connect_timeout(&address, connect_timeout);
        if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err("Download interrupted".into());
        }
        let mut stream = connection.map_err(|_| "Could not connect to peer")?;
        stream
            .set_nodelay(true)
            .map_err(|_| "Could not configure TCP")?;
        let deadline = end
            .unwrap_or(Instant::now() + Duration::from_secs(5))
            .min(Instant::now() + Duration::from_secs(5));
        let extensions = handshake_deadline(&mut stream, hash, id, true, stop, deadline)?;
        if extensions {
            write_message_deadline(
                &mut stream,
                20,
                &extended_handshake(meta, port, pex),
                stop,
                deadline,
            )?;
        }
        write_message_deadline(&mut stream, 2, &[], stop, deadline)?;
        Ok(Self {
            stream,
            v2_wire,
            counters,
            download_gate,
            local_download_gate,
            metadata_id: None,
            pex_id: None,
            metadata_size: None,
            bitfield: Vec::new(),
            choked: true,
            extensions,
            discovered: Vec::new(),
            pex_allowed: pex && meta.is_some_and(|m| !m.private),
            piece_count: meta.map(Meta::count),
            bitfield_received: false,
            availability_known: false,
        })
    }

    /// Read the initial availability and unchoke before assigning any payload
    /// piece. Peers which omit a bitfield retain unknown availability; an
    /// advertised all-zero bitfield is explicitly unavailable.
    pub fn prepare_download(&mut self, meta: &Meta, stop: &AtomicBool) -> Result<()> {
        self.bind_metadata(meta)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        for _ in 0..128 {
            if stop.load(Ordering::Acquire) {
                return Err("Download interrupted".into());
            }
            if !self.choked {
                return Ok(());
            }
            let (id, payload) = read_message_deadline(&mut self.stream, Some(stop), deadline)?;
            self.ancillary(id, &payload)?;
        }
        Err("Peer keeps the download choked".into())
    }

    pub fn availability(&self) -> Option<&[u8]> {
        self.availability_known.then_some(self.bitfield.as_slice())
    }

    pub fn has_piece(&self, index: usize) -> bool {
        self.piece_count.is_none_or(|count| index < count)
            && (!self.availability_known
                || self
                    .bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0))
    }

    fn bind_metadata(&mut self, meta: &Meta) -> Result<()> {
        let count = meta.count();
        if self.piece_count.is_some_and(|old| old != count) {
            return Err("Peer metadata piece count changed".into());
        }
        if self.bitfield_received {
            validate_bitfield(&self.bitfield, Some(count))?;
        } else if self.bitfield.len() > count.div_ceil(8)
            || !valid_spare_bits(&self.bitfield, count)
        {
            return Err("Have index exceeds metadata piece count".into());
        }
        self.piece_count = Some(count);
        Ok(())
    }

    fn ancillary(&mut self, id: u8, payload: &[u8]) -> Result<()> {
        match id {
            0 => {
                if !payload.is_empty() {
                    return Err("Invalid choke message".into());
                }
                self.choked = true;
            }
            1 => {
                if !payload.is_empty() {
                    return Err("Invalid unchoke message".into());
                }
                self.choked = false;
            }
            4 => {
                if payload.len() != 4 {
                    return Err("Invalid have message".into());
                }
                let n = u32::from_be_bytes(payload.try_into().map_err(|_| "Invalid have index")?)
                    as usize;
                if n >= 8 * 1024 * 1024 {
                    return Err("Have index exceeds limits".into());
                }
                if self.piece_count.is_some_and(|count| n >= count) {
                    return Err("Have index exceeds metadata piece count".into());
                }
                if self.bitfield.len() <= n / 8 {
                    self.bitfield.resize(n / 8 + 1, 0);
                }
                self.bitfield[n / 8] |= 0x80 >> (n % 8);
                self.availability_known = true;
            }
            5 => {
                if self.bitfield_received {
                    return Err("Peer sent a repeated bitfield".into());
                }
                validate_bitfield(payload, self.piece_count)?;
                self.bitfield = payload.to_vec();
                self.bitfield_received = true;
                self.availability_known = true;
            }
            20 if payload.first() == Some(&0) => {
                let v = crate::bencode::parse(&payload[1..])?;
                if let Some(m) = value_field(&v, b"m") {
                    for (key, slot) in [
                        (b"ut_metadata".as_slice(), &mut self.metadata_id),
                        (b"ut_pex".as_slice(), &mut self.pex_id),
                    ] {
                        if let Some(Value::Int(n)) = value_field(m, key) {
                            *slot = u8::try_from(*n).ok().filter(|n| *n != 0);
                        }
                    }
                }
                if let Some(Value::Int(n)) = value_field(&v, b"metadata_size") {
                    let n = usize::try_from(*n).map_err(|_| "Invalid metadata size")?;
                    if !(1..=MAX_META).contains(&n) {
                        return Err("Peer metadata is too large".into());
                    }
                    self.metadata_size = Some(n);
                }
            }
            20 if payload.first() == Some(&EXT_PEX) && self.pex_allowed => {
                self.discovered
                    .extend(super::discovery::pex_peers(&payload[1..])?);
                self.discovered.truncate(1000);
            }
            _ => {}
        }
        Ok(())
    }
    pub fn metadata(
        &mut self,
        expected_v1: Option<[u8; 20]>,
        expected_v2: Option<[u8; 32]>,
        stop: &AtomicBool,
    ) -> Result<Meta> {
        self.metadata_before(expected_v1, expected_v2, stop, None)
    }

    pub fn metadata_before(
        &mut self,
        expected_v1: Option<[u8; 20]>,
        expected_v2: Option<[u8; 32]>,
        stop: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Meta> {
        if !self.extensions {
            return Err("Peer does not provide metadata".into());
        }
        for _ in 0..64 {
            if stop.load(Ordering::Relaxed) {
                return Err("Download interrupted".into());
            }
            if self.metadata_id.is_some() && self.metadata_size.is_some() {
                break;
            }
            let (id, payload) = read_message_deadline(
                &mut self.stream,
                Some(stop),
                deadline.unwrap_or(Instant::now() + Duration::from_secs(5)),
            )?;
            self.ancillary(id, &payload)?;
        }
        let ext = self.metadata_id.ok_or("Missing metadata extension")?;
        let size = self.metadata_size.ok_or("Missing metadata size")?;
        let mut info = vec![0; size];
        for piece in 0..size.div_ceil(BLOCK) {
            if stop.load(Ordering::Relaxed) {
                return Err("Download interrupted".into());
            }
            let request = value_dict(&[
                (b"msg_type", Value::Int(0)),
                (b"piece", Value::Int(piece as i64)),
            ]);
            let mut request_bytes = vec![ext];
            request_bytes.extend(crate::bencode::encode(&request));
            write_message_deadline(
                &mut self.stream,
                20,
                &request_bytes,
                Some(stop),
                deadline.unwrap_or(Instant::now() + Duration::from_secs(5)),
            )?;
            let mut received = false;
            for _ in 0..128 {
                if stop.load(Ordering::Relaxed) {
                    return Err("Download interrupted".into());
                }
                let (id, payload) = read_message_deadline(
                    &mut self.stream,
                    Some(stop),
                    deadline.unwrap_or(Instant::now() + Duration::from_secs(5)),
                )?;
                if id == 20 && payload.first() == Some(&EXT_METADATA) {
                    let (header, length) = crate::bencode::parse_prefix(&payload[1..])?;
                    let kind = match value_field(&header, b"msg_type") {
                        Some(Value::Int(n)) => *n,
                        _ => return Err("Invalid metadata response".into()),
                    };
                    let index = match value_field(&header, b"piece") {
                        Some(Value::Int(n)) => {
                            usize::try_from(*n).map_err(|_| "Invalid metadata index")?
                        }
                        _ => return Err("Missing metadata index".into()),
                    };
                    if index != piece {
                        continue;
                    }
                    if kind == 2 {
                        return Err("Peer rejected metadata request".into());
                    }
                    if kind != 1 {
                        continue;
                    }
                    let actual_size = match value_field(&header, b"total_size") {
                        Some(Value::Int(n)) => {
                            usize::try_from(*n).map_err(|_| "Invalid metadata size")?
                        }
                        _ => return Err("Missing metadata size".into()),
                    };
                    let block = &payload[1 + length..];
                    let start = piece * BLOCK;
                    let n = (size - start).min(BLOCK);
                    if actual_size != size || block.len() != n {
                        return Err("Metadata block mismatch".into());
                    }
                    info[start..start + n].copy_from_slice(block);
                    received = true;
                    break;
                }
                self.ancillary(id, &payload)?;
            }
            if !received {
                return Err("Missing metadata block".into());
            }
        }
        if expected_v1.is_some_and(|v| crate::crypto::sha1(&info) != v)
            || expected_v2.is_some_and(|v| crate::crypto::sha256(&info) != v)
        {
            return Err("Metadata hash mismatch".into());
        }
        let mut encoded = b"d4:info".to_vec();
        encoded.extend_from_slice(&info);
        encoded.push(b'e');
        let meta = Meta::from_info(info, encoded)?;
        self.bind_metadata(&meta)?;
        Ok(meta)
    }
    pub fn fetch_piece(
        &mut self,
        meta: &mut Meta,
        index: usize,
        stop: &AtomicBool,
    ) -> Result<Vec<u8>> {
        if index >= meta.count() {
            return Err("Invalid piece index".into());
        }
        self.prepare_download(meta, stop)?;
        if !self.has_piece(index) {
            return Err("Peer does not advertise the requested piece".into());
        }
        if meta.v2.is_some() && meta.v2_pieces[index].is_none() && meta.pieces[index].is_none() {
            self.fetch_hashes(meta, index, stop)?;
        }
        let n = meta.wire_piece_size(index, self.v2_wire);
        let mut data = vec![0; meta.piece_size(index)];
        let mut received = vec![false; n.div_ceil(BLOCK)];
        let mut next = 0usize;
        let mut pending = 0usize;
        let mut deadline = Instant::now() + Duration::from_secs(120);
        let mut messages = received.len() * 8 + 256;
        while received.iter().any(|v| !v) {
            if Instant::now() >= deadline || messages == 0 {
                return Err("Peer did not provide the requested piece".into());
            }
            messages -= 1;
            if stop.load(Ordering::Relaxed) {
                return Err("Download interrupted".into());
            }
            // Limited requests stay at one outstanding block. Waiting for the
            // next reservation never leaves old responses unread in a pipeline.
            let pipeline =
                if self.download_gate.limit() == 0 && self.local_download_gate.limit() == 0 {
                    16
                } else {
                    1
                };
            while next < received.len() && pending < pipeline {
                let offset = next * BLOCK;
                let length = (n - offset).min(BLOCK);
                let waiting = Instant::now();
                reserve_payload(
                    &mut self.stream,
                    &self.download_gate,
                    &self.local_download_gate,
                    length as u64,
                    stop,
                )?;
                // Rate waits are outside both the piece's network deadline and
                // the fresh per-message deadline established below.
                deadline = deadline
                    .checked_add(waiting.elapsed())
                    .ok_or("Peer deadline overflow")?;
                let mut request = Vec::with_capacity(12);
                request.extend_from_slice(&(index as u32).to_be_bytes());
                request.extend_from_slice(&(offset as u32).to_be_bytes());
                request.extend_from_slice(&(length as u32).to_be_bytes());
                write_message(&mut self.stream, 6, &request, Some(stop))?;
                next += 1;
                pending += 1;
            }
            let reading = Instant::now();
            let (id, payload) = read_message(&mut self.stream, Some(stop))?;
            if id == 255 && reading.elapsed() >= Duration::from_secs(1) {
                // A deliberately paced peer keeps its connection alive while
                // waiting for payload credit. Rapid keepalives still consume
                // the bounded message budget, preventing a busy-message loop.
                deadline = deadline
                    .checked_add(reading.elapsed())
                    .ok_or("Peer deadline overflow")?;
                messages += 1;
                continue;
            }
            if id == 7 {
                if payload.len() < 8 {
                    return Err("Truncated piece block".into());
                }
                let piece =
                    u32::from_be_bytes(payload[..4].try_into().map_err(|_| "Invalid piece index")?)
                        as usize;
                let offset = u32::from_be_bytes(
                    payload[4..8]
                        .try_into()
                        .map_err(|_| "Invalid piece offset")?,
                ) as usize;
                if piece != index
                    || !offset.is_multiple_of(BLOCK)
                    || offset >= n
                    || payload.len() - 8 != (n - offset).min(BLOCK)
                    || offset / BLOCK >= next
                {
                    return Err("Unsolicited or inconsistent piece block".into());
                }
                super::add_payload(&self.counters.downloaded, (payload.len() - 8) as u64);
                if !received[offset / BLOCK] {
                    data[offset..offset + payload.len() - 8].copy_from_slice(&payload[8..]);
                    received[offset / BLOCK] = true;
                    pending -= 1;
                }
            } else {
                self.ancillary(id, &payload)?;
            }
        }
        if !meta.verify(index, &data) {
            return Err("Piece hash mismatch".into());
        }
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        // A peer may close after sending its final permitted block. HAVE is an
        // advisory announcement and cannot invalidate the verified payload.
        let _ = write_message(
            &mut self.stream,
            4,
            &(index as u32).to_be_bytes(),
            Some(stop),
        );
        if stop.load(Ordering::Acquire) {
            return Err("Download interrupted".into());
        }
        Ok(data)
    }
    pub fn fetch_hashes(&mut self, meta: &mut Meta, piece: usize, stop: &AtomicBool) -> Result<()> {
        let file = meta.v2_file(piece).ok_or("Missing v2 file")?.clone();
        let root = file.root.ok_or("Missing v2 root")?;
        let count = file.length.div_ceil(meta.piece_length as u64) as usize;
        if count <= 1 {
            return Err("V2 hash mismatch".into());
        }
        let file_start = file.offset as usize / meta.piece_length;
        let index = piece - file_start;
        // Ask for one leaf with a full authentication path to the file root.
        let base = (meta.piece_length / BLOCK).ilog2();
        let height = count.next_power_of_two().ilog2();
        let proof = height - 1;
        let first = index & !1;
        let mut request = Vec::with_capacity(48);
        request.extend_from_slice(&root);
        request.extend_from_slice(&base.to_be_bytes());
        request.extend_from_slice(&(first as u32).to_be_bytes());
        request.extend_from_slice(&2u32.to_be_bytes());
        request.extend_from_slice(&proof.to_be_bytes());
        write_message(&mut self.stream, 21, &request, Some(stop))?;
        for _ in 0..128 {
            if stop.load(Ordering::Relaxed) {
                return Err("Download interrupted".into());
            }
            let (id, payload) = read_message(&mut self.stream, Some(stop))?;
            if id == 23 && payload == request {
                return Err("Peer rejected v2 proof request".into());
            }
            if id == 22 {
                if payload.len() != 48 + (2 + proof as usize) * 32 || payload[..48] != request {
                    return Err("Unsolicited or inconsistent v2 proof".into());
                }
                let mut left = [0; 32];
                left.copy_from_slice(&payload[48..80]);
                let mut right = [0; 32];
                right.copy_from_slice(&payload[80..112]);
                if first + 1 >= count && right != zero_hash(meta.piece_length / BLOCK) {
                    return Err("Invalid v2 proof padding".into());
                }
                let mut hash = pair(&left, &right);
                for (level, sibling) in payload[112..].as_chunks::<32>().0.iter().enumerate() {
                    let mut s = [0; 32];
                    s.copy_from_slice(sibling);
                    hash = if first & (1 << (level + 1)) == 0 {
                        pair(&hash, &s)
                    } else {
                        pair(&s, &hash)
                    };
                }
                if hash != root {
                    return Err("V2 proof has not been authenticated".into());
                }
                meta.v2_pieces[file_start + first] = Some(left);
                if first + 1 < count {
                    meta.v2_pieces[file_start + first + 1] = Some(right);
                }
                return Ok(());
            }
            self.ancillary(id, &payload)?;
        }
        Err("Missing v2 piece proof".into())
    }
}

pub fn hash_response(
    meta: &Meta,
    request: &[u8],
    storage: Option<&std::path::Path>,
) -> Result<Vec<u8>> {
    if request.len() != 48 {
        return Err("Invalid v2 proof request".into());
    }
    let mut root = [0; 32];
    root.copy_from_slice(&request[..32]);
    let get = |i| -> Result<usize> {
        Ok(u32::from_be_bytes(
            request[i..i + 4]
                .try_into()
                .map_err(|_| "Invalid v2 request")?,
        ) as usize)
    };
    let base = get(32)?;
    let index = get(36)?;
    let length = get(40)?;
    let proof = get(44)?;
    let file = meta
        .files
        .iter()
        .find(|f| f.root == Some(root))
        .ok_or("Unknown v2 root")?;
    let piece_base = (meta.piece_length / BLOCK).ilog2() as usize;
    let blocks = file.length.div_ceil(BLOCK as u64) as usize;
    let height = blocks.max(1).next_power_of_two().ilog2() as usize;
    if (base != 0 && base != piece_base)
        || base > height
        || !(2..=512).contains(&length)
        || !length.is_power_of_two()
        || index % length != 0
    {
        return Err("Invalid v2 proof geometry".into());
    }
    let width = 1usize << (height - base);
    let subtree = length.ilog2() as usize;
    if proof > height.saturating_sub(base + 1)
        || index >= blocks.div_ceil(1 << base)
        || index.checked_add(length).is_none_or(|v| v > width)
    {
        return Err("Invalid v2 proof range".into());
    }
    fn hash_at(
        meta: &Meta,
        file: &super::metainfo::MediaFile,
        layer: usize,
        index: usize,
        storage: Option<&std::path::Path>,
    ) -> Result<[u8; 32]> {
        let piece_base = (meta.piece_length / BLOCK).ilog2() as usize;
        if layer >= piece_base {
            let count = file.length.div_ceil(meta.piece_length as u64) as usize;
            let start = file.offset as usize / meta.piece_length;
            let width = 1usize << (layer - piece_base);
            let first = index * width;
            let mut hashes = Vec::with_capacity(width);
            for i in first..first + width {
                hashes.push(if i < count {
                    meta.v2_pieces[start + i].ok_or("Incomplete v2 layer")?
                } else {
                    zero_hash(meta.piece_length / BLOCK)
                });
            }
            return Ok(super::metainfo::merkle_hashes(
                &hashes,
                zero_hash(meta.piece_length / BLOCK),
            ));
        }
        let width = 1usize << layer;
        let offset = index as u64 * width as u64 * BLOCK as u64;
        if offset >= file.length {
            return Ok(zero_hash(width));
        }
        let storage = storage.ok_or("Block proof requires file data")?;
        let path = super::metainfo::confined(storage, &file.path)?;
        super::inspect_media_file(&path)?;
        let mut input = std::fs::File::open(path).map_err(|_| "Could not read v2 blocks")?;
        let length = (file.length - offset).min((width * BLOCK) as u64) as usize;
        let mut data = vec![0; length];
        use std::io::Seek;
        input
            .seek(std::io::SeekFrom::Start(offset))
            .and_then(|_| input.read_exact(&mut data))
            .map_err(|_| "Truncated v2 block read")?;
        Ok(super::metainfo::merkle_data(&data, width))
    }
    let mut response = request.to_vec();
    for i in index..index + length {
        response.extend_from_slice(&hash_at(meta, file, base, i, storage)?);
    }
    for layer in base + subtree..(base + proof + 1).min(height) {
        let position = index >> (layer - base);
        response.extend_from_slice(&hash_at(meta, file, layer, position ^ 1, storage)?);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_pair(meta: Option<&Meta>) -> (Peer, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let stream = TcpStream::connect(listener.local_addr().expect("address")).expect("client");
        let (remote, _) = listener.accept().expect("peer");
        (
            Peer {
                stream,
                metadata_id: None,
                pex_id: None,
                metadata_size: None,
                bitfield: Vec::new(),
                choked: true,
                extensions: false,
                discovered: Vec::new(),
                pex_allowed: false,
                v2_wire: false,
                counters: Arc::new(super::super::TransferCounters::default()),
                download_gate: Arc::new(super::super::RateGate::default()),
                local_download_gate: Arc::new(super::super::RateGate::default()),
                piece_count: meta.map(Meta::count),
                bitfield_received: false,
                availability_known: false,
            },
            remote,
        )
    }

    fn three_piece_meta() -> Meta {
        let info = value_dict(&[
            (b"name", Value::Bytes(b"availability.bin".to_vec())),
            (b"piece length", Value::Int(BLOCK as i64)),
            (b"length", Value::Int((BLOCK * 3) as i64)),
            (b"pieces", Value::Bytes(vec![1; 60])),
        ]);
        Meta::parse(&crate::bencode::encode(&value_dict(&[(b"info", info)])))
            .expect("synthetic metadata")
    }

    #[test]
    fn cancelled_startup_does_not_connect_or_wait_for_a_silent_handshake() {
        use std::{net::TcpListener, sync::mpsc};
        let settings = || PeerSettings {
            meta: None,
            port: 6881,
            pex: false,
            v2_wire: false,
            counters: Arc::new(super::super::TransferCounters::default()),
            download_gate: Arc::new(super::super::RateGate::default()),
            local_download_gate: Arc::new(super::super::RateGate::default()),
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        assert!(
            Peer::connect_cancellable(
                listener.local_addr().expect("address"),
                &[1; 20],
                &[2; 20],
                settings(),
                &AtomicBool::new(true),
            )
            .is_err()
        );
        assert_eq!(
            listener
                .accept()
                .expect_err("no connection after cancellation")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );

        listener.set_nonblocking(false).expect("blocking listener");
        let address = listener.local_addr().expect("address");
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        let (ready, received) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("peer");
            read_exact_deadline(
                &mut stream,
                &mut [0; 68],
                None,
                Instant::now() + Duration::from_secs(2),
            )
            .expect("outgoing handshake");
            ready.send(()).expect("announce pending handshake");
            assert!(
                read_exact_deadline(
                    &mut stream,
                    &mut [0; 1],
                    None,
                    Instant::now() + Duration::from_secs(2),
                )
                .is_err()
            );
        });
        let canceller = std::thread::spawn(move || {
            received
                .recv_timeout(Duration::from_secs(2))
                .expect("handshake started");
            cancel.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let error = Peer::connect_cancellable(address, &[1; 20], &[2; 20], settings(), &flag)
            .err()
            .expect("silent handshake must be interrupted");
        assert_eq!(error, "Download interrupted");
        assert!(started.elapsed() < Duration::from_secs(2));
        canceller.join().expect("canceller");
        server.join().expect("server");
    }

    #[test]
    fn sparse_availability_is_read_before_assigning_a_piece() {
        let meta = three_piece_meta();
        let (mut peer, mut remote) = peer_pair(Some(&meta));
        let server = std::thread::spawn(move || {
            write_message(&mut remote, 5, &[0x40], None).expect("sparse bitfield");
            write_message(&mut remote, 1, &[], None).expect("unchoke");
        });
        peer.prepare_download(&meta, &AtomicBool::new(false))
            .expect("initial availability");
        server.join().expect("server");
        assert_eq!(peer.availability(), Some([0x40].as_slice()));
        assert!(!peer.has_piece(0));
        assert!(peer.has_piece(1));
        assert!(!peer.has_piece(2));
        assert!(!peer.has_piece(3));

        let (mut peer, _remote) = peer_pair(Some(&meta));
        assert!(peer.has_piece(0), "omitted availability remains unknown");
        assert_eq!(peer.availability(), None);
        peer.ancillary(5, &[0])
            .expect("explicit empty availability");
        assert!(
            !peer.has_piece(0),
            "an all-zero bitfield is known unavailable"
        );
        assert_eq!(peer.availability(), Some([0].as_slice()));
    }

    #[test]
    fn authenticated_metadata_rejects_malformed_or_out_of_range_availability() {
        let meta = three_piece_meta();
        for bitfield in [vec![], vec![0, 0], vec![0x21]] {
            let (mut peer, _remote) = peer_pair(Some(&meta));
            assert!(peer.ancillary(5, &bitfield).is_err());
            assert_eq!(
                peer.availability(),
                None,
                "invalid payload must not mutate availability"
            );
        }
        let (mut peer, _remote) = peer_pair(Some(&meta));
        assert!(peer.ancillary(4, &3u32.to_be_bytes()).is_err());
        assert!(peer.ancillary(4, &[0; 3]).is_err());
        assert_eq!(peer.availability(), None);
        peer.ancillary(5, &[0x20]).expect("valid bitfield");
        assert!(
            peer.ancillary(5, &[0x40]).is_err(),
            "repeated bitfield is invalid"
        );
        assert_eq!(peer.availability(), Some([0x20].as_slice()));
        peer.ancillary(4, &1u32.to_be_bytes()).expect("valid have");
        assert!(peer.has_piece(1));

        for before_metadata in [false, true] {
            let (mut peer, _remote) = peer_pair(None);
            if before_metadata {
                peer.ancillary(5, &[0x10])
                    .expect("bounded unauthenticated bitfield");
            } else {
                peer.ancillary(4, &3u32.to_be_bytes())
                    .expect("bounded unauthenticated have");
            }
            assert!(
                peer.bind_metadata(&meta).is_err(),
                "authentication must revalidate availability"
            );
        }
        assert!(validate_bitfield(&vec![0; 1024 * 1024 + 1], None).is_err());
    }

    #[test]
    fn verified_final_piece_survives_a_failed_have_announcement() {
        use std::net::{Shutdown, TcpListener};
        for length in [32, BLOCK * 3 + 5] {
            let payload: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
            let info = value_dict(&[
                (b"name", Value::Bytes(b"final.bin".to_vec())),
                (b"piece length", Value::Int((BLOCK * 4) as i64)),
                (b"length", Value::Int(payload.len() as i64)),
                (
                    b"pieces",
                    Value::Bytes(crate::crypto::sha1(&payload).to_vec()),
                ),
            ]);
            let mut meta = Meta::parse(&crate::bencode::encode(&value_dict(&[(b"info", info)])))
                .expect("synthetic metadata");
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let stream =
                TcpStream::connect(listener.local_addr().expect("address")).expect("client");
            let (mut remote, _) = listener.accept().expect("peer");
            let client_writer = stream.try_clone().expect("shared client socket");
            let expected = payload.clone();
            let server = std::thread::spawn(move || {
                let mut requests = Vec::new();
                for block in 0..payload.len().div_ceil(BLOCK) {
                    let (id, request) = read_message(&mut remote, None).expect("block request");
                    assert_eq!(id, 6);
                    assert_eq!(request.len(), 12);
                    assert_eq!(u32::from_be_bytes(request[..4].try_into().unwrap()), 0);
                    assert_eq!(
                        u32::from_be_bytes(request[4..8].try_into().unwrap()) as usize,
                        block * BLOCK
                    );
                    assert_eq!(
                        u32::from_be_bytes(request[8..12].try_into().unwrap()) as usize,
                        (payload.len() - block * BLOCK).min(BLOCK)
                    );
                    requests.push(request);
                }
                // Disable announcements only after all pipelined requests are
                // received. Reads remain available for every final block.
                client_writer
                    .shutdown(Shutdown::Write)
                    .expect("disable announcements");
                for (request, payload) in requests.iter().zip(payload.chunks(BLOCK)) {
                    let mut block = request[..8].to_vec();
                    block.extend_from_slice(payload);
                    write_message(&mut remote, 7, &block, None).expect("final block");
                }
            });
            let counters = Arc::new(super::super::TransferCounters::default());
            let mut peer = Peer {
                stream,
                metadata_id: None,
                pex_id: None,
                metadata_size: None,
                bitfield: Vec::new(),
                choked: false,
                extensions: false,
                discovered: Vec::new(),
                pex_allowed: false,
                v2_wire: false,
                counters: counters.clone(),
                download_gate: Arc::new(super::super::RateGate::default()),
                local_download_gate: Arc::new(super::super::RateGate::default()),
                piece_count: Some(meta.count()),
                bitfield_received: false,
                availability_known: false,
            };
            let result = peer.fetch_piece(&mut meta, 0, &AtomicBool::new(false));
            server.join().expect("server");
            assert_eq!(result.expect("verified piece is retained"), expected);
            assert_eq!(
                counters.downloaded.load(Ordering::Relaxed),
                expected.len() as u64
            );
            assert!(
                write_message(&mut peer.stream, 4, &0u32.to_be_bytes(), None).is_err(),
                "the write half must remain unavailable for HAVE"
            );
        }
    }

    #[test]
    fn slow_drip_read_obeys_an_absolute_deadline_and_cancellation() {
        use std::net::TcpListener;
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let address = listener.local_addr().expect("address");
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("peer");
                for _ in 0..100 {
                    if stream.write_all(&[1]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            let mut stream = TcpStream::connect(address).expect("peer");
            let flag = Arc::new(AtomicBool::new(false));
            let canceller = if cancel {
                let flag = flag.clone();
                Some(std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(40));
                    flag.store(true, Ordering::Release);
                }))
            } else {
                None
            };
            let started = Instant::now();
            let deadline = started
                + if cancel {
                    Duration::from_secs(30)
                } else {
                    Duration::from_millis(100)
                };
            assert!(
                read_exact_deadline(&mut stream, &mut [0; 4096], Some(&flag), deadline).is_err()
            );
            assert!(started.elapsed() < Duration::from_millis(500));
            drop(stream);
            if let Some(canceller) = canceller {
                canceller.join().expect("cancel");
            }
            server.join().expect("server");
        }
    }
    #[test]
    fn malicious_hash_request_is_rejected() {
        let info = value_dict(&[
            (b"name", Value::Bytes(b"x".to_vec())),
            (b"piece length", Value::Int(BLOCK as i64)),
            (b"length", Value::Int(0)),
            (b"pieces", Value::Bytes(Vec::new())),
        ]);
        let encoded = crate::bencode::encode(&value_dict(&[(b"info", info)]));
        let meta = Meta::parse(&encoded).expect("empty torrent");
        assert!(hash_response(&meta, &[0; 48], None).is_err());
    }

    #[test]
    fn real_tcp_pex_is_accepted_only_for_public_metadata() {
        use std::net::TcpListener;
        for private in [false, true] {
            let info = value_dict(&[
                (b"name", Value::Bytes(b"x".to_vec())),
                (b"piece length", Value::Int(BLOCK as i64)),
                (b"length", Value::Int(0)),
                (b"pieces", Value::Bytes(Vec::new())),
                (b"private", Value::Int(i64::from(private))),
            ]);
            let meta = Meta::parse(&crate::bencode::encode(&value_dict(&[(b"info", info)])))
                .expect("meta");
            let hash = meta.v1.expect("v1");
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let address = listener.local_addr().expect("address");
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("peer");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("timeout");
                handshake(&mut stream, &hash, &[2; 20], false).expect("handshake");
                let (id, body) = read_message(&mut stream, None).expect("extended");
                assert_eq!(id, 20);
                let ext = crate::bencode::parse(&body[1..]).expect("ext");
                assert_eq!(
                    value_field(value_field(&ext, b"m").expect("m"), b"ut_pex").is_some(),
                    !private
                );
                assert_eq!(read_message(&mut stream, None).expect("interested").0, 2);
                let payload =
                    value_dict(&[(b"added", Value::Bytes(vec![127, 0, 0, 1, 0x1a, 0xe1]))]);
                let mut body = vec![EXT_PEX];
                body.extend(crate::bencode::encode(&payload));
                write_message(&mut stream, 20, &body, None).expect("pex");
            });
            let mut peer = Peer::connect(
                address,
                &hash,
                &[1; 20],
                PeerSettings {
                    meta: Some(&meta),
                    port: 6882,
                    pex: true,
                    v2_wire: false,
                    counters: Arc::new(super::super::TransferCounters::default()),
                    download_gate: Arc::new(super::super::RateGate::default()),
                    local_download_gate: Arc::new(super::super::RateGate::default()),
                },
            )
            .expect("peer");
            let (id, body) = read_message(&mut peer.stream, None).expect("pex");
            peer.ancillary(id, &body).expect("accept");
            if private {
                assert!(peer.discovered.is_empty());
            } else {
                assert_eq!(
                    peer.discovered,
                    vec!["127.0.0.1:6881".parse().expect("address")]
                );
            }
            worker.join().expect("worker");
        }
    }
}
