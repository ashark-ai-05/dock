---
id: 3
title: Implement Git worktree facts and delta diff adapter
status: review
priority: high
created: 2026-08-20T12:55:13.452842+10:00
updated: 2026-08-20T12:57:12.08888+10:00
started: 2026-08-20T12:55:13.486658+10:00
tags:
    - integration
    - git
    - delta
class: standard
---

User outcome: Dock can resolve a bound worktree, base/head revision, changed-file totals, and render the exact comparison through delta when available.\n\nAcceptance:\n- no shell interpolation; every Git/delta argument is passed separately\n- fixture repository tests cover clean and changed states\n- live Dock repo smoke prints facts and delta fallback/renderer status\n\nOut: staging, commits, rebase, merge, and push.

[[2026-08-20]] Thu 12:57
Dock resolves a worktree, branch, base/head revisions, numstat facts, and passes exact Git diff bytes to delta. Eight unit tests and a live HEAD~1 smoke passed. No staging, commits, merge, or push controls were added.
