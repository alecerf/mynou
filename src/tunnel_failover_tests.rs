//! Several profiles in one runtime (#407): a profile out of attempts hands
//! over to the next, wrapping around, and a list where every profile failed
//! rests with a bounded backoff. Profiles share the one outer socket, so no
//! generation is live during a switch and nothing but WireGuard messages ever
//! leaves it.
use super::*;

/// A loopback VPN server that never answers; it keeps what it was sent.
struct Dead(UdpSocket);

impl Dead {
    fn new() -> Self {
        let socket = UdpSocket::bind(loopback()).unwrap();
        socket.set_nonblocking(true).unwrap();
        Self(socket)
    }

    fn address(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }

    fn received(&self) -> Vec<Vec<u8>> {
        let mut all = Vec::new();
        let mut buffer = [0; 4096];
        while let Ok((length, _)) = self.0.recv_from(&mut buffer) {
            all.push(buffer[..length].to_vec());
        }
        all
    }
}

impl World {
    /// A client whose profiles lead to `endpoints` in order; `None` is the
    /// relay to the server.
    fn with_profiles(
        endpoints: &[Option<SocketAddr>],
        configure: impl Fn(&mut TunnelConfig),
    ) -> Self {
        let clock = Manual(Rc::new(Cell::new(Duration::from_secs(10))));
        let mut relay = Relay::new();
        let server = Tunnel::new(
            server_config(relay.address(0)),
            Stream::new(1_000_000),
            clock.clone(),
        )
        .unwrap();
        relay.server = Some(server.local_addr().unwrap());
        let profiles = endpoints
            .iter()
            .map(|endpoint| {
                let mut config = client_config(endpoint.unwrap_or(relay.address(0)));
                configure(&mut config);
                config
            })
            .collect();
        let client = Tunnel::with_profiles(profiles, Stream::new(10), clock.clone()).unwrap();
        Self {
            clock,
            client,
            server,
            relay,
        }
    }

    /// Advances one second at a time until `done`, at most `limit` times.
    /// While no generation is live, nothing can be sent at all.
    fn until(&mut self, limit: usize, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..limit {
            if done(self) {
                return;
            }
            if self.client.status().state == TunnelState::Reconnecting {
                assert_eq!(self.client.udp_bind(0), Err(TunnelError::Unavailable));
            }
            self.advance(Duration::from_secs(1));
        }
        panic!("not reached: {:?}", self.client.status());
    }
}

fn short(config: &mut TunnelConfig) {
    config.reconnect_initial = Duration::from_secs(1);
    config.reconnect_max = Duration::from_secs(1);
    config.reconnect_attempts = 1;
    config.rest_initial = Duration::from_secs(60);
    config.rest_max = Duration::from_secs(150);
}

/// Every profile failed in a row: no generation until the rest is over.
fn resting(world: &mut World) -> bool {
    let status = world.client.status();
    status.state == TunnelState::Reconnecting && status.reason == Some(Reason::Exhausted)
}

fn hops(switches: &[Switch]) -> Vec<(usize, usize)> {
    switches.iter().map(|s| (s.from, s.to)).collect()
}

#[test]
fn an_unreachable_profile_hands_over_to_the_next_and_the_list_wraps_around() {
    let dead = Dead::new();
    let mut world = World::with_profiles(&[Some(dead.address()), None], short);
    assert_eq!(world.client.status().profile, 0);

    // A packet queued for the dead server waits in its generation and is
    // dropped with it: it never leaves, in clear or to another server.
    let early = world.client.udp_bind(5000).unwrap();
    let service = world.server.udp_bind(7000).unwrap();
    world
        .client
        .udp_send_to(early, (SERVER_IP.into(), 7000), MARKER)
        .unwrap();

    // Nothing answers the first profile: its first generation and its one
    // retry give up, then the second profile takes over.
    world.until(400, |world| world.protected());
    let status = world.client.status();
    assert_eq!(status.profile, 1);
    assert_eq!(status.reason, None);
    let switches = world.client.switches();
    assert_eq!(hops(&switches), [(0, 1)]);
    assert_eq!(switches[0].reason, Reason::GaveUp);
    assert!(switches[0].at > UNIX_EPOCH + Duration::from_secs(1_000_000));
    assert!(switches[0].at < world.clock.wall());
    assert_eq!(world.client.stats().generations, 3);
    let probes = dead.received();
    assert!(!probes.is_empty());
    assert!(probes.iter().all(|d| d.len() == 148 && d[0] == 1));
    assert_eq!(world.client.udp_recv(early), Err(TunnelError::Revoked));
    world.settle();
    assert!(drain(&mut world.server, service).is_empty());

    // Traffic now flows through the second profile, encrypted.
    let socket = world.client.udp_bind(0).unwrap();
    world
        .client
        .udp_send_to(socket, (SERVER_IP.into(), 7000), b"through-the-next")
        .unwrap();
    world.settle();
    assert_eq!(
        drain(&mut world.server, service)[0].payload,
        b"through-the-next"
    );

    // That server goes silent too: the list wraps around to the first
    // profile, and once both failed in a row the runtime rests.
    world.relay.block = true;
    world.until(1_500, resting);
    let status = world.client.status();
    assert_eq!(status.state, TunnelState::Reconnecting);
    assert_eq!(status.profile, 1);
    assert_eq!(
        status.retry_at.unwrap() - world.clock.now(),
        Duration::from_secs(60)
    );
    assert_eq!(world.client.retry_in(), Some(Duration::from_secs(60)));
    assert_eq!(hops(&world.client.switches()), [(0, 1), (1, 0), (0, 1)]);

    // The server answers again: after the rest, the next round finds it.
    world.relay.block = false;
    world.until(120, |world| world.protected());
    let status = world.client.status();
    assert_eq!((status.profile, status.reason), (1, None));
    assert!(dead.received().iter().all(|d| d.len() == 148 && d[0] == 1));
    let seen = &world.relay.seen;
    assert!(seen.iter().all(|(d, _)| matches!(d.first(), Some(1..=4))));
    assert!(
        !seen
            .iter()
            .any(|(d, _)| d.windows(MARKER.len()).any(|w| w == MARKER))
    );

    // A protected session resets the rest: the next one is short again.
    world.relay.block = true;
    world.until(1_500, resting);
    assert_eq!(world.client.retry_in(), Some(Duration::from_secs(60)));
}

#[test]
fn a_list_where_every_profile_failed_rests_longer_each_round_up_to_a_bound() {
    let (first, second) = (Dead::new(), Dead::new());
    let mut world = World::with_profiles(&[Some(first.address()), Some(second.address())], short);
    let mut rests = Vec::new();
    while rests.len() < 4 {
        world.until(600, resting);
        rests.push(world.client.retry_in().unwrap());
        // Each round starts again on the first profile.
        assert_eq!(world.client.status().profile, 0);

        // Nothing at all is sent while resting.
        first.received();
        second.received();
        let generations = world.client.stats().generations;
        for _ in 1..rests.last().unwrap().as_secs() {
            world.advance(Duration::from_secs(1));
        }
        world.relay.quiesce();
        assert!(first.received().is_empty() && second.received().is_empty());
        assert_eq!(world.client.stats().generations, generations);
        assert_eq!(world.client.status().reason, Some(Reason::Exhausted));
        world.advance(Duration::from_secs(1));
        assert_eq!(world.client.stats().generations, generations + 1);
    }
    assert_eq!(rests, [60, 120, 150, 150].map(Duration::from_secs));
    // Two switches a round; the history keeps the last few.
    let switches = world.client.switches();
    assert_eq!(switches.len(), SWITCHES);
    assert!(
        hops(&switches)
            .chunks(2)
            .all(|round| round == [(0, 1), (1, 0)])
    );
    assert!(switches.iter().all(|s| s.reason == Reason::GaveUp));

    // A manual reconnect during a rest starts at once and resets the backoff.
    world.until(600, resting);
    let generations = world.client.stats().generations;
    world.client.reconnect().unwrap();
    assert_eq!(world.client.status().state, TunnelState::Connecting);
    assert_eq!(world.client.stats().generations, generations + 1);
    world.until(600, resting);
    assert_eq!(world.client.retry_in(), Some(Duration::from_secs(60)));
    assert_eq!(world.client.switches().len(), SWITCHES);
    // Only a rest or a block can be left by hand.
    world.advance(Duration::from_secs(60));
    assert_eq!(world.client.reconnect(), Err(TunnelError::Unavailable));
}

#[test]
fn profile_lists_are_validated_before_anything_is_sent() {
    let clock = Manual(Rc::new(Cell::new(Duration::ZERO)));
    let target = SocketAddr::from(([127, 0, 0, 1], 51820));
    let build = |profiles: Vec<TunnelConfig>| {
        Tunnel::with_profiles(profiles, Stream::new(5), clock.clone()).map(|_| ())
    };
    let profile = |change: &dyn Fn(&mut TunnelConfig)| {
        let mut config = client_config(target);
        change(&mut config);
        config
    };
    assert_eq!(build(vec![profile(&|_| {}), profile(&|_| {})]), Ok(()));
    assert_eq!(build(Vec::new()), Err(TunnelError::Config));
    assert_eq!(
        build(vec![profile(&|_| {}); MAX_PROFILES + 1]),
        Err(TunnelError::Config)
    );
    let invalid: [&dyn Fn(&mut TunnelConfig); 4] = [
        &|config| config.bind = "127.0.0.1:1".parse().unwrap(),
        &|config| config.rest_initial = Duration::ZERO,
        &|config| config.rest_max = Duration::from_secs(2 * 3600),
        &|config| config.rest_initial = config.rest_max + Duration::from_secs(1),
    ];
    for change in invalid {
        assert_eq!(
            build(vec![profile(&|_| {}), profile(change)]),
            Err(TunnelError::Config)
        );
    }
    let refused = profile(&|config| {
        config.endpoint = Endpoint::Address("[::1]:51820".parse().unwrap());
    });
    assert!(matches!(
        build(vec![profile(&|_| {}), refused]),
        Err(TunnelError::Endpoint(_))
    ));
}
