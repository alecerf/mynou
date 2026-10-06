# Active release — 0.22.7 bounded ZIP and DEFLATE formats

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

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

The active 0.22.7 source adds original bounded classic ZIP parsing and streaming
raw DEFLATE decoding. Stored, fixed-Huffman and dynamic-Huffman payloads use a
32 KiB window, bounded lookup tables and 64 KiB writes; decoded size and CRC must
match the checked directory before a payload proof is returned. Headers,
descriptors, names, attributes, offsets, overlap and resource limits are checked.
The read-only `zip-inspect` CLI reports metadata with `content_verified: false`
and creates no output, jobs or network requests. Generic extraction writes only
to a caller-owned provisional sink and rechecks metadata before decoding.
This source requires its own complete Actions validation and publication.
Ownership-bound archive admission follows in 0.22.8; ZIP64, RAR, PAR2, multi-file
packs and Usenet upgrades remain separate bounded increments.

After publication, continue 0.22.8 ownership-bound ZIP admission. Capture the
exact verified archive identity, selected entry, decoder limits and output proof
before publishing private results; recheck canonical/requester lease and approval
before every durable transition. Keep source/output separate and private, and
copy library imports. Recovery must reject unknown or corrupt extraction state
before initializer writes. Then implement separate bounded RAR/PAR2, multi-file
and Usenet-upgrade increments, followed by 0.23 verified cross-seeding and guarded
bulk controls. Use original synthetic media and loopback services only. Record
exact source/five jobs/tests/seven bot assets and prior immutability at every
publication. No broad parity or unmeasured performance claims.
