# Current work — proposed 0.22.38 optional live progress

Actions actually published immutable [v0.22.32](https://github.com/alecerf/mynou/releases/tag/v0.22.32)
at 97c8efcbb97a74716e21791b5d4cf1578f85c28d in successful seven-job
[CI37895820331](https://github.com/alecerf/mynou/actions/runs/37895820331).
Release407617414 was published2026-10-09T06:56:34Z with the exact source tag,
four named digested assets and source/run/attempt receipt11600417547.
Issue52/PR55 are closed after verified publication and safe merged-branch cleanup.
Their original source and previous default remain actual merge parents.

[Issue45](https://github.com/alecerf/mynou/issues/45) addresses repeated refreshes
while following a request or transfer. Its reviewed UX scope adds opt-in live
progress on Jobs and Transfers lists/details, preserving native forms, filters,
focus, selections and unfinished edits. A small original embedded script requests
only displayed identifiers; the server omits private metadata and errors.
Exact script CSP/SRI content pinning preserves the browser trust boundary.

The increment bounds requests to 50 identifiers and replies to 48 KiB, uses one
visible-only request every ten seconds and an eight-second deadline, rejects
stale responses and stops on authentication failure. Other failures pause until
explicit retry. No global preferences, new-row insertion, streaming or invented
ETA/speed is included.

Original CI-only native HTTP/session/query/privacy/read-only fixtures and Node
standard-library browser-state fixtures accompany this source. They verify
complete-response validation, pause/visibility/abort races, exact counters and
preservation of edited controls. Node is a CI tool only, with no npm dependencies
or product runtime role. No local checks or real-browser proof are claimed.

Distinct exact-head/base Security and complete-diff QA, all actual objective
gates and Actions-owned immutable publication remain required. This source is
proposed, not a published v0.22.38 claim. See [scope](releases/0.22.38.md),
[browser guidance](web.md#live-progress) and [limits](limits.md).

Product planning #42 preserved existing work and added private iCalendar export
#56 as one bounded future increment. PAR2 #39 remains Ready; #40 retains its real
native dependency on #39. Neither opportunity is implemented by this release.

## Earlier: 0.22.33 default-source CI fix-forward

Actions published immutable [v0.22.32](https://github.com/alecerf/mynou/releases/tag/v0.22.32)
at `97c8efcbb97a74716e21791b5d4cf1578f85c28d` (release 407617414, run 37895820331).
Two later engineering-only merges, PR59 and PR60 (`02a536f2c75e454094c7cb28d85ec2771c7404f0`),
kept version 0.22.32. Actions publishes an immutable release for every push to the
default branch and the registry refuses a second image for a published version, so
the `release` job failed in [run 37920787776](https://github.com/alecerf/mynou/actions/runs/37920787776).
Every other job passed, and mechanical delivery recorded the failed default CI.

[Issue62](https://github.com/alecerf/mynou/issues/62) fixes forward. This source
proposes 0.22.33 with no product code change since 0.22.32. Delivery also treats a
recorded `ci-failed` commit as superseded only when its own native run completed
without success and the commit is a strict ancestor of the current default head.
Pending, unknown or unverified records, unrelated heads and a missing or unfinished
run still stop delivery, and the new head still needs its own CI and publication proof.
See [scope](releases/0.22.33.md). This version remains proposed until actual CI,
distinct exact-head/base Security/full-diff QA, every objective gate and Actions-only
immutable publication are complete. No local validation.
