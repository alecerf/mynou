# Next-release checkpoint — 0.18 coordinated shared-group replacements

v0.16 implements explicit numbering: retained catalog IDs keep canonical library
numbers while approved catalog/source labels may change. CLI/API/browser preview
and guarded apply persist choices before future jobs capture source labels.
Existing requests, exclusions and library paths stay fixed. See [numbering](numbering.md).

Mynou 0.16.0 was published by CI from
`0821a4d3b499a5863fe5b50206c98bda25d6fb49` after
[run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691) passed:
430 Rust tests across 36 targets, four scheduler checks and all five workflow
jobs. Seven assets were uploaded by `github-actions[bot]` on October 4, 2026, at
21:59:49 UTC. The tag targets that validated commit. See
[validation evidence](validation.md#recorded-0160-ci-evidence) for asset digests.

Earlier releases stay immutable; the 0.15 source, tag and seven asset digests
remain recorded in [validation](validation.md). Later documentation changes
require their own complete CI and never replace published artifacts.

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

## Shared ownership implemented in 0.17; validate before continuing

The 0.17 implementation now records a bounded authenticated torrent/path,
canonical owner range and one deterministic Plex destination. All new owners
commit atomically; runtime claims are serialized per group. Selected-file
verification, no-overwrite imports, individual exact Plex checks, queued-owner
cancellation interests, retry/restart and subset reuse retain the binding.
Format 2 rejects silent downgrade. CLI/API/browser apply guards repeat metadata
inspection; browser source credentials remain server-side for ten minutes.
See [shared files](shared-files.md). Its complete Actions run and CI publication
remain required before beginning 0.18; earlier green runs do not validate it.

Individual shared remaps, baselines and upgrades remain blocked. Automatic packs
and ordinary mapped packs still require unique single-episode files. This gives
0.17 an independently usable scope without partially replacing shared owners.

## Implement coordinated group replacements in 0.18

1. Define immutable old-group to replacement-group lineage, whole-group release
   baselines and quality decisions. Capture the complete canonical owner set;
   a partial request cannot implicitly discard other owners.
2. Persist every replacement and its full lineage before acquisition in one
   bounded transaction. Keep old entries current until all required replacements
   complete native verification, import and exact Plex confirmation.
3. Make group promotion atomic, recoverable and idempotent across worker races,
   retry, cancellation, monitoring changes and restart. Retain all earlier bytes;
   no overwrite or automatic deletion. Define shared-to-individual replacements
   before exposing them.
4. Add guarded CLI/API/browser preview/apply for the coordinated operation.
   Automatic range-file recognition can follow only when it obeys that ownership
   and complete-scope model. Alternate source numbering remains explicit.
5. Add original synthetic CI cases for incomplete groups, stale policy/source
   guards, partial Plex confirmation, failed/cancelled replacements, retries,
   restart and terminal promotion. Keep the 64-job action bound and complete
   Cargo harness execution.

## Release acceptance and handoff

Bump Cargo/lockfile, Compose/deployment examples and release notes once complete.
Preserve offline graph, formatting, Clippy, all Rust tests, GNU/musl builds,
isolated native/container demonstrations and archive/checksum checks. Record the
exact source/run/tag/asset evidence after CI publication; later docs need CI too.

Then continue Plex user policies, IRC automation, native indexer adapters, Usenet
and cross-seeding on the [roadmap](roadmap.md). Do not claim full stack parity
or unmeasured performance. End at a committed, validated checkpoint with the
next action recorded.
