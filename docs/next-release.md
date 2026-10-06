# Active release — 0.22.6 native Usenet library admission

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

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

The active 0.22.6 source connects typed Newznab acquisitions to approved canonical
movie and episode jobs. The journal captures the selection/profile, exact NZB
hash and preparation limits before held staging; only a matching current lease
and approved demand may grant queue permission. Permissions expire at the lease
deadline or thirty seconds, and polling/heartbeats renew them without resetting
article attempts. Cancellation/removal revoke rights and fence late results.
Joint journal/queue/source/owner checks precede existing-journal tail repair,
initializer writes and workers. A checked held crash window can be linked without
refetching the document. Fully reverified single-file media must satisfy filename,
year/numbering, captured quality and actual size policy before ordinary analysis
and atomic import. Plex must confirm the exact imported path. Explicit retry
retains source identity and budgets. Public projections omit private bindings.
This source requires its own complete Actions validation and publication.
Archive/PAR2, pack and Usenet-upgrade support remain separate increments.

After publication, continue native bounded archive/repair and Usenet-upgrade increments.
Implement original ZIP/DEFLATE first with retained provenance and no external helpers;
RAR/PAR2 and multi-file admission require their own checked stages.
Preserve canonical requester approval,
quota/profile/ownership/numbering/import/Plex gates; raw queue output cannot bypass
them. Use original synthetic media and loopback provider/indexer/Plex fixtures.
Improve safe constructor recovery through explicit checked intents if needed;
never repair unknown/corrupt data implicitly. RAR/ZIP/PAR2 need separate original
bounded implementations, without helpers. Then continue 0.23 verified cross-seeding
and guarded bulk controls. Record exact source/five jobs/tests/seven bot assets and
prior immutability at every publication. No broad parity/performance claims.
