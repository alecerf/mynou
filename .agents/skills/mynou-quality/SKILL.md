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

Merged branches are deleted by GitHub. Before deleting any other branch, freshly
prove it is merged or identical to `trunk`, has no open PR, no open Issue reference
and no active claim on its Issue. Never delete `trunk`, control/release or
ambiguous branches or unique unmerged work. Age is not proof. Preserve useful work.

After merge, check that the linked Issue closed, update status labels and unblock
native dependencies. Encode recurring friction in the smallest useful reviewed
Skill, tool, fixture or doc. Retire obsolete instructions and overlapping roles
without weakening security, secret protection or truthful review.

The retired lease refs (`refs/heads/control/engineering`,
`refs/notes/mynou-engineering`) are history: never delete or rewrite them
without an explicit owner decision.
