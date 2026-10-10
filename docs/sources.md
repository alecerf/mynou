# Sources: indexers and IRC announcements

Mynou finds releases in two kinds of sources. **Indexers** are searched when a
request needs a release: RSS, JSON and Torznab endpoints. **IRC announcement
channels** push new releases as they appear; rules decide whether to keep them
for review or route them to requests. Every candidate then goes through your
[selection profiles](selection.md). Bounds for both are listed in
[limits](limits.md#sources).

## Indexers

Add each source to the `indexers` array of `mynou.json`:

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

- `kind` is `rss`, `json` or `torznab` (default `json`). The URL must serve that
  format directly: redirects are refused.
- `api_key_env` names the variable holding an API key (default
  `MYNOU_INDEXER_API_KEY`); it is used only when the variable is set.
- `enabled` defaults to `true`. `min_interval_ms` (0 to 60,000, default 0)
  spaces out requests to this source.
- `id` is an optional stable identifier of 1 to 64 lowercase letters, digits,
  underscores or hyphens. It binds the name, kind, URL, API key variable,
  interval and authentication: changing any of them needs a new ID.
- Credential values always come from environment variables, never from
  `mynou.json`.

### Authentication

`authentication.method` is one of:

- `none` (the default).
- `bearer`, with `token_env`.
- `basic`, with `username_env` and `password_env`. Usernames cannot contain a
  colon.
- `form`, with `login_url`, `username_env`, `password_env`, `username_field`,
  `password_field`, `cookie_name` and optional `max_age_secs` (1 to 3,600,
  default 1,800). Mynou posts the two fields directly to `login_url` on the same
  origin, without a query or fragment, and expects a 2xx response that sets
  exactly the named cookie. Redirects, HTML form discovery, CSRF or CAPTCHA
  challenges and interactive logins are not supported.

Remote authentication requires verified HTTPS; plain HTTP is accepted only for
loopback addresses. Session cookies stay in memory and are never stored or
displayed. An expired session is renewed before the next fetch; a `401`
triggers one renewal, and a second `401` fails the request. There is never an
unauthenticated fallback.

### Health, pause and probe

```sh
mynou indexers --config ./mynou.json
mynou indexer-control movies-source --action pause --config ./mynou.json
mynou indexer-control movies-source --action pause --apply \
  --plan-id PLAN_ID --config ./mynou.json
```

`mynou indexers` and `GET /api/indexers` show
each source's alias, format, enabled and authentication mode, request counters,
last diagnostic, HTTP status, parse result and whether a session exists. They
never show URLs, variable names or values, response bodies, headers or cookies.

`indexer-control` previews an action and returns a `plan_id`; applying it
requires that plan and the running service. Actions are `enable`, `pause`,
`reset_session` (forget the cookie) and `probe` (search the source once with a
fixed synthetic query, within five seconds, downloading nothing). Paused
sources are never searched or probed. The API equivalent is
`POST /api/indexers/SOURCE_ID/control` with `action` and, to apply,
`"apply": true` and `"plan_id"`. The enabled state is read from the configuration
once and then kept in a private `indexers.bin` file, so a later `enabled`
change in `mynou.json` does not override a pause.

A source that answers `429` is rested for its `Retry-After` delay (60 seconds
when absent, clamped to 1 to 3,600) and is never retried immediately. A second
request to a busy source fails at once instead of waiting; other sources are
unaffected.

## IRC announcements

IRC reception is opt-in. Each source connects out to one channel and accepts
announcements from one exact sender. No inbound port needs to be opened.

```json
{
  "irc": {
    "sources": [{
      "id": "releases",
      "enabled": true,
      "url": "ircs://irc.example.test:6697",
      "nickname": "mynou",
      "channel": "#announces",
      "sender": "announce!bot@tracker.example.test",
      "password_env": "MYNOU_IRC_PASSWORD",
      "join_key_env": "MYNOU_IRC_CHANNEL_KEY",
      "idle_timeout_secs": 180,
      "reconnect_min_secs": 1,
      "reconnect_max_secs": 60
    }],
    "rules": [{
      "id": "movies",
      "source": "releases",
      "enabled": true,
      "kind": "movie",
      "profile": "any",
      "required_terms": ["bluray"],
      "blocked_terms": ["cam"],
      "action": "review"
    }]
  }
}
```

Sources and rules are disabled unless `enabled` is `true`. Restart the service
after any change.

- `url` is `ircs://host:port` with an explicit port, no path and no embedded
  credentials. Mynou verifies the certificate and host name. Plain `irc://` is
  accepted only for loopback IP addresses.
- `sender` is the exact `nick!user@host` prefix allowed to announce. Channel and
  nickname comparisons ignore ASCII case; the sender must match exactly.
- `password_env` (server password) and `join_key_env` (channel key) are optional.
- `idle_timeout_secs` accepts 30 to 600. Reconnection backs off from
  `reconnect_min_secs` (1 to 30) to `reconnect_max_secs` (1 to 300), doubling
  each time and resetting after a connection lasts 30 seconds.
- A source `id` binds the endpoint, nickname, channel, sender, credential
  variable names, authentication and announcement format. Rotating a credential
  value keeps the binding; any other change needs a new ID, and retired IDs stay
  reserved.

### SASL and NickServ

A source can require SASL PLAIN or NickServ identification, not both. Neither
has an unauthenticated fallback: a failure closes the connection, and every
reconnection authenticates again. Remote use requires the verified TLS
connection.

SASL PLAIN authenticates before Mynou joins the channel:

```json
"sasl": {
  "mechanism": "PLAIN",
  "username_env": "MYNOU_IRC_SASL_USERNAME",
  "password_env": "MYNOU_IRC_SASL_PASSWORD",
  "authorization_env": "MYNOU_IRC_SASL_AUTHORIZATION"
}
```

`authorization_env` is optional; omit it to use the account of the
authentication identity. Values are sent as UTF-8 bytes without normalization.

NickServ identification gates the channel join on an exact confirmation notice.
Because services differ between networks, every detail is explicit:

```json
"nickserv": {
  "account": "myAccount",
  "password_env": "MYNOU_IRC_SERVICE_PASSWORD",
  "service": "NickServ",
  "sender": "NickServ!service@services.example.test",
  "success_notice": "You are now identified as {account}",
  "failure_notices": ["Invalid password", "Account unavailable"]
}
```

After the server's welcome, Mynou sends `IDENTIFY account password` to the
service and joins only after a notice from that exact sender matches
`success_notice` exactly, with `{account}` (which must appear once) replaced by
the account. A configured failure notice closes the connection, even after
success.

### Announcement formats

By default an announcement is a `PRIVMSG` or `NOTICE` to the channel whose text
is `MYNOU` followed by a space and a strict JSON object:

```json
{
  "title": "Example.Movie.2024.1080p.BluRay.x264",
  "kind": "movie",
  "media_title": "Example Movie",
  "year": 2024,
  "tmdb_id": "123456",
  "info_hash": "0123456789abcdef0123456789abcdef01234567"
}
```

Episodes use `"kind": "episode"` with `season` and a positive `episode`.
`tmdb_id` is a positive decimal string and `info_hash` has 40 (v1) or 64 (v2)
hexadecimal characters. Unknown fields, URLs, control characters, missing
identities and ambiguous kinds are rejected. Nothing is inferred from titles.

A source can instead describe a fixed-delimiter text format:

```json
"announcement_format": {
  "type": "delimited",
  "prefix": "NEW | ",
  "separator": " | ",
  "suffix": " END",
  "fields": ["title", "kind", "media_title", "year", "tmdb_id", "info_hash"]
}
```

which accepts, for example:

```text
NEW | Example.Movie.2024.1080p | movie | Example Movie | 2024 | 123456 | 0123456789abcdef0123456789abcdef01234567 END
```

The six fields above are mandatory; episode formats add `season` and `episode`
in any order (movies use zero when those fields exist). Values follow the JSON
rules, and IRC color and formatting codes are removed first. Announcements
that lack a catalog ID or a hash cannot be described this way. Omit the format,
or use `{"type": "json"}`, for the default.

Test a format without storage, network or acquisition:

```sh
mynou irc-preview SOURCE_ID --text announcement.txt --config ./mynou.json
mynou irc-preview SOURCE_ID --announcement claim.json --config ./mynou.json
```

The text file holds the message body without line endings. The API equivalent
is `POST /api/irc/preview` with `{"source_id": "...", "text": "..."}` or an
`announcement` object (one of the two).

### Rules and actions

Every rule runs independently, in ID order. Required and blocked terms are
case-insensitive substrings of the release title; then the named profile judges
the title. Each rule records `disabled`, `kind_mismatch`, `blocked`,
`required_missing`, `profile_rejected` or `matched`. One matching rule makes the
announcement `matched`, none makes it `unmatched` and several make it a
conflict. The first claim for an announcement is kept: duplicates never rewrite
history.

- **`review`** (default) keeps matched announcements for you to acknowledge or
  dismiss. This never creates download work.
- **`grab`** routes announced torrents to requests that are already waiting.
  Add a `magnet_template` to the source, with `{xt}` exactly once, for example
  `magnet:?xt={xt}&tr=https%3A%2F%2Ftracker.example.test%2Fannounce`. `{xt}`
  becomes `urn:btih:` with the 40-character hash, or `urn:btmh:1220` with the
  64-character v2 hash. Downloads must be enabled. While an enabled grab rule exists, queued movie and episode
  jobs with a catalog ID and the same profile wait for an announcement instead
  of searching. Explicit
  `--url` or `--path` requests, running acquisitions, upgrades, packs and shared
  videos keep their normal scheduling, and disabling the rule or source restores
  it. No announcement ever creates a request on its own.
- **`request`** creates demand for one configured
  [Plex requester account](requesters.md) after you review an announcement. The
  rule must name that account in `requester`, and the source needs a
  `magnet_template`.

Grab and request rules can name a `requester` to serve only that account's
demand. Before routing, Mynou checks Plex availability (media already present
under the captured destination fulfills the request), then downloads only the
torrent metadata and authenticates the announced hash. The metadata must
contain exactly one video whose title, year and episode labels match the
waiting job; packs, ranges and several videos are rejected. The job then
records the magnet and that file, and downloads only it. A retry reuses the same
torrent and never searches. Catalog IDs in announcements remain claims: a
matching hash and labels do not prove which film or episode the video shows.

A request review checks the account's current Plex identity and fresh TMDB
details (complete ID, title, year and release dates; episodes also need an
episode ID and an air date) within ten seconds. Ambiguous or mismatched facts
stay unresolved. The request then follows the account's normal admission rules
and waits for a matching announcement to download.

### Review announcements

```sh
mynou irc --config ./mynou.json
mynou announcements --offset 0 --limit 50 --config ./mynou.json
mynou announcement ANNOUNCEMENT_ID --config ./mynou.json
mynou irc-control ANNOUNCEMENT_ID --action dismiss --config ./mynou.json
mynou irc-control ANNOUNCEMENT_ID --action dismiss --apply \
  --plan-id PLAN_ID --config ./mynou.json
```

Actions are `acknowledge`, `dismiss` and `request`. Each is previewed first; the
apply step needs the returned `plan_id` and the running service, and a changed
announcement, rule or profile requires a new preview. Acknowledged and dismissed
announcements are final and are no longer routed; to stop a routed download,
cancel its job. Listing and previews work offline without writing.

| Route (Bearer token) | Purpose |
| --- | --- |
| `GET /api/irc` | Sources, health and rules |
| `GET /api/irc/announcements?offset=0&limit=50` | Announcement history |
| `GET /api/irc/announcements/ID` | One announcement |
| `POST /api/irc/preview` | Parse a sample announcement |
| `POST /api/irc/announcements/ID/control` | `acknowledge`, `dismiss` or `request`; add `"apply": true` and `"plan_id"` to apply |

Reports show authentication state (for example
`health.sasl_authenticated` or `health.nickserv_authenticated`) but never
credentials, templates, magnets or raw server text.
