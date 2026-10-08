# Branch-independent control storage

Issue #19 prepares a non-branch Git reference for the execution state. The live
authority is still `refs/heads/control/engineering`; this preparatory change
does not migrate it, delete it, publish a release or change the hourly task.

## Administration contract

`control_ref` names a fully qualified reference. An existing `control_branch`
alias may remain only when both identify exactly the same reference. Legacy
configuration without `control_ref` remains readable. The legacy alias accepts
branches only, and the product default branch cannot store control state.
Missing, malformed or
conflicting configuration fails before any storage request. There is no fallback
to another state store after a missing reference, API denial or transport failure.

The administration adapter accepts only branch and notes namespaces. It rejects
release tags, traversal, ambiguous components and query/fragment syntax. A read
must return the exact requested reference, pointing directly to a lowercase
40-character commit SHA. Prefix matches or annotated tag objects are not accepted.

Every write reads the observed head, creates a state tree and a commit with that
head as its sole parent, then updates that exact reference with `force:false`.
A stale initial head produces no writes. Competing sibling updates cannot
fast-forward over the winner. A rejected update is terminal; reconstruct remote
state before another attempt. Success requires the native update response to
identify the exact requested reference and proposed commit.

QA/Security still bind reviews to historical control commits, exact PR head/base,
their own scoped lease and the canonical control ancestry. Moving a pointer must
not replace commit-bound review history with editable Issue or package content.

CI-only scenarios exercise namespace/response confusion, authority ambiguity,
default-branch refusal, branch-only legacy aliases,
missing-reference refusal, stale expected heads, notes-reference races and
historical review ancestry. Fixture responses are not evidence of live GitHub
notes-write access. No local tests, lint, builds or validation are authorized.

## Cutover conditions

A candidate is `refs/notes/mynou-engineering`. GitHub's
[Git-reference API](https://docs.github.com/en/rest/git/refs?apiVersion=2022-11-28)
documents reference namespaces and non-force updates. Before activation, establish
actual read/create/non-force-update access from the supported hourly worker.
Currently connected write wrappers describe branch-named operations; their
non-branch behavior is unverified. The std-only REST client can express the
documented requests, but that alone does not prove scheduled-worker access.

A subsequent reviewed cutover must:

1. Hold one migration lease and preserve the complete current state/history.
2. Fail closed for legacy workers before a second location can be acquired.
3. Retain interrupted-cutover evidence and prove restart at every boundary.
4. Validate the exact replacement reference and single-parent arbitration.
5. Update versioned admission instructions and reconcile the existing native task.
6. Remove a legacy branch only after preservation and worker fencing are proved.

Do not create two live control locations, rely on a permanent Boolean lock, force
a reference update or quietly abandon historical proof. A package may hold a
checkpoint artifact later, but cannot become the arbiter without a demonstrated
atomic conditional update. Current immutable product releases remain untouched.
