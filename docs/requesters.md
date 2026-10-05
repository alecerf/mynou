# Plex requester policies

Mynou 0.19 introduces requester demand alongside the existing operator queue.
An account's stable identity, policy and quota reservation persist before a new
job can acquire anything. A successful full watchlist poll updates that account
alone. Compatible accounts share one canonical acquisition; incompatible profiles
or destinations remain visible as conflicts without a quota charge. This
implementation requires its own complete CI and CI publication before its checks
can be reported as passing.

## Bind accounts without exposing credentials

The existing operator API token and browser sign-in remain the management
authority. Plex requester tokens live in environment variables and are never
entered in public controls or persisted reports. Add this optional section to
your existing schema-1 configuration:

```json
{
  "requesters": {
    "accounts": [
      {
        "id": "alice",
        "expected_user_id": "123456",
        "token_env": "MYNOU_PLEX_ALICE_TOKEN"
      },
      {
        "id": "bob",
        "expected_user_id": "234567",
        "token_env": "MYNOU_PLEX_BOB_TOKEN"
      }
    ],
    "destinations": [
      {
        "id": "family",
        "movies_root": "/library/family/movies",
        "series_root": "/library/family/series"
      }
    ]
  }
}
```

The default identity endpoint is `https://plex.tv/api/v2/user`; the default
watchlist endpoint is
`https://discover.provider.plex.tv/library/sections/watchlist/all`.
Optional `identity_url` and `watchlist_url` fields support explicit services and
original local protocol fixtures. Each poll checks the account token's user ID
against `expected_user_id` before reading its watchlist. The configured numeric
user IDs must be unique. Account and destination aliases use 1–64 lowercase ASCII
letters, digits, hyphens or underscores. Token-variable names contain uppercase
ASCII letters, digits or underscores. Rotating a token in the same binding does
not change identity. Changing a retained binding requires a new stable alias;
retired aliases remain reserved.

`default` routes to the existing library roots. Named routes resolve relative
paths against the configuration file, like ordinary roots. Make each selected
destination visible to Plex and mount it in the container; existing Plex section
and path-mapping settings still apply. Account tokens authorize watchlist reads.
The existing server token authorizes library confirmation and refresh.

Without configured requester accounts, the existing single-account watchlist
behavior remains available. Explicit operator submissions remain independent
interests. Removing a requester cannot cancel those interests or erase ready
files. New requester-tracked series start unmonitored in the operator series
registry; account polling produces separately approved aired-episode demand.

## Review an explicit policy

New accounts start disabled and require approval. Their initial profiles are the
existing configured movie and episode defaults; acquisition starts only after
explicit opt-in. The policy is a complete object:

```json
{
  "action": "policy",
  "policy": {
    "enabled": true,
    "approval_required": true,
    "max_active": 8,
    "max_daily": 32,
    "movie_profile": "any",
    "episode_profile": "any",
    "destination": "family",
    "notifications": "decisions"
  }
}
```

Choose names present in your configured selection profiles. Names can include
uppercase ASCII letters. `max_active` accepts 1–64; `max_daily` accepts 1–1024.
Both limits count canonical movie/episode requests per account. A series watchlist
entry produces demand for aired, identified, nonspecial episodes. Each episode
is approved and charged independently. Duplicate entries and overlapping roots
for the same account/media identity retain one demand and charge. Compatible
accounts each retain their own interest and quota charge while sharing a job.

A UTC-day quota is charged on first admission, including reuse of a ready
compatible job. Retries of an admitted request retain that charge. Removing or
cancelling demand retains its daily history. Ready and cancelled initial jobs
release active capacity; failed acquisitions keep capacity reserved until
cancelled or completed. Ready requests, library maintenance and explicit operator
submissions are outside new-demand admission quotas. These quotas count requests;
native concurrency, bandwidth and seeding settings remain separate controls.

Policy edits affect future admissions. Pending demand needs approval under the
current revision; explicit approval captures that current policy. Existing
reservations, jobs and retry decisions retain their original profile definition
and absolute destinations. Disabling acquisition prevents new admissions; use a
reviewed removal to revoke existing demand. Shared work requires identical
captured profile names/definitions and routes. Historical operator jobs must also
have compatible behavior; a ready job without a known import cannot establish
route compatibility by itself.

## CLI, API and browser

The browser's **Requesters** page shows accounts, poll results, active/daily
usage, pending decisions and recorded notification outcomes. Review a policy,
approval, rejection, removal or retry, then apply that exact review. The server
keeps one bounded session review for ten minutes. Apply carries account, action
and guard fields; policy substitutions, another session, logout and stale reviews
cannot reuse it.

```sh
mynou requesters --config mynou.json
mynou requester alice --offset 0 --limit 100 --config mynou.json
mynou requester-sync --config mynou.json
mynou requester-control alice --mapping policy.json --config mynou.json
mynou requester-control alice --mapping policy.json --apply --plan-id REVIEW_ID --config mynou.json
```

Mapping files contain only `action`, `policy` and/or `demand_id`, according to the
action. They are limited to 64 KiB. Preview can read existing offline storage
without writes; apply and polling require the running service. Example approval:

```json
{"action":"approve","demand_id":"THE_64_CHARACTER_DEMAND_ID"}
```

The Bearer-authenticated API exposes:

- `GET /api/requesters` and `GET /api/requesters/ACCOUNT?offset=0&limit=100`.
- `POST /api/requesters/sync` with an empty object, returning independent results.
- `POST /api/requesters/ACCOUNT/control` with the same action object. Preview
  omits `apply`/`plan_id`; apply requires `"apply":true` and the returned guard.

Controls reject unknown fields, wrong account ownership and stale snapshots.
Guards bind identity, policy, complete demand and jobs, configured selection/
destinations and the UTC day. Preview performs no acquisition or persistence.
An approval may remain quota-blocked or conflicting; status explains the outcome.
Removal changes only the selected account's interest. Rejection is for unadmitted
demand. `retry` requires retained approved failed/cancelled acquisition and
available active capacity. Removed and rejected records are tombstones; polls
never silently revive them. Explicit operator submission can establish an
independent interest, followed by a deliberate retry where needed.

## Polling, persistence and notifications

An account poll verifies identity, completes bounded watchlist pagination and
resolves canonical movie/episode identities and retained numbering before
committing demand. Successful polls remove only absent watchlist interests.
Failures preserve that account's cursor and demand. A policy change during I/O
discards its late result. Accounts with older attempts have polling priority;
skipped deadlines preserve that timestamp. There are 32 retained account aliases,
32 named destinations, 512 watchlist/expanded items per account and a shared
90-second poll deadline with ten seconds per account. Requester polls acquire
episodes individually; automatic operator packs keep their existing policy.

`requesters.bin` is a checksummed, private, atomic snapshot of up to 16 MiB and
10,000 retained demands. A pass admits up to 64 requests. Snapshot reservations
persist before request-journal jobs. If interruption occurs between those writes,
the same reserved request recovers by identity without another job or charge.
Startup verifies both stores before native transfers start; lost, conflicting or
corrupt provenance fails explicitly. Job/snapshot format 4 carries immutable
requester capture; readers still accept formats 1–3. Earlier binaries reject
format 4. Numbering and complete shared-group lineage retain their earlier
ownership and promotion rules.

Existing Plex media can satisfy demand without a native download when its path
is below the captured, mapped root. Newly imported requester media needs exact
path confirmation. In-flight work uses captured profile/destination behavior
after policy and configuration edits. A removed last requester cancels only
requester-created unfinished work without other approved/operator interests;
ready bytes remain in place.

`notifications` chooses `none`, `decisions` or `all`. Outcomes are recorded in a
bounded inbox, with up to 1,000 retained records globally and 100 recent outcomes
on an account page. Repeated polls do not repeat the same outcome. `decisions`
omits routine reservation/active transitions; `all` includes them. Credential
values and endpoint URLs are absent from reports and recorded outcomes. External
notification delivery and a requester self-service login are later integrations.
