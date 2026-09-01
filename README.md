# Dock

A terminal multiplexer for coding agents. Real PTY panes, `Ctrl+B` prefix, live agent state. Unix only. Needs `git` for worktrees; nothing else.

## Run

```bash
cargo install --path . --locked   # crate dock-tui, binary dock
dock
```

Same thing: `./scripts/install.sh`. Run from any directory. Dock starts a per-directory daemon or attaches to one. Each new pane is `$SHELL`. Type `claude`, `codex`, or `amp` as usual.

Protocol is v15 — kill an older `dockd` before connecting.

## Keys

Prefix is `Ctrl+B`. Unprefixed keys go to the pane. `Esc` is never intercepted. `Ctrl+B` `Ctrl+B` sends a literal `Ctrl+B`. `Ctrl+B ?` is help.

| Key | | Key | |
|---|---|---|---|
| `n` | new workspace | `z` | zoom |
| `,` `.` | prev / next workspace | `r` | rename |
| `w` | pick workspace | `f` | type a file path into the pane |
| `a` | resume agent in this pane | `i` | review handoffs |
| `k` | task board | `g` / `G` | git overlay / lazygit if on PATH |
| `1`–`9` | jump workspace | `l` | launch an agent |
| `h` `v` | split | `R` | restart exited pane |
| arrows, Tab | focus | `x` | close pane |
| `+` `-` | resize | `[` | copy mode |
| `d` | detach (daemon keeps running) | `q` | prompt queue |

Mouse: tabs, split/close on the pane border, drag dividers, drag to select.

## Roster

Dock only tracks PTYs it launched.

| | State |
|---|---|
| `◆` | needs you |
| `◉` | working |
| `◐` | done (your turn) |
| `○` | idle |

Screen scrape is the fallback. For Claude Code, install hooks so state is reported:

```bash
dock hooks              # print config
dock hooks --install    # merge into .claude/settings.json
```

`Ctrl+B a` resumes the last agent in this pane (`claude --continue`, `codex resume --last`, `amp threads continue --last`). Unknown resume flags are refused, not guessed.

## Board

`Ctrl+B k` is markdown tasks in the repo (`kanban/tasks/`). Watch updates the TUI when files change. `Enter` dispatches (claims `in-progress`, `git worktree add` on `dock/task-<id>` in a repo). Dock never closes a card from a screen heuristic.

```bash
dock task list
dock task add "fix the retry path"
dock task claim 3
dock task move 3 review
dock task show 3
```

Panes get `DOCK_BOARD`, `DOCK_WORKSPACE`, `DOCK_PANE`, `DOCK_RUN`, and `DOCK_TASK` when dispatched. `PATH` includes Dock, so `dock` works inside a pane.

## Queue and handoff

`Ctrl+B q` is the prompt queue. `dock queue add "$DOCK_PANE" "continue"` from a shell. Auto-feed does not trust screen-inferred done unless you set trust explicitly.

```bash
dock handoff "added retry with backoff" --check="cargo test:pass"
```

`Ctrl+B i` reviews claimed vs observed (branch, files, diffstat). `a` accept / `c` changes — recorded only. Dock does not merge.

## Agents driving Dock

Inside a pane:

```bash
dock split vertical
dock prompt "continue"
dock read
dock wait --until=blocked
dock workspace inspect
```

SSH: no `--remote`. Forward the unix socket:

```bash
ssh -L /tmp/dock.sock:"$DOCK_SOCKET" user@host
dock --socket=/tmp/dock.sock inspect
```

## Copy

`Ctrl+B [` copy mode (`v` select, `y` yank, `/` search). Clipboard via OSC 52 (`DOCK_CLIPBOARD=helper` uses `pbcopy`/`wl-copy`/`xclip`).

## Safety

- Only PTYs Dock created.
- Only git write: `git worktree add` on dispatch. No stage, commit, rebase, merge, push, or worktree remove.
- Review does not merge or complete a task. `dock task` rewrites `status:` (and claim) only.
- Layout is in `.dock/local`. A daemon restart restores layout with fresh shells, not old processes.

MIT
