# Tunnel load evidence

This is the record of [#313](https://github.com/alecerf/mynou/issues/313), the
first part of the "Resources and claims" gate of the
[WireGuard design](wireguard-design.md#verification-gates-before-protected-delivery).
It measures CPU, memory and timing as the number of virtual TCP peer flows
grows, through the [tunnel owner](tunnel-http.md) whose `Network` torrent
workers use, and exercises its explicit flow and queue limits on native macOS
arm64. It does not complete [#224](https://github.com/alecerf/mynou/issues/224)
or activate torrent VPN protection.

## Reproduce

The `Tunnel load evidence` workflow runs on `macos-26` (arm64) when the tunnel,
the virtual stack or the harness changes, and can be dispatched manually. It
uses Rust 1.99.0, a release library test harness and no Cargo dependencies.
Locally, compile with:

```sh
cargo test --release --lib --no-run --offline --locked --message-format=json
```

and pass the library test executable to:

```sh
python3 .github/scripts/tunnel_load.py PATH_TO_HARNESS --output OUTPUT_DIRECTORY
```

`--smoke` moves 64 KiB per flow and only checks the harness. The artifact,
named `tunnel-load-COMMIT_SHA`, holds the environment and commit, JSON results,
raw test output, BSD `time -l` output and a Markdown table. Actions artifacts
are kept 30 days; the tables of the accepted run are posted on #313.

## Method

Each level runs in its own process (`tunnel_load_report` in
`src/tunnel_load_tests.rs`). Two tunnel owners start in that process over
loopback WireGuard UDP: the client with the default `TunnelConfig` limits
(64 flows, 4 MiB of buffered bytes over all flows, a 32-command owner queue),
and a responder that is the same implementation with larger flow, backlog
and buffer limits, so that the client side is the one under test. Flows
connect to a virtual listener; each sends 4 MiB of a fixed synthetic pattern
with the torrent wire helpers (`write_all_deadline` with 100 ms write
timeouts), the responder checks every byte and answers, and the flow stays
open until all transfers end. Levels are 1, 8, 32, 64 and 96 flows, the last
one above the flow limit, in two connection modes:

- `concurrent`: one thread per flow, all connecting at once;
- `serial`: connections opened one after another, then all transfers at once.

Timings use monotonic `Instant`. CPU (user plus system) and peak RSS come from
`/usr/bin/time -l` for the whole process: the harness, **both** owners and
every worker thread. They are an upper bound for one client, not its cost
alone. The reported refusals are the error messages of the failed connects.

## Results on a development Mac

One local run on Apple silicon, at the commit that introduced the harness. The
accepted Actions run is posted on #313; numbers vary between machines and runs.

| Connect | Flows | Completed | Refused | MiB moved | Transfer s | MiB/s | CPU s | Peak RSS MiB |
| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| concurrent | 1 | 1 | none | 4 | 0.041 | 98.5 | 0.06 | 9.3 |
| concurrent | 8 | 8 | none | 32 | 0.240 | 133.2 | 0.48 | 11.1 |
| concurrent | 32 | 32 | none | 128 | 0.969 | 132.0 | 2.09 | 14.6 |
| concurrent | 64 | 64 | none | 256 | 2.848 | 89.9 | 5.25 | 27.1 |
| concurrent | 96 | 64 | 32 at the flow limit | 256 | 2.640 | 97.0 | 5.13 | 28.0 |
| serial | 64 | 64 | none | 256 | 2.575 | 99.4 | 5.09 | 27.5 |
| serial | 96 | 64 | 32 at the flow limit | 256 | 2.798 | 91.5 | 5.33 | 28.3 |

Above the limit, exactly 64 flows are admitted and the rest are refused at
once ("Tunnel operation failed"); peak memory stays at the 64-flow level. A
required regression,
`flows_above_the_limit_are_refused_at_once_and_released_flows_are_reusable`,
checks the same with a 4-flow limit: refusal is immediate, the generation is
kept and a released flow can be reused.

## Defects found and fixed

The first runs of this harness found two owner defects above about 32 active
flows, fixed in the same change:

- **Lost and duplicated stream bytes.** A caller whose deadline expired while
  the owner was running its command received a timeout, and the command's
  result was dropped. For a read, the bytes already taken from the virtual
  TCP stream were lost; for a write, the caller resent bytes the stack had
  accepted. 25 of 64 flows delivered corrupted data. The owner now claims a
  command before running it; a caller only gives up on a command that is still
  queued, and a claimed command always returns its result
  (`a_claimed_action_returns_its_result_after_the_caller_deadline`,
  `an_abandoned_queued_action_never_runs`).
- **Busy retry on a full queue.** A full owner queue made stream calls fail
  with `WouldBlock` at once, which the wire helpers retry without pausing. At
  64 flows this used about 430 CPU seconds over 44 seconds of wall time, and
  32 of 64 concurrent connects were refused before the flow limit was reached.
  Connects, lookups, stream reads/writes/flushes and datagram reads now wait
  for queue space, retrying every 2 ms until their deadline
  (`stream_operations_wait_for_queue_space_until_their_deadline`). Status,
  telemetry, listener accepts and datagram sends still fail fast.

## Limits

- Loopback only, both ends in one process, no packet loss or added latency;
  `src/tunnel_tests.rs` covers loss, reordering and duplication without timing.
- Individual runs, not percentiles or throughput guarantees. The owner is a
  single thread: throughput falls above about 32 flows as commands queue.
- Not an engine-level measurement (see [torrent engine load](torrent-load.md)): no torrent metadata, piece verification,
  disk writes, trackers or DHT. Torrent downloads are additionally limited by
  their configured peer and transfer concurrency.
- UDP datagram, DNS and listener load, and memory over hours of operation, are
  not measured.

## Delivery evidence for #224

Engine-level torrent load is recorded in [torrent engine load](torrent-load.md)
(#320). The [delivery record](wireguard-acceptance.md) links final native
validation, scope decisions and the macOS migration guide.
