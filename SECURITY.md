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
with no runner execution are reported separately. Running workflows are explicitly
outside the completed-attempt snapshot and are covered by subsequent audits.

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
