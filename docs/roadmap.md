# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current stage: 0.10.0

Bounded parallel TCP peers cooperate on verified torrent pieces. The
`downloads.max_peers` setting defaults to four and accepts one through eight;
the effective worker count can be lower to retain global and per-transfer
resource bounds. Set it to one for a single-peer baseline. Discovery runs
alongside usable known peers, respecting private-torrent restrictions. See
[transfer controls](transfers.md) for the exact bounds and remaining limits.

The preceding transfer-control stage, 0.9.0, passed
[Actions run 37187100999](https://github.com/alecerf/mynou/actions/runs/37187100999)
for commit `c45e127e3b8587a4f4ace6a2bbc7eab86bf48a66`: 302 tests passed with
none ignored. GitHub Actions published
[v0.9.0](https://github.com/alecerf/mynou/releases/tag/v0.9.0) with seven assets.
That result does not validate the 0.10.0 changes. Inspect their own completed CI
run and release before claiming success or reporting throughput measurements.

## Previous stage: 0.9.0

Native transfer controls add durable pause/resume, priority/FIFO scheduling,
per-file piece priority, global payload bandwidth limits, persistent counters
and ratio/time seeding policies. Earlier configuration files keep unlimited
rates and seeding until those limits are configured. Controls retain downloaded
sources and library imports. Per-file priorities change download order; they do
not skip files. See [transfer controls](transfers.md).

A web interface and full parity with Radarr, Sonarr, Pulsarr, qBittorrent, qui,
autobrr or Prowlarr remain future stages.

## Planned stages

The following sequence is provisional. A release may be split when its scope
needs separate validation. No delivery dates or performance improvements are
promised before implementation and measurement.

| Planned release | Scope | Evidence required before continuing |
| --- | --- | --- |
| 0.11 | Web management interface, jobs, library/search views and bulk actions | Authenticated operations, clear errors, accessibility and safe bulk changes; no third-party browser framework dependency |
| 0.12 | Series monitoring, calendar, season packs, specials and alternate/anime numbering | Unambiguous episode mapping, pack file selection and deduplication; unresolved mappings require an explicit decision |
| 0.13 | Plex users, approvals, quotas, routing and notification preferences | User ownership and limits remain enforced across polling, retries and restart |
| 0.14 | IRC announcements, immediate grabs, filters, action routing and notifications | Bounded announcement parsing, reconnect/backoff, duplicate suppression and auditable rule decisions |
| 0.15 | Native indexer adapters, login/session management, source health and configuration | Per-adapter protocol fixtures, credential redaction, rate limits and safe session renewal |
| 0.16 | Usenet search/acquisition and management | Native protocol support, bounded message processing, integrity/recovery and explicit format limits without external helpers |
| 0.17 | Cross-seeding and further bulk automation | Verified content identity and safe reuse of existing files; no accidental extra acquisition or library overwrite |

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
[Transfer controls](transfers.md) · [Current limits](limits.md) ·
[CI validation](validation.md)
