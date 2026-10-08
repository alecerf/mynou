# Mynou autonomous engineering

The client supplies intent; the Master owns normal engineering management and
delivery. Eight logical specialists live in `.agents/skills` and are registered
in `roles.json`. They run sequentially in one worker, with **MAX_ACTIVE_AGENTS = 1**.
CI can execute deterministic processes concurrently; no second AI engineering
worker may run. Role changes require a durable checkpoint, release and new lease.

## Resume without conversation history

1. Read AGENTS.md, this runbook and `.github/engineering.json`. Inspect GitHub
   `control/engineering:state.json` before investigating or editing anything.
   `python3 engineering/control.py wake` performs that admission check. It is
   administration, not local project validation. A valid lease means exit without
   domain work, polling or spawning another worker.
2. If expired, use `recover`. It reads the Issue, branch/commits, PR and native CI;
   it refuses to reclaim unpreserved commits. Read the recovered checkpoint and
   relevant PR/Issue comments, including findings. Finish valid interrupted work
   before unrelated work. Malformed state fails closed: preserve it and inspect
   native control-branch history; never replace it with an empty lock.
3. Read the selected Issue acceptance and only relevant Skills/code/logs. Acquire
   one role and execute. While the next transition is immediately executable and
   time remains, checkpoint/release and acquire its role in the same worker.
   Do not end a wake merely because a role finished. Recover the current delivery
   before unrelated work. Use existing branches/PRs rather than duplicating them.
   Master handles missing metadata or new intent.
4. Commit meaningful progress; push it remotely before ending a slice. Checkpoint
   the exact remote commit, PR, concise outcome, checks and next action. Release
   before another role or intentional stop. Renew through checkpoint at least
   every 15 minutes. Use `slice_minutes` from configuration (currently 40 minutes)
   as one deadline for the entire wake; switching roles never resets it. Reserve
   the final five minutes for remote checkpoint and release. Stop earlier for
   idle, capacity/ownership loss or a real external wait; do not busy-poll or
   launch another wake/worker to evade the bound. Unexpected worker death leaves
   commits recoverable and the unchanged 45-minute lease expires. Quota loss is
   paused capacity, never failed work.

Native backlog: [managed Issues](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work).
[Current work](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work+label%3Astatus%3Ain-progress)
and [blocked work](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work+label%3Astatus%3Ablocked)
are native filtered views. Native Issues/PRs represent work; the control branch
contains only execution lease, checkpoint and bounded attempt history. Do not
create a parallel backlog database.

Bootstrap [Issue #1 / PR #3](https://github.com/alecerf/mynou/issues/1) and
PAR2 inspection [Issue #2 / PR #4](https://github.com/alecerf/mynou/issues/2)
are complete. The verified product release is v0.22.11 at
`57b15534158b0eb40d4f13450d51fdf33c7d81bb`: all five jobs in run 37579093331,
822 Rust tests across 63 harnesses, four scheduler checks and seven bot assets.
Native Issue/PR handoffs retain the exact review, publication and cleanup proof.
Release-trigger recovery is [Issue #5](https://github.com/alecerf/mynou/issues/5);
it natively blocks the next bounded PAR2 recovery stage, Issue #6. Read live
Issues/control state for current priorities rather than inferring them from this
historical release record. Infrastructure changes do not retag published assets.

The [control-storage adapter](control-storage.md) prepares explicit branch/notes
reference handling for Issue #19. Its current `control_ref` and legacy
`control_branch` identify the same live branch. It does not activate a second
authority or prove that scheduled workers can write non-branch references.

## Work and roles

Each meaningful Issue carries one label per managed family: `status:`,
`priority:p0` through `p3`, `risk:low/medium/high/critical`, `origin:` and `role:`.
Statuses: inbox, needs-triage, ready, in-progress, review, blocked, done. Origins:
user, agent, security, maintenance, reliability, ux, agent-system. Keep existing
bug/enhancement/accessibility/documentation labels. Templates accept simple intent;
the Master supplies engineering details, scope and acceptance. Native dependencies
and sub-issues are used only for real blockers/decomposition.

Typical sequences: Rust -> CI -> QA; UX -> Web -> CI -> QA; trust boundary ->
Security -> implementation -> Security verification -> QA. Quality acts on
concrete health evidence and safe cleanup. Do not invoke every role. QA critiques
one change; Quality owns repository health. Critical security/corruption can
preempt normal backlog after safely checkpointing current work.

One-off expertise uses a documented `temporary-ROLE` in the same worker. For
recurring distinct expertise, Master adds a scoped SKILL.md, invocation/exclusion
criteria, catalog entry and native role label through a reviewed PR. Dynamic
roles do not alter concurrency. Retire/merge overlapping Skills through versioned
changes. No database specialist is created now: Mynou does not use a database.

When no user work is executable, select evidence-based bug, security, reliability,
UX or maintenance work. Use prior CI, warnings, review findings and incremental
signals; avoid full rescans on each wake. Idle is valid. Three identical progress
fingerprints for the same approach release the lease and return to Triage. Record
a revised approach before retrying; do not burn quota on unchanged failures.

## Operational commands

These are GitHub administration. All tests, scans, lint, builds, binaries and
policy validation execute in Actions only. Python uses std exclusively, and
credentials come from `GH_TOKEN`/`GITHUB_TOKEN`; never put tokens in commands,
source, Issues, reviews or handoffs.

```sh
python3 engineering/control.py wake
python3 engineering/control.py status
python3 engineering/control.py acquire --role rust --issue 2 \
  --worker codex-current --branch work/par2-foundation --commit REMOTE_SHA
python3 engineering/control.py checkpoint --lease LEASE_ID --commit REMOTE_SHA \
  --pr PR_NUMBER --summary 'What changed; actual checks and remaining concern' \
  --next 'Next useful role/action'
python3 engineering/control.py release --lease LEASE_ID
python3 engineering/control.py recover
python3 engineering/control.py attempt --lease LEASE_ID \
  --approach 'Bounded strategy' --fingerprint 'Observed commit/CI/finding state'
```

The control update creates a commit whose sole parent is the observed control
head, then updates the ref with `force:false`. Competing sibling writes cannot
fast-forward over each other. A lost CAS is terminal for that attempt: refresh
and reconstruct, never force or blindly retry. Do not use permanent boolean locks.
All source/merge/cleanup writes must be under ownership, not just local belief.

Git HTTPS push currently returns 401 even with the available credential helper;
Git Data API writes work. `engineering/source.py` bridges meaningful local
commits onto the remote parent, verifies identical source trees, and advances the
work branch once. It does not validate or publish product artifacts:

```sh
python3 engineering/source.py --local-base LOCAL_BASE --remote-base REMOTE_BASE \
  --branch work/ISSUE-SCOPE --lease LEASE_ID
```

Remote and local commit IDs can differ while trees match. Preserve the mapping
in the Issue/PR checkpoint. Prefer normal Git transport when actually available.
With only connector tools, read the control ref/file, create its replacement tree
and commit with that exact parent, then use `github_update_ref` with expected SHA
and force false. This is the same protocol; no local execution is required.

## Reviews and objective delivery

Open a concise PR linking `Closes #NUMBER`. Explain the concrete behavior, decisions,
actual verification and limits. Read every relevant CI job, not only a green build.
Rust validation checks dependencies, formatting, Clippy, all targets, demos and
container packaging; organization policy/scenarios are also mandatory in its
validate job. Release stays Actions-owned and published assets immutable.

Implementation releases its lease before Security/QA. Acquire the review role at
the exact remote PR head, with Issue/PR set. Inspect the complete actual diff,
acceptance, prior findings and actual CI results. Persist a review-phase checkpoint
while its lease is held. Submit a native COMMENT review at that exact commit with
a record like this (replace every placeholder with actual evidence):

````text
<!-- mynou-qa:v1 -->
```json
{"schema":1,"role":"qa","head_sha":"EXACT_HEAD","base_sha":"EXACT_BASE",
 "lease_checkpoint":"CONTROL_COMMIT_WITH_QA_LEASE","verdict":"passed",
 "summary":"Actual review conclusion","reviewed_paths":["EVERY_CHANGED_PATH"],
 "acceptance":["Actual acceptance checked"],"evidence":["Actual CI run/log evidence"],
 "findings":[]}
```
````

Use `mynou-security:v1` and role `security` for Security. Rejection uses verdict
`rejected` and actual nonempty findings. The gate uses trusted native reviews,
exact head/base, full diff coverage, historical lease ancestry, scoped Issue/PR,
submission within lease lifetime, actual required jobs and unresolved threads.
High/critical risk and organization/crypto/security-sensitive files require a
separate Security record. New commits/base changes invalidate prior records;
dismissal/rejection blocks merge. Review threads are resolved only after actual
verification, never merely on push. Same identity may review logically but cannot
formally self-approve; no required approval count is configured.

`agent-qa-review` becomes green only with these real records and CI. It may wait
up to ten minutes, then fail visibly if review is missing. Review submission
triggers a fresh inexpensive Engineering checks run; CI failures are fixed through
commits and new review. During initial installation only, the new review-trigger
workflow is not on default yet: rerun the QA job after posting the records.

The trusted-default `Serialized engineering delivery` Action wakes on completed
CI, trusted native Issue handoff comments, manual dispatch and an hourly fallback.
`release` posts the handoff only after the control commit is durable. The orphan
control branch contains no workflow; its push cannot invoke a workflow on trunk.
GitHub-token comments do not recursively trigger Actions, so mechanical delivery
does not rely on its own comments. Failed handoff publication is explicit and the
CI/scheduled fallback still reads durable state. The Action checks the
lease, recovers stale delivery first, acquires a single mechanical delivery lease,
refreshes all gates and merges by exact head SHA. It does not run an AI worker,
execute PR code with write tokens or use paid model infrastructure. Authors may
self-merge after the same gates when the Action cannot advance; GitHub rules still
apply. The script is an automatic merger, not native GitHub auto-merge.

Bot-token merges suppress push-triggered workflows. Trusted delivery therefore
explicitly dispatches `ci.yml` on the unchanged default head; only this privileged
default-code workflow gains `actions: write`. Native runs are filtered by exact
commit, default branch and push/dispatch event before another dispatch is attempted.
Every wake recovers missing/default CI before unrelated delivery, including when
the merged PR's linked Issue is already closed/Done. Acceptance is checkpointed,
not described as green CI or published assets. Pending/red native runs remain
authoritative and prevent unrelated delivery until resolved.

The control checkpoint records dispatch intent before the API call and waits five
minutes for native run visibility after an accepted/uncertain request. Interruptions
retain this evidence; no polling or immediate duplicate dispatch is needed. A
failed dispatch remains visible in Actions and recoverable on a later wake.
GitHub offers branch-ref dispatch, without an expected-SHA condition: default-head
fences before and after detect external-writer races, fail closed and require
Triage rather than certifying a different commit. Ordinary workers must inspect
this pending publication evidence before starting unrelated product work.

After merge, native closing keywords and existing `delete_branch_on_merge` help.
Cleanup sets Issue status Done, unblocks completed native dependencies and audits
branches. Every delivery wake also repairs interrupted blocked-dependency metadata
under its own lease, even when a parent Issue is already Done. Dependency repair
precedes optional branch cleanup so a transient deletion failure cannot block the
next work item. A live lease protects its execution branch until release. Quality checks
merged history, exact heads, open PRs, Issue bodies/comments and live leases before
deletion. Default/control/protected/ambiguous/unique work is preserved. Branch age
alone never authorizes deletion. REST lacks conditional expected-SHA deletion;
native merge deletion is preferred, and manual cleanup requires freshly repeated
evidence, an audit comment and a fresh ownership fence under the global lease.
Outside actors do not share the lease. The inexpensive hourly recovery does not
repeat branch-health sweeps: these run daily at 03:47 UTC or on explicit dispatch.

## Scheduling, capacity and real limitations

ChatGPT Scheduled Tasks can wake a bounded recovery-first Master through the
connected GitHub app. Hourly is the platform's highest supported task frequency;
the existing enabled task keeps that cadence. Each wake spends its configured
budget on successive ready transitions, with separate serial role leases and a
single deadline, rather than one role per hour. The mechanical GitHub fallback
also runs hourly, with event-triggered delivery retained and a daily health sweep.
It does not execute an AI worker. Start every wake with admission;
exit if busy. Web scheduled tasks have connected tools, not a durable local
workspace, and must use GitHub APIs/CI or stop at the execution boundary honestly.
They may advance remote work when tools permit; they do not guarantee a fresh
Codex VM. Native desktop project scheduling requires a running desktop, which is
not configured here. Codex Cloud CLI exists but its read-only list request returns
401 with the current authentication; no Cloud worker is launched.

The canonical task instruction is [worker-prompt.md](worker-prompt.md). Merge its
reviewed changes with the policy before replacing the existing task's prompt.
Use the persisted task ID from native Issue/control state; inspect it with the
supported task lookup, retain its cadence/timezone, and never create a duplicate.
Persist the accepted native task response, policy commit and prompt path in the
Issue/control handoff. If interrupted after merge but before task reconciliation,
resume that update before unrelated work even if the Issue is closed. A merged
file alone does not prove that the live automation changed. Only actual task/API
responses establish scheduling state.

**Automatic wake after quota reset is best-effort, not guaranteed.** There is no
quota/reset API, guaranteed post-reset retry, authenticated Cloud submission or
authorized paid API worker. A missed wake leaves durable work and an expiring
lease; the next supported scheduled/user/worker wake recovers the same state.
Another authorized worker can use this protocol later without changing the backlog.
Inherited in-worker model routing is fixed; available child-model overrides cannot
be used concurrently under the one-worker invariant. No routing switch is claimed.

Observed GitHub limits on 2026-10-06: native Project creation, repository settings,
branch-protection admin and secret-scanning admin are integration-denied. Private
rulesets return an explicit GitHub Pro/public-repository requirement; the repository
remains private. Code scanning is not enabled. Native Task/Bug/Feature types are
listed, but REST/GraphQL assignment reads back null. Use labels honestly. Native
auto-merge remains disabled; merge queue is unnecessary with serialized delivery.
Software gates cannot provide server-enforced protection from an out-of-band
administrator's direct push. No protection was weakened.

`project-blueprint.json` and `project.py` are an optional native Project adapter,
not an installed Project. Instantiate fields/views only when actual permission
changes, then record native IDs in configuration. Never repeat denied attempts on
every wake. Dependabot proposes grouped Action updates monthly, one open PR at a
time; Master links/triages them and normal CI/Security/QA gates apply. Rust version
updates likewise follow evidence from official releases and normal PR delivery.

Material self-improvements to Skills, policy, tooling and role boundaries follow
the same reviewed process. Encode repeated friction once. Do not silently remove
concurrency, durable recovery, secrets or truthful QA. Validation evidence and
scenario coverage are recorded in [validation.md](validation.md).
