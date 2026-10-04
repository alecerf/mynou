# Next-release checkpoint — 0.17 shared multi-episode files

v0.16 implements explicit numbering: retained catalog IDs keep canonical library
numbers while approved catalog/source labels may change. CLI/API/browser preview
and guarded apply persist choices before future jobs capture source labels.
Existing requests, exclusions and library paths stay fixed. See [numbering](numbering.md).

This version is prepared for CI; its complete workflow and publication must be
recorded before claiming release evidence. The last published release is 0.15.0
from `1fe40eed0b0ea170a03ffce8d30d2ab8cb3e7125`, validated by
[run 37230875486](https://github.com/alecerf/mynou/actions/runs/37230875486)
with 409 Rust tests across 34 targets and seven CI-published assets. Those tags
and assets stay immutable. [Validation](validation.md) retains historical digests.

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

## Define shared physical ownership before accepting duplicate pack paths

One verified video can represent several logical episodes. Keep the existing
one-file-per-episode validator until durable shared import ownership is complete.
Media metadata is not a decoder or a means to split videos. Retain sources and
earlier imports; overwriting and automatic deletion remain outside this stage.

1. Bind a bounded physical identity to authenticated torrent hash, exact path and
   canonical logical owners. Define consecutive episode/season rules and a
   deterministic Plex naming convention while preserving numbering anchors.
2. Persist ownership before acquisition. Concurrent imports, retry and restart
   must reuse one verified shared path without duplicate copies or overwrites.
   Define how later requests for only some owners interact with prior ownership.
3. Confirm the exact Plex path for every owner. Cancellation retains interests
   and paths required by others; retries cannot reassign physical bytes.
4. Define upgrades before allowing shared owners into individual upgrade logic.
   Keep earlier versions current until all required replacements are ready.
5. Add guarded explicit CLI/API/browser preview/apply before loosening duplicate
   pack paths or automatic mapping. Preserve source/hash guards, selected-file
   verification and the 64-job acquisition batch.
6. Add original CI cases for concurrent shared import, cancellation, partial
   ownership, restart, stale plans, Plex confirmation and upgrades. Split further
   if a concrete smaller release can be independently validated.

## Release acceptance and handoff

Bump Cargo/lockfile, Compose/deployment examples and release notes once complete.
Preserve offline graph, formatting, Clippy, all Rust tests, GNU/musl builds,
isolated native/container demonstrations and archive/checksum checks. Record the
exact source/run/tag/asset evidence after CI publication; later docs need CI too.

Then continue Plex user policies, IRC automation, native indexer adapters, Usenet
and cross-seeding on the [roadmap](roadmap.md). Do not claim full stack parity
or unmeasured performance. End at a committed, validated checkpoint with the
next action recorded.
