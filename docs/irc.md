# IRC reception, reviews, requester demand and verified routing

Mynou 0.20.0 adds opt-in receivers, deterministic filters, durable duplicate
suppression and guarded audit reviews. The 0.20.1 increment adds explicit grab
rules: verified candidates attach only to existing admitted canonical jobs.
Acknowledgement/dismissal change no download work. Reception and routing passed complete CI.
The 0.20.2 increment adds required SASL PLAIN authentication and passed complete
CI/publication with 543 Rust tests. Exact source/run/asset evidence is recorded in
[validation](validation.md#recorded-0202-ci-evidence). Later commits require their
own completed workflow.

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
Server PASS, channel keys, required SASL PLAIN and fixed-delimiter text formats
are supported. The 0.20.4 increment adds required NickServ identification.
Interactive authentication and a broad provider adapter catalog remain later work.

Nicknames contain 1–24 ASCII characters starting with a letter; subsequent
characters are letters, digits, hyphens or underscores. Channels contain # and
1–63 letters, digits, hyphens or underscores. Sender prefixes require the exact
configured nick!user@host form. Channel/nickname comparisons use ASCII case
folding; sender matching is exact.

## Required SASL PLAIN authentication

Add an optional sasl object to an IRC source. Omitting it or setting it to null
retains the earlier registration behavior and source binding.

    "sasl": {
      "mechanism": "PLAIN",
      "username_env": "MYNOU_IRC_SASL_USERNAME",
      "password_env": "MYNOU_IRC_SASL_PASSWORD",
      "authorization_env": "MYNOU_IRC_SASL_AUTHORIZATION"
    }

Username and password variables are mandatory; authorization_env is optional.
Omit authorization_env to request the authentication identity's usual account.
Variable names follow the existing uppercase credential-name rules. Values are
loaded before connecting, must contain 1–256 UTF-8 bytes without control
characters, and are sent without Unicode normalization. Optional authorization
is empty only when no variable is configured. Unknown fields and mechanisms are
rejected. Supply credentials through Docker's private .env; do not put their
values in mynou.json. Server password_env remains an independent IRC PASS secret.

The client sends CAP LS 302 before NICK/USER, collects up to 16 capability lines,
64 unique capabilities and 4,096 aggregate list bytes, then requests sasl.
A bare sasl advertisement supports the earlier SASL protocol; a mechanism list
must explicitly include PLAIN. Duplicate/ambiguous capabilities are rejected.
ACK precedes AUTHENTICATE PLAIN. Only an empty server challenge triggers the
bounded Base64 response: authorization, NUL, username, NUL, password. Encoded
chunks contain at most 400 bytes; an exact 400-byte final chunk requires a
separate AUTHENTICATE + terminator. The 1,028-byte maximum response uses at most
three payload chunks. Only a correctly addressed success numeric after the
response permits CAP END, welcome and channel membership.

PING replies remain available during negotiation. Capability removal, logout,
rejection, abort, unsupported commands, repeated challenges and premature
welcome/success invalidate the connection. There is no unauthenticated fallback.
The existing ten-second connect/registration deadline covers the entire exchange;
incoming traffic cannot renew it. Every reconnect loads credentials and performs
the complete authentication again. Remote PLAIN credentials require the existing
verified TLS transport; plaintext remains limited to explicit loopback IPs.

Credential values and encoded responses stay out of errors, reports, bindings
and persistent history. Public source reports show authentication sasl_plain or
none and health.sasl_authenticated, which resets on disconnect/shutdown. The
browser shows required/authenticated state. Authentication variable names and
policy contribute to the source binding; policy edits require a new stable
source ID. Rotating a value in the same variable retains that binding.

## Required NickServ identification in 0.20.4

Configure either SASL or NickServ for a source. Their omission or null value
retains the original binding; selecting both is rejected. NickServ settings are
explicit because service identities and replies vary between networks:

```json
{
  "nickserv": {
    "account": "myAccount",
    "password_env": "MYNOU_IRC_SERVICE_PASSWORD",
    "service": "NickServ",
    "sender": "NickServ!service@services.example.test",
    "success_notice": "You are now identified as {account}",
    "failure_notices": ["Invalid password", "Account unavailable"]
  }
}
```

Use the network's exact full success and failure notices. Success contains
exactly one {account}, expanded to the configured account; substring matches and
wrong accounts never grant access. The sender must match the exact configured
service prefix and service nickname, and the command must be NOTICE addressed
to Mynou. Bounded display formatting is removed before comparison. Unrecognized
notices cannot renew the overall ten-second registration deadline.

After the server welcome, Mynou sends one PRIVMSG service :IDENTIFY account
password. It sends JOIN only after confirmation, and ignores premature own JOIN
or numeric channel membership. A configured failure notice invalidates the
connection even after successful identification. Reconnect reloads credentials
and repeats the exchange; there is no unauthenticated fallback. Verified TLS is
mandatory for remote connections. Missing credentials fail before opening a socket.

Account names contain 1–64 ASCII letters, digits, hyphens or underscores.
The service follows nickname bounds; the complete sender is at most 128 bytes.
Success and 1–8 unique failure notices are at most 256 bytes without controls.
Password values contain 1–256 printable nonspace ASCII bytes and are transient.
Public reports expose authentication nickserv and health.nickserv_authenticated;
the latter resets on disconnect and shutdown. Account/notice settings and variable
names appear only in private configuration and its hashed source binding. They
and credential values never appear in public source reports or diagnostics.
Authentication policy changes require a new source ID.

## Requester selectors in 0.20.5

An optional rule requester names one configured requester account. Omitting it
or setting it to null preserves the original rule serialization and pending
fingerprints. Unknown or invalid aliases are rejected during configuration.
Example addition to a grab rule:

```json
{"requester":"alice"}
```

The selected account must retain admitted demand for that canonical identity
with the job's same captured profile and destination. Compatible demand can
share a job originally created by another account. An operator job, an unrelated
account, unadmitted demand or a conflicting route cannot stand in for that
selection. Changing a selector makes pending claims/reviews
stale without reinterpreting their original evaluations.

Waiting and final routing use the same selection predicate. Routing rechecks
the selected interest after metadata I/O and immediately before reservation;
removing a selected co-owner cannot borrow another account's admission. The other
account's job and ready media remain intact. Public source/API/browser reports
show only the stable requester alias. Selection does not create new demand.
Reviewed demand is a separate request action in the following increment.

## Reviewed requester demand in 0.20.6

Set a rule's action to request and select an explicit configured requester:

    {
      "id": "movie-requests",
      "source": "releases",
      "enabled": true,
      "kind": "movie",
      "profile": "any",
      "action": "request",
      "requester": "home"
    }

The source needs a pinned magnet_template, and home must be a configured account.
One current rule must match; receipt alone creates no demand. Review through the
browser or the existing protected control endpoint. CLI commands are:

    mynou irc-control ANNOUNCEMENT_ID --action request --config mynou.json
    mynou irc-control ANNOUNCEMENT_ID --action request --apply --plan-id REVIEW_ID --config mynou.json

Request preview verifies the selected account's current Plex user identity and
fresh TMDB details under one ten-second budget. Complete catalog ID/title/year
and released dates are required. Episode facts need a stable episode ID and an
aired date; retained canonical/source numbering is checked without changing a
series plan. Multiple retained series identities, future/missing/mismatched facts
and incompatible source labels remain unresolved. HTTP calls occur outside
persistent locks. Synchronous DNS can exceed the budget; late results are rejected.
Offline preview reads private state without writes. Apply requires the service.
Pure irc-preview and acknowledgement/dismissal retain their existing behavior.

The request guard binds the first claim, configuration, full requester/job scope,
captured profile/route and retained numbering. Apply rechecks this scope under
series, IRC, requester and job locks. An IRC intent is written before the
canonical explicit origin. Ordinary requester admission retains the account
opt-in and compatible sharing; existing unready operator work cannot stand in
for a captured request. Existing compatible demand retains its job and original
capture. Removed identities remain tombstones.

Successful empty Plex polls preserve irc:SOURCE:ANNOUNCEMENT origins. Admitted
request work waits for a matching explicit-origin candidate; routing retains
hash-authenticated metadata, exact file/profile/source labels and physical
ownership before native publication. A disabled account cannot be bypassed. A
reviewed requester policy change captures the current policy for unadmitted
demand. Identity/transport failures expose fixed errors,
and public admission reports contain stable aliases and demand status.

Before native startup, recovery validates both directions between intents and
requester origins. A durable origin completes a prepared intent; an intent whose
origin was not written aborts without replay. Committed/aborted decisions remain
terminal. Canonical request and source numbering remain stored in both proof and
demand. New semantics require requester format 2 and IRC format 3; older IRC
history remains readable when it contains no new semantics. Requester snapshots
saved by earlier versions hold removed notification records and no longer open.
Earlier binaries cannot read these new formats safely. Preserve private backups
before a downgrade.

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

## Configurable text announcements in 0.20.3

An optional announcement_format on a source describes one strict text grammar:

    "announcement_format": {
      "type": "delimited",
      "prefix": "NEW | ",
      "separator": " | ",
      "suffix": " END",
      "fields": ["title", "kind", "media_title", "year", "tmdb_id", "info_hash"]
    }

The matching body has exactly the configured number of fields:

    NEW | Example.Movie.2024.1080p | movie | Example Movie | 2024 | 123456 | 0123456789abcdef0123456789abcdef01234567 END

The six identity fields above are mandatory. Episode formats add both season
and episode, in any configured order. A movie uses zero season/episode when
those fields exist. Values are nonempty without leading/trailing whitespace;
numbers are canonical decimal strings, and catalog/hash validation is identical
to JSON claims. No title-only identity, range or download URL is inferred.
Missing/extra/ambiguous fields and URL/passkey/token markers are rejected before
history or acquisition. This configurable adapter does not claim support for
providers whose announcements lack these identities.

Prefix/suffix use at most 128 ASCII bytes each; prefix is nonempty. Separators
use 1–16 ASCII bytes and contain punctuation. Fields are unique and limited to
the eight documented names. Unknown configuration fields, pattern engines and
URL fields are rejected. The existing 8,192-byte wire bound still applies.
Recognized IRC bold, colour/reset, monospace, reverse, italic, strike and
underline controls are removed from text bodies, including bounded decimal/hex
colour arguments. Formatting is permitted only in NOTICE/PRIVMSG body parameters;
headers, tags, targets and PING tokens retain strict control rejection.

Omission, null or {"type":"json"} retains the original MYNOU JSON format,
source binding and pending-claim fingerprint. Text format configuration enters
the source binding; changes require a new stable source ID. Public reports show
only json/delimited, with no private grammar or raw server text. Valid text uses
the original first-claim identity, filters, review guards, reservations and
admitted-job routing. Duplicates never rewrite their original claim.

Pure previews accept exactly one input form:

    mynou irc-preview SOURCE_ID --text announcement.txt --config mynou.json

The file contains the body without CRLF framing. POST /api/irc/preview accepts
{"source_id":"SOURCE_ID","text":"BODY"} under Bearer authentication; its
existing announcement object remains supported. Passing both forms is rejected.
Neither preview opens storage, contacts IRC or acquires media.

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
- POST /api/irc/announcements/ID/control accepts acknowledge, dismiss or request.
  Preview omits apply/plan_id; apply adds apply:true and the returned plan_id.

Strict guards bind original claim/evaluations, source binding, row revision,
action and current rules/profiles. Duplicate or unrelated receipts do not
invalidate a current review. Acknowledged/dismissed records are terminal;
replay never revives them. Acknowledgement/dismissal change no job, requester
demand or imported file. Request review uses the separate full-scope contract above.

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

## Explicit grab rules

To enable automatic candidate routing, add magnet_template to the source and
change the selected rule's action to grab. Both source and rule must be enabled,
native downloads must be enabled, and the rule profile must match the job's
admitted profile including its frozen definition.

    "magnet_template": "magnet:?xt={xt}&tr=https%3A%2F%2Ftracker.example.test%2Fannounce"

The {xt} placeholder occurs exactly once and expands to urn:btih: followed by
the 40-character hash or urn:btmh:1220 followed by the 64-character v2 hash.
Unknown parameters and duplicate identities are rejected. Discovery permits
four unique trackers and eight unique literal-IP x.pe peers. HTTPS and UDP
trackers are supported; plaintext HTTP trackers require literal loopback.
Embedded URL credentials, hostnames in x.pe, zero ports, unspecified/multicast
peers and malformed escapes are rejected without network I/O during parsing.
Private tracker paths/query credentials stay in the configuration and private
origin records; public reports expose no template or magnet.

An enabled grab rule holds otherwise untouched queued movie/episode jobs whose
admitted profile agrees. Explicit local/URL requests, existing acquisitions,
upgrades, packs/shared owners and jobs without catalog IDs retain their ordinary
scheduling. Disabling the rule/source restores scheduling at the next due attempt. Review rules hold
no jobs. No unsolicited announcement creates demand.

Plex availability remains active for held jobs, with negative checks deferred
for 60 seconds. Routing also checks availability before choosing a torrent;
existing playable media under the captured destination fulfills demand without
acquisition. A routed candidate resets the waiting job's next attempt to zero.

One background pass attempts one candidate, sharing a ten-second availability
and metadata budget.
Rules must have one current match and the original configuration fingerprint.
Grab uses an unreviewed claim; request uses its committed reviewed demand.
Canonical media identity, normalized title, year and exact source numbering must
match one admitted job. Routing uses existing durable admission and cannot
bypass a disabled account. Only an explicitly reviewed request action can create
demand; ordinary requester machinery performs admission.
Claimed catalog IDs remain claims; agreement with admitted labels does not prove
semantic catalog identity from torrent bytes.

Metadata-only discovery authenticates the pinned hash without payload or a
native transfer queue. Exactly one nonempty supported video must have matching
title/year and source labels. Multiple videos, episode ranges, packs, mismatched
labels and ambiguous paths are rejected. Metadata supplies v1/v2 aliases for
hybrid ownership checks. Admission rechecks the claim, job, admitted interests,
captured profile and retained physical ownership after I/O and under the
IRC/requester/job lock order.

A private reservation is synchronized before one journal transaction attaches
the immutable magnet, aliases, exact path/length and release/profile. Native
acquisition authenticates the same identity again and selects only that file's
pieces, including boundaries. Imports retain canonical episode numbering and
captured destinations. Retry retains the selected origin; it never starts a new
search. Source/template/profile changes do not rebind existing origins.

Public history exposes routing_outcome, candidate_routed and the associated job.
Outcomes include routed, already_available, availability_unavailable,
waiting_for_admitted_job, admission_required,
claim_mismatch, profile_mismatch, metadata_unavailable, metadata_rejected,
admission_changed and admission_or_storage_rejected. Attempt outcomes are
bounded in-memory observations; durable reservation phases are reserved, routed
and aborted. A failed candidate has a 30-second cooldown; the cursor lets other
claims proceed. The worker checks once per second. Restart resets attempts.
Acknowledgement/dismissal prevent routing an untouched claim; after routing they
are audit decisions. Cancel the associated job to stop acquisition.

## Checked routing recovery

Readers verify MYNOUI01 or MYNOUI02, length, SHA-256 and semantic provenance. Writes use
private exclusive temporary files, file sync, atomic rename and directory sync.
Uncertain final sync blocks writes until restart. Corruption, symlinks,
nonregular or multiply linked files fail before receivers/transfers start.
Reservations require history format 2. Origins require job journal/snapshot
format 5, so older readers reject silent downgrade. Formats 1–4 remain readable
for jobs without IRC origins. Startup validates both directions before native
workers resume: a committed origin completes its reservation, an uncommitted
intent becomes aborted, and missing/disagreeing provenance fails closed.
Read-only opening never repairs these phases. Aborted intents are not replayed.

Each source has one receiver; one shared monitor interrupts sockets on shutdown.
Connect/registration has a ten-second overall deadline. Idle timeout accepts
30–600 seconds; a partial line has a ten-second assembly deadline. Bounds are
8,192 bytes per CRLF line, 4,096 per read, 128 parsed messages per batch and
256 messages per source per second. Invalid framing/tags, TLS, registration or
membership loss closes the connection. Backoff doubles inside configured bounds,
resetting after a connection lasting 30 seconds. Server error text is not echoed.
Standard-library DNS is synchronous; late results are rejected, but resolution
cannot itself be interrupted.
The routing worker waits at most its metadata budget before noticing shutdown;
tracker DNS retains this same standard-library limitation.

Original CI uses synthetic loopback IRC services to exercise boundaries, trust,
filtering, history limits/corruption, concurrent and stale reviews, restart,
PING/PONG, reconnect and shutdown. No public IRC or torrents are contacted.
Routing scenarios use original local metadata/payload peers and gates to cover
exact imports, opt-in and removal, frozen routes, source numbering, races, ambiguity,
ownership, concurrent passes, corruption and both sides of an interrupted commit.
SASL fixtures exercise strict configuration, capability/challenge boundaries,
credential redaction, fragmented replies, the native CLI's exact 400-byte
terminator, repeated handshakes, shutdown and nonrenewable registration deadlines.
NickServ fixtures exercise exact trusted account confirmation, premature/forged
membership, failure invalidation, redaction, repeated handshakes, maximum command
bounds, missing credentials, shutdown and the nonrenewable deadline.
Broader provider adapters, new-demand actions, packs/upgrades and cross-seeding
remain future increments.
