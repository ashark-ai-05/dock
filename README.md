# Dock

**The local control plane for coding-agent delivery.**

Dock is a provider-neutral runtime for coding agents. It is being built to unify the daily workflow of a terminal multiplexer, agent runner, task board, Git review surface, and local delivery control plane—without taking ownership away from Git or a repository’s task source.

> One repository works immediately. Open more repositories when you need a local delivery programme with explicit dependencies and shared agent capacity.

## Product direction

Dock will own its workspaces, panes, PTYs, process groups, agent runs, runtime recovery, and local control API. It will support first-class agent adapters for tools such as Amp, Claude Code, Codex CLI, and GitHub Copilot CLI, plus a safe generic process fallback.

Its Control Pane brings together runtime state, task/run/worktree bindings, Git evidence, explicit handoffs, dependency gates, and human delivery decisions.

| Domain | Authority |
|---|---|
| Workspaces, panes, PTYs, process groups, runtime recovery | Dock |
| Programme graph, capacity, bindings, handoffs, dependency gates | Dock |
| Task cards, claims, repository task state | configured source, initially `kanban-md` |
| Worktree, branch, commit, merge facts | Git |
| Colour diff and review context | Dock, compatible with `delta` presentation |
| Advanced interactive Git mutation | human through LazyGit or another explicit client |

Dock must never infer task completion from terminal output, control a process it does not own without explicit import, or stage/commit/rebase/merge/push/deploy automatically.

## Current status

Slice 1 now provides the smallest local Dock-owned runtime:

- `dockd` owns exactly one explicitly supplied fixture command in a PTY and its own process group;
- a versioned, user-only Unix socket accepts a forward-compatible hello and strict inspect requests;
- `dock-inspect` reports owned process state and bounded in-memory scrollback;
- the daemon and fixture process continue running when inspectors disconnect and reconnect;
- malformed and mismatched protocol requests are rejected without process discovery or control.

The earlier foundations remain available:

- Ratatui fixture Control-Pane prototype;
- strict, versioned handoff packets and atomic local-only packet storage;
- `kanban-md` task intake and atomic claim adapter;
- Git worktree/base/head/numstat facts and `delta` diff rendering;
- explicit LazyGit launch intent.

Runtime process state, launch diagnostics, and scrollback are in memory only and disappear when `dockd` exits. The earlier handoff-packet foundation has separate durable local storage, but Slice 1 does not persist launch receipts or runtime recovery. It also does **not yet** launch/manage Amp/Claude/Codex/Copilot, accept terminal input, provide terminal-multiplexer parity, or run cross-repository dependency gates. The [runtime product spec](docs/dock-runtime-product-spec.md) distinguishes target behaviour from shipped capability.

## Slice 1 runtime

Start the daemon with one explicit fixture command. By default, both commands use `dockd.sock` beneath an owner-only per-user runtime directory (`$XDG_RUNTIME_DIR/dock` on Linux when available, otherwise a per-user temporary directory on Linux/macOS):

```bash
cargo run --bin dockd -- --scrollback-bytes=65536 -- \
  sh -c 'i=0; while :; do i=$((i+1)); echo "fixture tick $i"; sleep 1; done'
cargo run --bin dock-inspect
```

The explicit `--socket` override remains available:

```bash
mkdir -p .dock/run
cargo run --bin dockd -- --socket=.dock/run/dockd.sock --scrollback-bytes=65536 -- \
  sh -c 'i=0; while :; do i=$((i+1)); echo "fixture tick $i"; sleep 1; done'
```

From another terminal, inspect it non-interactively. Run this command repeatedly to demonstrate detach/reconnect without restarting the fixture:

```bash
cargo run --bin dock-inspect -- --socket=.dock/run/dockd.sock
```

Stop the foreground daemon with `Ctrl-C`. Slice 1 has no remote stop request by design: there is no API that accepts a PID or discovers an external process. After a forced termination, startup automatically removes a stale default socket only when it is a Unix socket owned by the effective user, remains inside Dock's owner-only runtime directory, and refuses a connection probe. Startup never stale-recovers a live socket, an untrusted path, or an explicit `--socket` override. Client reads have a finite deadline and concurrent clients are bounded; a stalled or excess inspector is rejected without stopping the daemon.

## The daily control loop

```text
open repository
  → Dock-owned workspace, worktree, and agent pane
  → bind external task ↔ run ↔ worktree ↔ Git base
  → agent emits explicit handoff
  → Control Pane shows evidence, checks, and question
  → human routes decision or opens bound worktree in LazyGit
  → optional dependency gate releases downstream work
```

The same loop works for a single repository. With multiple repositories open, Dock adds isolation, a portfolio view, capacity limits, and explicit cross-repository dependency edges.

## Runtime feature contract

Dock’s release bar is not “a dashboard with agent labels.” It must reach the daily-workflow capabilities expected from a complete coding-agent terminal runtime:

- workspaces, tabs, panes, splits, swaps, focus, resize, zoom, layout persistence;
- owned PTYs/process groups, bounded scrollback, detach/reattach, and recovery;
- local CLI and versioned Unix-socket API;
- agent launch, attach, focus, interrupt, stop, restart, and explicit lifecycle state;
- themes, configuration, notifications, and safe local extensions;
- native task/run/worktree/handoff/evidence control;
- single- and multi-repository operation;
- `delta`-quality colour Git review and explicit LazyGit access.

See the [vertical-slice plan](docs/implementation-breakdown.md) for the delivery sequence and acceptance evidence.

## Existing foundation commands

```bash
# fixture-backed Control Pane prototype
cargo run

# inspect a configured kanban-md board; optionally atomically claim a task
cargo run -- --kanban-dir=kanban
cargo run -- --kanban-dir=kanban --claim=dock-worker

# inspect real Git facts and a delta-rendered comparison
cargo run -- --git-dir=. --base=HEAD~1

# save and load a strict local-only handoff packet
cargo run -- --save-handoff=fixtures/demo-handoff.json
cargo run -- --load-handoff=fixture_DOCK7
```

The current prototype’s keyboard controls are `j/k` select, `a` accept an explicit scope decision, `r` request changes, `l` show a bound LazyGit command, and `q` quit.

## Safety and privacy

- Local-first by default; no hosted backend or telemetry is required.
- Dock does not store API keys or agent credentials; each agent retains its normal authentication flow.
- Raw terminal transcripts are not durable by default.
- External command inputs are structured and validated; durable packets reject unexpected schema fields.
- Git mutation remains a deliberate human action.

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Licence

MIT
