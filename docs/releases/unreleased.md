## Changed: A web interface that looks like a media application

The web interface is redesigned around five views: an overview of what is on
its way, the library of films and series, jobs, transfers and settings. It
follows a dark ink and lemon palette that turns to paper and olive by day, and
shows typographic covers for titles. It is still plain HTML, CSS and JavaScript
embedded in the executable, with no build step, remote asset or framework.
Every wait shows: busy buttons spin, missing data shows a skeleton, searching
or importing jobs show a progress bar, and reduced motion keeps the states
without the movement. The JSON settings editor is gone; optional settings such
as `selection` and `requesters` are saved with `POST /api/settings` (#370).

## Added: Find a title on TMDB and choose a release by hand

**New request** searches the TMDB catalog and shows its posters
(`GET /api/catalog`). Adding a film lets Mynou search the indexers by itself,
and adding a series follows it. From a job, or from any episode of a followed
series, **Choose a release** lists what the indexers offer with why each
release was kept or rejected, and downloads the one you pick (`POST /api/grab`,
`POST /api/series/ID/search` and `/grab`;
[details](/docs/selection.md#choose-a-release-by-hand), #370).

## Added: Add a torrent by hand, and remove a transfer

**Add a torrent** in Transfers takes a magnet link, the URL of a .torrent file
or the file itself (`POST /api/torrents` with `source`, or `torrent` holding
the file in Base64). Mynou matches the release name with the library: a film
or an episode takes it as its chosen release, a season pack fills the missing
episodes of a followed series, and anything else downloads as a plain
transfer. **Remove** stops a transfer and takes it off the list, with its
downloaded files when asked (`POST /api/transfers/ID/remove` with
`{"delete_files": false}`). Requests still waiting on it are cancelled; what is
already in the library stays. A transfer being imported cannot be removed
until the import ends ([details](/docs/transfers.md#add-a-torrent-by-hand)).

## Added: TLS 1.2 for trackers and indexers without TLS 1.3

Mynou now reaches HTTPS servers that offer only TLS 1.2: ECDHE with X25519 or
P-256, AES-128-GCM or ChaCha20-Poly1305, secure renegotiation required and the
extended master secret whenever the server offers it. There is no session
resumption or renegotiation, a TLS 1.3 server's downgrade sentinel is refused,
and the certificate and host name are validated as with TLS 1.3
([details](/docs/limits.md)).

## Removed: The access token

Mynou no longer asks for an access token: the web interface opens without
signing in. The API answers only this machine: `listen` must be a loopback address,
and a request must name it in its `Host` and come from no other site's page.
A settings document whose `listen` is not loopback is refused.

## Removed: The management commands

The web interface and the API replace the command line. `mynou` keeps `serve`,
`service`, `update`, `version` and `help`; `submit`, `jobs`, `torrents`,
`settings`, `secrets`, `vpn`, `search`, the series, library, indexer, IRC and
requester commands, `analyze`, `doctor`, `healthcheck` and `demo` are gone.

## Removed: The TMDB API key

The TMDB catalog uses its read access token only (`MYNOU_TMDB_TOKEN`); the
`catalog.api_key_env` setting and `MYNOU_TMDB_API_KEY` are no longer read.

## Changed: Settings with unknown fields load

A settings document with fields this version does not know, written by an older
or newer Mynou, now loads and ignores them instead of being refused.

## Changed: WireGuard profiles with IPv6 are accepted

A provider profile may add an IPv6 address, IPv6 resolvers and `::/0` to its
IPv4 settings. They are kept in the stored profile, but the tunnel still
carries IPv4 only: those entries are not used, and no IPv6 traffic falls back
to the host network
([details](/docs/wireguard-profiles.md)).

## Changed: Faster downloads through the VPN

Torrents through the WireGuard VPN download about ten times faster. On a busy
swarm over Wi-Fi, the steady rate went from about 2 MiB/s to 29 to 35 MiB/s, at
a quarter of the processor time per gigabyte. The VPN's TCP now negotiates window
scaling and selective acknowledgment, keeps out-of-order data for a whole
512 KiB window and acknowledges in batches; the tunnel reads ahead instead of
asking for each message; torrent requests leave in one packet instead of
three; and each peer is asked for its next piece before the current one ends,
instead of waiting a round trip between pieces. This last change also helps
without the VPN. A new download through the VPN also finds its first peers in seconds:
DHT queries no longer fail while the tunnel is busy, and an empty lookup is
retried after 10 seconds instead of a minute
([details](/docs/netstack-tcp.md#tcp-option-decision-and-measured-limits)).
A download now dials up to 50 peers by default instead of 4:
`downloads.max_peers` accepts 1 to 256, set in Settings as **Peers per
download**, and active downloads share at most 256 workers. Seeding counters
are saved every 30 seconds instead of every second.

## Fixed: Port forwarding brings peers to downloads

With VPN port forwarding, peers that connected to Mynou while a torrent was
still downloading were left idle until they gave up: they were only served once
the torrent was complete. They now join the download like the peers Mynou dials,
ahead of new outgoing attempts. Peers whose systems offer ECN when connecting,
the default on macOS and many Linux servers, were also refused by the VPN's TCP;
they are now accepted without ECN.
