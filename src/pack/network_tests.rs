//! Shared-file results are fenced by the network generation that fetched them.
use super::SharedFileRequest;
use crate::{
    Result,
    bencode::{self, Value},
    crypto::random_bytes,
    engine::{Engine, lock},
    net::{HttpClient, HttpConnector, NetworkDatagram, NetworkStream},
    numbering::EpisodeNumber,
    series::{Episode, Plan, Record},
    store::Request,
    torrent::transport::{Network, NetworkListener},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Serves one `.torrent` per request; optionally revokes its generation
/// while the response is in flight, as a lost tunnel would.
struct Revocable {
    generation: AtomicU64,
    revoked: AtomicBool,
    revoke_during_fetch: AtomicBool,
}

impl HttpConnector for Revocable {
    fn connect(&self, _: &str, _: u16, _: Instant) -> Result<Box<dyn NetworkStream>> {
        if self.revoke_during_fetch.load(Ordering::Acquire) {
            self.revoked.store(true, Ordering::Release);
        }
        let body = torrent();
        let mut reply =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        reply.extend_from_slice(&body);
        Ok(Box::new(Reply(io::Cursor::new(reply))))
    }
}

struct Fetch(Arc<Revocable>);

impl Network for Fetch {
    fn connect(&self, _: SocketAddr, _: Duration) -> Result<Box<dyn NetworkStream>> {
        Err("No peers in this fixture".into())
    }
    fn resolve(&self, _: &str, _: Instant) -> Result<Vec<SocketAddr>> {
        Err("No names in this fixture".into())
    }
    fn datagram(&self, _: bool) -> Result<Box<dyn NetworkDatagram>> {
        Err("No datagrams in this fixture".into())
    }
    fn http(&self) -> HttpClient {
        HttpClient::new().with_connector(self.0.clone())
    }
    fn listen(&self, _: SocketAddr) -> Result<Box<dyn NetworkListener>> {
        Err("No listener in this fixture".into())
    }
    fn generation(&self) -> Result<Option<u64>> {
        if self.0.revoked.load(Ordering::Acquire) {
            return Err("Torrent network generation revoked".into());
        }
        Ok(Some(self.0.generation.load(Ordering::Acquire)))
    }
}

struct Reply(io::Cursor<Vec<u8>>);
impl Read for Reply {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Write for Reply {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl NetworkStream for Reply {
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

fn torrent() -> Vec<u8> {
    let file = Value::Dict(BTreeMap::from([
        (b"length".to_vec(), Value::Int(1)),
        (
            b"path".to_vec(),
            Value::List(vec![Value::Bytes(b"shared.mp4".to_vec())]),
        ),
    ]));
    bencode::encode(&Value::Dict(BTreeMap::from([(
        b"info".to_vec(),
        Value::Dict(BTreeMap::from([
            (b"name".to_vec(), Value::Bytes(b"Pack".to_vec())),
            (b"files".to_vec(), Value::List(vec![file])),
            (b"piece length".to_vec(), Value::Int(16_384)),
            (b"pieces".to_vec(), Value::Bytes(vec![7; 20])),
        ])),
    )])))
}

fn series() -> Record {
    let episodes: Vec<Episode> = (1..=2)
        .map(|episode| Episode {
            catalog_id: Some(1_000 + u64::from(episode)),
            season: 1,
            episode,
            title: format!("Episode {episode}"),
            air_date: Some("2024-01-01".into()),
        })
        .collect();
    let anchors = episodes
        .iter()
        .map(|e| {
            let number = EpisodeNumber {
                season: e.season,
                episode: e.episode,
            };
            (e.catalog_id.unwrap(), number)
        })
        .collect();
    Record {
        id: "0123456789abcdef0123456789abcdef".into(),
        plan: Plan {
            request: Request {
                kind: "series".into(),
                title: "Synthetic Series".into(),
                year: 2020,
                season: 0,
                episode: 0,
                source_path: None,
                source_url: None,
                tmdb_id: Some(4242),
                source_numbering: None,
            },
            episodes,
        },
        monitored: true,
        include_specials: false,
        start_date: None,
        excluded: BTreeSet::new(),
        revision: 1,
        created_at: 10,
        updated_at: 10,
        checked_at: 10,
        next_check_at: 1_000,
        last_error: None,
        anchors,
        numbering: BTreeMap::new(),
    }
}

fn engine(directory: &Directory) -> (Arc<Engine>, Arc<Revocable>) {
    let mut config =
        crate::config::from_json(&crate::config::default_json(), &directory.0).unwrap();
    config.downloads_enabled = false;
    let mut engine = Engine::open_for_management(config).unwrap();
    let network = Arc::new(Revocable {
        generation: AtomicU64::new(7),
        revoked: AtomicBool::new(false),
        revoke_during_fetch: AtomicBool::new(false),
    });
    Arc::get_mut(&mut engine).unwrap().torrent_network = Arc::new(Fetch(network.clone()));
    lock(&engine.series_store).unwrap().save(series()).unwrap();
    (engine, network)
}

fn query() -> SharedFileRequest {
    SharedFileRequest {
        source_url: "http://source.invalid/pack.torrent".into(),
        file_path: "Pack/shared.mp4".into(),
        season: 1,
        episodes: vec![1, 2],
        apply: false,
        plan_id: None,
    }
}

fn plan_id(engine: &Engine) -> String {
    let preview = engine.shared_file(&series().id, &query()).unwrap();
    preview.get("plan_id").unwrap().as_str().unwrap().into()
}

#[test]
fn shared_files_discard_results_of_a_revoked_generation_and_bind_previews_to_it() {
    let suffix: String = random_bytes::<16>()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let directory = Directory(std::env::temp_dir().join(format!("mynou-pack-network-{suffix}")));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory.0)
        .unwrap();
    let (engine, network) = engine(&directory);
    let id = series().id;
    let first = plan_id(&engine);
    assert_eq!(plan_id(&engine), first);

    // A preview of an older generation cannot authorize an apply in a newer one.
    network.generation.store(8, Ordering::Release);
    let mut apply = query();
    apply.apply = true;
    apply.plan_id = Some(first.clone());
    let error = engine.shared_file(&id, &apply).unwrap_err();
    assert_eq!(
        error,
        "Shared-file preview changed; preview again before acquisition"
    );
    assert_ne!(plan_id(&engine), first);
    assert!(lock(&engine.store).unwrap().list().unwrap().is_empty());

    // Revocation while metadata is in flight discards preview and apply alike.
    apply.plan_id = Some(plan_id(&engine));
    network.revoke_during_fetch.store(true, Ordering::Release);
    for query in [query(), apply] {
        network.revoked.store(false, Ordering::Release);
        let error = engine.shared_file(&id, &query).unwrap_err();
        assert_eq!(error, "Torrent network changed during metadata inspection");
    }
    assert!(lock(&engine.store).unwrap().list().unwrap().is_empty());

    // The current generation still commits the same request.
    network.revoke_during_fetch.store(false, Ordering::Release);
    network.revoked.store(false, Ordering::Release);
    let mut apply = query();
    apply.apply = true;
    apply.plan_id = Some(plan_id(&engine));
    let report = engine.shared_file(&id, &apply).unwrap();
    assert_eq!(report.get("jobs").unwrap().as_array().unwrap().len(), 2);
    assert_eq!(lock(&engine.store).unwrap().list().unwrap().len(), 2);
}
