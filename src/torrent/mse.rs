//! Message Stream Encryption (MSE/PE), the obfuscated handshake that many
//! clients try first and some require. It hides the BitTorrent protocol from
//! traffic shaping and nothing more: confidentiality comes from the protected
//! tunnel, not from RC4 keyed by a public info hash.
use super::wire::{read_exact_deadline, write_all_deadline};
use crate::{
    Result,
    crypto::{Sha1, random_bytes},
    net::NetworkStream,
};
use std::{
    io::{Read, Write},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// The 768-bit safe prime of the specification (Oakley group 1); the
/// generator is 2.
const PRIME: [u8; 96] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2, 0x34,
    0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67, 0xcc, 0x74,
    0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e, 0x34, 0x04, 0xdd,
    0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d, 0xf2, 0x5f, 0x14, 0x37,
    0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5, 0x76, 0x62, 0x5e, 0x7e, 0xc6,
    0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x3a, 0x36, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x05, 0x63,
];
const VC: [u8; 8] = [0; 8];
const PLAINTEXT: u32 = 1;
const RC4: u32 = 2;
/// Padding may be up to 512 bytes; the synchronisation markers are searched
/// within that distance and no further.
const MAX_PAD: usize = 512;
/// The initial payload is the peer's BitTorrent handshake, sometimes followed
/// by its first messages.
const MAX_INITIAL: usize = 4096;

/// RC4 with the first 1024 keystream bytes discarded, as MSE requires.
#[derive(Clone)]
struct Rc4 {
    state: [u8; 256],
    i: u8,
    j: u8,
}
impl Rc4 {
    fn new(key: &[u8]) -> Self {
        let mut cipher = Self::raw(key);
        cipher.apply(&mut [0; 1024]);
        cipher
    }
    fn raw(key: &[u8]) -> Self {
        let mut state = [0u8; 256];
        for (i, slot) in state.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, usize::from(j));
        }
        Self { state, i: 0, j: 0 }
    }
    fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.state[usize::from(self.i)]);
            self.state.swap(usize::from(self.i), usize::from(self.j));
            let k = self.state[usize::from(
                self.state[usize::from(self.i)].wrapping_add(self.state[usize::from(self.j)]),
            )];
            *byte ^= k;
        }
    }
}

fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for part in parts {
        h.update(part);
    }
    h.finalize()
}

/// What an encrypted handshake reveals of its torrent: callers compare it with
/// this value for each info hash they serve.
pub fn torrent_marker(info_hash: &[u8; 20]) -> [u8; 20] {
    hash(&[b"req2", info_hash])
}

struct Keys {
    private: [u8; 20],
    public: Vec<u8>,
}
impl Keys {
    fn new() -> Result<Self> {
        let private = random_bytes::<20>()?;
        let public = crate::pki::dh_power(&PRIME, &[2], &private)?;
        Ok(Self { private, public })
    }
    fn shared(&self, theirs: &[u8; 96]) -> Result<Vec<u8>> {
        // 0, 1 and p-1 would make the secret predictable.
        let mut limit = PRIME;
        limit[95] -= 1;
        if theirs[..] >= limit[..] || theirs[..95].iter().all(|b| *b == 0) && theirs[95] <= 1 {
            return Err("Invalid encrypted handshake key".into());
        }
        crate::pki::dh_power(&PRIME, theirs, &self.private)
    }
    /// The public key followed by random padding.
    fn greeting(&self) -> Result<Vec<u8>> {
        let mut message = self.public.clone();
        let length = usize::from(u16::from_be_bytes(random_bytes::<2>()?)) % (MAX_PAD + 1);
        let padding = random_bytes::<MAX_PAD>()?;
        message.extend_from_slice(&padding[..length]);
        Ok(message)
    }
}

/// Reads until `marker` ends the bytes read, at most `limit` bytes.
fn synchronise(
    stream: &mut dyn NetworkStream,
    marker: &[u8],
    limit: usize,
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<()> {
    let mut seen = Vec::with_capacity(limit);
    while seen.len() < limit {
        let mut byte = [0];
        read_exact_deadline(stream, &mut byte, stop, deadline)?;
        seen.push(byte[0]);
        if seen.ends_with(marker) {
            return Ok(());
        }
    }
    Err("Encrypted handshake did not synchronise".into())
}

fn read_decrypted(
    stream: &mut dyn NetworkStream,
    cipher: &mut Rc4,
    length: usize,
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut data = vec![0; length];
    read_exact_deadline(stream, &mut data, stop, deadline)?;
    cipher.apply(&mut data);
    Ok(data)
}

/// Opens an outgoing encrypted stream for `info_hash`. The BitTorrent
/// handshake then follows through the returned stream.
pub fn initiate(
    mut stream: Box<dyn NetworkStream>,
    info_hash: &[u8; 20],
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<Box<dyn NetworkStream>> {
    let keys = Keys::new()?;
    write_all_deadline(&mut *stream, &keys.greeting()?, stop, deadline)?;
    let mut theirs = [0; 96];
    read_exact_deadline(&mut *stream, &mut theirs, stop, deadline)?;
    let secret = keys.shared(&theirs)?;
    let mut send = Rc4::new(&hash(&[b"keyA", &secret, info_hash]));
    let mut receive = Rc4::new(&hash(&[b"keyB", &secret, info_hash]));
    let mut message = hash(&[b"req1", &secret]).to_vec();
    let req3 = hash(&[b"req3", &secret]);
    message.extend(
        torrent_marker(info_hash)
            .iter()
            .zip(req3)
            .map(|(a, b)| a ^ b),
    );
    let mut offer = VC.to_vec();
    offer.extend_from_slice(&(RC4 | PLAINTEXT).to_be_bytes());
    // No padding and no initial payload: the handshake follows separately.
    offer.extend_from_slice(&[0; 4]);
    send.apply(&mut offer);
    message.extend(offer);
    write_all_deadline(&mut *stream, &message, stop, deadline)?;
    let mut marker = VC;
    receive.apply(&mut marker);
    synchronise(&mut *stream, &marker, MAX_PAD + VC.len(), stop, deadline)?;
    let answer = read_decrypted(&mut *stream, &mut receive, 6, stop, deadline)?;
    let select = u32::from_be_bytes(answer[..4].try_into().map_err(|_| "Invalid selection")?);
    let padding = usize::from(u16::from_be_bytes([answer[4], answer[5]]));
    if padding > MAX_PAD {
        return Err("Encrypted handshake padding is too long".into());
    }
    read_decrypted(&mut *stream, &mut receive, padding, stop, deadline)?;
    let cipher = match select {
        RC4 => Some((send, receive)),
        PLAINTEXT => None,
        _ => return Err("Peer selected an unsupported encryption".into()),
    };
    Ok(Box::new(Encrypted {
        inner: stream,
        initial: Vec::new(),
        offset: 0,
        cipher,
    }))
}

/// Answers an incoming encrypted handshake whose first `received` bytes were
/// already read. `find` returns the info hash with the given
/// [`torrent_marker`], if this client serves it. The peer's BitTorrent
/// handshake for that hash is then read from the returned stream.
pub fn respond(
    mut stream: Box<dyn NetworkStream>,
    received: &[u8],
    find: impl FnOnce(&[u8; 20]) -> Result<Option<[u8; 20]>>,
    stop: Option<&AtomicBool>,
    deadline: Instant,
) -> Result<(Box<dyn NetworkStream>, [u8; 20])> {
    let mut theirs = [0; 96];
    let known = received.len().min(theirs.len());
    theirs[..known].copy_from_slice(&received[..known]);
    read_exact_deadline(&mut *stream, &mut theirs[known..], stop, deadline)?;
    let keys = Keys::new()?;
    let secret = keys.shared(&theirs)?;
    write_all_deadline(&mut *stream, &keys.greeting()?, stop, deadline)?;
    // Bytes received past the public key belong to the padding before req1.
    let after = &received[known..];
    let req1 = hash(&[b"req1", &secret]);
    if !after.ends_with(&req1) {
        synchronise(&mut *stream, &req1, MAX_PAD + req1.len(), stop, deadline)?;
    }
    let mut obscured = [0; 20];
    read_exact_deadline(&mut *stream, &mut obscured, stop, deadline)?;
    let req3 = hash(&[b"req3", &secret]);
    let mut marker = [0; 20];
    for (slot, (a, b)) in marker.iter_mut().zip(obscured.iter().zip(req3)) {
        *slot = a ^ b;
    }
    let info_hash = find(&marker)?.ok_or("Unknown encrypted torrent")?;
    let mut receive = Rc4::new(&hash(&[b"keyA", &secret, &info_hash]));
    let mut send = Rc4::new(&hash(&[b"keyB", &secret, &info_hash]));
    let offer = read_decrypted(&mut *stream, &mut receive, 14, stop, deadline)?;
    if offer[..8] != VC {
        return Err("Invalid encrypted handshake verification".into());
    }
    let provide = u32::from_be_bytes(offer[8..12].try_into().map_err(|_| "Invalid offer")?);
    let padding = usize::from(u16::from_be_bytes([offer[12], offer[13]]));
    if padding > MAX_PAD {
        return Err("Encrypted handshake padding is too long".into());
    }
    read_decrypted(&mut *stream, &mut receive, padding, stop, deadline)?;
    let length = read_decrypted(&mut *stream, &mut receive, 2, stop, deadline)?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    if length > MAX_INITIAL {
        return Err("Encrypted initial payload is too long".into());
    }
    let initial = read_decrypted(&mut *stream, &mut receive, length, stop, deadline)?;
    // Like libtorrent by default, keep the payload in plaintext when the peer
    // accepts it: the handshake is already obfuscated and RC4 costs CPU.
    let select = if provide & PLAINTEXT != 0 {
        PLAINTEXT
    } else if provide & RC4 != 0 {
        RC4
    } else {
        return Err("Peer offered no supported encryption".into());
    };
    let mut answer = VC.to_vec();
    answer.extend_from_slice(&select.to_be_bytes());
    answer.extend_from_slice(&[0; 2]);
    send.apply(&mut answer);
    write_all_deadline(&mut *stream, &answer, stop, deadline)?;
    let stream = Encrypted {
        inner: stream,
        initial,
        offset: 0,
        cipher: (select == RC4).then_some((send, receive)),
    };
    Ok((Box::new(stream), info_hash))
}

/// The payload stream after an encrypted handshake: RC4 both ways, or
/// plaintext when that was selected. The initial payload is read first.
struct Encrypted {
    inner: Box<dyn NetworkStream>,
    initial: Vec<u8>,
    offset: usize,
    /// Sending, then receiving.
    cipher: Option<(Rc4, Rc4)>,
}
impl Read for Encrypted {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.offset < self.initial.len() {
            let n = buffer.len().min(self.initial.len() - self.offset);
            buffer[..n].copy_from_slice(&self.initial[self.offset..self.offset + n]);
            self.offset += n;
            return Ok(n);
        }
        let n = self.inner.read(buffer)?;
        if let Some((_, receive)) = &mut self.cipher {
            receive.apply(&mut buffer[..n]);
        }
        Ok(n)
    }
}
impl Write for Encrypted {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some((send, _)) = &mut self.cipher else {
            return self.inner.write(buffer);
        };
        // The keystream advances only over bytes the stream accepted.
        let mut trial = send.clone();
        let mut data = buffer.to_vec();
        trial.apply(&mut data);
        let n = self.inner.write(&data)?;
        if n == data.len() {
            *send = trial;
        } else {
            send.apply(&mut data[..n]);
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl NetworkStream for Encrypted {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_write_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn rc4_matches_published_vectors() {
        for (key, plain, cipher) in [
            (&b"Key"[..], &b"Plaintext"[..], "bbf316e8d940af0ad3"),
            (b"Wiki", b"pedia", "1021bf0420"),
            (b"Secret", b"Attack at dawn", "45a01f645fc35b383552544b9bf5"),
        ] {
            let mut data = plain.to_vec();
            Rc4::raw(key).apply(&mut data);
            assert_eq!(crate::torrent::hex(&data), cipher);
        }
    }

    #[test]
    fn partial_writes_advance_the_keystream_only_over_accepted_bytes() {
        struct Limited(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Read for Limited {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let n = bytes.len().min(7);
                self.0.lock().unwrap().extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl NetworkStream for Limited {
            fn set_read_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
                Ok(())
            }
            fn set_write_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = std::sync::Arc::default();
        let mut stream = Encrypted {
            inner: Box::new(Limited(std::sync::Arc::clone(&written))),
            initial: Vec::new(),
            offset: 0,
            cipher: Some((Rc4::new(b"key"), Rc4::new(b"other"))),
        };
        let plain: Vec<u8> = (0..100).collect();
        let mut sent = 0;
        while sent < plain.len() {
            sent += stream.write(&plain[sent..]).unwrap();
        }
        let mut expected = plain;
        Rc4::new(b"key").apply(&mut expected);
        assert_eq!(*written.lock().unwrap(), expected);
    }

    fn pair() -> (Box<dyn NetworkStream>, Box<dyn NetworkStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (Box::new(client), Box::new(server))
    }

    #[test]
    fn initiator_and_responder_agree_and_find_the_torrent() {
        let info_hash = [7; 20];
        let (client, mut server) = pair();
        let deadline = Instant::now() + Duration::from_secs(5);
        let responder = std::thread::spawn(move || {
            let mut first = [0; 68];
            read_exact_deadline(&mut *server, &mut first, None, deadline).unwrap();
            let (mut stream, found) = respond(
                server,
                &first,
                |marker| Ok((*marker == torrent_marker(&[7; 20])).then_some([7; 20])),
                None,
                deadline,
            )
            .unwrap();
            assert_eq!(found, [7; 20]);
            let mut hello = [0; 5];
            read_exact_deadline(&mut *stream, &mut hello, None, deadline).unwrap();
            assert_eq!(&hello, b"hello");
            write_all_deadline(&mut *stream, b"world", None, deadline).unwrap();
        });
        let mut stream = initiate(client, &info_hash, None, deadline).unwrap();
        write_all_deadline(&mut *stream, b"hello", None, deadline).unwrap();
        let mut world = [0; 5];
        read_exact_deadline(&mut *stream, &mut world, None, deadline).unwrap();
        assert_eq!(&world, b"world");
        responder.join().unwrap();
    }

    #[test]
    fn an_unknown_torrent_is_refused() {
        let (client, mut server) = pair();
        let deadline = Instant::now() + Duration::from_secs(5);
        let responder = std::thread::spawn(move || {
            let mut first = [0; 68];
            read_exact_deadline(&mut *server, &mut first, None, deadline).unwrap();
            respond(server, &first, |_| Ok(None), None, deadline).map(|_| ())
        });
        assert!(initiate(client, &[1; 20], None, deadline).is_err());
        assert!(responder.join().unwrap().is_err());
    }

    #[test]
    fn invalid_public_keys_are_rejected() {
        let keys = Keys::new().unwrap();
        let mut one = [0; 96];
        one[95] = 1;
        assert!(keys.shared(&[0; 96]).is_err());
        assert!(keys.shared(&one).is_err());
        assert!(keys.shared(&PRIME).is_err());
        let mut minus_one = PRIME;
        minus_one[95] -= 1;
        assert!(keys.shared(&minus_one).is_err());
        let mut two = [0; 96];
        two[95] = 2;
        assert_eq!(keys.shared(&two).unwrap(), keys.public);
    }
}
