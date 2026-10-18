# Security

This repository is public. Never report credentials, sensitive media, personal data
or exploitable details in public Issues, pull requests, comments or Actions logs.
Use GitHub's private vulnerability reporting when it is enabled; otherwise arrange
a private channel with the repository owner before sending sensitive details.
A credential found in public history or logs must be revoked or rotated. Removing
its current occurrence does not revoke it or erase copies.

CodeQL runs in CI and remains a required check. The custom source-history and
scheduled Actions-log scanners have been removed.
Check repository-native secret protections in GitHub Settings/Security; do not
assume they are enabled. Keep credentials outside the checkout and logs.
The release workflow publishes only validated tagged source with a scoped token.

Only synthetic fixtures are used for runtime verification. Keep production values
in secret bindings, never command text or diagnostic payloads.

The product uses original safe Rust std primitives, without a third-party crypto
audit or a claim of formal constant-time behavior. Format hashes prove integrity,
not authorization. Assess security-sensitive changes according to their actual risk.
