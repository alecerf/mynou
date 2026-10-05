# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current implementation: 0.19.1, patch awaiting CI

[Plex requester policies](requesters.md) retain per-account identity bindings,
versioned opt-in profiles, approvals and bounded quotas before acquisition.
Compatible canonical demand shares work; captured destination/profile behavior
survives edits and restart. Independent polling reports partial failures without
removing another account's demand. Guarded operator controls expose status,
limits, decisions and recorded notification outcomes. Original synthetic CI
scenarios cover multiple accounts, native acquisition and persistence recovery.

The requester stage passed [run 37303359973](https://github.com/alecerf/mynou/actions/runs/37303359973)
for `2388836a24ac02225a4171aa3d6bbbc32323b6c7`: 489 Rust tests across 43 targets,
four scheduler checks and all five jobs. CI published
[v0.19.0](https://github.com/alecerf/mynou/releases/tag/v0.19.0) with seven assets
on October 5, 2026, at 11:32:55 UTC. The 0.19.1 patch restricts reuse of uncaptured
operator jobs to verified ready imports with compatible destinations and quality;
its own complete CI and publication are pending. Only after all five patch jobs
and publication pass can IRC automation begin.

## Previous stage: 0.18.0

Complete shared-group baselines and replacements are reviewed through
CLI/API/browser. Every child retains immutable parent lineage and one new
authenticated video/destination. Exact Plex confirmation stages an owner; the
last required confirmation promotes the complete group in one synchronized frame.
Old imports remain current throughout partial work, failures and cancellation.
Whole-group retry, monitoring fences, stale guards and format 3 recovery retain
captured ownership and earlier bytes. See [group upgrades](group-upgrades.md).

This stage passed [run 37293887224](https://github.com/alecerf/mynou/actions/runs/37293887224)
for commit `f1a9733a5908c3fa8bc5e93b5d18800334fad5d1`: 467 Rust tests across 40 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.18.0](https://github.com/alecerf/mynou/releases/tag/v0.18.0) with seven assets
on October 5, 2026, at 10:04:19 UTC. See
[validation evidence](validation.md#recorded-0180-ci-evidence). No local validation
ran. The next stage is Plex requester policies in 0.19; later commits need their
own complete CI.

## Previous stage: 0.17.0

Explicit shared-file preview/apply binds one authenticated video to 2–64
consecutive canonical owners in one season. All owners commit in one synchronized
journal frame before acquisition and import one deterministic Plex range file.
Each owner confirms that exact path; cancellation, retry and later subsets retain
the full binding. Individual remaps, baselines and upgrades are blocked for shared
owners; coordinated group replacement follows in 0.18. See
[shared files](shared-files.md).

This stage passed [run 37271644993](https://github.com/alecerf/mynou/actions/runs/37271644993)
for commit `89e5aefa66026da084a6770fb76d51e7396602ec`: 450 Rust tests across 38 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.17.0](https://github.com/alecerf/mynou/releases/tag/v0.17.0) with seven assets
on October 5, 2026, at 06:19:26 UTC. See
[validation evidence](validation.md#recorded-0170-ci-evidence). No local validation
ran. Later commits require their own complete CI.

## Previous stage: 0.16.0

Explicit numbering choices retain canonical episode identities while approving
changed catalog numbers or alternate/absolute source labels. CLI/API/browser
preview and guarded apply persist accepted choices before future jobs capture
them. Existing library paths and request keys remain fixed; identity history
survives disappearance and restart. See [numbering](numbering.md).

This stage passed [run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691)
for commit `0821a4d3b499a5863fe5b50206c98bda25d6fb49`: 430 Rust tests across 36 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.16.0](https://github.com/alecerf/mynou/releases/tag/v0.16.0) with seven assets
on October 4, 2026, at 21:59 UTC. See
[validation evidence](validation.md#recorded-0160-ci-evidence).
Numbering and shared physical ownership were split into independently validated
releases; shared multi-episode imports followed in 0.17.

## Previous stage: 0.15.0

Automatic season-pack search assesses titles under the episode profile, resolves
bounded authenticated metadata and maps unique numbered video paths to every
eligible missing aired catalog episode. CLI/API/browser previews request no
payload and record no jobs. Guarded acquisition binds catalog/request scope,
candidate, torrent hash and exact paths; immutable origin provenance reaches
ordinary mapped jobs before workers begin. Optional `series_packs.enabled`
prefers packs during monitored tracking and refresh with individual fallback
inside the shared deadline and combined 64-job batch. Earlier configurations
default to individual acquisition. See [automatic packs](automatic-packs.md).

The automatic stage passed
[Actions run 37230875486](https://github.com/alecerf/mynou/actions/runs/37230875486)
for commit `1fe40eed0b0ea170a03ffce8d30d2ab8cb3e7125`: 409 tests passed with
none failed or ignored across 34 targets and the complete validation pipeline.
GitHub Actions published
[v0.15.0](https://github.com/alecerf/mynou/releases/tag/v0.15.0) with seven assets
on October 4, 2026, at 20:12 UTC. The next stage was split into explicit numbering in 0.16 and shared physical
multi-episode import ownership in 0.17. See
[validation evidence](validation.md#recorded-0150-ci-evidence) for asset digests.

## Previous stage: 0.14.0

Selective native acquisition retains the union of mapped pack file interests,
verifies required boundary pieces and selected v2/hybrid roots, then permits
mapped imports independently of full-torrent readiness. Selection expansion,
restart verification and existing pause/rate/counter controls retain safe shared
ownership. Partial torrents advertise no payload and never announce completion.
The CLI, API and browser provide additive file selection and full acquisition.
See [transfer selection](transfers.md#selective-acquisition-in-0140).

The selective stage passed
[Actions run 37221887812](https://github.com/alecerf/mynou/actions/runs/37221887812)
for commit `a077af8d660a2b5ca12e47b579562e7a9292f6e2`: 385 tests passed with
none failed or ignored across 31 targets and the complete validation pipeline.
GitHub Actions published
[v0.14.0](https://github.com/alecerf/mynou/releases/tag/v0.14.0) with seven assets.

This result validates the selective stage. Automatic pack choice follows in
0.15; numbering rules and multi-episode physical files need their own releases.

## Previous stage: 0.13.0

Explicit pack acquisition maps exact torrent video paths to already aired catalog
episodes. Ordinary jobs share the native torrent identity, verify the full
payload and import only their retained mapped files. New series scopes can be
created without automatic acquisition before choosing a pack. Input, catalog,
source-key and capacity checks precede recording; workers cannot change mappings and
existing episode jobs are reused. See [pack acquisition](packs.md) for operations
and bounds. A guarded correction can requeue a failed/cancelled mapped request
without imports or an active lease.

The pack stage passed
[Actions run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435)
for commit `cb6e89700a63c1a7f9aaaa644fce32bf8944386f`: 374 tests passed with
none failed or ignored. GitHub Actions published
[v0.13.0](https://github.com/alecerf/mynou/releases/tag/v0.13.0) with seven assets.

This stage supplies explicit mappings for absolute/anime-style filenames.
Automatic pack selection and general numbering rules remain later work.
Selective downloading is implemented in the following 0.14 stage.
The [next-release checkpoint](next-release.md) records later numbering and
physical-import prerequisites.

## Previous stage: 0.12.0

Durable series scopes retain TMDB episode plans and monitoring settings.
Background checks acquire newly aired missing episodes, respecting known
catalog identities, earliest monitored dates, exclusions and optional specials.
Future and unresolved episodes stay visible. The CLI, Bearer API and browser
provide series controls and a bounded episode calendar. Settings revisions
reject late refresh results, verified snapshots survive restart and existing
request identities deduplicate overlapping scopes. See [series monitoring](series.md)
for exact limits.

The series stage passed
[Actions run 37210722790](https://github.com/alecerf/mynou/actions/runs/37210722790)
for commit `fbdf61c19b31f941a08f91fd19e3bb843aec0541`: 364 tests passed with
none failed or ignored. GitHub Actions published
[v0.12.0](https://github.com/alecerf/mynou/releases/tag/v0.12.0) with seven assets.
That result does not validate the following pack changes.

## Previous stage: 0.11.0

Browser management adds original Rust-rendered HTML/CSS and native forms on
`/ui`: sessions, jobs/history, search/request submission, owned-library
monitoring/upgrades, native transfer controls and bounded bulk changes.
Each bulk entry reports its own result. No browser framework or JavaScript
dependency is required. See [web management](web.md) for authentication,
deployment and exact limits.

The browser stage passed
[Actions run 37206645776](https://github.com/alecerf/mynou/actions/runs/37206645776)
for commit `d7cb8eb20d364c217ed89ab183ebf754512d7dfe`: 343 tests passed with
none failed or ignored. GitHub Actions published
[v0.11.0](https://github.com/alecerf/mynou/releases/tag/v0.11.0) with seven assets.
That result does not validate the following series changes.

## Previous stage: 0.10.0

Bounded parallel TCP peers cooperate on verified torrent pieces. The
`downloads.max_peers` setting defaults to four and accepts one through eight;
the effective worker count can be lower to retain global and per-transfer
resource bounds. Set it to one for a single-peer baseline. Discovery runs
alongside usable known peers, respecting private-torrent restrictions. See
[transfer controls](transfers.md) for the exact bounds and remaining limits.

The parallel-peer stage passed
[Actions run 37203872630](https://github.com/alecerf/mynou/actions/runs/37203872630)
for commit `5b0b202b78bc906db914ef4713c57ec14807169f`: 321 tests passed with
none ignored. GitHub Actions published
[v0.10.0](https://github.com/alecerf/mynou/releases/tag/v0.10.0) with seven assets.
That result validates the parallel-peer release. Later changes require their own
completed CI run and release before claiming success or new measurements.

## Previous stage: 0.9.0

Native transfer controls add durable pause/resume, priority/FIFO scheduling,
per-file piece priority, global payload bandwidth limits, persistent counters
and ratio/time seeding policies. Earlier configuration files keep unlimited
rates and seeding until those limits are configured. Controls retain downloaded
sources and library imports. Per-file priorities change download order; they do
not skip files. See [transfer controls](transfers.md).

Full parity with Radarr, Sonarr, Pulsarr, qBittorrent, qui,
autobrr or Prowlarr remain future stages.

## Planned stages

The following sequence is provisional. A release may be split when its scope
needs separate validation. No delivery dates or performance improvements are
promised before implementation and measurement.

| Planned release | Scope | Evidence required before continuing |
| --- | --- | --- |
| 0.19 | Plex users, approvals, quotas, routing and notification preferences | User ownership and limits remain enforced across polling, retries and restart |
| 0.20 | IRC announcements, immediate grabs, filters, action routing and notifications | Bounded announcement parsing, reconnect/backoff, duplicate suppression and auditable rule decisions |
| 0.21 | Native indexer adapters, login/session management, source health and configuration | Per-adapter protocol fixtures, credential redaction, rate limits and safe session renewal |
| 0.22 | Usenet search/acquisition and management | Native protocol support, bounded message processing, integrity/recovery and explicit format limits without external helpers |
| 0.23 | Cross-seeding and further bulk automation | Verified content identity and safe reuse of existing files; no accidental extra acquisition or library overwrite |

Each stage needs meaningful automated checks and an updated support matrix.
Tests use synthetic content and local peers/services. Compatibility with a
personal installation or public-swarm throughput requires separate observed
evidence; passing local protocol fixtures does not establish either.

## Capabilities still outside the current release

- Advanced torrent transports and networking: uTP, WebTorrent, webseeds,
  automatic NAT traversal and a complete persistent DHT table.
- Broad tracker/provider coverage, changing authentication schemes and a
  comprehensive adapter catalog.
- Mature administration across multiple installations and operating systems.
- Independent review of the original cryptographic and protocol implementation.

These are tracked as explicit limits until implemented. “Zero dependencies”
describes the implementation constraint, not a guarantee of completeness,
security, optimal performance, or compatibility.

[Selection profiles](selection.md) · [Library upgrades](library.md) ·
[Transfer controls](transfers.md) · [Series and calendar](series.md) · [Current limits](limits.md) ·
[CI validation](validation.md)
