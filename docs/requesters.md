# Plex requester accounts

With Plex enabled, Mynou follows the watchlist of the server token's account
(`plex.watchlist_url`). Requester accounts let it follow several Plex users
instead, each with its own profiles and library destination. When compatible
accounts ask for the same movie or episode, one download serves all of them.
Without requester accounts, the single-watchlist behavior is unchanged.

The operator API token and browser sign-in remain the only management
authority. Requester tokens only read their account's identity and watchlist;
the server token still confirms and refreshes the library.

## Bind accounts

Add an optional `requesters` section to `mynou.json`:

```json
{
  "requesters": {
    "accounts": [
      {"id": "alice", "expected_user_id": "123456", "token_env": "MYNOU_PLEX_ALICE_TOKEN"},
      {"id": "bob", "expected_user_id": "234567", "token_env": "MYNOU_PLEX_BOB_TOKEN"}
    ],
    "destinations": [
      {"id": "family", "movies_root": "/library/family/movies", "series_root": "/library/family/series"}
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
  resolve relative paths against the configuration file. Make them visible to
  Plex; Plex sections and [path mappings](macos.md#connect-plex) still apply.

Keep tokens in the environment; they never appear in controls or reports.

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
reviewed policy change recaptures the account's pending or conflicting demand
that has not been admitted, using the new profiles and destination. Already
admitted requests keep their captured policy. Disabling an account stops new
admissions; use `remove` to withdraw existing demand.

```sh
mynou requesters --config ./mynou.json
mynou requester alice --offset 0 --limit 100 --config ./mynou.json
mynou requester-sync --config ./mynou.json
mynou requester-control alice --mapping policy.json --config ./mynou.json
mynou requester-control alice --mapping policy.json --apply \
  --plan-id PLAN_ID --config ./mynou.json
```

A control file contains `action` and either `policy` or a `demand_id`:

- `policy` sets the account's policy, as above.
- `remove` withdraws one demand of this account only. Removed demand stays
  recorded and polls never revive it.
- `retry` restarts one failed or cancelled acquisition of this account.

Every control is previewed first; applying needs the returned `plan_id` and the
running service, and a changed account, policy or demand requires a new
preview. Previews can read offline storage without writing. `requester-sync`
polls every account now and needs the service.

| Route (Bearer token) | Purpose |
| --- | --- |
| `GET /api/requesters` | Accounts and poll results |
| `GET /api/requesters/ACCOUNT?offset=0&limit=100` | One account and its demand |
| `POST /api/requesters/sync` | Poll all accounts, with `{}` |
| `POST /api/requesters/ACCOUNT/control` | Same object as the control file; add `"apply": true` and `"plan_id"` to apply |

The browser's **Requesters** page shows accounts, poll results and demand, and
offers the same reviewed controls.

## How demand is handled

- A poll verifies the account, reads its complete watchlist and resolves each
  entry to a catalog identity before recording anything. A successful poll
  removes only the entries that left the watchlist; a failed poll changes
  nothing for that account. Accounts that waited longest are polled first.
- A series entry requests its aired, identified, regular episodes one by one.
  The series record created for it starts unmonitored, and automatic packs are
  not used for this demand.
- Accounts with identical profile definitions and destination share one job,
  each keeping its own interest. Incompatible demand stays visible as a
  conflict.
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

Requester demand lives in a private, checksummed `requesters.bin` beside the
journal. Startup checks it against the journal before any transfer starts, and
an interruption between the two writes is recovered without a duplicate job.
Bounds are listed in [limits](limits.md#plex-requester-accounts).

Snapshots saved before the approvals, quotas and notification removals are
rejected at startup; there is no migration. Removing `requesters.bin` alone
does not restore startup while requester-provenance jobs or committed IRC
admissions remain: they require
their original durable demand and origin. No stored data is deleted
automatically. Read the [release notes](releases/) and back up private storage
before upgrading.
