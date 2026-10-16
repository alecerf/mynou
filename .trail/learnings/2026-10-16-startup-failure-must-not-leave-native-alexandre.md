---
title: Startup failure must not leave native transfers running; claim cursor checkpoints
author: Alexandre
date: 2026-10-16
branch: work/214-bounded-inventory
touches:
  - "src/engine/startup.rs#start_transfers"
  - "src/store.rs#claim_filtered"
  - src/engine/startup_tests.rs
  - src/store/tests.rs
---

## Decided
- `Engine::start_transfers` publishes the native `Client` into `Engine::downloads` only after `cancel_orphaned_transfers` and `retire_transfers` pass on the local client. Any error drops it, and `impl Drop for Client` stops the listener and manager threads. A published client outlives a failed startup, because the failure is sticky and the process keeps serving.
- The claim cursor (`CLAIM_CURSOR_KEY`) lives in `Store::claim_cursor` and is written durably on a process's first claim attempt, then at most once per `CLAIM_CHECKPOINT_SECS` (60) of the caller's claim clock `at`. A successful lease still deletes it in the lease transaction. Saving it on every attempt cost one F_FULLFSYNC per worker tick whenever more than 256 queued jobs were unclaimable.

## Tried and ruled out
- Checkpointing every N passes: `claim_scan_resumes_after_restart_and_reaches_past_new_old_entries` restarts after each turn, so the cursor would never advance across restarts. The fresh-process checkpoint is what keeps restart fairness.
- Injecting a transfers-stage failure through a contradicting ownership entry: `retire_transfers` checks `client.contains` first, so that needs a seeded native transfer. Damaging the job record (`Store::record_key` plus `inventory::damage_record` on `jobs.idx`) of a plain, non-requester request with a pending retirement is simpler. Only `pending_retirements` reads that record, and it does so in stage 12.

## Watch out
- Count durable commits with `store::flushes()`, which is thread-local and test-only. Promotion (`db.promote`) commits on its own while `claim_pass[0] > 0`, so drain promotions before you measure claim writes.
- Native macOS tests need a physical `TMPDIR` such as `/private/tmp/...`, because the store path guard rejects the symlinked default.
