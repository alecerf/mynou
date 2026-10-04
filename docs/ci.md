# CI execution, parallel tests and build reuse

Validation runs only in GitHub Actions. Never run tests, lint, builds, binaries,
demos or browser previews locally. Formatting edits are allowed. Commit changes,
push the completed work, inspect every required job and fix failures in new
commits. GitHub Actions alone creates tags/releases/assets.

## Independent work and publication gates

```mermaid
flowchart LR
    Commit --> Validate[Graph, format, Clippy and all tests]
    Commit --> Native[Native release build]
    Commit --> Static[Static release build and demo]
    Validate --> Package[Docker demo, archive and checksums]
    Native --> Package
    Static --> Package
    Package --> Release[CI publication]
```

Validation and both build targets run concurrently on separate runners. The
build matrix retains native GNU and static musl coverage. Packaging waits for
all three jobs to succeed for the same workflow commit; publication waits for
successful packaging. PR runs validate/build/package but cannot publish.

The static build also compiles the Rust standard-library-only packaging example.
The next job receives that tool and the checked binary as a workflow artifact,
so neither is recompiled for packaging. It explicitly passes the current source
checkout to the packaging tool, independent of the build runner's directory.

The Dockerfile defaults to its existing source build. CI selects the `prebuilt`
stage, assembling the same final scratch image with the checked static binary
and CA trust data. Docker syntax checks also cover the default build choice.
CI extracts `/mynou` from the resulting image and compares it byte-for-byte with
the standalone binary, then runs the existing isolated container demo. Native
and static checks, standalone/container demos, ZIP integrity, binary permissions
and all checksum checks remain required. Published versions stay immutable.

## Parallel test harnesses

Cargo already executes tests concurrently within one harness. Its normal
`cargo test --all-targets` invocation runs those harnesses sequentially. CI now
first compiles **all targets once**, retaining Cargo's JSON artifact manifest.
The original [scheduler](../.github/scripts/parallel_tests.py) discovers every
test executable from that successful manifest, including library, binary,
integration and example harnesses. It does not glob a build directory, skip
fresh cached artifacts, filter test cases or maintain a manual target list.
The manifest's complete source-target set must equal the current Cargo metadata
graph. An exact compiled-input cache hit can reuse harness binaries; all tests
still execute. Test builds omit debug symbols to reduce linking/storage overhead;
debug assertions and overflow checks retain their test-profile behavior. Release
build settings are unchanged.

The default process bound is the lesser of four and the runner CPU count. Each
harness uses up to two test threads, giving at most eight concurrent test cases
at the default bounds. Network fixtures may create their own bounded workers.
Temporary directories include process-specific unique identities and listeners
use ephemeral ports. Original assertions and fixture deadlines are unchanged.

Slower harnesses start first, using recorded per-target timing history. Checked-in
initial weights come from the previous green run; successful debug caches can
provide newer weights. Timing history affects ordering only. A malformed history
cannot suppress tests. Concurrency and timeout options can be tuned in CI, with
explicit bounds; this is not permission to execute the scheduler locally.

Each target has a 180-second process deadline by default. A timeout kills its
process group, including CLI fixture children. Remaining harnesses still run
after a failure so CI collects their outcomes. Every target needs a successful
exit and exactly one complete harness summary, with no failed, ignored or
filtered tests. A missing summary or oversized log fails the aggregate.

The runner keeps separate target logs, writes `report.json`, prints grouped logs
and adds a duration table to the Actions job summary. The `mynou-tests-COMMIT`
artifact retains these files for seven days, including failed test results.
Four standard-library scheduler checks exercise discovery, failure propagation,
actual overlap and timeout handling in CI. They are infrastructure checks,
separate from the Rust application test count. CI's existing Python standard
library supplies orchestration; Mynou retains zero Cargo/runtime dependencies.

## Caches

The workflow uses the official `actions/cache` 6.1.0 restore/save actions.
Only successful push jobs on `trunk` save caches; PR runs can restore existing
eligible caches. A cache miss always performs the normal build.

- The test cache is scoped by OS, architecture, repository, Rust version and an
  exact fingerprint of manifests, Cargo configuration/build script, every
  source/test/example file, deployment template and workflow. It retains compiled
  executables, their Cargo manifest and timing history, excluding incremental
  compilation state. There are no partial-key fallbacks. An exact hit avoids
  relinking the same harnesses; the current Cargo target graph still checks
  complete coverage and every test runs. Formatting, Clippy and scheduler checks
  always execute.
- Release caches contain only the target binary and, for musl, the packaging
  executable. Keys include OS/architecture, Rust version, target, manifests,
  Cargo configuration/build script, every source/example file, embedded
  deployment template and workflow. There
  are no partial-key fallbacks. Only an exact compiler-input match can reuse a
  binary; ELF checks and the standalone demo still run. Source/workflow changes
  invalidate the key. Source-build settings must remain part of this fingerprint
  when new build inputs are introduced.

The caches contain no journal, downloads, library or personal configuration.
Generated Docker input and Python bytecode are excluded from source publication.
Caches support build reuse; they never replace runtime checks or authorize a
release. Previously published tags and assets are not rewritten.

## Timing evidence

The previous green [run 37231391564](https://github.com/alecerf/mynou/actions/runs/37231391564),
at commit `c25da060d94965a9f2aa4926a46962a438b6de17`, recorded 409 passing Rust
tests across 34 harnesses. Its serial validation job took 299 seconds:

| Step | Seconds |
| --- | ---: |
| Tests, including compilation | 101 |
| Test compilation within that step | 25.65 |
| Harness execution, sum of reported durations | 74.98 |
| Native and musl release builds | 74 |
| Docker build/demo/save | 91 |
| Duplicate Rust compilation inside Docker | 74.24 |

The optimized cold-cache [run 37233284333](https://github.com/alecerf/mynou/actions/runs/37233284333)
passed for commit `8b29506158f43c6346d1d5a9d68496c5d6bfb332`. All 409 Rust tests
passed across the same 34 harnesses, with none failed or ignored. Four scheduler
checks also passed. This runner used two harness processes with two threads each.
The test compilation took 19.88 seconds and harness execution took 39.392 seconds;
the complete validation job took 88 seconds. Native and static build jobs took
55 and 61 seconds concurrently. Docker assembly/demo/packaging passed, including
byte-for-byte comparison with the checked static binary. Publication remained
gated and left the existing 0.15 release immutable.

| Observed duration | Previous workflow | Optimized, cold cache |
| --- | ---: | ---: |
| Rust harness execution | 74.98 s | 39.392 s |
| Validation job | 299 s | 88 s |
| Entire workflow, creation through completion | 313 s | 130 s |

Whole-workflow time fell by approximately 58% in this comparison. Validation
job time is not the whole optimized workflow: builds now run independently,
and packaging/publication follow them. Cold and warm runs must be reported
separately; cache downloads, scheduling and runner load affect elapsed time.
The first warm follow-up passed every check and reused release binaries, but
the initial general debug cache restored 274 MiB in 13 seconds and still rebuilt
tests in 22.17 seconds. That full workflow took 145 seconds. The revised exact
test cache and symbol-free test builds must pass their own cold/warm workflows
before final timings are recorded. These measurements describe CI duration,
not application throughput or general performance.

[Validation](validation.md) · [Dependencies](dependencies.md) ·
[Performance](performance.md) · [Next feature release](next-release.md)
