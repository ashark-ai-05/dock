# Dock

**A local runtime for coding agents.**

Dock gives coding-agent work a safe local home: owned workspaces and panes, provider-neutral agent runs, human-controlled lifecycle actions, and optional multi-repository delivery gates. Git remains the source of code truth; your task system remains the source of task truth.

## Run it

### 1. Start Dock

From a repository, launch the foreground dashboard; it reconnects to the private daemon or starts
one automatically:

```bash
cargo run --bin dock
```

The separate `cargo run --bin dockd` command remains supported for explicit daemon operation.

### 2. Create a workspace and owned run in the dashboard

Press `n` to create a workspace, then `l` to launch the explicit Dock-owned fixture path. The pane
shows the authoritative repository, task, run, workspace, and pane binding returned by the runtime;
an empty pane labels each fact as unbound. Press `i` to send input to a selected owned run.

Discovered existing agent processes are display-only and say `external/read-only`. Press `d` or
click `dismiss all` to remove those candidates from the view. Launching never adopts a discovered
process.

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

Dock is not yet a full terminal multiplexer. Themes, zoom, notifications, and durable transcript replay are deferred. Bounded live scrollback is shipped: each daemon-owned runtime retains only its configured byte limit in memory for reconnects to that same daemon, truncating the oldest bytes as new output arrives; it is never written to layout or restored after daemon restart. See the [terminal-runtime parity matrix](docs/terminal-runtime-parity.md) for the exact status.

## Verify a change

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
scripts/smoke-slice5-macos.sh
scripts/smoke-slice6-macos.sh
scripts/smoke-slice61-macos.sh
```

See the [implementation breakdown](docs/implementation-breakdown.md) for planned work and acceptance evidence.

## Licence

MIT
