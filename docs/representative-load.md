# Representative load qualification (#342)

Inventory size and concurrently active work are different quantities. This
fixture's 500 retained torrents are a **measurement point**, not a proposed
inventory limit. A user setting of zero meaning no configured queue limit is
a product requirement, not a claim that memory, CPU or peer capacity is infinite.
The [queue controls](transfers.md#peers-and-concurrency) accept zero for both
`downloads.max_active` and `downloads.max_seeding`. Native regressions exercise
concurrent zero-limit downloads and finite seed admission with priorities,
pause, restart and queued-time accounting. The recorded run below predates
these controls; it does not qualify their representative-load behavior.

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

## Remaining qualification

This stage must not close #342 alone. It does not yet exercise:

- the owner's anonymized distribution or an accepted resource/response budget;
- continuously overlapping seed traffic/acquisitions for large payloads or
  fairness of slow/stalled torrents, or the new configurable seed/download
  queues at representative scale;
- automatic cross-seeding, protected source polling and library/series
  monitoring in the same engine under this load;
- authenticated HTTP API responsiveness or monitoring lag;
- restart while transfers are in flight, tunnel renewal/loss/recovery or
  NAT-PMP port changes during useful work;
- provider or live-swarm operation, which remains separate from this native
  synthetic experiment and from the final installation gate #344.

A measured no-go should identify a reproducible capacity blocker. A successful
finite run should report only the measured workload, with no unlimited-capacity
or full replacement claim. Preserve #342 and #344 until their remaining
acceptance evidence exists.

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
