# Project checkpoint — Mynou 0.7.0

The active implementation rule is Rust with its standard library alone: no crates,
bundled third-party code, FFI, `unsafe`, external runtime programs, or fallback to
Go. The rewrite replaces SQLite, ffprobe, and the Go torrent engine. Data from
previous versions stays separate and is not migrated implicitly.

All project prose and diagnostics must be English. The active development policy
prohibits local tests and lint. Make meaningful commits, push to `trunk`, inspect
GitHub Actions, and fix failures until the required checks pass. GitHub Actions
alone creates release tags and publishes artifacts from validated commits.

## Implemented scope

Media parsers, the torrent engine, persistence, orchestration, integrations,
CLI/API, and Docker deployment are implemented. Rust 1.99.0 is the pinned
build toolchain. The Cargo graph must contain one package, `mynou`, with
`dependencies: []`.

The 0.7.0 change adds named movie/episode selection profiles, suffix-based
resolution/source/codec/language markers, required and blocked token phrases,
additive scores and deterministic ranking. `search` and `POST /api/search`
preview automatic source selection without submitting a job; their public
reports omit acquisition URLs and credentials. Missing `selection` configuration
retains unrestricted behavior. See [selection.md](selection.md).

Automatic upgrades, torrent controls and parallel peers, a web interface,
series-pack/specials management, multi-user Plex policies, IRC automation,
native indexer adapters, Usenet and cross-seeding remain future stages. See the
[release roadmap](roadmap.md). Selection is the first stage, not full parity.

Releases are hosted in [alecerf/mynou](https://github.com/alecerf/mynou/releases).
A successful validation run on `trunk` publishes a new `Cargo.toml` version if it
has not already been released. The assets are a full source ZIP with the static
Linux x86_64 binary, a separate static binary, a saved Docker image archive, and
SHA-256 checksum files. There is no Docker Hub publication step.

## Recorded 0.6.1 CI checkpoint

Commit `2a066c2c3b9aafda47a3fc898872d1c371b5f249` passed
[Actions run 37134671116](https://github.com/alecerf/mynou/actions/runs/37134671116):
131 tests plus formatting, Clippy, builds, demos and packaging. GitHub Actions
published [v0.6.1](https://github.com/alecerf/mynou/releases/tag/v0.6.1).
This preceding release does not validate the 0.7.0 changes. Record or inspect
their own successful Actions run before claiming validation or publication.

## Historical 0.6.0 evidence

The former delivery was validated on October 3, 2026, before the CI-only policy.
It passed 131 tests without failures or ignored tests, formatting, warning-free
Clippy across all targets, and GNU/musl release builds. The tests included the
race between request cancellation and torrent startup. Its musl binary was
1,717,088 bytes and had no ELF interpreter or required dynamic library.

Its final `scratch` image was 1,898,812 bytes, ran as UID/GID 1000:1000, and had ID:

```text
sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146
```

Layer inspection found only the binary and CA bundle as embedded regular files.
Its demo completed a local magnet download, import, and simulated Plex
confirmation with `--network none`, a read-only root, and Linux capabilities
removed. A generated Compose installation was checked without Rust on the host:
health, doctor, API, byte-identical import, and the same request ID and `ready`
state after restart. The validation containers were removed afterward.

The historical release benchmark ran 10,000 warm-cache analyses of the demo MP4:
8.98 µs per analysis, approximately 111,333 analyses/s, SHA-256 at 177.14 MiB/s,
and SHA-1 at 250.74 MiB/s in a shared environment with two logical processors.
See [performance.md](performance.md) and its unchanged raw JSON.

These results describe **0.6.0**. Do not reuse them as proof that later changes
passed CI. Use the Actions run for the current commit and release.

## Remaining installation configuration

Connecting personal Plex requires its address, section IDs, shared paths, Plex
token, and TMDB/source credentials. Local-service tests are not a deployment on
that personal installation.

No available tool exposes the account's ChatGPT quota consumption. This file
supports an explicit handoff; it does not claim automatic quota monitoring or
resumption after a reset.
