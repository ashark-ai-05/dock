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

It's drawn as a board — a column per status, cards inside them:

```
╭ BOARD ──────────────────────────────────────────────────────────────────╮
│BACKLOG · 2            TODO · 0        IN-PROGRESS · 1     REVIEW · 1     │
│─────────────────────  ──────────────  ──────────────────  ───────────────│
│› #3 write the docs      —               #1 wire the parse   #2 add tests │
│  #5 fix the retry pa…                                                    │
│                                                                          │
│←/→ column · ↑/↓ card · </> move it · n new · Enter dispatch · Esc close   │
│~/.dock/boards/workspace_1/tasks                                          │
╰──────────────────────────────────────────────────────────────────────────╯
```

`←`/`→` picks a column, `↑`/`↓` a card, `<`/`>` moves the card itself between
columns, `n` starts a new task, and `Enter` puts an agent on the selected one.
The footer names the agent `Enter` will use — the last one you launched from
`Ctrl+B l`, or otherwise the first installed agent that can be handed the task.
The board names its own directory along the bottom, so a workspace board and a
repository board are never confused — and a repository's board is shown but
never altered, since `kanban-md` owns it.

Agents track their own work with `dock task`. Every pane Dock launches gets
`DOCK_BOARD` (its board), `DOCK_WORKSPACE`, `DOCK_PANE`, `DOCK_RUN`, and
`DOCK_TASK` when it was dispatched onto one — and Dock's own directory leads
`PATH`, so `dock` resolves inside a pane even when Dock was started from a
checkout with `cargo run`. An agent needs no arguments:

```bash
dock task list                       # what is on this board
dock task add "fix the retry path"   # write one down
dock task move 3 in-progress         # backlog · todo · in-progress · review · done
dock task show 3

dock handoff "added a retry with backoff" --check="cargo test:pass"
```

`dock handoff` puts a result in front of you for review. The agent supplies a
sentence; Dock measures the evidence itself — branch, changed files,
insertions, deletions — so what was claimed sits beside what was observed.
`Ctrl+B i` shows those, undecided first, with the decision each one received.

The same commands work from any shell with `--board=<dir>`, so a board with no
agent on it is entirely yours to run by hand. Tasks are Markdown with YAML
front matter and `move` rewrites only the `status:` line, leaving everything
else byte for byte — so `kanban-md`, your editor, and Dock all read and write
the same files without fighting.

Choosing a task puts an agent on it, and claims it: the task moves to
`in-progress` on Dock's own boards before the agent starts. The task is handed
to the agent as its opening instruction where that agent documents one —
`claude [prompt]` and `codex [PROMPT]` do, `amp` does not, so it opens in the
right place and you type the task yourself.

The sidebar names the task each agent is on, and so does the pane's title, so
three agents of the same kind are told apart by their work rather than by
guessing. An agent you start yourself by typing `claude` into a pane is **not**
linked to any task — Dock cannot know what it is for. Dispatching from the
board is what creates the link.

A dispatched agent is also told how to close the loop: its prompt ends with
`dock task move <id> review`, so the board follows the work. Dock never moves a
task because an agent *looks* finished — "looks finished" is a regex over a
screen, and the board is the durable record of what happened.

### Exact state, from the agent itself

Screen-reading is the fallback. Where an agent has its own event system, Dock
uses that instead — it knows, where a pattern can only infer:

```bash
dock hooks              # print the Claude Code hook config
dock hooks --install    # merge it into .claude/settings.json
```

That wires `UserPromptSubmit` → working, `Stop` → your turn,
`PermissionRequest`/`Notification` → needs you, `SessionEnd` → idle. A reported
state is sticky: it holds until the agent reports something else, because
"finished" stays true until the next turn begins. Merging is per event and
repeatable — your own hooks on those events are left alone.

Dock recognises **20 agents** by binary name: claude, codex, amp, copilot,
cursor, droid, kimi, opencode, hermes, pi, gemini, qwen, kiro, antigravity,
vibe, omp, aider, devin, kilo and qoder. Recognition is what puts an agent in
the roster; the rules that give it a *state* are per-agent and yours to edit.

### When the state is wrong, fix it yourself

Detection rules are files, not code. Ask what is in force and why:

```bash
dock detect claude                        # the rules, and where they came from
dock detect claude --explain < screen.txt # which rule matched, and the verdict
```

Override any of them at `~/.config/dock/agent-detection/<agent>.json`. A file
replaces only the states it names, so narrowing one does not mean restating the
rest:

```json
{ "schema": 1, "blocked": ["(?i)approve this change\\?"] }
```

An unknown key is refused and names the valid ones, because a typo in a rules
file is exactly where silence costs most — you are already looking at an answer
you are trying to change.

Agent states are distinct on purpose: **needs you** means the agent asked
something and cannot continue (a permission prompt, a chooser); **your turn**
means it finished and will wait indefinitely. Reporting the second as the first
makes every idle agent shout for attention until nothing in the roster means
anything. In a repository it gets a worktree of its
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

The freeze is real: entering copy mode takes a copy of the pane's screen,
and the pane is painted from that copy — and the selection read from it —
until the mode ends, so a pane that is still producing output cannot scroll
the highlighted text out from under you. The scrollback comes with the copy,
so the wheel and `k` past the top row still walk history from inside the
mode. Nothing is held back while you select: the pane's own emulator keeps
consuming every byte behind the freeze, so leaving copy mode drops the copy
and the pane is already caught up, with nothing to replay and nothing to
resynchronise. A pane that is *resized* while frozen leaves copy mode with a
notice, because a selection is a set of coordinates on one particular grid
and that grid has just been replaced.

Dragging with the mouse inside a pane enters copy mode and extends a
selection the same way; a plain click still just focuses the pane. Releasing
a drag **copies the selection**, which is what iTerm2, Ghostty, WezTerm and
GNOME Terminal all do — a selection that then needs a second keystroke reads
as a selection that did not work. The highlight stays up after the release,
and `y` still works. A gesture that selected nothing copies nothing, so a
plain click can never overwrite what is already on the clipboard.

Double-click selects the word under the pointer and triple-click selects the
line, trimmed of trailing padding. "Word" is spelled for terminal content
rather than prose: `/`, `.`, `-`, `_`, `~`, `+`, `@` and `:` all bind, so
`src/main.rs:12`, `user@host` and `localhost:8080` select whole.

Middle-click and right-click paste the last thing Dock copied into the
focused pane, through the same bracketed-paste encoder the host's own paste
uses. It is the last thing *this dashboard* copied rather than whatever the
OS clipboard holds: reading the OS clipboard means running `pbpaste`, and
spawning a process on the render thread to answer a click is exactly the
stall this avoids. It is also what a middle click means on X11, where PRIMARY
is per-application and holds the last selection.

A copy is handed to the host terminal as OSC 52, which works over SSH since
it asks the *host* terminal to set its clipboard. OSC 52 is one-way: the
terminal never acknowledges it, Terminal.app disables it outright, iTerm2
disables it by default and tmux ignores it without `set -g set-clipboard on`.
Dock therefore says what it *asked for* rather than claiming a clipboard it
cannot check. Set `DOCK_CLIPBOARD` to choose the route:

| `DOCK_CLIPBOARD` | Route |
| --- | --- |
| unset, `auto`, `osc52` | OSC 52 only (default; the only route that works over SSH) |
| `helper` | `pbcopy`/`wl-copy`/`xclip` only, for a terminal that refuses OSC 52 |
| `both` | Both, when a terminal's OSC 52 support is unknown |
| `off` | Neither |

The helper is spawned and reaped off the render thread, so a copy never
blocks a frame on a subprocess, and an unrecognised `DOCK_CLIPBOARD` value is
refused rather than silently leaving you on the route you were trying to
change.

The mouse wheel scrolls a pane's history, and scrolling back to the bottom
resumes following live output. The daemon streams each pane's raw output to
the client, so the client's own terminal scrolls exactly as the daemon's does
and retains the same number of rows (`dockd --scrollback-rows`, default
2000). In copy mode, `k`/`↑` past the top row walks into that history a row
at a time — through the frozen copy, so live output cannot move it —
`g`/`G` jump to the top and bottom of the current viewport, and `/` search
covers the rows on screen, so scrolling back first widens what it searches. History is per-connection: a pane accumulates it from the moment
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
cargo run --bin dock -- inspect --run-id=dock_try_1
cargo run --bin dock -- agent --run-id=dock_try_1 --operation=stop
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
- The dashboard writes task files **only under `~/.dock/boards/`**, its own
  boards: it adds a task there, and claims one to `in-progress` when it puts an
  agent on it. A repository's board belongs to `kanban-md` and to whoever
  commits to it, so the dashboard never writes there. The `dock task` command
  writes wherever it is explicitly pointed — that is how an agent records its
  own progress — and rewrites only the `status:` line, never reformatting the
  rest of a file it shares with other tools. Reading any board is always safe
  and never runs `kanban-md`. Dock never deletes a board or a task.
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

Dock speaks protocol v10 — stop any older daemon before connecting.

CI runs the same three gates plus a build on Linux and macOS. Many tests drive
real PTYs, subprocesses, and signals, so they wait on a wall clock; those
deadlines exist so a regression fails with a message instead of hanging, not to
assert how fast Dock is. `DOCK_TEST_TIMEOUT_SCALE` multiplies every one of them
when a machine needs more patience than a developer laptop — CI uses `4`:

```bash
DOCK_TEST_TIMEOUT_SCALE=4 cargo test --all-targets
```

The smoke scripts are macOS-only and now run in CI. They drive a real daemon
over a real socket with a second client attaching to it — reconnect, layout
persistence, signal delivery — none of which exists inside the unit suite. CI
also fails if any run leaves a daemon behind.

## Licence

MIT
