# IRC reception and announcement reviews

Mynou 0.20.0 adds opt-in IRC receivers, deterministic filters, durable duplicate
suppression and guarded reviews. A review acknowledges or dismisses a release
claim without submitting a download job. Automatic acquisition follows after
metadata, canonical identity, requester approval and ownership are bound together.
This implementation requires its own completed CI and publication.

## Configure a source and review rules

Add this optional section to your existing schema-1 configuration:

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

Sources and rules default to disabled. Source IDs bind endpoint, nickname,
channel, exact sender prefix and credential variable names. Credential rotation
in the same variable retains the binding; a changed binding requires a new ID.
Retired IDs remain reserved. Configuration edits require a service restart.
Management reports display source health and rule settings without credentials.

The original TLS 1.3 client verifies certificates and the configured hostname.
Endpoints require an explicit port and no path or embedded credentials.
Plaintext IRC is limited to explicit loopback IP addresses for local fixtures.
Connections are outbound; no IRC port needs publishing from Docker. The existing
Compose env_file supplies configured credential variables from a private .env.
Server PASS and channel keys are supported; SASL, NickServ workflows, interactive
authentication and tracker-specific text adapters remain later work.

Nicknames contain 1–24 ASCII characters starting with a letter; subsequent
characters are letters, digits, hyphens or underscores. Channels contain # and
1–63 letters, digits, hyphens or underscores. Sender prefixes require the exact
configured nick!user@host form. Channel/nickname comparisons use ASCII case
folding; sender matching is exact.

## Explicit release claims

After registration and channel membership, the allowed sender delivers PRIVMSG
or NOTICE to the configured channel. Its trailing parameter starts with MYNOU,
a space and this strict JSON envelope:

    {
      "title": "Example.Movie.2024.1080p.BluRay.x264",
      "kind": "movie",
      "media_title": "Example Movie",
      "year": 2024,
      "tmdb_id": "123456",
      "info_hash": "0123456789abcdef0123456789abcdef01234567"
    }

Episodes use kind "episode", an explicit season and a positive episode number.
TMDB IDs are positive canonical decimal strings. Hash claims contain 40 hex
characters for v1 or 64 for v2, normalized to lowercase. Unknown fields,
acquisition URLs, controls, missing identities and ambiguous kinds are rejected.
No ranges, title-only identities or source numbering are inferred. These remain
unverified claims: trusted reception does not authenticate metadata or catalog.

Rules run independently in stable ID order. Required/blocked terms use
case-insensitive ASCII substrings, followed by the named profile's title
assessment. Title markers remain hints rather than verified media tracks.
Each rule records disabled, kind_mismatch, blocked, required_missing,
profile_rejected or matched. One matching rule yields matched, none unmatched
and multiple matches conflict. Recorded claims start pending.

## Management and guarded decisions

The browser's Announcements page lists source health, rules and bounded history.
Inspect a claim, review acknowledgement/dismissal and apply that exact review.
The private session review binds announcement, action and guard for ten minutes.
Another review replaces it. Logout, restart, expiry, substitution and stale
decisions require a new review.

    mynou irc --config mynou.json
    mynou announcements --offset 0 --limit 50 --config mynou.json
    mynou announcement ANNOUNCEMENT_ID --config mynou.json
    mynou irc-preview SOURCE_ID --announcement claim.json --config mynou.json
    mynou irc-control ANNOUNCEMENT_ID --action dismiss --config mynou.json
    mynou irc-control ANNOUNCEMENT_ID --action dismiss --apply --plan-id REVIEW_ID --config mynou.json

Explicit JSON preview is pure: no storage, service or network operation. Its file
is bounded to 8 KiB. Listing/control preview can read existing offline storage
without writes, repair or permission changes. Apply requires the service.
API routes retain operator Bearer authentication:

- GET /api/irc exposes sources, health and rules.
- GET /api/irc/announcements?offset=0&limit=50 lists at most 200 claims.
- GET /api/irc/announcements/ID exposes one claim.
- POST /api/irc/preview accepts source_id and announcement in an 8 KiB body.
- POST /api/irc/announcements/ID/control accepts action acknowledge or dismiss.
  Preview omits apply/plan_id; apply adds apply:true and the returned plan_id.

Strict guards bind original claim/evaluations, source binding, row revision,
action and current rules/profiles. Duplicate or unrelated receipts do not
invalidate a current review. Acknowledged/dismissed records are terminal;
replay never revives them. Reviews change no job, requester quota or imported file.

## Bounds, recovery and connections

Limits are eight configured sources, 32 retained source IDs and 64 rules, each
with at most 16 required and 16 blocked terms of 128 bytes. The private checked
8 MiB announcements.bin snapshot retains 1,000 identities. Full history rejects
new identities while suppressing known duplicates; no pruning discards authority.
Identity combines source binding, canonical movie/episode claim and torrent hash.
The first claim and evaluations survive restart. Changed content with the same
identity returns duplicate/claim_changed while preserving the original decision.
Duplicate receipts do not rewrite storage. Receipt/duplicate counters and source
health are in-memory observations that reset at process restart.

Readers verify MYNOUI01, length, SHA-256 and semantic provenance. Writes use
private exclusive temporary files, file sync, atomic rename and directory sync.
Uncertain final sync blocks writes until restart. Corruption, symlinks,
nonregular or multiply linked files fail before receivers/transfers start.
Existing job formats and imports keep their earlier behavior.

Each source has one receiver; one shared monitor interrupts sockets on shutdown.
Connect/registration has a ten-second overall deadline. Idle timeout accepts
30–600 seconds; a partial line has a ten-second assembly deadline. Bounds are
8,192 bytes per CRLF line, 4,096 per read, 128 parsed messages per batch and
256 messages per source per second. Invalid framing/tags, TLS, registration or
membership loss closes the connection. Backoff doubles inside configured bounds,
resetting after a connection lasting 30 seconds. Server error text is not echoed.
Standard-library DNS is synchronous; late results are rejected, but resolution
cannot itself be interrupted.

Original CI uses synthetic loopback IRC services to exercise boundaries, trust,
filtering, history limits/corruption, concurrent and stale reviews, restart,
PING/PONG, reconnect and shutdown. No public IRC or torrents are contacted.
