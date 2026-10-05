# Active release — 0.20.4 required NickServ identification

Continue all roadmap stages autonomously, release by release. A green publication
is the transition into the following scope. Do not stop at a release checkpoint.
Read AGENTS.md and retain English, Rust std only, zero Cargo dependencies, no
unsafe/FFI/copied code/external runtime helpers and CI-only validation. Formatting
is an edit. Commit meaningful chunks, push completed scope, inspect all five
Actions jobs, and fix every red run with commits before continuing. GitHub
Actions alone creates tags/releases/assets; published versions stay immutable.

The completed v0.20.3 text-format release passed run 37360688158 for
a2a3c9f1ec9a0cd6af09031b24c246be8614d2c8: 551 Rust tests across 49 harnesses,
none failed or ignored, four scheduler checks and all five jobs. CI published
seven assets at 19:07:13 UTC on October 5, 2026. The preceding v0.20.2 tag and
assets stayed unchanged. Exact [validation evidence](validation.md#recorded-0203-ci-evidence)
and prior release evidence are recorded. This source needs its own workflow.

## Active contract and validation

src/irc/nickserv.rs owns strict settings, bounded transient IDENTIFY credentials
and required exact trusted NOTICE confirmation. protocol.rs gates channel
membership on identification and permanently invalidates failed connections.
client.rs reloads credentials on reconnect, sends identification once, resets
public health on disconnect/shutdown and retains the fixed registration deadline.
SASL and NickServ are mutually exclusive. Configuration without NickServ retains
old source bindings and pending fingerprints. Authentication policy edits require
a new source ID. Credential values never enter bindings, storage or diagnostics.

Original CI-only local fixtures cover strict identity settings, early/forged
confirmation, failures before/after membership, redaction, reconnect/restart,
missing credentials, maximum native CLI commands, shutdown and the nonrenewable
deadline. Inspect every job and publication source before starting the next stage.
See [IRC contracts](irc.md), [requester policies](requesters.md), [numbering](numbering.md),
[shared files](shared-files.md), [group upgrades](group-upgrades.md) and [CI execution](ci.md).

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

Implement explicit requester/action selectors and reviewed new-demand admission.
Existing admitted-job routing remains the default. New actions must call the same
canonical admission, approval, quota and capture machinery rather than silently
creating operator jobs. Ambiguous catalog identity, source numbers, stale reviews
and physical ownership stay unresolved without acquisition. Add original local
fixtures across CLI/API/browser, metadata gates, races and restart.

Then implement durable outcome/notification routing with bounded retries,
credential redaction and idempotent event identities before external delivery.
Preserve complete pack/shared-group ownership and frozen policy/routes. Update
the support matrix and record exact source/run/tag/seven-asset evidence for each
scope. Refresh CI test weights only from a passing run's measured harness times.

Continue native indexer adapters, authentication/session renewal, configuration
and source health in 0.21. Continue bounded native Usenet processing, integrity
and recovery in 0.22, with explicit supported formats and no external helpers.
Continue verified cross-seeding and safe bulk automation in 0.23. Inspect existing
implementations before extending them, use synthetic media/local services and
never claim broad stack parity or unmeasured performance.

No account quota interface is available. Do not claim automatic quota monitoring
or unattended resumption. Save concrete progress and live CI/publication evidence
as work proceeds; continue naturally across context compaction.
