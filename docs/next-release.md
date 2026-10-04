# Next-release checkpoint — 0.15 automatic pack acquisition

## Validated starting point

Mynou 0.14.0 was published by GitHub Actions from
`a077af8d660a2b5ca12e47b579562e7a9292f6e2` after
[run 37221887812](https://github.com/alecerf/mynou/actions/runs/37221887812)
completed successfully. Its 385 tests passed with no failures or ignored tests
across 31 targets. The source ZIP, static Linux x86_64 binary, Docker image archive
and four checksum assets belong to that exact release commit.

0.12.0 provides durable series monitoring/calendar; 0.13.0 provides explicit
catalog-backed file mappings and shared whole-torrent acquisition. A guarded
operator correction can requeue an unimported failed/cancelled pack request.
Normal workers cannot change mappings. Earlier unmapped jobs retain ordinary
behavior. [Series](series.md), [packs](packs.md) and [limits](limits.md) document
the implemented surface; the [roadmap](roadmap.md) tracks later stack features.

0.14 adds durable shared native file interests, selective v1/v2/hybrid acquisition,
verified mapped imports from partial torrents and protected CLI/API/browser
selection expansion. Its complete CI and publication passed on October 4, 2026,
at 17:53 UTC. [Validation evidence](validation.md#recorded-0140-ci-evidence)
records the release assets and digests. No local validation has run. Later
documentation commits need their own CI and do not change the released tag.

The remaining automatic-pack and numbering sections describe planned later work.

## Preserve the development policy

Read `AGENTS.md` before changing code. Use Rust 1.99.0 and its standard library
only, with zero Cargo dependencies of any kind. Keep all project text in English.
Never run local tests, lint, builds, binaries, demos or browser previews.
Formatting edits with `cargo fmt --all` are allowed. Commit meaningful chunks,
push the completed release, inspect GitHub Actions, and fix red runs in new
commits. Move to another release only after the complete validation/publication
workflow is green. GitHub Actions alone creates tags, releases and assets.

Inspect the actual branch/run before assuming this historical checkpoint is the
latest state. The repository is `alecerf/mynou`, branch `trunk`. Account-specific
credentials and personal Plex/source settings are not part of the repository.
No available account interface exposes the remaining ChatGPT quota. A saved
checkpoint does not imply automatic monitoring or resumption after reset.

## Implemented selective-file slice (0.14)

`src/torrent/selection.rs` retains full acquisition or a bounded path union in
verified native controls, with absent older fields defaulting to full. Creation
persists controls before queue-visible metadata/source files. File interests
only expand; cancellation retains them. A correction can remove only interests
proven absent by authenticated metadata, preserving real shared interests.

The parallel coordinator handles selected transfers even at one peer. It
prepares selected files and boundary neighbors, schedules overlapping pieces,
verifies selected v2 roots and hybrid hashes, synchronizes affected files and
publishes available paths separately from full `DownloadStatus.ready`. Expanding
selection retires the old generation. Restart rehashes bytes. Partial torrents
advertise an empty bitfield, do not seed and stop tracker activity without a
completion event. Normal full requests and existing full controls remain full.

`Engine` requests mapped path interests before payload scheduling and imports
only published verified paths. CLI `torrent-select`, API selection updates and
browser include/all forms expand interests with existing guards and pause policy.
See [transfer selection](transfers.md#selective-acquisition-in-0140) for bounds,
boundary storage and downgrade restrictions. Selection contraction and partial
seeding remain unimplemented.

Passing CI scenarios in `selective_transfers`, `selective_management`, mapped
acquisition and tracker lifecycle cover the implemented behavior in the recorded
0.14 run. Continue from these protections when implementing 0.15.

## Pack search and mapping decisions

Automatic pack search must keep strict series/season identity and existing movie
and episode profile rules. Add a distinct pack candidate assessment rather than
loosening individual episode matching globally. Search previews must explain
accepted/rejected candidates, respect existing budgets, expose no acquisition
URLs and avoid recording jobs or starting payload downloads.

Before an automatic pack creates episode jobs, resolve its file metadata against
the accepted catalog plan. Only unique known episode identities may be selected.
Conflicting markers, missing files, duplicate episode assignments and unresolved
alternate numbering need an explicit decision. Persist any accepted mapping and
its captured series revision before payload/import work. Do not infer baseline
quality from a pack's ambiguous title or silently retry terminal requests.

`src/pack.rs` currently prevalidates explicit operator input, catalog scope,
source-key collisions and remaining job capacity. `src/store.rs` records mappings
with ordinary episode jobs. `src/engine.rs` selects exact native paths after
verification and retains only those paths in each job. Reuse those protections
when adding automatic pack selection.

## Alternate numbering and multi-episode files

Model explicit source-to-catalog numbering choices with stable catalog identities
and bounded durable data. Current known-ID/number changes reject refreshes; they
must remain blocked until an approved mapping resolves the ambiguity. Preserve
earlier media identities and current library records when catalog numbers change.
Do not guess anime absolute numbering from an isolated filename or silently
reassign previously imported episodes.

One physical video may cover several logical episodes. Decide its import naming,
logical ownership, Plex path confirmation and upgrade behavior before allowing
several mappings to the same file. The current one-file-per-episode constraint
must remain until that model is implemented. Metadata analysis is not a video
decoder or a tool for cutting multi-episode videos. Sources and earlier imports
remain intact; overwriting or automatic deletion is outside this stage.

## Release acceptance and handoff

The roadmap splits selection into 0.14, automatic pack assessment/mapping into
0.15, and numbering/multi-episode files into 0.16. Keep the implemented support matrix precise.
Bump Cargo/lockfile, both Compose files, deployment examples and release notes
only when the concrete scope is ready for its own CI run. Preserve the static
musl build, native/Docker demonstrations and archive/checksum validation.

Record exact successful source/run/tag/asset evidence after publication. Keep
earlier published versions immutable. End quota-limited work at a committed,
validated checkpoint with the next action stated; never claim access to account
quota or an unattended continuation mechanism that is unavailable.
