# Next-release checkpoint — 0.19 Plex requester policies

The 0.18 release adds [coordinated shared-group upgrades](group-upgrades.md):
complete baselines and immutable replacement lineage, authenticated new video
paths, staged exact Plex confirmation, atomic promotion and whole-group controls.
Its complete validation and CI publication passed for
`f1a9733a5908c3fa8bc5e93b5d18800334fad5d1` in
[run 37293887224](https://github.com/alecerf/mynou/actions/runs/37293887224):
467 Rust tests across 40 targets, four scheduler checks and all five workflow
jobs. CI published [v0.18.0](https://github.com/alecerf/mynou/releases/tag/v0.18.0)
with seven `github-actions[bot]` assets on October 5, 2026, at 10:04:19 UTC.
The tag targets that exact tested source. See
[validation evidence](validation.md#recorded-0180-ci-evidence) for asset digests.
The prior 0.17 tag and all seven asset IDs, sizes and digests remain unchanged.
Later documentation changes require their own complete CI and never replace
published artifacts. Inspect the live branch and all five newest jobs before
beginning 0.19.

v0.17 implements explicit shared files: one authenticated video belongs to a
complete consecutive canonical episode range and imports to one deterministic
Plex path. All owners commit together before acquisition; cancellation, retries,
restart and subset reuse preserve the captured binding. See
[shared files](shared-files.md).

Mynou 0.17.0 was published by CI from
`89e5aefa66026da084a6770fb76d51e7396602ec` after
[run 37271644993](https://github.com/alecerf/mynou/actions/runs/37271644993) passed:
450 Rust tests across 38 targets, four scheduler checks and all five workflow
jobs. Seven assets were uploaded by `github-actions[bot]` on October 5, 2026, at
06:19:26 UTC. The tag targets that validated commit. See
[validation evidence](validation.md#recorded-0170-ci-evidence) for asset digests.

The previous 0.16 numbering release passed
[run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691)
for `0821a4d3b499a5863fe5b50206c98bda25d6fb49` with 430 Rust tests, four scheduler
checks and all five jobs. Its tag and seven asset IDs, sizes and digests remain
unchanged. Earlier releases stay immutable and retain their evidence in
[validation](validation.md). Later documentation changes require their own
complete CI and never replace published artifacts.

The CI optimization's final commit `1583ca5768400fa2e9511d3444ec28b076d3b1f3`
passed [run 37235568061](https://github.com/alecerf/mynou/actions/runs/37235568061)
with all 409 Rust tests, four scheduler checks and all five jobs. Inspect the live
branch and newest run before continuing; later commits need their own complete CI.

## Preserve the development policy

Read `AGENTS.md`. Use Rust 1.99.0 and std only, with zero Cargo dependencies of
any kind. All source, diagnostics and docs must be English. Never run local
tests, lint, builds, binaries, demos or previews. `cargo fmt --all` is an allowed
edit. Commit meaningful chunks, push completed work and fix red Actions runs
with new commits. Do not start the next release before the current whole workflow
and publication are green. Actions alone creates validated tags/releases/assets.

Read [CI execution](ci.md). The scheduler checks the current Cargo target graph
and runs every harness; exact caches reuse compilation, never test execution.
Inspect validation, both native/static builds, packaging and publication together.
Preserve the four scheduler checks and immutable earlier releases.

The repository is `alecerf/mynou`, branch `trunk`. Personal Plex/source settings
are outside the repository. No available interface exposes remaining ChatGPT
quota. A saved checkpoint does not establish unattended resumption.

## Retain the numbering model

`src/numbering.rs` defines bounded canonical and source labels.
`src/series/numbering.rs` binds decisions to retained known catalog IDs and
normalizes approved remote labels back to canonical numbers before queueing.
Retired IDs remain reserved; a new ID cannot reuse their number. Overlapping
scopes must agree on canonical identities. Absolute order is always explicit.

Schema 1 infers anchors in memory without migration writes. Successful saves use
schema 2 with verified anchors and choices. Older job requests omit the optional
source label and retain historical keys. Existing retries/upgrades and terminal
requests keep captured source choices when series policy changes.

Preview/apply binds the previous complete record, fresh catalog and proposed
choices. Apply fetches again and rejects stale/unresolved decisions. Choices
persist before acquisition; apply itself queues no jobs. Source labels affect
queries, title matching and file selection. Import/Plex checks stay canonical.
Alternate source labels currently require explicit pack mappings.

## Shared ownership completed in 0.17

The 0.17 implementation now records a bounded authenticated torrent/path,
canonical owner range and one deterministic Plex destination. All new owners
commit atomically; runtime claims are serialized per group. Selected-file
verification, no-overwrite imports, individual exact Plex checks, queued-owner
cancellation interests, retry/restart and subset reuse retain the binding.
Format 2 rejects silent downgrade. CLI/API/browser apply guards repeat metadata
inspection; browser source credentials remain server-side for ten minutes.
See [shared files](shared-files.md). The exact complete Actions run and CI
publication are recorded above. Inspect the live branch and all five newest
jobs before beginning 0.18; later commits require their own completed workflow.

Individual shared remaps, baselines and upgrades remain blocked. Automatic packs
and ordinary mapped packs still require unique single-episode files. This gives
0.17 an independently usable scope without partially replacing shared owners.

## Retain coordinated group replacement in 0.18

`src/library/groups.rs` validates complete current owners and their existing
regular-file import, release/range identity and current episode policy. Metadata
inspection stays outside the journal lock. The guard captures parents, source,
profile, exact new hash/path and existing candidate state before a writable apply.
Browser credentials stay in one bounded session review, without hidden source
fields. Offline preview is read-only and apply requires the service.

`src/store/groups.rs` captures ordered parent IDs for every child and owns format
3 baseline/create/promote/cancel/retry transactions. Confirmations become staged;
the final owner promotes the whole group in one frame. Group claims serialize
imports and require every current parent to remain monitored. Exhausted failures
reserve their scope until cancelled. Retry retains bindings and bytes, repeats
required confirmations and rejects obsolete parents or competing replacements.

Journal/snapshot readers accept formats 1–3; old readers reject the new group
formats. Complete-scope and semantic validation reject partial baselines,
promotion/cancellation, fabricated individual frames and mutated lineage.
Memoized iterative root traversal retains lineage precedence without recursion.
Synthetic CI cases include 64-owner transactions, corruption/torn writes,
monitoring/control races, source/policy guards and native partial Plex confirmation
across restart. Its whole run and publication passed as recorded above; later
commits require their own complete workflow.

This stage replaces one shared video with one shared video. Automatic range-file
inference/search and shared-to-individual replacement remain later work; expose
them only after complete ownership, source labels and promotion are defined.

## Implement Plex requester policies in 0.19

1. Define bounded stable requester identities and per-account Plex token bindings
   without changing the operator's existing API/browser authentication. Keep
   credentials outside persisted public reports and review forms. Capture explicit
   approval, quota, destination-routing and notification preferences in the
   requester's versioned policy.
2. Add independent watchlist cursors/poll results and explicit opt-in movie/episode
   profile policies. Persist pending approvals and enforce quotas before any
   acquisition. Preserve the current single-account configuration and define
   policy compatibility before combining demand for one canonical media identity.
3. Persist requester provenance before acquisition. Removing one user's demand
   must not cancel another user's interest or delete ready media. Retain ownership,
   source numbering and complete shared-group replacement scope across restarts.
4. Expose requester status, approvals, limits, routing/notification preferences
   and guarded management through CLI/API/browser. Report partial account failures
   separately and retain bounded poll deadlines; stale identity/policy decisions
   require new review. Define notification outcomes without leaking credentials
   or changing another requester's demand.
5. Add original local-service CI cases for multiple accounts, duplicate media,
   conflicting policies, approval races, quota exhaustion/retry, destination
   routing, notification preferences, partial failures, credential redaction,
   removals and restart. Keep complete Cargo harness execution and all publication
   gates.

## Release acceptance and handoff

Bump Cargo/lockfile, Compose/deployment examples and release notes once complete.
Preserve offline graph, formatting, Clippy, all Rust tests, GNU/musl builds,
isolated native/container demonstrations and archive/checksum checks. Record the
exact source/run/tag/asset evidence after CI publication; later docs need CI too.

Then continue Plex user policies, IRC automation, native indexer adapters, Usenet
and cross-seeding on the [roadmap](roadmap.md). Do not claim full stack parity
or unmeasured performance. End at a committed, validated checkpoint with the
next action recorded.
