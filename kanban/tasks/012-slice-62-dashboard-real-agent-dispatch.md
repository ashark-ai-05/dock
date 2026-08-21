---
id: 12
title: 'Slice 6.2: repo-optional terminal runtime and real-agent launch'
status: ready
priority: high
created: 2026-08-21T12:55:48+10:00
updated: 2026-08-21T13:05:00+10:00
tags:
    - runtime
    - tui
    - adapters
    - dispatch
    - safety
    - tmux-parity
depends_on:
    - 11
class: standard
---

# Outcome

A developer can run `dock` from **any directory**, including outside a Git repository, and use it as a local terminal-runtime workspace: create/split/focus/resize panes, reconnect to Dock-owned live processes, and explicitly launch a fixed supported coding-agent profile into an empty pane. When a Git repository and explicit task/worktree binding are available, Dock may additionally show those facts and use its existing repository-bound dispatch path; they are an optional control-plane layer, never a prerequisite for tmux-like terminal use.

# Scope

- Make repository discovery optional for the foreground `dock` runtime. Outside Git, select a safe per-directory/private local state identity without creating or requiring Git/task artifacts.
- Add a typed read-only dashboard launch catalog for fixed built-in adapter availability. In a repository it also exposes canonical existing same-repository worktrees and read-only task references when configured; outside a repository those fields render as explicitly unavailable, not errors.
- Replace fixture-only dashboard launch as the normal `l`/`LAUNCH` path with a keyboard-first, mouse-assisted bounded selector form and explicit confirmation.
- Support two **separate, explicit** launch modes:
  - **Terminal launch (default):** creates an exact Dock-owned PTY in the current runtime directory without repository, worktree, task, Git, or task-system bindings.
  - **Repository-bound dispatch (optional):** only when a verified Git repository and selected existing worktree/task are available; continues using the existing transactional `LaunchIntoPane` authority path.
- Retain Fixture as a selectable deterministic profile. Expose supported built-in providers only when their fixed executable is available: Amp, Claude Code, Codex CLI, and GitHub Copilot CLI.
- Render whether the selected pane/run is unbound terminal work or repository/task/worktree-bound dispatch; never infer facts that do not exist.

# Hard boundaries

- No arbitrary executable, command, arguments, environment, PID, or shell entry in the dashboard form. `generic` remains explicit CLI/API only.
- Terminal launch does not invent a repository, task reference, worktree, branch, or Git fact. It must not call Git or task-system operations.
- Task selection is read-only: no claim, move, completion, or task-system mutation.
- Worktree selection is read-only: no create/delete/checkout/branch/Git mutation.
- Never discover/adopt/attach/input/signal an external process.
- Terminal launch and repository dispatch each retain exact Dock-created process-group ownership, selected-pane binding, capacity, lifecycle, and rollback guarantees. Neither may bypass final daemon-side validation.
- Existing repository-bound API/CLI semantics remain strict and backward compatible.

# Acceptance

- A temporary **non-Git** directory integration smoke invokes only `dock`, starts/reconnects its private daemon, creates and operates a workspace/panes, uses the launch form to start Fixture in the selected pane, reconnects, stops it, exits, restores the caller terminal exactly, and proves no Git/task probing or files occurred.
- The non-Git dashboard can select an available fixed real-agent adapter profile. If a provider binary is unavailable it is visibly unavailable and cannot launch; no arbitrary command/executable/argument route exists.
- In a repository, the launch form renders the optional verified repository/task/worktree mode and launches it through `LaunchIntoPane`; stale/foreign worktree or task data is rejected again by the daemon.
- Protocol/server/client tests strictly serialize and validate separate terminal-launch and repository-bound-dispatch requests; unbound terminal launches cannot contain repository/task/worktree fields, and generic executable/argument fields cannot enter dashboard selection.
- Adapter tests prove fixed built-in profiles, availability reporting, missing-binary behavior, and generic exclusion.
- Dashboard tests prove keyboard and mouse launch-form navigation, cancellation, explicit mode/adapter confirmation, unavailable/empty catalog errors, exact focused-pane request construction, and honest unbound/bound fact rendering.
- Fixture selection launches through the same form and binds only the addressed Dock-owned pane. Existing external candidate discovery remains display-only and cannot be adopted.
- `cargo fmt --check`, `cargo test --all-targets`, warnings-denied Clippy, and Slice 5, Slice 6, Slice 6.1, and Slice 6.2 smokes pass.

# Explicitly deferred

Arbitrary shell command entry, provider credential handling, automatic task claims, worktree creation, terminal semantic parsing, automatic Git mutations, plugins, and hosted coordination.
