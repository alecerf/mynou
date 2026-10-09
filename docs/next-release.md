# Current work — proposed 0.22.37 opt-in parallel engineering team

[Issue 58](https://github.com/alecerf/mynou/issues/58) adds a bounded, opt-in team
model: per-worker leases with server-read `area:*` labels, a capacity cap,
exclusive `area:control` work that drains the team, individual expired-lease
recovery and a singleton delivery lease. Schema-1 behavior is unchanged until the
reviewed `enable-team` transition. See [scope](releases/0.22.37.md). Versions
0.22.33-0.22.36 are reserved by other in-flight work; this source stays proposed
until CI, distinct exact-head/base Security/QA and Actions publication complete.
No local validation.

## Earlier proposed 0.22.32 scope

Fresh engineering authority reads (proposed 0.22.32):

Actions actually published immutable [v0.22.31](https://github.com/alecerf/mynou/releases/tag/v0.22.31)
at `adb8808d91e0f87017c7cad0ae9d787a7448feee` in successful seven-job
[CI37879166856](https://github.com/alecerf/mynou/actions/runs/37879166856).
Release407471331 was published2026-10-09T03:30:15Z with the exact source tag,
four named digested assets and source/run/attempt receipt11593488474.
User Safari Issue48 and PR53 are closed with distinct Security/QA, accepted gate,
actual publication and merged-branch absence evidence. The original source
and prior default remain merge parents; earlier immutable releases are preserved.

[Issue52](https://github.com/alecerf/mynou/issues/52) records a publication recovery
that verified v0.22.30, then stopped at the authority head mismatch guard and
left its lease held until expiry. The retained native history shows no competing
committed advance in that failed job's window. Historical logs do not establish
a particular HTTP cache or provider cause; later v0.22.31 recovery succeeded.

This bounded mitigation requests `Cache-Control: no-cache` for authenticated
native REST GETs, so compliant caches revalidate authority and gate observations.
It adds no API retry, request allowance, worker, scheduling change or alternate
state store. Exact observed heads, sole-parent/non-force arbitration and ownership
checks remain required. A provider that still returns inconsistent state causes
the existing safe stop; cache revalidation is not a consistency guarantee.

Original CI-only transport scenarios cover checkpoint-to-release with a
synthetic cached reference, preservation of publication metadata, changed
authority/owner, concurrent CAS conflicts and ignored revalidation.
The fixtures model the boundary rather than reproduce the historical provider.
See [scope](releases/0.22.32.md). This source version remains proposed until
actual CI, distinct exact-head/base Security/full-diff QA, every objective gate
and Actions-only immutable publication are complete. No local validation.

After this bounded recovery improvement, Product42 can rearm from actual
publication.39/45 remain Ready product proposals,40 is natively blocked by39,
46 requires triage and54 separately tracks audit API capacity. Native Issues
and dependencies take precedence over these historical documentation snapshots.
The hourly recovery and single-worker policy remain unchanged.

## Earlier guided setup scope

Actions actually published immutable v0.22.27 at
`5fb6bcb46b80926b212e5bce0fccbf42addcfcbe` in complete successful
[CI37824292805](https://github.com/alecerf/mynou/actions/runs/37824292805).
Release407145259 was published2026-10-08T18:32:46Z with the exact tag,
three executable assets/SHA256SUMS and source-bound private GHCR proof.
Issue38/PR43 retain separate Security/QA, accepted gate and cleanup evidence.

The first Product review on native Issue42 maintains three unblocked Ready
product increments: #41 guided setup (P1), #39 media-sized PAR2 (P2) and #45
bounded live progress (P2). #40 remains Needs Triage and actually blocked by39;
#46 proposes native Usenet upgrades with prerequisites still to triage.
#44 records the actual structured-publication rearm gap; fix it after41.
These are native proposal/priority decisions, not delivered features or dates.

[Issue41](https://github.com/alecerf/mynou/issues/41) delivered a private
server-rendered Setup page using existing configuration, browser sessions and
guarded source/Usenet diagnostics. Static missing/attention/configured/optional
states, matching native routes and first-request guidance expose no raw paths,
endpoints, credential names/values or upstream responses. GET has no network,
disk, acquisition or notification side effects. Configuration presence is not
verified connectivity or permissions. See [scope](releases/0.22.28.md).

Complete original CI fixtures, distinct exact-head Security/full-diff QA,
objective merge and default Actions publication before claiming this new release.
No local validation/manual publication, concurrency or runtime dependencies.
The existing hourly recovery remains unchanged and best-effort.

## Earlier PAR2 work and publication history

GitHub Actions published v0.22.13 multi-file memory recovery at
`6085368588c8077af53d15bc4f07d752e670e445` in run 37648617664: all five jobs
passed, 846 Rust tests across 64 harnesses and seven immutable bot assets.
[Issue #9](https://github.com/alecerf/mynou/issues/9) comment 6041888395 and
[PR #10](https://github.com/alecerf/mynou/pull/10) retain actual native publication,
Security/QA and cleanup evidence. Cadence #11 / PR #12 is delivered separately;
its organization-only merge did not retag the product.

Actions published v0.22.14 read-only diagnosis at
`7910156c6cce0f74a9ab530b289c3734750bc994` in CI37668451522, all five jobs
green, 858 Rust tests and seven immutable assets. Issue #13 comment6045313189 /
PR #14 retain Security/QA, machine gate, native cleanup and publication evidence.

Completed [Issue #15](https://github.com/alecerf/mynou/issues/15) delivered owner-bound
private persistence: reconstruct every file before new private writes, hold a
standard file lock, commit exact immutable source/policy/inventory and reverify
all reopened output. No existing-file overwrite or automatic library/queue
activation is claimed. See [support](par2.md) and [scope](releases/0.22.15.md).

Complete actual CI, separate exact-head/base Security and independent QA, merge
and immutable Actions publication before automatic queue/repair and library
admission integration. Use the installed continuous execution policy with serial role leases and
frequent remote checkpoints; stop for actual waits, capacity or ownership loss. No local validation or manual publication. A proposed
source version does not establish publication.

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

The active 0.22.10 source adds opt-in stored RAR5 admission to native Usenet
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

After 0.22.14 verification publication, continue ownership-bound persistent repair/admission. Keep
RAR compression as a separate original implementation increment; do not claim
compressed RAR support from the stored subset. Continue multi-file/pack and
Usenet-upgrade stages, then 0.23 verified cross-seeding and guarded bulk controls.
Preserve source/entry/owner identity, captured bounds, private intent/proofs,
current canonical/requester permission and independent library imports. Use
original synthetic media and loopback services only. Record exact source, five
jobs, test counts, seven bot assets and prior immutability at every publication.
After bootstrap, each green release queues its next bounded scope. English,
Rust std only, no local validation or manual publication. No unmeasured
performance or broad parity claims.

