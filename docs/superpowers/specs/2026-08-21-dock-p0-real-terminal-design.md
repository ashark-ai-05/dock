# Dock P0 — Real Terminal Foundation

Status: proposed
Date: 2026-08-21
Sub-project: P0 of 5 (see "Programme decomposition")

## Decision

Dock is **the review loop for parallel coding agents**, not a nicer multiplexer.

> Herdr shows you a red dot. Dock shows you the red dot, the diff that caused it,
> and one keystroke to accept it, reject it, or release the work it unblocks.

This positioning is load-bearing. Zellij is better than tmux on nearly every axis and
still has not displaced it, because "nicer multiplexer" is not a wedge against eighteen
years of muscle memory. Dock's wedge is the disposition engine it already has —
`HandoffPacket`, `ReviewDecision`, `HandoffEvidence`, dependency gates and capacity in
`src/model.rs` and `src/dispatch.rs`. No competitor in the agent-orchestration space has one.

P0 does not build the wedge. P0 builds the substrate the wedge is unusable without:
**a pane must be a real terminal.**

## Problem

Three defects make the current dashboard inert.

1. **No terminal emulation.** `src/dashboard.rs:296` formats `RuntimeSnapshot.scrollback`
   — a `String` — into a `Paragraph`. `src/runtime.rs:283` produces it with
   `String::from_utf8_lossy` over raw PTY bytes. Cursor motion, colour, and alternate-screen
   redraws render as literal escape-sequence text.
2. **Fixed 80x24 PTY.** `src/runtime.rs:498` calls `openpty(None, None)` and never issues
   `TIOCSWINSZ`. Codex CLI and Steer are themselves Ratatui applications; a full-screen TUI
   in a mis-sized PTY is unusable, not merely degraded.
3. **Poll-and-dump protocol.** `src/main.rs:459` polls `Inspect` every 200ms and the daemon
   answers with the entire scrollback of every run. Eight panes at 256KB is ~2MB of JSON
   five times a second regardless of whether anything changed.

Consequently panes never host anything, so `n`/`h`/`v`/`l`/`q` bare keys still work — which
in turn is why the product looks like a debug dump rather than a terminal.

## Scope

In scope for P0:

- VT emulation per Dock-owned run, with correct styled rendering.
- PTY sizing bound to pane geometry, with `SIGWINCH` to the owned process group.
- Push-based protocol v7 carrying screen deltas instead of polled full dumps.
- Direct keystroke path with no daemon round trip before local paint.
- `Ctrl+B` prefix keymap replacing bare single-letter commands.
- Shell-by-default: every created or split pane is immediately a working terminal.
- Basic agent detection and four-state model (heuristic tier only).
- Theme system and the "warm terminal-modern" default.
- OSC 2 / OSC 7 / OSC 133 capture (data captured and exposed; UI that consumes it is P4).

Explicit non-goals for P0:

- Native diff view, handoff surfacing, review decisions (P2).
- Hook-based agent integrations, session identity, notifications (P1).
- Kanban board rendering (P3).
- Command palette, copy mode, scrollback search, effects, config file (P4).
- Reimplementing lazygit. Dock hosts it in a pane; it does not rebuild it.
- Windows support.

## Verified technical findings

These were confirmed by compiling and running a probe, not read from documentation.

| Claim | Evidence |
|---|---|
| `tui-term` 0.3.4 is compatible with `ratatui` 0.30 | Probe rendered `fg=Indexed(2) mods=BOLD` into a `Buffer` |
| `vt100` emits minimal screen deltas | `contents_diff` produced 11 bytes for "hello world"; full snapshot 66 bytes |
| Reattach can be exact | `contents_formatted()` round-trips: `p2.screen().contents() == screen.contents()` |
| OSC 7 / OSC 133 are reachable | `Callbacks::unhandled_osc` captured cwd and marks `["A","B","C","D;1"]` |
| Window title is reachable | `Callbacks::set_window_title` captured OSC 2 |
| Resize lives on `Screen`, not `Parser` | `p.screen_mut().set_size(40, 120)` |

Licences verified against the crates.io API: `vt100` MIT, `tui-term` MIT, `ratatui` MIT,
`gix` MIT/Apache-2.0, `tachyonfx` MIT, `rio-vt` MIT, `nucleo` **MPL-2.0**.

**Herdr is AGPL-3.0-or-later with a commercial option.** Dock is MIT. Behaviour and UX may
be mimicked; source may not be read or copied. All Herdr-derived understanding in this spec
comes from public documentation.

## Architecture

```
                      ┌─────────────────────── dockd ───────────────────────┐
  PTY master bytes ──▶│ OwnedRuntime                                        │
                      │   PaneTerminal (vt100::Parser + Hooks)              │
                      │     ├── screen        authoritative grid            │
                      │     ├── prev          last screen sent per client   │
                      │     └── hooks         title / cwd / OSC 133 marks   │
                      │   detect::classify(screen, pgid) ──▶ AgentState     │
                      └──────────────┬──────────────────────────────────────┘
                                     │ protocol v7
                    ┌────────────────┴─────────────────┐
                    │ event stream (push)              │ control (req/resp)
                    │  PaneDelta{run_id, bytes, rev}   │  Workspace/Launch/Resize
                    │  PaneState / AgentState          │
                    ▼                                  ▼
                      ┌─────────────────── dock (client) ───────────────────┐
                      │ PaneView: own vt100::Parser fed by deltas           │
                      │   rendered via tui_term::PseudoTerminal             │
                      │ Keymap: prefix state machine ──▶ encode ──▶ PaneInput│
                      │ Theme                                                │
                      └──────────────────────────────────────────────────────┘
```

The daemon parses because it owns the PTY, must survive client detach, and needs screen text
for state detection. The client parses a *second* time so it can hand `tui-term` a real
`vt100::Screen`. This duplication is deliberate and cheap: the client is fed only deltas.

### Data flow

1. PTY reader thread appends bytes to the daemon's `vt100::Parser`.
2. On client attach, daemon sends `contents_formatted()` — one exact full screen.
3. Thereafter, per tick, daemon computes `screen.contents_diff(&prev)`, sends it if non-empty,
   and stores the new screen as `prev` for that client.
4. Client feeds received bytes into its own parser and renders with `PseudoTerminal`.
5. Keystrokes encode to bytes and go out on a write-only connection without awaiting a reply.

### Modules

| Path | Responsibility |
|---|---|
| `src/terminal/mod.rs` | `PaneTerminal` trait — feed bytes, resize, snapshot, delta. Abstracts the emulator so `rio-vt` (3x faster parse, 45x faster resize) can replace `vt100` later without touching callers |
| `src/terminal/vt.rs` | `vt100` implementation of `PaneTerminal` plus `Hooks` (title, cwd, OSC 133) |
| `src/terminal/keys.rs` | `encode(KeyEvent) -> Vec<u8>`: arrows, function keys, ctrl/alt combos, application cursor mode, bracketed paste |
| `src/keymap.rs` | Prefix state machine: `Direct \| Pending \| Command`. Owns the published binding table |
| `src/theme.rs` | `Theme` struct, palette tokens, border and dot glyphs |
| `src/detect/mod.rs` | `AgentKind`, `AgentState`, pane classification |
| `src/detect/process.rs` | Process-tree walk from a pane's own PGID |
| `src/detect/heuristic.rs` | Regex rules over screen tail producing `AgentState` |
| `src/discovery.rs` | Retained but enriched: machine-wide external agents with pid, cwd, tty, uptime. Display-only |

## Terminal emulation

`PaneTerminal` replaces `Scrollback` in `OwnedRuntime`:

```rust
pub trait PaneTerminal: Send {
    fn feed(&mut self, bytes: &[u8]);
    fn resize(&mut self, rows: u16, cols: u16);
    fn full_snapshot(&self) -> Vec<u8>;
    fn delta_since(&self, prev: &Self) -> Vec<u8>;
    fn text_tail(&self, rows: u16) -> String;   // for heuristic detection
    fn cursor(&self) -> (u16, u16);
    fn alternate_screen(&self) -> bool;
}
```

Scrollback capacity moves from a byte budget to a **row** budget (`vt100::Parser::new(rows,
cols, scrollback_rows)`), which is the unit users reason about. The privacy invariant is
unchanged: screen state is memory-only, never written to durable layout, never restored
across daemon restart.

## PTY sizing

`launch_child` gains a `winsize` parameter and calls `openpty(Some(&winsize), None)`. A new
`Request::PaneResize { workspace_id, pane_id, rows, cols }` triggers `TIOCSWINSZ` on the PTY
master followed by `SIGWINCH` to the owned process group only. The dashboard emits it when a
pane's inner rectangle changes, debounced to one per frame.

Signalling reuses the existing `OwnedProcessGroup` capability so an arbitrary PID still cannot
be signalled — the safety property in `src/runtime.rs` is preserved verbatim.

## Protocol v7

`RuntimeSnapshot` loses `scrollback: String`, `scrollback_bytes`, `scrollback_capacity_bytes`,
`scrollback_truncated` and gains `rows`, `cols`, `agent: Option<AgentKind>`,
`agent_state: AgentState`, `title: Option<String>`, `cwd: Option<String>`.

New request `Subscribe` puts a connection into stream mode. The daemon then writes
`Event` frames:

```rust
pub enum Event {
    PaneAttached { run_id: String, revision: u64, screen: Vec<u8> },
    PaneDelta    { run_id: String, revision: u64, bytes: Vec<u8> },
    PaneState    { run_id: String, state: ProcessState },
    AgentState   { run_id: String, agent: Option<AgentKind>, state: AgentState },
    LayoutChanged,
}
```

Deltas are base64 in JSON to stay within the existing line-delimited framing. `revision` is
monotonic per run; a client detecting a gap requests re-attach and receives a full snapshot.

The client runs a reader thread pushing `Event` into an `mpsc::Receiver`, and the main loop
selects over crossterm events and daemon events. Polling is removed.

Because every protocol struct carries `deny_unknown_fields`, daemon and client must upgrade
together. `HelloRequest.version` becomes 7 and a mismatched daemon is refused with a clear
message instructing the user to stop the old daemon.

## Input path and keymap

`Ctrl+B` is the prefix. The state machine:

- `Direct` — every key encodes to bytes and is written to the focused pane's PTY.
- `Pending` — entered by `Ctrl+B`. A which-key hint bar appears listing available actions,
  which is the discoverability property Zellij is repeatedly praised for.
- `Ctrl+B Ctrl+B` sends a literal `0x02` to the pane.

Published bindings:

| Key | Action |
|---|---|
| `n` | new workspace |
| `h` / `v` | split horizontal / vertical |
| `arrows` or `h/j/k/l` | focus pane |
| `+` / `-` | resize focused split |
| `z` | zoom focused pane |
| `r` | rename pane |
| `x` | close pane |
| `l` | launch agent picker |
| `d` | detach |
| `?` | help |
| `q` | quit |

`i` input mode is deleted; it existed only because panes were not real terminals. `Esc` is no
longer intercepted and is forwarded to the pane, which is required for Vim and for agent TUIs.

Latency budget: keypress to PTY write is one `write(2)` on an already-open socket, with no
await. Local paint happens before the echo returns.

## Shell by default

New `AdapterId::Shell` and `DashboardProfile::Shell` resolving `$SHELL`, falling back to
`/bin/sh`, launched as a login shell to match tmux's default on macOS. `WorkspaceRequest::Create`
and `::Split` auto-launch a shell run bound `BindingKind::Terminal` with cwd set to the
runtime directory. A pane is therefore never inert.

This preserves the no-adoption invariant: the shell is a Dock-created PTY in a Dock-created
process group, identical to any other owned run.

## Agent detection (heuristic tier)

```rust
pub enum AgentKind { Claude, Codex, Amp, Copilot, OpenCode, Gemini, Cursor,
                     Droid, Qwen, Kimi, Kiro, Hermes, Pi, Antigravity, Vibe, Omp }

pub enum AgentState { Blocked, Working, Done, Idle }
```

Classification is two-layer, mirroring the public description of Herdr's approach:

1. **Process layer** — walk `ps -axo pid=,ppid=,pgid=,comm=` filtered to the pane's own PGID
   and match the leaf executable name to an `AgentKind`.
2. **Heuristic layer** — embedded per-agent regex rules matched against `text_tail(N)`
   producing an `AgentState`, defaulting to `Idle` when nothing matches.

Ordering for the sidebar is attention-first: `Blocked > Working > Done > Idle`. Hook-based
exact state (`dock integration install claude`, using Claude Code's `Notification` hook with
`permission_prompt` / `idle_prompt` and its `Stop` hook) is P1; the `AgentState` type is
introduced here so P1 only swaps the producer.

The existing machine-wide scan stays, enriched with pid, cwd, tty and uptime, and remains
strictly display-only and labelled `external/read-only`.

## Theme

`Theme` carries semantic tokens rather than raw colours: `accent`, `surface`, `muted`,
`border`, `border_focused`, plus one colour per `AgentState`. Default is "warm terminal-modern":
rounded borders (`BorderType::Rounded`), a filled dot for active and hollow for idle, focused
panes accented while siblings dim, and pane titles reading `● claude · dock · 2m14s`.

Only the default theme ships in P0. Loading themes from config is P4, but no colour is
hardcoded outside `theme.rs`, so P4 is a data change.

## Error handling

- Emulator never panics on malformed input; `vt100` is total over byte sequences.
- A delta gap (non-contiguous `revision`) triggers re-attach, not a crash.
- Daemon protocol mismatch fails closed with an actionable message.
- Resize on an exited run is a no-op, not an error; stale PGIDs are never signalled.
- Detection failure degrades to `AgentKind::None` and `AgentState::Idle`; it never blocks render.
- Shell launch failure surfaces in the pane as a `FailedToLaunch` run with its diagnostic, and
  the pane remains operable for close and relaunch.

## Testing

Rewritten, not deleted. The 13 tests in `src/dashboard.rs` (499 lines) assert behaviour P0
deliberately removes (`published_keymap_help_is_contextual`, `input_mode_requires_owned_binding`).
Each is rewritten to assert the new contract. The repository has 132 tests total; none may regress.

New coverage:

- Emulation: SGR, cursor motion, alternate screen enter/exit, and wrap render to expected cells.
- Delta round-trip: `full_snapshot` then a sequence of `delta_since` reproduces the source screen.
- Revision gap forces re-attach.
- Resize: `PaneResize` issues `TIOCSWINSZ` and the child observes the new size (PTY integration test).
- Key encoding: table-driven over arrows, F-keys, ctrl/alt, application cursor mode, bracketed paste.
- Prefix state machine: direct passthrough, pending transitions, literal `Ctrl+B Ctrl+B`, `Esc` forwarded.
- Shell auto-launch on create and split; pane is `Running` without explicit launch.
- Detection: fixture `ps` output to `AgentKind`; fixture screens to `AgentState`; attention ordering.
- Privacy: durable layout still contains no screen content, command vectors, PIDs, or PGIDs.

Existing verification gates continue to apply unchanged:
`cargo fmt --check`, `cargo test --all-targets`,
`cargo clippy --all-targets --all-features -- -D warnings`, and the macOS smoke scripts.
Smoke scripts referencing input mode are updated to the prefix model.

## Migration

Protocol v6 to v7 is breaking. `dock` refuses a v6 daemon with an instruction to stop it.
Durable layout records are unaffected — topology and labels are unchanged, and screen state was
never persisted. Non-interactive binaries (`dock-workspace`, `dock-dispatch`, `dock-inspect`,
`dock-agent`, `dock-programme`) keep their current contracts; only `RuntimeSnapshot` output
changes shape, and `docs/terminal-runtime-parity.md` is updated accordingly.

## Acceptance evidence

P0 is complete when, on a clean checkout:

1. `cargo run --bin dock` in a non-Git directory opens a shell pane accepting input immediately.
2. `vim` runs correctly in a pane, including alternate screen and `Esc`.
3. `claude` or `codex` runs in a pane and redraws correctly after a split changes its width.
4. Splitting to four panes and typing in each keeps keypress-to-paint visibly immediate.
5. The sidebar shows per-pane agent identity and state, blocked first.
6. `Ctrl+B ?` lists bindings; `Ctrl+B Ctrl+B` types a literal `^B` into the shell.
7. All four verification gates pass.

## Risks

| Risk | Mitigation |
|---|---|
| Scope creep back into P1–P4 | Non-goals are enumerated above and enforced at review |
| `vt100` is stable but last released 2025-07 | `PaneTerminal` trait makes `rio-vt` a drop-in later; `rio-vt` is deliberately not adopted now at 21k downloads |
| Duplicate parsing cost on client | Client is fed deltas only; probe shows 11 bytes for a typical update |
| Prefix key breaks existing muscle memory | Unavoidable once panes are live terminals; `Ctrl+B` matches tmux and Herdr so it is the least surprising choice |
| Test churn | Rewrite rather than delete; contract coverage must not regress |

## Programme decomposition

P0 is one of five sub-projects, each getting its own spec, plan, and build cycle.

| | Sub-project | Content |
|---|---|---|
| **P0** | Real terminal | This document |
| **P2** | Review loop | Native fast-path diff (syntect + two-face), handoff surfacing, accept/reject, worktree binding, `prefix+g` to hosted lazygit. **The wedge** |
| **P1** | Agent awareness | Hook integrations for exact state, session identity and resume, attention queue, notifications |
| **P3** | Board | kanban-md rendering, claim-to-dispatch into a worktree, dependency gates |
| **P4** | Craft | Command palette (nucleo), copy mode and scrollback search, OSC 133 consumers, themes, effects, single-binary install |

Build order is P0, P2, P1, P3, P4: shipping the wedge before full Herdr parity means the first
demo shows something no competitor can show, rather than a weaker Herdr.
