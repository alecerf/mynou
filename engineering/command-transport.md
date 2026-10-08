# Scheduled control-command transport

The scheduled web worker submits native comments on Issue #30. The
`Authenticated engineering control` workflow runs reviewed default code only.
It does not run another AI worker or depend on a Codex VM. Issue #30 is a command
mailbox, not the backlog or lease authority; it remains usable after closure.

Only configured native actor IDs can submit commands. The handler re-fetches
the unchanged comment and managed mailbox, verifies repository and Issue IDs,
requires created events and accepts commands at most five minutes old. Edited
comments, duplicate JSON keys, unknown fields, arbitrary reference names,
caller-provided recovery evidence and shell instructions are rejected.
Write permissions cover only Git content and native receipts. Comments are
never interpolated into shell commands; failures do not echo caller content.

Envelope (all placeholders must come from current native GitHub state):

````text
<!-- mynou-control-command:v1 -->
```json
{"schema":1,"command":"release","expected_sha":"EXACT_CONTROL_SHA","args":{"worker":"CURRENT_WORKER","lease":"OWNED_LEASE_ID"}}
```
````

Supported commands and exact argument sets:
- acquire: worker, registered role, managed Issue number, branch, exact commit, PR or null.
- checkpoint: worker, lease, summary, next_action, exact commit, PR or null.
- release: worker, lease; a checkpoint from this owned phase is mandatory.
- recover: no arguments; the trusted handler inspects native interrupted work.
- attempt: worker, lease, approach, fingerprint; three unchanged attempts release to Triage.
- probe-notes: worker, lease, expected_probe_sha (null only before the first probe).

Every control command includes the exact observed canonical expected_sha.
Ref advancement uses a sole-parent commit and force:false. A stale request loses
without force or retry. The committed generation makes accepted control commands
non-replayable. Receipts report only command ID, command, control SHA and lease
ID. A lost receipt does not roll back progress: read the canonical ref and
historical state before deciding what remains. Never repeat an uncertain command
or infer ownership from comment creation or a queued Action.

The notes probe is restricted to refs/notes/mynou-control-transport-probe.
It writes only an inert probe.json marker with no lease state. Its creation
and subsequent sole-parent non-force update establish real non-branch access
through the connected comment/default Actions transport. Exact probe heads
prevent replay; control ownership is fenced before and after the write.
This is evidence for Issue #19, not activation of a second live authority.

Wait for actual native execution and read the exact receipt/ref before performing
engineering work under a requested lease. A queued transport is an external
wait: checkpoint/release any currently owned work when appropriate and stop
without busy polling or another worker. Do not claim successful transport or
notes access until actual default execution proves it.

GITHUB_TOKEN receipts do not recursively trigger delivery. After a confirmed
release, a connected worker can post its ordinary native handoff; existing
CI/event/hourly delivery fallback also reads durable state. Bridge installation
does not alter the live hourly task, legacy-worker fence or canonical branch.
Those reviewed cutover steps remain in Issue #19 / preserved draft PR #24.

Regression scenarios run only in CI. Independent Security/QA inspect the
complete source diff. Closing Issue #30 also requires actual post-merge
connected-comment/default-run/control-and-notes proof; pre-merge fixtures
alone are not a live-backend capability claim.

## Fixed migration commands

Issue19 adds strict `fence-control` and `activate-control` commands with `worker`
and `lease`, `recover-fence` with `worker`, and `retire-control` with `worker`,
`lease`, exact `notes_proof_sha` and native `task_receipt_id`. The existing
schema1 envelope and `expected_sha` remain mandatory. These use only reviewed
fixed legacy/notes refs; no caller selects a path, script or arbitrary authority.
See [cutover and interruption recovery](control-storage.md). A queued command is
not proof of success. Actual receipts, Actions and ref/history must be inspected.
