use super::*;
use std::net::UdpSocket;

fn hub(socket: Box<dyn NetworkDatagram + Sync>, stop: &Arc<AtomicBool>) -> Arc<Hub> {
    Hub::start(
        super::super::transport::direct(),
        socket,
        false,
        stop.clone(),
    )
    .unwrap()
}

fn loopback() -> (Box<dyn NetworkDatagram + Sync>, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    (Box::new(socket), address)
}

fn connect(from: &Hub, to: SocketAddr) -> UtpStream {
    let stream = from.connect(to).unwrap();
    assert!(
        stream
            .wait_connected(Instant::now() + Duration::from_secs(5))
            .unwrap()
    );
    stream
}

fn accept(hub: &Hub) -> UtpStream {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((stream, _)) = hub.accept() {
            return stream;
        }
        assert!(Instant::now() < until, "no incoming connection");
        thread::sleep(Duration::from_millis(5));
    }
}

fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed ^ (i >> 11) as u8)
        .collect()
}

/// Sends `data` one way while the other side reads it all, then closes.
fn exchange(mut writer: UtpStream, mut reader: UtpStream, data: Vec<u8>) {
    writer
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let expected = data.clone();
    let sending = thread::spawn(move || {
        for chunk in data.chunks(16 * 1024 + 13) {
            writer.write_all(chunk).unwrap();
        }
        // Dropping the writer sends FIN after the data; the hub still
        // retransmits whatever is lost.
    });
    let mut received = Vec::new();
    reader.read_to_end(&mut received).unwrap_or_else(|e| {
        panic!(
            "read failed after {} of {} bytes: {e}",
            received.len(),
            expected.len()
        )
    });
    sending.join().unwrap();
    assert_eq!(received.len(), expected.len());
    assert!(received == expected, "corrupted transfer");
}

#[test]
fn headers_round_trip_and_extensions_are_skipped() {
    let header = Header {
        kind: ST_DATA,
        connection: 0x1234,
        timestamp: 7,
        difference: 9,
        window: 65536,
        seq: 0xfffe,
        ack: 3,
    };
    let packet = encode(&header, b"payload");
    assert_eq!(parse(&packet), Some((header, &b"payload"[..])));
    // A selective-ack extension (type 1, four bytes) before the payload.
    let mut extended = packet.clone();
    extended[1] = 1;
    extended.splice(HEADER..HEADER, [0, 4, 0xff, 0, 0, 0]);
    assert_eq!(parse(&extended), Some((header, &b"payload"[..])));
    // A truncated extension, version 2, type 5 and a bencoded DHT message.
    extended.truncate(HEADER + 3);
    assert!(parse(&extended).is_none());
    let mut version = packet.clone();
    version[0] = ST_DATA << 4 | 2;
    assert!(parse(&version).is_none());
    let mut kind = packet.clone();
    kind[0] = 5 << 4 | VERSION;
    assert!(parse(&kind).is_none());
    assert!(parse(b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaae1:q4:ping1:t2:aa1:y1:qe").is_none());
    assert!(before(0xffff, 0) && before(1, 2) && !before(2, 1) && !before(5, 5));
}

#[test]
fn streams_carry_data_both_ways_over_udp_and_end_with_fin() {
    let stop = Arc::new(AtomicBool::new(false));
    let (a_socket, _) = loopback();
    let (b_socket, b_address) = loopback();
    let (a, b) = (hub(a_socket, &stop), hub(b_socket, &stop));
    let outgoing = connect(&a, b_address);
    let incoming = accept(&b);
    let (out2, in2) = (connect(&a, b_address), accept(&b));
    let forward = thread::spawn(move || exchange(outgoing, incoming, pattern(4 << 20, 1)));
    exchange(in2, out2, pattern(3 << 20, 2));
    forward.join().unwrap();
    // Both connections closed: the hubs forget them.
    let until = Instant::now() + Duration::from_secs(5);
    while !(a.shared.conns().is_empty() && b.shared.conns().is_empty()) {
        assert!(Instant::now() < until, "connections never closed");
        thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Release);
    a.join();
    b.join();
}

type Queue = Arc<(Mutex<VecDeque<(Vec<u8>, SocketAddr)>>, Condvar)>;

/// A link that loses one packet in 13 and swaps some neighbours.
struct Lossy {
    address: SocketAddr,
    inbox: Queue,
    peer: Queue,
    sent: Mutex<u64>,
    timeout: Mutex<Option<Duration>>,
}

impl NetworkDatagram for Lossy {
    fn send_to(&self, packet: &[u8], _target: SocketAddr) -> io::Result<usize> {
        let mut sent = self.sent.lock().unwrap();
        *sent += 1;
        if sent.is_multiple_of(13) {
            return Ok(packet.len());
        }
        let mut queue = self.peer.0.lock().unwrap();
        if sent.is_multiple_of(5) && !queue.is_empty() {
            let at = queue.len() - 1;
            queue.insert(at, (packet.to_vec(), self.address));
        } else {
            queue.push_back((packet.to_vec(), self.address));
        }
        self.peer.1.notify_all();
        Ok(packet.len())
    }

    fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let timeout = self
            .timeout
            .lock()
            .unwrap()
            .unwrap_or(Duration::from_millis(100));
        let mut queue = self.inbox.0.lock().unwrap();
        if queue.is_empty() {
            queue = self.inbox.1.wait_timeout(queue, timeout).unwrap().0;
        }
        let Some((packet, from)) = queue.pop_front() else {
            return Err(ErrorKind::WouldBlock.into());
        };
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok((packet.len(), from))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self.timeout.lock().unwrap() = timeout;
        Ok(())
    }

    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn lost_and_reordered_packets_are_recovered() {
    let queues = || Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
    let (a_inbox, b_inbox) = (queues(), queues());
    let a_address: SocketAddr = "192.0.2.1:6881".parse().unwrap();
    let b_address: SocketAddr = "192.0.2.2:6881".parse().unwrap();
    let link = |address, inbox: &Arc<_>, peer: &Arc<_>| -> Box<dyn NetworkDatagram + Sync> {
        Box::new(Lossy {
            address,
            inbox: Arc::clone(inbox),
            peer: Arc::clone(peer),
            sent: Mutex::new(0),
            timeout: Mutex::new(None),
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let a = hub(link(a_address, &a_inbox, &b_inbox), &stop);
    let b = hub(link(b_address, &b_inbox, &a_inbox), &stop);
    // The SYN or its answer may be lost: retried until connected.
    let outgoing = loop {
        let stream = a.connect(b_address).unwrap();
        if stream
            .wait_connected(Instant::now() + Duration::from_secs(3))
            .unwrap_or(false)
        {
            break stream;
        }
    };
    let incoming = accept(&b);
    exchange(outgoing, incoming, pattern(1 << 20, 3));
    stop.store(true, Ordering::Release);
    a.join();
    b.join();
}

#[test]
fn a_refused_tcp_peer_is_reached_over_utp_and_unknown_packets_are_reset() {
    let stop = Arc::new(AtomicBool::new(false));
    let (a_socket, _) = loopback();
    // A UDP port with no TCP listener behind it: TCP is refused at once.
    let (b_socket, b_address) = loopback();
    let (a, b) = (hub(a_socket, &stop), hub(b_socket, &stop));
    let network = WithUtp {
        inner: super::super::transport::direct(),
        hub: a.clone(),
    };
    let started = Instant::now();
    let mut stream = network.connect(b_address, Duration::from_secs(2)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    let mut answer = accept(&b);
    stream.write_all(b"hello").unwrap();
    let mut buffer = [0; 5];
    answer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    answer.read_exact(&mut buffer).unwrap();
    assert_eq!(&buffer, b"hello");
    // The listener hands uTP peers over after TCP ones.
    let tcp = network.listen("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = Listener {
        tcp,
        hub: b.clone(),
    };
    assert!(matches!(listener.accept(), Err(e) if e.kind() == ErrorKind::WouldBlock));
    let _second = network.connect(b_address, Duration::from_secs(2)).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let accepted = loop {
        match listener.accept() {
            Ok((_, address)) => break address,
            Err(_) if Instant::now() < until => thread::sleep(Duration::from_millis(5)),
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(accepted.ip(), b_address.ip());

    // A packet for no connection is answered with a reset.
    let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
    probe
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let header = Header {
        kind: ST_DATA,
        connection: 77,
        timestamp: 0,
        difference: 0,
        window: 0,
        seq: 5,
        ack: 0,
    };
    probe.send_to(&encode(&header, b"x"), b_address).unwrap();
    let mut reply = [0; 64];
    let (n, _) = probe.recv_from(&mut reply).unwrap();
    let (reset, _) = parse(&reply[..n]).unwrap();
    assert_eq!((reset.kind, reset.connection, reset.ack), (ST_RESET, 77, 5));

    // A reset ends the stream; a stopped hub aborts the rest.
    drop(answer);
    stop.store(true, Ordering::Release);
    a.join();
    b.join();
    let mut buffer = [0; 1];
    assert!(stream.read(&mut buffer).is_err() || stream.write(b"x").is_err());
}

#[test]
fn ledbat_backs_off_above_the_target_delay_and_timeouts_shrink_the_window() {
    let now = Instant::now();
    let mut st = State::new(Phase::Connected, 1, 0, now);
    st.slow_start = false;
    st.cwnd = 64 * PAYLOAD;
    // A first sample sets the base delay; no queuing grows the window.
    st.congestion(PAYLOAD, 50_000, now);
    let grown = st.cwnd;
    assert!(grown > 64 * PAYLOAD);
    // 200 ms of queuing, twice the target: the window shrinks.
    for _ in 0..50 {
        st.congestion(PAYLOAD, 250_000, now);
    }
    assert!(st.cwnd < grown);
    // Never below one packet, never above the maximum.
    for _ in 0..10_000 {
        st.congestion(PAYLOAD, 2_000_000, now);
    }
    assert_eq!(st.cwnd, PAYLOAD);
    st.slow_start = true;
    for _ in 0..10_000 {
        st.congestion(PAYLOAD, 50_000, now);
    }
    assert_eq!(st.cwnd, MAX_CWND);

    // RTT samples set the retransmission timeout, within its bounds.
    st.rtt_sample(Duration::from_millis(10));
    assert_eq!(st.rto, MIN_RTO);
    for _ in 0..20 {
        st.rtt_sample(Duration::from_secs(60));
    }
    assert_eq!(st.rto, MAX_RTO);
    // Expiries back the timeout off, up to the maximum.
    st.rtt_sample(Duration::from_millis(400));
    st.rto = Duration::from_secs(1);
    st.timeouts = 3;
    assert_eq!(st.backed_off(), Duration::from_secs(8));
    st.timeouts = 6;
    assert_eq!(st.backed_off(), MAX_RTO);
}
