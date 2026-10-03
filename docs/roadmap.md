# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current stage: 0.7.0

Release selection profiles add resolution, source, codec, language, title-term
and custom-score policies for automatic movie and episode acquisition. A CLI/API
preview explains the decision without submitting a job. Existing configurations
retain unrestricted selection until a policy is configured.

This stage does not add automatic upgrades or establish equivalence with
Radarr, Sonarr, Pulsarr, qBittorrent, qui, autobrr, or Prowlarr. Its CI status and
release publication must be verified in GitHub; the source version alone is
not evidence that a release has passed.

## Planned stages

The following sequence is provisional. A release may be split when its scope
needs separate validation. No delivery dates or performance improvements are
promised before implementation and measurement.

| Planned release | Scope | Evidence required before continuing |
| --- | --- | --- |
| 0.8 | Library records, monitored movies/episodes, quality cutoffs and controlled upgrades | Existing imports are preserved until a replacement is verified; restart, cancellation and duplicate requests cannot cause an unintended replacement |
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

[Selection profiles](selection.md) · [Current limits](limits.md) ·
[CI validation](validation.md)
