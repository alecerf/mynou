# Native indexers and authentication

Mynou 0.21.0 keeps its original JSON, RSS and Torznab adapters and release
selection. The authentication/session/health source awaits its own complete CI.
Configure an endpoint that directly serves that adapter; fetches reject redirects.
Existing sources default to enabled, without authentication or interval limits.
Existing API-key query configuration remains supported.

```json
{
  "indexers": [
    {
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
Offline inspection reads configuration alone; it does not restore live sessions
or imply a successful network check. Persistent reviewed source controls follow
in the next focused increment.
