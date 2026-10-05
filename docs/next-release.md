# Next-release checkpoint — 0.20.1 approved IRC candidate routing

The 0.20.0 implementation adds opt-in live IRC reception, explicit source trust,
strict bounded framing, original verified TLS, title/profile filters, durable
original claim identities and guarded CLI/API/browser reviews. It records no
acquisition jobs. Run 37335746159 passed for
4da379a4ef48763d8a1035fb6b3cd4acc4a721bc with 512 Rust tests across 46 harnesses,
four scheduler checks and all five jobs. CI published v0.20.0 on October 5, 2026,
at 15:50:41 UTC. Its seven assets and the prior v0.19.1 release remain immutable.

The active 0.20.1 increment implements configured hash-pinned magnets and
metadata-only verification before candidate routing to existing admitted jobs.
Plex availability, requester approval/quotas, captured profiles/destinations,
source numbering and exclusive physical ownership remain mandatory. Checked
reservations precede immutable job origins. Twenty original CI scenarios cover
the full path, metadata gates, races, quotas, imports and recovery. Complete
validation and CI publication remain pending for the active source.

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

persistence.rs reads private checked atomic MYNOUI01/MYNOUI02 snapshots. Identity binds
source, canonical media claim and torrent hash. Original claim/evaluations and
terminal decisions survive repeats and restart. Duplicate receipts never write
the snapshot. Full history rejects new identities while retaining existing ones.
engine.rs provides pure previews and row-scoped audit decisions. Shared browser review
slots bind session, action, record and guard and expire in ten minutes.

## Validate the current increment first

Finish all five Actions jobs for the exact current branch. Fix failures with
new commits, push and inspect the new complete run. Confirm that CI alone
publishes v0.20.1, its tag targets the validated source, and all seven assets
have bot uploaders and recorded digests. Compare v0.20.0 metadata before/after.
Never replace a published tag or asset. Record live validation/checkpoint docs;
their later commit requires its own complete CI.

src/irc/routing.rs owns restricted template parsing, metadata selection,
immutable Origin/Route, the serialized bounded automatic pass and startup
recovery. No metadata I/O holds persistent-store locks. Admission takes
IRC/requester/job locks, rechecks approval/profile/job state and saves the
reservation before the origin transaction. Private magnets persist only in
checked private storage; public reports remove them, fingerprints and bindings.

src/store/irc.rs protects canonical and physical ownership. Origins require
job format 5; reservations require IRC format 2. Retry retains original hash,
aliases, file and release. A committed origin completes on recovery; an
uncommitted reservation aborts without replay. Missing or inconsistent
cross-storage provenance fails before native transfer startup.

Plex jobs keep availability checks before waiting, with negative checks due
after 60 seconds. Routing uses a shared ten-second availability/metadata budget
and sets a selected job's next attempt to zero. Do not bypass captured roots or
turn ready data into a new acquisition.

## Following IRC stages after green publication

Add one independently usable stage at a time: bounded tracker text adapters,
SASL/NickServ authentication, explicit requester/action selectors and reviewed
new-demand actions. Existing admitted-job routing must remain the default.
Ambiguous catalog identities, source ranges, existing ownership and stale
configuration remain unresolved without side effects.

Add durable outcome/notification routing with bounded retries, credential
redaction and idempotent event identities before any external transport.
Later pack/upgrade actions must retain complete canonical/shared ownership,
frozen profiles/destinations and approved interests. Extend original local
IRC/Plex/metadata/payload fixtures for each new contract.

Publish only after complete green CI and record exact source/run/tag/asset
evidence. Later docs need their own complete workflow. The broader roadmap keeps
native indexer adapters, Usenet and cross-seeding as later stages; do not claim
full-stack parity or unmeasured performance.
