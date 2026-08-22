<div align="center">

# d·ock

**A terminal multiplexer that understands coding agents.**

Real PTY panes · `Ctrl+B` prefix · live agent state · local-first

</div>

---

```
╭─ ● claude · dock ──────────────╮╭─ ○ zsh · ~/dev/dock ──────────╮
│ › Reading src/runtime.rs       ││ ❯ cargo test                  │
│   ▸ 3 files changed            ││    Running 244 tests          │
│                                ││                               │
╰────────────────────────────────╯╰───────────────────────────────╯
 AGENTS ─────────────────────────  Ctrl+B ? help
 ● claude   dock          2m14s
 ● codex    api-svc      18m02s
 ○ amp      idle
```

Every pane is a real terminal with full VT emulation, so `vim`, agent CLIs, and
anything else using the alternate screen work correctly. Dock watches its own
panes and tells you which agent needs you — without ever touching a process it
did not launch.

## Quick start

```bash
cargo run --bin dock
```

That's it. Run it from any directory, Git or not. Dock connects to that
directory's private daemon, or starts one for you.

Every pane launches `$SHELL` the moment it exists, so you can type immediately.
Start an agent the way you always do — `claude`, `codex`, `amp` — and Dock picks
it up.

## Keys

`Ctrl+B` is the prefix (same as tmux). **Unprefixed keys go straight to the
pane**, `Esc` is never intercepted, and `Ctrl+B` twice sends a literal `Ctrl+B`.

| After `Ctrl+B` | |  | |
|---|---|---|---|
| `n` | new workspace | `z` | zoom pane |
| `,` `.` | previous / next workspace | `r` | rename |
| `w` | pick a workspace by name | `f` | find a file, type its path |
| `a` | resume the agent here | `i` | review agent handoffs |
| `k` | task board — dispatch one | `g` | what changed here |
| `1`–`9` | jump to a workspace | | |
| `h` `v` | split ⇋ / ⇵ | `R` | restart a pane whose shell exited |
| arrows, `Tab`/`S-Tab` | focus | `x` | close pane |
| `+` `-` | resize split | `l` | launch an agent |
| `[` | copy mode | `d` | leave — runs keep running |
| `?` | help | `q` | quit |

With more than one workspace open, a tab strip names them all and numbers them
by the digit that jumps there. Most of Dock is reachable with the mouse: click
a tab to switch, `✎` on the active tab to rename the workspace, `+` to add one,
and `⇋ ⇵ ×` on the focused pane's lower border to split or close it. Dragging a
divider resizes; dragging inside a pane selects. Every one of these mirrors a
published key rather than being reachable only by pointer.

`Ctrl+B f` lists the files where the focused pane actually is — following the
shell's `cd`, and honouring `.gitignore` inside a repository. Taking one types
its path into the pane rather than opening it, so `vim ` first opens the file
and reaching for it mid-sentence hands an agent the path.

`Ctrl+B a` relaunches the agent that last ran in the focused pane, asking it to
continue its most recent session — `claude --continue`, `codex resume --last`,
`amp threads continue --last`. Dock never adopts a process it did not start, so
what survives is the agent's own transcript, not its pid: resume works after the
pane's process died, after the daemon restarted, and after a reboot. Two panes
running the same agent in the same directory share a "most recent", so resuming
one can land on the other's session. An agent whose resume flag Dock has not
verified reports that it cannot be resumed rather than silently starting fresh.

`Ctrl+B k` opens the task board, read straight from Markdown front matter — no
`kanban-md` binary needed to see it. **Every workspace has one.** In a
repository it's that repository's `kanban/tasks/`, shared by every workspace
open on it; otherwise it's the workspace's own board under
`~/.dock/boards/<workspace>/tasks`, created the first time you add something.

An empty board opens like any other and invites the first task: type a title
and press `Enter` (or `Ctrl+N`). Typing filters what's there; if nothing
matches, `Enter` writes down what you typed. The board names its own directory
along the bottom, so a workspace board and a repository board are never
confused.

Choosing a task puts an agent on it. In a repository it gets a worktree of its
own on a `dock/task-<id>` branch, and dispatching the same task again lands in
the worktree the first dispatch made. Outside one there is nothing to isolate
from, so the agent launches where you are with the task as its opening prompt.
See **Safety** for exactly what that touches.

`Ctrl+B g` shows what changed in the focused pane's worktree — branch, counts,
and the diff, coloured with Dock's own palette. No `delta` or `lazygit` needed;
`j`/`k` scroll and `g`/`G` jump to either end.

`Ctrl+B i` opens the review queue: the handoffs agents submitted with
`dock-handoff --submit` and are waiting on a person for. Each shows what the
agent claimed beside what Dock measured — changed files, insertions, deletions,
branch — so a claim and the evidence for it are read together. `a` accepts the
scope, `c` requests changes, and either needs a note saying why. **A decision is
recorded, never merged**: Dock does not touch Git, and does not close the task.

Pasting is bracketed — a multi-line paste arrives as one payload instead of
executing line by line.

## Copy and scrollback

`Ctrl+B [` freezes the focused pane into copy mode, signalled by `COPY` in
the pane's border and a footer that switches to copy-mode bindings. `hjkl`
or the arrow keys move the cursor, `g`/`G` jump to the top/bottom of the
**visible screen**, `v` starts a selection, and `y` yanks it — a bare `y`
with no selection yanks the cursor's line instead, trimmed of trailing
whitespace. `/` opens a search prompt, `n`/`N` cycle matches. `Esc` unwinds
one level at a time (the search prompt first, then the mode itself); `q`
always leaves immediately.

Dragging with the mouse inside a pane enters copy mode and extends a
selection the same way; a plain click still just focuses the pane. Releasing
a drag finalises the selection but never copies anything by itself — yanking
is always an explicit `y`.

A yank tries OSC 52 first (works over SSH, since it asks the *host* terminal
to set its clipboard) and falls back to `pbcopy`/`wl-copy`/`xclip` if that
fails. The notice after a yank names which route was used, because OSC 52 is
disabled by default in some terminals and a silent no-op would look
identical to a working copy.

The mouse wheel scrolls a pane's history, and scrolling back to the bottom
resumes following live output. The daemon streams each pane's raw output to
the client, so the client's own terminal scrolls exactly as the daemon's does
and retains the same number of rows (`dockd --scrollback-rows`, default
2000). In copy mode, `k`/`↑` past the top row walks into that history a row
at a time; `g`/`G` jump to the top and bottom of the current viewport, and
`/` search covers the rows on screen, so scrolling back first widens what it
searches. History is per-connection: a pane accumulates it from the moment
this client attached, the way it does in tmux, and it is not restored across
a daemon restart.

## Agent awareness

Dock classifies each pane from what it renders and sorts the sidebar
**blocked-first**, so whatever is costing you time surfaces at the top.

| | State | Meaning |
|---|---|---|
| ● | **blocked** | waiting on you |
| ● | **working** | actively producing output |
| ◍ | **done** | finished |
| ○ | **idle** | nothing happening |

This is a heuristic, not a protocol the agent speaks. Agents running *outside*
Dock appear as `external/read-only` and are never adopted or controlled.

## Scripting

Dock's runtime is scriptable without the dashboard. Prove it with the `fixture`
adapter first:

```bash
cargo run --bin dock-dispatch -- \
  --repo="$(pwd)" --task=TRY-1 --run-id=dock_try_1 \
  --worktree="$(pwd)" --adapter=fixture -- -c 'sleep 30'
cargo run --bin dock-inspect  -- --run-id=dock_try_1
cargo run --bin dock-agent    -- --run-id=dock_try_1 --operation=stop
```

Swap `fixture` for `amp`, `claude-code`, `codex-cli`, or `github-copilot-cli`.
Dock checks the binary exists before creating a run; agent authentication stays
with that agent's own setup.

`dock-workspace` manages panes non-interactively, and `dock-programme` inspects
multi-repository capacity and dependency gates.

## Safety

- Dock **only** controls PTYs and process groups it created. There is no
  adoption path, and stale process groups are never signalled.
- Dock touches your repository in exactly **one** way: `Ctrl+B k` dispatching a
  task runs `git worktree add`, creating a branch (`dock/task-<id>`) when that
  branch does not already exist. It goes beside your repository, never inside
  it. A path that is already occupied is refused rather than reused, and
  dispatching the same task twice lands in the worktree the first one made.
- Beyond that it **never** stages, commits, rebases, merges, pushes, deploys,
  rewrites history, deletes branches, or removes worktrees — including the ones
  it created. Cleaning up is yours, because Dock cannot know what is still
  wanted.
- It never completes or closes a task. A review decision is recorded and
  carries `external_task_completed: false` and `git_mutated: false`; accepting
  scope is a note, not a merge. Claiming a task through `kanban-md` moves it to
  in-progress and is the only status Dock will ever set.
- Dock writes task files **only under `~/.dock/boards/`**, its own boards. A
  repository's board belongs to `kanban-md` and to whoever commits to it, so
  adding is refused there and says why. Reading any board is always safe: it
  parses the Markdown front matter directly and never runs `kanban-md` to look.
  Dock never deletes a board or a task.
- Durable state lives in `.dock/local` at `0700`/`0600`. Layout records hold
  topology and labels only — never terminal output, commands, credentials, PIDs,
  or process-group IDs. Use `--state-dir=` to relocate it.
- A daemon restart recovers layout only. Old processes are never reattached;
  restored panes get a fresh shell.

## Status

Shipped: VT emulation, PTY resize, shell panes, push-based streaming, agent
state detection, zoom, bracketed paste, `Ctrl+C` signal delivery, wheel
scrollback, and copy mode (keyboard and mouse-drag selection, search, OSC 52
clipboard).

Deferred: pane swap, alternative theme palettes, notifications, and durable
transcript replay. Scrollback is bounded per pane (default 2000 rows,
`dockd --scrollback-rows`) and is not restored across a daemon restart.

The [parity matrix](docs/terminal-runtime-parity.md) states the exact status of
every capability, including known limitations.

## Development

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings

scripts/smoke-slice5-macos.sh
scripts/smoke-slice6-macos.sh
scripts/smoke-slice61-macos.sh
scripts/smoke-slice62-nongit-macos.sh
```

Dock speaks protocol v9 — stop any older daemon before connecting.

CI runs the same three gates plus a build on Linux and macOS. Many tests drive
real PTYs, subprocesses, and signals, so they wait on a wall clock; those
deadlines exist so a regression fails with a message instead of hanging, not to
assert how fast Dock is. `DOCK_TEST_TIMEOUT_SCALE` multiplies every one of them
when a machine needs more patience than a developer laptop — CI uses `4`:

```bash
DOCK_TEST_TIMEOUT_SCALE=4 cargo test --all-targets
```

The smoke scripts are macOS-only and are not part of CI.

## Licence

MIT
