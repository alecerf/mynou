# Active release — 0.20.7 durable native notifications

Continue all roadmap stages autonomously. A green publication starts the next
scope; do not stop at a checkpoint. Read AGENTS.md: English, Rust std only, zero
Cargo dependencies, no unsafe/FFI/copied code, no external runtime helpers or local
validation. Formatting is an edit. Commit meaningful chunks, push the completed
scope and inspect all five jobs. Fix reds through commits. Actions alone creates
tags/releases/assets. Await exact publication before the successor push.

v0.20.6 passed run 37427693256 for 0b7d9c67a666b6929c626a930450f3dccae13d41:
585 tests/52 harnesses, none failed/ignored, four scheduler checks, execution
64.921 seconds with two processes/two threads. CI published seven bot assets
at 07:10:02 UTC on October 6, 2026; the tag matched and v0.20.5 stayed unchanged.
Proof: /workspace/scratch/mynou-20.6-release-proof.json and validation.md.

The 0.20.7 source implements original notifications.rs/delivery.rs. Requester
State.notify enqueues preference-filtered outcomes atomically. IRC store.save
compares durable decisions/routing phases and enqueues fixed outcomes atomically.
DeliveryState retains immutable private route bindings, stable digest IDs and
bounded events/attempts. A saved sending lease precedes HTTP I/O outside persistent
locks. Acknowledgments remain separate from acquisition; expired leases preserve
attempts. Native endpoint-bound HTTP rejects redirects and uses fixed errors.
Guarded CLI/API/browser controls retain scope/session/replay protection. Public
payloads expose only aliases/IDs/fixed outcomes. Legacy empty delivery state is
omitted; new semantics require requester format 3 and IRC format 4.

Original CI fixtures cover actual local HTTP, preferences, retry/budget, expired
leases, redaction, disabled/removed routes, no backfill, immutable bindings,
corruption/magic, capacity/terminal pruning, credential isolation, protected
controls, offline reads and worker delivery. No local validation is permitted.
The current source awaits its exact complete workflow and publication.

Retain canonical IRC request admission/recovery, quotas/approvals and captured
numbering/routes; metadata-only hash/file/profile/ownership gates remain mandatory.
Notify failures never approve or download. Old release tags/assets stay immutable.
Use synthetic media/local services, never public torrents or personal messages.

After green publication, continue native indexer adapters, authentication/session
renewal, source health and configuration in 0.21. Inspect existing Source and
integrations::search/HTTP/TLS before implementation; add original Basic/Bearer/form
session support with strict origin binding, bounded renewal/rate limits and
redacted health/configuration controls. Then implement native bounded Usenet
search/acquisition/integrity/recovery in 0.22, with explicit formats and no
external unpacking/recovery helpers. Then implement verified safe cross-seeding
and further bulk automation in 0.23. Each scope may split into tested increments.

Record exact source, all five jobs, tests/harness measurements, seven CI-owned
assets and preceding release immutability at each publication. No account quota
interface or unattended resumption is available; do not invent either.
