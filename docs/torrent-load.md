# Torrent engine load evidence

This is the record of [#320](https://github.com/alecerf/mynou/issues/320), the
engine-level part of the "Resources and claims" gate of the
[WireGuard design](wireguard-design.md#verification-gates-before-protected-delivery),
after the [tunnel owner load](tunnel-load.md) of #313. It measures CPU, memory,
timing and throughput of the real native torrent client as the number of
torrents and peers grows, with all of its peer traffic on the
[tunnel owner's](tunnel-http.md) `Network`, and checks that the explicit
engine and flow limits hold on native macOS arm64. It does not complete
[#224](https://github.com/alecerf/mynou/issues/224) or activate torrent VPN
protection.

## Reproduce

The `Torrent engine load evidence` workflow runs on `macos-26` (arm64) when the
torrent engine, the tunnel, the virtual stack or the harness changes, and can be
dispatched manually. It uses Rust 1.99.1, a release library test harness and no
Cargo dependencies. Locally, compile with:

```sh
cargo test --release --lib --no-run --offline --locked --message-format=json
```

and pass the library test executable to:

```sh
TMPDIR=/private/tmp python3 .github/scripts/torrent_load.py PATH_TO_HARNESS --output OUTPUT_DIRECTORY
```

`--smoke` moves 256 KiB per torrent and only checks the harness. The artifact,
named `torrent-load-COMMIT_SHA`, holds the environment and commit, JSON results,
raw test output, BSD `time -l` output and a Markdown table. Actions artifacts
are kept 30 days; the tables of the accepted run are posted on #320.

## Method

Each level runs in its own process (`torrent_load_report` in
`src/torrent/load_tests.rs`). Two tunnel owners start in that process over
loopback WireGuard UDP: the client with the default `TunnelConfig` limits
except the flow limit of the level, and a responder with larger flow, backlog
and buffer limits. On the responder network, several **native seeding
`Client`s** listen on virtual ports and each holds every torrent, verified from
disk before the measurement starts. A **native downloading `Client`** with the
level's `max_active` and `max_peers` is opened on the client tunnel network and
given one magnet per torrent, naming every seed by its virtual address (no
trackers, DHT or PEX). It fetches the metadata from a peer, downloads 4 MiB of
distinct synthetic payload per torrent in 256 KiB pieces, verifies and writes
every piece, and keeps its durable transfer state as in production. After the
run every downloaded file is compared with its source.

The downloader's network is wrapped by a test observer that counts connect
attempts and refusals and the open peer flows, in total and per torrent (from
the info hash of the outgoing handshake). A sampler reads the engine's job
table every millisecond for the number of running transfers and the moment
each torrent becomes ready.

Transfer time runs from opening the downloader until every torrent is ready.
Transfer CPU and RSS come from `ps` at the start and end of that phase; peak
RSS and total CPU from `/usr/bin/time -l` for the whole process. All of them
include **both** tunnel owners, every seeding client and the harness, so they
are an upper bound for one downloading engine, not its cost alone.

Levels (torrents, seeds, `max_active` / `max_peers`, client flow limit):

- 1, 1, 2 / 4, 64: one transfer;
- 8, 6, 2 / 4, 64: the default engine limits, more torrents than `max_active`
  and more seeds than `max_peers`;
- 16, 4, 4 / 8, 64: a longer queue;
- 16, 4, 16 / 4, 64: the engine's outbound connection budget
  (`parallel::CONNECTION_BUDGET`, 64 = 16 × 4) equals the tunnel flow limit;
- 16, 4, 16 / 4, 16: the same engine demand against 16 tunnel flows.

## Results on a development Mac

One local run on Apple silicon (`d9b5c3f` plus this change), with nothing else
running. The accepted Actions run is posted on #320; numbers vary between
machines and runs.

| Torrents | Seeds | Active / peers | Flow limit | Verified | MiB | Transfer s | MiB/s | Transfer CPU s | Peak RSS MiB | Running max | Open flows max (total / torrent) | Refused connects |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 1 | 1 | 2 / 4 | 64 | 1 | 4 | 0.78 | 5.2 | 0.46 | 12.9 | 1 | 1 / 1 | none |
| 8 | 6 | 2 / 4 | 64 | 8 | 32 | 4.83 | 6.6 | 3.06 | 16.7 | 2 | 8 / 4 | none |
| 16 | 4 | 4 / 8 | 64 | 16 | 64 | 11.61 | 5.5 | 7.68 | 26.5 | 4 | 16 / 4 | none |
| 16 | 4 | 16 / 4 | 64 | 16 | 64 | 9.02 | 7.1 | 7.52 | 44.9 | 16 | 40 / 4 | none |
| 16 | 4 | 16 / 4 | 16 | 16 | 64 | 11.26 | 5.7 | 7.55 | 21.0 | 16 | 16 / 4 | 82 at the flow limit |

In every level all payloads were verified, the running transfers never
exceeded `max_active` and reached it whenever more torrents were queued, and the flows of one torrent never
exceeded its peer limit (the smaller of `max_peers`, 64 / `max_active` and the
number of seeds). With 16 tunnel flows, connects above the table were refused
at once ("Tunnel operation failed"), the open flows stayed at 16, the refused
peers were retried, every torrent completed, and the generation was kept.
Seed setup (verifying 32 to 64 seed copies) took 9 to 13 seconds and is not
part of the transfer time.

Throughput is bounded by durable transfer state, not by the tunnel: every
second the engine saves each transfer whose progress or counters changed,
which on macOS means several full-device flushes (`F_FULLFSYNC`) per transfer,
and the in-process seeding clients save their upload counters on the same
disk. Samples of the transfer phase show the threads waiting in those flushes
and on the engine's job lock, while process CPU stays well below one core. The
tunnel owner alone moved 64 × 4 MiB at about 90 MiB/s in #313.

A required regression,
`queued_torrents_peer_flows_and_an_exhausted_flow_table_hold_their_limits`,
runs three magnets from three seeds with `max_active` 1, `max_peers` 2 and a
2-flow client table that the test fills first: the engine's connects are
refused explicitly, no transfer fails, and after the flows are released all
payloads complete and verify, with one running transfer at a time, at most two
flows per torrent and the same tunnel generation.

## Defect found and fixed

**The periodic save held the engine lock for every transfer.** Once a second
the manager saved all changed transfers while holding the job table lock, so
all peer workers (which take it to commit each verified piece), inbound
seeding peers and status calls waited for every transfer's durable flushes in
a row. With 16 torrents from 2 seeds, `max_active` 4 and `max_peers` 8, the
transfer took 92.0 s and 12.1 CPU s before the fix and 14.5 s and 4.8 CPU s
after it (same machine, consecutive runs). The manager now saves one transfer
per lock hold; what is saved and when is unchanged, and each transfer is
still saved atomically under the lock as before.

## Limits

- Loopback only; both tunnel ends, every peer and the downloader in one
  process, sharing CPU and disk; no packet loss or added latency.
- Synthetic swarm: native seeds with complete data, peers given in the magnet,
  no trackers, DHT, PEX, choking by remote clients, slow or hostile peers.
  Tracker, DHT and UDP load are not measured.
- Individual runs, not percentiles or throughput guarantees; 4 MiB per
  torrent and at most 16 torrents and 64 MiB per level. No claim is made
  beyond the measured levels, and nothing here is real-swarm or provider
  evidence.
- The workflow writes to the runner's ordinary disk, not the RAM disk of the
  required test job, so durable flushes cost what they cost there.
- Memory over hours of operation, seed rotation and restart under load are
  not measured.

## Delivery evidence for #224

Real loopback host-route loss/restoration, IRC orchestration (#323) and retained
engine pause/selection across session changes (#327) are recorded in
[host-io-observer.md](host-io-observer.md). The final
[delivery evidence](wireguard-acceptance.md) records required CI and the owner's
waiver of final independent acceptance; the [macOS guide](wireguard-macos-migration.md)
documents migration. `complete_isolation_verified` stays conservatively false;
these measured levels are not a universal isolation or resource guarantee.
