# Dock

A terminal multiplexer for coding agents. Real PTY panes, `Ctrl+B` prefix, live agent state. Unix only. Needs `git` for worktrees; nothing else.

## Run

```bash
cargo install --path . --locked   # crate dock-tui, binary dock
dock
```

Same thing: `./scripts/install.sh`. Run from any directory. Dock starts a per-directory daemon or attaches to one. Each new pane is `$SHELL`. Type `claude`, `codex`, or `amp` as usual.

Protocol is v17 — kill an older `dockd` before connecting.

## Keys

Prefix is `Ctrl+B`. Unprefixed keys go to the pane. `Esc` is never intercepted. `Ctrl+B` `Ctrl+B` sends a literal `Ctrl+B`. `Ctrl+B ?` is help.

| Key | | Key | |
|---|---|---|---|
| `n` | new workspace | `z` | zoom |
| `,` `.` | prev / next workspace | `r` | rename |
| `w` | pick workspace | `f` | type a file path into the pane |
| `a` | resume agent in this pane | `i` | review handoffs |
| `k` | task board overlay | `B` | split a board pane |
| `s` | sidebar | `X` | close workspace |
| `1`–`9` | jump workspace | `l` | launch an agent |
| `h` `v` | split | `R` | restart exited pane |
| arrows, Tab | focus | `x` | close pane |
| `+` `-` | resize | `[` | copy mode |
| `d` | detach (daemon keeps running) | `q` | prompt queue |
| `g` / `G` | git review overlay / lazygit if on PATH | `u` | cycle agents that need you |

Mouse: tabs, sidebar agent rows (focus that pane), split/close on the pane border, drag dividers, drag to select. A blocked pane marks the left edge (and the sidebar ◆); not a full-perimeter red frame. The sidebar quotes the last hook reason next to ◆ when it has one.

## Roster

Dock only tracks PTYs it launched. Detected by foreground binary: pi, omp, copilot, devin, kimi, hermes, qoder, qwen, droid, opencode, kilo, mastra, claude, codex, cursor-agent, amp, grok, antigravity, kiro, maki (plus gemini/aider). Launch those names if they are on PATH. Resume only: `claude --continue`, `codex resume --last`, `amp threads continue --last`. Anything else: Ctrl+B a refuses with a reason, same as Copilot.

| | State |
|---|---|
| `◆` | needs you |
| `◐` | working |
| `◉` | done (your turn) |
| `○` | idle |

Screen scrape is the fallback. `dock hooks --install` merges Claude Code into `.claude/settings.json` and Codex command hooks into `.codex/hooks.json` (and `$CODEX_HOME/hooks.json`). Amp has no published command-hook stdin schema (Plugin API / `--stream-json` is not an interactive-pane hook). GitHub Copilot CLI has no verified hook schema.

```bash
dock hooks              # print config
dock hooks --install    # Claude Code + Codex: merge, keep existing handlers
```

`Ctrl+B a` resumes only when Dock has read that CLI's flags: `claude --continue`, `codex resume --last`, `amp threads continue --last`. GitHub Copilot CLI is refused — those flags were never read, so Dock does not guess them.

## Board

`Ctrl+B k` is markdown tasks in the repo (`kanban/tasks/`). Same drive keys on the overlay and a `@board` pane: hjkl move, H/L set status (WIP), c claim, a archive, A show archived and done, n new, Enter dispatch (body, not title-only). `o` focuses the card's live pane, or resumes/launches in its worktree. Dock never closes a card from a screen heuristic. The board shows markdown cards only (no agent/mux rows); `done` is hidden until A.

```bash
dock task list
dock task add "fix the retry path"
dock task claim 3
dock task move 3 review
dock task show 3
dock task next
```

Panes get `DOCK_BOARD`, `DOCK_WORKSPACE`, `DOCK_PANE`, `DOCK_RUN`, and `DOCK_TASK` when dispatched. `PATH` includes Dock, so `dock` works inside a pane.

## Queue and handoff

`Ctrl+B q` is the prompt queue. `dock queue add "$DOCK_PANE" "continue"` from a shell. Auto-feed does not trust screen-inferred done unless you set trust explicitly.

```bash
dock handoff "added retry with backoff" --check=test
```

`Ctrl+B i` reviews claimed vs observed (branch, files, diffstat). `a` accept / `c` changes — recorded only. Dock does not merge.

## Receipts

`dock handoff` writes a receipt: four parts, each authored by exactly one party, plus a
verdict derived from all four. `claimed` is the agent's summary and the names of checks it
asks Dock to run. `observed` is git facts and hook payloads Dock collected itself.
`witnessed` is what Dock's own check runs found. `decided` is the human's accept or
changes-requested. An agent can never write to `witnessed`. Dock can never write to
`decided`. That is the whole trust model.

Checks are declared by the repository, never composed by the agent:

```toml
# .dock/checks.toml — committed to the repository
[check.test]
run     = ["cargo", "test", "--locked"]
timeout = "10m"

[check.lint]
run     = ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
timeout = "5m"
```

An agent names a check (`--check=test`); a name this file does not hold comes back
`unwitnessed`, never a command an agent composed. A repository that declares no checks is
never shown a `✓` — `no_checks_declared` fires, and a run that fires it can never be
`clear`. The way to earn fast approvals is to write this file.

`dock verdict explain <run-id>` prints every rule and the fact it read, so the verdict a
receipt carries can be re-derived by hand.

Dock resolves a declared check by name against `.dock/checks.toml` in the repository root,
never against the copy in the run's own worktree — an agent can write any file in the
worktree it was given, that file included. Dock does not confine an agent's process to its
worktree, so this closes only the path that needed no walking out: **Dock never reads a
declaration from the path it handed the agent.**

That is a check's declaration, not its execution: a check runs the worktree's code; that is
what a check is. `run = ["./scripts/ci.sh"]` resolves relative to the worktree, and
`run = ["cargo", "test"]` compiles and runs whatever the agent just wrote, by design. The
same limit reaches the `~/.config/dock/checks.toml` secrets gate — a permitted variable is
handed to that same worktree's code, so the gate is only as strong as the confinement
conceded above, not a boundary against a hostile agent.

## Agents driving Dock

Inside a pane:

```bash
dock split vertical
dock prompt "continue"
dock read
dock wait --until=blocked
dock workspace inspect
```

`dock inspect` / `dock agent` talk to the daemon about a run. `dock --help` lists every verb; `dock programme` is extra multi-repo work, not 0.1.

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
- Layout is in `.dock/local`. A daemon restart restores layout: agent panes Dock launched resume with that CLI's documented flags (`claude --continue`, `codex resume --last`, `amp threads continue --last`). Plain shells stay shells. Copilot has no resume flags, so it is launched again without invented ones. PTY contents are not saved.

MIT
