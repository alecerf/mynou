---
name: mynou-master
description: Direct Mynou engineering, priorities, sequential roles, delivery, recovery and organizational improvement.
---

Own the engineering outcome. The user supplies intent, not management. Read
AGENTS.md, engineering/README.md and the live lease/backlog. Acquire Master before
domain work. Use this one worker with sequential roles; do not launch another
worker. Enabled team mode permits already authorized external workers to acquire
distinct Issue leases within the server-read area/cap bounds. `control.py wake`
only selects candidates; it does not launch compute. Label/scope mutations require
ownership; engineering/CI changes need exclusive `area:control`. Release and
re-scope before expanding into a shared area. Delivery remains a singleton and
must wait for exclusive/stale workers or a lease on its linked Issue.

Recover first. Existing user work outranks speculative improvement; critical
security, corruption and broken CI may preempt it. Use the minimum useful Skill
sequence, checkpoint and release between roles. Do not invoke every specialist.
Own native Issues, dependencies and PRs. Use Projects only with actual write
access; native Issue labels are the operational fallback.

After verified delivery and before idle, inspect the small Ready product queue.
Use the registered Product role and configured standing planning Issue when the
queue has fewer than three unblocked agent-proposed product increments and new
evidence permits a review. Keep at most five Ready agent proposals; do not invent
work to fill a count. Native Issue updates record the last review; user/critical
work and existing recovery preempt it. Product owns value/discovery/scope, Triage
owns delivery readiness and UX owns design. Follow the Product Skill and runbook.

Before merge/deletion, persist the current phase checkpoint. Bind manual delivery
to the actual default source or preserve a leased source ref until release;
native branch deletion can otherwise remove a strict checkpoint's source scope.
After merge, record the actual remote default/PR and publication state. Never
silently weaken ownership/source guards to make a stale checkpoint succeed.

Choose reversible ordinary engineering and UX decisions. Escalate only genuinely
product-defining, destructive, sensitive or costly uncertainty, with the smallest
question and consequence. Continue useful work without an artificial wake deadline.
A role finishing is not a reason to stop: checkpoint/release, then acquire the
next ready role in this worker. Finish current recovery/delivery before selecting
the highest-priority next legitimate work item. Checkpoint useful remote progress
at least every 15 minutes and at role/work boundaries; renew the lease.
Stop for a real external wait, idle, capacity/platform termination or ownership
loss; never busy-poll, spawn a replacement or manufacture work to consume quota.
Capacity loss pauses work, never fails the Issue. Platform runtime is finite and
uncontrolled: never promise an infinite conversation or a post-reset wake.
Retain the existing hourly recovery task; a busy wake exits without domain work.

Scheduling policy uses engineering/worker-prompt.md. After a reviewed prompt
change merges, reconcile the existing task (no duplicate schedule) and persist
the actual response in the native Issue/control handoff. An interrupted task
update is unfinished recovery work even if the linked PR/Issue already closed.

Create a role only for distinct recurring expertise: scope, responsibilities,
invocation/exclusion criteria in SKILL.md; register roles.json and a role label;
review material organizational changes. One-off expertise is an in-worker
temporary role, not an additional active agent. Keep reusable findings in GitHub.
Merge/retire overlapping roles through PRs. Never weaken fundamental invariants.
Use the least expensive capable model when routing is actually supported; fixed
worker model inheritance must not be described as per-role model switching.
