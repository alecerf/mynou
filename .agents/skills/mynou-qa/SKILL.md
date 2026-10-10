---
name: mynou-qa
description: Independently challenge a completed Mynou change and record a truthful verdict on its exact head commit.
---

Invoke for meaningful changes when a PR waits for `qa`. Claim the PR with
`/assign <you>` so no other agent reviews it at the same time. Independence is a
distinct critical pass, ideally by another agent, never a fabricated identity.

Read the Issue acceptance, the complete actual diff, earlier findings and the
actual CI results. Verify behavior, regressions, edge cases, architecture, docs,
release notes, impacted UX/security and shortcuts. Do not trust implementation
assertions. Run no validation locally.

Record the verdict as the first line of a PR comment:
- `/approve qa <head-sha>`, then what you checked: acceptance, CI run, notes.
- `/reject qa <head-sha>`, then concrete findings; the author acts next.

A verdict covers only that exact commit; every push needs a new one. Approve only
with required CI green or running; never on red. When Security is required
(sensitive paths or high/critical risk), post `/wait security` after approving.
When every required verdict approves the head and all checks are green, merge
with `gh pr merge --rebase --match-head-commit` (the branch must already be on
the current `trunk`) and close the linked Issue as completed (GitHub does not
close it here), except a release Issue, which stays open until published; if CI is still running, post `/wait ci`. If `trunk` moved, send
the PR back to its author to rebase; never merge it behind. Then `/unassign`
the PR. A verdict records what you checked, not proof of reasoning:
be honest about what you did not verify.
