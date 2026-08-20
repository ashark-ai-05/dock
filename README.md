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

Slice 5 adds a local programme-control layer to the Slice 4 repository-bound runtime:

- `dockd` accepts bounded global and per-repository run capacity plus a human-review reserve; capacity refusal occurs before a receipt, pane, PTY, or process group exists;
- `dock-programme` explicitly queues a downstream fixture dispatch against one exact upstream run id and required human route; Dock never discovers dependencies from tasks or terminal text;
- explicit release requires that upstream run's strict valid handoff and matching human decision; blocked or capacity-refused release leaves the gate queued and starts nothing;
- portfolio inspection reports opaque repository identities, sorted active/queued run ids, capacity, and deterministic gate state/reason without exposing repository/worktree paths;
- queued dependency gates are atomically persisted in private local state and revalidated on daemon startup; private records omit raw output, commands, and absolute repository/worktree paths;
- protocol version 5 keeps the forward-compatible hello and strict programme requests.

Slice 4 continues to provide provider-neutral agent launch and safe lifecycle control:

- `dockd` starts empty and accepts adapter-backed dispatches through protocol version 5;
- explicit profiles discover `amp`, `claude`, `codex`, and `copilot`, plus deterministic `fixture` and explicit-executable `generic` adapters;
- discovery and capability preflight happen before receipt, run, workspace, pane, PTY, or process-group creation, so a missing binary leaves no partial dispatch;
- `dock-agent` provides attach/focus acknowledgements and interrupt/stop/restart controls for active Dock-owned runs; signals can target only the process-group capability minted at launch;
- snapshots report Dock-owned process controls separately from explicit provider-adapter capabilities, adapter identity, observable process state, and a separate provider state; no current adapter claims unverified provider-native features, and generic/fixture provider state remains `unknown` rather than inferred from terminal output;
- each run is bound before launch to a canonical Git repository root, non-empty external task reference, Dock run id, and supplied existing worktree path;
- worktree paths are canonicalized, kept beneath the repository root, checked against Git's top-level, and rejected on traversal, symlink escape, or repository mismatch;
- each accepted run receives its own Dock-owned pane, PTY, and process group;
- a versioned, user-only Unix socket accepts a forward-compatible hello and strict inspect requests;
- `dock-dispatch` is the compact client/smoke path and `dock-inspect` reports one or all bound runs, including Git branch/base facts;
- the daemon and fixture process continue running when inspectors disconnect and reconnect;
- owner-only durable prelaunch reservations make every run id fail-closed before spawn; committed receipts record initial launch evidence, while an interrupted launch remains permanently non-retryable and its private guardian terminates the exact Dock-created process group when daemon ownership is lost;
- malformed/mismatched requests, missing tasks, duplicate ids, and invalid bindings are rejected without process discovery, process import, Git mutation, or worktree creation.

The earlier foundations remain available:

- Ratatui fixture Control-Pane prototype;
- strict, versioned handoff packets and atomic local-only packet storage;
- `kanban-md` task intake and atomic claim adapter;
- Git worktree/base/head/numstat facts and `delta` diff rendering;
- explicit LazyGit launch intent.

Live process handles and scrollback remain in memory and disappear when `dockd` exits. Dispatch binding receipts and queued dependency gates persist beneath the configured state directory, but daemon restart recovery does not reattach or import former processes. A launch interrupted after spawn but before receipt commit is not recoverable or retryable: its guardian terminates the created group and the durable reservation keeps the run id sealed. Stored gates are re-canonicalized and rejected at startup if their Git binding or opaque repository identity no longer matches. Slice 5 does **not** handle agent credentials, scrape terminal output for semantics, adopt arbitrary processes, create or mutate Git worktrees, mutate task systems, infer/release dependencies automatically, accept terminal input, or provide terminal-multiplexer parity. Child processes receive only `HOME`, `LANG`, `LC_*`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, `TMPDIR`, `USER`, and Dock's worktree marker; ambient API keys, tokens, and credential sockets are not inherited. Agent CLIs use their existing file-based authenticated setup. The supplied worktree must already exist at a Git worktree top-level beneath (or equal to) the canonical repository root and share that repository's Git common directory.

## Slice 5 programme control

```bash
cargo run --bin dockd -- --global-run-capacity=3 \
  --repository-run-capacity=1 --human-review-reserved=1
cargo run --bin dock-programme                    # portfolio inspection
cargo run --bin dock-programme -- --upstream-run-id=dock_upstream \
  --required-route=accept-scope --repo=/canonical/repo-b --task=B-1 \
  --run-id=dock_downstream --worktree=/canonical/repo-b
cargo run --bin dock-programme -- --release=dock_downstream
```

The agent run limit is the global capacity minus the human-review reserve. `--global-run-capacity` and `--repository-run-capacity` must be positive; the reserve must be smaller than global capacity. Capacity refusal happens before durable receipt or process creation and leaves a ready gate queued for retry.

The programme API adds strict `queue_gated`, `release_gate`, and `inspect_programme` requests in protocol v5. Queueing requires an active exact upstream run, a distinct unused downstream run id, a canonical Git binding, and an argument-free built-in adapter. The current CLI deliberately selects the fixture adapter. Generic executables and adapter arguments are rejected for durable gates because Dock does not persist raw commands or executable paths. After restart a gate remains inspectable and releasable when its persisted handoff and matching human decision exist, although former processes are not recovered. Release is explicit, one-time, and duplicate-safe.

On macOS, `scripts/smoke-slice5-macos.sh` creates two clean temporary Git repositories and independently proves per-repository and global capacity refusal, queued handoff/decision blocking, one downstream release, portfolio state, and unchanged Git HEAD/status. It requires `jq` and cleans up its daemon and fixtures.

## Slice 4 agent adapters and lifecycle

Use `--adapter=amp|claude-code|codex-cli|github-copilot-cli` followed by `--` and provider arguments. Dock validates the profile binary before creating anything. For an unsupported command, use `--adapter=generic --executable=/absolute/or/PATH/name`; Dock reports only process facts and keeps provider state `unknown`. The fixture profile runs `sh` for deterministic tests.

```bash
cargo run --bin dock-dispatch -- --repo="$(pwd)" --task=FIXTURE-4 \
  --run-id=dock_slice4 --worktree="$(pwd)" --adapter=fixture -- -c 'sleep 30'
cargo run --bin dock-agent -- --run-id=dock_slice4 --operation=focus
cargo run --bin dock-agent -- --run-id=dock_slice4 --operation=interrupt
cargo run --bin dock-agent -- --run-id=dock_slice4 --operation=restart
cargo run --bin dock-agent -- --run-id=dock_slice4 --operation=stop
```

On macOS, `scripts/smoke-slice4-macos.sh` exercises missing-binary atomicity and the deterministic fixture lifecycle. A real-agent smoke intentionally requires the user to complete that CLI’s own authentication first; Dock neither reads nor stores credentials.

## Slice 3 handoff and review

Protocol version 3 adds strict handoff submission for an existing daemon-bound run, a pending-review inbox, and explicit `accept-scope` / `request-change` routes. The daemon reconciles the existing `HandoffPacket` schema with the active binding, captures current branch/base/head and concise numstat evidence from Git, and stores only structured local records. Decisions explicitly record that Git was not mutated and the external task was not completed.

On macOS, the compact dispatch → handoff → review → route smoke is:

```bash
scripts/smoke-slice3-macos.sh
```

It uses a short `/tmp` socket path for macOS, requires `jq`, cleans up its temporary daemon state, and does not commit, stage, or otherwise mutate Git. Sandboxed environments can select another short writable directory with `DOCK_SMOKE_PARENT=target`. For individual operations use `dock-handoff --submit=packet.json`, `dock-handoff --inbox`, or `dock-handoff --run-id=dock_ID --route=accept-scope|request-change --note=TEXT` with an optional `--socket=PATH`.

## Slice 2 fixture dispatch smoke

Start the empty daemon. By default clients use `dockd.sock` beneath an owner-only per-user runtime directory (`$XDG_RUNTIME_DIR/dock` on Linux when available, otherwise a per-user temporary directory on Linux/macOS):

```bash
cargo run --bin dockd -- --scrollback-bytes=65536
```

From another terminal, dispatch a fixture into this existing repository worktree. The client generates a `dock_…` run id unless `--run-id=dock_smoke` is supplied for a deterministic smoke:

```bash
cargo run --bin dock-dispatch -- \
  --repo="$(pwd)" --task=FIXTURE-1 --run-id=dock_smoke \
  --worktree="$(pwd)" --adapter=fixture -- -c 'pwd; git rev-parse --show-toplevel'
cargo run --bin dock-inspect -- --run-id=dock_smoke
```

Use a fresh run id for each accepted dispatch; ids remain reserved in `.dock/local/dispatches`. Dock requires its state and dispatch directories to be owned by the current user and inaccessible to group/other users (mode `0700` or stricter); receipt files are mode `0600`. Run `dock-inspect` without `--run-id` to list all runs. The explicit `--socket=PATH` option remains available on all three commands.

Stop the foreground daemon with `Ctrl-C`. No API accepts a PID, discovers an external process, or imports one. Existing Slice 1 socket recovery, finite client deadlines, bounded clients, bounded scrollback, child reaping, and Dock-owned process-group cleanup remain in force.

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
- Raw dispatch command vectors and absolute repository/worktree paths are never written to durable receipts because arguments can contain credentials and local paths are private context.
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
