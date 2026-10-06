# Active release — 0.22.4 typed native Newznab discovery

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

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

The active 0.22.4 source adds a native Newznab adapter and typed Usenet
advertisements. Movie/year and seasonal/absolute episode identity, selection
profiles, explicit provider bindings, advertised size and password policies gate
candidates. Public search previews identify the transport without private URLs
or credentials. Bound NZB document reads reuse native authentication, source
pause/generation, rate, origin and absolute-deadline checks; they never queue an
article or create a library job. Existing torrent source bindings remain intact.
This source requires its own complete Actions validation and publication.
Ordinary Engine admission follows before verified cross-seeding/bulk.

After publication, continue ordinary Engine admission in further 0.22 increments.
Preserve canonical requester approval,
quota/profile/ownership/numbering/import/Plex gates; raw queue output cannot bypass
them. Use original synthetic media and loopback provider/indexer/Plex fixtures.
Improve safe constructor recovery through explicit checked intents if needed;
never repair unknown/corrupt data implicitly. RAR/ZIP/PAR2 need separate original
bounded implementations, without helpers. Then continue 0.23 verified cross-seeding
and guarded bulk controls. Record exact source/five jobs/tests/seven bot assets and
prior immutability at every publication. No broad parity/performance claims.
