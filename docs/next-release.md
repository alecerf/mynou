# Active release — 0.22.1 native NNTP/TLS and guarded probes

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

CI published v0.22.0 from `9644114e5f1e8c2ed679d57a3a2b18c479ee4ac2` in
[run 37434792979](https://github.com/alecerf/mynou/actions/runs/37434792979).
All five jobs passed: 639 Rust tests across 55 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 64.375 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:17:36 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.1 tag and asset IDs, sizes and digests remained unchanged.

The 0.22.1 source adds original NNTP/settings/management modules. Config.usenet
is opt-in with up to eight immutable providers and environment credential names.
Verified TLS, strict greeting/AUTHINFO/BODY identity, bounded CRLF/dot bodies and
absolute budgets prevent fallback or raw errors. Shared runtime try-locks avoid
duplicate connections. Guarded non-acquiring probes bind settings/instance/attempt
count; CLI/API/browser paths and expiring session reviews exclude endpoints and
secrets. Original local NNTP fixtures plus a verified native TLS authentication
unit await the complete CI. Encoded yEnc limits include worst-case line overhead.

After publication implement 0.22.2 checked disk-backed NZB acquisition/recovery.
Use immutable source/server bindings, private snapshots, per-part identity/CRC,
atomic part receipts and verified restart recovery before publication/import.
Do not trust raw NNTP success or expose arbitrary article downloads through API.
Preserve original canonical admission, requester/quality/ownership gates and
bounded concurrency. Then connect Newznab in further 0.22 increments, followed by
verified cross-seeding and bulk controls in 0.23. Archives/PAR2 require original
implementations with explicit format limits, no external helpers. Synthetic media/
local services only. Record exact source/five jobs/tests/seven bot assets/prior
immutability at every publication. No quota interface/unattended resumption exists.
