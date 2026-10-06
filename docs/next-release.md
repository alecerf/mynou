# Active release — 0.22.0 NZB/yEnc format integrity

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

CI published v0.21.1 from `88c52d761eed0ea1fb88ba3d52571047ee862dd7` in
[run 37433196413](https://github.com/alecerf/mynou/actions/runs/37433196413).
All five jobs passed: 628 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 60.185 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:02:35 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.0 tag and asset IDs, sizes and digests remained unchanged.

The 0.22.0 source adds usenet/nzb.rs and yenc.rs. Original XML moved to xml.rs
and retains RSS/Torznab regression fixtures. NZB has bounded exact article IDs,
contiguous numbering, global capacities and private claims. CLI nzb-inspect is
read-only without configuration/network/journal effects. Original CRC table and
incremental checks validate immutable decoded parts and exact complete assembly.
Header/CRC/range/filename limits are explicit; memory assembly is capped at 64 MiB.
Original tests/usenet_formats.rs scenarios await this source's complete CI.

After publication, continue 0.22.1 original bounded NNTP transport with verified
TLS, strict greeting/AUTHINFO/BODY responses, safe environment credentials,
dot-unstuffing, per-operation absolute budgets and local protocol fixtures. Then
implement checked disk-backed acquisition/recovery and Newznab integration in
further 0.22 increments. No external archive/repair helpers. Then implement
verified safe cross-seeding and bulk automation in 0.23. Synthetic media/local
services only; no broad parity/performance claims. Record exact source/five jobs/
tests/seven bot assets/prior immutability at every publication. No quota interface
or unattended resumption is available.
