# Durable notifications

Mynou 0.20.7 implements opt-in native HTTP outcome delivery. It passed complete
CI/publication in run 37429460979 with 602 Rust tests and four scheduler checks. Requester preferences still select `none`, `decisions`
or `all`; the existing bounded account inbox remains available. IRC source routes
receive first receipt, review, admission, routing and aborted recovery outcomes.

Configure explicit stable route IDs. Endpoints use HTTPS, with plain HTTP allowed
only for a literal loopback address. Queries, fragments and embedded credentials
are rejected. Credentials are read from an optional environment variable at send
time; their values never enter state or reports.

```json
{
  "notifications": {
    "routes": [
      {
        "id": "alice-outcomes",
        "enabled": true,
        "account_id": "alice",
        "url": "https://notify.example.test/mynou",
        "token_env": "MYNOU_NOTIFY_TOKEN",
        "max_attempts": 4
      },
      {
        "id": "announcements",
        "enabled": true,
        "source_id": "tracker",
        "url": "https://notify.example.test/announcements",
        "max_attempts": 4
      }
    ]
  }
}
```

Exactly one configured requester account or IRC source is required per route.
Routes default to disabled. Up to 32 configured routes are allowed. Their binding
includes ID, scope, endpoint, credential environment name and attempt limit;
changing one of these requires a new ID. Removal or disabling pauses pending
work. Re-enabling the same binding resumes it. Adding a route does not backfill
old outcomes. Neither route configuration nor delivery approves or acquires media.

Each event is saved in the same checked snapshot as its owning outcome. The
payload has schema, route/event ID, kind, scope/subject IDs, emission, fixed
outcome and timestamp. It contains no raw announcements, provider titles, magnets,
paths, URLs or credentials. Requester and IRC stores independently retain up to
1,024 events and 32 historical route bindings. Only the oldest terminal event
can be pruned when capacity is needed; live work is never evicted. Full live
capacity rejects the owning new outcome before its checked save.

The worker posts JSON with `Idempotency-Key` and `X-Mynou-Event-ID`. Only a 2xx
response acknowledges delivery. Redirects are rejected. Calls use a five-second
budget and 4 KiB response limit; synchronous DNS may exceed the budget. An attempt
and 15-second lease are saved before I/O. Expired leases recover without resetting
the attempt count. Failed calls use bounded exponential delays and a configured
1–8 total attempts. Each pass handles at most eight events, alternating requester
and IRC scopes. Shutdown admits no new attempt and finishes acknowledgment of an
already acquired lease.

Delivery is **at-least-once**: a crash after receiver acceptance but before local
acknowledgment can repeat the same stable event ID. The receiver must deduplicate
that ID. No exactly-once delivery or specific third-party service compatibility
is claimed. Errors are fixed diagnostics, excluding response bodies and secrets.

```sh
mynou notifications --limit 100 --config mynou.json
mynou notifications-dispatch --config mynou.json
mynou notification-control EVENT_ID --kind irc --action discard --config mynou.json
# Review the result, then apply its plan ID:
mynou notification-control EVENT_ID --kind irc --action discard --apply --plan-id PLAN_ID --config mynou.json
```

Protected API routes are `GET /api/notifications?offset=0&limit=100`,
`POST /api/notifications/dispatch` with `{}`, and
`POST /api/notifications/control` with kind, event_id, action, optional apply and
plan_id. Retry/discard require a current guarded review; sending, delivered and
discarded events cannot be changed. A retry retains the same ID and remaining
attempt budget; exhausted events cannot be retried. Discard affects delivery
alone. The browser **Notifications** page uses expiring session-bound reviews.

Listing and offline previews remain read-only and never repair leases. Dispatch
and confirmed changes require the running service. Checked notification semantics
use requester snapshot format 3 and IRC history format 4; older binaries cannot
read them. Without routes, old snapshot serialization and review guards remain
unchanged. Preserve the state directory when upgrading.
