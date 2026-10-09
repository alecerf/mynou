# Next release

Merged changes accumulate on `trunk` without being released. Each user-visible
change adds a short note in [releases/unreleased](releases/unreleased/), named
after its Issue. Engineering, CI and documentation changes need no note and are
never released on their own.

At most once a week, when shipped code changed since the
[latest release](https://github.com/alecerf/mynou/releases/latest), a release PR
labeled `release` bumps the version, gathers those notes into
`releases/<version>.md` and removes them. Once it merges, GitHub Actions
validates and publishes that version. A security fix or an explicit owner request
can use `release-now` to publish sooner. The
[engineering runbook](../engineering/README.md#releases) describes the steps, and
the `Release policy` check enforces them.

Published notes for every version live in [releases](releases/). Earlier
revisions of this page, which tracked one proposed version per change, remain in
Git history.
