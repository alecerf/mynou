# Current product work — proposed 0.22.14 read-only PAR2 verification

GitHub Actions published v0.22.13 multi-file memory recovery at
`6085368588c8077af53d15bc4f07d752e670e445` in run 37648617664: all five jobs
passed, 846 Rust tests across 64 harnesses and seven immutable bot assets.
[Issue #9](https://github.com/alecerf/mynou/issues/9) comment 6041888395 and
[PR #10](https://github.com/alecerf/mynou/pull/10) retain actual native publication,
Security/QA and cleanup evidence. Cadence #11 / PR #12 is delivered separately;
its organization-only merge did not retag the product.

[Issue #13](https://github.com/alecerf/mynou/issues/13) owns the next bounded
repair-planning prerequisite: diagnose protected content before considering any
filesystem repair. `par2-verify FILE --root DIRECTORY` and exact-ID library
reports check slices, full-file and first-16-KiB hashes with bounded reads,
identity fencing and cancellation. No parity correctness, ownership, atomic
snapshot, writes or library admission is claimed. See [support](par2.md) and
[proposed scope](releases/0.22.14.md).

Complete actual CI, separate exact-head/base Security and independent QA, merge
and immutable Actions publication before ownership-bound persistent repair and
admission. Use one configured whole-wake deadline with serial role leases and a
five-minute handoff reserve. No local validation or manual publication. A proposed
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
