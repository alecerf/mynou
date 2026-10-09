# CI, releases and download verification

GitHub Actions is the only place where Mynou is tested, built and published.
Contributors push commits and read the results; they never run tests, linters,
builds or binaries locally (see [AGENTS.md](../AGENTS.md)). A run is evidence
for its exact commit only: an earlier green run does not validate later changes.

## Workflows

| Workflow | Runs on | Purpose |
| --- | --- | --- |
| Mynou CI | Pull requests, `trunk` | Dependency graph, format, Clippy, all tests, release builds, demos, container; publishes a new version from `trunk` |
| Engineering checks | Pull requests, `trunk` | Organization policy and engineering tooling scenarios |
| Release policy | Pull requests | Version changes only in a release PR |
| Security audit | Pull requests, `trunk`, hourly | Reachable Git objects and Actions logs |
| Format source branches | Pushes to `format/**` | Runs rustfmt with Rust 1.99.0 and commits the edits; validates and publishes nothing |

## Mynou CI

The workflow in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) has
four jobs:

1. **validate** checks the Cargo graph offline (exactly one package, `mynou`,
   with no dependencies), formatting, Clippy with warnings denied, the
   live-progress browser fixture (Node without npm packages), the release
   script's syntax, the test scheduler and the engineering policy. It then runs
   every Cargo test harness.
2. **build** produces the three published executables on separate runners: a
   static `x86_64-unknown-linux-musl` binary, and `aarch64-apple-darwin` and
   `x86_64-apple-darwin` binaries on Apple Silicon `macos-26` runners. The Intel
   binary is cross-compiled and its demo runs under Rosetta 2. CI checks that the
   Linux binary has no ELF interpreter or shared library and that each macOS
   binary has the exact target architecture. Every target runs the standalone
   demo, which must reach `ready` with simulated Plex confirmation.
3. **package** waits for validation and all builds of the same commit. It builds
   the container from the checked static binary (the Dockerfile's `prebuilt`
   stage) without recompiling, compares the binary in the image byte for byte,
   runs the container demo without network access, with a read-only root and no
   capabilities, then writes the three executables and `SHA256SUMS`.
4. **release** runs only on `trunk`, and only when `Cargo.toml` carries a version
   without a `v<version>` tag. Only a merged release PR introduces such a
   version, so every other `trunk` commit is validated without a release.

Pull request runs validate, build and package but cannot publish. A newer pull
request head cancels the older run; `trunk` runs are never cancelled, so a later
merge cannot interrupt a publication. Let a release publish before merging the
next change. Network tests use synthetic media, loopback peers and simulated
services; CI never acquires public media.

## Test harnesses

CI compiles all targets once, keeping Cargo's JSON build manifest. The
[scheduler](../.github/scripts/parallel_tests.py) runs every test executable in
that manifest, whose targets must match the current Cargo metadata graph. It
does not filter tests or keep a manual target list. By default it runs up to four
harness processes (no more than the CPU count) with two test threads each, and
gives each harness 180 seconds before killing its process group. Every harness
must exit successfully with exactly one complete summary and no failed, ignored
or filtered tests; a missing summary or an oversized log fails the run.

Slower harnesses start first, using recorded timings. Timings change the order
only, never the set of tests. Logs, `report.json` and a duration table are kept in
the `mynou-tests-<commit>` artifact for seven days, including failed runs. Test
builds omit debug symbols; debug assertions and overflow checks stay enabled.

## Caches

Only successful `trunk` runs save caches; pull requests can restore them. Keys
are exact fingerprints of the manifests, sources, tests, examples, deployment
template and workflow, with no partial-key fallback. The test cache holds
compiled harnesses, their build manifest and timing history; every test still
runs. Release caches hold only the target binary; its checks and demo still run.
Caches never contain a journal, downloads, a library or personal configuration,
and they never authorize a release.

## Releases

Merging to `trunk` does not release anything. At most once a week, a release PR
bumps the version and gathers the notes from `docs/releases/unreleased/`; see the
[engineering runbook](../engineering/README.md#releases). The `Release policy`
check refuses version changes anywhere else.

The release job first publishes the checked Linux amd64 image to the private
`ghcr.io/alecerf/mynou` package. The publisher verifies the repository and
package identity, refuses conflicting tags or labels, reuses an existing exact
tag without pushing, then pulls the recorded digest, compares its executable and
repeats the container demo. Only then does
[publish-release.sh](../.github/scripts/publish-release.sh) create the tag and
the GitHub release from the validated commit. Only this job has `contents: write`
and `packages: write`, using the ephemeral repository token.

| Asset | Contents |
| --- | --- |
| `mynou-vVERSION-linux-x86_64` | Static Linux x86_64 executable |
| `mynou-vVERSION-macos-arm64` | Native Apple Silicon executable |
| `mynou-vVERSION-macos-x86_64` | Native Intel macOS executable |
| `SHA256SUMS` | One manifest covering the three executables |

The release notes record the image's content digest. Registry tags can be
changed by authorized writers, so pin the digest. GitHub's own source downloads
contain sources only and need a build. Releases before 0.22.19 had other assets
(source archives, image archives and per-file checksums) and remain unchanged.

Published tags and assets are immutable. A failed publication is re-run from the
failed job, never by replacing assets; a burned version moves to the next patch
in a new release PR. If GitHub refuses to create a release because the workflow
on `trunk` changed since the run started, keep the validated commit on a source
branch (for example `release-source/<version>`) before re-running the failed job.

## Verify release assets

Download your executable and `SHA256SUMS` from the same published release.
Select its exact filename from the manifest so other architectures need not
be downloaded. For example, after downloading the 0.22.19 Apple Silicon binary:

```sh
awk '$2 == "mynou-v0.22.19-macos-arm64"' SHA256SUMS | shasum -a 256 -c -
```

Use `sha256sum -c -` on Linux. A missing or incorrect selected entry fails
verification; do not install or execute after failure. Downloading all three
executables also permits `sha256sum -c SHA256SUMS`.

Checksums establish integrity relative to the same release's manifest, not Apple
signing or publisher identity. Review the exact source and successful Actions
run. Container deployment uses the verified content digest; registry version
tags are mutable by authorized writers. See [Docker installation](deployment.md).
