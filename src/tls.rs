//! Original TLS 1.3 client: X25519, ChaCha20-Poly1305, RSA-PSS and P-256/P-384.
//! Certificate authentication is mandatory; unsupported algorithms fail closed.
use crate::Result;
use crate::crypto::{
    ChaCha20Poly1305, Sha256, hkdf_expand, hkdf_extract, hmac_sha256, sha256, x25519,
    x25519_public_key,
};
use crate::net::DeadlineStream;
use crate::pki::{
    PublicKey, validate_chain, verify_ecdsa_p256_sha256, verify_ecdsa_p384_sha384,
    verify_pss_sha256, verify_pss_sha384,
};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_RECORD: usize = 16_384 + 256;
const MAX_HANDSHAKE: usize = 262_144;

pub struct TlsStream {
    stream: DeadlineStream,
    send: RecordKeys,
    receive: RecordKeys,
    pending: Vec<u8>,
    offset: usize,
    post_handshake: Vec<u8>,
    closed: bool,
}

struct RecordKeys {
    secret: [u8; 32],
    key: [u8; 32],
    iv: [u8; 12],
    sequence: u64,
}

impl RecordKeys {
    fn new(secret: [u8; 32]) -> Result<Self> {
        let key = expand_label(&secret, "key", &[], 32)?
            .try_into()
            .map_err(|_| "Invalid TLS key")?;
        let iv = expand_label(&secret, "iv", &[], 12)?
            .try_into()
            .map_err(|_| "Invalid TLS nonce")?;
        Ok(Self {
            secret,
            key,
            iv,
            sequence: 0,
        })
    }

    fn nonce(&self) -> [u8; 12] {
        let mut nonce = self.iv;
        for (out, input) in nonce[4..].iter_mut().zip(self.sequence.to_be_bytes()) {
            *out ^= input;
        }
        nonce
    }

    fn advance(&mut self) -> Result<()> {
        self.sequence = self.sequence.checked_add(1).ok_or("TLS sequence counter exhausted")?;
        Ok(())
    }

    fn update(&mut self) -> Result<()> {
        let secret = expand_label(&self.secret, "traffic upd", &[], 32)?
            .try_into()
            .map_err(|_| "Invalid TLS secret")?;
        *self = Self::new(secret)?;
        Ok(())
    }

    fn seal(&mut self, kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() > 16_384 {
            return Err("TLS record to send is too large".into());
        }
        let mut inner = Vec::with_capacity(payload.len() + 1);
        inner.extend_from_slice(payload);
        inner.push(kind);
        let length = u16::try_from(inner.len() + 16).map_err(|_| "TLS record is too large")?;
        let mut record = vec![23, 3, 3];
        record.extend_from_slice(&length.to_be_bytes());
        let ciphertext = ChaCha20Poly1305::new(self.key).seal(self.nonce(), &record, &inner)?;
        self.advance()?;
        record.extend_from_slice(&ciphertext);
        Ok(record)
    }

    fn open(&mut self, header: &[u8; 5], ciphertext: &[u8]) -> Result<(u8, Vec<u8>)> {
        if header[0] != 23 || header[1..3] != [3, 3] || ciphertext.len() < 17 {
            return Err("Invalid encrypted TLS record".into());
        }
        let mut plaintext = ChaCha20Poly1305::new(self.key)
            .open(self.nonce(), header, ciphertext)
            .map_err(|_| "TLS record authentication failed")?;
        self.advance()?;
        while plaintext.last() == Some(&0) {
            plaintext.pop();
        }
        let kind = plaintext.pop().ok_or("TLS record has no inner content type")?;
        if plaintext.len() > 16_384 || !matches!(kind, 21..=23) {
            return Err("Invalid TLS record type or length".into());
        }
        Ok((kind, plaintext))
    }
}

impl TlsStream {
    pub fn connect(stream: TcpStream, hostname: &str) -> Result<Self> {
        let timeout = stream
            .read_timeout()
            .map_err(|_| "TLS TCP timeout is unavailable")?
            .unwrap_or(Duration::from_secs(30));
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("TLS timeout is too large")?;
        Self::connect_stream(DeadlineStream::new(stream, deadline), hostname)
    }

    pub(crate) fn connect_stream(stream: DeadlineStream, hostname: &str) -> Result<Self> {
        let roots = load_roots()?;
        Self::connect_with_roots(stream, hostname, &roots)
    }

    fn connect_with_roots(
        mut stream: DeadlineStream,
        hostname: &str,
        roots: &[u8],
    ) -> Result<Self> {
        let mut random = [0; 96];
        secure_random(&mut random)?;
        let mut secret = [0; 32];
        secret.copy_from_slice(&random[..32]);
        let public = x25519_public_key(secret);
        let client_random: [u8; 32] = random[32..64].try_into().map_err(|_| "Invalid TLS random bytes")?;
        let session_id: [u8; 32] = random[64..].try_into().map_err(|_| "Invalid TLS random bytes")?;
        let hello = client_hello(hostname, &client_random, &session_id, &public)?;
        let mut transcript = Sha256::new();
        transcript.update(&hello);
        write_plain_record(&mut stream, 22, &hello)?;
        let server_hello = read_server_hello(&mut stream)?;
        let peer = parse_server_hello(&server_hello, &session_id)?;
        transcript.update(&server_hello);
        let shared = x25519(secret, peer)?;
        secret.fill(0);
        random.fill(0);
        let zeros = [0; 32];
        let early = hkdf_extract(&zeros, &zeros);
        let derived = derive_secret(&early, "derived", &sha256(&[]))?;
        let handshake = hkdf_extract(&derived, &shared);
        let digest = transcript.clone().finalize();
        let send_secret = derive_secret(&handshake, "c hs traffic", &digest)?;
        let receive_secret = derive_secret(&handshake, "s hs traffic", &digest)?;
        let mut send_hs = RecordKeys::new(send_secret)?;
        let mut receive_hs = RecordKeys::new(receive_secret)?;
        let mut buffered = Vec::new();
        let mut stage = 0;
        let mut leaf_key = None;
        let mut finished = false;
        for _ in 0..256 {
            while buffered.len() >= 4 {
                let length = u24(&buffered[1..4]);
                if length > MAX_HANDSHAKE {
                    return Err("TLS handshake message is too large".into());
                }
                if buffered.len() < length + 4 {
                    break;
                }
                let message: Vec<u8> = buffered.drain(..length + 4).collect();
                match (stage, message[0]) {
                    (0, 8) => {
                        parse_encrypted_extensions(&message[4..])?;
                        stage = 1;
                    }
                    (1, 11) => {
                        let chain = parse_certificates(&message[4..])?;
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| "Invalid system clock for TLS")?
                            .as_secs();
                        leaf_key = Some(validate_chain(&chain, roots, hostname, now)?);
                        stage = 2;
                    }
                    (2, 15) => {
                        let key = leaf_key.as_ref().ok_or("Missing TLS certificate")?;
                        verify_certificate_signature(key, &message[4..], &transcript)?;
                        stage = 3;
                    }
                    (3, 20) => {
                        if message.len() != 36 {
                            return Err("Invalid TLS Finished message".into());
                        }
                        let key = expand_label(&receive_secret, "finished", &[], 32)?;
                        let expected = hmac_sha256(&key, &transcript.clone().finalize());
                        if !constant_time_equal(&expected, &message[4..]) {
                            return Err("TLS Finished authentication failed".into());
                        }
                        finished = true;
                        stage = 4;
                    }
                    (_, 13) => {
                        return Err(
                            "TLS client certificate authentication is unsupported".into(),
                        );
                    }
                    _ => return Err("Invalid TLS handshake order or type".into()),
                }
                transcript.update(&message);
                if finished {
                    break;
                }
            }
            if finished {
                break;
            }
            let (header, payload) = read_record(&mut stream)?;
            if header[0] == 20 {
                if payload != [1] {
                    return Err("Invalid TLS ChangeCipherSpec".into());
                }
                continue;
            }
            if header[0] == 21 {
                return Err("Server rejected the TLS handshake".into());
            }
            let (kind, plaintext) = receive_hs.open(&header, &payload)?;
            match kind {
                22 => {
                    if buffered.len() + plaintext.len() > MAX_HANDSHAKE + 4 {
                        return Err("TLS handshake exceeds the limit".into());
                    }
                    buffered.extend_from_slice(&plaintext);
                }
                21 => return Err("Server rejected the TLS handshake".into()),
                _ => return Err("TLS data received before authentication".into()),
            }
        }
        if !finished || !buffered.is_empty() {
            return Err("Incomplete TLS handshake or premature data".into());
        }
        let server_finished_digest = transcript.clone().finalize();
        let master_derived = derive_secret(&handshake, "derived", &sha256(&[]))?;
        let master = hkdf_extract(&master_derived, &zeros);
        let send = RecordKeys::new(derive_secret(
            &master,
            "c ap traffic",
            &server_finished_digest,
        )?)?;
        let receive = RecordKeys::new(derive_secret(
            &master,
            "s ap traffic",
            &server_finished_digest,
        )?)?;
        let finish_key = expand_label(&send_secret, "finished", &[], 32)?;
        let finish = handshake_message(20, &hmac_sha256(&finish_key, &server_finished_digest))?;
        let finish_record = send_hs.seal(22, &finish)?;
        stream
            .write_all(&finish_record)
            .map_err(|_| "Unable to send TLS Finished")?;
        Ok(Self {
            stream,
            send,
            receive,
            pending: Vec::new(),
            offset: 0,
            post_handshake: Vec::new(),
            closed: false,
        })
    }

    fn receive_application(&mut self) -> Result<()> {
        for _ in 0..256 {
            let (header, payload) = read_record(&mut self.stream)?;
            let (kind, plaintext) = self.receive.open(&header, &payload)?;
            match kind {
                23 => {
                    if !self.post_handshake.is_empty() {
                        return Err("Incomplete TLS post-handshake message".into());
                    }
                    if plaintext.is_empty() {
                        continue;
                    }
                    self.pending = plaintext;
                    self.offset = 0;
                    return Ok(());
                }
                21 => {
                    if plaintext == [1, 0] || plaintext == [2, 0] {
                        self.closed = true;
                        return Ok(());
                    }
                    return Err("TLS alert from server".into());
                }
                22 => {
                    if self.post_handshake.len() + plaintext.len() > MAX_HANDSHAKE {
                        return Err("TLS post-handshake message is too large".into());
                    }
                    self.post_handshake.extend_from_slice(&plaintext);
                    self.process_post_handshake()?;
                }
                _ => return Err("Invalid TLS content type".into()),
            }
        }
        Err("Too many TLS messages without application data".into())
    }

    fn process_post_handshake(&mut self) -> Result<()> {
        while self.post_handshake.len() >= 4 {
            let length = u24(&self.post_handshake[1..4]);
            if length > MAX_HANDSHAKE - 4 {
                return Err("TLS post-handshake message is too large".into());
            }
            if self.post_handshake.len() < length + 4 {
                return Ok(());
            }
            let message: Vec<u8> = self.post_handshake.drain(..length + 4).collect();
            match message[0] {
                4 => validate_ticket(&message[4..])?,
                24 => {
                    if message.len() != 5 || message[4] > 1 || !self.post_handshake.is_empty() {
                        return Err("Invalid TLS KeyUpdate".into());
                    }
                    self.receive.update()?;
                    if message[4] == 1 {
                        let reply = self.send.seal(22, &[24, 0, 0, 1, 0])?;
                        self.stream
                            .write_all(&reply)
                            .map_err(|_| "Unable to send TLS KeyUpdate response")?;
                        self.send.update()?;
                    }
                }
                _ => return Err("Unsupported TLS post-handshake message".into()),
            }
        }
        Ok(())
    }
}

impl Read for TlsStream {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        if self.offset == self.pending.len()
            && !self.closed
            && let Err(error) = self.receive_application()
        {
            self.closed = true;
            self.pending.clear();
            self.offset = 0;
            return Err(io_error(error));
        }
        let available = self.pending.len() - self.offset;
        let amount = destination.len().min(available);
        destination[..amount].copy_from_slice(&self.pending[self.offset..self.offset + amount]);
        self.offset += amount;
        Ok(amount)
    }
}

impl Write for TlsStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "TLS connection closed",
            ));
        }
        if data.is_empty() {
            return Ok(0);
        }
        let amount = data.len().min(16_384);
        let record = self.send.seal(23, &data[..amount]).map_err(io_error)?;
        if let Err(error) = self.stream.write_all(&record) {
            self.closed = true;
            return Err(error);
        }
        Ok(amount)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn io_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn secure_random(destination: &mut [u8]) -> Result<()> {
    let mut source = File::open("/dev/urandom")
        .map_err(|_| "TLS requires a system random source (/dev/urandom)")?;
    source
        .read_exact(destination)
        .map_err(|_| "Unable to read system random bytes".into())
}

fn load_roots() -> Result<Vec<u8>> {
    let paths = match std::env::var_os("MYNOU_CA_FILE") {
        Some(path) => vec![std::path::PathBuf::from(path)],
        None => vec![
            "/etc/ssl/certs/ca-certificates.crt".into(),
            "/etc/pki/tls/certs/ca-bundle.crt".into(),
            "/etc/ssl/cert.pem".into(),
        ],
    };
    for path in paths {
        if let Ok(mut file) = File::open(path) {
            let metadata = file
                .metadata()
                .map_err(|_| "Unable to read TLS trust anchors")?;
            if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
                return Err("TLS trust anchor file is invalid or too large".into());
            }
            let mut data = Vec::new();
            Read::by_ref(&mut file)
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut data)
                .map_err(|_| "Unable to read TLS trust anchors")?;
            if data.is_empty() || data.len() > 4 * 1024 * 1024 {
                return Err("TLS trust anchor file is empty or too large".into());
            }
            return Ok(data);
        }
    }
    Err("Missing TLS trust anchors: set MYNOU_CA_FILE to a PEM bundle".into())
}

fn expand_label(secret: &[u8], label: &str, context: &[u8], length: usize) -> Result<Vec<u8>> {
    let length = u16::try_from(length).map_err(|_| "Invalid TLS HKDF length")?;
    let full_label = format!("tls13 {label}");
    let label_len = u8::try_from(full_label.len()).map_err(|_| "TLS HKDF label is too large")?;
    let context_len = u8::try_from(context.len()).map_err(|_| "TLS HKDF context is too large")?;
    let mut info = Vec::with_capacity(4 + full_label.len() + context.len());
    info.extend_from_slice(&length.to_be_bytes());
    info.push(label_len);
    info.extend_from_slice(full_label.as_bytes());
    info.push(context_len);
    info.extend_from_slice(context);
    hkdf_expand(secret, &info, length as usize)
}

fn derive_secret(secret: &[u8], label: &str, digest: &[u8; 32]) -> Result<[u8; 32]> {
    expand_label(secret, label, digest, 32)?
        .try_into()
        .map_err(|_| "Invalid derived TLS secret".into())
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn handshake_message(kind: u8, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_HANDSHAKE {
        return Err("TLS message is too large".into());
    }
    let length = (body.len() as u32).to_be_bytes();
    let mut message = vec![kind, length[1], length[2], length[3]];
    message.extend_from_slice(body);
    Ok(message)
}

fn extension(out: &mut Vec<u8>, kind: u16, body: &[u8]) -> Result<()> {
    let length = u16::try_from(body.len()).map_err(|_| "TLS extension is too large")?;
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

fn client_hello(
    hostname: &str,
    random: &[u8; 32],
    session: &[u8; 32],
    public: &[u8; 32],
) -> Result<Vec<u8>> {
    let mut body = vec![3, 3];
    body.extend_from_slice(random);
    body.push(32);
    body.extend_from_slice(session);
    body.extend_from_slice(&[0, 2, 0x13, 3, 1, 0]);
    let mut extensions = Vec::new();
    if hostname.parse::<IpAddr>().is_err() {
        if hostname.is_empty() || hostname.len() > 253 || !hostname.is_ascii() {
            return Err("Invalid TLS hostname".into());
        }
        let mut name = Vec::new();
        name.extend_from_slice(&((hostname.len() + 3) as u16).to_be_bytes());
        name.push(0);
        name.extend_from_slice(&(hostname.len() as u16).to_be_bytes());
        name.extend_from_slice(hostname.as_bytes());
        extension(&mut extensions, 0, &name)?;
    }
    extension(&mut extensions, 43, &[2, 3, 4])?;
    extension(&mut extensions, 10, &[0, 2, 0, 29])?;
    extension(&mut extensions, 13, &[0, 8, 8, 4, 8, 5, 4, 3, 5, 3])?;
    extension(
        &mut extensions,
        50,
        &[0, 12, 8, 4, 8, 5, 4, 1, 5, 1, 4, 3, 5, 3],
    )?;
    extension(&mut extensions, 5, &[1, 0, 0, 0, 0])?;
    extension(&mut extensions, 18, &[])?;
    let mut share = vec![0, 36, 0, 29, 0, 32];
    share.extend_from_slice(public);
    extension(&mut extensions, 51, &share)?;
    extension(&mut extensions, 16, b"\0\x09\x08http/1.1")?;
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);
    handshake_message(1, &body)
}

fn write_plain_record<W: Write>(stream: &mut W, kind: u8, data: &[u8]) -> Result<()> {
    if data.len() > 16_384 {
        return Err("Initial TLS record is too large".into());
    }
    let mut record = vec![kind, 3, 1];
    record.extend_from_slice(&(data.len() as u16).to_be_bytes());
    record.extend_from_slice(data);
    stream
        .write_all(&record)
        .map_err(|_| "Unable to write TLS handshake".into())
}

fn read_record<R: Read>(stream: &mut R) -> Result<([u8; 5], Vec<u8>)> {
    let mut header = [0; 5];
    stream
        .read_exact(&mut header)
        .map_err(|_| "TLS record is missing, incomplete or deadline exceeded")?;
    if header[1] != 3 || !matches!(header[2], 1..=3) {
        return Err("Invalid TLS record version".into());
    }
    let length = u16::from_be_bytes([header[3], header[4]]) as usize;
    if length == 0 || length > MAX_RECORD {
        return Err("Invalid TLS record length".into());
    }
    let mut payload = vec![0; length];
    stream
        .read_exact(&mut payload)
        .map_err(|_| "Incomplete TLS record or deadline exceeded")?;
    Ok((header, payload))
}

fn read_server_hello<R: Read>(stream: &mut R) -> Result<Vec<u8>> {
    let mut buffered = Vec::new();
    for _ in 0..32 {
        let (header, data) = read_record(stream)?;
        match header[0] {
            20 if data == [1] => continue,
            22 => {}
            21 => return Err("Server rejected TLS 1.3 or its algorithms".into()),
            _ => return Err("Unexpected initial TLS record".into()),
        }
        buffered.extend_from_slice(&data);
        if buffered.len() > 4096 {
            return Err("TLS ServerHello is too large".into());
        }
        if buffered.len() >= 4 {
            if buffered[0] != 2 {
                return Err("TLS ServerHello expected".into());
            }
            let length = u24(&buffered[1..4]);
            if length + 4 == buffered.len() {
                return Ok(buffered);
            }
            if length > 4092 || length + 4 < buffered.len() {
                return Err("Invalid TLS ServerHello".into());
            }
        }
    }
    Err("Incomplete TLS ServerHello".into())
}

fn parse_server_hello(message: &[u8], session: &[u8; 32]) -> Result<[u8; 32]> {
    let mut data = Decoder::new(message.get(4..).ok_or("Incomplete TLS ServerHello")?);
    if data.u16()? != 0x0303 {
        return Err("Invalid TLS ServerHello version".into());
    }
    let random = data.bytes(32)?;
    const RETRY: [u8; 32] = [
        0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8,
        0x91, 0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8,
        0x33, 0x9c,
    ];
    if random == RETRY {
        return Err("TLS HelloRetryRequest is unsupported; X25519 is required".into());
    }
    let length = data.u8()? as usize;
    if data.bytes(length)? != session {
        return Err("Invalid TLS ServerHello session ID".into());
    }
    if data.u16()? != 0x1303 || data.u8()? != 0 {
        return Err("Server must select TLS_CHACHA20_POLY1305_SHA256".into());
    }
    let length = data.u16()? as usize;
    let extensions = data.bytes(length)?;
    data.finish()?;
    let mut ext = Decoder::new(extensions);
    let mut version = false;
    let mut public = None;
    while !ext.empty() {
        let kind = ext.u16()?;
        let length = ext.u16()? as usize;
        let value = ext.bytes(length)?;
        match kind {
            43 if !version && value == [3, 4] => version = true,
            51 if public.is_none() && value.len() == 36 && value[..4] == [0, 29, 0, 32] => {
                public = Some(
                    value[4..]
                        .try_into()
                        .map_err(|_| "Invalid TLS X25519 key")?,
                );
            }
            _ => return Err("Invalid or unsupported TLS ServerHello extension".into()),
        }
    }
    if !version {
        return Err("TLS 1.3 selection is missing".into());
    }
    public.ok_or("TLS X25519 key is missing".into())
}

fn parse_encrypted_extensions(body: &[u8]) -> Result<()> {
    let mut data = Decoder::new(body);
    let length = data.u16()? as usize;
    let extensions = data.bytes(length)?;
    data.finish()?;
    let mut data = Decoder::new(extensions);
    let mut seen = Vec::new();
    while !data.empty() {
        let kind = data.u16()?;
        let length = data.u16()? as usize;
        let value = data.bytes(length)?;
        if seen.contains(&kind) {
            return Err("Duplicate TLS extension".into());
        }
        seen.push(kind);
        match kind {
            0 if value.is_empty() => {}
            10 => {
                let mut groups = Decoder::new(value);
                let size = groups.u16()? as usize;
                if size == 0 || !size.is_multiple_of(2) {
                    return Err("Invalid TLS supported groups".into());
                }
                groups.bytes(size)?;
                groups.finish()?;
            }
            16 if value == b"\0\x09\x08http/1.1" => {}
            _ => return Err("Unsolicited or invalid TLS encrypted extension".into()),
        }
    }
    Ok(())
}

fn parse_certificates(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut data = Decoder::new(body);
    if data.u8()? != 0 {
        return Err("Invalid TLS server certificate context".into());
    }
    let length = data.u24()?;
    let entries = data.bytes(length)?;
    data.finish()?;
    let mut data = Decoder::new(entries);
    let mut chain = Vec::new();
    while !data.empty() {
        let length = data.u24()?;
        if length == 0 || length > 65_536 || chain.len() >= 8 {
            return Err("TLS certificate chain exceeds the limit".into());
        }
        chain.push(data.bytes(length)?.to_vec());
        let length = data.u16()? as usize;
        let mut extensions = Decoder::new(data.bytes(length)?);
        let mut seen = Vec::new();
        while !extensions.empty() {
            let kind = extensions.u16()?;
            let length = extensions.u16()? as usize;
            extensions.bytes(length)?;
            if !matches!(kind, 5 | 18) || seen.contains(&kind) {
                return Err("Unsolicited or duplicate TLS certificate extension".into());
            }
            seen.push(kind);
        }
    }
    if chain.is_empty() {
        return Err("Missing TLS server certificate".into());
    }
    Ok(chain)
}

fn verify_certificate_signature(key: &PublicKey, body: &[u8], transcript: &Sha256) -> Result<()> {
    let mut data = Decoder::new(body);
    let scheme = data.u16()?;
    let length = data.u16()? as usize;
    let signature = data.bytes(length)?;
    data.finish()?;
    let mut signed = vec![b' '; 64];
    signed.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    signed.push(0);
    signed.extend_from_slice(&transcript.clone().finalize());
    match (scheme, key) {
        (0x0804, PublicKey::Rsa { modulus, exponent }) => {
            verify_pss_sha256(modulus, exponent, &signed, signature)
        }
        (0x0805, PublicKey::Rsa { modulus, exponent }) => {
            verify_pss_sha384(modulus, exponent, &signed, signature)
        }
        (0x0403, PublicKey::EcdsaP256 { point }) => {
            verify_ecdsa_p256_sha256(point, &signed, signature)
        }
        (0x0503, PublicKey::EcdsaP384 { point }) => {
            verify_ecdsa_p384_sha384(point, &signed, signature)
        }
        _ => Err("Unsupported TLS CertificateVerify signature".into()),
    }
}

fn validate_ticket(body: &[u8]) -> Result<()> {
    let mut data = Decoder::new(body);
    data.bytes(8)?;
    let length = data.u8()? as usize;
    data.bytes(length)?;
    let length = data.u16()? as usize;
    if length == 0 {
        return Err("Empty TLS session ticket".into());
    }
    data.bytes(length)?;
    let length = data.u16()? as usize;
    let mut extensions = Decoder::new(data.bytes(length)?);
    let mut seen = Vec::new();
    while !extensions.empty() {
        let kind = extensions.u16()?;
        let size = extensions.u16()? as usize;
        let value = extensions.bytes(size)?;
        if kind != 42 || value.len() != 4 || seen.contains(&kind) {
            return Err("Invalid TLS session ticket extension".into());
        }
        seen.push(kind);
    }
    data.finish()
}

fn u24(data: &[u8]) -> usize {
    ((data[0] as usize) << 16) | ((data[1] as usize) << 8) | data[2] as usize
}

struct Decoder<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    fn bytes(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or("TLS length exceeds the limit")?;
        let value = self
            .data
            .get(self.position..end)
            .ok_or("Truncated TLS message")?;
        self.position = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let value = self.bytes(2)?;
        Ok(u16::from_be_bytes([value[0], value[1]]))
    }
    fn u24(&mut self) -> Result<usize> {
        Ok(u24(self.bytes(3)?))
    }
    fn empty(&self) -> bool {
        self.position == self.data.len()
    }
    fn finish(&self) -> Result<()> {
        if self.empty() {
            Ok(())
        } else {
            Err("Trailing TLS data".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pki::{sign_pss_sha256_for_test, test_fixture};
    use std::net::{SocketAddr, TcpListener};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    #[derive(Clone, Copy)]
    enum Fault {
        None,
        CertificateVerify,
        Finished,
        Application,
    }

    fn local_server(fault: Fault) -> (SocketAddr, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().map_err(|e| e.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            let (header, client) = read_record(&mut stream)?;
            if header[0] != 22 || client.first() != Some(&1) {
                return Err("ClientHello expected".into());
            }
            let mut data = Decoder::new(&client[4..]);
            data.bytes(34)?;
            let length = data.u8()? as usize;
            let session = data.bytes(length)?.to_vec();
            let length = data.u16()? as usize;
            data.bytes(length)?;
            let length = data.u8()? as usize;
            data.bytes(length)?;
            let length = data.u16()? as usize;
            let mut extensions = Decoder::new(data.bytes(length)?);
            let mut peer = None;
            while !extensions.empty() {
                let kind = extensions.u16()?;
                let length = extensions.u16()? as usize;
                let value = extensions.bytes(length)?;
                if kind == 51 && value.len() == 38 {
                    peer = Some(value[6..].try_into().map_err(|_| "Invalid client key")?);
                }
            }
            let secret = [0x42; 32];
            let public = x25519_public_key(secret);
            let shared = x25519(secret, peer.ok_or("Missing client key")?)?;
            let mut hello = vec![3, 3];
            hello.extend_from_slice(&[0x73; 32]);
            hello.push(session.len() as u8);
            hello.extend_from_slice(&session);
            hello.extend_from_slice(&[0x13, 3, 0]);
            let mut ext = Vec::new();
            extension(&mut ext, 43, &[3, 4])?;
            let mut share = vec![0, 29, 0, 32];
            share.extend_from_slice(&public);
            extension(&mut ext, 51, &share)?;
            hello.extend_from_slice(&(ext.len() as u16).to_be_bytes());
            hello.extend_from_slice(&ext);
            let hello = handshake_message(2, &hello)?;
            write_plain_record(&mut stream, 22, &hello)?;
            let mut transcript = Sha256::new();
            transcript.update(&client);
            transcript.update(&hello);
            let early = hkdf_extract(&[0; 32], &[0; 32]);
            let derived = derive_secret(&early, "derived", &sha256(&[]))?;
            let handshake = hkdf_extract(&derived, &shared);
            let send_secret =
                derive_secret(&handshake, "s hs traffic", &transcript.clone().finalize())?;
            let receive_secret =
                derive_secret(&handshake, "c hs traffic", &transcript.clone().finalize())?;
            let mut send_hs = RecordKeys::new(send_secret)?;
            let mut receive_hs = RecordKeys::new(receive_secret)?;
            let ee = handshake_message(8, &[0, 0])?;
            transcript.update(&ee);
            let mut cert_entries = Vec::new();
            let length = (test_fixture::LEAF_DER.len() as u32).to_be_bytes();
            cert_entries.extend_from_slice(&length[1..]);
            cert_entries.extend_from_slice(test_fixture::LEAF_DER);
            cert_entries.extend_from_slice(&[0, 0]);
            let mut certificates = vec![0];
            let length = (cert_entries.len() as u32).to_be_bytes();
            certificates.extend_from_slice(&length[1..]);
            certificates.extend_from_slice(&cert_entries);
            let certificates = handshake_message(11, &certificates)?;
            transcript.update(&certificates);
            let mut signed = vec![b' '; 64];
            signed.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
            signed.extend_from_slice(&transcript.clone().finalize());
            let mut signature = sign_pss_sha256_for_test(
                test_fixture::MODULUS,
                test_fixture::PRIVATE_EXPONENT,
                &signed,
            )?;
            if matches!(fault, Fault::CertificateVerify) {
                signature[0] ^= 1;
            }
            let mut cv = vec![8, 4];
            cv.extend_from_slice(&(signature.len() as u16).to_be_bytes());
            cv.extend_from_slice(&signature);
            let cv = handshake_message(15, &cv)?;
            transcript.update(&cv);
            let key = expand_label(&send_secret, "finished", &[], 32)?;
            let mut verify = hmac_sha256(&key, &transcript.clone().finalize());
            if matches!(fault, Fault::Finished) {
                verify[0] ^= 1;
            }
            let finished = handshake_message(20, &verify)?;
            transcript.update(&finished);
            // Split messages across authenticated records, including a split header.
            let mut flight = Vec::new();
            for message in [&ee, &certificates, &cv, &finished] {
                flight.extend_from_slice(message);
            }
            for fragment in flight.chunks(73) {
                stream
                    .write_all(&send_hs.seal(22, fragment)?)
                    .map_err(|e| e.to_string())?;
            }
            let (header, payload) = read_record(&mut stream)?;
            let (kind, client_finish) = receive_hs.open(&header, &payload)?;
            if kind != 22 || client_finish.len() != 36 || client_finish[0] != 20 {
                return Err("Invalid client Finished".into());
            }
            let finish_key = expand_label(&receive_secret, "finished", &[], 32)?;
            if !constant_time_equal(
                &client_finish[4..],
                &hmac_sha256(&finish_key, &transcript.clone().finalize()),
            ) {
                return Err("Client Finished authentication failed".into());
            }
            let derived = derive_secret(&handshake, "derived", &sha256(&[]))?;
            let master = hkdf_extract(&derived, &[0; 32]);
            let mut send = RecordKeys::new(derive_secret(
                &master,
                "s ap traffic",
                &transcript.clone().finalize(),
            )?)?;
            let mut receive = RecordKeys::new(derive_secret(
                &master,
                "c ap traffic",
                &transcript.clone().finalize(),
            )?)?;
            let (header, payload) = read_record(&mut stream)?;
            let (kind, request) = receive.open(&header, &payload)?;
            if kind != 23 || request != b"ping" {
                return Err("Invalid client data".into());
            }
            if matches!(fault, Fault::Application) {
                let mut record = send.seal(23, b"pong")?;
                record[5] ^= 1;
                stream.write_all(&record).map_err(|e| e.to_string())?;
                return Ok(());
            }
            let ticket = handshake_message(4, &[0, 0, 0, 60, 0, 0, 0, 1, 0, 0, 1, 9, 0, 0])?;
            stream
                .write_all(&send.seal(22, &ticket)?)
                .map_err(|e| e.to_string())?;
            stream
                .write_all(&send.seal(22, &[24, 0, 0, 1, 1])?)
                .map_err(|e| e.to_string())?;
            send.update()?;
            stream
                .write_all(&send.seal(23, b"pong")?)
                .map_err(|e| e.to_string())?;
            stream
                .write_all(&send.seal(21, &[1, 0])?)
                .map_err(|e| e.to_string())?;
            let (header, payload) = read_record(&mut stream)?;
            let (kind, reply) = receive.open(&header, &payload)?;
            if kind != 22 || reply != [24, 0, 0, 1, 0] {
                return Err("Missing KeyUpdate acknowledgment".into());
            }
            Ok(())
        });
        (address, thread)
    }

    fn connect_local(address: SocketAddr, hostname: &str, roots: &[u8]) -> Result<TlsStream> {
        let stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        TlsStream::connect_with_roots(
            DeadlineStream::new(stream, Instant::now() + Duration::from_secs(10)),
            hostname,
            roots,
        )
    }

    #[test]
    fn authenticates_local_handshake_and_key_updates() {
        let (address, server) = local_server(Fault::None);
        let mut stream = connect_local(address, "localhost", test_fixture::ROOT_PEM).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"pong");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn rejects_forged_handshake_hostname_and_trust() {
        for fault in [Fault::CertificateVerify, Fault::Finished] {
            let (address, server) = local_server(fault);
            assert!(connect_local(address, "localhost", test_fixture::ROOT_PEM).is_err());
            let _ = server.join().unwrap();
        }
        let (address, server) = local_server(Fault::None);
        assert!(connect_local(address, "other.test", test_fixture::ROOT_PEM).is_err());
        let _ = server.join().unwrap();
        let (address, server) = local_server(Fault::None);
        assert!(connect_local(address, "localhost", b"untrusted").is_err());
        let _ = server.join().unwrap();
    }

    #[test]
    fn rejects_forged_application_and_poisons_connection() {
        let (address, server) = local_server(Fault::Application);
        let mut stream = connect_local(address, "localhost", test_fixture::ROOT_PEM).unwrap();
        stream.write_all(b"ping").unwrap();
        assert!(stream.read(&mut [0; 4]).is_err());
        assert_eq!(stream.read(&mut [0; 4]).unwrap(), 0);
        assert!(stream.write_all(b"again").is_err());
        server.join().unwrap().unwrap();
    }

    #[test]
    fn authenticates_rsa384_and_both_ecdsa_certificate_verify_schemes() {
        let mut transcript = Sha256::new();
        transcript.update(b"Mynou TLS CertificateVerify fixture");
        let p256 = crate::pki::parse_certificate(include_bytes!("pki/fixtures/ec256-leaf.der"))
            .unwrap()
            .public_key;
        let p384 = crate::pki::parse_certificate(include_bytes!("pki/fixtures/ec384-leaf.der"))
            .unwrap()
            .public_key;
        let rsa = PublicKey::Rsa {
            modulus: test_fixture::MODULUS.to_vec(),
            exponent: vec![1, 0, 1],
        };
        let fixtures: [(u16, PublicKey, &[u8]); 3] = [
            (
                0x0403,
                p256,
                include_bytes!("pki/fixtures/tls-certverify-256.der"),
            ),
            (
                0x0503,
                p384,
                include_bytes!("pki/fixtures/tls-certverify-384.der"),
            ),
            (
                0x0805,
                rsa,
                include_bytes!("pki/fixtures/tls-certverify-rsa384.bin"),
            ),
        ];
        for (scheme, key, signature) in fixtures {
            let mut body = Vec::new();
            body.extend_from_slice(&scheme.to_be_bytes());
            body.extend_from_slice(&(signature.len() as u16).to_be_bytes());
            body.extend_from_slice(signature);
            verify_certificate_signature(&key, &body, &transcript).unwrap();
            assert!(verify_certificate_signature(&key, &body, &Sha256::new()).is_err());
            *body.last_mut().unwrap() ^= 1;
            assert!(verify_certificate_signature(&key, &body, &transcript).is_err());
        }
    }

    #[test]
    fn authenticated_record_round_trip_and_tamper() {
        let mut send = RecordKeys::new([7; 32]).unwrap();
        let mut receive = RecordKeys::new([7; 32]).unwrap();
        for data in [&b"hello"[..], &b"second"[..]] {
            let record = send.seal(23, data).unwrap();
            let header = record[..5].try_into().unwrap();
            let (kind, plaintext) = receive.open(&header, &record[5..]).unwrap();
            assert_eq!(kind, 23);
            assert_eq!(plaintext, data);
        }
        let mut corrupt = send.seal(23, b"changed").unwrap();
        corrupt[5] ^= 1;
        assert!(
            receive
                .open(&corrupt[..5].try_into().unwrap(), &corrupt[5..])
                .is_err()
        );
    }

    #[test]
    fn record_replay_and_key_update() {
        let mut send = RecordKeys::new([3; 32]).unwrap();
        let mut receive = RecordKeys::new([3; 32]).unwrap();
        let record = send.seal(23, b"first").unwrap();
        let header = record[..5].try_into().unwrap();
        receive.open(&header, &record[5..]).unwrap();
        assert!(receive.open(&header, &record[5..]).is_err());
        send.update().unwrap();
        receive.update().unwrap();
        let record = send.seal(23, b"new key").unwrap();
        assert_eq!(
            receive
                .open(&record[..5].try_into().unwrap(), &record[5..])
                .unwrap()
                .1,
            b"new key"
        );
    }

    #[test]
    fn malformed_tls_messages_are_bounded() {
        assert!(parse_server_hello(&[2, 0, 0, 0], &[0; 32]).is_err());
        assert!(parse_encrypted_extensions(&[0, 5, 0, 16, 0, 0, 0]).is_err());
        assert!(parse_certificates(&[0, 0, 0, 0]).is_err());
        assert!(validate_ticket(&[0; 8]).is_err());
        assert!(handshake_message(1, &vec![0; MAX_HANDSHAKE + 1]).is_err());
    }

    #[test]
    fn rfc8448_derived_secret() {
        fn decode(text: &str) -> Vec<u8> {
            text.as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect()
        }
        let early = hkdf_extract(&[0; 32], &[0; 32]);
        assert_eq!(
            early.to_vec(),
            decode("33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a")
        );
        assert_eq!(
            derive_secret(&early, "derived", &sha256(&[]))
                .unwrap()
                .to_vec(),
            decode("6f2615a108c702c5678f54fc9dbab69716c076189c48250cebeac3576c3611ba")
        );
    }
}
