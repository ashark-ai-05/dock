# Terminal-runtime parity matrix

Status reflects the initial Slice 6 vertical slice, not complete terminal-emulator parity.

| Daily capability | Status | Current evidence / boundary |
|---|---|---|
| Multiple workspaces and panes | Shipped | Dynamic daemon-owned model and protocol v6 CLI |
| Horizontal/vertical split | Shipped | Deterministic bounded binary layout tree |
| Focus, resize, rename, close | Shipped | Published keyboard-first map plus mouse equivalents; focus and geometry paint optimistically before strict socket authority replies |
| Layout persistence | Shipped | Owner-only atomic topology and labels |
| Bounded pane admission | Shipped | 64 panes per workspace; atomic refusal precedes receipt, pane, PTY, and process creation; explicit close reaps the owned runtime and frees the slot |
| Client reconnect | Shipped | Live daemon state remains authoritative |
| Daemon restart | Intentionally different | Every durable pane returns as `restored` with no run binding; live-daemon fresh panes start `empty`; old processes/output are never recovered |
| Safe exited-pane operations | Shipped | Metadata remains operable; stale PGIDs are never signalled |
| Owned PTY/process groups | Shipped | Only Dock-created groups; no adoption API |
| Bounded live scrollback | Shipped | Per-runtime configured byte bound; oldest bytes are discarded as live output arrives; available on same-daemon reconnect only and absent from durable layout/restart recovery |
| Pane swap | Deferred | Follow-on tree operation |
| Zoom | Deferred | Follow-on layout operation |
| Terminal input | Shipped (bounded) | Explicit labelled input mode for a focused Dock-owned run; `Esc` exits locally and is never forwarded. Full terminal emulation remains deferred |
| Themes/configuration | Deferred | Slice 6 follow-on |
| Notifications | Deferred | Slice 6 follow-on |
| Mouse layout interaction | Shipped | Pane focus, divider resize, dismiss, and fixed-profile launch mirror keyboard-accessible actions |

Durable layout records contain no raw terminal content, command vectors, PIDs, process-group IDs, retained run bindings after restart, or absolute repository/worktree paths. Git, task, worktree, and programme authority remains unchanged.
