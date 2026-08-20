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

Slice 2 now provides one explicitly bound, fixture-only repository runtime:

- `dockd` starts empty and accepts deterministic fixture dispatches through protocol version 2;
- each run is bound before launch to a canonical Git repository root, non-empty external task reference, Dock run id, and supplied existing worktree path;
- worktree paths are canonicalized, kept beneath the repository root, checked against Git's top-level, and rejected on traversal, symlink escape, or repository mismatch;
- each accepted run receives its own Dock-owned pane, PTY, and process group;
- a versioned, user-only Unix socket accepts a forward-compatible hello and strict inspect requests;
- `dock-dispatch` is the compact client/smoke path and `dock-inspect` reports one or all bound runs, including Git branch/base facts;
- the daemon and fixture process continue running when inspectors disconnect and reconnect;
- owner-only local dispatch receipts reserve run ids across daemon restarts but omit raw PTY scrollback and the raw command vector;
- malformed/mismatched requests, missing tasks, duplicate ids, and invalid bindings are rejected without process discovery, process import, Git mutation, or worktree creation.

The earlier foundations remain available:

- Ratatui fixture Control-Pane prototype;
- strict, versioned handoff packets and atomic local-only packet storage;
- `kanban-md` task intake and atomic claim adapter;
- Git worktree/base/head/numstat facts and `delta` diff rendering;
- explicit LazyGit launch intent.

Live process handles and scrollback remain in memory and disappear when `dockd` exits. Dispatch identity/launch facts persist beneath the configured state directory, but daemon restart recovery does not reattach or import the former process. Slice 2 does **not** create or mutate Git worktrees, claim tasks in a remote provider, launch real agents, accept terminal input, or provide terminal-multiplexer/dependency-gate parity. The supplied fixture worktree must already exist at a Git worktree top-level beneath (or equal to) the canonical repository root and share that repository's Git common directory.

## Slice 2 fixture dispatch smoke

Start the empty daemon. By default clients use `dockd.sock` beneath an owner-only per-user runtime directory (`$XDG_RUNTIME_DIR/dock` on Linux when available, otherwise a per-user temporary directory on Linux/macOS):

```bash
cargo run --bin dockd -- --scrollback-bytes=65536
```

From another terminal, dispatch a fixture into this existing repository worktree. The client generates a `dock_…` run id unless `--run-id=dock_smoke` is supplied for a deterministic smoke:

```bash
cargo run --bin dock-dispatch -- \
  --repo="$(pwd)" --task=FIXTURE-1 --run-id=dock_smoke \
  --worktree="$(pwd)" -- sh -c 'pwd; git rev-parse --show-toplevel'
cargo run --bin dock-inspect -- --run-id=dock_smoke
```

Use a fresh run id for each accepted dispatch; ids remain reserved in `.dock/local/dispatches`. Dock requires its state and dispatch directories to be owned by the current user and inaccessible to group/other users (mode `0700` or stricter); receipt files are mode `0600`. Run `dock-inspect` without `--run-id` to list all runs. The explicit `--socket=PATH` option remains available on all three commands.

Stop the foreground daemon with `Ctrl-C`. Slice 2 still has no remote stop/import request: no API accepts a PID or discovers an external process. Existing Slice 1 socket recovery, finite client deadlines, bounded clients, bounded scrollback, and Dock-owned process-group cleanup remain in force.

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
- Raw dispatch command vectors are never written to durable receipts because arguments can contain credentials.
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
