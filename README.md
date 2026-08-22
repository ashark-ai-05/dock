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
| `,` `.` | switch workspace | `r` | rename |
| `h` `v` | split ⇋ / ⇵ | `R` | restart a pane whose shell exited |
| arrows, `Tab`/`S-Tab` | focus | `x` | close pane |
| `+` `-` | resize split | `l` | launch an agent |
| `[` | copy mode | `d` | leave — runs keep running |
| `?` | help | `q` | quit |

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
- It never stages, commits, rebases, merges, pushes, deploys, creates worktrees,
  or mutates your task system.
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

Dock speaks protocol v8 — stop any older daemon before connecting.

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
