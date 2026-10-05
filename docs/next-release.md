# Next-release checkpoint — Complete 0.20.0 CI, then IRC acquisition

The 0.20.0 implementation adds opt-in live IRC reception, explicit source trust,
strict bounded framing, original verified TLS, title/profile filters, durable
original claim identities and guarded CLI/API/browser reviews. It records no
acquisition jobs. Its complete validation and CI publication are pending. Finish
every job of the exact current workflow and fix any red check before advancing.

The prior v0.19.1 requester release passed run 37305082540 for
83e6d1a40ac2d5abac0baf355a0011c58e49429a, with 490 Rust tests across 43 harnesses,
four scheduler checks and all five jobs. CI published it on October 5, 2026, at
11:48:49 UTC. Its tag and assets remain immutable. See
[validation evidence](validation.md#recorded-0191-ci-evidence).

Read AGENTS.md, [IRC contracts](irc.md), [requester policies](requesters.md),
[numbering](numbering.md), [shared files](shared-files.md), [group upgrades](group-upgrades.md)
and [CI execution](ci.md). Keep Rust std only, zero Cargo dependencies, English
text and no local tests/lint/builds/binaries/demos/previews. Formatting is an edit.
Commit meaningful chunks, push completed scope and inspect every Actions job.
CI alone creates tags/releases/assets. The pinned Rust/Actions versions were
checked against official latest releases at the start of this stage. All
synthetic test services/peers remain local. No account quota interface is available;
do not claim automatic quota monitoring or unattended resumption.

## Retain the reception/review contracts

IRC settings and identities belong to src/irc/mod.rs; source bindings do not
store credentials. protocol.rs retains incremental bounded framing, registration
and membership before exact sender/channel delivery. client.rs owns at most
eight opt-in receivers and one shutdown monitor. Credential values stay only in
connection commands; errors expose generic outcomes. TLS retains certificate and
hostname authentication, with deadlines renewed only between complete operations.

persistence.rs uses a private checked atomic MYNOUI01 snapshot. Identity binds
source, canonical media claim and torrent hash. Original claim/evaluations and
terminal decisions survive repeats and restart. Duplicate receipts never write
the snapshot. Full history rejects new identities while retaining existing ones.
engine.rs provides pure previews and row-scoped decisions. Shared browser review
slots bind session, action, record and guard and expire in ten minutes.

## Continue with an independently validated acquisition increment

1. Define explicit opt-in automatic action/routing settings and a configured
   way to resolve announcement hashes to acquisition metadata without persisting
   private URLs or credentials. Metadata-only inspection must authenticate the
   announced torrent hash before any payload transfer.
2. Resolve and verify canonical movie/episode identities using existing catalog
   and retained series/source numbering. Ambiguous, unmatched, conflicting,
   stale or changed metadata produces an auditable outcome without acquisition.
3. Bind source/rule revision, verified metadata/hash, canonical request and
   destination/profile/ownership in a durable action reservation before workers
   begin. Recover interruptions without duplicate jobs or charges.
4. Route requester actions through existing opt-in approvals, quotas and
   immutable captures. Distinguish explicit operator interests. Retain another
   requester's demand, ready bytes, source numbering and complete shared-group
   scope. An IRC event cannot silently approve or rebind acquisition.
5. Expose preview, reviewed apply, automatic action outcomes and notification
   routing through CLI/API/browser. Define idempotent retries and external
   delivery before enabling a transport.
6. Add original local IRC/HTTP/native-peer fixtures covering metadata changes,
   immediate grabs, conflicts, duplicate/restart recovery, approval/quota races,
   routing, protected controls and retained earlier imports.

Publish only after complete green CI and record exact source/run/tag/asset
evidence. Later docs need their own complete workflow. The broader roadmap keeps
native indexer adapters, Usenet and cross-seeding as later stages; do not claim
full-stack parity or unmeasured performance.
