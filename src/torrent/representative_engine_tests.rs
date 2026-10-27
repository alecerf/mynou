//! Full Engine extension of the opt-in retained-load fixture. All identities,
//! media, gateway responses and source offers are synthetic.
use super::*;
use crate::{
    config,
    engine::{Engine, Workers, lock},
    net::HttpClient,
    server::Api,
    store::Request,
};
use std::ops::Deref;

const TOKEN: &str = "synthetic-load-token-0123456789abcdef";

pub(super) enum Subject {
    Vacant,
    Client(Client),
    Engine(Running),
}
impl Subject {
    pub fn open(
        config: DownloadConfig,
        network: Arc<dyn Network>,
        fixture: Option<&Fixture>,
    ) -> Self {
        if let Some(fixture) = fixture {
            Self::Engine(fixture.open())
        } else {
            Self::Client(
                Client::open_with_policy_and_admission(
                    config,
                    TransferPolicy::default(),
                    Arc::new(|_| Ok(true)),
                    network,
                )
                .unwrap(),
            )
        }
    }
    pub fn probe(&self) {
        if let Self::Engine(running) = self {
            running.observe_monitoring();
            for route in [
                "/api/transfers?limit=50",
                "/api/jobs?limit=50",
                "/api/library?limit=50",
                "/api/series?limit=50",
            ] {
                let at = Instant::now();
                running.call("GET", route, None);
                running
                    .metrics
                    .lock()
                    .unwrap()
                    .api_us
                    .push(at.elapsed().as_micros());
            }
        }
    }
}
impl Deref for Subject {
    type Target = Client;
    fn deref(&self) -> &Client {
        match self {
            Self::Vacant => panic!("subject is restarting"),
            Self::Client(client) => client,
            Self::Engine(running) => running.engine.native_client().unwrap(),
        }
    }
}

pub(super) fn acquiring(client: &Client, torrents: &[Torrent]) -> bool {
    let jobs = client.jobs.lock().unwrap();
    torrents.iter().any(|torrent| {
        jobs.get(&torrent.hash)
            .is_some_and(|job| job.running && !job.status.ready && job.status.progress > 0.0)
    })
}

#[derive(Default)]
struct Metrics {
    api_us: Vec<u128>,
    import_us: Vec<u128>,
    imports: u64,
    imported: Vec<String>,
    monitoring_pending: BTreeMap<String, Instant>,
    monitoring_us: Vec<u128>,
    restarts_in_flight: u64,
    generation_changes: u64,
    port_changes: u64,
    renewals: u64,
    feed_successes: u64,
}
pub(super) struct Running {
    engine: Arc<Engine>,
    workers: Option<Workers>,
    api: Option<JoinHandle<crate::Result<()>>>,
    origin: String,
    metrics: Arc<Mutex<Metrics>>,
}
impl Running {
    fn observe_monitoring(&self) {
        let mut metrics = self.metrics.lock().unwrap();
        let mut done = Vec::new();
        for (id, began) in &metrics.monitoring_pending {
            if lock(&self.engine.store)
                .unwrap()
                .get(id)
                .unwrap()
                .is_some_and(|j| j.monitor_checked_at > 0)
            {
                done.push((id.clone(), began.elapsed().as_micros()));
            }
        }
        for (id, elapsed) in done {
            metrics.monitoring_pending.remove(&id);
            metrics.monitoring_us.push(elapsed);
        }
    }
    fn call(&self, method: &str, route: &str, body: Option<&Json>) -> Json {
        let bytes = body.map_or_else(Vec::new, |v| json::stringify(v).into_bytes());
        let response = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(30))
            .request(
                method,
                &format!("{}{route}", self.origin),
                &[
                    ("Authorization".into(), format!("Bearer {TOKEN}")),
                    ("Content-Type".into(), "application/json".into()),
                ],
                &bytes,
            );
        let Ok(response) = response else {
            panic!("Synthetic Engine API request failed");
        };
        assert!(
            (200..300).contains(&response.status),
            "Synthetic Engine API request was rejected"
        );
        let Ok(body) = std::str::from_utf8(&response.body) else {
            panic!("Synthetic Engine API returned non-UTF-8 data");
        };
        let Ok(value) = json::parse(body) else {
            panic!("Synthetic Engine API returned invalid JSON");
        };
        value
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        self.workers.take();
        if let Some(api) = self.api.take() {
            api.join().unwrap().unwrap();
        }
    }
}

pub(super) struct Fixture {
    config: config::Config,
    started: AtomicBool,
    operator: Arc<crate::vpn::Operator>,
    metrics: Arc<Mutex<Metrics>>,
    gateway: crate::tunnel_http::tests::NatPmp,
    source: Service,
    catalog: Service,
    candidate: String,
    selection_id: String,
    source_file: PathBuf,
}
impl Fixture {
    pub fn new(
        root: &Path,
        pair: &Pair,
        downloads: &DownloadConfig,
        library: &Path,
        inventory: &[Torrent],
    ) -> Self {
        // A new info hash over an actively admitted retained payload. Discovery
        // must verify shared data; it cannot fetch another payload silently.
        let original =
            fs::read(library.join(format!("{}.torrent", inventory.last().unwrap().name))).unwrap();
        let Value::Dict(mut meta) = crate::bencode::parse(&original).unwrap() else {
            panic!("fixture metainfo");
        };
        let Value::Dict(info) = meta.get_mut(b"info".as_slice()).unwrap() else {
            panic!("fixture info");
        };
        info.insert(
            b"source".to_vec(),
            Value::Bytes(b"synthetic-cross-seed".to_vec()),
        );
        let candidate = hex(&crate::crypto::sha1(&crate::bencode::encode(
            meta.get(b"info".as_slice()).unwrap(),
        )));
        let encoded = crate::bencode::encode(&Value::Dict(meta));
        let title = &inventory.last().unwrap().name;
        let feed = format!(
            r#"<rss><channel><item><title>{title}</title><guid>load-cross-seed</guid><size>1048576</size><enclosure url="/candidate" type="application/x-bittorrent"/></item></channel></rss>"#
        );
        let source = Service::open(
            pair.server_network
                .listen("0.0.0.0:0".parse().unwrap())
                .unwrap(),
            false,
            feed.into_bytes(),
            encoded,
        );
        let direct = transport::direct();
        let catalog = Service::open(
            direct.listen("127.0.0.1:0".parse().unwrap()).unwrap(),
            true,
            Vec::new(),
            Vec::new(),
        );
        let mut settings = config::default_json();
        settings.insert("listen", "127.0.0.1:0");
        settings.insert("poll_interval_ms", 50_u32);
        settings.insert("selection", json::parse(r#"{"profiles":{"load":{"resolutions":[2160,1080,720],"cutoff_resolution":2160}},"movie_profile":"load","episode_profile":"load"}"#).unwrap());
        settings.insert("indexers", json::parse(&format!(r#"[{{"name":"load-source","id":"load-source","kind":"torznab","url":"http://10.7.0.1:{}/api","protection":"protected","api_key_env":"MYNOU_LOAD_ABSENT_KEY","watch":{{"enabled":true,"interval_secs":60}},"cross_seed":{{"enabled":true,"interval_secs":60}}}}]"#,source.port)).unwrap());
        let mut config = config::from_json(&settings, &root.join("engine")).unwrap();
        config.downloads = downloads.clone();
        config.download_policy.download_limit_bps = 64 << 10;
        config.monitoring.enabled = true;
        config.monitoring.interval_secs = 60;
        config.catalog.enabled = true;
        config.catalog.url = format!("http://127.0.0.1:{}/3", catalog.port);
        config.catalog.token_env = "PATH".into(); // Existing fixture convention, never logged.
        let gateway = crate::tunnel_http::tests::NatPmp::start(&pair.server, 53186);
        let operator = crate::vpn::tests::load_operator(&config, pair.client.clone());
        let source_file = root.join("synthetic.mp4");
        fs::write(
            &source_file,
            include_bytes!("../../tests/fixtures/demo.mp4"),
        )
        .unwrap();
        Self {
            config,
            started: AtomicBool::new(false),
            operator,
            metrics: Arc::new(Mutex::new(Metrics::default())),
            gateway,
            source,
            catalog,
            candidate,
            selection_id: inventory[2].hash.clone(),
            source_file,
        }
    }
    fn open(&self) -> Running {
        let engine = Engine::open_operator(
            self.config.clone(),
            true,
            false,
            true,
            Some(self.operator.clone()),
        )
        .unwrap();
        wait_for(
            Instant::now() + Duration::from_secs(90),
            "loaded engine startup",
            || engine.startup_step().unwrap(),
        );
        let api = Api::bind(engine.clone()).unwrap();
        let origin = format!("http://{}", api.address().unwrap());
        let api = thread::spawn(move || api.run());
        let workers = self.started.load(Ordering::Acquire).then(|| engine.start());
        Running {
            engine,
            workers,
            api: Some(api),
            origin,
            metrics: self.metrics.clone(),
        }
    }
    pub fn begin(&self, subject: &mut Subject) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let Subject::Engine(running) = subject else {
            unreachable!()
        };
        running.workers = Some(running.engine.start());
    }
    pub fn exercise(
        &self,
        subject: &mut Subject,
        round: usize,
        acquisitions: &[Torrent],
        pair: &Pair,
        reopen: &impl Fn() -> Subject,
    ) {
        assert!(
            acquiring(subject, acquisitions),
            "automation must overlap verified acquisition progress"
        );
        let Subject::Engine(running) = subject else {
            return;
        };
        // Port-changing incidents wait for incoming consumers to finish. Later
        // rounds start automation as soon as acquisition progress is observed.
        let source_file = self
            .source_file
            .with_file_name(format!("synthetic-{round}.mp4"));
        fs::copy(&self.source_file, &source_file).unwrap();
        for kind in ["movie", "episode"] {
            let at = Instant::now();
            let request = Request {
                kind: kind.into(),
                title: format!("Synthetic load {round}"),
                year: 2026,
                season: if kind == "episode" { 1 } else { 0 },
                episode: if kind == "episode" { 1 } else { 0 },
                source_path: Some(source_file.to_str().unwrap().into()),
                source_url: None,
                source_numbering: None,
                tmdb_id: None,
            };
            let first = running.call("POST", "/api/jobs", Some(&request.to_json()));
            let duplicate = running.call("POST", "/api/jobs", Some(&request.to_json()));
            let id = first.as_array().unwrap()[0]
                .get("id")
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(
                duplicate.as_array().unwrap()[0].get("id").unwrap().as_str(),
                Some(id)
            );
            wait_for(
                Instant::now() + Duration::from_secs(60),
                "loaded media import",
                || {
                    lock(&running.engine.store)
                        .unwrap()
                        .get(id)
                        .unwrap()
                        .is_some_and(|j| j.state == "ready")
                },
            );
            let job = lock(&running.engine.store)
                .unwrap()
                .get(id)
                .unwrap()
                .unwrap();
            assert_eq!(job.imports.len(), 1);
            assert_eq!(
                fs::read(&job.imports[0]).unwrap(),
                include_bytes!("../../tests/fixtures/demo.mp4")
            );
            let import_us = at.elapsed().as_micros();
            let release = if kind == "movie" {
                format!("Synthetic load {round}.2026.2160p.WEB-DL.H265")
            } else {
                format!("Synthetic load {round}.S01E01.2160p.WEB-DL.H265")
            };
            let mut baseline = Json::object();
            baseline.insert("release_title", release);
            running.call(
                "POST",
                &format!("/api/library/{id}/baseline"),
                Some(&baseline),
            );
            let mut monitor = Json::object();
            monitor.insert("enabled", true);
            running.call(
                "POST",
                &format!("/api/library/{id}/monitor"),
                Some(&monitor),
            );
            let mut metrics = self.metrics.lock().unwrap();
            metrics
                .monitoring_pending
                .insert(id.to_owned(), Instant::now());
            metrics.imported.push(id.to_owned());
            metrics.imports += 1;
            metrics.import_us.push(import_us);
        }
        // Refresh a real monitored series through the catalog each round.
        let request = Request {
            kind: "series".into(),
            title: "Synthetic monitored series".into(),
            year: 2024,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            source_numbering: None,
            tmdb_id: Some(42),
        };
        let record = running.engine.track_series(&request, false, true).unwrap();
        let id = record.get("id").unwrap().as_str().unwrap();
        running.engine.refresh_series(id).unwrap();
        if round < 3 {
            assert!(
                acquiring(running.engine.native_client().unwrap(), acquisitions),
                "incident must interrupt a download with verified payload progress"
            );
            match round {
                0 => self.metrics.lock().unwrap().restarts_in_flight += 1,
                1 => {
                    self.gateway.port.store(53190, Ordering::Release);
                    wait_for(
                        Instant::now() + Duration::from_secs(20),
                        "loaded NAT-PMP change",
                        || running.engine.vpn_needs_reload(),
                    );
                    self.metrics.lock().unwrap().port_changes += 1;
                }
                _ => {
                    pair.replace_client_generation();
                    wait_for(
                        Instant::now() + Duration::from_secs(20),
                        "loaded generation loss",
                        || running.engine.vpn_needs_reload(),
                    );
                    assert!(running.engine.torrent_network.generation().is_err());
                    self.metrics.lock().unwrap().generation_changes += 1;
                }
            }
            running.engine.retire_vpn_generation();
            // Drop the engine completely before reopening the same state.
            let old = std::mem::replace(subject, Subject::Vacant);
            drop(old);
            *subject = reopen();
            assert_eq!(
                subject.listen_port(),
                if round == 0 { 53186 } else { 53190 }
            );
            assert_eq!(
                subject.controls(&self.selection_id).unwrap().file_selection,
                FileSelection::paths(&["load-2.bin".into()]).unwrap()
            );
        }
    }
    pub fn finish(&self, subject: &Subject) -> u64 {
        let Subject::Engine(running) = subject else {
            unreachable!()
        };
        eprintln!(
            "MYNOU_ENGINE_DISCOVERY {}",
            json::stringify(&running.engine.cross_seed_status().unwrap())
        );
        wait_for(
            Instant::now() + Duration::from_secs(120),
            "automatic cross-seed registration",
            || subject.controls(&self.candidate).is_ok(),
        );
        subject.set_priority(&self.candidate, 11).unwrap();
        wait_for(
            Instant::now() + Duration::from_secs(120),
            "automatic cross-seed",
            || subject.check(&self.candidate).is_ok_and(|s| s.ready),
        );
        let report = running.engine.cross_seed_status().unwrap();
        assert_eq!(report.get("added_total").and_then(Json::as_str), Some("1"));
        assert!(self.source.requests.load(Ordering::Relaxed) > 0);
        let feeds = running.engine.feeds().unwrap();
        let successes = feeds.get("sources").unwrap().as_array().unwrap()[0]
            .get("successes")
            .and_then(Json::as_str)
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            successes > 0,
            "automatic protected feed polling must succeed"
        );
        self.metrics.lock().unwrap().feed_successes = successes;
        assert!(self.catalog.requests.load(Ordering::Relaxed) > 0);
        wait_for(
            Instant::now() + Duration::from_secs(90),
            "loaded library monitoring",
            || {
                running.observe_monitoring();
                self.metrics.lock().unwrap().monitoring_pending.is_empty()
            },
        );
        for id in &self.metrics.lock().unwrap().imported {
            let job = lock(&running.engine.store)
                .unwrap()
                .get(id)
                .unwrap()
                .unwrap();
            assert_eq!(job.state, "ready");
            assert_eq!(job.imports.len(), 1);
            assert_eq!(
                fs::read(&job.imports[0]).unwrap(),
                include_bytes!("../../tests/fixtures/demo.mp4")
            );
        }
        self.metrics.lock().unwrap().renewals =
            self.gateway.requests.load(Ordering::Relaxed) as u64;
        1
    }
    pub fn report(&self, report: &mut Json) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.api_us.sort_unstable();
        metrics.import_us.sort_unstable();
        metrics.monitoring_us.sort_unstable();
        report.insert("full_engine", true);
        report.insert("cross_seed_added", "1");
        for (key, value) in [
            ("api_samples", metrics.api_us.len() as u128),
            (
                "api_p95_us",
                metrics.api_us[(metrics.api_us.len() - 1) * 95 / 100],
            ),
            ("api_max_us", *metrics.api_us.last().unwrap()),
            ("imports", metrics.imports as u128),
            ("monitoring_samples", metrics.monitoring_us.len() as u128),
            (
                "monitoring_lag_max_us",
                *metrics.monitoring_us.last().unwrap(),
            ),
            ("import_max_us", *metrics.import_us.last().unwrap()),
            ("in_flight_restarts", metrics.restarts_in_flight as u128),
            ("port_changes", metrics.port_changes as u128),
            ("generation_changes", metrics.generation_changes as u128),
            ("natpmp_requests", metrics.renewals as u128),
            ("protected_feed_successes", metrics.feed_successes as u128),
            (
                "protected_source_requests",
                self.source.requests.load(Ordering::Relaxed) as u128,
            ),
            (
                "catalog_requests",
                self.catalog.requests.load(Ordering::Relaxed) as u128,
            ),
        ] {
            report.insert(key, value.to_string());
        }
    }
}

struct Service {
    port: u16,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Service {
    fn open(
        listener: Box<dyn NetworkListener>,
        catalog: bool,
        feed: Vec<u8>,
        candidate: Vec<u8>,
    ) -> Self {
        let port = listener.port();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, q) = (stop.clone(), requests.clone());
        let worker = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while head.len() < 16384 && !head.ends_with(b"\r\n\r\n") {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head);
                let path = text.split_whitespace().nth(1).unwrap_or("");
                q.fetch_add(1, Ordering::Relaxed);
                let body = if catalog {
                    if path.contains("/season/") {
                        br#"{"episodes":[{"id":1,"season_number":1,"episode_number":1,"name":"Future episode","air_date":"2099-01-01"}]}"#.to_vec()
                    } else {
                        br#"{"id":42,"name":"Synthetic monitored series","first_air_date":"2024-01-01","seasons":[{"season_number":1}]}"#.to_vec()
                    }
                } else if path == "/candidate" {
                    candidate.clone()
                } else {
                    feed.clone()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .and_then(|()| stream.write_all(&body))
                .and_then(|()| stream.flush());
                // Dropping a virtual stream aborts unread bytes. Keep it alive
                // until the HTTP client has consumed the response and closes.
                let _ = stream.read(&mut [0]);
            }
        });
        Self {
            port,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            w.join().unwrap();
        }
    }
}
