---
name: mynou-triage
description: Interpret Mynou intent, reproduce failures, prioritize bounded Issues, select specialists and recover stalled work.
---

Invoke for ambiguous intent, reproduction, priority, impact or stalled work.
Skip a separate phase when an existing small Issue already gives complete scope.

Own expected user behavior, evidence, acceptance, risk, origin, priority and next
role. Reproduce Rust behavior locally with the installed pinned native toolchain,
and use Docker for Python checks or optional Linux reproduction. Record platform
gaps and use required CI for unavailable coverage. Any code edits use
the Issue Git worktree; inspect code/prior logs first. Use
one meaningful Issue; add native blockers/sub-issues only for real dependencies.
Keep one status/priority/risk/origin label per family. Native issue types are
optional and must actually persist before being claimed.

Product supplies evidence-based opportunities/value/bounded acceptance. Triage
checks prerequisites, risk and executable specialist sequence; UX designs the
experience. Keep unknown prerequisites Needs Triage and actual native blockers.
A small queue is not product completeness or permission to manufacture work.

Choose minimum useful sequences: Rust -> local checks -> QA with required CI;
UX -> Rust -> local checks -> QA with required CI;
trust boundary -> Security -> implementation -> Security verification -> QA.
Recover lapsed claims from the actual Issue, branch, commits, PR and CI: take
over with `/unassign <old>` then `/assign <you>`, and continue the existing PR.
Preserve unique work; avoid duplicate Issues and PRs.

Three unchanged attempts at the same approach: stop, record what was tried and
choose a changed approach before retrying. Quota loss is temporary capacity,
never product failure. Merge only with fresh verdicts on the exact head and green
checks. Refresh after transient failures; never blindly retry stale writes or
transfer routine management to the user.
