# Native indexers and authentication

Mynou 0.21.0 keeps its original JSON, RSS and Torznab adapters and release
selection. Native authentication/session/health passed complete v0.21.0 CI.
Configure an endpoint that directly serves that adapter; fetches reject redirects.
Existing sources default to enabled, without authentication or interval limits.
Existing API-key query configuration remains supported.
The native Newznab adapter in 0.22.4 adds explicit Usenet metadata/provider
bindings through the same authentication and source-policy controls. See
[Newznab configuration and staged limits](usenet.md#native-newznab-discovery-in-0224).

```json
{
  "indexers": [
    {
      "id": "movies-source",
      "name": "movies",
      "kind": "torznab",
      "url": "https://indexer.example.test/api",
      "enabled": true,
      "min_interval_ms": 1000,
      "api_key_env": "MYNOU_INDEXER_API_KEY",
      "authentication": {
        "method": "bearer",
        "token_env": "MYNOU_INDEXER_TOKEN"
      }
    }
  ]
}
```

Authentication methods are `none`, `bearer`, `basic`, and `form`. Bearer requires
`token_env`. Basic requires `username_env` and `password_env`; usernames cannot
contain a colon. Credential environment names are bounded, and values retain
UTF-8 bytes without trimming. Empty/control-bearing/oversized values are rejected.
Bearer values must satisfy the token alphabet. Basic produces original Base64
UTF-8 credentials. Remote authentication requires verified HTTPS; plain HTTP is
allowed only for literal loopback fixtures. Public health exposes the mode alone.

An explicit form adapter requires method, login_url, username_env, password_env,
username_field, password_field and cookie_name. `max_age_secs` defaults to 1800
and accepts 1–3600. Form names are distinct bounded tokens. Login is a direct
same-origin POST with two percent-encoded fields; its URL has no query/fragment.
A 2xx response must contain exactly the configured named cookie. No redirect,
HTML form inference, CSRF/CAPTCHA challenge or interactive login is supported.

The single-cookie parser bounds value/attributes and checks path/domain,
Max-Age/canonical GMT Expires, Secure, HttpOnly and SameSite. It rejects ambiguous
multiple cookies, duplicate/unknown attributes, invalid expiry and unrelated
scope. Max-Age takes precedence over Expires; configured TTL is always an upper
bound. Cookie values stay in memory and never enter state, Debug output or reports.
Expired sessions renew before a fetch; a 401 renews once and a second 401 clears
the session and fails. There is no unauthenticated fallback. Cloned source
configuration shares the same bounded runtime/session.

Fetches retain the existing shared search deadline and an 8 MiB response limit.
Login has a 64 KiB response limit and a ten-second maximum within that deadline.
Source request intervals accept 0–60000 ms. HTTP 429 accepts bounded numeric
Retry-After, defaulting to 60 seconds and clamping to 1–3600; it never retries
immediately. A concurrent request to the same busy source fails without waiting.
Other configured sources retain independent behavior. Synchronous DNS can exceed
an HTTP budget; late results are rejected. Selection/metadata/integrity gates
remain mandatory and health probes do not download media.

`mynou indexers --config mynou.json`, protected `GET /api/indexers` and the
browser **Indexers** page report safe source aliases, adapter, enabled/auth mode,
request/success/failure counters, fixed last diagnostic, HTTP status, parse result
and session presence. URLs, environment names/values, raw response bodies,
authentication headers and cookie values are omitted. Busy views are explicit.
Offline inspection verifies existing policy storage without writing; it does not
restore live sessions or imply a successful network check.

## Checked policy and guarded controls in 0.21.1

This increment passed complete v0.21.1 CI. An optional stable `id` is
1–64 lowercase letters, digits, underscores or hyphens. Without it, identity is a
SHA-256 digest of the private binding, preserving legacy configuration. Duplicate
IDs fail. A stable ID binds name, kind, endpoint, API-key environment name, request
interval and authentication configuration. Changing that binding requires a new
ID. Credential values and cookies are never stored. Enabled policy starts from
configuration once, then persists independently of subsequent `enabled` settings.

The private `indexers.bin` snapshot uses MYNOUS01, a length and SHA-256 check,
atomic replacement and directory synchronization. It retains up to 1,000 sources
within 2 MiB, including removed sources; removal does not erase operator policy.
Corruption, unsupported formats, links and public file permissions fail closed
before workers. Uncertain durability requires restart. Read-only inspection of a
missing snapshot uses initial configuration in memory without creating it.

```sh
mynou indexers --config mynou.json
mynou indexer-control movies-source --action pause --config mynou.json
# Copy the returned plan_id into the explicit application.
mynou indexer-control movies-source --action pause --apply --plan-id REVIEWED_ID --config mynou.json
```

Actions are `enable`, `pause`, `reset_session` and `probe`. Protected
`POST /api/indexers/SOURCE_ID/control` takes action, optional apply and plan_id.
Every application requires a fresh whole-policy guard and the running service.
Any applied source change invalidates outstanding policy guards and active HTTP
responses for that source. Reset discards the in-memory cookie before the next
fetch; health marks it inactive immediately. Paused sources cannot be probed.

A probe preview performs no network request. Applying the review records its
revision before searching the configured source with a fixed synthetic query
and a five-second budget. It reports success only when transport and adapter
parsing succeed. It creates no download or library job. Source intervals, 429
cooldowns and busy rejection still apply; an unavailable probe does not reset
these limits. A crash after recording a probe may leave an unknown transport
outcome; no automatic replay occurs. Browser reviews bind source, action, guard
and session, expire after ten minutes and require CSRF/origin checks.
