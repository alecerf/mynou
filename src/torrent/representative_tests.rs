//! Opt-in mixed retained/active load. This reports one finite measured level;
//! it is not a statement about the maximum supported inventory.
use super::*;

fn open(config: DownloadConfig, network: Arc<dyn Network>) -> Client {
    Client::open_with_policy_and_admission(
        config,
        TransferPolicy::default(),
        Arc::new(|_| Ok(true)),
        network,
    )
    .unwrap()
}

fn verify(client: &Client, torrents: &[Torrent], deadline: Instant) {
    // Point checks hydrate cold transfers and may schedule rehash. Walk once,
    // waiting for each item, rather than repeatedly restarting a full scan.
    for (n, torrent) in torrents.iter().enumerate() {
        wait_for(deadline, "mixed-load payload verification", || {
            client.check(&torrent.hash).unwrap().ready
        });
        assert!(same_file(
            &client
                .config
                .data_dir
                .join(&torrent.hash)
                .join(&torrent.name),
            &torrent.file,
        ));
        if (n + 1).is_multiple_of(100) {
            eprintln!(
                "MYNOU_REPRESENTATIVE_VERIFY payloads={}/{}",
                n + 1,
                torrents.len()
            );
        }
    }
}

#[test]
fn representative_load_report() {
    let Ok(seconds) = std::env::var("MYNOU_REPRESENTATIVE_SECONDS") else {
        return;
    };
    let seconds: u64 = seconds.parse().unwrap();
    let retained = variable("MYNOU_REPRESENTATIVE_RETAINED");
    assert!(seconds > 0 && retained >= 8);
    let directory = scratch();
    let pair = Pair::with(|_, server| {
        server.tcp.flows = 256;
        server.tcp.listeners = 16;
        server.tcp.total_buffer = 128 << 20;
        server.tcp.backlog = 128;
    });
    let library = directory.0.join("library");
    let mut inventory = torrents(&library, retained - 8, 4096, BLOCK);
    inventory.extend(torrents_from(&library, 8, 1 << 20, PIECE, retained - 8));
    let download_limit =
        std::env::var("MYNOU_REPRESENTATIVE_DOWNLOAD_LIMIT").map_or(2, |v| v.parse().unwrap());
    let seed_limit =
        std::env::var("MYNOU_REPRESENTATIVE_SEED_LIMIT").map_or(0, |v| v.parse().unwrap());
    let mut config = download_config(&directory.0.join("subject"), true, download_limit, 4);
    config.max_seeding = seed_limit;
    // All retained torrents have valid metainfo and payload, rather than being
    // synthetic index rows or hundreds of empty magnets.
    for torrent in &inventory {
        let base = config.data_dir.join(&torrent.hash);
        fs::create_dir_all(&base).unwrap();
        fs::copy(&torrent.file, base.join(&torrent.name)).unwrap();
    }
    let subject = open(config.clone(), pair.client_network.clone());
    for torrent in &inventory {
        subject
            .ensure(
                library
                    .join(format!("{}.torrent", torrent.name))
                    .to_str()
                    .unwrap(),
            )
            .unwrap();
    }
    verify(
        &subject,
        &inventory,
        Instant::now() + Duration::from_secs(300),
    );
    let paused = &inventory[0].hash;
    subject.pause(paused).unwrap();
    subject.set_priority(paused, 7).unwrap();
    // For a finite seed queue, offer the eight actual traffic targets first.
    // The remaining retained payloads stay queued without losing readiness.
    if seed_limit != 0 {
        assert!(seed_limit >= 8);
        for torrent in &inventory[retained - 8..] {
            subject.set_priority(&torrent.hash, 10).unwrap();
        }
        wait_for(
            Instant::now() + Duration::from_secs(60),
            "finite seed admission",
            || {
                inventory[retained - 8..]
                    .iter()
                    .all(|t| subject.policy.seed_queue.allowed(&t.hash))
            },
        );
        for torrent in &inventory[retained - 8..] {
            subject.check(&torrent.hash).unwrap();
            wait_for(
                Instant::now() + Duration::from_secs(60),
                "initial seed rehash",
                || {
                    subject
                        .jobs
                        .lock()
                        .unwrap()
                        .get(&torrent.hash)
                        .is_some_and(|j| j.status.ready && !j.needs_rehash && !j.running)
                },
            );
        }
    }
    let began = Instant::now();
    let (cpu_before, _) = usage();
    let mut rounds = 0;
    let mut page_times = Vec::new();
    let mut uploaded = 0_u64;
    let mut downloaded = 0_u64;
    let mut max_round_us = 0;
    let mut max_fds = 0;
    let mut max_consumers_running = 0;
    let mut subject = subject;
    let mut restarted = false;
    let mut restart_us = 0;
    while began.elapsed() < Duration::from_secs(seconds) {
        let round = Instant::now();
        let active_seeds = &inventory[retained - 8..];
        let acquisition_dir = directory.0.join(format!("acquisition-{rounds}"));
        let acquisitions =
            torrents_from(&acquisition_dir, 2, 1 << 20, PIECE, retained + rounds * 2);
        let sources = seeds(
            &pair,
            &directory.0.join(format!("source-{rounds}")),
            &acquisition_dir,
            &acquisitions,
            1,
        );
        let consumers: Vec<_> = active_seeds
            .iter()
            .enumerate()
            .map(|(n, torrent)| {
                let cfg = download_config(
                    &directory.0.join(format!("consumer-{rounds}-{n}")),
                    false,
                    1,
                    1,
                );
                // Keep incoming traffic alive while all consumers and the
                // subject's acquisitions are admitted. This is a controlled
                // offered load, not an unthrottled throughput ceiling.
                let client = Client::open_with_policy_and_admission(
                    cfg,
                    TransferPolicy {
                        download_limit_bps: 128 << 10,
                        ..TransferPolicy::default()
                    },
                    Arc::new(|_| Ok(true)),
                    pair.server_network.clone(),
                )
                .unwrap();
                client
                    .ensure(&format!(
                        "magnet:?xt=urn:btih:{}&x.pe=10.7.0.2:{}",
                        torrent.hash,
                        subject.listen_port()
                    ))
                    .unwrap();
                client
            })
            .collect();
        for torrent in &acquisitions {
            subject.ensure(&magnet(&torrent.hash, &sources)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let at = Instant::now();
            let (page, _) = subject.statuses_page(None, 50).unwrap();
            assert!((50.min(retained)..=50).contains(&page.len()));
            max_fds = max_fds.max(fs::read_dir("/dev/fd").unwrap().count());
            max_consumers_running = max_consumers_running.max(
                consumers
                    .iter()
                    .filter(|client| {
                        client
                            .jobs
                            .lock()
                            .unwrap()
                            .values()
                            .any(|job| job.running && !job.status.ready)
                    })
                    .count(),
            );
            page_times.push(at.elapsed().as_micros());
            let outgoing_done = acquisitions
                .iter()
                .all(|t| subject.check(&t.hash).unwrap().ready);
            let incoming_done = consumers
                .iter()
                .zip(active_seeds)
                .all(|(c, t)| c.check(&t.hash).unwrap().ready);
            if outgoing_done && incoming_done {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mixed load made insufficient progress in 60 seconds"
            );
            thread::sleep(Duration::from_millis(100));
        }
        verify(&subject, &acquisitions, deadline);
        for (consumer, torrent) in consumers.iter().zip(active_seeds) {
            verify(consumer, std::slice::from_ref(torrent), deadline);
        }
        uploaded += 8 << 20;
        downloaded += 2 << 20;
        max_round_us = max_round_us.max(round.elapsed().as_micros());
        rounds += 1;
        eprintln!(
            "MYNOU_REPRESENTATIVE_PROGRESS rounds={rounds} elapsed_s={}",
            began.elapsed().as_secs()
        );
        // Restart a populated client halfway through the observation window.
        // The next round exercises inbound reachability on its new listener.
        if !restarted && began.elapsed() >= Duration::from_secs(seconds / 2) {
            drop(consumers);
            drop(sources);
            let at = Instant::now();
            drop(subject);
            subject = open(config.clone(), pair.client_network.clone());
            wait_for(
                Instant::now() + Duration::from_secs(60),
                "restarted seed queue",
                || {
                    active_seeds
                        .iter()
                        .all(|t| subject.policy.seed_queue.allowed(&t.hash))
                },
            );
            // A restored ready flag alone is not proof that the worker
            // finished its restart rehash. Observe each seed after hydration.
            for torrent in active_seeds {
                subject.check(&torrent.hash).unwrap();
                wait_for(
                    Instant::now() + Duration::from_secs(60),
                    "restart seed rehash",
                    || {
                        subject
                            .jobs
                            .lock()
                            .unwrap()
                            .get(&torrent.hash)
                            .is_some_and(|job| {
                                job.status.ready && !job.needs_rehash && !job.running && !job.failed
                            })
                    },
                );
            }
            verify(
                &subject,
                active_seeds,
                Instant::now() + Duration::from_secs(60),
            );
            assert!(subject.controls(paused).unwrap().user_paused);
            assert_eq!(subject.controls(paused).unwrap().priority, 7);
            restart_us = at.elapsed().as_micros();
            restarted = true;
        }
    }
    // Cold queued seeds must not be forced into active verification just to
    // prove preservation. The admitted cohort was rehashed above; compare all
    // retained payloads directly, including the deliberately paused torrent.
    for torrent in &inventory {
        assert!(same_file(
            &config.data_dir.join(&torrent.hash).join(&torrent.name),
            &torrent.file,
        ));
    }
    let (count, _, _) = subject.aggregate_stats().unwrap();
    assert_eq!(count, (retained + rounds * 2) as u64);
    let (cpu_after, rss) = usage();
    page_times.sort_unstable();
    let mut report = Json::object();
    for (name, value) in [
        ("retained_initial", retained as u128),
        ("retained_final", count as u128),
        ("active_seed_torrents", 8),
        ("concurrent_acquisitions", 2),
        ("max_peers", 4),
        ("download_queue_limit", download_limit as u128),
        ("seed_queue_limit", seed_limit as u128),
        ("rounds", rounds as u128),
        ("max_sampled_fds", max_fds as u128),
        (
            "max_sampled_consumers_running",
            max_consumers_running as u128,
        ),
        ("elapsed_us", began.elapsed().as_micros()),
        ("max_round_us", max_round_us),
        ("restart_rehash_us", restart_us),
        ("uploaded_payload_bytes", uploaded as u128),
        ("downloaded_payload_bytes", downloaded as u128),
        ("page_p95_us", page_times[(page_times.len() - 1) * 95 / 100]),
        ("page_max_us", *page_times.last().unwrap()),
        ("rss_after_bytes", rss as u128),
    ] {
        report.insert(name, value.to_string());
    }
    report.insert("cpu_s", format!("{:.2}", cpu_after - cpu_before));
    report.insert("restart_preserved_pause_priority_payload", restarted);
    eprintln!("MYNOU_REPRESENTATIVE_LOAD {}", json::stringify(&report));
}
