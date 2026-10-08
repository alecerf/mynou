# Canonical engineering control and legacy retirement

Issue #19 installs a fixed, recoverable migration to
`refs/notes/mynou-engineering`. Configuration deliberately retains
the legacy alias during bootstrap; the resolver, not a mutable Issue pointer,
decides the one authority. Installation is not live cutover proof or task proof.
The original branch remains authoritative until an actual reviewed fence succeeds.

## Reference and admission contract

Read only configuration to locate control before domain work. The explicit
reference adapter validates exact heads/notes namespace, native commit identity,
legacy alias consistency and the default-branch exclusion before writes. No
force, fallback to an empty store, prefix match or blind retry is permitted.

With the fixed migration policy, admission reads both exact control refs:

- A schema1 legacy state and absent notes means legacy is the sole authority.
- A schema2 legacy fence and absent notes pauses domain work. Its nested lease
  supplies admission only; a valid lease means another wake exits. The already
  admitted owner can renew/checkpoint or release this same nested lease.
  Deliberate release or expired capacity requires native Issue, branch/commit,
  PR and CI preservation before fence recovery.
- Active notes must descend from the exact native sole-parent legacy fence.
  A changed/recreated legacy branch or conflicting notes fails closed.
- After retirement, notes admission validates the preserved fence commit and
  ancestry even though the old ref is absent. Both refs absent is an error,
  never permission to initialize.

Every state update creates an original commit with the observed head as its sole
parent and uses a non-force ref update. Competing siblings cannot overwrite each
other. The legacy fence has a distinct schema and complete nested prior state:
old workers refuse it rather than reacquiring an expired old branch. An old
in-flight sibling also loses the same non-force arbitration. The fence is a
migration record, not a permanent Boolean execution lock.

## Reviewed installation, then live cutover

Reuse draft PR24 and Issue19. Complete Security and independent full-diff QA
against exact source/base, then require every actual objective gate. Passing
installation review certifies the proposed migration code, not unperformed live
activation. Merge leaves Issue19 In Progress; mechanical cleanup must not mark
this operation delivered. Actions alone publishes the new immutable version.

After actual default CI/publication is successful:

1. Master reconciles the **existing** hourly task with the merged admission prompt,
   retains native ID/cadence/timezone, checks the actual supported response and
   records its trusted attestation. This precedes fencing so interruption resumes
   through the updated schedule. Master acquires Issue19 at the exact installed
   default merge, linked PR24.
   The strict native-comment/default-Action transport uses the fixed mailbox30,
   trusted native identities, freshness, unedited payload, exact heads and leases.
2. `fence-control` checks source and current objective review/CI gates,
   the actual task attestation, absent target and fresh owner. It writes the
   schema2 fence on legacy by CAS.
   No candidate notes ref exists before this write. The exact admitted Master
   owner can use `checkpoint-fence` at least every 15 minutes, or
   `release-fence` before an intentional stop. Neither creates another authority
   or changes the installed source scope; both use schema2 sole-parent CAS.
3. `activate-control` creates notes from an original valid-state commit
   whose sole parent is that fence. All old commits, reviews, attempts and
   checkpoints remain ancestors. Native creation is atomic; an existing target
   is preserved rather than replaced. Confirm native receipt, ref, exact state,
   parent/ancestry and Actions success before describing activation as complete.
4. Run a real canonical `checkpoint` command through the same transport
   on notes. Its commit records the native command ID. Preserve actual
   command/receipt/run and exact-parent proof; fixture tests are not live proof.
5. Recheck the existing hourly task against the accepted response and merged
   prompt before retirement. Never create a duplicate schedule or invent a Task
   API in Actions.
6. Quality may request `retire-control` only with actual notes write proof
   and recorded Task reconciliation. Source/default gates, owner, fence, open
   PRs and fresh exact legacy head are checked again. Retirement intent is first
   durable on notes; the legacy branch is then deleted and absence confirmed.
   Its history remains reachable from notes. This explicit retirement is the
   sole exception to general Quality's rule preserving control branches.
7. Confirm sole notes admission and recovery, then close Issue19 and finish
   native cleanup. Do not alter or republish earlier immutable releases.

The normal control commands use the resolved authority. Migration commands use
only these fixed refs; they cannot take an arbitrary repository, ref or script.
All execute reviewed default code, never proposed PR code with write tokens.

## Interruption and recovery

Loss before fencing leaves the normal renewable legacy lease. Loss after fencing
but before notes creation leaves a complete schema2 snapshot. An owned
`checkpoint-fence` renews the unchanged 45-minute lease and records useful
progress. `release-fence` requires the phase checkpoint, preserves the exact
still-held native-parent owner as a handoff and releases the sole nested lease.
It is a historical owner record, never a second active lease.

When the nested lease is expired or deliberately released, `recover-fence`
inspects the native Issue, branch/commit, PR and CI, preserves useful work and
acquires one scoped Master lease in a new single-parent fence. A deliberate
release must match its actual held parent, release timestamp, generation,
source scope and attempts before recovery; it does not wait for expiry.
Current installed source/gates must still be valid before recovery or activation.
A valid nested owner cannot be reclaimed; stale or competing writes stop without
force or retry. Activation uses the latest recovered fence.

The administrative CLI routes owned `checkpoint`/`release` to the fenced
operations when schema2 is pending; supply the exact `--worker` and `--lease`.
A fenced checkpoint cannot replace the source commit or PR. Web workers use the
strict native `checkpoint-fence`/`release-fence` envelopes instead. This routing
does not run local checks; recovery is the documented native command protocol.

Loss after notes creation is ordinary notes lease recovery, even if the receipt
was lost. Read current refs before another command; stale commands never replay
writes. Loss after deletion but before confirmation preserves retirement intent
on notes. A later owned Quality phase confirms absence and finishes that record.
Never recreate the legacy branch or discard a conflicting candidate.

No quota/reset API, guaranteed post-reset wake, paid worker or persistent VM is
introduced. Hourly task wakes remain best-effort. Missing connected writes are
handled by the proven reviewed default-Action transport. Missing Task access
blocks retirement and leaves the old branch fenced, with progress intact.

## Actual Task attestation boundary

Actions cannot query ChatGPT Tasks. It accepts a bounded, unedited native comment
by a configured trusted worker on Issue19 after that worker **actually** receives
and checks the native Task response. This is an auditable worker attestation,
not cryptographic/API proof that Actions independently queried that service.

The worker records the exact merged policy commit, native prompt blob SHA,
existing task ID, enabled state, hourly cadence and SHA256 digest of the actual
accepted response in one comment:

````text
<!-- mynou-task-reconciliation:v1 -->
```json
{"schema":1,"task_id":"ACTUAL_NATIVE_ID","policy_commit":"ACTUAL_MERGE_SHA",
 "prompt_sha":"ACTUAL_PROMPT_BLOB_SHA","enabled":true,"cadence":"hourly",
 "response_digest":"SHA256_OF_ACTUAL_ACCEPTED_RESPONSE"}
```
````

Persist the actual safe API response in the native handoff/control checkpoint;
the digest is not a substitute for reading it. Secrets and unrelated personal
Task content must never be published. If the actual response is unavailable,
do not write an attestation. Retirement is blocked rather than falsely proven.

GitHub REST offers no expected-SHA conditional deletion. Fresh exact-head and
ownership checks plus preserved notes intent are the strongest supported guard;
out-of-band administrators do not share the lease. No stronger guarantee is
claimed. All tests, scans and validation execute only in CI.
