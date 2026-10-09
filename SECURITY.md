# Security

This repository is public. Never report credentials, sensitive media, personal data
or exploitable details in public Issues, pull requests, comments or Actions logs.
Use GitHub's private vulnerability reporting when it is enabled; otherwise arrange
a private channel with the repository owner before sending sensitive details.
A credential found in public history or logs must be revoked or rotated. Removing
its current occurrence does not revoke it or erase copies.

The Security audit workflow checks all reachable fetched Git objects and all
completed visible Actions attempts. Reports contain rule names and object/run
locations only; matched values, previews, raw filenames, signed log URLs and raw
archives are never published. Missing executed logs fail as coverage gaps; attempts
with no runner execution are reported separately. A pending rerun's earlier completed
attempts remain in scope; its current incomplete attempt is recorded as pending.

Routine audits reuse redacted completed-attempt evidence only from a successful
native default-branch Security audit in this repository, with exact scanner and
workflow bytes, default ancestry, successful required jobs and one unexpired,
attempt-bound artifact. The native ZIP digest, producer and every current
run/attempt/source/workflow/repository identity must match. PR/fork artifacts,
old aggregate reports, changed identities and missing or conflicting applicable
receipts cannot certify coverage. New attempts are scanned; lost executed logs
remain gaps. Reports distinguish newly downloaded archives from reused coverage.

The first compatible receipt requires a full scan. Weekly scheduled audits and
the manual `full_log_audit` option rebuild it; routine runs only download uncovered
archives after re-enumerating native history. An observed GitHub request reserve
and a hard request bound stop insufficient-capacity work without queued request
storms. Failure remains visible and blocks delivery. This does not inspect Codex
quota or guarantee a provider reset/wake. Receipts remain native artifacts for
14 days; raw archives are never uploaded. Detailed attempt metadata stays in the
redacted artifact while logs print aggregate coverage and safe numeric counters.

The original Python standard-library detector recognizes selected provider token
formats and private-key markers. It cannot prove the absence of every custom
credential or arbitrary sensitive data, and is not GitHub's native secret scanning.
Native alert reads and scanning/push-protection activation returned HTTP 403
(Resource not accessible by integration) on 2026-10-08, even after public visibility.
The owner must enable and inspect native protections in repository Settings/Security
where available. Do not describe unverified native settings as enabled.

All scans and regression checks execute in Actions only. Write-token delivery and
release jobs use trusted default source; audit jobs have read-only permissions.
Only synthetic fixtures are used for runtime verification. Keep production values
in secret bindings, never command text or diagnostic payloads.

The product uses original safe Rust std primitives, without a third-party crypto
audit or a claim of formal constant-time behavior. Format hashes prove integrity,
not authorization. Master owns triage, separate Security verification and QA.
