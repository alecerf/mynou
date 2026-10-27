# Plex requester accounts

With Plex enabled, Mynou follows the watchlist of the server token's account
(`plex.watchlist_url`). Requester accounts let it follow several Plex users
instead, each with its own profiles and library destination. When compatible
accounts ask for the same movie or episode, one download serves all of them.
Without requester accounts, the single-watchlist behavior is unchanged.

Only the local API manages Mynou. Requester tokens only read their account's identity and watchlist;
the server token still confirms and refreshes the library.

## Bind accounts

Add an optional `requesters` section to the settings document, sent whole as
`{"settings": {...}}` to `POST /api/settings`:

```json
{
  "requesters": {
    "accounts": [
      {"id": "alice", "expected_user_id": "123456", "token_env": "MYNOU_PLEX_ALICE_TOKEN"},
      {"id": "bob", "expected_user_id": "234567", "token_env": "MYNOU_PLEX_BOB_TOKEN"}
    ],
    "destinations": [
      {"id": "family", "movies_root": "/Volumes/Media/family/movies", "series_root": "/Volumes/Media/family/series"}
    ]
  }
}
```

- `id` is a stable alias of 1 to 64 lowercase ASCII letters, digits, hyphens or
  underscores, for accounts and destinations alike.
- Before every poll, Mynou checks that the token in `token_env` (uppercase
  letters, digits and underscores) belongs to the numeric `expected_user_id`.
  User IDs must be unique. Rotating the token's value keeps the binding;
  changing the user ID, the variable name or the URLs needs a new alias, and
  retired aliases stay reserved.
- `identity_url` (default `https://plex.tv/api/v2/user`) and `watchlist_url`
  (default `https://discover.provider.plex.tv/library/sections/watchlist/all`)
  are optional.
- The destination `default` is the main library roots. Named destinations
  resolve relative paths against the data directory. Make them visible to
  Plex; Plex sections and [path mappings](macos.md#connect-plex) still apply.

Save tokens as secrets; they never appear in controls or reports.

## Enable an account

New accounts start disabled and request nothing. Enabling one takes an explicit,
reviewed policy, a complete object such as:

```json
{
  "action": "policy",
  "policy": {
    "enabled": true,
    "movie_profile": "any",
    "episode_profile": "any",
    "destination": "family"
  }
}
```

Profile names must exist in your [selection](selection.md) configuration. A
reviewed policy change fences new admissions; bounded reconciliation passes
recapture pending or conflicting demand that has not been admitted, using the
new profiles and destination. Already
admitted requests keep their captured policy. Disabling an account stops new
admissions; use `remove` to withdraw existing demand.

A control (`POST /api/requesters/ACCOUNT/control`) contains `action` and
either `policy` or a `demand_id`:

- `policy` sets the account's policy, as above.
- `remove` withdraws one demand of this account only. Removed demand stays
  recorded and polls never revive it.
- `retry` restarts one failed or cancelled acquisition of this account.

Every control is previewed first; applying needs the returned `plan_id`, and a
changed account, policy or demand requires a new preview. Previews write
nothing. `POST /api/requesters/sync` polls every account now.

| Route | Purpose |
| --- | --- |
| `GET /api/requesters` | Accounts and poll results |
| `GET /api/requesters/ACCOUNT?limit=100&after=CURSOR` | One account and a page of demand (`next` continues; omit `after` first) |
| `POST /api/requesters/sync` | Poll all accounts, with `{}` |
| `POST /api/requesters/ACCOUNT/control` | A control as above; add `"apply": true` and `"plan_id"` to apply |

## How demand is handled

- A poll verifies the account and continues its watchlist sweep: up to eight
  pages of 200 entries per pass, each resolved to catalog identities and
  recorded before the next page is read. The sweep position survives errors
  and restarts, so a large watchlist is read over several passes without
  starting again from the first page. Accounts that waited longest are polled
  first, so one large watchlist does not hold up the others.
- Entries that left the watchlist are removed only after a sweep reads the
  whole list without noticing a change. Each page re-reads the entry before it
  and compares the reported total. A sweep resumed in another polling pass
  also needs a second complete sweep with the same ordered identity digest
  before it can remove demand. This catches reorders that preserve both the
  total and the page anchor; a stable large list therefore takes two sweeps
  to publish. Changed digests need another confirmation, and progress survives
  restarts. Continuously changing upstream lists may delay removals; Plex does
  not provide an atomic snapshot, so matching traversals cannot prove that no
  concurrent edits occurred. A detected change or failed page removes nothing.
  `GET /api/requesters` shows an unfinished sweep under
  `sweep` (`position`, the upstream `total` and whether the list has stayed
  `consistent`, plus `verifying` while confirming a prior traversal), and each
  sync reports the pages, requests and rows it used.
- A series entry requests its aired, identified, regular episodes one by one.
  The series record created for it starts unmonitored, and automatic packs are
  not used for this demand.
- Accounts with identical profile definitions and destination share one job,
  each keeping its own interest. Incompatible demand stays visible as a
  conflict. A [per-title profile](selection.md#profiles-for-individual-titles)
  takes precedence over every account's captured profile for that title.
- Media already in Plex under the captured destination satisfies demand without
  a download. New requester imports need an exact Plex path confirmation.
- A ready operator job can serve a new requester only when every import is an
  existing regular file under the captured destination (no symbolic links) and
  its recorded release passes the requester's profile; without a recorded
  release, only the unrestricted default profile qualifies. Unfinished operator
  work is reported as a conflict.
- Removing a demand cancels only unfinished work that no other account or
  operator request needs. Ready files always stay, and operator submissions are
  never cancelled by requester changes.

Requester demand lives in the private, checksummed `requesters.idx` beside the
job index. Startup checks bidirectional job and IRC provenance before any
transfer starts, and an interruption between reservation and job creation is
recovered without a duplicate job. The first writable start migrates supported
legacy snapshots and preserves their exact bytes; `requesters.bin` becomes an
index identity marker. Read [migration and downgrade](inventory-storage.md#requester-storage)
and the remaining per-operation [limits](limits.md#plex-requester-accounts).

Snapshots saved before the approvals, quotas and notification removals are
rejected at startup; there is no migration. Removing `requesters.bin` alone
does not restore startup while requester-provenance jobs or committed IRC
admissions remain: they require
their original durable demand and origin. No stored data is deleted
automatically. Read the [release notes](releases/) and back up private storage
before upgrading.
