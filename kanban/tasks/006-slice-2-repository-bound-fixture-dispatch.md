---
id: 6
title: 'Slice 2: repository-bound fixture dispatch'
status: done
priority: high
created: 2026-08-20T14:50:31.458046+10:00
updated: 2026-08-20T15:20:59.591065+10:00
started: 2026-08-20T15:20:59.59272+10:00
tags:
    - runtime
    - repository
    - worktree
class: standard
---

Implement Slice 2 from docs/implementation-breakdown.md. Acceptance: explicit task/reference-to-run-to-worktree binding, input validation, deterministic fixture dispatch, and no arbitrary process/worktree import or Git mutation.

[[2026-08-20]] Thu 15:20
Slice 2 ready for review: canonical repository/task/run/worktree binding; deterministic fixture dispatch; strict rejection of traversal, symlink escape, non-Git roots, repository mismatch, missing task reference, and duplicate runs. Owner-only local receipts contain binding evidence only—no raw command or scrollback. Validation: 33 tests, Clippy warnings denied, live macOS dispatch/inspect smoke with 0700 state and 0600 receipt/socket.
