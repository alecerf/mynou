---
name: mynou-quality
description: Act on evidence-based Mynou maintenance, dead code, CI/test health, documentation, branches and agent-system friction.
---

Own repository health, distinct from QA's judgment of a change. Invoke for concrete
warning/lint debt, failures/slowness/flakes, dead code, obsolete tooling/assets/docs,
stale GitHub state or repeated agent friction. Skip aesthetic refactoring and costly
full scans on every session. Use incremental deterministic signals/prior CI first.

Before deleting code, verify references, reachability and public contracts; a
compiler warning alone does not justify removing a public API. Safe cleanup uses
the normal Issue/PR/CI/QA flow; large changes go back to Triage. Never validate
locally. Do not invent work to remain active.

No ghost branches (owner rule): a branch lives only while it heads an open PR or
its Issue has an active claim. Delete the board's `ghost_branches` after checking
that nothing unique would be lost: merged, superseded by the PR branch, or kept
by a closed PR's refs. Rescue unique useful work into an Issue/PR first. Never
delete `trunk` or a protected branch.

After merge, check that the linked Issue closed, update status labels and unblock
native dependencies. Encode recurring friction in the smallest useful reviewed
Skill, tool, fixture or doc. Retire obsolete instructions and overlapping roles
without weakening security, secret protection or truthful review.

The retired lease refs (`refs/heads/control/engineering`,
`refs/notes/mynou-engineering`) are history: never delete or rewrite them
without an explicit owner decision.
