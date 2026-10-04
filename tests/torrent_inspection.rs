//! Metadata-only source inspection with original synthetic peers and trackers.
mod automatic_pack_support;
#[allow(dead_code)]
mod transfer_support;
use automatic_pack_support::Provider;
use mynou::{
    bencode::{self, Value as Bencode},
    crypto::{sha1, sha256},
    torrent::inspect_metadata,
};
use std::{
    collections::BTreeMap,
    fs,
    net::UdpSocket,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use transfer_support::{BLOCK, RecordingProxy, Scratch, Seeder, Torrent, payload};

#[test]
fn explicit_magnet_inspection_authenticates_metadata_without_requesting_payload_or_writing_state() {
    let scratch = Scratch::new();
    let torrent = Torrent::multiple(
        &scratch.0.join("meta"),
        "Pack",
        vec![
            ("S01E01.mp4".into(), payload(BLOCK, 5)),
            ("unneeded.bin".into(), payload(BLOCK * 2, 11)),
        ],
    );
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let metadata = inspect_metadata(
        &torrent.magnet(proxy.port),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(metadata.id, torrent.id);
    assert_eq!(metadata.files[0].path, "Pack/S01E01.mp4");
    assert_eq!(metadata.files[0].length, BLOCK as u64);
    assert!(proxy.requests.lock().unwrap().is_empty());
    assert!(!scratch.0.join("client").exists());
    let known = inspect_metadata(
        torrent.path.to_str().unwrap(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(known, metadata);
}

#[test]
fn http_tracker_lookup_uses_only_stopped_events_and_never_claims_payload_or_completion() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0.join("meta"), "S01E01.mp4", payload(BLOCK, 17));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let provider = Provider::open();
    let mut peers = vec![127, 0, 0, 1];
    peers.extend_from_slice(&proxy.port.to_be_bytes());
    provider.response(
        "/tracker",
        200,
        bencode::encode(&Bencode::Dict(BTreeMap::from([
            (b"interval".to_vec(), Bencode::Int(1800)),
            (b"peers".to_vec(), Bencode::Bytes(peers)),
        ]))),
    );
    let magnet = format!(
        "magnet:?xt=urn:btih:{}&tr={}/tracker",
        torrent.id, provider.url
    );
    let metadata = inspect_metadata(&magnet, Instant::now() + Duration::from_secs(5)).unwrap();
    assert_eq!(metadata.id, torrent.id);
    let requests = provider.calls.lock().unwrap();
    assert_eq!(requests.len(), 1);
    for expected in ["event=stopped", "downloaded=0", "uploaded=0", "left=1"] {
        assert!(requests[0].contains(expected));
    }
    assert!(!requests[0].contains("event=completed"));
    assert!(proxy.requests.lock().unwrap().is_empty());
}

#[test]
fn udp_tracker_metadata_discovery_preserves_transaction_ids_and_zero_payload_counters() {
    let scratch = Scratch::new();
    let torrent = Torrent::single(&scratch.0.join("meta"), "S01E01.mp4", payload(BLOCK, 23));
    let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
    let proxy = RecordingProxy::open(seed.client.listen_port());
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let tracker = socket.local_addr().unwrap();
    let peer_port = proxy.port;
    let worker = thread::spawn(move || {
        let mut bytes = [0; 1024];
        let (n, address) = socket.recv_from(&mut bytes).unwrap();
        assert_eq!(n, 16);
        assert_eq!(&bytes[8..12], &0_u32.to_be_bytes());
        let mut response = vec![0, 0, 0, 0];
        response.extend_from_slice(&bytes[12..16]);
        response.extend_from_slice(&12345_u64.to_be_bytes());
        socket.send_to(&response, address).unwrap();
        let (n, address) = socket.recv_from(&mut bytes).unwrap();
        assert_eq!(n, 98);
        assert_eq!(&bytes[8..12], &1_u32.to_be_bytes());
        assert_eq!(&bytes[56..64], &0_u64.to_be_bytes());
        assert_eq!(&bytes[64..72], &1_u64.to_be_bytes());
        assert_eq!(&bytes[72..80], &0_u64.to_be_bytes());
        assert_eq!(&bytes[80..84], &3_u32.to_be_bytes());
        let mut response = 1_u32.to_be_bytes().to_vec();
        response.extend_from_slice(&bytes[12..16]);
        response.extend_from_slice(&1800_u32.to_be_bytes());
        response.extend_from_slice(&0_u32.to_be_bytes());
        response.extend_from_slice(&1_u32.to_be_bytes());
        response.extend_from_slice(&[127, 0, 0, 1]);
        response.extend_from_slice(&peer_port.to_be_bytes());
        socket.send_to(&response, address).unwrap();
    });
    let magnet = format!(
        "magnet:?xt=urn:btih:{}&tr=udp://{tracker}/announce",
        torrent.id
    );
    assert_eq!(
        inspect_metadata(&magnet, Instant::now() + Duration::from_secs(5))
            .unwrap()
            .id,
        torrent.id
    );
    worker.join().unwrap();
    assert!(proxy.requests.lock().unwrap().is_empty());
}

#[test]
fn v2_and_hybrid_metadata_inspection_keep_the_authenticated_source_alias_without_payload() {
    for hybrid in [false, true] {
        let scratch = Scratch::new();
        let bytes = payload(9000, 31);
        let leaf = Bencode::Dict(BTreeMap::from([
            (b"length".to_vec(), Bencode::Int(bytes.len() as i64)),
            (
                b"pieces root".to_vec(),
                Bencode::Bytes(sha256(&bytes).to_vec()),
            ),
        ]));
        let tree = Bencode::Dict(BTreeMap::from([(
            b"S01E01.mp4".to_vec(),
            Bencode::Dict(BTreeMap::from([(Vec::new(), leaf)])),
        )]));
        let mut info = BTreeMap::from([
            (b"name".to_vec(), Bencode::Bytes(b"Pack".to_vec())),
            (b"piece length".to_vec(), Bencode::Int(BLOCK as i64)),
            (b"private".to_vec(), Bencode::Int(1)),
            (b"meta version".to_vec(), Bencode::Int(2)),
            (b"file tree".to_vec(), tree),
        ]);
        if hybrid {
            info.insert(
                b"files".to_vec(),
                Bencode::List(vec![Bencode::Dict(BTreeMap::from([
                    (b"length".to_vec(), Bencode::Int(bytes.len() as i64)),
                    (
                        b"path".to_vec(),
                        Bencode::List(vec![Bencode::Bytes(b"S01E01.mp4".to_vec())]),
                    ),
                ]))]),
            );
            info.insert(b"pieces".to_vec(), Bencode::Bytes(sha1(&bytes).to_vec()));
        }
        let info = Bencode::Dict(info);
        let raw = bencode::encode(&info);
        let v2: String = sha256(&raw).iter().map(|b| format!("{b:02x}")).collect();
        let v1: String = sha1(&raw).iter().map(|b| format!("{b:02x}")).collect();
        let path = scratch.0.join("pack.torrent");
        fs::write(
            &path,
            bencode::encode(&Bencode::Dict(BTreeMap::from([(b"info".to_vec(), info)]))),
        )
        .unwrap();
        let torrent = Torrent {
            id: v2.clone(),
            path,
            files: vec![(Path::new("Pack").join("S01E01.mp4"), bytes)],
            total: 9000,
        };
        let seed = Seeder::open(&scratch.0.join("seed"), &[&torrent]);
        let proxy = RecordingProxy::open(seed.client.listen_port());
        let magnet = format!("magnet:?xt=urn:btmh:1220{v2}&x.pe=127.0.0.1:{}", proxy.port);
        assert_eq!(
            inspect_metadata(&magnet, Instant::now() + Duration::from_secs(5))
                .unwrap()
                .id,
            v2
        );
        if hybrid {
            assert_eq!(
                inspect_metadata(
                    &torrent.magnet(proxy.port).replace(&torrent.id, &v1),
                    Instant::now() + Duration::from_secs(5)
                )
                .unwrap()
                .id,
                v1
            );
        }
        assert!(proxy.requests.lock().unwrap().is_empty());
    }
}

#[test]
fn inspection_file_limits_expired_deadlines_and_bad_hashes_fail_without_payload() {
    let scratch = Scratch::new();
    let huge = Torrent::multiple(
        &scratch.0.join("meta"),
        "Large",
        (0..1025)
            .map(|n| (format!("file-{n}.txt").into(), vec![1]))
            .collect(),
    );
    assert!(
        inspect_metadata(
            huge.path.to_str().unwrap(),
            Instant::now() + Duration::from_secs(5)
        )
        .unwrap_err()
        .contains("1024 files")
    );
    assert!(
        inspect_metadata(
            huge.path.to_str().unwrap(),
            Instant::now() - Duration::from_secs(1)
        )
        .unwrap_err()
        .contains("deadline")
    );
    assert!(
        inspect_metadata(
            "magnet:?xt=urn:btih:bad",
            Instant::now() + Duration::from_secs(5)
        )
        .is_err()
    );
    assert!(
        inspect_metadata(
            &format!("magnet:?xt=urn:btih:{}", "1".repeat(40)),
            Instant::now() + Duration::from_secs(5)
        )
        .is_err()
    );
}
