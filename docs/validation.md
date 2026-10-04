# CI validation and release verification

The development policy requires **GitHub Actions validation only**. Do not run
local tests or lint. Commit changes, push, inspect the run, and fix any failed
check with a new commit. Proceed once required checks are green.

The workflow is tracked in [`.github/workflows`](../.github/workflows), and run
results are available in [GitHub Actions](https://github.com/alecerf/mynou/actions).
A run is evidence for its exact commit. Historical results do not validate a later
revision.

## Automated checks and releases

CI checks the offline Cargo graph, formatting, warning-free Clippy, tests, release
builds, a local end-to-end demo, and Docker behavior. The Cargo graph must contain
one package with no dependencies. Network tests use synthetic media and local
peers or services; no public torrent acquisition is part of validation.

Release automation depends on successful validation on `trunk`. It reads
`Cargo.toml`, creates the version tag and GitHub release for the validated commit
if that version is new, and publishes:

| Asset | Contents |
| --- | --- |
| `mynou-vVERSION-source.zip` | Full sources, tests, docs, Docker files, synthetic demo, `bin/mynou`, and internal `SHA256SUMS` |
| `mynou-vVERSION-linux-x86_64` | Static Linux x86_64 binary |
| `mynou-vVERSION-linux-amd64-image.tar.gz` | Saved Docker image tagged `mynou:VERSION` |
| `SHA256SUMS` and `.sha256` files | SHA-256 integrity checks for release assets |

CI excludes personal data, secrets, build caches, and old Go sources from the
source archive. Release creation and artifact uploads are not manual development
steps. No Docker Hub repository is needed.

## Verify release assets

Download the assets and checksum files from the same release. For example:

```sh
sha256sum -c mynou-v0.13.0-source.zip.sha256
sha256sum -c mynou-v0.13.0-linux-amd64-image.tar.gz.sha256
```

To verify all downloaded assets together, download `SHA256SUMS` and every listed
asset, then run:

```sh
sha256sum -c SHA256SUMS
```

After extracting the source ZIP, verify its internal manifest from the extracted
project root:

```sh
sha256sum -c SHA256SUMS
```

Checksum verification establishes integrity relative to the downloaded manifest.
It does not replace reviewing the release's commit and successful Actions run.

## Pack changes in 0.13.0

CI-only local-peer journeys exercise two mapped episodes sharing one verified
torrent, canonical imports of absolute-named files, ignored malformed unmapped
video, missing-file failure without fallback, shared cancellation, persistence
through restart and read-only mutation rejection. Other scenarios check traversal
paths, duplicates, unknown/future/undated episodes, worker mapping protection, guarded terminal-job correction, source-key
conflicts across series, complete-input prevalidation, old unmapped jobs,
Bearer/browser protection, escaped labels, credential-free responses and CLI
mapping-file routing. New scopes can be recorded without automatic acquisition.

The recorded run below validates the released pack implementation. No tests,
lint, builds or demos are run locally under the active development policy.

## Recorded 0.13.0 CI evidence

[Run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435) passed
for commit `cb6e89700a63c1a7f9aaaa644fce32bf8944386f`: 374 tests passed with
none failed or ignored across 29 targets, alongside all dependency, formatting,
Clippy, native/musl build, native/Docker demonstration and packaging checks.
GitHub Actions published
[v0.13.0](https://github.com/alecerf/mynou/releases/tag/v0.13.0) with seven assets
on October 4, 2026, at 15:39 UTC. Later documentation or code commits need their
own completed CI run and do not alter this published source/tag.

## Recorded 0.12.0 CI evidence

[Run 37210722790](https://github.com/alecerf/mynou/actions/runs/37210722790) passed
for commit `fbdf61c19b31f941a08f91fd19e3bb843aec0541`: 364 tests passed with
none failed or ignored across 27 targets, alongside the complete dependency,
formatting, Clippy, build, native/Docker demonstration and packaging checks.
GitHub Actions published [v0.12.0](https://github.com/alecerf/mynou/releases/tag/v0.12.0)
with seven assets on October 4, 2026, at 14:55 UTC. That result validates durable
series monitoring and calendar, not the following pack changes.

## Series changes in 0.12.0

New CI-only local-service scenarios cover future/undated/unmapped episodes,
optional specials, earliest-air-date choices, per-episode exclusions, background
newly aired acquisition and visible catalog failures. Persistence checks cover
restart/read-only views, checksum corruption, linked snapshots and bounded
partial batches with overlapping-scope deduplication. Numbering checks reject
duplicate or changed known identities; a gated HTTP response verifies settings
revision protection. Calendar date/range/pagination rules, Bearer API controls,
browser forms/escaping/bulk prevalidation and CLI routing have dedicated journeys.

The completed 0.12.0 run above records these checks. Fixtures use synthetic metadata and local services; they do not
establish personal-installation compatibility.

## Recorded 0.11.0 CI evidence

[Run 37206645776](https://github.com/alecerf/mynou/actions/runs/37206645776)
completed successfully for commit
`d7cb8eb20d364c217ed89ab183ebf754512d7dfe`: 343 tests passed with none failed
or ignored, alongside the full validation and release pipeline. GitHub Actions
published [v0.11.0](https://github.com/alecerf/mynou/releases/tag/v0.11.0)
with seven assets on October 4, 2026, at 13:48 UTC. That result validates the
browser release and does not validate the following series changes.

## Browser changes in 0.11.0

The new browser scope is covered by CI-only unit and native HTTP/form journeys:

- Sign-in challenge and session rotation, expiration/capacity, five-attempt
  challenge removal, logout/restart, HTTPS Secure cookie and host/origin binding.
- Bearer API isolation, same-origin/form-token requirements, cookie ambiguity,
  strict form decoding, unknown/duplicate fields and numeric/size bounds.
- Escaped hostile titles/paths, credential-free errors/history/search reports,
  no script dependency and semantic navigation/labels/table structure.
- Request submission/deduplication, cancel/retry, filters/pagination and mixed
  bulk outcomes with prevalidation before side effects.
- Search previews without journal writes, retained request identity, monitoring,
  one-time baselines, upgrade preview/apply and retained earlier imports.
- Native transfer pause/resume, priority, file choices and policy changes;
  invalid controls preserve prior choices and downloaded metadata.

The completed 0.11.0 run above records these checks. Form-protocol checks do not
establish independent visual/browser accessibility review or personal Plex
installation compatibility. Existing offline dependency, lint, build, demo,
Docker and archive-integrity checks continue to run.

## Recorded 0.10.0 CI evidence

[Run 37203872630](https://github.com/alecerf/mynou/actions/runs/37203872630)
completed successfully for commit
`5b0b202b78bc906db914ef4713c57ec14807169f`: 321 tests passed with none ignored,
alongside the complete dependency, lint, build, native/Docker demo and packaging
checks. GitHub Actions published
[v0.10.0](https://github.com/alecerf/mynou/releases/tag/v0.10.0) with seven assets.
This evidence validates 0.10.0, not later source changes.

The debug test build's delayed local TCP fixture transferred 786,432 payload
bytes with a 100 ms per-block delay: 6,574 ms using the retained sequential path
(`max_peers: 1`) and 1,480 ms using four parallel peers, a 4.441x ratio for that
observed fixture. The baseline is inside 0.10.0; it is not the separately
released 0.9.0 executable. This is not a public-swarm or optimized-release
benchmark, and it does not establish a general speedup.

## Parallel-peer changes in 0.10.0

CI must exercise bounded parallel transfers with synthetic torrents and local
TCP peers, without downloading public content. Validation must preserve the
0.9.0 transfer controls and cover:

- Missing `max_peers` defaults, accepted values one through eight, invalid
  values, a single-peer baseline and effective global/resource worker bounds.
- Exclusive in-flight piece ownership, verified publication, file-priority
  changes for subsequent work, cancellation/restart and reclaiming work after
  a peer disconnects or sends corrupt data.
- Several usable peers contributing without duplicate endgame requests or
  allowing one bad peer to corrupt a ready transfer. v1, v2 and hybrid
  verification requirements remain in force.
- Discovery alongside known peers, bounded results and private-torrent rules;
  tracker/DHT latency must not hold back a usable direct peer unnecessarily.
- Aggregate bandwidth caps and persistent accounting across parallel peers,
  including pause, policy changes and seeding-limit retention.
- A reproducible local throughput comparison between the retained sequential
  path with `max_peers: 1` and parallel peers, recording the environment,
  workload, timing and limitations of the comparison. This baseline is not a
  measurement of the separately released 0.9.0 binary.

The completed run above records these checks for the released 0.10.0 commit.
A local fixture comparison does not establish public-swarm throughput or
performance on a personal installation.

## Transfer-control changes in 0.9.0

The new scope is durable native transfer controls, priority/FIFO scheduling,
per-file piece order, global payload bandwidth limits, persistent counters and
seeding elapsed time, and ratio/time policies. CI must cover:

- Defaults and bounded numeric configuration, with older configurations retaining
  unlimited rates and seeding unless limits are set.
- Durable pause/resume and queue priority across restart, including shared
  requests and cancellation without file deletion.
- Per-file priorities using the listed original metadata indices, rejecting
  padding entries and changing piece order while all files still download;
  boundary pieces remain verified under v1/v2/hybrid constraints.
- Aggregate download/upload payload limits across simultaneous local peers,
  a bounded 16 KiB burst, global caps enforced alongside local rates, and
  bounded scheduling. Local policy objects replace seed defaults in full;
  clearing an override restores configured defaults.
- Persistent payload counters and seeding elapsed time, clean shutdown flushes,
  ratio/time stop decisions using verified non-padding payload size, concurrent
  whole-block upload reservations, online availability elapsed time, and
  retention of downloaded/library files. Earlier records begin missing historical
  counters at zero rather than inventing activity. Abrupt
  crashes may lose the latest unflushed increments; exact crash accounting is
  not a supported guarantee.
- CLI/API transfer controls, authenticated operations, bounded inputs and
  credential-free reports.

The completed run below records these checks for the released 0.9.0 commit.
Preserve the existing dependency, formatting, Clippy, native/Docker demo,
selection and upgrade safety checks, and release packaging when changing the
transfer engine. Earlier passing runs do not validate changed transfer code.

## Recorded 0.9.0 CI evidence

[Run 37187100999](https://github.com/alecerf/mynou/actions/runs/37187100999)
completed successfully for transfer-control commit
`c45e127e3b8587a4f4ace6a2bbc7eab86bf48a66`. Its 302 tests passed with none
ignored, together with dependency, formatting, Clippy, build, demo and release
checks. GitHub Actions published
[v0.9.0](https://github.com/alecerf/mynou/releases/tag/v0.9.0) with seven assets.
This result validates that release, not the 0.10.0 changes or a personal Plex
installation.

## Recorded 0.8.0 CI evidence

[Run 37151554961](https://github.com/alecerf/mynou/actions/runs/37151554961)
completed successfully for monitoring commit
`9ef6f1fc94f2437aa6797e18385c0e81273c5e44`. Its 251 tests passed with none
ignored, together with dependency, formatting, Clippy, build, demo and release
checks. GitHub Actions published
[v0.8.0](https://github.com/alecerf/mynou/releases/tag/v0.8.0) with seven assets.
This result validates that release, not the 0.9.0 changes or a personal Plex
installation.

## Recorded 0.7.0 CI evidence

[Run 37137061361](https://github.com/alecerf/mynou/actions/runs/37137061361)
completed successfully for selection commit
`14686f01c17a88c6b9e45ce9d2672e0e3c66d21f`, and GitHub Actions published
[v0.7.0](https://github.com/alecerf/mynou/releases/tag/v0.7.0).
This result validates that release, not the 0.8.0 changes or a personal Plex
installation.

## Recorded 0.6.1 CI evidence

[Run 37134671116](https://github.com/alecerf/mynou/actions/runs/37134671116)
completed successfully on October 3, 2026, for commit
`2a066c2c3b9aafda47a3fc898872d1c371b5f249`. Its 131 tests passed, as did the
dependency graph, formatting, Clippy, GNU/musl builds, native and Docker demos,
and release packaging. GitHub Actions published
[v0.6.1](https://github.com/alecerf/mynou/releases/tag/v0.6.1).

This is evidence for 0.6.1 only. It does not establish that later changes passed
or that the project has been connected to a personal Plex installation.

## Historical 0.6.0 validation

The following checks succeeded on October 3, 2026, before the CI-only policy was
adopted and after fixing the cancellation/startup race. They are recorded evidence
for **0.6.0**, not a passing result for later versions.

| Check | Historical result |
| --- | --- |
| Offline Cargo graph | One `mynou` package, zero dependencies |
| Formatting and Clippy across all targets with `-D warnings` | Passed |
| Offline test suite | 131 passed, none failed or ignored |
| Media | Ten tests, including demo MP4, sparse MP4 >4 GiB, synthetic formats, and mutations |
| GNU and musl release builds | Passed |
| musl linkage | No ELF interpreter or required dynamic library |
| Final Docker build | Passed; `scratch`, user 1000:1000 |
| Final container without external networking | Local magnet, import, and simulated Plex confirmation reached `ready` |
| Fresh Compose installation | Generated from the image without Rust on the host; health, doctor, and API checked |
| Compose import and restart | Identical imported bytes; same request ID and `ready` state after restart |

The historical static binary was 1,717,088 bytes. Its final image was 1,898,812
bytes with ID:

```text
sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146
```

Layer inspection found exactly two embedded regular files: `/mynou` and
`/etc/ssl/certs/ca-certificates.crt`. No shell, helper program, or shared library
was present. The final demo ran with a read-only root, removed Linux capabilities,
and `--network none`; validation containers were removed afterward.

Tests covered real local peer transfers, v1/v2 magnets, hybrid torrents, Merkle
proofs, restart recovery, unsafe paths, tracker lifecycles, media metadata, and
local-service orchestration. They do not establish deployment on a personal Plex
server or public-torrent throughput.

[Performance measurements](performance.md) and their
[raw results](benchmark-results.json) preserve the historical benchmark. The
[checkpoint](checkpoint.md) describes the implementation and remaining personal
installation configuration.
