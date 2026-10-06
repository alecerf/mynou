# Active release — 0.22.9 bounded RAR5 stored formats

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

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

The active 0.22.9 source adds original bounded RAR5 parsing and stored-payload
verification. Headers have checked CRCs, bounded variable integers and an explicit
main/file/end layout. Safe UTF-8 names, file types, collisions and declared limits
are checked. Stored payloads stream in 64 KiB chunks into caller-owned provisional
sinks and return size/CRC/SHA proof only after exact integrity and cancellation
checks. Structural metadata is reparsed before extraction. Read-only `rar-inspect`
reports `content_verified: false` without configuration, network or output writes.
This source requires its own complete Actions validation and publication.
Compressed/solid/split/encrypted RAR, RAR4, service blocks and most extras remain
unsupported explicitly. Native RAR admission, PAR2, multi-file packs and Usenet
upgrades follow in separate increments; the verified v0.22.8 ZIP path remains.

After publication, continue 0.22.10 ownership-bound stored RAR5 admission.
Capture format, exact source/entry/owner and limits without changing legacy ZIP
identities. Reuse private journal/frame intent-before-write, complete-before-link
recovery, current permission fences and independent library imports. Then continue
original RAR compression and PAR2 increments, multi-file admission and Usenet
upgrades, followed by 0.23 verified cross-seeding and guarded bulk controls.
Use original synthetic media and loopback services only. Record exact source,
five jobs, test counts, seven bot assets and prior immutability at every
publication. A green release starts the next scope; do not stop at a checkpoint.
English and Rust std only, no local validation, manual publication or unmeasured
performance/parity claims.
