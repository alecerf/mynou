# Next-release checkpoint

## Validated starting point

Mynou 0.13.0 was published by GitHub Actions from
`cb6e89700a63c1a7f9aaaa644fce32bf8944386f` after
[run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435)
completed successfully. Its 374 tests passed with no failures or ignored tests
across 29 targets. The source ZIP, static Linux x86_64 binary, Docker image archive
and four checksum assets belong to that exact release commit.

0.12.0 provides durable series monitoring/calendar; 0.13.0 provides explicit
catalog-backed file mappings and shared whole-torrent acquisition. A guarded
operator correction can requeue an unimported failed/cancelled pack request.
Normal workers cannot change mappings. Earlier unmapped jobs retain ordinary
behavior. [Series](series.md), [packs](packs.md) and [limits](limits.md) document
the implemented surface; the [roadmap](roadmap.md) tracks later stack features.

No 0.14 implementation is recorded at this checkpoint. The following is a
concrete engineering plan, not a feature claim or a validated result.

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

## Native selective-file prerequisites

The next stage must preserve `DownloadStatus.ready` as whole-torrent readiness.
Introduce a separate explicit result for selected-file verification; do not let
a partial transfer become a fully ready torrent or advertise absent pieces.

Relevant code is `src/torrent.rs`, `src/torrent/control.rs`,
`src/torrent/parallel.rs` and `src/torrent/metainfo.rs`. Current file priorities
only reorder pieces and every payload file remains required. Current parallel
completion and final verification require all pieces/roots. Selection must be
durable and bounded, with the earlier missing field retaining full acquisition.

For v1, selected files need all overlapping hash-verified pieces, including
unselected boundary bytes. Decide explicitly how those boundary bytes are stored
and recovered without treating skipped files as complete. For v2, selected files
need valid file roots and authenticated proofs; hybrid selection must satisfy
both v1 boundary hashes and selected v2 roots. Preserve padding handling,
zero-length files, generation ownership and verified disk-write controls.

Requests sharing a torrent need a bounded union of their file interests. One
request's cancellation or selection change must not discard another request's
required pieces. A deliberate full-torrent request still needs every file.
Retain pause, priorities, aggregate bandwidth, counters and seeding controls.
Prevent payload scheduling from assuming full acquisition while magnet metadata
and path-to-index selection are still unresolved. A missing explicit mapped path
must report a decision/error rather than silently selecting another file.

Use meaningful local-peer CI scenarios for boundary pieces, padding, v1/v2/hybrid
verification, late metadata, shared interests, cancellation, pause and policy
changes, corrupt peers, selection expansion and restart. Assert partial readiness
and full readiness independently, with no invalid bitfield or seeding claim.

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
when adding automatic selection and per-file readiness.

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

The roadmap's 0.14 scope may be split further if native verification and mapping
decisions need separate releases. Keep the implemented support matrix precise.
Bump Cargo/lockfile, both Compose files, deployment examples and release notes
only when the concrete scope is ready for its own CI run. Preserve the static
musl build, native/Docker demonstrations and archive/checksum validation.

Record exact successful source/run/tag/asset evidence after publication. Keep
earlier published versions immutable. End quota-limited work at a committed,
validated checkpoint with the next action stated; never claim access to account
quota or an unattended continuation mechanism that is unavailable.
