# Terminal-runtime parity matrix

Status reflects the shipped P0 real-terminal runtime, not complete terminal-emulator parity.
Every row below was checked directly against the code, not carried over from the original plan.

| Daily capability | Status | Current evidence / boundary |
|---|---|---|
| Multiple workspaces and panes | Shipped | Dynamic daemon-owned model and protocol v7 CLI; a v6 daemon must be stopped before this client can attach |
| Horizontal/vertical split | Shipped | Deterministic bounded binary layout tree |
| Focus, resize, rename, close | Shipped | Published keyboard-first map (`Ctrl+B` prefix) plus mouse equivalents; focus and geometry paint optimistically before strict socket authority replies |
| Zoom | Shipped | `Ctrl+B z` toggles a client-local full-area view of the focused pane; the daemon's layout tree is untouched, but the pane's inner geometry changes and the next frame announces the new PTY size |
| Layout persistence | Shipped | Owner-only atomic topology and labels |
| Bounded pane admission | Shipped | 64 panes per workspace; atomic refusal precedes receipt, pane, PTY, and process creation; explicit close reaps the owned runtime and frees the slot |
| Shell panes | Shipped | Every pane auto-launches `$SHELL` (falling back to `/bin/sh`) the moment it exists, bound under a synthetic `dock_sh_<workspace>_<pane>` run; a pane is never inert. An explicit launch into that pane displaces and retires the placeholder shell |
| Terminal input | Shipped | Full VT emulation via `vt100`, rendered through `tui-term`; agent TUIs and `vim` (including the alternate screen) work. Unprefixed keys reach the PTY; `Ctrl+B` is the command prefix, `Ctrl+B Ctrl+B` sends a literal `0x02`, and `Esc` is always forwarded, never intercepted |
| PTY resize | Shipped | Pane geometry drives `TIOCSWINSZ` plus `SIGWINCH` on the owned process group; `openpty` takes the initial size so a program never observes a placeholder size before its real one |
| Agent state detection | Shipped (heuristic tier) | Screen-content heuristic classifies each run as blocked/working/done/idle; the sidebar sorts blocked-first. Detection of external (non-Dock-launched) processes is display-only — `external/read-only` — and launching never adopts one |
| Signal delivery (`Ctrl+C`) | **Known defect** | `Ctrl+C` does not interrupt a running pane child. The launch guardian backgrounds the worker shell; POSIX sets `SIGINT`/`SIGQUIT` to ignore for a non-interactive shell's async jobs, and `SIG_IGN` survives `exec`. Both delivery paths are dead: Dock's own `killpg(SIGINT)` and the line discipline converting a `0x03` byte written to the PTY master. `SIGTERM` is unaffected, so `stop`, `Drop`, and guardian cleanup all still work. Tracked as a dedicated follow-up task |
| Client reconnect | Partial | A running dashboard holds two daemon connections — the event stream and a separate request connection, since `CLIENT_READ_TIMEOUT` is inert after `Subscribe` and makes a subscribed connection one-way. Only the event stream re-subscribes automatically on a dropped socket; the request connection has no reconnect path, so its loss ends the dashboard |
| Concurrent dashboards per daemon | Shipped (bounded) | The daemon admits 32 connections total; each running dashboard consumes 2 of them, so one daemon supports 16 concurrent dashboards. Slots release when a dashboard exits or its socket closes, so a crashed client recovers capacity on its own |
| Daemon restart | Intentionally different | Restart recovers layout metadata only, never processes: every durable pane returns as `restored` with no run binding; live-daemon fresh panes start with their auto-launched shell; old processes/output are never recovered |
| Safe exited-pane operations | Shipped | Metadata remains operable; stale PGIDs are never signalled |
| Owned PTY/process groups | Shipped | Only Dock-created groups; no adoption API |
| Bounded live scrollback | Shipped | Per-pane configured row budget (`vt100::Parser`'s scrollback rows, default 2000 via `dockd --scrollback-rows`); oldest rows are discarded as live output arrives; available on same-daemon reconnect only and absent from durable layout/restart recovery |
| Pane swap | Deferred | Follow-on tree operation |
| Themes/configuration | Partial | Warm terminal-modern theme shipped, with rounded borders and semantic colour tokens for every agent state; loading alternative palettes as data is deferred |
| Notifications | Deferred | Follow-on |
| Mouse layout interaction | Shipped | Pane focus, divider resize, dismiss, and fixed-profile launch mirror keyboard-accessible actions. Dismissing all discovered external agents (`d` in the sidebar) is mouse-only — there is no keyboard binding for it, distinct from the `Ctrl+B d` "leave" command |

Durable layout records contain no raw terminal content, command vectors, PIDs, process-group IDs, retained run bindings after restart, or absolute repository/worktree paths. Git, task, worktree, and programme authority remains unchanged.
