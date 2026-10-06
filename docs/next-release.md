# Active release — 0.20.6 reviewed canonical IRC requests

Continue all roadmap stages autonomously, release by release. A green publication
starts the following scope. Do not stop at a checkpoint. Read AGENTS.md: English,
Rust std only, zero Cargo dependencies, no unsafe/FFI/copied third-party code,
external runtime helpers or local validation. Formatting is an edit. Commit
meaningful chunks, push completed scope, inspect all five Actions jobs and fix
reds through commits. Actions alone creates tags/releases/assets. Complete exact
publication before pushing a successor; workflow cancellation is literal false.

## Exact preceding evidence

Run 37423924045 passed all five jobs for
861f8ae2f0274f82a7351fe1a5ddb890b8391f01: 568 Rust tests across 51 harnesses,
none failed or ignored, four scheduler checks, observed execution 61.795 seconds
with two processes/two threads. CI published v0.20.5 at 06:31:53 UTC on October 6,
2026, release 404389242, with seven bot-owned assets. The exact tag matched and
v0.20.4's tag/asset IDs/sizes/digests remained unchanged. Proof is recorded in
validation.md and /workspace/scratch/mynou-20.5-release-proof.json.

v0.20.4 publication recovered in attempt 4 of run 37362054311 after the queued
job was superseded, hosted-runner allocation failed, and the next retry returned
403 for historical workflow permissions. The source branch release-source/0.20.4
retains 35dec95705bb067dbcd2f149dfd8b050f0f550c1; the existing CI alone published
that source. No release/tag/assets were created manually. Its seven assets and
560-test validation evidence remain recorded separately.

## Active implementation and remaining validation

The 0.20.6 implementation adds a request action requiring an explicit configured
requester. Fresh Plex identity and complete released catalog facts precede all
persistent locks. Episodes retain aired stable IDs and approved canonical/source
numbering without modifying the series plan. The full-scope guard covers the
claim, configuration, account/policy/capture, ledger/jobs, numbering and UTC day.

src/irc/admission.rs writes a checked intent before the canonical requester origin
and calls ordinary approval/quota/sharing admission. The canonical request stays
frozen; approval may recapture an unadmitted policy through existing controls.
Empty watchlist polls retain explicit origins, and removal/rejection tombstones
cannot revive. Startup verifies both stores before native workers; committed
origins complete intents, while intents without origins abort without replay.
Requester format 2 and IRC format 3 reject new semantics under older magic.

Routing accepts a committed request only with its original rule/selected account,
explicit origin and captured source numbering. It retains metadata-only pinned
hash/file/profile/physical ownership gates. Waiting recognizes explicit request
origins; receipt alone neither creates demand nor defers unrelated operator work.
CLI/API/browser use existing protected reviewed controls. Public reports expose
aliases/status, and identity/transport errors remain generic.

Seventeen new original CI scenarios in tests/irc_request_admission.rs cover
scope, pure previews, approval/quotas, sharing, operator conflicts, catalog/account
failures, stale/concurrent/racing reviews, aired identities, retained numbering,
committed/uncommitted crash recovery, corrupted/missing proof, magic downgrades,
protected browser/API/CLI, offline read-only preview and verified native import.
These are implemented and unvalidated until their exact Actions run succeeds.
No local tests, lint, builds, demos or project binary executions are allowed.
Test weights now contain only 51 actual v0.20.5 harness measurements.

## Retain the earlier data and acquisition gates

The strict JSON envelope stays the default. Configurable fixed-delimiter grammar
requires complete explicit catalog/hash fields; unknown, missing, ambiguous or
credential-bearing claims cannot acquire. Source binding includes only configured
grammar/authentication policy and hashes it. Unchanged JSON sources retain their
bindings and fingerprints. Pure CLI/API previews preserve all storage.

Receivers remain opt-in, bounded to eight sources and exact sender/channel trust
after registration/membership. Remote connections use original verified TLS.
Private atomic checked IRC snapshots retain the original first claim and terminal
review across duplicates and restart; duplicate receipts never write the snapshot.
Public reports omit raw server messages, source endpoints, credentials and magnets.

src/irc/routing.rs performs metadata/availability I/O outside persistent locks,
under a shared ten-second deadline. Immutable origins/reservations commit before
native transfer publication; source labels, captured profiles/destinations,
approval/quotas and exclusive physical ownership remain mandatory. Lock order is
IRC, requester ledger, job store. Startup verifies cross-storage provenance before
native transfer startup. Committed reservations recover; uncommitted intents abort.
Retry retains original hash, aliases, file and release. Ready imports are preserved.

## Continue immediately after this green publication

Implement durable outcome/notification routing in 0.20.7. Persist bounded fixed
outcome events before delivery with stable event IDs, immutable private route
bindings, bounded retries/attempts and redacted alias/status reports. Preserve
requester notification preferences and shared ownership. Test original local
HTTP delivery, duplicates, retry, restart and failures before claiming transport.
Delivery acknowledgment stays separate from acquisition. Inspect existing
requester notifications, IRC intents/routing and background scheduling first.

Then continue native indexer adapters, authentication/session renewal, source
configuration and health in 0.21. Continue bounded native Usenet processing,
integrity and recovery in 0.22 with explicit supported formats and zero external
helpers. Continue verified cross-seeding and safe bulk automation in 0.23. Each
scope needs exact CI/source/tag/seven-asset proof and the immediately preceding
release must stay unchanged. Use synthetic media/local services; never claim
broad stack parity or unmeasured performance.

No account quota interface is available. Do not claim quota monitoring or
unattended resumption. Save concrete progress and live CI evidence as work
proceeds and continue naturally across context compaction.
