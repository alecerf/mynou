# Active release — 0.22.2 checked Usenet disk receipts/assembly

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. AGENTS.md mandates English, Rust std only,
zero dependencies, no unsafe/FFI/copied code/external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push completed scope,
inspect all five jobs and fix reds through commits. Actions alone publishes exact
source tags/assets. Complete publication before a successor push.

CI published v0.22.1 from `cbc10d16b108c03a157d7afb6c91034ef8ac4343` in
[run 37436785004](https://github.com/alecerf/mynou/actions/runs/37436785004).
All five jobs passed: 652 Rust tests across 56 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 71.509 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:35:55 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.0 tag and asset IDs, sizes and digests remained unchanged.

The 0.22.2 source adds workspace.rs/workspace/disk.rs and checked yEnc receipt
restoration. New private single-file workspaces bind exact NZB/file/provider/
size limits and capture expected article identities. Checked descriptor and part
frames, private modes/process lock and immutable receipts protect restart reuse.
Assembly requires complete coverage/CRC and streams one bounded part in 64 KiB
chunks. Checked prepared output SHA/CRC/length/temp proofs precede publication/
ready state; read-only recovery writes nothing. Original tests include corruption,
private/link/owner checks, crash windows and 65 MiB output. This source awaits CI.

After publication, continue 0.22.3 native bounded durable queue and manager using
these workspaces. Preserve original NZB bytes once per source, not per file. Add
opt-in download settings with resolved private state_dir and at most two workers;
immutable provider bindings, checked queue state and persisted per-part attempts
must precede network. Guarded raw operator enqueue/controls stage private data
only; canonical/requester/profile/ownership/library admission remains ordinary
Engine work in the next increment. Then add Engine/Newznab integration, followed
by verified cross-seeding/bulk in 0.23. Inspect the staged design at
/workspace/scratch/mynou-usenet-acquisition-design.md. No archive/PAR2 helpers or
broad parity/performance claims. Synthetic local services/media only. Record exact
source/five jobs/tests/seven bot assets/prior immutability at every publication.
