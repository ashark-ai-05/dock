# Dock

**A local runtime for coding agents.**

Dock gives coding-agent work a safe local home: owned workspaces and panes, provider-neutral agent runs, human-controlled lifecycle actions, and optional multi-repository delivery gates. Git remains the source of code truth; your task system remains the source of task truth.

## Run it

### 1. Start Dock

From any directory, including outside Git, launch the foreground dashboard; it reconnects to that
directory's private daemon or starts one automatically:

```bash
cargo run --bin dock
```

The separate `cargo run --bin dockd` command remains supported for explicit daemon operation.
Dock speaks protocol v7; a v6 daemon must be stopped before this client can connect to it.

### 2. Create a workspace and owned run in the dashboard

Every Dock pane is a real terminal from the moment it exists: it auto-launches `$SHELL` (falling
back to `/bin/sh`) immediately, so a pane is never inert, and full VT emulation (via `vt100`,
rendered through `tui-term`) means `vim`, agent CLIs, and anything else that uses the alternate
screen or redraws in place work correctly.

`Ctrl+B` is Dock's command prefix, chosen to match tmux. Unprefixed keystrokes go straight to the
focused pane; `Esc` is always forwarded to the pane and never intercepted; `Ctrl+B` pressed twice
sends a literal `Ctrl+B` (`0x02`) to the pane instead of opening a command.

| Key (after `Ctrl+B`) | Action |
|---|---|
| `n` | new workspace |
| `[` / `]` | switch workspace |
| `h` / `v` | split horizontal / vertical |
| arrows, `Tab` / `Shift+Tab` | focus |
| `+` / `-` | resize the focused split |
| `z` | zoom (toggle full-area view of the focused pane) |
| `r` | rename |
| `x` | close |
| `l` | launch |
| `d` | leave — runs keep running |
| `?` | help |
| `q` | quit |

Press `Ctrl+B l` for the compact fixed-provider picker. Type to filter, use arrows or `j`/`k`,
press `Enter` to review the exact pane and terminal-unbound versus optional repository-bound mode,
then `Enter` again to launch. Safe provider/mode choices are retained for the current dashboard
runtime. Missing fixed executables are labelled and cannot launch. Every form uses `Esc` to cancel.
Pane commands with no valid target show an inline reason.

The pane's own bound run identity is shown in its border title (the body is the live emulated
screen and has no room for it); an unbound terminal never invents Git or task facts.
Repository/task/worktree catalog discovery starts only when launch is opened and does not delay
the initial dashboard.

Discovered existing agent processes are display-only and say `external/read-only`. Click
`dismiss all` in the sidebar to remove those candidates from the view — there is no keyboard
binding for this action. Launching never adopts a discovered process.

The direct noninteractive workspace commands remain available for scripts and compatibility, but
they are not the normal user or Slice 6.1 smoke path:

```bash
cargo run --bin dock-workspace -- create daily "Daily work" editor
cargo run --bin dock-workspace -- split daily editor agent vertical
cargo run --bin dock-workspace -- inspect
```

You can focus, resize, rename, or close Dock-owned panes:

```bash
cargo run --bin dock-workspace -- focus daily agent
cargo run --bin dock-workspace -- resize daily agent 650
cargo run --bin dock-workspace -- rename-pane daily agent "Coding agent"
```

## Agent awareness

Dock classifies each of its own runs into one of four states, from the screen content it renders,
and sorts the sidebar blocked-first so whichever runs are costing the most attention surface at
the top:

- **blocked** — waiting on you
- **working** — actively producing output
- **done** — finished and idle
- **idle** — no run bound, or nothing happening yet

This is heuristic, not a protocol the agent speaks to Dock. Detection of processes Dock did not
launch is display-only: they appear labelled `external/read-only` and are never adopted,
controlled, or classified into these four states.

### 3. Use direct runtime commands when scripting

Use `fixture` to prove the runtime before using a real provider:

```bash
cargo run --bin dock-dispatch -- \
  --repo="$(pwd)" --task=TRY-1 --run-id=dock_try_1 \
  --worktree="$(pwd)" --adapter=fixture -- -c 'sleep 30'
cargo run --bin dock-inspect -- --run-id=dock_try_1
cargo run --bin dock-agent -- --run-id=dock_try_1 --operation=stop
```

For a real agent, replace `fixture` with one of:

```text
amp | claude-code | codex-cli | github-copilot-cli
```

Dock checks that the provider binary is available before creating a run. Agent authentication stays with that agent's normal local setup.

## Features

### Daily runtime

- Create, split, focus, resize, rename, close, and inspect Dock-owned panes.
- Persist bounded layout metadata across daemon restart.
- Restore prior panes as **restored**, never as attached former processes.
- Run only Dock-created PTYs and process groups; stop, interrupt, focus, or restart only those runs.
- Use provider-neutral adapters: Amp, Claude Code, Codex CLI, GitHub Copilot CLI, fixture, or an explicit generic executable.

### Programme control

- Run multiple repositories with global and per-repository agent capacity.
- Reserve capacity for human review.
- Release downstream work only through an explicit local chain:

```text
exact upstream run → valid handoff → human decision → one downstream release
```

- Inspect active, queued, and blocked local programme work with `dock-programme`.

```bash
cargo run --bin dock-programme
```

### Safety by default

- Dock never automatically stages, commits, rebases, merges, pushes, deploys, creates worktrees, or mutates task systems.
- By default Dock intentionally creates private durable runtime state and records under
  `.dock/local` in the current repository. That directory is ignored by this repository's
  `.gitignore`; its no-follow directories and files are restricted to modes 0700 and 0600. This
  expected local state does not change Git history, the index, tracked worktree content, or task
  truth. Use `--state-dir=...` when the state must live elsewhere.
- Dock never discovers, imports, or controls arbitrary processes.
- Durable state is private. Semantic layout metadata contains only bounded workspace/pane topology
  and labels: never credentials, terminal output, commands, repository/worktree paths, run bindings,
  PIDs, or process-group IDs. Separately, private dispatch receipts necessarily retain the owned
  runtime binding—including repository/worktree identity, run and pane identity, PID, process-group
  ID, and lifecycle/provider state—for lifecycle control and reconciliation; they do not retain raw
  terminal output or credentials.
- Restart recovers layout metadata only; it does not reattach or adopt processes.

## What is not here yet

Dock is not yet a full terminal multiplexer. Pane swap, loading alternative theme palettes, and
notifications are deferred, and durable transcript replay across a daemon restart is out of scope.
Bounded live scrollback is shipped: each pane retains only its configured row budget (default
2000, `dockd --scrollback-rows`) in memory for reconnects to that same daemon, discarding the
oldest rows as new output arrives; it is never written to layout or restored after daemon restart.

**`Ctrl+C` does not currently interrupt a running pane child.** The launch guardian backgrounds
the worker shell, and POSIX sets `SIGINT`/`SIGQUIT` to ignore for a non-interactive shell's
background jobs — a disposition that survives `exec`. `stop` and daemon/guardian cleanup (which
use `SIGTERM`) are unaffected. This is a known defect with a dedicated follow-up task, not a
silent gap; see the [terminal-runtime parity matrix](docs/terminal-runtime-parity.md) for the
exact status of every capability, including this one.

## Verify a change

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
scripts/smoke-slice5-macos.sh
scripts/smoke-slice6-macos.sh
scripts/smoke-slice61-macos.sh
scripts/smoke-slice62-nongit-macos.sh
```

See the [implementation breakdown](docs/implementation-breakdown.md) for planned work and acceptance evidence.

## Licence

MIT
