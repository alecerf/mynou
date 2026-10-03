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
sha256sum -c mynou-v0.8.0-source.zip.sha256
sha256sum -c mynou-v0.8.0-linux-amd64-image.tar.gz.sha256
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

## Monitoring changes in 0.8.0

The new scope is owned-library current-entry resolution, durable monitoring and
baselines, profile cutoffs, strict quality improvement, deduplicated upgrade
children, unique imports and file-specific Plex confirmation. CI must cover:

- Backward-compatible configuration and journal decoding; background checks
  default to disabled and missing baselines remain ineligible.
- Cutoffs following preference order and current-profile comparisons. Acceptance
  precedes rank: policy changes can replace rejected baselines with accepted
  candidates; accepted baselines require a strict rank improvement. Seed count
  or deterministic tie breaking alone cannot trigger an upgrade.
- Read-only offline listing and previews without directory/file creation,
  permission changes, compaction or tail repair; fresh storage returns no entries
  and incomplete tails require explicit writable recovery.
- Preview passes without journal writes, applied timestamp persistence even on
  search failure, manual passes ignoring polling intervals, background passes
  honoring them, bounded oldest-check scheduling and duplicate-child suppression.
- Explicit baseline setup requiring matching identity, safe present owned files
  and declared video streams without demanding acceptance by today's profile.
- Earlier ready entries remaining current through queued, failed or canceled
  children, including restart and transaction recovery. Pending upgrades block
  unrelated same-media promotion, and ready children inherit their parent's
  current monitoring choice.
- Upgrade filenames preserving earlier imports and downloads, plus new-file Plex
  confirmation with exact paths and longest component-prefix mappings,
  including absolute roots resolved from a relative configuration filename.
- Shared 90-second indexer HTTP/socket and processing budgets with late-result
  rejection; synchronous DNS remains an explicitly documented timing limit.
- CLI/API library, monitor/unmonitor, baseline and preview/apply operations,
  authenticated access, bounded inputs and credential-free reports.

These are validation requirements, not a recorded passing result. Inspect the
Actions run for the exact 0.8.0 source commit. Preserve the existing dependency,
formatting, Clippy, native/Docker demo and release-packaging checks. Do not use a
previous release's test count as evidence for changed monitoring code.

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
