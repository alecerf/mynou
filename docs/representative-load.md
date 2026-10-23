# Representative load qualification (#342)

Inventory size and concurrently active work are different quantities. This
fixture's 500 retained torrents are a **measurement point**, not a proposed
inventory limit. A user setting of zero meaning no configured queue limit is
a product requirement, not a claim that memory, CPU or peer capacity is infinite.
The [queue controls](transfers.md#peers-and-concurrency) accept zero for both
`downloads.max_active` and `downloads.max_seeding`. Native regressions exercise
concurrent zero-limit downloads and finite seed admission with priorities,
pause, restart and queued-time accounting. The historical run below predates
these controls; the queue follow-up evidence is linked from #342.

## Native mixed-load stage

Build the optimized library harness and pass its executable to:

```sh
cargo test --release --lib --no-run --offline --locked --message-format=json
TMPDIR=/private/tmp python3 .github/scripts/representative_load.py PATH_TO_HARNESS --output OUTPUT_DIRECTORY
```

This opt-in macOS arm64 stage retains 500 valid torrents with real metainfo
and generated payloads. Of these, 492 are 4 KiB and eight are 1 MiB. Each
round has eight remote native clients fetch those eight seeds while the
subject downloads two new 1 MiB torrents from another native seeder. Each
consumer is limited to 128 KiB/s to offer overlapping inbound traffic during
acquisition admission; this is not an unthrottled throughput ceiling. Eight is
the number of consumer torrents offered per round, not a measured guarantee
that all eight transfer simultaneously. The report separately records the
maximum sampled number of consumer download workers running together. All
peer traffic crosses two native WireGuard owners over loopback UDP. The
subject keeps `max_active=2`, `max_peers=4` and the default client tunnel
limits; the responder has extra listeners/flows for the fixture clients.
The payload distribution is deliberately small and synthetic, not a copy
of the private deployment's media distribution.

The observation window is 600 seconds, checked between complete rounds.
The subject is restarted between rounds halfway through the window. The
next round must seed through its replacement listener, while a paused
transfer, its priority and all original payloads must survive. Every received
payload is compared byte for byte; the concurrent peer phase of each round
must complete in 60 seconds (source preparation is outside that deadline).
This timeout is a fixture liveness check, not an agreed production service
level. The setup and final verification can extend total process time.

Use `--queue-mode finite` to exercise no configured download limit and an
eight-seed queue (`max_active=0`, `max_seeding=8`); the eight offered seeds get
higher priority than the cold retained inventory before the timed phase.
`--queue-mode unlimited` uses zero for both queues. The default mode preserves
the historical 2/0 workload for comparison. The runner verifies that the
reported queue settings match the requested mode. The manual workflow defaults
to `finite` and exposes the same choice.

`--smoke` uses 16 retained torrents and a two-second window. It checks the
fixture only. Ordinary tests skip the fixture unless its environment
variables are present. The manual `Representative torrent load` workflow
preserves the results for its exact checked-out commit.

Artifacts contain the source commit, dirty status, harness SHA-256, toolchain,
finite workload, raw log, BSD process timing, JSON measurements and a summary.
CPU, RSS and sampled descriptor counts include both tunnel owners and every
fixture peer in the same process. The fields named `page_p95_us` and `page_max_us` time the combined probe:
the native client's first page, descriptor sampling and acquisition of
consumer-worker locks. These may wait for durable writes. They measure
neither an isolated listing request nor the HTTP API. Uploaded/downloaded payload totals
count fully verified rounds, not protocol overhead or an estimate from files
merely marked ready. Each round adds two retained transfers. Restart timing
includes reopening and verifying the eight active seeds, not a full-inventory
rehash. File descriptors are sampled at page calls, not continuously.

## Qualification scope and owner acceptance

The historical Client stage alone does not qualify the full Engine. On
2026-10-23, the owner explicitly selected the finite full Engine extension
below as sufficient qualification for [#342](https://github.com/alecerf/mynou/issues/342):
500 retained torrents, eight offered seed consumers, two acquisitions per
round, independent 0/8 queues, at least ten minutes of mixed work, actual HTTP
measurements, automation, monitoring and in-flight recovery. The issue may
close after a successful exact-commit run, published evidence and the merged
implementation with required checks satisfied. See
[PR #355](https://github.com/alecerf/mynou/pull/355) for the evidence.

That owner decision supersedes a private-deployment distribution and a
separate numerical production resource budget as closure requirements for
#342. The finite observation point remains distinct from a product capacity
ceiling. Fixture deadlines remain test stop conditions, not service-level
guarantees. Small payloads, controlled peers and whole-process measurements
do not establish large-media, unthrottled or live-swarm throughput, fairness
under hostile/stalled peers, or isolated Engine resource cost.

The private migration manifest, provider/source operation, released binary,
unattended deployed behavior and authorized cutover still belong to
[#344](https://github.com/alecerf/mynou/issues/344). Parent #335 stays open
until that installation gate succeeds. No failed or incomplete benchmark
attempt may be counted as accepted evidence.

## Recorded finite run (2026-10-23)

[Machine-readable evidence](representative-load-results.json) is from clean
commit [`43fa485`](https://github.com/alecerf/mynou/commit/43fa485), an optimized
Rust 1.99.0 library test harness on native macOS arm64.

| Observation | Result |
| --- | ---: |
| Retained torrents, initial / final | 500 / 528 |
| Verified rounds | 14 |
| Window at last completed round | 603 s |
| Phase including final verification / whole process | 653.37 / 741.43 s |
| Verified payload served / acquired | 112 / 28 MiB |
| Maximum sampled consumers running / descriptors | 8 / 45 |
| Restart plus eight active seed rehashes | 7.11 s |
| Phase CPU / whole-process CPU | 250.98 / 278.07 s |
| Whole-process peak RSS | 21.88 MiB |
| Combined status/diagnostic probe p95 / max | 2002.93 / 3309.08 ms |

The pause, priority and all original payload checks passed after restart.
The probe timings include diagnostic lock waits and cannot establish API
responsiveness. No owner-approved production CPU/RSS/latency budget was
asserted by this experiment. The result completes this finite client-stage
experiment and leaves the remaining qualification listed above open.

## Full Engine extension

Add `--engine --queue-mode finite` to run the same retained inventory through
an actual `Engine`, its production workers and a local authenticated HTTP API.
The manual workflow exposes this switch. Without it, the historical native
client stage remains available.

The extension uses a synthetic protected Torznab source served inside the
loopback tunnel, an ordinary loopback catalog, a synthetic two-second NAT-PMP
gateway lease, and generated media. The source publishes one distinct torrent
identity over an actively admitted retained seed. The automatic discovery worker must
register it exactly once. After traffic ends, the fixture raises its priority
to verify its shared bytes within the finite seed queue. Each round uses a fresh local media source and submits a movie and episode
through HTTP, repeats both submissions to check deduplication, waits for the
workers to finish imports, and compares their bytes. Imports receive a release baseline at the configured
quality cutoff and are monitored by the production library worker. The report
records the delay until their first monitor check is observed (an upper bound
including probe spacing). It subscribes to a future
series episode and explicitly refreshes the catalog. Production feed,
cross-seed, library and series workers run alongside transfers.

The subject's download rate is limited to 64 KiB/s. Later rounds start
automation as soon as verified acquisition progress is observed, allowing it
to overlap slower incoming consumers. During the first three
rounds, respectively, it restarts the populated Engine, changes the granted
NAT-PMP port, and replaces the tunnel generation. These three rounds wait
for incoming consumers to finish before changing their listener; this ordering
is not imposed on later rounds. Each incident must start
while a download worker has verified part of an unfinished acquisition. Old generations are retired and dropped
before reopening durable state. The following rounds offer fresh peers on the
current listener; existing remote peers are not expected to discover a port
change without an announce. Completed payloads, pauses, priorities, selected
file interests and cross-seed provenance must survive. This uses a one-file
selection; pack selection remains covered by its dedicated fixtures.

The API latency samples time actual authenticated HTTP GET requests for
transfers, jobs, library and monitored series, independently of the historical
combined status/descriptor/worker-lock probe. Import latency includes HTTP
submission and the wait for the worker's ready state. CPU, RSS and descriptor
measurements still include every fixture service and peer. The extension
allows 120 seconds for each finite round; this is a test liveness deadline,
not an approved production response budget. Smoke mode checks one round and
therefore does not qualify all three incidents.

The owner accepted this finite synthetic experiment for #342. It does not
establish a private workload distribution, a production resource guarantee,
provider behavior or the final deployed installation gate #344. Record a
successful exact-commit result and required checks before closing #342.
