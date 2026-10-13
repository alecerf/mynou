Describe the problem and resulting user behavior.

Closes #ISSUE

Implementation and decisions: explain only what helps assess the change.

Verification: state the checked commit, local commands, Docker/native environment,
results and coverage gaps; link actual required Actions runs and behavior evidence.
Batch locally validated changes before pushing. Do not claim unfinished checks passed.

Checklist: one Issue, one branch, one isolated worktree, this one PR. Commits use
Conventional Commits (`<type>(<scope>): <summary>`). Work PRs keep the version and add
user-facing notes to `docs/releases/unreleased.md` (`## <Category>: <Title>`); only a PR labeled
`release` bumps it. When ready, comment `/wait qa`.
