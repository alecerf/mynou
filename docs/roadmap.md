# Release roadmap

Mynou will grow through focused releases rather than declaring parity with an
entire media-management stack at once. Every stage keeps Rust std only, no Cargo
dependencies, English project text, and CI-only validation. GitHub Actions
publishes a release only after its checks pass for the exact source commit.

## Current implementation: 0.22.8, awaiting CI

The active 0.22.8 source adds opt-in, ownership-bound ZIP extraction to native
Usenet movie and episode admission. Selection captures decoder limits. A checked
private intent binds the original queue source SHA-256, owner, chosen media entry
and bounds before output creation; exact size, CRC and SHA precede completion.
Extraction holds a bounded queue slot outside its mutex. Revocation fences results
before journal attachment, and current approved leases gate durable transitions.
Known writing intents restart partial output; completed private proofs can join
the preceding journal intent without rewriting. Unknown or corrupt state is
rejected before recovery writes. ZIP-aware jobs require journal format 7.
Outer and inner filename/numbering/profile, source and decoded size, native media,
independent-inode atomic import and exact Plex checks all remain required.
This source requires its own complete Actions validation and publication.
ZIP64, RAR, PAR2, multi-file packs and Usenet upgrades remain separate increments.

## Preceding published stage: 0.22.7

CI published v0.22.7 from `0c85954628c0e8e32a5e89429446f132b4394aaf` in
[run 37501969608](https://github.com/alecerf/mynou/actions/runs/37501969608).
All five jobs passed: 750 Rust tests across 61 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 89.003 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
17:17:59 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.6 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.6

CI published v0.22.6 from `c933f289860ab73615de7c61c4f58edbc2b88bc8` in
[run 37476040909](https://github.com/alecerf/mynou/actions/runs/37476040909).
All five jobs passed: 729 Rust tests across 60 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 93.111 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
14:09:19 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.5 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.5

CI published v0.22.5 from `a3c326f9a8234e19077dea83d2804ef0cd611773` in
[run 37470439302](https://github.com/alecerf/mynou/actions/runs/37470439302).
All five jobs passed: 712 Rust tests across 60 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 77.994 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
13:27:07 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.4 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.4

CI published v0.22.4 from `93711ac221c97edd0392d8e262e3b3961553561a` in
[run 37465017605](https://github.com/alecerf/mynou/actions/runs/37465017605).
All five jobs passed: 693 Rust tests across 59 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 81.582 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
12:44:29 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.3 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.3

CI published v0.22.3 from `e4e77c9ddf142bafd91b17a32bfb7741efb45230` in
[run 37459145003](https://github.com/alecerf/mynou/actions/runs/37459145003).
All five jobs passed: 680 Rust tests across 58 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 80.652 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
11:54:05 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.2 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.2

CI published v0.22.2 from `1a4949026a615443fe40767015f97dc90437430d` in
[run 37439687975](https://github.com/alecerf/mynou/actions/runs/37439687975).
All five jobs passed: 663 Rust tests across 57 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 79.506 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
09:01:20 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.1 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.1

CI published v0.22.1 from `cbc10d16b108c03a157d7afb6c91034ef8ac4343` in
[run 37436785004](https://github.com/alecerf/mynou/actions/runs/37436785004).
All five jobs passed: 652 Rust tests across 56 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 71.509 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:35:55 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.22.0 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.22.0

CI published v0.22.0 from `9644114e5f1e8c2ed679d57a3a2b18c479ee4ac2` in
[run 37434792979](https://github.com/alecerf/mynou/actions/runs/37434792979).
All five jobs passed: 639 Rust tests across 55 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 64.375 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:17:36 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.1 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.21.1

CI published v0.21.1 from `88c52d761eed0ea1fb88ba3d52571047ee862dd7` in
[run 37433196413](https://github.com/alecerf/mynou/actions/runs/37433196413).
All five jobs passed: 628 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 60.185 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
08:02:35 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.21.0 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.21.0

CI published v0.21.0 from `8edd0e4f91039ba2781008ea5c1cf010ef59781f` in
[run 37431238845](https://github.com/alecerf/mynou/actions/runs/37431238845).
All five jobs passed: 616 Rust tests across 54 harnesses, none failed or ignored,
and four scheduler checks. Test execution took 63.220 seconds with two harness
processes and two threads each. Seven bot-owned assets were published at
07:44:52 UTC on October 6, 2026. The tag matched the validated source and the
prior v0.20.7 tag and asset IDs, sizes and digests remained unchanged.

## Preceding published stage: 0.20.7

Durable atomic requester/IRC outcomes precede opt-in native HTTP notifications.
Stable IDs, immutable route bindings, leases and bounded attempts retain delivery
through restart. Guarded CLI/API/browser controls affect delivery alone. Run
37429460979 passed all five jobs for 3de105188e855e35b5ae86be012df43842c25148:
602 tests/53 harnesses and four scheduler checks. CI published seven bot assets
at 07:28:39 UTC on October 6, 2026; v0.20.6's tag/assets stayed unchanged.

## Preceding published stage: 0.20.6

Reviewed request rules confirm the selected Plex identity and fresh canonical
catalog facts before admitting persistent explicit requester origins. Ordinary
approval/quotas, captured routes, numbering and checked intent recovery remain
mandatory. Run 37427693256 passed all five jobs for
0b7d9c67a666b6929c626a930450f3dccae13d41: 585 tests across 52 harnesses and four
scheduler checks. CI published seven bot assets at 07:10:02 UTC on October 6,
2026; the preceding v0.20.5 tag/assets remained unchanged.

## Preceding published stage: 0.20.5

Explicit requester selectors require compatible approved retained demand and
recheck it after metadata inspection. Run 37423924045 passed all five jobs for
861f8ae2f0274f82a7351fe1a5ddb890b8391f01 with 568 Rust tests across 51 harnesses
and four scheduler checks. CI published seven assets on October 6, 2026,
at 06:31:53 UTC; the prior v0.20.4 tag/assets remained unchanged.

## Preceding published stage: 0.20.4

Required NickServ identification binds exact service/account confirmation before
channel membership. Failure closes the connection, reconnect repeats the exchange
and public health redacts credentials. Original local fixtures cover trust,
maximum commands, missing credentials, restart, shutdown and fixed deadlines.
Run 37362054311, attempt 4, passed all five jobs for the original source,
retaining 560 Rust tests across 50 harnesses and four scheduler checks. CI
published v0.20.4 on October 6, 2026, at 06:25:00 UTC, with seven checked bot
assets and an unchanged v0.20.3 tag/assets. Reviewed demand and durable
notification routing follow immediately after 0.20.5, then 0.21–0.23.

## Previous published stage: 0.20.3

Configurable fixed-delimiter text formats require complete catalog/hash claims,
bound IRC display formatting and share the original review/routing path. Pure
CLI/API previews preserve storage. Old JSON identities/fingerprints remain
stable; grammar changes require a new source ID. Original local scenarios cover
format boundaries, protected previews, live delivery and native exact imports.
Run 37360688158 passed for a2a3c9f1ec9a0cd6af09031b24c246be8614d2c8 with
551 Rust tests across 49 harnesses, four scheduler checks and all five jobs.
CI published seven v0.20.3 assets at 19:07:13 UTC on October 5, 2026, preserving
v0.20.2. See [validation evidence](validation.md#recorded-0203-ci-evidence).
NickServ follows in the active increment before broader IRC action policies.

## Previous published stage: 0.20.2

Required SASL PLAIN negotiates bounded capabilities and credentials before IRC
channel membership. Missing/rejected authentication closes the connection;
every reconnect authenticates again. Public reports redact credentials and
display required/authenticated state. Old unauthenticated bindings remain stable;
authentication policy changes require a new source ID. Original local fixtures
cover success, bounds, fragmentation, failures, chunk boundaries and deadlines.
Run 37348828003 passed for `bb5aebd3727a34b02ab10c07909549529807fa4a` with
543 Rust tests across 48 harnesses, four scheduler checks and all five jobs.
CI published seven v0.20.2 assets on October 5, 2026, at 17:32:22 UTC.
Previous v0.20.1 and v0.20.0 assets remain unchanged. See
[validation evidence](validation.md#recorded-0202-ci-evidence).
Bounded tracker text adapters are the next focused stage; NickServ and broader
actions follow independently.

## Previous published stage: 0.20.1

Explicit grab rules route configured hash-pinned magnets to existing approved
jobs after metadata-only verification. Canonical title/year/source labels and
captured profiles must agree. Reservations precede immutable journal origins;
restart completes committed origins and aborts uncommitted intents. Native
selection/import uses the retained exact file and authenticated hash aliases.
Original local CI covers imports, gates, races, corruption and recovery.
Run 37345455739 passed for `cf838f3750aaf129f1ac939a675a6eee4f635594` with
533 Rust tests across 47 harnesses, four scheduler checks and all five jobs.
CI published seven v0.20.1 assets on October 5, 2026, at 17:05:20 UTC.
Previous v0.20.0 and v0.19.1 assets remain unchanged. See
[validation evidence](validation.md#recorded-0201-ci-evidence).

The following increment implements required SASL PLAIN, described above.
Tracker text adapters, NickServ and broader actions follow independently.

## Previous published stage: 0.20.0

[IRC reception and reviews](irc.md) provide opt-in verified TLS sources,
sender/channel restrictions, strict release envelopes, title/profile filters,
durable duplicate suppression and guarded CLI/API/browser decisions. Original
local fixtures cover bounds, trust, corruption, capacity, review races, reconnect
and shutdown. Reviews do not acquire media. Automatic grabs follow as a separate
increment, after this reception/review release.

Run 37335746159 passed for `4da379a4ef48763d8a1035fb6b3cd4acc4a721bc` with
512 Rust tests across 46 harnesses, four scheduler checks and all five jobs.
CI published seven v0.20.0 assets on October 5, 2026, at 15:50:41 UTC.
The next increment implements candidate routing, described above.
See [validation evidence](validation.md#recorded-0200-ci-evidence).

## Previous stage: 0.19.1

[Plex requester policies](requesters.md) retain per-account identity bindings,
versioned opt-in profiles, approvals and bounded quotas before acquisition.
Compatible canonical demand shares work; captured destination/profile behavior
survives edits and restart. Independent polling reports partial failures without
removing another account's demand. Guarded operator controls expose status,
limits, decisions and recorded notification outcomes. Original synthetic CI
scenarios cover multiple accounts, native acquisition and persistence recovery.

The requester stage passed [run 37303359973](https://github.com/alecerf/mynou/actions/runs/37303359973)
for `2388836a24ac02225a4171aa3d6bbbc32323b6c7`: 489 Rust tests across 43 targets,
four scheduler checks and all five jobs. CI published
[v0.19.0](https://github.com/alecerf/mynou/releases/tag/v0.19.0) with seven assets
on October 5, 2026, at 11:32:55 UTC. The 0.19.1 patch restricts reuse of uncaptured
operator jobs to verified ready imports with compatible destinations and quality.
It passed [run 37305082540](https://github.com/alecerf/mynou/actions/runs/37305082540)
for `83e6d1a40ac2d5abac0baf355a0011c58e49429a`: 490 Rust tests across 43 targets,
four scheduler checks and all five jobs. CI published
[v0.19.1](https://github.com/alecerf/mynou/releases/tag/v0.19.1) with seven assets
on October 5, 2026, at 11:48:49 UTC. The earlier tags and assets remain unchanged.
See [validation evidence](validation.md#recorded-0191-ci-evidence). The next stage
is IRC automation; inspect live branch CI before beginning it.

## Previous stage: 0.18.0

Complete shared-group baselines and replacements are reviewed through
CLI/API/browser. Every child retains immutable parent lineage and one new
authenticated video/destination. Exact Plex confirmation stages an owner; the
last required confirmation promotes the complete group in one synchronized frame.
Old imports remain current throughout partial work, failures and cancellation.
Whole-group retry, monitoring fences, stale guards and format 3 recovery retain
captured ownership and earlier bytes. See [group upgrades](group-upgrades.md).

This stage passed [run 37293887224](https://github.com/alecerf/mynou/actions/runs/37293887224)
for commit `f1a9733a5908c3fa8bc5e93b5d18800334fad5d1`: 467 Rust tests across 40 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.18.0](https://github.com/alecerf/mynou/releases/tag/v0.18.0) with seven assets
on October 5, 2026, at 10:04:19 UTC. See
[validation evidence](validation.md#recorded-0180-ci-evidence). No local validation
ran. The next stage is Plex requester policies in 0.19; later commits need their
own complete CI.

## Previous stage: 0.17.0

Explicit shared-file preview/apply binds one authenticated video to 2–64
consecutive canonical owners in one season. All owners commit in one synchronized
journal frame before acquisition and import one deterministic Plex range file.
Each owner confirms that exact path; cancellation, retry and later subsets retain
the full binding. Individual remaps, baselines and upgrades are blocked for shared
owners; coordinated group replacement follows in 0.18. See
[shared files](shared-files.md).

This stage passed [run 37271644993](https://github.com/alecerf/mynou/actions/runs/37271644993)
for commit `89e5aefa66026da084a6770fb76d51e7396602ec`: 450 Rust tests across 38 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.17.0](https://github.com/alecerf/mynou/releases/tag/v0.17.0) with seven assets
on October 5, 2026, at 06:19:26 UTC. See
[validation evidence](validation.md#recorded-0170-ci-evidence). No local validation
ran. Later commits require their own complete CI.

## Previous stage: 0.16.0

Explicit numbering choices retain canonical episode identities while approving
changed catalog numbers or alternate/absolute source labels. CLI/API/browser
preview and guarded apply persist accepted choices before future jobs capture
them. Existing library paths and request keys remain fixed; identity history
survives disappearance and restart. See [numbering](numbering.md).

This stage passed [run 37238156691](https://github.com/alecerf/mynou/actions/runs/37238156691)
for commit `0821a4d3b499a5863fe5b50206c98bda25d6fb49`: 430 Rust tests across 36 targets,
four scheduler checks and all five workflow jobs. CI published
[v0.16.0](https://github.com/alecerf/mynou/releases/tag/v0.16.0) with seven assets
on October 4, 2026, at 21:59 UTC. See
[validation evidence](validation.md#recorded-0160-ci-evidence).
Numbering and shared physical ownership were split into independently validated
releases; shared multi-episode imports followed in 0.17.

## Previous stage: 0.15.0

Automatic season-pack search assesses titles under the episode profile, resolves
bounded authenticated metadata and maps unique numbered video paths to every
eligible missing aired catalog episode. CLI/API/browser previews request no
payload and record no jobs. Guarded acquisition binds catalog/request scope,
candidate, torrent hash and exact paths; immutable origin provenance reaches
ordinary mapped jobs before workers begin. Optional `series_packs.enabled`
prefers packs during monitored tracking and refresh with individual fallback
inside the shared deadline and combined 64-job batch. Earlier configurations
default to individual acquisition. See [automatic packs](automatic-packs.md).

The automatic stage passed
[Actions run 37230875486](https://github.com/alecerf/mynou/actions/runs/37230875486)
for commit `1fe40eed0b0ea170a03ffce8d30d2ab8cb3e7125`: 409 tests passed with
none failed or ignored across 34 targets and the complete validation pipeline.
GitHub Actions published
[v0.15.0](https://github.com/alecerf/mynou/releases/tag/v0.15.0) with seven assets
on October 4, 2026, at 20:12 UTC. The next stage was split into explicit numbering in 0.16 and shared physical
multi-episode import ownership in 0.17. See
[validation evidence](validation.md#recorded-0150-ci-evidence) for asset digests.

## Previous stage: 0.14.0

Selective native acquisition retains the union of mapped pack file interests,
verifies required boundary pieces and selected v2/hybrid roots, then permits
mapped imports independently of full-torrent readiness. Selection expansion,
restart verification and existing pause/rate/counter controls retain safe shared
ownership. Partial torrents advertise no payload and never announce completion.
The CLI, API and browser provide additive file selection and full acquisition.
See [transfer selection](transfers.md#selective-acquisition-in-0140).

The selective stage passed
[Actions run 37221887812](https://github.com/alecerf/mynou/actions/runs/37221887812)
for commit `a077af8d660a2b5ca12e47b579562e7a9292f6e2`: 385 tests passed with
none failed or ignored across 31 targets and the complete validation pipeline.
GitHub Actions published
[v0.14.0](https://github.com/alecerf/mynou/releases/tag/v0.14.0) with seven assets.

This result validates the selective stage. Automatic pack choice follows in
0.15; numbering rules and multi-episode physical files need their own releases.

## Previous stage: 0.13.0

Explicit pack acquisition maps exact torrent video paths to already aired catalog
episodes. Ordinary jobs share the native torrent identity, verify the full
payload and import only their retained mapped files. New series scopes can be
created without automatic acquisition before choosing a pack. Input, catalog,
source-key and capacity checks precede recording; workers cannot change mappings and
existing episode jobs are reused. See [pack acquisition](packs.md) for operations
and bounds. A guarded correction can requeue a failed/cancelled mapped request
without imports or an active lease.

The pack stage passed
[Actions run 37213526435](https://github.com/alecerf/mynou/actions/runs/37213526435)
for commit `cb6e89700a63c1a7f9aaaa644fce32bf8944386f`: 374 tests passed with
none failed or ignored. GitHub Actions published
[v0.13.0](https://github.com/alecerf/mynou/releases/tag/v0.13.0) with seven assets.

This stage supplies explicit mappings for absolute/anime-style filenames.
Automatic pack selection and general numbering rules remain later work.
Selective downloading is implemented in the following 0.14 stage.
The [next-release checkpoint](next-release.md) records later numbering and
physical-import prerequisites.

## Previous stage: 0.12.0

Durable series scopes retain TMDB episode plans and monitoring settings.
Background checks acquire newly aired missing episodes, respecting known
catalog identities, earliest monitored dates, exclusions and optional specials.
Future and unresolved episodes stay visible. The CLI, Bearer API and browser
provide series controls and a bounded episode calendar. Settings revisions
reject late refresh results, verified snapshots survive restart and existing
request identities deduplicate overlapping scopes. See [series monitoring](series.md)
for exact limits.

The series stage passed
[Actions run 37210722790](https://github.com/alecerf/mynou/actions/runs/37210722790)
for commit `fbdf61c19b31f941a08f91fd19e3bb843aec0541`: 364 tests passed with
none failed or ignored. GitHub Actions published
[v0.12.0](https://github.com/alecerf/mynou/releases/tag/v0.12.0) with seven assets.
That result does not validate the following pack changes.

## Previous stage: 0.11.0

Browser management adds original Rust-rendered HTML/CSS and native forms on
`/ui`: sessions, jobs/history, search/request submission, owned-library
monitoring/upgrades, native transfer controls and bounded bulk changes.
Each bulk entry reports its own result. No browser framework or JavaScript
dependency is required. See [web management](web.md) for authentication,
deployment and exact limits.

The browser stage passed
[Actions run 37206645776](https://github.com/alecerf/mynou/actions/runs/37206645776)
for commit `d7cb8eb20d364c217ed89ab183ebf754512d7dfe`: 343 tests passed with
none failed or ignored. GitHub Actions published
[v0.11.0](https://github.com/alecerf/mynou/releases/tag/v0.11.0) with seven assets.
That result does not validate the following series changes.

## Previous stage: 0.10.0

Bounded parallel TCP peers cooperate on verified torrent pieces. The
`downloads.max_peers` setting defaults to four and accepts one through eight;
the effective worker count can be lower to retain global and per-transfer
resource bounds. Set it to one for a single-peer baseline. Discovery runs
alongside usable known peers, respecting private-torrent restrictions. See
[transfer controls](transfers.md) for the exact bounds and remaining limits.

The parallel-peer stage passed
[Actions run 37203872630](https://github.com/alecerf/mynou/actions/runs/37203872630)
for commit `5b0b202b78bc906db914ef4713c57ec14807169f`: 321 tests passed with
none ignored. GitHub Actions published
[v0.10.0](https://github.com/alecerf/mynou/releases/tag/v0.10.0) with seven assets.
That result validates the parallel-peer release. Later changes require their own
completed CI run and release before claiming success or new measurements.

## Previous stage: 0.9.0

Native transfer controls add durable pause/resume, priority/FIFO scheduling,
per-file piece priority, global payload bandwidth limits, persistent counters
and ratio/time seeding policies. Earlier configuration files keep unlimited
rates and seeding until those limits are configured. Controls retain downloaded
sources and library imports. Per-file priorities change download order; they do
not skip files. See [transfer controls](transfers.md).

Full parity with Radarr, Sonarr, Pulsarr, qBittorrent, qui,
autobrr or Prowlarr remain future stages.

## Planned stages

The following sequence is provisional. A release may be split when its scope
needs separate validation. No delivery dates or performance improvements are
promised before implementation and measurement.

| Planned release | Scope | Evidence required before continuing |
| --- | --- | --- |
| 0.20 | IRC announcements, immediate grabs, filters, action routing and notifications | Bounded announcement parsing, reconnect/backoff, duplicate suppression and auditable rule decisions |
| 0.21 | Native indexer adapters, login/session management, source health and configuration | Per-adapter protocol fixtures, credential redaction, rate limits and safe session renewal |
| 0.22 | Usenet search/acquisition and management | Native protocol support, bounded message processing, integrity/recovery and explicit format limits without external helpers |
| 0.23 | Cross-seeding and further bulk automation | Verified content identity and safe reuse of existing files; no accidental extra acquisition or library overwrite |

Each stage needs meaningful automated checks and an updated support matrix.
Tests use synthetic content and local peers/services. Compatibility with a
personal installation or public-swarm throughput requires separate observed
evidence; passing local protocol fixtures does not establish either.

## Capabilities still outside the current release

- Advanced torrent transports and networking: uTP, WebTorrent, webseeds,
  automatic NAT traversal and a complete persistent DHT table.
- Broad tracker/provider coverage, changing authentication schemes and a
  comprehensive adapter catalog.
- Mature administration across multiple installations and operating systems.
- Independent review of the original cryptographic and protocol implementation.

These are tracked as explicit limits until implemented. “Zero dependencies”
describes the implementation constraint, not a guarantee of completeness,
security, optimal performance, or compatibility.

[Selection profiles](selection.md) · [Library upgrades](library.md) ·
[Transfer controls](transfers.md) · [Series and calendar](series.md) · [Current limits](limits.md) ·
[CI validation](validation.md)
