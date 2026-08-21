---
id: 11
title: 'Slice 6.1: one-command interactive Dock TUI'
status: done
priority: high
created: 2026-08-21T08:04:40+10:00
updated: 2026-08-21T02:32:41Z
tags:
    - runtime
    - tui
    - terminal
    - usability
    - mouse
depends_on:
    - 10
class: standard
---

# Outcome

A developer launches `dock` once from a repository and lands in a polished, foreground interactive TUI. Dock starts or reconnects to its private local daemon automatically, renders the current workspace/pane tree, and supports keyboard-first and mouse-assisted pane interaction without a separate background-process command.

# Scope

- One-command foreground launcher: `dock` determines the repository-local state location, starts or reconnects to its private local daemon automatically, discovers **display-only** local coding-agent candidates, waits for the socket handshake, then enters the TUI; quitting the TUI must restore the terminal and leave the owned daemon lifecycle explicit and observable.
- Launch discovery is intentionally useful but non-invasive: Dock may show a clearly labelled “existing agents” inbox using conservative local signals (recognised provider executable + repository/worktree correlation where available), but it never attaches to their PTY, sends input/signals, persists their PID, or treats a candidate as Dock-owned. The user can start a new Dock-owned run or dismiss a candidate; existing non-Dock agents remain external/read-only.
- Replace the fixture-only board as the default application surface with a real runtime dashboard driven by protocol v6: workspace tree, active pane, run state, repository/task binding facts, discovered-agent inbox, visible errors, and concise keyboard help.
- Add the terminal-attachment transport needed for an active Dock-owned pane: trusted input forwarding and bounded live output/replay while the daemon lives. No durable raw terminal input/output.
- Render nested split panes accurately, with focus, resize, rename, close, and create/split controls. Add mouse hit-testing for focus and split-boundary resize only after the keyboard path is complete.
- Establish a deliberate visual system: colour-coded runtime states, active-pane contrast, compact status bar, readable typography, small terminal-width fallback, and no decorative dashboard clutter.
- Preserve strict ownership: input only reaches the selected exact Dock-owned PTY; no process adoption, arbitrary PID control, automatic Git/task mutation, transcript persistence, or cloud dependency.

# Acceptance

- A temporary-repository integration smoke runs one `dock` command, proves daemon bootstrap/reconnect, performs a workspace/pane operation, and exits cleanly with the caller terminal restored.
- Discovery tests prove a recognised non-Dock agent candidate is shown with an `external/read-only` state and cannot be focused as a PTY, receive input/signals, consume runtime capacity, or become durable process authority.
- Protocol and domain tests prove input cannot target an unknown, restored, exited, or unbound pane; client disconnect/reconnect leaves an owned run live.
- Ratatui rendering/action tests cover split geometry, focus, narrow terminal fallback, state colours, and keyboard commands.
- Mouse tests cover pane focus and a bounded resize gesture; mouse input never becomes terminal input unless terminal-input mode is explicitly active.
- A manual macOS walkthrough covers keyboard use, mouse focus/resize, nested panes, live output, detach/reconnect, and daemon restart recovery.
- `cargo fmt --check`, `cargo test --all-targets`, warnings-denied Clippy, and both existing Slice 5/6 smokes pass.

# Explicitly deferred

Full VT terminal emulation, mouse reporting to subprocesses, pane swap/zoom, themes/configuration, notifications, embedded LazyGit, and plugins. These follow only after the real interactive path is proven.
