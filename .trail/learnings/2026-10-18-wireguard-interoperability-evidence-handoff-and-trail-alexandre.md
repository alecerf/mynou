---
title: WireGuard interoperability evidence handoff and Trail conflict scope
author: Alexandre
date: 2026-10-18
branch: work/220-wireguard-interop
touches:
  - docs/wireguard-design.md
  - docs/wireguard-interop.md
---

## Decided
PR #257 records Actions run 39033029167 at 7c41ae1944757ca62502e86a00621f33ffc5a052 as evidence for all 13 wireguard-go 0.0.20250522 interoperability scenarios. Satisfy only the independent-interoperability gate within Linux/Docker, IPv4 inner/outer and loopback. Keep other delivery gates and the job's non-required status separate. A new head still needs fresh QA/Security and required checks.

## Tried and ruled out
Leaving the gate unresolved despite a reviewed green Actions run failed Issue #220 acceptance. Trail check's lexical conflicts with unrelated inventory job-record damage, Orca watchdog workspace caps, the 70-line AGENTS limit, TCP ICMP handshake handling and IRC deadlines do not apply to this isolated WireGuard fixture or its evidence docs. The fixture creates no inventory jobs, changes no host orchestration or AGENTS policy, and modifies no TCP/IRC behavior. References to local validation are required practice, not a reintroduction of a rejected approach.

## Watch out
Link the tested revision, actual run and prior review comments; never imply prior verdicts approve a later head. Rebase and sign before final validation/push, then obtain fresh exact-head reviews. Retain all scope limits, particularly native macOS, torrent isolation and leak protection.
