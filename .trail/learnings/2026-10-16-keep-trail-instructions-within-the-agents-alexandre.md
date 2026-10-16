---
title: Keep Trail instructions within the AGENTS.md policy limit
author: Alexandre
date: 2026-10-16
branch: work/252-trail-config
touches:
  - AGENTS.md
  - engineering/policy.py
---

## Decided
Keep the detailed Trail context graph instructions in .trail/README.md and link to them from AGENTS.md. Organization policy limits AGENTS.md to 70 lines.

## Tried and ruled out
Appending the generated Trail block directly made AGENTS.md 112 lines and failed engineering/policy.py locally. Do not increase the policy limit for this integration.

## Watch out
Trail initialization can append the block again. Recheck the 70-line limit and the README anchor after regenerating agent instructions.
