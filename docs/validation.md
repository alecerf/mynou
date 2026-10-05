# CI validation and release verification

The development policy requires **GitHub Actions validation only**. Do not run
local tests or lint. Commit changes, push, inspect the run, and fix any failed
check with a new commit. Proceed once required checks are green.

The workflow is tracked in [`.github/workflows`](../.github/workflows), and run
results are available in [GitHub Actions](https://github.com/alecerf/mynou/actions).
A run is evidence for its exact commit. Historical results do not validate a later
revision.

## Automated checks and releases

The 0.19 requester scenarios exercise explicit opt-in and approvals, compatible
and conflicting accounts, quota/approval races, retries and UTC-day rollover,
partial polls and identity mismatch, stale policy/session reviews, redaction,
removals, notification deduplication and restart. Original local torrent/Plex
fixtures check acquisition ordering, captured profile/destination behavior and
exact path confirmation; existing routed Plex media requires no native download.
Canonical demand retains captured source numbering. Missing or corrupt provenance
fails before native startup. The 0.19.1 regression checks verified legacy imports
and title/profile compatibility before new reuse, with unchanged operator journals
and restart. Their exact completed runs and CI publications are recorded below;
later changes require their own complete workflow.

The 0.18 scenarios cover 64-owner baselines/creation/promotion, torn transactions,
signed incomplete lineage, partial-ready snapshots, cancellation of staged owners,
retry/obsolete-parent/competitor fences, monitoring changes, source/quality guards,
private browser reviews and offline/online CLI behavior. An original local native
peer/Plex journey keeps the old group current during partial path confirmation
and resumes staging after restart. Their result must come from a complete Actions
run for the exact source; prior release evidence does not validate them.

The 0.17 shared-file scenarios cover metadata-only guards, 64-owner atomic
creation, interrupted and semantically invalid persistence, subset reuse,
concurrent workers, source changes, queued-owner cancellation and restart,
pre-journal import recovery, per-owner exact Plex confirmation, individual upgrade
blocks, protected API/browser controls and offline/online CLI behavior. They use
original synthetic fixtures only. Their passing result must come from a complete
Actions run for the exact source; earlier release evidence does not validate them.

CI checks the offline Cargo graph, formatting, warning-free Clippy, tests, release
builds, a local end-to-end demo, and Docker behavior. The Cargo graph must contain
one package with no dependencies. Network tests use synthetic media and local
peers or services; no public torrent acquisition is part of validation.

The [CI execution guide](ci.md) describes bounded parallel execution of every
Cargo harness, retained timings/logs, independent native/static builds and exact
build reuse. Packaging and publication remain gated by every required job.

Release automation depends on successful validation on `trunk`. It reads
`Cargo.toml`, creates the version tag and GitHub release for the validated commit
if that version is new, and publishes:

| Asset | Contents |
| --- | --- |
| `mynou-vVERSION-source.zip` | Full sources, tests, docs, Docker files, synthetic demo, `bin/mynou`, and internal `SHA256SUMS` |
| `mynou-vVERSION-linux-x86_64` | Static Linux x86_64 binary |
| `mynou-vVERSION-linux-amd64-image.tar.gz` | Saved Docker image tagged `mynou:VERSION` |
| `SHA256SUMS` and `.sha256` files | SHA-256 integrity checks for release assets |

CI excludes personal data, secrets, build caches, and old Go sources from the
source archive. Release creation and artifact uploads are not manual development
steps. No Docker Hub repository is needed.

## Verify release assets

Download the assets and checksum files from the same release. For example:

```sh
sha256sum -c mynou-v0.15.0-source.zip.sha256
sha256sum -c mynou-v0.15.0-linux-amd64-image.tar.gz.sha256
```

To verify all downloaded assets together, download `SHA256SUMS` and every listed
asset, then run:

```sh
sha256sum -c SHA256SUMS
```

After extracting the source ZIP, verify its internal manifest from the extracted
project root:

```sh
sha256sum -c SHA256SUMS
```

Checksum verification establishes integrity relative to the downloaded manifest.
It does not replace reviewing the release's commit and successful Actions run.

## Automatic pack changes in 0.15.0

New original scenarios cover profile ranking and bounded candidate fallback,
strict season identity, unique file mappings, unknown/future/excluded episodes,
terminal-job deduplication and unchanged preview storage. Local HTTP/UDP tracker
and TCP peer fixtures check metadata authentication for v1/v2/hybrid magnets and
absence of payload requests. Other scenarios exercise shared selective imports,
durable origin recovery, catalog/settings races, mutable HTTP torrents, stale
preview guards, shared deadlines and read-only apply rejection.

CLI, Bearer API and browser journeys check strict inputs, authentication, form
guards, escaping/redaction, offline read-only previews and guarded live apply.
Opt-in monitoring scenarios cover default individual behavior, newly aired pack
preference, unresolved fallback, no duplicate retries and the combined 64-job
allowance across seasons. Existing full validation remains required.

The recorded run below validates these scenarios through GitHub Actions only.
No local tests, lint, builds or demos ran.

## Recorded 0.19.1 CI evidence

`83e6d1a40ac2d5abac0baf355a0011c58e49429a` passed
[Actions run 37305082540](https://github.com/alecerf/mynou/actions/runs/37305082540):
**490 Rust tests passed**, none failed or ignored, across **43 targets**.
Four scheduler checks and all five validation/build/package/release jobs passed.
CI checked the offline one-package/zero-dependency graph, formatting, Clippy,
GNU/musl builds, standalone and isolated container demonstrations, executable
permissions, archive integrity and all checksum manifests. Every Cargo harness
ran with two processes and two threads per harness; execution took 45.505 seconds.

GitHub Actions published [v0.19.1](https://github.com/alecerf/mynou/releases/tag/v0.19.1)
on October 5, 2026, at 11:48:49 UTC. The tag targets this exact tested commit.
All seven assets were uploaded by `github-actions[bot]`. Recorded payloads:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.19.1-linux-amd64-image.tar.gz` | 1767437 | `4df973257f942f1f9fa701422d777458401f46467cc5fd6f62964f832a67bcfa` |
| `mynou-v0.19.1-linux-amd64-image.tar.gz.sha256` | 105 | `75d74f3357170a15f1fa6184a775d1b32b7e57e266c6f9474d3ec2331a8fcfb1` |
| `mynou-v0.19.1-linux-x86_64` | 3519328 | `475072aa8015f04df90f4b52bc621744cde2a9f6bfede92ebaa663c45f3f1a03` |
| `mynou-v0.19.1-linux-x86_64.sha256` | 93 | `0ae977e821acf036904175c0025c267e5498d516629478f77e911c36ccd926aa` |
| `mynou-v0.19.1-source.zip` | 6228245 | `cf7ef1bfe9391db5d5e242f9712636f3b468089c33f88d6cfc87714342724666` |
| `mynou-v0.19.1-source.zip.sha256` | 91 | `d682bf5b0270e9b7f53e9c1068ca31680ec4fb74613b9a8fe1dc07325f932332` |
| `SHA256SUMS` | 289 | `ea504c7234ff423ad4b7f4be7fdcc79a9aa0c2f9861cf211a283a2caef40b846` |

The original regression loops through independent local fixtures for pending
operator work, foreign and similarly prefixed routes, missing/nonregular files,
parent traversal and file/ancestor symlinks, unrestricted ready reuse, missing
restricted baselines, rejected/accepted title assessment and profile mismatch.
Only compatible ready jobs join requester demand; conflicts keep null job/charge
fields. Every case retains the operator journal and ownership across restart.
The existing requester/native acquisition scenarios and all earlier harnesses
also pass. The v0.19.0 and v0.18.0 tags and every asset ID, size and digest remain
unchanged. No local tests/lint/builds/binaries/demonstrations or manual publication
ran. Later documentation changes require their own completed workflow without
replacing published tags or assets.

## Recorded 0.19.0 CI evidence

`2388836a24ac02225a4171aa3d6bbbc32323b6c7` passed
[Actions run 37303359973](https://github.com/alecerf/mynou/actions/runs/37303359973):
**489 Rust tests passed**, none failed or ignored, across **43 targets**.
Four CI scheduler checks passed. All five validation/build/package/release jobs
completed successfully: the offline single-package dependency graph, formatting,
Clippy, GNU/musl builds, native/static and isolated container demonstrations,
archive integrity, executable permissions and checksum checks. Every Cargo
harness ran with two processes and two threads per harness; recorded execution
took 45.628 seconds.

GitHub Actions published [v0.19.0](https://github.com/alecerf/mynou/releases/tag/v0.19.0)
on October 5, 2026, at 11:32:55 UTC. The tag targets the validated source.
All seven assets were uploaded by `github-actions[bot]`. Recorded payloads:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.19.0-linux-amd64-image.tar.gz` | 1765442 | `163a6a14c5d8991a7e0f7128e9e026ad7eca93ab6dab48b951236a79496fc09e` |
| `mynou-v0.19.0-linux-amd64-image.tar.gz.sha256` | 105 | `ac6007daffbcf526d7d784fcce08de4239a103c5ba655fa32a9697f77003b0e4` |
| `mynou-v0.19.0-linux-x86_64` | 3515232 | `908e64f4e39d84112d545e57a8714dec6449a26e9954a169c7f3842c7acc3dcd` |
| `mynou-v0.19.0-linux-x86_64.sha256` | 93 | `a70901ac6b3940138270f0e2a1e1a3cc6618c2316fff486f5d64ab038242a9b5` |
| `mynou-v0.19.0-source.zip` | 6211659 | `bbc67aaaf51fb4ecf589f3213ec73ffb4d011a67aeed2209ced6259f1e772961` |
| `mynou-v0.19.0-source.zip.sha256` | 91 | `f37992eb21de6aa72f0b7519adf0fa7f87687b3d0dc52e524007afb5e182f637` |
| `SHA256SUMS` | 289 | `75c8ab74b928e83b17abc8b73598d65faa88c1ed6681076566d00286c73e176b` |

Original scenarios cover account opt-in, approval and quota fencing, compatible
canonical sharing, immutable profile/destination captures, source numbering,
independent identity-checked polling, removals, notification outcomes, guarded
CLI/API/browser controls and crash recovery. Local native-peer/Plex journeys
verify acquisition ordering and routed exact-path confirmation without public
content. The preceding v0.18.0 tag and all asset IDs, sizes and digests remain
unchanged. No local validation or manual tag/release/upload ran. Later patch and
documentation commits need their own complete CI; published artifacts stay fixed.

## Recorded 0.18.0 CI evidence

`f1a9733a5908c3fa8bc5e93b5d18800334fad5d1` passed
[Actions run 37293887224](https://github.com/alecerf/mynou/actions/runs/37293887224):
**467 Rust tests passed**, none failed or ignored, across **40 targets**.
Four CI scheduler checks also passed. All five validation/build/package/release
jobs completed successfully, including the offline single-package dependency
graph, formatting, Clippy, GNU/musl builds, native/static and isolated Docker
demonstrations, archive integrity, executable permissions and checksum checks.
The bounded scheduler ran every Cargo harness with two processes and two threads
per harness; the recorded all-target execution took 41.531 seconds.

GitHub Actions published [v0.18.0](https://github.com/alecerf/mynou/releases/tag/v0.18.0)
on October 5, 2026, at 10:04:19 UTC. The tag points to the validated source commit.
All seven assets were uploaded by `github-actions[bot]`. Recorded payloads:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.18.0-linux-amd64-image.tar.gz` | 1631326 | `7584dc37ce5fd12dff54209433e709fe7b5541c9d68adabc375473427a768d93` |
| `mynou-v0.18.0-linux-amd64-image.tar.gz.sha256` | 105 | `6fcfd4bd7a86b696d1965329a601af7a7c4ded1f6cab0c84374aae2166b0d290` |
| `mynou-v0.18.0-linux-x86_64` | 3208032 | `20154ac99a90f68fbaf6a10cee3d6bb0fafb26dc42842d88509cca99a381b1f1` |
| `mynou-v0.18.0-linux-x86_64.sha256` | 93 | `f1789ec6d8bd54214beedd7921b248e9b3a84bf7a57dde030f007a62d622bd07` |
| `mynou-v0.18.0-source.zip` | 5716251 | `f880b85498f9ef7a6f308676c7dcc6bd2ff2cb4a4e4828ec9536c91ea5170497` |
| `mynou-v0.18.0-source.zip.sha256` | 91 | `605726928e4106dacf2c2482148a06dfe9d34cd326f7bf8880d2f3f59baab217` |
| `SHA256SUMS` | 289 | `12961318fc8c1a74200d7546ab3fdd02b65cc3570600a42a2233d769cedbf341` |

The original scenarios cover 64-owner baseline/creation/promotion transactions,
torn writes and complete corruption, signed incomplete group records, partial
ready snapshots, immutable lineage, staged cancellation, retry/competitor/obsolete
parent fences, monitoring changes, source and quality guards, protected JSON/form
controls and read-only offline CLI preview with guarded service apply. A local
native peer/Plex journey retains the entire old group during partial exact-path
confirmation and resumes staging across restart with independent payload/import
inodes. Browser logout rejects the stale apply with HTTP 403 and unchanged state.

No local tests, lint, builds, binaries or demonstrations were executed. CI alone
created the tag/release/assets. The existing v0.17.0 tag and all seven asset IDs,
sizes and digests remain unchanged. Later documentation commits require their
own complete CI and do not replace the v0.18.0 tag or assets.

## Recorded 0.17.0 CI evidence

`89e5aefa66026da084a6770fb76d51e7396602ec` passed
[Actions run 37271644993](https://github.com/alecerf/mynou/actions/runs/37271644993):
**450 Rust tests passed**, none failed or ignored, across **38 targets**.
Four CI scheduler checks also passed. All five validation/build/package/release
jobs completed successfully, including the offline single-package dependency
graph, formatting, Clippy, GNU/musl builds, static/native and isolated Docker
demonstrations, archive integrity, executable permissions and checksum checks.
The bounded scheduler ran every Cargo harness with two processes and two threads
per harness; the recorded all-target execution took 40.600 seconds.

GitHub Actions published [v0.17.0](https://github.com/alecerf/mynou/releases/tag/v0.17.0)
on October 5, 2026, at 06:19:26 UTC. The tag points to the validated source commit.
All seven assets were uploaded by `github-actions[bot]`. Recorded payloads:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.17.0-linux-amd64-image.tar.gz` | 1576503 | `e5d18d88147129c7513dfd7f928222f6f31606863439970721d93825f0d325f7` |
| `mynou-v0.17.0-linux-amd64-image.tar.gz.sha256` | 105 | `79947b4fe4dfb3a5e868a06c101e80a331f0f9cb12d28f618eda335a71ed2ee9` |
| `mynou-v0.17.0-linux-x86_64` | 3089248 | `f4fdb0eff17cadd60c0a6afc2e086121592d882dffd0432dee89e77146d49f1b` |
| `mynou-v0.17.0-linux-x86_64.sha256` | 93 | `e6743e7c4c26784cb5b11af156144a662a5cb022113f9e0a1b8146814bac4961` |
| `mynou-v0.17.0-source.zip` | 5460855 | `932954f391948acd6d43a2fd95220d381bd8f58c2717b2dd26a56e8c15092c07` |
| `mynou-v0.17.0-source.zip.sha256` | 91 | `cb217b1d854adc730f9a558f738d3714d01520b04ad8299c8c1bd0f7ec99e923` |
| `SHA256SUMS` | 289 | `d948fc85db5780e915b146342049f00480eab34a885d213ba1918ca391ec35c0` |

The recorded scenarios cover 64-owner atomic journal frames, incomplete or
semantically invalid state, guarded source/catalog decisions, exact shared
imports, independent payload/library inodes, per-owner Plex confirmation, worker
concurrency, queued-owner interests, cancellation/retry/restart, pre-journal copy
recovery and protected API/browser/CLI controls. Individual shared upgrades remain
blocked until whole-group replacement is available in a later release.

No local tests, lint, builds, binaries or demonstrations were executed. CI alone
created the tag/release/assets. The existing v0.16.0 tag and all seven asset IDs,
sizes and digests remain unchanged. Later documentation commits require their
own complete CI and do not replace the v0.17.0 tag or assets.

## Recorded 0.16.0 CI evidence

`0821a4d3b499a5863fe5b50206c98bda25d6fb49` passed
[Actions run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691):
**430 Rust tests passed**, none failed or ignored, across **36 targets**.
Four CI scheduler checks also passed. All five validation/build/package/release
jobs completed successfully, including the offline single-package dependency
graph, formatting, Clippy, GNU/musl builds, static/native and isolated Docker
demonstrations, archive integrity, executable permissions and checksum checks.

GitHub Actions published [v0.16.0](https://github.com/alecerf/mynou/releases/tag/v0.16.0)
on October 4, 2026, at 21:59:49 UTC. The tag points to the validated source commit.
All seven assets were uploaded by `github-actions[bot]`. Recorded payloads:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.16.0-linux-amd64-image.tar.gz` | 1524912 | `78619821d782a56178589829cec18d615a1974742618b0d50b8e83e63d7e97b8` |
| `mynou-v0.16.0-linux-amd64-image.tar.gz.sha256` | 105 | `10fd48edf74fb8a4a44b7665e9f516a4a81d42badcc9c345539afa9701b1b820` |
| `mynou-v0.16.0-linux-x86_64` | 2970464 | `4fb882372c8a75685281687078ac1b0f1ea30e9b40e1b2af20706024b8e2cbb2` |
| `mynou-v0.16.0-linux-x86_64.sha256` | 93 | `cd519cd7240af83566b3998ff86099bc600219cf3bf76b28c4df897799befa99` |
| `mynou-v0.16.0-source.zip` | 5225310 | `42d192bd3fbcf9e95138b72591311e4580a901b4c8b3388ac7c03e918139e7b5` |
| `mynou-v0.16.0-source.zip.sha256` | 91 | `f04628c8f2e45ea4f6aedf60aed8b134f820a230b75cc171f16c0d3218a266ee` |
| `SHA256SUMS` | 289 | `931bd2f8df8bba88dfdcf51b9cb757e7fbdbb5c489e3c24c58a4828f1f8e6385` |

The recorded scenarios exercise renumbered and cross-season catalogs, explicit
absolute/season source queries, exact single-file labels, collisions, stale plan
guards, late results, legacy/versioned snapshots, corrupt semantic data, retained
history, protected API/browser/CLI actions and unchanged existing library bytes.

No local tests, lint, builds, binaries or demonstrations were executed. CI alone
created the tag/release/assets. The existing v0.15.0 tag and all seven asset IDs,
sizes and digests remain unchanged. Later documentation commits require their
own complete CI and do not replace the v0.16.0 tag or assets.

## Recorded 0.15.0 CI evidence

[Run 37230875486](https://github.com/alecerf/mynou/actions/runs/37230875486) passed
for commit `1fe40eed0b0ea170a03ffce8d30d2ab8cb3e7125`: 409 tests passed with
none failed or ignored across 34 targets, alongside all dependency, formatting,
Clippy, native/musl build, native/Docker demonstration, packaging and checksum
checks. GitHub Actions published
[v0.15.0](https://github.com/alecerf/mynou/releases/tag/v0.15.0) with seven assets
on October 4, 2026, at 20:12:26 UTC. The tag and release target match the validated
commit; every asset was uploaded by `github-actions[bot]`.

GitHub records these SHA-256 digests for the three payload assets:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.15.0-source.zip` | 4,997,709 | `21d3b8324305434436bbcbf34db70d93af250b25e9f874cc6594f765f75d2b07` |
| `mynou-v0.15.0-linux-x86_64` | 2,868,064 | `eb05e073ed4aa77e766ae6a53a697dafa01e1ddfe7cc1efbd2e9dbc9b945c249` |
| `mynou-v0.15.0-linux-amd64-image.tar.gz` | 1,487,728 | `b2f2ec477a8231b1852226331ca85ab9e3b88b92e05894ae3ef120d8589fe176` |

The three individual checksum files and combined `SHA256SUMS` complete the seven
published assets. CI verified package checksums, source ZIP integrity, the
internal manifest and embedded binary permissions before publication. Later
commits need their own completed run and do not alter this release's tag/assets.
Synthetic local protocol fixtures do not establish personal-installation
compatibility, public-swarm throughput or general performance improvements.

## Selective changes in 0.14.0

New CI-only local-peer scenarios exercise v1 boundary pieces, untouched unrelated
files, explicit selected/full readiness, empty seeding bitfields, padding/empty
files, late magnet metadata, sparse v2 proofs and hybrid hashes. They also cover
shared selection expansion, generation retirement, durable user pause, offline
restart, corrupted selected bytes, missing-path repair, tracker bytes-left without
completion, mapped imports from a partial torrent, shared cancellation and strict
Bearer/browser/CLI controls. Genuine earlier controls retain full acquisition;
new checksum records preserve selections. Existing full-transfer corrupt-peer,
rate, seeding and restart checks remain in the complete workflow.

The recorded run below validates these scenarios. Tests, lint, builds and demos
have not been run locally under the active development policy.

## Recorded 0.14.0 CI evidence

[Run 37221887812](https://github.com/alecerf/mynou/actions/runs/37221887812) passed
for commit `a077af8d660a2b5ca12e47b579562e7a9292f6e2`: 385 tests passed with
none failed or ignored across 31 targets, alongside all dependency, formatting,
Clippy, native/musl build, native/Docker demonstration, packaging and checksum
checks. GitHub Actions published
[v0.14.0](https://github.com/alecerf/mynou/releases/tag/v0.14.0) with seven assets
on October 4, 2026, at 17:53 UTC. The tag and release target match the validated
commit; every asset was uploaded by `github-actions[bot]`.

GitHub records these SHA-256 digests for the three payload assets:

| Asset | Bytes | SHA-256 |
| --- | ---: | --- |
| `mynou-v0.14.0-source.zip` | 4,723,506 | `e1c7f609e49ad147279a6b88ab3c1b96a0688f86ef4f50fa7498e2379957066f` |
| `mynou-v0.14.0-linux-x86_64` | 2,732,896 | `3a291cf3af8e7d1a39e7d6f9e20d11109eee11d41970495bf37bfc6c53816781` |
| `mynou-v0.14.0-linux-amd64-image.tar.gz` | 1,428,142 | `b30cae2c8e0614d47ef74ca15367ba4df27427bd7d630fed84d35553ddb45efb` |

The three individual checksum files and combined `SHA256SUMS` complete the seven
published assets. CI checked the package checksums, source ZIP integrity,
internal manifest and embedded binary permissions before publication. Later
documentation or code commits need their own completed run and do not alter the
published tag or assets. Local protocol fixtures do not establish personal
installation compatibility or public-swarm throughput.

## Pack changes in 0.13.0

CI-only local-peer journeys exercise two mapped episodes sharing one verified
torrent, canonical imports of absolute-named files, ignored malformed unmapped
video, missing-file failure without fallback, shared cancellation, persistence
through restart and read-only mutation rejection. Other scenarios check traversal
paths, duplicates, unknown/future/undated episodes, worker mapping protection, guarded terminal-job correction, source-key
conflicts across series, complete-input prevalidation, old unmapped jobs,
Bearer/browser protection, escaped labels, credential-free responses and CLI
mapping-file routing. New scopes can be recorded without automatic acquisition.

The recorded run below validates the released pack implementation. No tests,
lint, builds or demos are run locally under the active development policy.

## Recorded 0.13.0 CI evidence

[Run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435) passed
for commit `cb6e89700a63c1a7f9aaaa644fce32bf8944386f`: 374 tests passed with
none failed or ignored across 29 targets, alongside all dependency, formatting,
Clippy, native/musl build, native/Docker demonstration and packaging checks.
GitHub Actions published
[v0.13.0](https://github.com/alecerf/mynou/releases/tag/v0.13.0) with seven assets
on October 4, 2026, at 15:39 UTC. Later documentation or code commits need their
own completed CI run and do not alter this published source/tag.

## Recorded 0.12.0 CI evidence

[Run 37210722790](https://github.com/alecerf/mynou/actions/runs/37210722790) passed
for commit `fbdf61c19b31f941a08f91fd19e3bb843aec0541`: 364 tests passed with
none failed or ignored across 27 targets, alongside the complete dependency,
formatting, Clippy, build, native/Docker demonstration and packaging checks.
GitHub Actions published [v0.12.0](https://github.com/alecerf/mynou/releases/tag/v0.12.0)
with seven assets on October 4, 2026, at 14:55 UTC. That result validates durable
series monitoring and calendar, not the following pack changes.

## Series changes in 0.12.0

New CI-only local-service scenarios cover future/undated/unmapped episodes,
optional specials, earliest-air-date choices, per-episode exclusions, background
newly aired acquisition and visible catalog failures. Persistence checks cover
restart/read-only views, checksum corruption, linked snapshots and bounded
partial batches with overlapping-scope deduplication. Numbering checks reject
duplicate or changed known identities; a gated HTTP response verifies settings
revision protection. Calendar date/range/pagination rules, Bearer API controls,
browser forms/escaping/bulk prevalidation and CLI routing have dedicated journeys.

The completed 0.12.0 run above records these checks. Fixtures use synthetic metadata and local services; they do not
establish personal-installation compatibility.

## Recorded 0.11.0 CI evidence

[Run 37206645776](https://github.com/alecerf/mynou/actions/runs/37206645776)
completed successfully for commit
`d7cb8eb20d364c217ed89ab183ebf754512d7dfe`: 343 tests passed with none failed
or ignored, alongside the full validation and release pipeline. GitHub Actions
published [v0.11.0](https://github.com/alecerf/mynou/releases/tag/v0.11.0)
with seven assets on October 4, 2026, at 13:48 UTC. That result validates the
browser release and does not validate the following series changes.

## Browser changes in 0.11.0

The new browser scope is covered by CI-only unit and native HTTP/form journeys:

- Sign-in challenge and session rotation, expiration/capacity, five-attempt
  challenge removal, logout/restart, HTTPS Secure cookie and host/origin binding.
- Bearer API isolation, same-origin/form-token requirements, cookie ambiguity,
  strict form decoding, unknown/duplicate fields and numeric/size bounds.
- Escaped hostile titles/paths, credential-free errors/history/search reports,
  no script dependency and semantic navigation/labels/table structure.
- Request submission/deduplication, cancel/retry, filters/pagination and mixed
  bulk outcomes with prevalidation before side effects.
- Search previews without journal writes, retained request identity, monitoring,
  one-time baselines, upgrade preview/apply and retained earlier imports.
- Native transfer pause/resume, priority, file choices and policy changes;
  invalid controls preserve prior choices and downloaded metadata.

The completed 0.11.0 run above records these checks. Form-protocol checks do not
establish independent visual/browser accessibility review or personal Plex
installation compatibility. Existing offline dependency, lint, build, demo,
Docker and archive-integrity checks continue to run.

## Recorded 0.10.0 CI evidence

[Run 37203872630](https://github.com/alecerf/mynou/actions/runs/37203872630)
completed successfully for commit
`5b0b202b78bc906db914ef4713c57ec14807169f`: 321 tests passed with none ignored,
alongside the complete dependency, lint, build, native/Docker demo and packaging
checks. GitHub Actions published
[v0.10.0](https://github.com/alecerf/mynou/releases/tag/v0.10.0) with seven assets.
This evidence validates 0.10.0, not later source changes.

The debug test build's delayed local TCP fixture transferred 786,432 payload
bytes with a 100 ms per-block delay: 6,574 ms using the retained sequential path
(`max_peers: 1`) and 1,480 ms using four parallel peers, a 4.441x ratio for that
observed fixture. The baseline is inside 0.10.0; it is not the separately
released 0.9.0 executable. This is not a public-swarm or optimized-release
benchmark, and it does not establish a general speedup.

## Parallel-peer changes in 0.10.0

CI must exercise bounded parallel transfers with synthetic torrents and local
TCP peers, without downloading public content. Validation must preserve the
0.9.0 transfer controls and cover:

- Missing `max_peers` defaults, accepted values one through eight, invalid
  values, a single-peer baseline and effective global/resource worker bounds.
- Exclusive in-flight piece ownership, verified publication, file-priority
  changes for subsequent work, cancellation/restart and reclaiming work after
  a peer disconnects or sends corrupt data.
- Several usable peers contributing without duplicate endgame requests or
  allowing one bad peer to corrupt a ready transfer. v1, v2 and hybrid
  verification requirements remain in force.
- Discovery alongside known peers, bounded results and private-torrent rules;
  tracker/DHT latency must not hold back a usable direct peer unnecessarily.
- Aggregate bandwidth caps and persistent accounting across parallel peers,
  including pause, policy changes and seeding-limit retention.
- A reproducible local throughput comparison between the retained sequential
  path with `max_peers: 1` and parallel peers, recording the environment,
  workload, timing and limitations of the comparison. This baseline is not a
  measurement of the separately released 0.9.0 binary.

The completed run above records these checks for the released 0.10.0 commit.
A local fixture comparison does not establish public-swarm throughput or
performance on a personal installation.

## Transfer-control changes in 0.9.0

The new scope is durable native transfer controls, priority/FIFO scheduling,
per-file piece order, global payload bandwidth limits, persistent counters and
seeding elapsed time, and ratio/time policies. CI must cover:

- Defaults and bounded numeric configuration, with older configurations retaining
  unlimited rates and seeding unless limits are set.
- Durable pause/resume and queue priority across restart, including shared
  requests and cancellation without file deletion.
- Per-file priorities using the listed original metadata indices, rejecting
  padding entries and changing piece order while all files still download;
  boundary pieces remain verified under v1/v2/hybrid constraints.
- Aggregate download/upload payload limits across simultaneous local peers,
  a bounded 16 KiB burst, global caps enforced alongside local rates, and
  bounded scheduling. Local policy objects replace seed defaults in full;
  clearing an override restores configured defaults.
- Persistent payload counters and seeding elapsed time, clean shutdown flushes,
  ratio/time stop decisions using verified non-padding payload size, concurrent
  whole-block upload reservations, online availability elapsed time, and
  retention of downloaded/library files. Earlier records begin missing historical
  counters at zero rather than inventing activity. Abrupt
  crashes may lose the latest unflushed increments; exact crash accounting is
  not a supported guarantee.
- CLI/API transfer controls, authenticated operations, bounded inputs and
  credential-free reports.

The completed run below records these checks for the released 0.9.0 commit.
Preserve the existing dependency, formatting, Clippy, native/Docker demo,
selection and upgrade safety checks, and release packaging when changing the
transfer engine. Earlier passing runs do not validate changed transfer code.

## Recorded 0.9.0 CI evidence

[Run 37187100999](https://github.com/alecerf/mynou/actions/runs/37187100999)
completed successfully for transfer-control commit
`c45e127e3b8587a4f4ace6a2bbc7eab86bf48a66`. Its 302 tests passed with none
ignored, together with dependency, formatting, Clippy, build, demo and release
checks. GitHub Actions published
[v0.9.0](https://github.com/alecerf/mynou/releases/tag/v0.9.0) with seven assets.
This result validates that release, not the 0.10.0 changes or a personal Plex
installation.

## Recorded 0.8.0 CI evidence

[Run 37151554961](https://github.com/alecerf/mynou/actions/runs/37151554961)
completed successfully for monitoring commit
`9ef6f1fc94f2437aa6797e18385c0e81273c5e44`. Its 251 tests passed with none
ignored, together with dependency, formatting, Clippy, build, demo and release
checks. GitHub Actions published
[v0.8.0](https://github.com/alecerf/mynou/releases/tag/v0.8.0) with seven assets.
This result validates that release, not the 0.9.0 changes or a personal Plex
installation.

## Recorded 0.7.0 CI evidence

[Run 37137061361](https://github.com/alecerf/mynou/actions/runs/37137061361)
completed successfully for selection commit
`14686f01c17a88c6b9e45ce9d2672e0e3c66d21f`, and GitHub Actions published
[v0.7.0](https://github.com/alecerf/mynou/releases/tag/v0.7.0).
This result validates that release, not the 0.8.0 changes or a personal Plex
installation.

## Recorded 0.6.1 CI evidence

[Run 37134671116](https://github.com/alecerf/mynou/actions/runs/37134671116)
completed successfully on October 3, 2026, for commit
`2a066c2c3b9aafda47a3fc898872d1c371b5f249`. Its 131 tests passed, as did the
dependency graph, formatting, Clippy, GNU/musl builds, native and Docker demos,
and release packaging. GitHub Actions published
[v0.6.1](https://github.com/alecerf/mynou/releases/tag/v0.6.1).

This is evidence for 0.6.1 only. It does not establish that later changes passed
or that the project has been connected to a personal Plex installation.

## Historical 0.6.0 validation

The following checks succeeded on October 3, 2026, before the CI-only policy was
adopted and after fixing the cancellation/startup race. They are recorded evidence
for **0.6.0**, not a passing result for later versions.

| Check | Historical result |
| --- | --- |
| Offline Cargo graph | One `mynou` package, zero dependencies |
| Formatting and Clippy across all targets with `-D warnings` | Passed |
| Offline test suite | 131 passed, none failed or ignored |
| Media | Ten tests, including demo MP4, sparse MP4 >4 GiB, synthetic formats, and mutations |
| GNU and musl release builds | Passed |
| musl linkage | No ELF interpreter or required dynamic library |
| Final Docker build | Passed; `scratch`, user 1000:1000 |
| Final container without external networking | Local magnet, import, and simulated Plex confirmation reached `ready` |
| Fresh Compose installation | Generated from the image without Rust on the host; health, doctor, and API checked |
| Compose import and restart | Identical imported bytes; same request ID and `ready` state after restart |

The historical static binary was 1,717,088 bytes. Its final image was 1,898,812
bytes with ID:

```text
sha256:b71a5d2f06c0cf2dfcfc13dceb86a73626a103cf8129616b0b94374da2c05146
```

Layer inspection found exactly two embedded regular files: `/mynou` and
`/etc/ssl/certs/ca-certificates.crt`. No shell, helper program, or shared library
was present. The final demo ran with a read-only root, removed Linux capabilities,
and `--network none`; validation containers were removed afterward.

Tests covered real local peer transfers, v1/v2 magnets, hybrid torrents, Merkle
proofs, restart recovery, unsafe paths, tracker lifecycles, media metadata, and
local-service orchestration. They do not establish deployment on a personal Plex
server or public-torrent throughput.

[Performance measurements](performance.md) and their
[raw results](benchmark-results.json) preserve the historical benchmark. The
[checkpoint](checkpoint.md) describes the implementation and remaining personal
installation configuration.
