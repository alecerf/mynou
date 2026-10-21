# Sources: indexers and IRC announcements

Mynou finds releases in three ways. **Indexers** are searched when a request
needs a release: RSS, JSON and Torznab endpoints. An RSS or Torznab indexer can
also be **watched**: Mynou polls its feed for new releases and routes them to
requests that already wait. **IRC announcement channels** push new releases as
they appear; rules decide whether to keep them for review or route them to
requests. Every candidate then goes through your [selection profiles](selection.md).
Bounds for all of them are listed in [limits](limits.md#sources).

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

### Connecting any indexer

Pick the first format your indexer offers:

1. **Torznab** (`kind: "torznab"`) is the standard search API, served directly
   by many indexers and by aggregators such as Prowlarr or Jackett, and Mynou
   speaks it natively.
2. **RSS** (`kind: "rss"`) reads any feed whose items carry a title and an
   enclosure, link or magnet.
3. **JSON** (`kind: "json"`) reads any other JSON search API once a `mapping`
   tells Mynou where things are. Without a `mapping`, Mynou expects a list (or
   `results` / `items`) of objects with `title`, a download address
   (`download_url`, `magnet`, `magnet_url`, `url` or `link`) and `seeders`, and
   sends `q`, `kind`, `year`, `season`, `episode`, `absolute` and `apikey`.

A `mapping` is a section of a `json` source. Every key is optional:

```json
{
  "name": "tracker",
  "kind": "json",
  "url": "https://indexer.example.test/api/search",
  "api_key_env": "MYNOU_INDEXER_API_KEY",
  "mapping": {
    "results": "data.torrents",
    "title": "meta.name",
    "url": ["links.magnet", "links.download"],
    "seeders": "stats.seeders",
    "params": { "query": "search", "kind": "category", "year": "", "api_key": "key" },
    "kind_values": { "movie": "2000", "episode": "5000" },
    "fixed_params": { "limit": "100" }
  }
}
```

- `results`, `title`, `url` and `seeders` are dot-separated paths of up to eight
  parts through objects; a numeric part picks an array element (`links.0`).
  `url` accepts up to four paths and uses the first that holds a text value.
  Each part is made of letters, digits, `_` or `-`, so a key with a space or a
  dot cannot be mapped. An address that starts with `/` is resolved against the
  source's origin; any other relative address is skipped, like an entry without
  a title or without a valid magnet or HTTP(S) address. `seeders` may be a
  number or numeric text (missing means 0).
- `params` renames what Mynou sends: `query`, `kind`, `year`, `season`,
  `episode`, `absolute` and `api_key` map to your parameter name, and an empty
  name leaves that parameter out. `kind_values` replaces the value sent for
  `movie`, `episode` and `series`, for indexers that expect a category.
- `fixed_params` adds up to eight constant parameters, such as a category or a
  page size. They are written in `mynou.json` in clear: never put a credential
  there. Credentials stay in environment variables (`api_key_env` and
  [authentication](#authentication)), and the API key is sent as a query
  parameter named by `params.api_key`.
- All resulting parameter names must be distinct. Unknown keys, malformed paths
  and a `mapping` on a non-`json` source stop the service from starting.
- A response that does not match the mapping is reported as a parse failure
  (`last_parse` in `mynou indexers`). `mynou indexers` never shows the mapping.
- The mapping is part of the source's identity: changing it on a source with an
  `id` needs a new `id`, as for the URL.

Not supported: HTML scraping, reading several result pages, API keys sent in a
header (use `bearer` or `basic` authentication where the indexer accepts it),
interactive or CAPTCHA logins, and scripts. An indexer that offers none of the
three formats is not reachable directly; put an aggregator that serves Torznab
in front of it. Compatibility of any particular tracker is not verified by
Mynou: check it with `mynou indexer-control SOURCE --action probe`.

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

### Watching a feed

Watching is opt-in per RSS or Torznab source and independent of IRC and of the
per-title search:

```json
{
  "id": "movies-feed",
  "name": "movies",
  "kind": "torznab",
  "url": "https://indexer.example.test/api",
  "watch": { "enabled": true, "interval_secs": 900 }
}
```

- `interval_secs` accepts 60 to 86,400 (default 900). At most 64 sources can be
  watched. Watching needs downloads enabled, and a JSON source cannot be watched.
- Each poll fetches the newest entries once: a Torznab source gets `t=search`
  without a query and an RSS source is requested as configured. The source's
  authentication, `min_interval_ms`, busy state, `429` rest and `pause` apply as
  for a search. A search that finds the source busy fails at once, and a poll
  that finds it busy tries again after ten seconds.
- The first successful poll only **baselines** the feed: what it lists is
  remembered and never acquired. Later polls consider only entries not seen
  before, whatever their order, so duplicates and reordering change nothing. The
  cursor and a window of the last 1,024 entry identities are saved in a private
  checksummed `feeds.bin` **before** a poll is routed or reported as successful;
  a failed save reports `storage_failed` and the same entries are seen again.
  Restarting keeps the window and replays nothing. The file holds opaque
  digests, never addresses, titles or credentials.
- A source that is paused, or whose `watch` is disabled, forgets its window:
  resuming baselines it again instead of acquiring what it published meanwhile.
  So does a source that was not polled successfully for seven days.
- New entries go **only to demand that already exists**, never to a new request:
  queued or retrying movie and episode jobs that no worker has leased and that
  have no selected release, which includes watchlist requests, requester
  demand and monitored missing episodes; and, when [monitoring](library.md#background-monitoring) is
  enabled, owned monitored media whose profile has not reached its cutoff. An
  entry must pass the same title, year, episode, profile and minimum-seeder
  gates as a search, and an upgrade must rank strictly higher than the recorded
  release. The best entry wins, a job takes at most one release, and the job's
  admitted requester demand and captured profile are checked again where the
  job changes. A job that a worker, an IRC announcement or another poll already
  selected is never replaced. A configured Plex library is asked first, and
  media already present is left to the normal availability check.
- Per poll, at most 500 entries are read, 200 unseen ones are considered
  (the rest stay new for the next poll) and 32 jobs and 32 upgrades are routed.

```sh
mynou feeds --config ./mynou.json
```

`mynou feeds` and `GET /api/feeds` report, for every RSS or Torznab source,
whether it is watched and active, whether it has a baseline, when it last
succeeded, how many polls, failures and new entries it saw, the last outcome
(`ok`, `busy`, `disabled`, `rate_limited`, `authentication_failed`,
`response_rejected`, `network_failed`, `parse_rejected`, `storage_failed`,
`timed_out` or `request_failed`), the delay before the next poll and any `429`
rest. Last-outcome counters reset when the service restarts. Reports never show
URLs, titles, entries or credentials.

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

#### Bracketed announcements resolved by lookup

Some trackers announce only a release name and the ID of its page:

```text
[NEW] [Films] [Example.Movie.2024.MULTi.1080p.WEB.x264-GROUP] [WEB-DL] [8.71 GiB] [0%] [pre 0h 1m 52s] par uploader -> https://tracker.example/torrents/123456
```

The `bracketed` format reads that grammar. The first group can be any tag (`NEW`,
`EXCLUSiVE`…) and the `[pre …]` group is optional:

```json
"announcement_format": {
  "type": "bracketed",
  "lookup_indexer": "tracker",
  "details_prefix": "https://tracker.example/torrents/",
  "categories": {"Films": "movie", "Series": "episode"}
}
```

- `lookup_indexer` is the `id` of a configured `torznab` indexer for the same
  tracker, directly or through an aggregator such as Prowlarr.
- `details_prefix` is the HTTPS address of the tracker's release pages, ending
  with `/`. The announced address must be exactly this prefix followed by a
  decimal ID.
- `categories` maps the announced category labels (1 to 16) to `movie` or
  `episode`. Any other category is rejected.

A line is first only an **unverified candidate** held in memory. Mynou then
searches the indexer for the release name and keeps the single item whose
`comments` (details page) is the announced address. That item must carry one
`tmdbid` and one `infohash`, either 40 or 64 hexadecimal digits or their
hexadecimal ASCII encoding, as some aggregators return them. The
TMDB catalog (`catalog` in `mynou.json`) then supplies the canonical title and year. Only then does
the announcement enter the history, with the same duplicate detection, rules,
profiles, admitted demand and metadata checks as any other. GUIDs and
download links, which carry a passkey, are never read.

Episodes need exactly one `S01E05` marker. Season packs, multi-episode labels
and bare `E12` numbers are rejected, as are unknown details pages. When the item is
not listed yet, or the indexer or catalog fails, the candidate retries up to
three times, 30 seconds apart. A restart drops unresolved candidates instead
of replaying them. `GET /api/irc/sources` reports the queue under `lookup`,
and a preview only parses the candidate, without any lookup.

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
torrent and never searches. An announcement that cannot be routed yet (no
waiting job, unavailable metadata) is retried after 30 seconds without holding
up the others; `GET /api/irc` reports `routing.retry_entries` against
`retry_limit` (200) and `retry_dropped`, the backoffs shortened while full.
Catalog IDs in announcements remain claims: a matching hash and labels do not
prove which film or episode the video shows.

A request review checks the account's current Plex identity and fresh TMDB
details (complete ID, title, year and release dates; episodes also need an
episode ID and an air date) within ten seconds. Ambiguous or mismatched facts
stay unresolved. The request then follows the account's normal admission rules
and waits for a matching announcement to download.

### Review announcements

History is stored on disk without a total identity limit. Follow `next_cursor`
with `mynou announcements --after CURSOR --limit 50` or the API `after` parameter
for later pages. Each page holds at most 200 rows; `--offset` remains available
up to 1,000 rows. See [migration and downgrade](inventory-storage.md#irc-announcement-storage).

```sh
mynou irc --config ./mynou.json
mynou announcements --limit 50 --config ./mynou.json
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
| `GET /api/irc/announcements?limit=50&after=CURSOR` | Announcement history (`next_cursor`; omit `after` for the first page) |
| `GET /api/irc/announcements/ID` | One announcement |
| `POST /api/irc/preview` | Parse a sample announcement |
| `POST /api/irc/announcements/ID/control` | `acknowledge`, `dismiss` or `request`; add `"apply": true` and `"plan_id"` to apply |

Reports show authentication state (for example
`health.sasl_authenticated` or `health.nickserv_authenticated`) but never
credentials, templates, magnets or raw server text.

A malformed announcement from the configured sender is skipped without closing
the connection: `health.rejected` counts them and the message text is never kept
or logged. Authentication, TLS, framing, size-limit and storage failures still
end the connection and trigger a reconnect.
