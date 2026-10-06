# Active release — 0.21.0 native indexer authentication/session health

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

v0.20.7 passed run 37429460979 for 3de105188e855e35b5ae86be012df43842c25148:
602 tests/53 harnesses, none failed/ignored, four scheduler checks, execution
63.950 seconds at two processes/two threads. CI published seven bot assets
at 07:28:39 UTC on October 6, 2026, preserving v0.20.6. Exact proof is recorded in
validation.md and /workspace/scratch/mynou-20.7-release-proof.json.

The 0.21.0 source adds indexers.rs and session.rs. Source.options has legacy
configuration defaults and a shared in-memory runtime through Config clones.
Basic/Bearer credentials, explicit same-origin form/cookie sessions, bounded
renewal and intervals/429 cooldowns use original HttpClient/TLS, with no redirect
or unauthenticated fallback. Cookie bytes are absent from persistence/Debug/
reports. Existing RSS/JSON/Torznab parsing and selection remain mandatory. Native
search calls indexers::fetch and records parse health. Protected API/CLI/browser
views show only aliases, fixed diagnostics, bounded counters and session presence.
Original protocol scenarios and two parser units await their exact complete CI.

After publication, implement 0.21.1 checked persistent source policy controls.
Use a separate checked private snapshot with immutable stable source bindings,
operator enable/pause policy and bounded records, without cookies/secrets. Apply
shared runtime policy before workers. Guarded CLI/API/browser controls need
current scope/session checks; offline preview remains read-only. Add safe session
reset and transport probe without acquisition. Keep original default source names
and query identity behavior compatible; changed explicit bindings require new IDs.
Inspect /workspace/scratch/mynou-indexer-design.md for the staged design.

Then continue native bounded Usenet search/acquisition/integrity/recovery in 0.22,
with explicit formats and no external helpers. Then continue verified safe
cross-seeding and further bulk automation in 0.23. Use synthetic media/local
services. No personal messages/public torrents or broad parity/performance claims.
Record exact source/five jobs/tests/seven CI-owned assets/prior immutability at
every publication. No quota interface or unattended resumption is available.
