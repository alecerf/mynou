# CI, releases and download verification

Validate locally first with the installed Rust and Python tools. Use native
macOS for platform-specific checks. See the
[local validation commands](#local-validation). Batch
validated changes before pushing to minimize CI runs. GitHub Actions performs
required final checks and publishes releases. A run is evidence for its exact
commit only: an earlier green run does not validate later changes.

## Workflows

| Workflow | Runs on | Purpose |
| --- | --- | --- |
| Mynou CI | Pull requests; manual dispatch; a pushed `v<version>` tag | Select tests, macOS arm64 build, start check and packaging from the diff; manual runs validate everything, releases only build and publish |
| Repository checks | Pull requests; manual dispatch | Python tooling and scheduler tests when their inputs change |
| Release policy | Pull requests | Version changes only in a release PR |
| CodeQL | Pull requests, pushes to `trunk`, weekly schedule, manual dispatch | Required security analysis; Rust and JavaScript selected by diff |
| WireGuard interoperability | Manual dispatch only | Optional isolated independent-peer test |

## Mynou CI

The workflow in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) has
a shared change selector and five validation/release jobs. Mynou ships for macOS
on Apple Silicon only, so `validate` and `build`
run on an Apple Silicon `macos-26` runner. A small Linux **decide** job runs
beside the checks. The native CI does not run for a commit pushed to `trunk`: protection
requires an up-to-date branch and signed fast-forward integration, so `trunk` holds the tree its
pull request already validated. A release changes nothing but the version, so
its PR and the tag `v<version>` run no tests, start check or Rust and JavaScript
CodeQL: they build the tagged commit, check the binary's architecture and
version, package and publish. The release job publishes only when the tag
equals the `Cargo.toml` version (the binary reports that version), the commit is
on `trunk`, and it raised the version over its parent. A failed decision publishes nothing, and on a pull request it fails `package`.

1. **validate** checks the Cargo graph offline (exactly one package, `mynou`,
   with no dependencies), formatting, the
   [torrent network inventory](network-inventory.md) and Clippy with warnings denied. Scheduler
   and release tooling tests run in the Linux `organization` check.
   It then runs every Cargo test harness,
   using the physical runner directory as temporary directory because Mynou refuses the
   symbolic link behind macOS's default one.
2. **build** produces the published `aarch64-apple-darwin` executable. CI checks
   that the binary has the exact target architecture, then starts it on an
   empty data directory: it must become ready, answer `GET /api/status` and
   the web interface, refuse a request naming another host and stop cleanly on
   `POST /api/shutdown`. The complete acquisition path is covered by the test
   harnesses, which use `tests/fixtures/demo.mp4`.
3. **package** waits for the build and any selected validation of the same
   commit. It checks
   the release script's syntax, copies the checked executable under its release
   name, compares the copy byte for byte and writes `SHA256SUMS`. It only copies
   and checksums, so it runs on a Linux runner.
4. **release** runs only for a pushed version tag that passed the decision, after
   build and package succeeded on the tagged commit.

Pull requests run only the selected checks and cannot publish. A newer pull
request head cancels the older run; tag runs are never cancelled, so a later
push cannot interrupt a publication. Network tests use synthetic media, loopback peers and simulated
services; CI never acquires public media.

## Selecting checks from changes

[changes.yml](../.github/workflows/changes.yml) uses
[ci_changes.py](../.github/scripts/ci_changes.py) in each calling workflow.
PR selection compares the merge base with the current PR head, using the full
history and both sides of renames/deletions. It has no GitHub path-filter
300-file limit. The checks run against the usual checkout merge commit.

| Changed inputs | Rust validation | Native build/start + package | Python checks |
| --- | --- | --- | --- |
| Markdown under `docs/`, root Markdown files, issue templates | Skip | Skip | Skip |
| `src/**`, Cargo manifests/lockfile, Rust toolchain/configuration, `build.rs` | Run | Run | Skip |
| `tests/**` only | Run | Skip | Skip |
| CI scheduler, release tooling, interoperability driver, Python tests | Skip | Skip | Run |
| Crypto fixture generator | Run | Skip | Run |
| Release publisher shell script | Skip | Run | Run |
| Native CI workflow or shared selector | Run | Run | Run |
| Other workflows/actions | Skip | Skip | Run |
| Unknown inputs | Run | Run | Run |
| Release: only the package version in `Cargo.toml`/`Cargo.lock`, with `docs/releases/unreleased.md` deleted, or a version tag | Skip | Build + package, no start check | Skip |
| Manual dispatch | Run | Run | Run when that workflow is dispatched |

The selection is additive. Embedded UI files and certificates are build/test
inputs even when no `.rs` file changes; test media under `tests/` are test
inputs only. Test-only changes do not
produce an unused release artifact. A packaging-only change checks the native
binary and its reported version before packaging. A release keeps the build,
binary, version and publication checks; on its PR, CodeQL scans only the
workflows, because the branch rules require a CodeQL result.

Workflows remain triggered on every PR so their existing required job names
are reported even when skipped. Workflow-level `paths` filters can leave required
checks pending ([GitHub workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax)).
An unreadable diff selects all work. A selector job failure makes the required
native/Python checks fail. Release policy intentionally still runs on every PR,
including label changes: it checks version changes and unreleased-note policy.
WireGuard interoperability remains manual only.

## Activating selective CodeQL

Advanced setup was activated on 2026-10-18 after
[PR #264](https://github.com/alecerf/mynou/pull/264) passed every required check
and was merged. Default setup is disabled and repository variable
`MYNOU_CODEQL_ADVANCED` is `true`.
[The full baseline run](https://github.com/alecerf/mynou/actions/runs/39121048647)
analyzes Actions, JavaScript/TypeScript and Rust on `trunk` with the default suite.
[codeql.yml](../.github/workflows/codeql.yml) now owns triggers and selection.

The workflow selects Rust for `.rs`, Cargo, toolchain and `.cargo`
changes; JavaScript/TypeScript for its sources/manifests; and both when the
CodeQL configuration or shared selector changes. Each selected language scans
the complete codebase, with build mode `none`, the default queries and the
existing `/language:<language>` categories. Pushes to `trunk` update only the
changed languages' baselines; weekly and manual runs scan all three languages.

PRs retain a real Actions scan even for documentation-only changes. `trunk`
requires the `CodeQL` check from GitHub's code-scanning app (57789); an Actions
job named `CodeQL` cannot replace that app's check. This small scan keeps an
actual security check while avoiding the Rust/JavaScript scans on doc-only PRs.
No branch protection changes or synthetic security verdicts are used.

For a fresh installation, activate advanced setup after publishing the workflow:

1. Keep default setup enabled while reviewing/merging the workflow change.
2. Switch CodeQL from default to advanced setup in repository security settings,
   then set `MYNOU_CODEQL_ADVANCED=true`. GitHub requires default setup to be
   disabled before advanced results can be uploaded
   ([advanced setup documentation](https://docs.github.com/en/code-security/how-tos/find-and-fix-code-vulnerabilities/configure-code-scanning/configuring-advanced-setup-for-code-scanning)).
3. Dispatch `Selective CodeQL` on `trunk` to establish all three baselines.
4. Verify Rust, JavaScript and documentation-only PRs, including the required
   app-owned `CodeQL` check. Inspect stale configurations in the tool-status page
   if the former default configuration is still expected; do not weaken branch
   protection to bypass a missing analysis.

To roll back, unset the variable and restore default setup. The migration
preserves branch protection, existing alerts and their triage.

## Test harnesses

CI compiles all targets once, keeping Cargo's JSON build manifest. The
[scheduler](../.github/scripts/parallel_tests.py) runs every test executable in
that manifest, whose targets must match the current Cargo metadata graph. It
keeps no manual target or test list. It discovers all library test names, divides
them by top-level module and uses exact-name filters so every test executes once.
This gives each large persistence module its own finite deadline, rather than
timing out the cumulative duration of unrelated library tests. Other harnesses
run whole. By default it runs up to four group processes (no more than the CPU
count) with two test threads each. CI gives each group 480 seconds before killing
its process group (the standalone scheduler default is 180 seconds). Every group
must exit successfully with one complete summary and no failed or ignored tests.
Library groups must report exactly their discovered test names and counts; the
groups together must cover the full library without overlap or omissions. Tests
filtered from one library group execute in another. Whole harnesses may filter
nothing. A missing summary, incomplete coverage or oversized log fails the run.
Each group runs from its own writable temporary directory under `target/`, so
relative-path fixtures do not modify the checkout or collide with other groups.
Compiled-in fixture paths still point to the original source directory.

Slower harnesses start first, using recorded timings. Timings change the order
only, never the set of tests. Logs, `report.json` and a duration table are kept in
the `mynou-tests-<commit>` artifact for seven days, including failed runs. Test
builds omit debug symbols; debug assertions and overflow checks stay enabled.

## Caches

Only successful runs on `trunk` (a manual dispatch there) save caches, and pull
requests restore them only when one exists, so a cold cache costs a PR about 80
seconds in parallel jobs. Keys
are exact fingerprints of the manifests, sources, tests and workflow,
with no partial-key fallback. The test cache holds
compiled harnesses, their build manifest and timing history; every test still
runs. Release caches hold only the target binary; its checks and start check still run.
Caches never contain a journal, downloads, a library or personal configuration,
and they never authorize a release.

## Releases

Merging to `trunk` does not release anything. At most once a week, a release PR
bumps the version and deletes `docs/releases/unreleased.md`, the one file that
holds the notes of everything not yet released; once it is merged, pushing the tag
`v<version>` publishes it with a changelog generated from those notes, grouped by
category (Added, Changed, Deprecated, Removed, Fixed, Security) and ending with a
link to the full comparison. The `Release policy`
check refuses version changes anywhere else.

The release job runs
[publish-release.sh](../.github/scripts/publish-release.sh), which checks the
packaged files and creates the GitHub release for the pushed tag from the
validated commit, with the changelog generated from the notes in that commit's
parent. Only this job has `contents: write`, using the ephemeral repository
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

## Local validation

Use Python 3.11 or newer and the Rust toolchain pinned in
`rust-toolchain.toml`, already installed on the host. No credentials or Docker
are needed for routine development. Build and start locally with:

```sh
cargo run --offline --locked -- --help
cargo run --offline --locked -- serve
```

`serve` uses your existing data directory and its saved settings; pass
`--dir` with a scratch folder to try a change without touching them. For a quick
check after a tooling or documentation change:

```sh
cargo fmt --all --check
cargo check --all-targets --offline --locked
python3 -B -m unittest discover -s .github/scripts/tests -p 'test_*.py'
python3 -B .github/scripts/network_inventory.py
```

For product changes, run the relevant Cargo test target. To run the full suite
with the same scheduler as CI, use a fresh directory below `target/`:

```sh
mkdir -p target
validation_dir=$(mktemp -d "$(pwd -P)/target/local-validation.XXXXXX")
export TMPDIR="$validation_dir"
cargo clippy --all-targets --offline --locked -- -D warnings
python3 -B .github/scripts/crypto_fixtures.py "$validation_dir/crypto-vectors.json"
export MYNOU_CRYPTO_FIXTURES="$validation_dir/crypto-vectors.json"
cargo metadata --offline --locked --format-version 1 > "$validation_dir/metadata.json"
cargo test --all-targets --no-run --offline --locked --message-format=json-render-diagnostics > "$validation_dir/build.jsonl"
python3 -B .github/scripts/parallel_tests.py "$validation_dir/build.jsonl" --metadata "$validation_dir/metadata.json" --output "$validation_dir/results" --timeout 480
```

The physical temporary path avoids macOS's symlinked default temporary directory.
Persistence tests can be slow on physical disks because they request durable
flushes. The [optional WireGuard interoperability fixture](wireguard-interop.md)
retains Docker to isolate a privileged independent peer; it is separate from
local builds, runtime and routine checks.

### Preparing a release

Use a separate PR labeled `release` with one signed commit that changes only the
version in `Cargo.toml` and `Cargo.lock` and deletes `docs/releases/unreleased.md`.
Keep weekly cadence and shipped-change checks; `release-now` is reserved for a
security fix or explicit owner request. After review, green required checks and
integration into `trunk`, plan the tag with:

```sh
python3 .github/scripts/releases.py tag <merged-pr-number>
```

The command requires `GH_TOKEN` or `GITHUB_TOKEN`; keep it outside the repository
and test output. Review the reported commit and tag, then repeat with
`--create` to create the validated tag. Confirm Actions publishes the executable
and `SHA256SUMS`. Never move published tags or replace published assets; fix a
failed published version with a new patch release.
