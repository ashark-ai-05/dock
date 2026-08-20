---
id: 5
title: 'Slice 1: owned runtime and reconnectable Control Pane'
status: review
priority: high
created: 2026-08-20T13:58:31.855121+10:00
updated: 2026-08-20T14:49:53.009172+10:00
started: 2026-08-20T14:49:53.00984+10:00
tags:
    - runtime
    - daemon
    - pty
class: standard
---

Implement Slice 1 from docs/implementation-breakdown.md. Acceptance: versioned local protocol, Dock-owned fixture process, reconnectable inspect/control surface, no unowned-process control.

[[2026-08-20]] Thu 14:49
Slice 1 is ready for review: dockd owns a fixture PTY/session/process group through a strict local Unix-socket protocol; bounded in-memory scrollback, reconnectable inspection, owner-only runtime/socket permissions, safe stale-default-socket recovery, protocol mismatch/timeout/admission tests, and no arbitrary-process control. Verified with 28 Rust tests, clippy warnings denied, and macOS daemon/inspect reconnect smoke.
