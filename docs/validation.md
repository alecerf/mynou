# CI, releases and download verification

GitHub Actions is the only place where Mynou is tested, built and published.
Contributors push commits and read the results; they never run tests, linters,
builds or binaries locally (see [AGENTS.md](../AGENTS.md)). A run is evidence
for its exact commit only: an earlier green run does not validate later changes.

## Workflows

| Workflow | Runs on | Purpose |
| --- | --- | --- |
| Mynou CI | Pull requests; a pushed `v<version>` tag | Dependency graph, format, Clippy, all tests, the macOS arm64 build and demo; a version tag publishes after the same checks |
| Engineering checks | Pull requests | Organization policy and engineering tooling scenarios |
| Release policy | Pull requests | Version changes only in a release PR |
| Security audit | Pull requests, `trunk`, hourly | Reachable Git objects and Actions logs |
| Format source branches | Pushes to `format/**` | Runs rustfmt with Rust 1.99.0 and commits the edits; validates and publishes nothing |

## Mynou CI

The workflow in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) has
five jobs. Mynou ships for macOS on Apple Silicon only, so `validate` and `build`
run on an Apple Silicon `macos-26` runner. A small Linux **decide** job runs
beside the checks. Nothing runs for a commit pushed to `trunk`: protection
requires an up-to-date branch and rebase merging, so `trunk` holds the tree its
pull request already validated. Pushing the tag `v<version>` runs every check
again on the tagged commit, and the release job publishes only when the tag
equals the `Cargo.toml` version (the binary reports that version) and the commit
is on `trunk`. A failed decision publishes nothing and fails `package`.

1. **validate** checks the Cargo graph offline (exactly one package, `mynou`,
   with no dependencies), formatting, Clippy with warnings denied, the test
   scheduler. The engineering policy and its tests run in the required Linux
   `organization` check. It then runs every Cargo test harness,
   using the physical runner directory as temporary directory because Mynou refuses the
   symbolic link behind macOS's default one.
2. **build** produces the published `aarch64-apple-darwin` executable. CI checks
   that the binary has the exact target architecture and runs the standalone
   demo, which must reach `ready` with simulated Plex confirmation.
3. **package** waits for validation and the build of the same commit. It checks
   the release script's syntax, copies the checked executable under its release
   name, compares the copy byte for byte and writes `SHA256SUMS`. It only copies
   and checksums, so it runs on a Linux runner.
4. **release** runs only for a pushed version tag that passed the decision, after
   validate, build and package succeeded on the tagged commit.

Pull request runs validate, build and package but cannot publish. A newer pull
request head cancels the older run; tag runs are never cancelled, so a later
push cannot interrupt a publication. Network tests use synthetic media, loopback peers and simulated
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

Only successful runs on `trunk` (a manual dispatch there) save caches, and pull
requests restore them only when one exists, so a cold cache costs a PR about 80
seconds in parallel jobs. Keys
are exact fingerprints of the manifests, sources, tests, examples and workflow,
with no partial-key fallback. The test cache holds
compiled harnesses, their build manifest and timing history; every test still
runs. Release caches hold only the target binary; its checks and demo still run.
Caches never contain a journal, downloads, a library or personal configuration,
and they never authorize a release.

## Releases

Merging to `trunk` does not release anything. At most once a week, a release PR
bumps the version and gathers the notes from `docs/releases/unreleased/`; once it
is merged, pushing the tag `v<version>` publishes it; see the
[engineering runbook](../engineering/README.md#releases). The `Release policy`
check refuses version changes anywhere else.

The release job runs
[publish-release.sh](../.github/scripts/publish-release.sh), which checks the
packaged files and creates the GitHub release for the pushed tag from the
validated commit. Only this job has `contents: write`, using the ephemeral repository
token.

| Asset | Contents |
| --- | --- |
| `mynou-vVERSION-macos-arm64` | Native Apple Silicon executable |
| `SHA256SUMS` | One manifest covering the executable |

GitHub's own source downloads contain sources only and need a build. Earlier
releases also provided a Linux x86_64 executable, an Intel macOS executable and a
private container image, and releases before 0.22.19 had other assets (source
archives, image archives and per-file checksums); they remain unchanged.

Published tags and assets are immutable. A failed publication is re-run from the
failed job, never by replacing assets; a burned version moves to the next patch
in a new release PR.

## Verify release assets

Download the executable and `SHA256SUMS` from the same published release. Select
the executable's exact filename from the manifest, which also works for releases
that list more files. For example, after downloading the 0.22.19 Apple Silicon
binary:

```sh
awk '$2 == "mynou-v0.22.19-macos-arm64"' SHA256SUMS | shasum -a 256 -c -
```

A missing or incorrect selected entry fails verification; do not install or
execute after failure.

Checksums establish integrity relative to the same release's manifest, not Apple
signing or publisher identity. Review the exact source and successful Actions
run.
