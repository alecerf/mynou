# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current stage: 0.8.0

Library records now expose Mynou's current ready imports. Per-entry monitoring,
quality cutoffs and controlled child requests add upgrades under the current
selection profile. Previews are read-only; applying decisions is explicit unless
background monitoring is enabled. Existing configurations keep background
monitoring disabled, and older imports without a recorded baseline remain
ineligible until one is explicitly supplied.

Earlier imports stay current until a replacement is ready. Unique upgrade paths
preserve earlier files and downloads. Plex confirmation checks the new file's
path, with optional mappings between container paths. There is no automatic
cleanup or adoption of unrelated existing Plex files. See the
[library guide](library.md).

The preceding selection stage, 0.7.0, passed
[Actions run 37137061361](https://github.com/alecerf/mynou/actions/runs/37137061361)
and was published as [v0.7.0](https://github.com/alecerf/mynou/releases/tag/v0.7.0).
That result does not validate the 0.8.0 changes. Inspect their own completed CI
run and release before claiming success.

This stage does not establish equivalence with Radarr, Sonarr, Pulsarr,
qBittorrent, qui, autobrr or Prowlarr.

## Planned stages

The following sequence is provisional. A release may be split when its scope
needs separate validation. No delivery dates or performance improvements are
promised before implementation and measurement.

| Planned release | Scope | Evidence required before continuing |
| --- | --- | --- |
| 0.9 | Native torrent queue controls, bandwidth limits, file priorities, persistent counters, ratio/time seeding policies | Enforced limits, durable counters and queue recovery with local peers; unrelated shared requests stay intact |
| 0.10 | Parallel peer transfers and discovery improvements | Corrupt-peer isolation, bounded memory and fairness; measured local throughput compared with the previous release |
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
[Current limits](limits.md) ·
[CI validation](validation.md)
