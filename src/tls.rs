//! Original TLS client: TLS 1.3 with X25519, ChaCha20-Poly1305, RSA-PSS and
//! P-256/P-384, and TLS 1.2 for servers without it: ECDHE X25519 or P-256 with
//! AES-128-GCM or ChaCha20-Poly1305, secure renegotiation required, extended
//! master secret when offered, no resumption or renegotiation.
//! Certificate authentication is mandatory; unsupported algorithms fail closed.
use crate::Result;
use crate::crypto::{
    Aes128Gcm, ChaCha20Poly1305, Sha256, hkdf_expand, hkdf_extract, hmac_sha256, sha256, x25519,
    x25519_public_key,
};
use crate::net::DeadlineStream;
use crate::pki::{
    PublicKey, p256_public_key, p256_shared_secret, validate_chain, verify_ecdsa_p256_sha256,
    verify_ecdsa_p384_sha384, verify_pkcs1_sha256, verify_pkcs1_sha384, verify_pss_sha256,
    verify_pss_sha384,
};
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_RECORD: usize = 16_384 + 256;
const MAX_HANDSHAKE: usize = 262_144;

pub struct TlsStream {
    stream: DeadlineStream,
    send: Protection,
    receive: Protection,
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
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("TLS sequence counter exhausted")?;
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
        let kind = plaintext
            .pop()
            .ok_or("TLS record has no inner content type")?;
        if plaintext.len() > 16_384 || !matches!(kind, 21..=23) {
            return Err("Invalid TLS record type or length".into());
        }
        Ok((kind, plaintext))
    }
}

/// Record protection of an established connection.
enum Protection {
    Tls13(RecordKeys),
    Tls12(LegacyKeys),
}

impl Protection {
    fn seal(&mut self, kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
        match self {
            Self::Tls13(keys) => keys.seal(kind, payload),
            Self::Tls12(keys) => keys.seal(kind, payload),
        }
    }
    fn open(&mut self, header: &[u8; 5], payload: &[u8]) -> Result<(u8, Vec<u8>)> {
        match self {
            Self::Tls13(keys) => keys.open(header, payload),
            Self::Tls12(keys) => keys.open(header, payload),
        }
    }
    fn update(&mut self) -> Result<()> {
        match self {
            Self::Tls13(keys) => keys.update(),
            Self::Tls12(_) => Err("TLS 1.2 has no KeyUpdate".into()),
        }
    }
}

/// The TLS 1.2 suites offered, all with ECDHE and the SHA-256 PRF.
const ECDHE_ECDSA_AES_128_GCM: u16 = 0xc02b;
const ECDHE_RSA_AES_128_GCM: u16 = 0xc02f;
const ECDHE_RSA_CHACHA20: u16 = 0xcca8;
const ECDHE_ECDSA_CHACHA20: u16 = 0xcca9;

#[derive(Clone)]
enum LegacyCipher {
    AesGcm(Aes128Gcm),
    ChaCha(ChaCha20Poly1305),
}

/// TLS 1.2 record protection (RFC 5288 and RFC 7905).
struct LegacyKeys {
    cipher: LegacyCipher,
    /// The 4-byte GCM salt, or the 12-byte ChaCha20 IV.
    iv: [u8; 12],
    sequence: u64,
}

impl LegacyKeys {
    fn new(suite: u16, key: &[u8], iv: &[u8]) -> Result<Self> {
        let cipher = match suite {
            ECDHE_ECDSA_AES_128_GCM | ECDHE_RSA_AES_128_GCM => LegacyCipher::AesGcm(
                Aes128Gcm::new(key.try_into().map_err(|_| "Invalid TLS key")?),
            ),
            _ => LegacyCipher::ChaCha(ChaCha20Poly1305::new(
                key.try_into().map_err(|_| "Invalid TLS key")?,
            )),
        };
        let mut fixed = [0; 12];
        fixed[..iv.len()].copy_from_slice(iv);
        Ok(Self {
            cipher,
            iv: fixed,
            sequence: 0,
        })
    }

    /// GCM sends the sequence number as its explicit nonce; ChaCha20 mixes it in.
    fn nonce(&self) -> ([u8; 12], Option<[u8; 8]>) {
        let sequence = self.sequence.to_be_bytes();
        let mut nonce = self.iv;
        match self.cipher {
            LegacyCipher::AesGcm(_) => {
                nonce[4..].copy_from_slice(&sequence);
                (nonce, Some(sequence))
            }
            LegacyCipher::ChaCha(_) => {
                for (out, input) in nonce[4..].iter_mut().zip(sequence) {
                    *out ^= input;
                }
                (nonce, None)
            }
        }
    }

    fn aad(&self, kind: u8, length: usize) -> Result<[u8; 13]> {
        let mut aad = [0; 13];
        aad[..8].copy_from_slice(&self.sequence.to_be_bytes());
        aad[8] = kind;
        aad[9..11].copy_from_slice(&[3, 3]);
        aad[11..].copy_from_slice(
            &u16::try_from(length)
                .map_err(|_| "TLS record is too large")?
                .to_be_bytes(),
        );
        Ok(aad)
    }

    fn advance(&mut self) -> Result<()> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("TLS sequence counter exhausted")?;
        Ok(())
    }

    fn seal(&mut self, kind: u8, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() > 16_384 {
            return Err("TLS record to send is too large".into());
        }
        let aad = self.aad(kind, payload.len())?;
        let (nonce, explicit) = self.nonce();
        let sealed = match &self.cipher {
            LegacyCipher::AesGcm(cipher) => cipher.seal(nonce, &aad, payload)?,
            LegacyCipher::ChaCha(cipher) => cipher.seal(nonce, &aad, payload)?,
        };
        let explicit = explicit.as_ref().map_or(&[][..], |bytes| &bytes[..]);
        let length =
            u16::try_from(explicit.len() + sealed.len()).map_err(|_| "TLS record is too large")?;
        let mut record = vec![kind, 3, 3];
        record.extend_from_slice(&length.to_be_bytes());
        record.extend_from_slice(explicit);
        record.extend_from_slice(&sealed);
        self.advance()?;
        Ok(record)
    }

    fn open(&mut self, header: &[u8; 5], payload: &[u8]) -> Result<(u8, Vec<u8>)> {
        let kind = header[0];
        if !matches!(kind, 21..=23) || header[1..3] != [3, 3] {
            return Err("Invalid encrypted TLS record".into());
        }
        let (mut nonce, explicit) = self.nonce();
        let sealed = match explicit {
            Some(_) => {
                if payload.len() < 8 + 16 {
                    return Err("Invalid encrypted TLS record".into());
                }
                nonce[4..].copy_from_slice(&payload[..8]);
                &payload[8..]
            }
            None => payload,
        };
        if sealed.len() < 16 || sealed.len() - 16 > 16_384 {
            return Err("Invalid TLS record type or length".into());
        }
        let aad = self.aad(kind, sealed.len() - 16)?;
        let plaintext = match &self.cipher {
            LegacyCipher::AesGcm(cipher) => cipher.open(nonce, &aad, sealed),
            LegacyCipher::ChaCha(cipher) => cipher.open(nonce, &aad, sealed),
        }
        .map_err(|_| "TLS record authentication failed")?;
        self.advance()?;
        Ok((kind, plaintext))
    }
}

/// The TLS 1.2 PRF with HMAC-SHA-256 (RFC 5246 section 5).
fn prf(secret: &[u8], label: &[u8], seed: &[u8], length: usize) -> Vec<u8> {
    let mut label_seed = label.to_vec();
    label_seed.extend_from_slice(seed);
    let mut a = hmac_sha256(secret, &label_seed);
    let mut out = Vec::with_capacity(length + 32);
    while out.len() < length {
        let mut input = a.to_vec();
        input.extend_from_slice(&label_seed);
        out.extend_from_slice(&hmac_sha256(secret, &input));
        a = hmac_sha256(secret, &a);
    }
    out.truncate(length);
    out
}

/// What the server chose.
enum ServerHello {
    Tls13([u8; 32]),
    Tls12 {
        random: [u8; 32],
        suite: u16,
        stapled: bool,
        /// Extended master secret (RFC 7627), used whenever the server has it.
        extended: bool,
    },
}

impl TlsStream {
    /// Renew a complete operation between records on a persistent connection.
    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.stream.set_deadline(deadline);
    }

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
        let client_random: [u8; 32] = random[32..64]
            .try_into()
            .map_err(|_| "Invalid TLS random bytes")?;
        let session_id: [u8; 32] = random[64..]
            .try_into()
            .map_err(|_| "Invalid TLS random bytes")?;
        let hello = client_hello(hostname, &client_random, &session_id, &public)?;
        let mut transcript = Sha256::new();
        transcript.update(&hello);
        write_plain_record(&mut stream, 22, &hello)?;
        let (server_hello, rest) = read_server_hello(&mut stream)?;
        let hello = parse_server_hello(&server_hello, &session_id)?;
        transcript.update(&server_hello);
        let peer = match hello {
            // Everything after a TLS 1.3 ServerHello is encrypted.
            ServerHello::Tls13(_) if !rest.is_empty() => {
                return Err("Plaintext TLS data after ServerHello".into());
            }
            ServerHello::Tls13(peer) => peer,
            ServerHello::Tls12 {
                random: server_random,
                suite,
                stapled,
                extended,
            } => {
                let result = legacy_handshake(
                    &mut stream,
                    Legacy {
                        hostname,
                        roots,
                        buffered: rest,
                        transcript,
                        client_random,
                        server_random,
                        secret,
                        suite,
                        stapled,
                        extended,
                    },
                );
                secret.fill(0);
                random.fill(0);
                let (send, receive) = result?;
                return Ok(Self {
                    stream,
                    send: Protection::Tls12(send),
                    receive: Protection::Tls12(receive),
                    pending: Vec::new(),
                    offset: 0,
                    post_handshake: Vec::new(),
                    closed: false,
                });
            }
        };
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
                        return Err("TLS client certificate authentication is unsupported".into());
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
            send: Protection::Tls13(send),
            receive: Protection::Tls13(receive),
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
            if let Protection::Tls12(_) = self.receive {
                // A HelloRequest may be ignored (RFC 5246 7.4.1.1): this client
                // never renegotiates. Anything else after Finished is invalid.
                if message == [0, 0, 0, 0] {
                    continue;
                }
                return Err("Unsupported TLS 1.2 post-handshake message".into());
            }
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
        None => vec!["/etc/ssl/cert.pem".into()],
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
    body.extend_from_slice(&[0, 10, 0x13, 3]);
    for suite in [
        ECDHE_ECDSA_CHACHA20,
        ECDHE_RSA_CHACHA20,
        ECDHE_ECDSA_AES_128_GCM,
        ECDHE_RSA_AES_128_GCM,
    ] {
        body.extend_from_slice(&suite.to_be_bytes());
    }
    body.extend_from_slice(&[1, 0]);
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
    extension(&mut extensions, 43, &[4, 3, 4, 3, 3])?;
    // A TLS 1.3 server without X25519 would ask to retry, which fails closed;
    // P-256 serves TLS 1.2 servers without X25519.
    extension(&mut extensions, 10, &[0, 4, 0, 29, 0, 23])?;
    // PKCS#1 v1.5 is for TLS 1.2 ServerKeyExchange only; TLS 1.3 refuses it.
    extension(
        &mut extensions,
        13,
        &[0, 12, 8, 4, 8, 5, 4, 3, 5, 3, 4, 1, 5, 1],
    )?;
    extension(&mut extensions, 11, &[1, 0])?;
    extension(&mut extensions, 23, &[])?;
    extension(&mut extensions, 0xff01, &[0])?;
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

/// The ServerHello, and what followed it in the same records: TLS 1.2 servers
/// may coalesce their whole first flight.
fn read_server_hello<R: Read>(stream: &mut R) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buffered = Vec::new();
    for _ in 0..32 {
        let (header, data) = read_record(stream)?;
        match header[0] {
            20 if data == [1] => continue,
            22 => {}
            21 => return Err("Server rejected every TLS version and algorithm offered".into()),
            _ => return Err("Unexpected initial TLS record".into()),
        }
        buffered.extend_from_slice(&data);
        if buffered.len() > MAX_RECORD + 4096 {
            return Err("TLS ServerHello is too large".into());
        }
        if buffered.len() >= 4 {
            if buffered[0] != 2 {
                return Err("TLS ServerHello expected".into());
            }
            let length = u24(&buffered[1..4]);
            if length > 4092 {
                return Err("Invalid TLS ServerHello".into());
            }
            if length + 4 <= buffered.len() {
                let rest = buffered.split_off(length + 4);
                return Ok((buffered, rest));
            }
        }
    }
    Err("Incomplete TLS ServerHello".into())
}

fn parse_server_hello(message: &[u8], session: &[u8; 32]) -> Result<ServerHello> {
    let mut data = Decoder::new(message.get(4..).ok_or("Incomplete TLS ServerHello")?);
    if data.u16()? != 0x0303 {
        return Err("Invalid TLS ServerHello version".into());
    }
    let random = data.bytes(32)?;
    if !message_has_tls13(message) {
        return parse_legacy_hello(random, data);
    }
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
    public
        .map(ServerHello::Tls13)
        .ok_or("TLS X25519 key is missing".into())
}

/// Whether a ServerHello selects TLS 1.3 through supported_versions.
fn message_has_tls13(message: &[u8]) -> bool {
    let read = || -> Result<bool> {
        let mut data = Decoder::new(message.get(4..).ok_or("Incomplete TLS ServerHello")?);
        data.bytes(2 + 32)?;
        let length = data.u8()? as usize;
        data.bytes(length + 3)?;
        if data.empty() {
            return Ok(false);
        }
        let length = data.u16()? as usize;
        let mut ext = Decoder::new(data.bytes(length)?);
        while !ext.empty() {
            let kind = ext.u16()?;
            let length = ext.u16()? as usize;
            ext.bytes(length)?;
            if kind == 43 {
                return Ok(true);
            }
        }
        Ok(false)
    };
    read().unwrap_or(true)
}

/// A TLS 1.2 ServerHello, after its version and random.
fn parse_legacy_hello(random: &[u8], mut data: Decoder) -> Result<ServerHello> {
    // RFC 8446 4.1.3: a server able to speak TLS 1.3 marks a downgrade here.
    if random[24..31] == *b"DOWNGRD" {
        return Err("TLS downgrade detected".into());
    }
    let length = data.u8()? as usize;
    if length > 32 {
        return Err("Invalid TLS ServerHello session ID".into());
    }
    data.bytes(length)?;
    let suite = data.u16()?;
    if !matches!(
        suite,
        ECDHE_ECDSA_AES_128_GCM | ECDHE_RSA_AES_128_GCM | ECDHE_RSA_CHACHA20 | ECDHE_ECDSA_CHACHA20
    ) || data.u8()? != 0
    {
        return Err("Server selected a TLS 1.2 suite that was not offered".into());
    }
    let length = data.u16()? as usize;
    let extensions = data.bytes(length)?;
    data.finish()?;
    let mut ext = Decoder::new(extensions);
    let mut seen = Vec::new();
    let (mut extended, mut renegotiation, mut stapled) = (false, false, false);
    while !ext.empty() {
        let kind = ext.u16()?;
        let length = ext.u16()? as usize;
        let value = ext.bytes(length)?;
        if seen.contains(&kind) {
            return Err("Duplicate TLS extension".into());
        }
        seen.push(kind);
        match kind {
            0 if value.is_empty() => {}
            5 if value.is_empty() => stapled = true,
            11 => {
                let mut formats = Decoder::new(value);
                let length = formats.u8()? as usize;
                if !formats.bytes(length)?.contains(&0) {
                    return Err("TLS server lacks uncompressed points".into());
                }
                formats.finish()?;
            }
            16 if value == b"\0\x09\x08http/1.1" => {}
            23 if value.is_empty() => extended = true,
            0xff01 if value == [0] => renegotiation = true,
            _ => return Err("Invalid or unsupported TLS ServerHello extension".into()),
        }
    }
    if !renegotiation {
        return Err("TLS 1.2 server lacks secure renegotiation".into());
    }
    Ok(ServerHello::Tls12 {
        random: random.try_into().map_err(|_| "Invalid TLS random")?,
        suite,
        stapled,
        extended,
    })
}

/// What a TLS 1.2 handshake starts from, after ClientHello and ServerHello.
struct Legacy<'a> {
    hostname: &'a str,
    roots: &'a [u8],
    /// Handshake bytes that arrived with the ServerHello.
    buffered: Vec<u8>,
    transcript: Sha256,
    client_random: [u8; 32],
    server_random: [u8; 32],
    secret: [u8; 32],
    suite: u16,
    stapled: bool,
    extended: bool,
}

/// The rest of a full TLS 1.2 handshake (RFC 5246 and RFC 7627): the server's
/// certificate and signed X25519 share, then both Finished messages.
fn legacy_handshake(
    stream: &mut DeadlineStream,
    mut session: Legacy,
) -> Result<(LegacyKeys, LegacyKeys)> {
    let mut buffered = std::mem::take(&mut session.buffered);
    let mut leaf_key = None;
    let mut peer = None;
    let mut stage = 0;
    'flight: for _ in 0..256 {
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
                (0, 11) => {
                    let chain = parse_legacy_certificates(&message[4..])?;
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| "Invalid system clock for TLS")?
                        .as_secs();
                    leaf_key = Some(validate_chain(
                        &chain,
                        session.roots,
                        session.hostname,
                        now,
                    )?);
                    stage = 1;
                }
                // OCSP stapling is offered but not evaluated, as in TLS 1.3.
                (1, 22) if session.stapled => stage = 2,
                (1 | 2, 12) => {
                    let key = leaf_key.as_ref().ok_or("Missing TLS certificate")?;
                    peer = Some(verify_key_exchange(key, &session, &message[4..])?);
                    stage = 3;
                }
                (3, 14) if message.len() == 4 => {
                    session.transcript.update(&message);
                    if !buffered.is_empty() {
                        return Err("TLS data after ServerHelloDone".into());
                    }
                    break 'flight;
                }
                (_, 13) => {
                    return Err("TLS client certificate authentication is unsupported".into());
                }
                _ => return Err("Invalid TLS handshake order or type".into()),
            }
            session.transcript.update(&message);
        }
        let (header, payload) = read_record(stream)?;
        match header[0] {
            22 => {
                if buffered.len() + payload.len() > MAX_HANDSHAKE + 4 {
                    return Err("TLS handshake exceeds the limit".into());
                }
                buffered.extend_from_slice(&payload);
            }
            21 => return Err("Server rejected the TLS handshake".into()),
            _ => return Err("Unexpected TLS record during the handshake".into()),
        }
    }
    let (public, mut shared) = match peer.ok_or("Incomplete TLS 1.2 handshake")? {
        Share::X25519(peer) => (
            x25519_public_key(session.secret).to_vec(),
            x25519(session.secret, peer)?.to_vec(),
        ),
        Share::P256(peer) => {
            // A fresh scalar in [1, n - 1]; almost every draw is in range.
            let mut scalar = [0; 32];
            let mut public = None;
            for _ in 0..16 {
                secure_random(&mut scalar)?;
                if let Ok(point) = p256_public_key(&scalar) {
                    public = Some(point);
                    break;
                }
            }
            let public = public.ok_or("Unable to draw a P-256 scalar")?;
            let shared = p256_shared_secret(&scalar, &peer);
            scalar.fill(0);
            (public, shared?.to_vec())
        }
    };
    let mut exchange = vec![u8::try_from(public.len()).map_err(|_| "Invalid TLS key share")?];
    exchange.extend_from_slice(&public);
    let exchange = handshake_message(16, &exchange)?;
    session.transcript.update(&exchange);
    // RFC 7627 binds the master secret to the whole handshake so far. A server
    // without it gets the RFC 5246 secret: the triple-handshake attack it
    // prevents needs resumption, renegotiation or client certificates, and
    // this client does none of them.
    let mut master = if session.extended {
        prf(
            &shared,
            b"extended master secret",
            &session.transcript.clone().finalize(),
            48,
        )
    } else {
        let mut randoms = session.client_random.to_vec();
        randoms.extend_from_slice(&session.server_random);
        prf(&shared, b"master secret", &randoms, 48)
    };
    shared.fill(0);
    let (key, iv) = match session.suite {
        ECDHE_ECDSA_AES_128_GCM | ECDHE_RSA_AES_128_GCM => (16, 4),
        _ => (32, 12),
    };
    let mut seed = session.server_random.to_vec();
    seed.extend_from_slice(&session.client_random);
    let mut block = prf(&master, b"key expansion", &seed, 2 * (key + iv));
    let mut send = LegacyKeys::new(session.suite, &block[..key], &block[2 * key..2 * key + iv])?;
    let mut receive = LegacyKeys::new(session.suite, &block[key..2 * key], &block[2 * key + iv..])?;
    block.fill(0);
    let verify = prf(
        &master,
        b"client finished",
        &session.transcript.clone().finalize(),
        12,
    );
    let finished = handshake_message(20, &verify)?;
    session.transcript.update(&finished);
    // After ServerHello every record carries the negotiated version, 3.3.
    let mut flight = vec![22, 3, 3];
    flight.extend_from_slice(
        &u16::try_from(exchange.len())
            .map_err(|_| "TLS key exchange is too large")?
            .to_be_bytes(),
    );
    flight.extend_from_slice(&exchange);
    flight.extend_from_slice(&[20, 3, 3, 0, 1, 1]);
    flight.extend_from_slice(&send.seal(22, &finished)?);
    stream
        .write_all(&flight)
        .map_err(|_| "Unable to send the TLS key exchange")?;
    let expected = prf(
        &master,
        b"server finished",
        &session.transcript.clone().finalize(),
        12,
    );
    master.fill(0);
    let (header, payload) = read_record(stream)?;
    if header[0] == 21 {
        return Err("Server rejected the TLS handshake".into());
    }
    if header[0] != 20 || payload != [1] {
        return Err("TLS ChangeCipherSpec expected".into());
    }
    let (header, payload) = read_record(stream)?;
    let (kind, message) = receive.open(&header, &payload)?;
    if kind != 22 || message.len() != 16 || message[..4] != [20, 0, 0, 12] {
        return Err("Invalid TLS Finished message".into());
    }
    if !constant_time_equal(&expected, &message[4..]) {
        return Err("TLS Finished authentication failed".into());
    }
    Ok((send, receive))
}

/// A TLS 1.2 Certificate message: certificates without per-entry extensions.
fn parse_legacy_certificates(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut data = Decoder::new(body);
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
    }
    if chain.is_empty() {
        return Err("Missing TLS server certificate".into());
    }
    Ok(chain)
}

/// The server's ephemeral key share.
enum Share {
    X25519([u8; 32]),
    P256(Vec<u8>),
}

/// Checks the signed key share of a ServerKeyExchange and returns it.
fn verify_key_exchange(key: &PublicKey, session: &Legacy, body: &[u8]) -> Result<Share> {
    let mut data = Decoder::new(body);
    let peer = match data.bytes(4)? {
        [3, 0, 29, 32] => Share::X25519(
            data.bytes(32)?
                .try_into()
                .map_err(|_| "Invalid TLS X25519 key")?,
        ),
        [3, 0, 23, 65] => Share::P256(data.bytes(65)?.to_vec()),
        _ => return Err("TLS server key exchange must use X25519 or P-256".into()),
    };
    let parameters = data.position;
    let scheme = data.u16()?;
    let length = data.u16()? as usize;
    let signature = data.bytes(length)?;
    data.finish()?;
    let mut signed = session.client_random.to_vec();
    signed.extend_from_slice(&session.server_random);
    signed.extend_from_slice(&body[..parameters]);
    let rsa_suite = matches!(session.suite, ECDHE_RSA_AES_128_GCM | ECDHE_RSA_CHACHA20);
    match (scheme, key) {
        (0x0401, PublicKey::Rsa { modulus, exponent }) if rsa_suite => {
            verify_pkcs1_sha256(modulus, exponent, &signed, signature)
        }
        (0x0501, PublicKey::Rsa { modulus, exponent }) if rsa_suite => {
            verify_pkcs1_sha384(modulus, exponent, &signed, signature)
        }
        (0x0804, PublicKey::Rsa { modulus, exponent }) if rsa_suite => {
            verify_pss_sha256(modulus, exponent, &signed, signature)
        }
        (0x0805, PublicKey::Rsa { modulus, exponent }) if rsa_suite => {
            verify_pss_sha384(modulus, exponent, &signed, signature)
        }
        (0x0403, PublicKey::EcdsaP256 { point }) if !rsa_suite => {
            verify_ecdsa_p256_sha256(point, &signed, signature)
        }
        (0x0503, PublicKey::EcdsaP384 { point }) if !rsa_suite => {
            verify_ecdsa_p384_sha384(point, &signed, signature)
        }
        _ => Err("Unsupported TLS ServerKeyExchange signature".into()),
    }?;
    Ok(peer)
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
pub(crate) mod tests {
    use super::*;
    use crate::pki::{sign_pss_sha256_for_test, test_fixture};
    use std::net::{SocketAddr, TcpListener};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    #[derive(Clone, Copy)]
    pub(crate) enum Fault {
        None,
        CertificateVerify,
        Finished,
        Application,
        HttpOk,
        HttpDowngrade,
    }

    fn local_server(fault: Fault) -> (SocketAddr, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            serve_fixture(stream, fault, None)
        });
        (address, thread)
    }

    /// The original authenticated synthetic TLS server on any supplied stream.
    pub(crate) fn serve_fixture<S: Read + Write>(
        mut stream: S,
        fault: Fault,
        http: Option<(&[u8], &[u8])>,
    ) -> Result<()> {
        serve_fixture_inner(&mut stream, fault, http, None)
    }
    pub(crate) fn serve_dialogue<S: Read + Write>(
        mut stream: S,
        dialogue: &[(&[u8], &[u8])],
    ) -> Result<()> {
        serve_fixture_inner(&mut stream, Fault::HttpOk, None, Some(dialogue))
    }
    fn serve_fixture_inner<S: Read + Write>(
        mut stream: S,
        fault: Fault,
        http: Option<(&[u8], &[u8])>,
        dialogue: Option<&[(&[u8], &[u8])]>,
    ) -> Result<()> {
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
        // TLS 1.3 without a PSK starts with the specified all-zero value,
        // not secret key material. Match the client key schedule explicitly.
        let zeros = [0; 32];
        let early = hkdf_extract(&zeros, &zeros);
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
        if let Some(dialogue) = dialogue {
            let mut pending = Vec::new();
            for (expected, reply) in dialogue {
                while pending.len() < expected.len() {
                    let (header, payload) = read_record(&mut stream)?;
                    let (kind, fragment) = receive.open(&header, &payload)?;
                    if kind != 23 || pending.len() + fragment.len() > 65536 {
                        return Err("Invalid fixture dialogue".into());
                    }
                    pending.extend(fragment);
                }
                if !pending.starts_with(expected) {
                    return Err("Unexpected fixture dialogue".into());
                }
                pending.drain(..expected.len());
                if !reply.is_empty() {
                    stream
                        .write_all(&send.seal(23, reply)?)
                        .map_err(|e| e.to_string())?;
                }
            }
            return Ok(());
        }
        if http.is_some() || matches!(fault, Fault::HttpOk | Fault::HttpDowngrade) {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let (header, payload) = read_record(&mut stream)?;
                let (kind, fragment) = receive.open(&header, &payload)?;
                if kind != 23 || request.len() + fragment.len() > 65_536 {
                    return Err("Invalid synthetic HTTPS request".into());
                }
                request.extend(fragment);
            }
            if !request.starts_with(
                http.map_or(b"GET /private HTTP/1.1\r\n".as_slice(), |(target, _)| {
                    target
                }),
            ) {
                return Err("Unexpected synthetic HTTPS target".into());
            }
            let response: &[u8] = if let Some((_, response)) = http {
                response
            } else if matches!(fault, Fault::HttpDowngrade) {
                b"HTTP/1.1 302 Found\r\nLocation: http://downgrade.invalid/\r\nContent-Length: 0\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong"
            };
            stream
                .write_all(&send.seal(23, response)?)
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
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

    // A custom stream has no TcpStream API; deliberately fragment I/O so TLS
    // must use the supplied Read/Write implementation throughout the session.
    struct Fragmented(TcpStream);

    impl Read for Fragmented {
        fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
            let count = data.len().min(7);
            self.0.read(&mut data[..count])
        }
    }

    impl Write for Fragmented {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.write(&data[..data.len().min(11)])
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl crate::net::NetworkStream for Fragmented {
        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_read_timeout(timeout)
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_write_timeout(timeout)
        }
    }

    fn connect_custom(address: SocketAddr, hostname: &str, roots: &[u8]) -> Result<TlsStream> {
        let stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
        TlsStream::connect_with_roots(
            DeadlineStream::from_stream(
                Box::new(Fragmented(stream)),
                Instant::now() + Duration::from_secs(10),
            ),
            hostname,
            roots,
        )
    }

    #[test]
    fn supplied_stream_authenticates_and_handles_application_data_and_key_updates() {
        let (address, server) = local_server(Fault::None);
        let mut stream = connect_custom(address, "localhost", test_fixture::ROOT_PEM).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"pong");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn supplied_stream_rejects_forged_handshakes_wrong_names_and_untrusted_roots() {
        for fault in [Fault::CertificateVerify, Fault::Finished] {
            let (address, server) = local_server(fault);
            assert!(connect_custom(address, "localhost", test_fixture::ROOT_PEM).is_err());
            let _ = server.join().unwrap();
        }
        let (address, server) = local_server(Fault::None);
        assert!(connect_custom(address, "other.test", test_fixture::ROOT_PEM).is_err());
        let _ = server.join().unwrap();
        let (address, server) = local_server(Fault::None);
        assert!(connect_custom(address, "localhost", b"untrusted").is_err());
        let _ = server.join().unwrap();
    }

    #[test]
    fn https_connector_authenticates_the_url_host_and_rejects_downgrades() {
        use crate::net::{HttpClient, HttpConnector, NetworkStream};
        use std::sync::{Arc, Mutex};
        const CHILD: &str = "MYNOU_HTTPS_CONNECTOR_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tls::tests::https_connector_authenticates_the_url_host_and_rejects_downgrades",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env(
                    "MYNOU_CA_FILE",
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("src/pki/fixtures/rsa-root.pem"),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        struct Mapped {
            address: SocketAddr,
            hosts: Mutex<Vec<String>>,
        }
        impl HttpConnector for Mapped {
            fn connect(
                &self,
                host: &str,
                _port: u16,
                deadline: Instant,
            ) -> Result<Box<dyn NetworkStream>> {
                self.hosts.lock().unwrap().push(host.into());
                let timeout = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("Synthetic connect deadline expired")?;
                let stream = TcpStream::connect_timeout(&self.address, timeout)
                    .map_err(|e| e.to_string())?;
                Ok(Box::new(Fragmented(stream)))
            }
        }
        for (fault, host, expected) in [
            (Fault::HttpOk, "localhost", None),
            (
                Fault::HttpDowngrade,
                "localhost",
                Some("HTTPS to HTTP redirect rejected"),
            ),
            (Fault::HttpOk, "wrong.invalid", Some("hostname")),
        ] {
            let (address, server) = local_server(fault);
            let connector = Arc::new(Mapped {
                address,
                hosts: Mutex::new(Vec::new()),
            });
            let result = HttpClient::new()
                .with_connector(connector.clone())
                .get(&format!("https://{host}/private"));
            if let Some(message) = expected {
                assert!(result.unwrap_err().contains(message));
            } else {
                assert_eq!(result.unwrap().body, b"pong");
            }
            assert_eq!(*connector.hosts.lock().unwrap(), vec![host]);
            let outcome = server.join().unwrap();
            if host == "localhost" {
                outcome.unwrap();
            }
        }
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
    fn tls12_prf_matches_the_published_vector() {
        let output = prf(
            &[
                0x9b, 0xbe, 0x43, 0x6b, 0xa9, 0x40, 0xf0, 0x17, 0xb1, 0x76, 0x52, 0x84, 0x9a, 0x71,
                0xdb, 0x35,
            ],
            b"test label",
            &[
                0xa0, 0xba, 0x9f, 0x93, 0x6c, 0xda, 0x31, 0x18, 0x27, 0xa6, 0xf7, 0x96, 0xff, 0xd5,
                0x19, 0x8c,
            ],
            100,
        );
        assert_eq!(output.len(), 100);
        assert_eq!(
            output[..8],
            [0xe3, 0xf2, 0x29, 0xba, 0x72, 0x7b, 0xe1, 0x7b]
        );
        assert_eq!(
            output[92..],
            [0x10, 0xff, 0xf7, 0x01, 0x87, 0x34, 0x7b, 0x66]
        );
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Legacy12 {
        Aes,
        ChaChaWithoutExtendedSecret,
        Downgrade,
        NoRenegotiationInfo,
        KeyExchangeSignature,
        Finished,
    }

    /// A synthetic TLS 1.2 server: RSA certificate, X25519, PSS-signed share.
    fn serve_legacy(mut stream: TcpStream, fault: Legacy12) -> Result<()> {
        let (_, client) = read_record(&mut stream)?;
        let client_random: [u8; 32] = client[6..38].try_into().map_err(|_| "Short hello")?;
        let mut server_random = [0x5a; 32];
        if fault == Legacy12::Downgrade {
            server_random[24..].copy_from_slice(b"DOWNGRD\x01");
        }
        let suite = if fault == Legacy12::ChaChaWithoutExtendedSecret {
            ECDHE_RSA_CHACHA20
        } else {
            ECDHE_RSA_AES_128_GCM
        };
        let extended = fault != Legacy12::ChaChaWithoutExtendedSecret;
        let mut hello = vec![3, 3];
        hello.extend_from_slice(&server_random);
        hello.push(0);
        hello.extend_from_slice(&suite.to_be_bytes());
        hello.push(0);
        let mut ext = Vec::new();
        if fault != Legacy12::NoRenegotiationInfo {
            extension(&mut ext, 0xff01, &[0])?;
        }
        if extended {
            extension(&mut ext, 23, &[])?;
        }
        hello.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        hello.extend_from_slice(&ext);
        let hello = handshake_message(2, &hello)?;
        let mut certificate = Vec::new();
        let length = (test_fixture::LEAF_DER.len() as u32).to_be_bytes();
        certificate
            .extend_from_slice(&((test_fixture::LEAF_DER.len() + 3) as u32).to_be_bytes()[1..]);
        certificate.extend_from_slice(&length[1..]);
        certificate.extend_from_slice(test_fixture::LEAF_DER);
        let certificate = handshake_message(11, &certificate)?;
        let secret = [0x24; 32];
        let mut parameters = vec![3, 0, 29, 32];
        parameters.extend_from_slice(&x25519_public_key(secret));
        let mut signed = client_random.to_vec();
        signed.extend_from_slice(&server_random);
        signed.extend_from_slice(&parameters);
        let mut signature = sign_pss_sha256_for_test(
            test_fixture::MODULUS,
            test_fixture::PRIVATE_EXPONENT,
            &signed,
        )?;
        if fault == Legacy12::KeyExchangeSignature {
            signature[0] ^= 1;
        }
        let mut exchange = parameters;
        exchange.extend_from_slice(&[8, 4]);
        exchange.extend_from_slice(&(signature.len() as u16).to_be_bytes());
        exchange.extend_from_slice(&signature);
        let exchange = handshake_message(12, &exchange)?;
        let done = handshake_message(14, &[])?;
        let mut transcript = Sha256::new();
        transcript.update(&client);
        let mut flight = Vec::new();
        for message in [&hello, &certificate, &exchange, &done] {
            transcript.update(message);
            flight.extend_from_slice(message);
        }
        // Coalesced and split messages, as servers send them.
        for fragment in flight.chunks(700) {
            let mut record = vec![22, 3, 3];
            record.extend_from_slice(&(fragment.len() as u16).to_be_bytes());
            record.extend_from_slice(fragment);
            stream.write_all(&record).map_err(|e| e.to_string())?;
        }
        let (header, key_exchange) = read_record(&mut stream)?;
        if header[0] != 22 || key_exchange.len() != 37 || key_exchange[..5] != [16, 0, 0, 33, 32] {
            return Err("ClientKeyExchange expected".into());
        }
        transcript.update(&key_exchange);
        let peer: [u8; 32] = key_exchange[5..].try_into().map_err(|_| "Bad share")?;
        let shared = x25519(secret, peer)?;
        let master = if extended {
            prf(
                &shared,
                b"extended master secret",
                &transcript.clone().finalize(),
                48,
            )
        } else {
            prf(
                &shared,
                b"master secret",
                &[client_random, server_random].concat(),
                48,
            )
        };
        let (key, iv) = if extended { (16, 4) } else { (32, 12) };
        let block = prf(
            &master,
            b"key expansion",
            &[server_random, client_random].concat(),
            2 * (key + iv),
        );
        let mut receive = LegacyKeys::new(suite, &block[..key], &block[2 * key..2 * key + iv])?;
        let mut send = LegacyKeys::new(suite, &block[key..2 * key], &block[2 * key + iv..])?;
        let (header, change) = read_record(&mut stream)?;
        if header[0] != 20 || change != [1] {
            return Err("ChangeCipherSpec expected".into());
        }
        let expected = prf(
            &master,
            b"client finished",
            &transcript.clone().finalize(),
            12,
        );
        let (header, payload) = read_record(&mut stream)?;
        let (kind, finished) = receive.open(&header, &payload)?;
        if kind != 22 || finished[..4] != [20, 0, 0, 12] || finished[4..] != expected[..] {
            return Err("Client Finished authentication failed".into());
        }
        transcript.update(&finished);
        let mut verify = prf(
            &master,
            b"server finished",
            &transcript.clone().finalize(),
            12,
        );
        if fault == Legacy12::Finished {
            verify[0] ^= 1;
        }
        stream
            .write_all(&[20, 3, 3, 0, 1, 1])
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&send.seal(22, &handshake_message(20, &verify)?)?)
            .map_err(|e| e.to_string())?;
        let (header, payload) = read_record(&mut stream)?;
        let (kind, request) = receive.open(&header, &payload)?;
        if kind != 23 || request != b"ping" {
            return Err("Application data expected".into());
        }
        // A HelloRequest is ignored: the client never renegotiates.
        stream
            .write_all(&send.seal(22, &[0, 0, 0, 0])?)
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&send.seal(23, b"pong")?)
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&send.seal(21, &[1, 0])?)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn legacy_server(fault: Legacy12) -> (SocketAddr, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .map_err(|e| e.to_string())?;
            serve_legacy(stream, fault)
        });
        (address, thread)
    }

    #[test]
    fn tls12_authenticates_both_suites_and_carries_data() {
        for fault in [Legacy12::Aes, Legacy12::ChaChaWithoutExtendedSecret] {
            let (address, server) = legacy_server(fault);
            let mut stream = connect_custom(address, "localhost", test_fixture::ROOT_PEM).unwrap();
            assert!(matches!(stream.send, Protection::Tls12(_)));
            stream.write_all(b"ping").unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            assert_eq!(response, b"pong");
            server.join().unwrap().unwrap();
        }
    }

    #[test]
    fn tls12_rejects_downgrades_missing_protection_and_forgeries() {
        for fault in [
            Legacy12::Downgrade,
            Legacy12::NoRenegotiationInfo,
            Legacy12::KeyExchangeSignature,
            Legacy12::Finished,
        ] {
            let (address, server) = legacy_server(fault);
            assert!(connect_custom(address, "localhost", test_fixture::ROOT_PEM).is_err());
            let _ = server.join().unwrap();
        }
        let (address, server) = legacy_server(Legacy12::Aes);
        assert!(connect_custom(address, "other.test", test_fixture::ROOT_PEM).is_err());
        let _ = server.join().unwrap();
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
