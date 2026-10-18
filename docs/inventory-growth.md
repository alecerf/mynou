# Inventory growth evidence

This is the acceptance record for [#217](https://github.com/alecerf/mynou/issues/217)
and [#106](https://github.com/alecerf/mynou/issues/106). It concerns retained
inventory capacity and bounded storage operations. Physical storage, CPU and
scheduled worker throughput remain finite.

## Reproduce and interpret

The `Inventory growth evidence` workflow runs on `macos-26` (arm64), against
synthetic data only, using a release library test harness, Rust 1.99.0 and no
Cargo dependencies. It also supports manual dispatch. The measurement script
is [.github/scripts/inventory_growth.py](../.github/scripts/inventory_growth.py).
Locally, compile with:

```sh
cargo test --release --lib --no-run --offline --locked --message-format=json
```

Pass the emitted library test executable to:

```sh
python3 .github/scripts/inventory_growth.py PATH_TO_HARNESS --output OUTPUT_DIRECTORY
```

`--smoke` uses 200 rows per category and checks the harness only; it is not
capacity evidence. `--case NAME` selects one category. The complete run uses
three increasing sizes per category. Each size has its own private disposable
directory. Construction and reading run in separate processes; no real
configuration, media, credentials or deployment is accessed, and no network
worker runs. Construction uses bounded batches through existing storage
primitives; transfers use the normal snapshot/index save path per record.

The artifact contains the environment and measured commit, JSON results, raw
harness output, BSD `time -l` output and a Markdown table. Timings use monotonic
`Instant` microseconds, displayed as milliseconds. Peak RSS is the process high
water mark from `/usr/bin/time -l`, whose macOS field is **bytes**, divided by
1,048,576 for MiB. It includes the test harness, cache, point update and complete
page walk; it is not a sum of per-operation allocations. Construction RSS is
reported separately. On-disk size sums logical sizes of every generated file,
including markers, snapshots, secondary entries, history and unreclaimed log
bytes; it is not filesystem allocated blocks or payload/media size.

Open includes the store's writable initialization/recovery checks in a fresh
process, without building a legacy inventory. Page measures the first hydrated
200-row page after open, except source policies whose internal primary page is
measured (there is no retained-policy listing endpoint). Routine pass means:

| Category | Synthetic shape | Storage work in one pass |
| --- | --- | --- |
| Jobs/history | Unique queued movies; one 4,096-byte history message per job | One normal claim, including its durable write |
| Library | Current imported tips, alternating movies/episodes, with release baselines and one 4,096-byte history message per tip | Four due monitoring tips |
| Series | Three seasons of 30 episodes per scope, 300-byte title padding | Four due scopes |
| Requesters | Pending movie demands with 1,400-byte origin padding, plus operator interests | One 64-demand pending admission page |
| Feeds | Baselined retained sources with 16 seen identities each | One 200-row active-reconciliation page |
| Source policies | Retained source controls, including disabled sources | Point guard and explicit `not_applicable` scheduler diagnostic |
| IRC | Pending announcements with 64 evaluations and padded titles | One 200-row pending routing page |
| Transfers | Queued v1 identities with durable snapshots and controls | One 64-identity admission queue page |

Point update changes one existing row with the normal store operation. The full
walk follows cursors to exhaustion, drops each page before the next and verifies
that the visited row count equals the constructed inventory. History remains
per-job bounded; total history grows with the jobs. The minimum fixture sizes
exceed the old job count and aggregate snapshot/journal sizes, series scope/
episode/snapshot limits, requester count/bytes, feed count/bytes, policy count/
bytes and IRC count/bytes. Transfers had no previous fixed count cap: their
sizes exercise increasing retained catalogs beyond a 200-row page.

These are individual warm-filesystem runs with fresh application caches, not
cold-disk experiments, statistical latency percentiles or throughput guarantees.
They measure store opening and bounded scheduling selections, not end-to-end
network acquisition, catalog refreshes, whole-service migrations or delivery
rates. The same configured worker limits mean a larger backlog takes longer
to finish. Existing regression tests below establish recovery, fairness and
bounded work independently of these timings. No performance threshold is added
to the required test suite.

## Results

Pending the measured Actions run. No capacity acceptance is asserted until the
results and the successful required checks are linked here.

## Remaining-bounds source audit

Source audit of the indexed write/read paths found no application-imposed total
inventory count or aggregate-byte ceiling. Checked integer/file-offset
representations and available disk remain physical/format constraints.

| Path | Intentional surviving bounds |
| --- | --- |
| `src/inventory/{mod,tree,persistence}.rs` | 1 MiB index value; 200-row / 4 MiB pages; 16,384 changes/dirty pages and 128 MiB WAL per transaction; tree height 63; 2,048 cached pages; bounded log-cleaning work |
| `src/store/{legacy,layout,ops}.rs`, `src/store.rs` | 16 MiB per hydrated job (chunked); 1,000 events per job / 4,096 bytes per message; page hydration budget and 1,000 examined search rows; bounded claim/ownership windows and continuing cursors |
| `src/series/{legacy,storage,engine}.rs` | Legacy 8 MiB snapshot input; 2,000 episodes/numbering decisions per scope/plan; 20,000-row calendar count/offset compatibility window with continuation beyond it; due-pass visit, attempt and time budgets |
| `src/requesters/{mod,index,persistence}.rs` | Legacy 10,000 demands/operator interests and 16 MiB input; 16 MiB per demand; 512 demands/origins per patch or origins per demand; 32 configured accounts; compatibility offsets stop at 10,000, cursors continue |
| `src/indexers/{feed,feed/storage,policy,policy/storage}.rs` | Legacy feed 1,000 rows / 4 MiB and policy 1,000 rows / 2 MiB; indexed 200-row patches/pages; 1,024 seen identities per feed; 64 actively watched sources and bounded polls/routing, independent of retained source count |
| `src/irc/{mod,index,persistence,routing}.rs` | Legacy 1,000 rows / 8 MiB; 200-row patches/pages and routing/backoff windows; bounded configured sources/rules, line and evaluation sizes |
| `src/torrent/{index,control,parallel}.rs`, `src/torrent.rs` | 64 MiB per status snapshot, bounded metadata/control parsers; 200-row listing and continuing queue cursors; configured active-transfer/peer/concurrency limits, independent of retained transfer count |

The former snapshot/count constants are used by legacy input validation, not
indexed retention. Configuration/parser/network limits constrain an operation
or the actively configured sources; they do not truncate retained inventory.
Recovery/downgrade instructions for each store remain linked in
[inventory-storage.md](inventory-storage.md). No version or storage format is
changed by this evidence increment.

## Parent acceptance mapping

The required macOS CI suite runs these existing regressions. Test symbols below
identify the scenario within the linked file, without duplicating its assertions
in the measurement harness.

| #106 criterion | Evidence |
| --- | --- |
| Movies above former limits, HTTP/WebSocket/CLI access | [`jobs_above_the_old_total_cap_reach_http_websocket_and_cli_pages`](../tests/web_ui.rs); jobs/library growth measurements |
| Series above scope/episode/byte limits | [`inventories_above_every_former_limit_page_completely_and_survive_restart`](../src/series/tests.rs); series growth measurements |
| Growth across pages, complete access, bounded page memory | [`grows_past_old_caps_and_pages_every_record_after_restart`, `byte_bounded_pages_resume_after_last_emitted_key_and_reject_mutations`](../src/inventory/tests.rs); every measured full walk |
| Migration, exact backups and restart | [`every_legacy_format_migrates_as_snapshot_journal_or_both`](../src/store/tests.rs); [`schema_one_and_two_snapshots_migrate_exactly_and_the_original_is_preserved`](../src/series/tests.rs); per-store recovery suites |
| Interrupted writes and migration | [`migration_resumes_after_an_interruption_at_every_stage`, `a_shared_group_commits_all_owners_or_none_at_every_crash_point`](../src/store/tests.rs); [`published_redo_repairs_covered_torn_root_pages_and_records_only`](../src/inventory/tests.rs) |
| Storage exhaustion without record loss | [`a_full_disk_at_each_commit_stage_keeps_every_committed_record`](../src/inventory/tests.rs); `full_disk_at_every_migration_flush_preserves_legacy_and_restarts` in [jobs](../src/store/tests.rs), [series](../src/series/tests.rs), [requesters](../src/requesters/index_tests.rs), [feeds](../src/indexers/feed/storage/tests.rs), [policies](../src/indexers/policy/storage/tests.rs), [IRC](../src/irc/tests.rs), and the [native-transfer migration suite](../src/torrent/index.rs) |
| Fair scheduling, cancellation and policy revisions | [`due_work_is_oldest_first_bounded_per_pass_and_never_starves_old_entries`](../src/series/tests.rs); [`monitoring_windows_reach_old_tips_beyond_inventory_prefixes`, `claim_scan_resumes_after_restart_and_reaches_past_new_old_entries`](../src/store/tests.rs); [requester sweeps](../tests/requester_sweeps.rs), [feed revocation](../tests/feed_watching.rs), [transfer admission fairness](../tests/transfer_inventory_fairness.rs) |
| Shared-demand deduplication and durable identity | [`durable_dedup_and_exclusive_owner`, `transfer_ownership_reads_a_fixed_window_beyond_former_limits`](../src/store/tests.rs); [requester acquisition](../tests/requester_acquisition.rs), [shared episode files](../tests/shared_episode_files.rs) |
| Bounded per-pass work and diagnostic truthfulness | Per-store backlog/freshness regressions in [jobs/library](../src/store/tests.rs), [series](../src/series/tests.rs), [requesters](../src/requesters/index_tests.rs), [IRC](../src/irc/tests.rs), [feeds](../tests/feed_watching.rs), [transfers](../src/torrent/index.rs), and [source-policy non-applicability](../src/indexers/policy/storage/tests.rs); [diagnostic semantics](inventory-storage.md#exact-range-summaries) |
| Increasing-size disk/time/peak-RSS evidence on macOS arm64 | Results above and the preserved Actions measurement artifact |

A failed measurement or acceptance regression keeps the parent open; any newly
discovered implementation gap is tracked separately from this evidence task.
