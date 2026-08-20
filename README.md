# Dock

**The local control plane for coding-agent delivery.**

Dock is a provider-neutral runtime for coding agents. It is being built to replace the combined daily workflow of a terminal multiplexer, agent runner, task board, Git review surface, and handoff desk—without taking ownership away from Git or a repository’s task source.

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

## Current prototype status

This repository currently contains **foundations**, not the Dock runtime described above:

- Ratatui fixture Control-Pane prototype;
- strict, versioned handoff packets and atomic local-only packet storage;
- `kanban-md` task intake and atomic claim adapter;
- Git worktree/base/head/numstat facts and `delta` diff rendering;
- explicit LazyGit launch intent.

It does **not yet** own PTYs, launch/manage Amp/Claude/Codex/Copilot, provide terminal-multiplexer parity, or run cross-repository dependency gates. The [runtime product spec](docs/dock-runtime-product-spec.md) distinguishes target behaviour from shipped capability.

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

Dock’s release bar is not “a dashboard with agent labels.” It must reach the daily-workflow capabilities needed to replace a Herdr-style terminal runtime:

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
