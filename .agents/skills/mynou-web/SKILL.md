---
name: mynou-web
description: Own Mynou standards-based browser engineering, accessible responsive interactions, rendering and client security.
---

Invoke for HTML/CSS/JavaScript, web state, networking/caching, keyboard behavior
or frontend trust. Skip Rust-only internals unless browser contracts change.
Mynou's embedded UI uses browser platform capabilities without a package manager,
framework or frontend build dependency.

Follow UX acceptance. Prefer semantic controls/labels, predictable focus, responsive
layout, explicit loading/empty/error/retry states, bounded requests and handling
of stale responses. Do not expose unnecessary implementation details. Avoid HTML
injection and secret storage; authorization remains server-side.

Make bounded commits and original regression fixtures. Tests, lint, runtime and
browser checks belong in CI. Use browser automation only when actually available;
never claim screenshots, accessibility or browser behavior verification otherwise.
Use the lease, Issue/PR and truthful CI evidence. Handoff to QA; record adjacent
improvements without aesthetic rewrites or unnecessary dependencies.
