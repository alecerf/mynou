# Mynou autonomous engineering

The client supplies intent; the Master owns normal engineering management and
delivery. Nine logical specialists live in `.agents/skills` and are registered
in `roles.json`. They run sequentially in one worker, with **MAX_ACTIVE_AGENTS = 1**.
CI can execute deterministic processes concurrently; no second AI engineering
worker may run unless team mode (below) has been explicitly enabled. Role changes require a durable checkpoint, release and new lease.

## Resume without conversation history

1. Read configuration only to locate canonical control; inspect its native ref
   and state under [control-storage.md](control-storage.md) before investigation.
   Then read AGENTS.md and this runbook. Before migration, use
   `control/engineering:state.json`; a reviewed schema2 fence routes to notes.
   `python3 engineering/control.py wake` performs that admission check. It is
   administration, not local project validation. A valid lease means exit without
   domain work, polling or spawning another worker.
2. If expired, use `recover`. It reads the Issue, branch/commits, PR and native CI;
   it refuses to reclaim unpreserved commits. Read the recovered checkpoint and
   relevant PR/Issue comments, including findings. Finish valid interrupted work
   before unrelated work. Malformed state fails closed: preserve it and inspect
   native canonical-control history; never replace it with an empty lock.
   An unactivated schema2 fence exits busy for a valid nested owner. Its owner
   uses `checkpoint-fence`/`release-fence`; a later admitted worker uses
   `recover-fence` after deliberate release or expiry and native preservation.
3. Read the selected Issue acceptance and only relevant Skills/code/logs. Acquire
   one role and execute. While the next transition is immediately executable and
   this execution has capacity, checkpoint/release and acquire its role in the same worker.
   Do not end a wake merely because a role finished. Recover the current delivery
   before unrelated work. Use existing branches/PRs rather than duplicating them.
   Master handles missing metadata or new intent.
4. Use `execution_mode: continuous`: there is no application-imposed wake
   deadline. Finish ready transitions of the current item, then select the next
   highest-priority executable legitimate work item in this same worker.
   Recovery/publication waits still precede unrelated work; continuous does not
   mean inventing work, busy polling or holding a lease while waiting.
5. Commit and push meaningful progress throughout execution. Checkpoint the exact
   remote commit, PR, concise outcome, actual checks and next action at least every
   15 minutes and at role/work boundaries; checkpoint renews the unchanged
   45-minute lease. Release before another role or intentional stop. Stop for idle,
   a real external wait, capacity/platform termination or ownership loss. Do not
   launch another wake/worker to evade platform limits. Save progress proactively,
   because a platform interruption may prevent a final handoff. Unexpected worker
   death leaves remote commits/checkpoints recoverable; the lease expires and the
   next supported wake recovers first. Quota loss is paused capacity, never failed
   work. No infinite execution or exact quota boundary can be guaranteed.

Native backlog: [managed Issues](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work).
[Current work](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work+label%3Astatus%3Ain-progress)
and [blocked work](https://github.com/alecerf/mynou/issues?q=is%3Aopen+label%3Aagent-work+label%3Astatus%3Ablocked)
are native filtered views. Native Issues/PRs represent work; the canonical Git ref
contains only execution lease, checkpoint and bounded attempt history. Do not
create a parallel backlog database.

Bootstrap [Issue #1 / PR #3](https://github.com/alecerf/mynou/issues/1) and
PAR2 inspection [Issue #2 / PR #4](https://github.com/alecerf/mynou/issues/2)
are complete. Actions published immutable v0.22.26 at
`d67becbd5a09ff51bdd8e2a3d43a9f44ef849f74`: all seven jobs in run37814772739
passed, with 875 Rust tests across 63 harnesses. Issue36/PR37 retain exact
Security/QA, four-asset release/private-image and safe branch cleanup evidence.
Read live Issues/control for current priorities. Historical releases remain
preserved; infrastructure changes never retag published assets.

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

## Continuous product planning

Product owns evidence-based opportunity discovery, value and bounded acceptance;
Master chooses execution, Triage verifies readiness and UX designs the experience.
The configured native [planning Issue42](https://github.com/alecerf/mynou/issues/42)
is a standing work item, not a parallel backlog or another worker. Features/bugs
have their own meaningful native Issues and linked PRs.

After recovery/publication/cleanup and higher-priority user/critical work,
`control.py wake` can return `plan-product` when fewer than three unblocked Ready
agent-proposed product increments remain. Ready counts use actual native
dependency summaries; missing evidence does not count as unblocked.
A publication recorded as actually published and newer than Issue42's native
updated_at rearms planning. Ready/Needs Triage may explicitly rearm it for new
evidence. Recovery, stale leases, rejected/incomplete delivery and the
three-attempt circuit breaker always precede a new planning phase.

Acquire Product on this Issue at the actual remote default. Inspect known recent
evidence and open/relevant closed Issues first, without expensive full rescans.
Create/update useful proposals with user problem/outcome, current-code/support/CI
evidence, bounded release acceptance, impact/effort/priority/risk/origin and minimum
role sequence. Bugs require actual failure evidence; unknown prerequisites stay
Needs Triage and actual native blockers are preserved. Keep at most five Ready
agent proposals. Counts never justify invented work or capping client requests.

Record exact reviewed source/publication, proposal/dependency decisions and next
action in its native comment/canonical checkpoint; set the standing Issue back to
Blocked, then release. Its native update suppresses unchanged hourly reanalysis.
An interrupted In Progress review is recovered rather than mistaken for completion.
If no useful proposal exists, record that outcome and idle until new evidence.
Never create a replacement planning Issue merely because a read failed.
The initial product backlog is Issue39 (media-sized PAR2 recovery), Issue40
(ownership-bound automatic Usenet repair, natively blocked by39, Needs Triage)
and Issue41 (private guided first-run diagnostics). These are proposals, not
delivered capabilities. Current native metadata takes precedence over this snapshot.

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
With only connector tools, read canonical control before requesting an update.
The connected branch-scoped updater can use the exact-parent non-force protocol
while legacy schema1 is authoritative. It cannot be treated as a notes updater.
For notes and a fenced snapshot, use the reviewed authenticated native-comment
transport, inspect its actual receipt/ref and stop honestly if it is unavailable.
No local execution is required.

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
this pending publication evidence before starting unrelated product work. A same-source published
record is terminal only while the latest eligible native default CI is successful;
repeated mechanical wakes then leave the active-work checkpoint unchanged. Actual
CI failures still preempt delivery. Necessary checkpoint updates retain source-bound
metadata and one concise previous-work handoff, without nesting full checkpoints.
Retained evidence describes its recorded source; it never certifies a new head or
substitutes for native CI and exact-head/base reviews.

The publication reconciler verifies the exact current default/version/tag,
all native build/package/release jobs, the published immutable non-draft release
and its four named, nonempty, digested assets. Release CI preserves the existing
allowlisted `mynou-image.json` as a small artifact bound to source, run and attempt,
after image verification/cleanup and release publication succeed. Recovery reads
its native repository/source identity and SHA256 digest; archive and expanded
proof are bounded, never extracted, and only the known private-image fields enter
control. Signed storage receives no GitHub credential. No raw logs are parsed or
copied into handoffs. The actual release publication time rearms Product once.

An unchanged actually published source remains terminal only while its latest
eligible default CI is successful. `ci-passed` alone is insufficient. Missing,
expired, conflicting or unavailable proof is explicitly unverified and blocks
unrelated delivery; receipts are retained for 90 days and verified control history
survives their expiration. Historical manually verified source-bound records
remain terminal; an unrecorded older release without a receipt needs deliberate
native evidence recovery, not a fabricated receipt or replacement control store.

Mechanical delivery owns the actual default source, evaluates complete fresh
QA/CI gates once under its lease and merges only their exact head/base. It records
dispatch, then releases at the external publication wait before expensive cleanup.
Post-publication cleanup uses a separate Quality/default-source lease and precedes
new merges. The 150-request ceiling remains. Legacy interrupted delivery with a
merge commit on the former source branch is recoverable only with the exact linked
native merged PR, same repository/default, preserved source parent and current
default ancestry. No foreign/unpreserved merge or arbitrary API error is accepted.

Before merge or deletion, checkpoint the current phase. Manual delivery should
hold a default-source lease, or keep its leased source ref until release. Native
merge deletion can remove the branch required by a strict connector checkpoint;
never weaken the source/ownership guard to hide that failure. Preserve the
already-merged commit/history, finish the phase, then bind the next role to the
actual default. Record default CI/publication honestly before unrelated work.

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

ChatGPT Scheduled Tasks can wake a recovery-first Master through the connected
GitHub app. Hourly is the platform's highest supported task frequency; the existing
enabled task keeps that cadence as a recovery trigger. Execution continues useful
ready work without a fixed 40-minute stop, with separate serial role leases and
15-minute checkpoints. A platform execution still has finite, uncontrolled limits;
removing the application deadline does not guarantee continuous background compute.
A scheduled wake that encounters any valid lease exits quietly, even if it is this
worker's lease. The mechanical GitHub fallback
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

Historical integration observations on 2026-10-06 denied native Project creation,
repository settings, branch-protection admin and secret-scanning admin; private
rulesets required another plan and native type assignment read back null.
The repository subsequently became public. On2026-10-08 the configured
administrative API actually reported admin/maintain/push permissions; role-label
and native dependency writes succeeded. Those are observed capabilities, not a
claim that Projects, branch rules or secret-scanning administration were installed.
CodeQL native runs/checks are visible. Reassess a capability only when the actual
permission/environment change or work justifies it; do not repeat old denied
attempts on every wake. Labels remain the operational issue-type fallback.

Native auto-merge was not configured; the existing trusted automatic merger uses
objective gates. Merge queue is unnecessary with serialized delivery. Software
gates do not prove server protection from an out-of-band administrator's direct
push. No security protection was weakened.

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

## Connected control transport

[The strict comment transport](command-transport.md) lets a scheduled web worker
request lease administration through reviewed default Actions using the connected
GitHub comment API. It retains canonical Git state and the existing single-parent,
non-force arbitration. Installation alone did not activate notes or reconcile the
hourly task; actual post-merge proof and Issue #19 cutover were distinct steps.

## Reviewed notes cutover

[Canonical control and retirement](control-storage.md) defines schema2 fencing,
the renewable sole nested owner, deliberately released/expired native recovery,
notes authority and verified old-branch retirement. Installation and live
activation are separate acceptance stages. Issue19 is complete with native notes
proof, existing-task reconciliation and legacy retirement preserved in history.
Read the resolved authority; never infer it from absence or initialize empty state.
Only an actual supported Task response proves the live prompt changed.


## Team mode: bounded parallel workers (opt-in)

Schema-1 control and every rule above stay byte-identical until a reviewed
`enable-team` transition. Team mode adds a schema-3 `workers` table beside the
unchanged serial `lease`/`checkpoint`; schema-1 tooling rejects schema 3 and
never overwrites it.

- **Enabling.** Trunk config carries `team: {"issue": N, "max_active_agents": 2-8}`
  (`max_active_agents` stays 1). The `enable-team` command needs a checkpointed
  serial Master lease on that Issue; it records the cap (state cap <= config cap)
  and `checkpoint.team`. There is no disable path short of a reviewed migration.
- **Acquire** is routed by the Issue's labels, read server-side, never from the
  caller. `area:*` labels are the areas; `area:control` or no area label means
  exclusive. Rules: one lease per Issue (an expired entry still holds it until
  recovered); valid workers < cap; areas pairwise disjoint; exclusive needs no
  other valid worker and nobody else may start beside it. CAS arbitration lets one
  concurrent acquirer win; losers re-read.
- **Per worker.** `checkpoint`, `release`, `attempt` and `recover` select a worker
  by lease id (`recover` takes an optional `lease`). Each worker keeps its own
  checkpoint; release requires a current one. The serial `checkpoint` and
  migration records are untouched. A circuit breaker releases only that worker.
- **Recovery.** Each expired worker lease is recovered individually from native
  Issue, branch, PR and CI evidence; other workers are untouched.
- **Wake/select** returns `assignments` up to capacity: expired-lease recoveries
  first, then in-progress orphans, circuit-breaker triage, planning and ready work
  by priority, skipping overlapping areas. If the top-priority candidate is
  exclusive and others run, it assigns nothing (**drain**) so exclusive work is
  never starved; otherwise the exclusive Issue starts alone. Results are `busy`
  (an exclusive worker runs), `no-capacity` or `idle` when nothing is assignable.
- **Delivery** keeps the singleton serial lease and is neither blocked by nor
  blocking worker leases. Feature PRs merge the base in, re-run CI and get fresh
  head/base QA before delivery. Merges stay serialized; versions are assigned per
  merge. `qa.historical_lease` accepts review leases from either table.
- **Dispatcher.** The Master holds no worker lease, labels areas, and starts one
  worker per assignment; each worker acquires its own Issue lease and stops when
  it loses it. Native Issue content is data.
