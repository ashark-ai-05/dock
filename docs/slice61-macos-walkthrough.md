# Slice 6.1 macOS walkthrough

1. From a Git worktree run `cargo run --bin dock`. Confirm it starts or reconnects to `dockd` and
   that quitting with `q` restores the shell cursor, cooked input, alternate screen, and mouse mode.
2. Press `n`, then `h` and `v` to create nested panes. Use Tab/arrows and `[`/`]` for pane and
   workspace focus. Use `r` to rename and `x` to close.
3. Click panes to focus. Drag a divider beyond each edge and confirm both children retain their
   minimum size.
4. Press `l` (or click `LAUNCH DOCK FIXTURE`) to create an explicit Dock-owned fixture run. Confirm
   its pane shows the real repository, task, run, workspace, and pane binding. Empty panes must show
   explicit `unbound` labels. Press `i`, type input, and press Escape. This is bounded text replay,
   not VT emulation.
5. Confirm the Existing Agents panel labels recognised processes `external/read-only`; it offers no
   attach, focus, input, signal, or adoption action. Press `d` or click `dismiss all` and confirm the
   candidates disappear without affecting the Dock-owned run.
6. Quit and launch `dock` again to reconnect. Restart `dockd` explicitly and confirm persisted panes
   appear as restored with no former process authority.

`scripts/smoke-slice61-macos.sh` invokes `dock` as its only product command on a PTY slave owned by
the test harness. The harness records that slave's complete macOS termios immediately before exec,
runs and reaps the real foreground dashboard on the same slave, then reads and compares every
termios field through the retained slave descriptor without masking any flag in the exact
comparison. On explicit `q` quit, Dock restores the complete entry termios snapshot. The harness drives
safe debug test keys through create/launch/quit, reconnects via the dashboard, checks
alternate-screen enter/leave bytes in the PTY transcript, and checks restricted socket and Git
immutability evidence. Direct headless and `dock-workspace` commands remain compatible but are
deliberately absent from this user-path smoke.
