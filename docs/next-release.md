# Active release — 0.21.1 checked source policy

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

CI published v0.21.0 from `8edd0e4f91039ba2781008ea5c1cf010ef59781f` in
[run 37431238845](https://github.com/alecerf/mynou/actions/runs/37431238845).
All five jobs passed: 616 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 63.220 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
07:44:52 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.20.7 tag and asset IDs, sizes and digests remained unchanged.

The 0.21.1 source implements MYNOUS01 checked private indexer policy, immutable
stable source bindings, retained removed records and shared runtime enable policy.
Whole-policy guards cover pause/enable/reset/probe; reset invalidates cookies and
policy changes reject in-flight responses. Protected CLI/API/browser controls
use read-only previews and session-bound expiring reviews. Probes search once
within five seconds and never queue media. Native source authentication fixtures
also cover persistence, concurrency, storage failure and controls; browser session
units check expiry/identity. This source awaits its own complete CI/publication.

Then continue native bounded Usenet in focused 0.22 increments: NZB/yEnc/integrity
parsing, original NNTP/TLS, checked acquisition/recovery and Newznab integration.
Explicit format limits and no external helpers remain mandatory. Then continue
verified safe cross-seeding and bulk automation in 0.23. Use synthetic media/local
services, with no personal messages/public torrents or broad parity/performance
claims. Record exact source/five jobs/tests/seven bot assets/prior immutability
at every publication. No quota interface or unattended resumption is available.
