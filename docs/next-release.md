# Next-release checkpoint — 0.20 IRC announcement automation

The requester implementation passed [run 37303359973](https://github.com/alecerf/mynou/actions/runs/37303359973)
for `2388836a24ac02225a4171aa3d6bbbc32323b6c7` with 489 Rust tests across 43
harnesses, four scheduler checks and all five jobs. CI published
[v0.19.0](https://github.com/alecerf/mynou/releases/tag/v0.19.0) on October 5, 2026,
at 11:32:55 UTC.

The compatibility patch passed [run 37305082540](https://github.com/alecerf/mynou/actions/runs/37305082540)
for `83e6d1a40ac2d5abac0baf355a0011c58e49429a` with **490 Rust tests**, none
failed or ignored, across **43 harnesses**, four scheduler checks and all five
jobs. CI published [v0.19.1](https://github.com/alecerf/mynou/releases/tag/v0.19.1)
with seven `github-actions[bot]` assets on October 5, 2026, at 11:48:49 UTC. Its
tag targets the exact tested source. See [validation evidence](validation.md#recorded-0191-ci-evidence)
for asset digests. The prior v0.19.0 and v0.18.0 tags and asset IDs, sizes and
digests remain unchanged. Later documentation changes need their own complete
workflow. Inspect live `trunk` and all five newest jobs before beginning 0.20.

The requester release adds stable verified account bindings, opt-in policy
revisions, durable approval/quota reservations and immutable profile/destination
captures. Independent account polls retain demand on partial failure. Compatible
canonical requests share acquisition; removals keep other interests, operator
work and imported bytes. CLI/API/browser management uses reviewed scope guards.
The 0.19.1 patch checks completed regular-file imports and recorded quality before
new requester reuse of uncaptured operator jobs; pending work stays uncharged.

Read [requester policies](requesters.md), [numbering](numbering.md),
[shared files](shared-files.md) and [group upgrades](group-upgrades.md) before
changing admission or acquisition. These are individually validated stages;
full Radarr/Sonarr/Pulsarr/qBittorrent/qui/autobrr/Prowlarr parity remains future
work. Existing release tags and assets must remain immutable.

## Preserve the development policy

Read `AGENTS.md` and [CI execution](ci.md). Use Rust std only, with zero Cargo
dependencies of any kind. All source, diagnostics and docs must be English.
Never run local tests, lint, builds, binaries, demonstrations or previews.
`cargo fmt --all` is an allowed edit. Commit meaningful chunks, push completed
work and fix red Actions runs with new commits. Do not begin another release
until the current complete workflow and publication are green. Actions alone
create tags, releases and assets.

Rust 1.99.0 and the pinned Actions were checked against their official latest
releases during the requester stage. Recheck their official release metadata
when the next stage begins; keep caches bound to every compiler input and run
every Cargo harness. Current CI retains parallel test execution, independent
GNU/musl builds, exact compiled-artifact reuse and all publication gates.

Personal Plex/TMDB/source credentials and mount mappings remain deployment
configuration. Synthetic local-service validation does not configure the user's
installation. No available tool exposes the ChatGPT five-hour quota. Finish at
a clean committed checkpoint; do not claim quota monitoring or unattended
resumption after a reset.

## Retain requester admission and ownership

`src/requesters/mod.rs` owns strict bounded identities, policies, captures,
canonical demands and controls. `persistence.rs` writes the checked private
atomic ledger; `engine.rs` verifies startup provenance, performs account I/O
outside storage locks, reserves quotas before job creation and reconciles
interests. Retain the ledger-before-jobs lock order and interruption recovery
without duplicate charges or acquisition. Operator interests and ready files
survive requester removal. Configurations without accounts keep earlier behavior.

`src/store.rs` reads formats 1–4 and keeps requester provenance immutable.
Workers use captured profile definitions and absolute roots after policy edits
and restart. Source numbering and shared/group ownership keep their original
identities and complete-scope promotion. Requester polls currently admit aired
episodes individually. Notification preferences currently record local outcomes;
external transports and requester self-service remain later work.

Requester CLI/API/browser reviews bind identity, policy, complete job/demand
scope, selected profiles/destinations and UTC day. Preserve read-only previews,
strict inputs, redaction, session/CSRF ownership, review expiry and stale-plan
rejection. An announcement must not bypass requester approval or quota gates.

## Implement IRC announcement automation in 0.20

1. Define explicit opt-in IRC sources, environment-bound credentials and bounded
   connection/rule settings. Reuse the original standard-library TLS client with
   certificate/hostname verification. Keep server credentials out of persisted
   records, reports and review forms. No shell scripts or external IRC clients.
2. Build a bounded incremental IRC parser and connection state machine for
   registration, channel membership, PING/PONG and announcements. Authenticate
   the configured source/channel before rule evaluation. Reject malformed or
   oversized messages, bound buffers and worker counts, and use interruptible
   reconnect/backoff and shutdown. Document supported authentication/features.
3. Define original explicit announcement templates and deterministic filters:
   title identity, media kind, release markers, required/blocked terms, source
   and selected profile. Do not infer a canonical episode or shared range from
   ambiguous text. Unsupported or unresolved announcements produce an auditable
   outcome without acquisition.
4. Persist bounded announcement identities and rule outcomes before action.
   Suppress duplicates across reconnects and restart. Bind resolved canonical
   request, source, authenticated metadata/hash and the chosen action; route
   accepted immediate grabs through existing acquisition/admission and ownership
   rules. Define operator versus requester interests explicitly. No duplicate
   torrent, silent approval, extra quota charge or uncaptured destination.
5. Expose source health, rule configuration, bounded history, preview decisions
   and reviewed actions through CLI/API/browser. Record acceptance, rejection,
   duplicate, quota and source-failure outcomes with credential redaction.
   Define notification routing and retry/idempotency before external delivery.
6. Add original local IRC/HTTP/native-peer CI fixtures for fragmented lines,
   registration and PING/PONG, oversized/malformed input, untrusted channels,
   rule conflicts, metadata changes, immediate acquisition, reconnect/backoff,
   duplicate restart recovery, requester approval/quotas, source numbering,
   retained imports and protected management controls. Keep every existing
   harness and publication check.

Split the stage into independently useful releases if bounded ingestion and
review need to ship before immediate grabs or external notification delivery.
Do not expose partial automatic actions before identity, persistence and
ownership rules are complete.

## Release acceptance and handoff

Bump Cargo/lockfile, Compose/deployment examples and release notes once scope is
complete. Preserve the offline graph, formatting, Clippy, all Rust tests,
GNU/musl builds, isolated native/container demonstrations and archive/checksum
checks. After CI publication, record the exact source/run/tag/asset evidence.
Later documentation commits need their own complete workflow and must retain
existing published artifacts. End at a clean validated checkpoint with the next
action recorded.

The following stages target native indexer adapters, Usenet and cross-seeding
on the [roadmap](roadmap.md). Claim only implemented behavior and measured
performance.
