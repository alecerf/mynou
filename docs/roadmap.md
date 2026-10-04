# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current stage: 0.13.0

Explicit pack acquisition maps exact torrent video paths to already aired catalog
episodes. Ordinary jobs share the native torrent identity, verify the full
payload and import only their retained mapped files. New series scopes can be
created without automatic acquisition before choosing a pack. Input, catalog,
source-key and capacity checks precede recording; mappings remain immutable and
existing episode jobs are reused. See [pack acquisition](packs.md) for operations
and bounds. Its own completed CI/release are required before claiming validation.

This stage supplies explicit mappings for absolute/anime-style filenames.
Automatic pack selection, selective downloading and general numbering rules
need a following focused release.

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
| 0.14 | Automatic season-pack selection, selective files, multi-episode videos and general alternate/anime numbering | Explicit numbering decisions, boundary-piece verification, safe sharing and restart; unresolved identities remain blocked |
| 0.15 | Plex users, approvals, quotas, routing and notification preferences | User ownership and limits remain enforced across polling, retries and restart |
| 0.16 | IRC announcements, immediate grabs, filters, action routing and notifications | Bounded announcement parsing, reconnect/backoff, duplicate suppression and auditable rule decisions |
| 0.17 | Native indexer adapters, login/session management, source health and configuration | Per-adapter protocol fixtures, credential redaction, rate limits and safe session renewal |
| 0.18 | Usenet search/acquisition and management | Native protocol support, bounded message processing, integrity/recovery and explicit format limits without external helpers |
| 0.19 | Cross-seeding and further bulk automation | Verified content identity and safe reuse of existing files; no accidental extra acquisition or library overwrite |

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
