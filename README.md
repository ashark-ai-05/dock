# Dock

**The handoff desk for coding-agent runtimes.**

Dock makes an agent stop legible: what task it owned, what changed, what proof exists, what it needs, and who should act next.

## Product boundary

Dock coordinates existing local-first tools; it does not replace them.

| Tool | Owns |
|---|---|
| kanban-md | Markdown task contracts, claims, task status |
| Herdr | managed agent panes, runtime state, agent messaging |
| Git | worktrees, branches, diffs, commits and merge facts |
| delta | high-quality diff presentation |
| LazyGit | human Git operations |
| Dock | task ↔ run ↔ worktree binding, handoff inbox, evidence context, review routing |

V0.1 excludes terminal multiplexing, Kanban/Git-client replacement, transcript surveillance, automatic merge/deploy, cloud backend, and model/vendor lock-in.

## First vertical slice

The initial fixture-backed Ratatui application demonstrates the human handoff loop:

1. show a bound task/run/worktree;
2. display an explicit agent handoff, Git facts, and declared check evidence;
3. accept a human scope decision or route changes back;
4. show the exact LazyGit command that would open the bound worktree;
5. never infer that a task is complete or merge code automatically.

Run it:

```bash
cargo run
```

Controls: `j/k` select, `a` accept an explicit scope decision, `r` request changes, `l` show the bound LazyGit command, `q` quit.

### Read and atomically claim kanban-md tasks

Dock delegates task mutation to `kanban-md`; it never edits task Markdown directly.

```bash
# list durable task contracts
cargo run -- --kanban-dir=kanban

# atomically claim the highest-priority available backlog task and move it in progress
cargo run -- --kanban-dir=kanban --claim=dock-worker
```

### Inspect a real worktree and render a contextual diff

```bash
cargo run -- --git-dir=. --base=HEAD~1
```

The adapter resolves Git facts through argument-safe subprocess calls and prefers `delta` as a renderer. It falls back to raw Git diff output if delta is unavailable or fails. Dock still never stages, commits, rebases, merges, or pushes.

### Save a local-only explicit handoff

```bash
cargo run -- --save-handoff=fixtures/demo-handoff.json
cargo run -- --load-handoff=fixture_DOCK7
```

By default, Dock writes to `.dock/local/handoffs/`, which is Git-ignored. Each save is atomic, each load validates the strict packet schema, and run IDs cannot escape the storage directory.

## Planned adapters

- kanban-md CLI adapter: atomic claim/status transition
- Herdr adapter: deterministic managed-pane binding through supported CLI/socket surface
- Git adapter: worktree, branch, base/head and changed-file facts
- delta adapter: external renderer with basic fallback
- LazyGit launcher: opens at the exact bound worktree; Dock does not control it
