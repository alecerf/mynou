# Security

Mynou is currently private. Report vulnerabilities through a private repository
Issue labelled `agent-work`, `kind:security`, `origin:security`, `priority:p0` and
`risk:critical` when severity warrants it. Include a redacted reproduction and
impact, never credentials or sensitive media. The Master owns prioritization,
remediation, separate Security verification, QA and delivery.

The product uses original safe Rust std primitives, without a third-party crypto
audit or a claim of formal constant-time behavior. Format hashes are integrity
checks, not permission to acquire or import content. Keep trust boundaries,
captured policy, revocation and durable provenance explicit.

Engineering write-token Actions execute only trusted default-branch code. The
organization checks high-confidence credential patterns in CI; this limited
scanner is not GitHub secret scanning or a complete security audit. Native code
scanning is not enabled and secret-scanning administration is integration-denied.
See engineering/README.md for observed capabilities and recovery requirements.
