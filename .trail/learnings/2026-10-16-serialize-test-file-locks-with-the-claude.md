---
title: Serialize test file locks with the mkfifo spawn
author: claude-030f
date: 2026-10-16
branch: work/244-test-fifos
touches:
  - "src/store.rs#try_lock_file"
  - "src/store/open_tests.rs#fifo"
---

## Decided
Unit tests spawn `mkfifo` (std has none; no `unsafe`/FFI allowed). Until the child execs it shares every descriptor, so a flock that a test released by closing its descriptor stays held, and a reopen in that window fails with `would block`. Fix: every file lock goes through `store::try_lock_file`, which under `cfg(test)` takes a read guard on `store::SPAWNING`. `store::open_tests::fifo` holds it for writing until `mkfifo` is reaped. Release builds call `File::try_lock` unchanged.

## Tried and ruled out
Creating FIFOs before any store opens: `open_tests::swapped` creates them inside the `BEFORE_PRIVATE_OPEN` hook mid-open, so they cannot be hoisted. Serializing only a hand-picked list of store tests: every lock site (store, inventory, IRC staging, torrent index and state, organizer) can hit the race, so the guard lives in the shared helper.

## Watch out
New file-lock sites must call `store::try_lock_file`, never `File::try_lock`/`lock` directly; any new unit-test spawn must hold `SPAWNING` for writing like `fifo`. The `Mutex::try_lock` sites (engine, series, indexers) are unrelated. Integration tests in `tests/` spawn the `mynou` binary without this guard (built without `cfg(test)`); no failure seen yet. The race is rare: a 25-run focused Docker batch on the unfixed parent did not reproduce it, so judge fixes by mechanism, and check `docker system df`. A full Docker VM disk fails whole batches with unrelated errors.
