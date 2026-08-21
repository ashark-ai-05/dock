---
id: 12
title: 'Slice 6.2: dashboard-owned real-agent dispatch'
status: ready
priority: high
created: 2026-08-21T12:55:48+10:00
updated: 2026-08-21T12:55:48+10:00
tags:
    - runtime
    - tui
    - adapters
    - dispatch
    - safety
depends_on:
    - 11
class: standard
---

# Outcome

A developer launches `dock`, opens a bounded dashboard launch form for an empty focused pane, selects an installed supported coding-agent adapter plus an existing task reference and valid existing worktree, reviews the exact launch facts, and explicitly starts a Dock-owned run in that pane.

# Scope

- Add a typed, read-only dashboard launch catalog: fixed built-in adapter profiles and availability, existing valid same-repository worktrees, and read-only task references.
- Replace fixture-only dashboard launch as the normal `l`/`LAUNCH` path with a keyboard-first, mouse-assisted bounded selector form and explicit confirmation.
- Launch through the existing transactional `LaunchIntoPane` authority path; daemon-side validation remains final authority against stale UI catalog data.
- Retain Fixture as a selectable deterministic profile. Expose supported built-in providers only when their fixed executable is available: Amp, Claude Code, Codex CLI, and GitHub Copilot CLI.
- Render selected adapter/task/worktree, unavailable choices, and structured launch errors honestly.
- Preserve exact selected-pane ownership and the existing live input/output/lifecycle capability rules.

# Hard boundaries

- No arbitrary executable, command, arguments, environment, PID, or shell entry in the dashboard form. `generic` remains explicit CLI/API only.
- Task selection is read-only: no claim, move, completion, or task-system mutation.
- Worktree selection is read-only: no create/delete/checkout/branch/Git mutation.
- Never discover/adopt/attach/input/signal an external process.
- Do not bypass `LaunchIntoPane`, capacity, receipt, binding, rollback, or exact process-group safeguards.
- Catalog data is advisory only; dispatch revalidates repository, task, worktree, adapter, capacity, and selected pane at launch time.

# Acceptance

- Protocol/server/client tests strictly serialize and validate catalog request/response data; generic executables and arbitrary arguments cannot enter dashboard launch selection.
- Adapter tests prove fixed built-in profiles, availability reporting, missing-binary behavior, and generic exclusion.
- Git/worktree tests enumerate only canonical existing same-common-directory worktrees and reject stale/foreign candidates at dispatch.
- Task catalog is read-only and task selection changes no task source content or state.
- Dashboard tests prove keyboard and mouse launch-form navigation, cancellation, explicit confirmation, unavailable/empty catalog errors, and exact focused-pane `LaunchIntoPane` construction.
- Fixture selection launches through the same form and binds only the addressed Dock-owned pane; an installed real adapter profile is selectable only when available.
- New macOS smoke drives the real foreground dashboard under a PTY, uses the form to select Fixture and launch a bound run, proves repository/task/worktree/adapter facts and terminal restoration, and proves Git/task source truth unchanged. It must not use a headless substitute, `dock-workspace`, arbitrary command input, or `dockd` as the product action.
- `cargo fmt --check`, `cargo test --all-targets`, warnings-denied Clippy, and Slice 5, Slice 6, Slice 6.1, and Slice 6.2 smokes pass.

# Explicitly deferred

Provider credential handling, automatic task claims, worktree creation, arbitrary generic command entry in the dashboard, terminal semantic parsing, automatic Git mutations, plugins, and hosted coordination.
