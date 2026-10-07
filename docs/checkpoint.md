# Project checkpoint — proposed Mynou 0.22.14

The active implementation rule is Rust with its standard library alone: no crates,
bundled third-party code, FFI, `unsafe`, external runtime programs, or fallback to
Go. The rewrite replaces SQLite, ffprobe, and the Go torrent engine. Data from
previous versions stays separate and is not migrated implicitly.

All project prose and diagnostics must be English. The active development policy
prohibits local tests, lint, builds, binaries and validation. Make meaningful commits on work branches, use reviewed PRs, inspect
GitHub Actions, and fix failures until required checks pass. See
[the organization resume runbook](../engineering/README.md). GitHub Actions
alone creates release tags and publishes artifacts from validated commits.

## Implemented scope

GitHub Actions published v0.22.13 multi-file memory recovery at
`6085368588c8077af53d15bc4f07d752e670e445`: all five jobs green in run
37648617664, 846 Rust tests across 64 harnesses and seven immutable assets.
Issue #9 comment 6041888395 / PR #10 retain exact review/publication/cleanup proof.

Proposed 0.22.14 in [Issue #13](https://github.com/alecerf/mynou/issues/13) adds
bounded read-only protected-content diagnosis through the library and CLI, with
exact File IDs/global slice indices, whole-file checks and source/path/byte
revalidation. It neither reconstructs parity nor creates ownership, writes or
library admission. Separate Security/QA and Actions still establish delivery.
See [support](par2.md) and [release contract](releases/0.22.14.md).
This document is product context, not live execution or verification state.

GitHub Actions published v0.22.12 at `d0c37f773b45b8948eb8c5b67a55e89f64ddbe10`
with all five jobs green in run 37617472864, 838 Rust tests across 64 harnesses,
four scheduler and 53 organization checks, and seven bot-uploaded assets.
Issue #6 / PR #8 preserve exact Security/QA, publication and merged-branch cleanup
proof. These are historical evidence; no new test success is inferred for proposed source.
Read-only PAR2 core inspection from Issue #2 / PR #4 was published as v0.22.11
at `57b15534158b0eb40d4f13450d51fdf33c7d81bb`.
Engineering bootstrap #1 / PR #3 is complete on trunk at
`afd6fa65f1189d181d8ce70161367309480a0730`.

The published 0.22.10 source adds opt-in stored RAR5 admission to native Usenet
movie and episode jobs. `usenet.downloads.rar` captures immutable decoder bounds
and a separately tagged owner capability. The exact source, format, entry, owner
and output proof use the existing private extraction and current-permission
workflow. RAR extraction descriptors require format 2 and RAR-aware journal
records/snapshots require format 8; checksum-valid downgrades are rejected.
ZIP-only selection identities, plan serialization and format-1/7 storage remain
compatible. Unknown or corrupt private state is rejected before recovery writes.
Outer/inner identity, numbering, profile and size, native media, copied atomic
import, approved requester/destination and exact Plex gates remain required.
This scope passed complete Actions validation and publication as recorded below.
Compressed/solid/split/encrypted RAR, RAR4, PAR2, multi-file packs and Usenet
upgrades remain subsequent bounded increments.

CI published v0.22.10 from `f5c59506d0a8d431d09e9f351e2de2ef71b124f9` in
[run 37510460908](https://github.com/alecerf/mynou/actions/runs/37510460908).
All five jobs passed: 804 Rust tests across 62 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 99.854 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
18:23:45 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.9 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.9 from `e69e2b323a74a0d60678bdd797e34d3fbc28e8be` in
[run 37508514751](https://github.com/alecerf/mynou/actions/runs/37508514751).
All five jobs passed: 791 Rust tests across 62 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 97.770 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
18:08:36 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.8 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.8 from `ca1aac9c215b491ffcba1b98c1fc3136a543dfd6` in
[run 37506471621](https://github.com/alecerf/mynou/actions/runs/37506471621).
All five jobs passed: 773 Rust tests across 61 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 95.048 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
17:52:59 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.7 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.7 from `0c85954628c0e8e32a5e89429446f132b4394aaf` in
[run 37501969608](https://github.com/alecerf/mynou/actions/runs/37501969608).
All five jobs passed: 750 Rust tests across 61 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 89.003 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
17:17:59 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.6 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.6 from `c933f289860ab73615de7c61c4f58edbc2b88bc8` in
[run 37476040909](https://github.com/alecerf/mynou/actions/runs/37476040909).
All five jobs passed: 729 Rust tests across 60 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 93.111 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
14:09:19 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.5 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.5 from `a3c326f9a8234e19077dea83d2804ef0cd611773` in
[run 37470439302](https://github.com/alecerf/mynou/actions/runs/37470439302).
All five jobs passed: 712 Rust tests across 60 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 77.994 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
13:27:07 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.4 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.4 from `93711ac221c97edd0392d8e262e3b3961553561a` in
[run 37465017605](https://github.com/alecerf/mynou/actions/runs/37465017605).
All five jobs passed: 693 Rust tests across 59 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 81.582 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
12:44:29 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.3 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.3 from `e4e77c9ddf142bafd91b17a32bfb7741efb45230` in
[run 37459145003](https://github.com/alecerf/mynou/actions/runs/37459145003).
All five jobs passed: 680 Rust tests across 58 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 80.652 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
11:54:05 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.2 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.2 from `1a4949026a615443fe40767015f97dc90437430d` in
[run 37439687975](https://github.com/alecerf/mynou/actions/runs/37439687975).
All five jobs passed: 663 Rust tests across 57 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 79.506 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
09:01:20 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.1 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.1 from `cbc10d16b108c03a157d7afb6c91034ef8ac4343` in
[run 37436785004](https://github.com/alecerf/mynou/actions/runs/37436785004).
All five jobs passed: 652 Rust tests across 56 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 71.509 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:35:55 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.0 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.22.0 from `9644114e5f1e8c2ed679d57a3a2b18c479ee4ac2` in
[run 37434792979](https://github.com/alecerf/mynou/actions/runs/37434792979).
All five jobs passed: 639 Rust tests across 55 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 64.375 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:17:36 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.1 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.21.1 from `88c52d761eed0ea1fb88ba3d52571047ee862dd7` in
[run 37433196413](https://github.com/alecerf/mynou/actions/runs/37433196413).
All five jobs passed: 628 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 60.185 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:02:35 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.0 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.21.0 from `8edd0e4f91039ba2781008ea5c1cf010ef59781f` in
[run 37431238845](https://github.com/alecerf/mynou/actions/runs/37431238845).
All five jobs passed: 616 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 63.220 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
07:44:52 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.20.7 tag and asset IDs, sizes and digests remained unchanged.

CI published v0.20.7 from 3de105188e855e35b5ae86be012df43842c25148 in run
37429460979: all five jobs, 602 Rust tests across 53 harnesses, four scheduler
checks and 63.950 seconds of execution at two processes/two threads. Seven bot
assets were published at 07:28:39 UTC on October 6, 2026; the tag matched and
v0.20.6's tag/assets remained unchanged. Durable native notification delivery
retains atomic outcomes, stable IDs, bounded leases/retries and guarded controls.

CI published v0.20.6 from 0b7d9c67a666b6929c626a930450f3dccae13d41 in run
37427693256. All five jobs passed with 585 Rust tests across 52 harnesses and four
scheduler checks. Execution took 64.921 seconds at two processes/two threads.
Seven bot assets were published at 07:10:02 UTC on October 6, 2026; the exact tag
matched, and v0.20.5's tag and asset IDs/sizes/digests stayed unchanged. Its reviewed
IRC request scope retains fresh account/catalog confirmation, approvals, quotas,
numbering, explicit origins and checked intent recovery.

CI published the preceding 0.20.5 requester-selector scope from
861f8ae2f0274f82a7351fe1a5ddb890b8391f01 in run 37423924045. All five jobs passed:
568 Rust tests across 51 harnesses, four scheduler checks and CI publication at
06:31:53 UTC on October 6, 2026. Seven assets belong to github-actions[bot]; the
v0.20.4 tag and asset IDs/sizes/digests remained unchanged.

The preceding 0.20.4 increment adds required NickServ identification. Exact trusted
account confirmation precedes JOIN; forged membership, failures and expired
registration cannot bypass it. Original local fixtures cover credentials, trust,
reconnect/restart, redaction, the native CLI and shutdown. Run 37362054311, attempt 4, completed all five jobs for
35dec95705bb067dbcd2f149dfd8b050f0f550c1, retaining its original 560-test,
50-harness validation and four scheduler checks. CI published v0.20.4 at
06:25:00 UTC on October 6, 2026, with seven bot-owned assets. The prior v0.20.3
tag and assets remain unchanged. The prepared 0.20.5 scope follows immediately.

The published 0.20.3 increment adds one configurable fixed-delimiter text grammar,
bounded IRC display formatting and pure CLI/API text previews. Complete explicit
catalog/hash claims enter the existing verified routing path. Old JSON bindings
and fingerprints remain stable. Original local CI fixtures cover controls,
ambiguity, redaction, duplicates, restart and a native verified import journey.
Run 37360688158 passed all five jobs with 551 Rust tests across 49 harnesses
and four scheduler checks. CI published v0.20.3 from
a2a3c9f1ec9a0cd6af09031b24c246be8614d2c8 at 19:07:13 UTC on October 5, 2026.
The preceding v0.20.2 tag and assets remain unchanged.

The published 0.20.2 increment adds opt-in required SASL PLAIN, bounded capability
negotiation, credential encoding and authentication before channel membership.
Failure cannot fall back to an unauthenticated connection; retries authenticate
again. Public source health exposes the mode and transient authentication state.
Original local fixtures cover bounds, failures, fragmented messages, exact chunk
boundaries, restart and deadlines. Complete CI/publication passed with 543 Rust
tests across 48 harnesses, four scheduler checks and all five jobs.

The published 0.20.1 increment adds explicit grab rules, hash-pinned configured
magnets, metadata-only verification and candidate routing to existing admitted
canonical jobs. Approval/quotas, frozen requester profiles/destinations, source
labels and physical file ownership remain mandatory. Durable reservations and
immutable job origins survive cancellation/retry and checked restart recovery.
Its complete CI and CI publication passed with 533 Rust tests across 47 harnesses,
four scheduler checks and all five jobs. Plex availability remains active before
waiting or routing, using the captured destination and a bounded deadline.

The published 0.20.0 implementation adds opt-in IRC reception, verified TLS, bounded
protocol handling, explicit filters, checked durable duplicate suppression and
guarded CLI/API/browser announcement reviews. No review creates download work.
Its complete CI and CI publication passed with 512 Rust tests across 46 harnesses,
four scheduler checks and all five jobs.
See [IRC behavior](irc.md) and [next-release notes](next-release.md).

Media parsers, the torrent engine, persistence, orchestration, integrations,
CLI/API, and Docker deployment are implemented. Rust 1.99.0 is the pinned
build toolchain. The Cargo graph must contain one package, `mynou`, with
`dependencies: []`.

The 0.7.0 change adds named movie/episode selection profiles, suffix-based
resolution/source/codec/language markers, required and blocked token phrases,
additive scores and deterministic ranking. `search` and `POST /api/search`
preview automatic source selection without submitting a job; their public
reports omit acquisition URLs and credentials. Missing `selection` configuration
retains unrestricted behavior. See [selection.md](selection.md).

The 0.8.0 change adds current owned-library records, per-entry monitoring,
explicit baselines, preference-ordered resolution cutoffs and controlled upgrade
children. The earlier ready entry remains current until the child is ready.
Upgrade imports use unique filenames and preserve earlier imports/downloads.
Plex confirmation requires the new file path, with optional path mappings.
Background checks default to disabled; missing older baselines are not inferred.
Preview passes contact indexers without journal writes, while explicit apply
passes persist check times and deduplicated children. Offline listing/previews
open read-only storage without creating files, changing permissions or repairing
journal data; interrupted tails require explicit writable recovery. Pending upgrades block unrelated
same-media promotion, and child promotion inherits the parent's current monitored
choice. Search/pass budgets cover HTTP/socket work and processing; synchronous
DNS may exceed the deadline, with late results rejected. Configured import roots
are absolute even with a relative configuration filename.
See [library.md](library.md).

The 0.9.0 change adds native durable pause/resume, priority/FIFO scheduling,
per-file piece priority, global payload bandwidth caps, persistent transfer
counters and seeding elapsed time, and ratio/time seeding limits. Earlier
configuration files keep unlimited rates and seeding. Global rate caps remain
mandatory and permit bounded 16 KiB bursts; a local policy replaces configured
seed defaults in full. Ratio budgets use verified non-padding payload size;
elapsed counts online seeding availability, including idle time. Older records
start missing historical counters at zero. Controls retain downloads and
library imports. File priorities still download all files; skipping is not
implemented. See [transfers.md](transfers.md).

The 0.10.0 change adds bounded parallel TCP peer transfers. The configured
`downloads.max_peers` defaults to four and accepts one through eight; global
worker and per-transfer resource bounds may reduce the effective count. The
single-peer setting is retained for comparisons. See [transfers.md](transfers.md)
for scheduling, discovery, verification and remaining transport limits.

The 0.11.0 change adds the browser interface on `/ui`: original Rust-rendered
HTML/CSS, expiring in-memory sessions, same-origin/form-token checks, credential
redaction, request/search/library/transfer pages, bounded pagination and bulk
changes with per-entry outcomes. API Bearer authentication remains separate.
See [web.md](web.md) for operations, exact bounds and deployment.

The 0.12.0 change adds durable series scopes, retained catalog plans, background
newly aired episode acquisition, optional specials, earliest monitored dates,
per-episode exclusions and a paginated calendar. CLI/API/browser controls share
revision checks, request-identity deduplication and a private verified snapshot.
Unknown dates or missing episode identities remain unresolved. See [series.md](series.md).

The 0.13.0 change adds explicit catalog-backed pack acquisition, durable
file-to-episode mappings, one shared native torrent identity, exact verified
imports and unmonitored initial series scopes. Full torrents still download.
Existing jobs remain deduplicated, missing selections fail without fallback and
older unmapped jobs keep their ordinary behavior. See [packs.md](packs.md).

The 0.14.0 change adds durable shared native file interests, selective boundary
pieces, v2/hybrid file-root verification, synchronized per-file availability and
additive CLI/API/browser controls. Partial torrents remain distinct from full
readiness, advertise no payload and never announce completion. Existing full
acquisitions retain their policy. CI-only scenarios cover boundaries, metadata,
proofs, padding/empty files, shared cancellation, expansion, pause, corrupted disk
recovery, offline restart, mapping repair and protected controls. Its complete
CI/publication result is recorded below.

The 0.15.0 change adds separate season-pack title assessment under episode
profiles, bounded metadata-only discovery and unique catalog-backed numbered
file mappings. CLI/API/browser previews and guarded apply bind scope, candidate,
torrent identity and exact paths. Immutable job origin provenance is persisted
before acquisition; mutable sources fail identity checks before native queue
publication. Optional monitored pack preference shares deadlines and the 64-job
allowance with individual fallback. Earlier configurations retain individual
acquisition. Pack provenance does not establish episode upgrade baselines.
See [automatic-packs.md](automatic-packs.md).

The 0.16.0 implementation adds durable canonical episode anchors, explicit
catalog/source numbering choices, read-only comparisons and guarded CLI/API/browser
apply. New jobs capture source labels while existing keys/imports remain fixed.
Older snapshots read without migration writes; successful saves use schema 2.
The complete CI/publication passed with 430 Rust tests across 36 targets and four
scheduler checks. The exact evidence is recorded below.
See [numbering](numbering.md).

The 0.17.0 implementation adds explicitly reviewed shared videos, immutable
physical ownership and one canonical Plex range import. All owners record
atomically, restart/retry/subset reuse retains the full binding, and cancellation
preserves queued interests. Exact Plex confirmation remains per owner. The new
format rejects silent downgrade. Complete CI and publication passed with 450 Rust
tests across 38 targets, four scheduler checks and all five workflow jobs. The
next stage is coordinated group replacement in 0.18; later commits need their
own complete CI. The exact release evidence is recorded below.
See [shared files](shared-files.md).

The 0.18.0 implementation adds reviewed complete shared-group baselines and
authenticated one-file replacements, immutable full parent lineage, staged exact
Plex confirmations and atomic promotion. Whole-group cancellation/retry and
monitoring fences preserve prior imports. Format 3 rejects partial or conflicting
group state and silent downgrade. Complete CI and publication passed with 467
Rust tests across 40 targets, four scheduler checks and all five workflow jobs.
The next stage is Plex requester policies in 0.19. Later commits need their own
complete CI. See [group upgrades](group-upgrades.md) and the exact evidence below.

The 0.19 implementation adds [requester policies](requesters.md): stable verified
account bindings, versioned opt-in profiles and routing, durable approvals/quotas,
independent full-watchlist cursors and compatible canonical demand. Removal keeps
other interests and imported bytes. Guarded CLI/API/browser controls and original
local-protocol CI scenarios cover recovery, races and credential redaction.
Its complete CI and publication passed with 489 Rust tests across 43 targets,
four scheduler checks and all five jobs. The 0.19.1 patch requires verified ready
imports and compatible quality before new requester reuse of operator jobs;
its own complete validation and publication passed with 490 Rust tests across
43 targets, four scheduler checks and all five jobs. The following stage is IRC
automation in 0.20; inspect live branch CI before beginning it.

Automatic anime/range inference, broader IRC adapters/actions, native
indexer adapters, Usenet and cross-seeding remain future stages. See the [release roadmap](roadmap.md). Series monitoring is a
focused stage and does not establish full stack parity.

Releases are hosted in [alecerf/mynou](https://github.com/alecerf/mynou/releases).
A successful validation run on `trunk` publishes a new `Cargo.toml` version if it
has not already been released. The assets are a full source ZIP with the static
Linux x86_64 binary, a separate static binary, a saved Docker image archive, and
SHA-256 checksum files. There is no Docker Hub publication step.

## Recorded 0.20.2 CI checkpoint

Commit `bb5aebd3727a34b02ab10c07909549529807fa4a` passed
[Actions run 37348828003](https://github.com/alecerf/mynou/actions/runs/37348828003):
543 Rust tests passed with none failed or ignored across 48 targets, plus four
scheduler checks and all five jobs. CI published
[v0.20.2](https://github.com/alecerf/mynou/releases/tag/v0.20.2) with seven assets
on October 5, 2026, at 17:32:22 UTC. The tag targets this exact tested source.
The v0.20.1 and v0.20.0 tags and all asset IDs, sizes and digests remain unchanged.
See [validation evidence](validation.md#recorded-0202-ci-evidence).
Later commits need their own completed workflow and do not replace published assets.

## Recorded 0.20.1 CI checkpoint

Commit `cf838f3750aaf129f1ac939a675a6eee4f635594` passed
[Actions run 37345455739](https://github.com/alecerf/mynou/actions/runs/37345455739):
533 Rust tests passed with none failed or ignored across 47 targets, plus four
scheduler checks and all five jobs. CI published
[v0.20.1](https://github.com/alecerf/mynou/releases/tag/v0.20.1) with seven assets
on October 5, 2026, at 17:05:20 UTC. The tag targets this exact tested source.
The v0.20.0 and v0.19.1 tags and all asset IDs, sizes and digests remain unchanged.
See [validation evidence](validation.md#recorded-0201-ci-evidence).
Later commits need their own completed workflow and do not replace published assets.

## Recorded 0.20.0 CI checkpoint

Commit `4da379a4ef48763d8a1035fb6b3cd4acc4a721bc` passed
[Actions run 37335746159](https://github.com/alecerf/mynou/actions/runs/37335746159):
512 Rust tests passed with none failed or ignored across 46 targets, plus four
scheduler checks and all five jobs. CI published
[v0.20.0](https://github.com/alecerf/mynou/releases/tag/v0.20.0) with seven assets
on October 5, 2026, at 15:50:41 UTC. The tag targets this exact tested source.
The preceding v0.19.1 tag and all asset IDs, sizes and digests remain unchanged.
See [validation evidence](validation.md#recorded-0200-ci-evidence).
Later commits need their own completed workflow and do not replace published assets.

## Recorded 0.19.1 CI checkpoint

Commit `83e6d1a40ac2d5abac0baf355a0011c58e49429a` passed
[Actions run 37305082540](https://github.com/alecerf/mynou/actions/runs/37305082540):
490 Rust tests passed with none failed or ignored across 43 targets, plus four
scheduler checks and all five workflow jobs. CI published
[v0.19.1](https://github.com/alecerf/mynou/releases/tag/v0.19.1) with seven assets
on October 5, 2026, at 11:48:49 UTC. The tag targets that exact tested source.
The preceding v0.19.0 and v0.18.0 tags and assets remain unchanged. See
[validation evidence](validation.md#recorded-0191-ci-evidence) for asset digests.
Later documentation commits need their own complete CI and retain published
artifacts. [Next-release notes](next-release.md) preserve concrete 0.20 work.

## Recorded 0.19.0 CI checkpoint

Commit `2388836a24ac02225a4171aa3d6bbbc32323b6c7` passed
[Actions run 37303359973](https://github.com/alecerf/mynou/actions/runs/37303359973):
489 Rust tests passed with none failed or ignored across 43 targets, plus four
scheduler checks and all five workflow jobs. CI published
[v0.19.0](https://github.com/alecerf/mynou/releases/tag/v0.19.0) with seven assets
on October 5, 2026, at 11:32:55 UTC. The tag targets that exact tested source.
The preceding v0.18.0 tag and assets remain unchanged. This evidence applies to
the initial requester release; the later patch needs its own completed run.

## Recorded 0.18.0 CI checkpoint

Commit `f1a9733a5908c3fa8bc5e93b5d18800334fad5d1` passed
[Actions run 37293887224](https://github.com/alecerf/mynou/actions/runs/37293887224):
467 Rust tests passed with none failed or ignored across 40 targets, plus four
scheduler checks and every dependency/format/Clippy/build/demo/package gate.
CI published [v0.18.0](https://github.com/alecerf/mynou/releases/tag/v0.18.0)
with seven assets on October 5, 2026, at 10:04:19 UTC. The tag targets the same
source commit, and all assets belong to `github-actions[bot]`.
See [validation evidence](validation.md#recorded-0180-ci-evidence) for SHA-256
digests. The v0.17.0 tag and all seven asset IDs, sizes and digests remain
unchanged. No local validation ran. Later documentation changes need their own
complete CI and do not replace the published source or assets.

The next stage is 0.19 Plex requester policies;
[next-release notes](next-release.md) preserve the bounded identities, per-account
token/poll/profile decisions, shared demand and recovery scenarios still required.

## Recorded 0.17.0 CI checkpoint

Commit `89e5aefa66026da084a6770fb76d51e7396602ec` passed
[Actions run 37271644993](https://github.com/alecerf/mynou/actions/runs/37271644993):
450 Rust tests passed with none failed or ignored across 38 targets, plus four
scheduler checks and every dependency/format/Clippy/build/demo/package gate.
CI published [v0.17.0](https://github.com/alecerf/mynou/releases/tag/v0.17.0)
with seven assets on October 5, 2026, at 06:19:26 UTC. The tag targets the same
source commit, and all assets belong to `github-actions[bot]`.
See [validation evidence](validation.md#recorded-0170-ci-evidence) for SHA-256
digests. The v0.16.0 tag and all seven asset IDs, sizes and digests remain
unchanged. No local validation ran. Later documentation changes need their own
complete CI and do not replace the published source or assets.

The next stage was 0.18 coordinated shared-group replacement, recorded above.

## Recorded 0.16.0 CI checkpoint

Commit `0821a4d3b499a5863fe5b50206c98bda25d6fb49` passed
[Actions run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691):
430 Rust tests passed with none failed or ignored across 36 targets, plus four
scheduler checks and every dependency/format/Clippy/build/demo/package gate.
CI published [v0.16.0](https://github.com/alecerf/mynou/releases/tag/v0.16.0)
with seven assets on October 4, 2026, at 21:59:49 UTC. The tag targets the same
source commit, and all assets belong to `github-actions[bot]`.
See [validation evidence](validation.md#recorded-0160-ci-evidence) for SHA-256
digests. Earlier published releases remain immutable. No local validation ran.
Later documentation changes need their own complete CI.

The next stage was 0.17 shared multi-episode physical ownership, recorded above.

## Recorded 0.15.0 CI checkpoint

Subsequent CI optimization adds bounded parallel harness execution, complete
Cargo-target checks, concurrent native/static builds, exact compiled-output
caches and Docker assembly from the checked binary. Successful cold/exact-cache
runs retain all 409 Rust tests and add four scheduler checks. The observed full
workflow changed from 313 seconds to 142 cold / 117 with exact caches. See
[CI execution and evidence](ci.md). This changes development infrastructure;
the published 0.15 tag/assets remain immutable and later commits require CI.

Commit `1fe40eed0b0ea170a03ffce8d30d2ab8cb3e7125` passed
[Actions run 37230875486](https://github.com/alecerf/mynou/actions/runs/37230875486):
409 tests passed with none failed or ignored across 34 targets, plus the complete
dependency, formatting, Clippy, native/musl build, native/Docker demonstration,
packaging and checksum pipeline. GitHub Actions published
[v0.15.0](https://github.com/alecerf/mynou/releases/tag/v0.15.0) with seven assets
on October 4, 2026, at 20:12 UTC. The release and tag point to that exact commit;
all assets were uploaded by `github-actions[bot]` with recorded SHA-256 digests.
See [validation evidence](validation.md#recorded-0150-ci-evidence).

No local tests, lint, builds, binaries or demonstrations were run. Later
documentation commits need their own complete CI and cannot replace the
published tag or assets. [Next-release notes](next-release.md) preserve the
next shared physical-import decisions after the numbering release.

## Recorded 0.14.0 CI checkpoint

Commit `a077af8d660a2b5ca12e47b579562e7a9292f6e2` passed
[Actions run 37221887812](https://github.com/alecerf/mynou/actions/runs/37221887812):
385 tests passed with none failed or ignored across 31 targets and the complete
dependency, formatting, Clippy, native/musl build, native/Docker demonstration,
packaging and checksum pipeline. GitHub Actions published
[v0.14.0](https://github.com/alecerf/mynou/releases/tag/v0.14.0) with seven assets
on October 4, 2026, at 17:53 UTC. The release and tag point to that exact commit;
all assets were uploaded by `github-actions[bot]` with recorded SHA-256 digests.

Post-release documentation commits need their own CI and do not replace the
published tag or assets. [Next-release notes](next-release.md) preserve the
later numbering and physical-import work. No local tests,
lint, builds, binaries or demonstrations were run for this release.

## Recorded 0.13.0 CI checkpoint

Commit `cb6e89700a63c1a7f9aaaa644fce32bf8944386f` passed
[Actions run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435):
374 tests passed with none failed or ignored across 29 targets and the complete
CI pipeline. GitHub Actions published
[v0.13.0](https://github.com/alecerf/mynou/releases/tag/v0.13.0) with seven assets
on October 4, 2026, at 15:39 UTC. This validates the explicit pack release.
Post-release documentation commits need their own CI, and do not replace the
published tag or assets. [Next-release notes](next-release.md) preserve the
concrete selective-file/automatic-pack/numbering work for the next session.

## Recorded 0.12.0 CI checkpoint

Commit `fbdf61c19b31f941a08f91fd19e3bb843aec0541` passed
[Actions run 37210722790](https://github.com/alecerf/mynou/actions/runs/37210722790):
364 tests passed with none failed or ignored across 27 targets and the complete
CI pipeline. GitHub Actions published
[v0.12.0](https://github.com/alecerf/mynou/releases/tag/v0.12.0) with seven assets
on October 4, 2026, at 14:55 UTC. This validates the series monitoring/calendar
release, not the new pack changes. Their own completed run/release are required.

## Recorded 0.11.0 CI checkpoint

Commit `d7cb8eb20d364c217ed89ab183ebf754512d7dfe` passed
[Actions run 37206645776](https://github.com/alecerf/mynou/actions/runs/37206645776):
343 tests passed with none failed or ignored, alongside the complete dependency,
formatting, Clippy, build, demo, Docker and packaging checks. GitHub Actions
published [v0.11.0](https://github.com/alecerf/mynou/releases/tag/v0.11.0)
with seven assets on October 4, 2026, at 13:48 UTC. This validates the browser
release, not later changes. Each following release needs its own completed CI.

## Recorded 0.10.0 CI checkpoint

Commit `5b0b202b78bc906db914ef4713c57ec14807169f` passed
[Actions run 37203872630](https://github.com/alecerf/mynou/actions/runs/37203872630):
321 tests passed with none ignored, together with the complete CI pipeline.
GitHub Actions published
[v0.10.0](https://github.com/alecerf/mynou/releases/tag/v0.10.0) with seven assets.
Its debug delayed local peer fixture measured 6,574 ms with the retained
single-peer path and 1,480 ms with four peers (4.441x). See
[validation.md](validation.md) for workload and limits.

This validates 0.10.0; later changes require their own completed run and release.

## Recorded 0.9.0 CI checkpoint

Commit `c45e127e3b8587a4f4ace6a2bbc7eab86bf48a66` passed
[Actions run 37187100999](https://github.com/alecerf/mynou/actions/runs/37187100999):
302 tests passed with none ignored, alongside the complete CI validation.
GitHub Actions published
[v0.9.0](https://github.com/alecerf/mynou/releases/tag/v0.9.0) with seven assets.
This is historical evidence for 0.9.0 and does not validate later changes.

## Recorded 0.8.0 CI checkpoint

Commit `9ef6f1fc94f2437aa6797e18385c0e81273c5e44` passed
[Actions run 37151554961](https://github.com/alecerf/mynou/actions/runs/37151554961):
251 tests passed with none ignored, alongside the complete CI validation.
GitHub Actions published
[v0.8.0](https://github.com/alecerf/mynou/releases/tag/v0.8.0) with seven assets.
This is historical evidence for 0.8.0 and does not validate later changes.

## Recorded 0.7.0 CI checkpoint

The preceding selection release at commit
`14686f01c17a88c6b9e45ce9d2672e0e3c66d21f` passed
[Actions run 37137061361](https://github.com/alecerf/mynou/actions/runs/37137061361)
and GitHub Actions published
[v0.7.0](https://github.com/alecerf/mynou/releases/tag/v0.7.0).
This is historical evidence for 0.7.0 and does not validate later changes.

## Recorded 0.6.1 CI checkpoint

Commit `2a066c2c3b9aafda47a3fc898872d1c371b5f249` passed
[Actions run 37134671116](https://github.com/alecerf/mynou/actions/runs/37134671116):
131 tests plus formatting, Clippy, builds, demos and packaging. GitHub Actions
published [v0.6.1](https://github.com/alecerf/mynou/releases/tag/v0.6.1).
This preceding release does not validate later changes. Record or inspect
their own successful Actions run before claiming validation or publication.

## Historical 0.6.0 evidence

The former delivery was validated on October 3, 2026, before the CI-only policy.
It passed 131 tests without failures or ignored tests, formatting, warning-free
Clippy across all targets, and GNU/musl release builds. The tests included the
race between request cancellation and torrent startup. Its musl binary was
1,717,088 bytes and had no ELF interpreter or required dynamic library.

Its final `scratch` image was 1,898,812 bytes, ran as UID/GID 1000:1000, and had ID:

```text
sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146
```

Layer inspection found only the binary and CA bundle as embedded regular files.
Its demo completed a local magnet download, import, and simulated Plex
confirmation with `--network none`, a read-only root, and Linux capabilities
removed. A generated Compose installation was checked without Rust on the host:
health, doctor, API, byte-identical import, and the same request ID and `ready`
state after restart. The validation containers were removed afterward.

The historical release benchmark ran 10,000 warm-cache analyses of the demo MP4:
8.98 µs per analysis, approximately 111,333 analyses/s, SHA-256 at 177.14 MiB/s,
and SHA-1 at 250.74 MiB/s in a shared environment with two logical processors.
See [performance.md](performance.md) and its unchanged raw JSON.

These results describe **0.6.0**. Do not reuse them as proof that later changes
passed CI. Use the Actions run for the current commit and release.

## Remaining installation configuration

Connecting personal Plex requires its address, section IDs, shared paths, Plex
token, and TMDB/source credentials. Local-service tests are not a deployment on
that personal installation.

No available tool exposes the account's ChatGPT quota consumption. This file
supports an explicit handoff; it does not claim automatic quota monitoring or
resumption after a reset.
