# Responsive Selection, Pointer Menus, and a Cool Palette — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make text selection feel instant, give the pointer a right-click menu on every surface, let the kanban board spend its width where the cards are and retire finished cards, collapse the sidebar to a rail, and repaint Dock in a cool palette whose contrast is solved for rather than eyeballed.

**Architecture:** Nine tasks, ordered repairs-before-additions. The two latency repairs come first because they are regressions: focus-on-click stops blocking on the daemon, and copy mode's screen clone becomes lazy so an idle pane pays nothing. The rest are additive and independent of each other. Everything lands in the existing client-side files — no protocol change, no daemon change, no new crate.

**Tech Stack:** Rust 2024 edition, `ratatui` + `crossterm` for the TUI, `vt100` for terminal emulation, `tui-term`'s `PseudoTerminal` widget. Tests are `#[cfg(test)]` modules inline in each file, run with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-08-26-responsive-selection-pointer-menus-and-a-cool-palette-design.md`

## Global Constraints

- **No colour may be hardcoded outside `src/theme.rs`.** This is a standing rule stated in that file's module comment. Every new render path takes its colours from `Theme`.
- **Never reformat a task file on the way past.** `src/board.rs` writes task files that `kanban-md`, editors, and other committers share. Rewrites touch only the one line they mean to touch and preserve everything else byte for byte.
- **`PaneSnapshot` must stay non-`Clone`.** The comment at `src/terminal/vt.rs:299` explains why: only the type system keeps a stray `.clone()` off the render path.
- **Dock only ever writes to its own board.** `board::is_personal` decides; a repository's board is refused with a sentence naming `kanban-md`.
- **Every task ends green:** `cargo test` (598 lib + 42 bin at baseline), `cargo fmt --check`, `cargo clippy -- -D warnings`.
- **Frame budget:** `render_breakdown_of_a_busy_dashboard_by_the_work_it_does` must not regress more than 10 % against the baseline recorded in the spec (0.091 / 0.327 / 1.438 ms at 80×24 / 200×50 / 400×100).

---

### Task 1: A press that starts a selection stops waiting for the daemon

`Dashboard::mouse` returns `UiCommand::Request` when a press lands in an unfocused pane. `main.rs` handles that by blocking on `client.request` and then calling `refresh`, which blocks on three more round trips, one of which can shell out to `ps`. The focus was already applied locally one line earlier, so none of that waiting buys anything.

**Files:**
- Modify: `src/dashboard.rs:110-135` (the `UiCommand` enum)
- Modify: `src/dashboard.rs:5159-5162` (the focus arm of `mouse`)
- Modify: `src/dashboard.rs:6592-6600` (an existing test that asserts the old variant)
- Modify: `src/main.rs:648-814` (the `match command` block)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `UiCommand::Send(Box<Request>)` — a request that is painted and sent without waiting for its reply. Task 5 and Task 6 both use it.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `src/dashboard.rs`, next to `keyboard_and_mouse_focus_and_bounded_resize`:

```rust
/// A press that focuses a pane must not put a blocking daemon round trip in front of the
/// drag it begins. `Send` is painted and posted; `Request` would be waited on, and then
/// `refresh` would wait on three more — which is what made the first click of a selection
/// hitch. The pane is focused locally either way, so nothing is lost by not waiting.
#[test]
fn focusing_a_pane_by_pointer_is_posted_rather_than_waited_on() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let b = dashboard.pane_areas["b"];
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: b.x + 1,
        row: b.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let a = dashboard.pane_areas["a"];
    let focus = dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: a.x + 1,
        row: a.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(&focus, UiCommand::Send(request) if matches!(request.as_ref(),
            Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "a")),
        "a pointer focus must be posted, not awaited: {focus:?}"
    );
    assert_eq!(
        dashboard.workspace().unwrap().focused_pane_id,
        "a",
        "and the focus must already be applied locally, or the paint would show the old pane"
    );
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib focusing_a_pane_by_pointer_is_posted -- --nocapture`
Expected: FAIL — `no variant or associated item named 'Send' found for enum 'UiCommand'`.

- [ ] **Step 3: Add the variant**

In `src/dashboard.rs`, inside `pub enum UiCommand`, directly after the `Request` variant:

```rust
    /// A request whose answer nobody needs: painted, posted, and not waited on.
    ///
    /// `Request` blocks on the daemon and then refreshes, which is four round trips and
    /// possibly a `ps`. That is right when the answer is the product — a queue listing, a
    /// page of history. It is wrong for a change the dashboard has already made locally and
    /// is only telling the daemon about, because there the waiting is pure latency in front
    /// of whatever gesture the user is mid-way through. `PaneResize` already goes this way
    /// for the same reason; a refused request is not lost, because the client counts unread
    /// replies and `take_deferred_error` surfaces them on the next drain.
    Send(Box<Request>),
```

- [ ] **Step 4: Return it from the focus arm**

In `src/dashboard.rs`, in `mouse`, replace the tail of the left-press-in-a-pane arm:

```rust
                self.layout.workspaces[self.workspace_index].focused_pane_id = pane_id.clone();
                UiCommand::Send(Box::new(Request::Workspace(WorkspaceRequest::Focus {
                    workspace_id,
                    pane_id,
                })))
```

- [ ] **Step 5: Handle it in the loop**

In `src/main.rs`, add an arm to `match command`, immediately after the `UiCommand::Request(request) => { … }` arm closes:

```rust
            UiCommand::Send(request) => {
                // Painted first, exactly as `Request` is, so the optimistic local change is on
                // screen before anything touches the socket. Then posted and forgotten: there
                // is no `refresh` here, because the daemon's own event stream is what
                // reconciles a change the dashboard has already made.
                terminal
                    .draw(|frame| dashboard.render(frame))
                    .map_err(|e| e.to_string())?;
                let _ = client.send(&request);
            }
```

- [ ] **Step 6: Fix the existing test that asserted the old variant**

In `src/dashboard.rs:6598`, inside `keyboard_and_mouse_focus_and_bounded_resize`, change:

```rust
        assert!(
            matches!(focus, UiCommand::Send(request) if matches!(request.as_ref(), Request::Workspace(WorkspaceRequest::Focus { pane_id, .. }) if pane_id == "a"))
        );
```

- [ ] **Step 7: Run the whole suite**

Run: `cargo test`
Expected: PASS, 598 lib + 42 bin. If any other test matches on `UiCommand::Request` for a *focus*, update it the same way; do not change assertions about any other request kind.

- [ ] **Step 8: Check formatting and lints**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: clean. `UiCommand::Send` is matched in `main.rs`, so no non-exhaustive-match warning should appear.

- [ ] **Step 9: Commit**

```bash
git add src/dashboard.rs src/main.rs
git commit -m "perf: stop a pointer focus from waiting on four daemon round trips"
```

---

### Task 2: Freeze a pane only when its output actually moves

Copy mode clones the whole `vt100::Screen` — grid and scrollback, about 7.7 MB at the daemon's default 2000 retained rows — on the first drag of every selection. The clone exists so live output cannot scroll text out from under the selection. On an idle pane there is no live output, so it buys nothing.

**Files:**
- Modify: `src/dashboard.rs:693-740` (`CopyMode`)
- Modify: `src/dashboard.rs:802-822` (`apply_event`'s `PaneDelta` arm)
- Modify: `src/dashboard.rs:2418-2432`, `3855`, `3866-3920`, `3953-3975`, `3982-4006`, `4036-4051`, `4109-4145`, `4180-4195`, `4242-4272` (every reader of `mode.frozen`)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `enum SelectionScreen { Live, Frozen(PaneSnapshot) }`
  - `CopyMode::screen_of<'a>(&'a self, live: Option<&'a PaneScreen>) -> Option<&'a vt100::Screen>`
  - `Dashboard::selection_screen(&self) -> Option<&vt100::Screen>`
  - `Dashboard::freeze_selection(&mut self, run_id: &str)`

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `src/dashboard.rs`:

```rust
/// A selection on an idle pane must not clone the screen, and a selection on a pane that
/// then produces output must still yank the grid it was made against.
///
/// Both halves matter and they pull opposite ways, which is why they are one test. The
/// freeze is what makes copy mode a freeze rather than a claim; doing it eagerly is what
/// put a multi-megabyte clone on the first frame of every drag.
#[test]
fn a_selection_clones_the_screen_only_once_output_arrives_under_it() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    dashboard.apply_event(attach_event("run_a", 40, 100, 2000));
    dashboard.apply_event(delta_event("run_a", 1, b"selected text\r\n"));
    terminal.draw(|frame| dashboard.render(frame)).unwrap();

    let a = dashboard.pane_areas["a"];
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: a.x + 1,
        row: a.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: a.x + 8,
        row: a.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(dashboard.copy.as_ref().unwrap().screen, SelectionScreen::Live),
        "an idle pane must not have been cloned"
    );

    // Output arrives under the selection. The grid it was made against has to be captured
    // before the delta reaches the parser, or the highlight would be over different text.
    dashboard.apply_event(delta_event("run_a", 2, b"\x1b[2J\x1b[Hwiped\r\n"));
    assert!(
        matches!(dashboard.copy.as_ref().unwrap().screen, SelectionScreen::Frozen(_)),
        "output under a live selection must freeze it"
    );
    let mode = dashboard.copy.as_ref().unwrap();
    let (from, to) = mode.session.selection().unwrap();
    let text = dashboard.selection_screen().unwrap();
    assert!(
        crate::terminal::text_between_for_test(text, from, to).contains("selecte"),
        "the frozen grid must still be the one the pointer was put on"
    );
}
```

If `attach_event` / `delta_event` / `fixture_dashboard` have different names or arities in this module, use the ones already there — `delta_event` is at `src/dashboard.rs:6280`. If no `text_between_for_test` helper exists, assert through `mode.selection_text()` instead (Step 4 adds it) and drop the helper call.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --lib a_selection_clones_the_screen_only_once -- --nocapture`
Expected: FAIL — `SelectionScreen` does not exist.

- [ ] **Step 3: Introduce the two-state screen**

In `src/dashboard.rs`, replace the `frozen` field on `CopyMode` and add the enum above it:

```rust
/// The grid a selection is against.
///
/// A selection starts `Live`: nothing has arrived for this pane since the gesture began, so
/// the live parser's own screen still shows exactly the characters the pointer was put on,
/// and cloning it would copy several megabytes to describe something already in hand. The
/// moment output is about to change that grid — or the moment the user asks to walk back
/// through scrollback, which the live viewport is not showing — the clone is taken and the
/// selection becomes `Frozen` against it.
///
/// The observable behaviour is identical either way. What changes is that the common case,
/// a selection dragged across a pane that is not printing anything, now costs nothing.
enum SelectionScreen {
    Live,
    Frozen(PaneSnapshot),
}

/// An open copy mode: the selection, and the screen it is a selection *of*.
struct CopyMode {
    session: CopySession,
    screen: SelectionScreen,
}

impl CopyMode {
    fn new(run_id: String, cursor: (u16, u16), screen: SelectionScreen) -> Self {
        Self {
            session: CopySession::new(run_id, cursor),
            screen,
        }
    }

    fn is_for(&self, run_id: &str) -> bool {
        self.session.run_id == run_id
    }

    /// The grid to read: the clone once one has been taken, and the live parser until then.
    fn screen_of<'a>(&'a self, live: Option<&'a PaneScreen>) -> Option<&'a vt100::Screen> {
        match &self.screen {
            SelectionScreen::Frozen(snapshot) => Some(snapshot.screen()),
            SelectionScreen::Live => live.map(PaneScreen::screen),
        }
    }
}
```

Keep `CopyMode::step` (the scrollback walk at `src/dashboard.rs:712`) but have it call `Dashboard::freeze_selection` first — see Step 5.

- [ ] **Step 4: Add the two resolvers on `Dashboard`**

```rust
    /// The grid the open selection is against, whichever half of `SelectionScreen` holds it.
    fn selection_screen(&self) -> Option<&vt100::Screen> {
        let mode = self.copy.as_ref()?;
        mode.screen_of(self.screens.get(&mode.session.run_id))
    }

    /// Captures the grid a selection was made against, if it has not been captured already.
    ///
    /// Called at every point where the live screen is about to stop being that grid. Cheap
    /// and idempotent when the selection is already frozen, which is what lets it sit on the
    /// delta path without a second guard around it.
    fn freeze_selection(&mut self, run_id: &str) {
        let needs = self.copy.as_ref().is_some_and(|mode| {
            mode.is_for(run_id) && matches!(mode.screen, SelectionScreen::Live)
        });
        if !needs {
            return;
        }
        // Read before the mutable borrow below rather than inside it: `screens` and `copy`
        // are both fields of `self`, and taking one mutably while reading the other is not
        // something the borrow checker will allow in one expression.
        let Some(snapshot) = self.screens.get(run_id).map(PaneScreen::snapshot) else {
            return;
        };
        if let Some(mode) = self.copy.as_mut() {
            mode.screen = SelectionScreen::Frozen(snapshot);
        }
    }
```

- [ ] **Step 5: Freeze at the four moments that need it**

In `apply_event`'s `PaneDelta` arm, restructure so the freeze happens *before* the parser is fed:

```rust
            Event::PaneDelta {
                run_id,
                revision,
                bytes,
            } => {
                let expected = self.revisions.get(&run_id).map(|value| value + 1);
                if expected != Some(revision) {
                    self.screens.remove(&run_id);
                    self.revisions.remove(&run_id);
                    self.history.remove(&run_id);
                    self.end_copy_mode_for(&run_id, "the pane lost sync and is re-seeding");
                    return;
                }
                let Ok(decoded) = STANDARD.decode(&bytes) else {
                    return;
                };
                // The one place a pointer selection ever pays for a clone: the grid it was
                // made against is about to be written over, so it is captured first.
                self.freeze_selection(&run_id);
                if let Some(terminal) = self.screens.get_mut(&run_id) {
                    terminal.feed(&decoded);
                    self.revisions.insert(run_id.clone(), revision);
                    self.retain_history_bytes(&run_id, &decoded);
                }
            }
```

Add `self.freeze_selection(&run_id);` at the top of the wheel-scroll path in `mouse` (`src/dashboard.rs:4184`, where `mode.frozen.scroll_by` is called) and at the top of `CopyMode::step`'s caller in `copy_key`, since both walk into scrollback the live viewport is not showing.

In `enter_copy_mode` (`src/dashboard.rs:3855`), keep the eager clone — `Ctrl+B [` is a deliberate request to walk history:

```rust
        self.copy = Some(CopyMode::new(
            run_id,
            screen.cursor(),
            SelectionScreen::Frozen(screen.snapshot()),
        ));
```

In `drag_selection` (`src/dashboard.rs:4258`), start live:

```rust
        if self.copy.is_none() {
            self.copy = Some(CopyMode::new(
                drag.run_id.clone(),
                drag.origin,
                SelectionScreen::Live,
            ));
            self.copy_searching = false;
            self.error = None;
        }
```

- [ ] **Step 6: Point every remaining reader at the resolver**

Each of these currently reads `mode.frozen.<method>()`. Replace with the resolved screen. The call sites and their replacements:

| site | was | becomes |
|---|---|---|
| `2425` render | `Some(mode.frozen.screen())` | `self.selection_screen()` |
| `3871` bounds | `mode.frozen.size()` | `self.screens.get(&mode.session.run_id).and_then(...)` → size of resolved screen, defaulting to `(0, 0)` |
| `3961` search | `mode.frozen.visible_row(row)` | `row_text` of the resolved screen |
| `3985`/`4044` yank | `mode.frozen.selection_text(from, to)` | `text_between` on the resolved screen |
| `4119` word/line click | `mode.frozen.size()`, `visible_row` | resolved screen |
| `4266` drag bounds | `mode.frozen.size()` | resolved screen |

To keep this from becoming a dozen borrow puzzles, add one helper beside `selection_screen` and use it everywhere:

```rust
    /// Size of the grid the open selection is against; `(0, 0)` when there is none, which
    /// every caller already clamps against.
    fn selection_bounds(&self) -> (u16, u16) {
        self.selection_screen().map_or((0, 0), vt100::Screen::size)
    }
```

Note that `copy_key` takes the mode out of `self.copy` (`src/dashboard.rs:3866`), so inside it the mode is owned and `self.screens` is freely borrowable — resolve with `mode.screen_of(self.screens.get(&mode.session.run_id))` directly there.

Make `text_between` in `src/terminal/vt.rs:265` `pub(crate)` so `dashboard.rs` can call it on a resolved `&vt100::Screen`. Its doc comment already explains why it is free-standing rather than a method; extend that comment with: *"and `pub(crate)` so a selection that has not been frozen yet can be read from the live screen through the same one answer."*

- [ ] **Step 7: Run the copy-mode tests**

Run: `cargo test --lib copy`
Expected: PASS. Pay particular attention to `a_mid_row_selection_yanks_exactly_as_many_characters_as_it_highlights` — it guards the inclusive/exclusive agreement between the highlight and the yank, and it must still pass unchanged.

- [ ] **Step 8: Run the whole suite and the lints**

Run: `cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 9: Commit**

```bash
git add src/dashboard.rs src/terminal/vt.rs
git commit -m "perf: clone a pane's screen only when output moves under the selection"
```

---

### Task 3: A selection that behaves the way selections behave, and a clipboard that is actually reached

Four gaps. Three are in the pointer path; the fourth is a default that makes every copy on a Mac silently do nothing visible to `Cmd+V`.

**Files:**
- Modify: `src/dashboard.rs:5002-5165` (`mouse`, the left-press arm)
- Modify: `src/dashboard.rs:3866-3920` (`copy_key`, the Esc arm)
- Modify: `src/clipboard.rs:57-68` (`preference_from`) and its tests

**Interfaces:**
- Consumes: `Dashboard::freeze_selection` from Task 2.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing tests**

In `src/clipboard.rs`'s test module:

```rust
/// An unset `DOCK_CLIPBOARD` must try a local helper as well as the terminal.
///
/// OSC 52 is write-only — the terminal is asked and never answers — and Terminal.app
/// disables it outright, iTerm2 disables it by default, and tmux ignores it without
/// `set-clipboard on`. Defaulting to it alone meant the common case on a Mac was a notice
/// saying "copied", a clipboard that had not changed, and no way to tell from the message
/// which had happened. `Both` costs one extra `pbcopy` on a deliberate gesture and makes the
/// copy real; where no helper exists, `copy_with` finds none and the behaviour — and the
/// honest notice — are exactly what they were.
#[test]
fn the_default_preference_asks_the_terminal_and_a_local_helper() {
    assert_eq!(preference_from(None), Ok(ClipboardPreference::Both));
    assert_eq!(preference_from(Some("")), Ok(ClipboardPreference::Both));
    assert_eq!(preference_from(Some("auto")), Ok(ClipboardPreference::Both));
    // Explicit values still win, including the old default.
    assert_eq!(preference_from(Some("osc52")), Ok(ClipboardPreference::Osc52));
    assert_eq!(preference_from(Some("helper")), Ok(ClipboardPreference::Helper));
    assert_eq!(preference_from(Some("off")), Ok(ClipboardPreference::Off));
}
```

In `src/dashboard.rs`'s test module:

```rust
/// Shift+click extends the standing selection instead of starting a new one, which is what
/// every terminal does and what makes a selection correctable without re-dragging it.
#[test]
fn shift_clicking_extends_the_selection_rather_than_restarting_it() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    dashboard.apply_event(attach_event("run_a", 40, 100, 2000));
    dashboard.apply_event(delta_event("run_a", 1, b"abcdefghijklmnop\r\n"));
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let a = dashboard.pane_areas["a"];
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), a.x + 1),
        (MouseEventKind::Drag(MouseButton::Left), a.x + 4),
        (MouseEventKind::Up(MouseButton::Left), a.x + 4),
    ] {
        dashboard.mouse(MouseEvent { kind, column, row: a.y + 1, modifiers: KeyModifiers::NONE });
    }
    let before = dashboard.copy.as_ref().unwrap().session.selection().unwrap();
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: a.x + 9,
        row: a.y + 1,
        modifiers: KeyModifiers::SHIFT,
    });
    let after = dashboard.copy.as_ref().unwrap().session.selection().unwrap();
    assert_eq!(after.0, before.0, "the anchor must survive a shift+click");
    assert_ne!(after.1, before.1, "and the cursor must have moved to it");
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib the_default_preference_asks && cargo test --lib shift_clicking_extends`
Expected: both FAIL — the first on `Osc52 != Both`, the second because shift+click currently arms a fresh `PaneDrag`.

- [ ] **Step 3: Change the clipboard default**

In `src/clipboard.rs`, in `preference_from`:

```rust
        None | Some("") | Some("auto") => Ok(ClipboardPreference::Both),
        Some("osc52") => Ok(ClipboardPreference::Osc52),
```

Update the doc comment on `ClipboardPreference::Osc52` so it no longer calls itself "the default", and update the `Both` variant's comment to say it is. Leave `fn preference()`'s `cfg!(test)` guard exactly as it is — it is what keeps the crate's own tests from spawning `pbcopy` over the clipboard of whoever is running them.

- [ ] **Step 4: Make shift+click extend**

In `mouse`, in the `MouseEventKind::Down(MouseButton::Left)` arm, before the `let armed = …` block that arms a fresh `PaneDrag`:

```rust
                // Shift extends what is already selected rather than starting again. The
                // anchor stays where the original press put it; only the cursor moves, which
                // is the whole difference between correcting a selection and remaking it.
                if event.modifiers.contains(KeyModifiers::SHIFT)
                    && self.copy.as_ref().is_some_and(|mode| mode.is_for(&run_id))
                {
                    let bounds = self.selection_bounds();
                    if let Some(inner) = self.pane_inner_areas.get(&pane_id).copied()
                        && let Some(mode) = self.copy.as_mut()
                    {
                        mode.session
                            .set_cursor(clamp_cell(inner, event.column, event.row), bounds);
                    }
                    self.copy_pointer_selection();
                    return UiCommand::None;
                }
```

`run_id` must be resolved before this block; take it from the same `workspace().panes.get(&pane_id).run_id` lookup the `armed` block already does, hoisted above both.

- [ ] **Step 5: Make a drag past the edge scroll**

In `drag_selection`, after the cursor is set:

```rust
        // A drag that leaves the pane scrolls it, one row per motion report, rather than
        // stopping at the boundary. Reaching for text just off-screen is the most common
        // reason a selection needs correcting at all, and clamping made it impossible.
        // Scrolling needs history the live viewport is not showing, so it freezes first.
        let beyond = if row < drag.inner.y {
            1
        } else if row >= drag.inner.bottom() {
            -1
        } else {
            0
        };
        if beyond != 0 {
            self.freeze_selection(&drag.run_id);
            if let Some(mode) = self.copy.as_mut() {
                let bounds = mode.screen_of(None).map_or((0, 0), vt100::Screen::size);
                if let SelectionScreen::Frozen(snapshot) = &mut mode.screen {
                    let before = snapshot.scroll_offset();
                    snapshot.scroll_by(beyond);
                    let moved = scrolled(before, snapshot.scroll_offset());
                    mode.session.shift_anchor(moved, bounds);
                }
            }
        }
```

`drag_selection`'s signature currently clamps `row` through `clamp_cell` before this point; keep the clamped value for the cursor and use the raw `row` argument for the test above.

- [ ] **Step 6: Make Esc unwind one level**

In `copy_key`, in the `KeyCode::Esc` arm, before the existing "leave copy mode" behaviour:

```rust
            KeyCode::Esc if mode.session.selecting() => {
                // One level at a time, as the board overlay and a half-typed title already
                // do: clearing a selection should not also close the mode behind it.
                mode.session.clear_selection();
                self.copy = Some(mode);
                return UiCommand::None;
            }
```

Add the method it needs to `src/copy.rs`, beside `begin_selection`:

```rust
    /// Drops the anchor, leaving the cursor where it is. The mode stays open.
    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS. `preference_from`'s old default is asserted in an existing test in `src/clipboard.rs` — update that assertion rather than deleting the test.

- [ ] **Step 8: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/dashboard.rs src/clipboard.rs src/copy.rs
git commit -m "feat: extend selections with shift, scroll past the edge, and reach the real clipboard"
```

---

### Task 4: The board spends its width where the cards are

`render_board_columns` divides width equally, so `DONE` gets a fifth of the pane however many of the other four columns are empty.

**Files:**
- Modify: `src/dashboard.rs:5502-5590` (`render_board_columns`)
- Modify: `src/dashboard.rs:5630-5680` (`card_lines`, the width-budget inconsistency)
- Test: `src/dashboard.rs` test module

**Interfaces:**
- Consumes: nothing.
- Produces: `fn column_widths(total: u16, labels: &[&str], counts: &[usize]) -> Vec<u16>` — Task 5 calls it with the *visible* counts.

- [ ] **Step 1: Write the failing tests**

```rust
/// The allocator's three invariants, over every shape a board can take.
///
/// The sum is the important one: `render_board_columns` lays columns out by accumulating
/// these widths into an x offset, so a vector that does not sum to the pane's width either
/// leaves a gap at the right edge or paints past it.
#[test]
fn column_widths_always_fill_the_pane_exactly() {
    for total in [0u16, 1, 7, 40, 79, 80, 100, 137, 200, 400] {
        for counts in [
            vec![0, 0, 0, 0, 0],
            vec![0, 0, 2, 0, 5],
            vec![1, 1, 1, 1, 1],
            vec![9, 0, 0, 0, 0],
            vec![0, 0, 0, 0, 1],
        ] {
            let labels: Vec<String> = ["BACKLOG · 0", "TODO · 0", "ACTIVE · 2", "REVIEW · 0", "DONE · 5"]
                .iter()
                .map(|value| (*value).to_owned())
                .collect();
            let borrowed: Vec<&str> = labels.iter().map(String::as_str).collect();
            let widths = column_widths(total, &borrowed, &counts);
            assert_eq!(widths.len(), counts.len(), "one width per column");
            assert_eq!(
                widths.iter().map(|w| u32::from(*w)).sum::<u32>(),
                u32::from(total),
                "widths must fill {total} exactly for {counts:?}, got {widths:?}"
            );
        }
    }
}

/// An empty column shrinks to a stub and a full one takes the room that frees.
#[test]
fn an_empty_column_stops_hoarding_width() {
    let labels = ["BACKLOG · 0", "TODO · 0", "ACTIVE · 2", "REVIEW · 0", "DONE · 5"];
    let widths = column_widths(100, &labels, &[0, 0, 2, 0, 5]);
    for empty in [0usize, 1, 3] {
        assert!(
            (8..=12).contains(&widths[empty]),
            "an empty column is a stub, not a fifth of the pane: {widths:?}"
        );
    }
    assert!(
        widths[4] > 100 / 5,
        "and DONE, which has the cards, gets more than its equal share: {widths:?}"
    );
    assert!(
        widths[4] > widths[2],
        "five cards should get more room than two: {widths:?}"
    );
}

/// A pane too narrow for stubs degrades to today's equal division rather than to a panic.
#[test]
fn a_pane_too_narrow_for_stubs_falls_back_to_equal_columns() {
    let labels = ["BACKLOG · 0", "TODO · 0", "ACTIVE · 2", "REVIEW · 0", "DONE · 5"];
    let widths = column_widths(30, &labels, &[0, 0, 2, 0, 5]);
    assert_eq!(widths.iter().map(|w| u32::from(*w)).sum::<u32>(), 30);
    assert!(
        widths.iter().all(|width| *width <= 8),
        "a narrow pane divides evenly rather than starving a column: {widths:?}"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib column_widths`
Expected: FAIL — `column_widths` is not defined.

- [ ] **Step 3: Write the allocator**

Add above `render_board_columns` in `src/dashboard.rs`:

```rust
/// An empty column keeps enough width to name itself and no more.
const STUB_MIN: u16 = 8;
const STUB_MAX: u16 = 12;
/// A column with cards in it is not worth drawing below this, so if the arithmetic cannot
/// give every occupied column this much, the whole board falls back to equal columns.
const FILLED_MIN: u16 = 18;

/// Widths for each board column, left to right, summing to exactly `total`.
///
/// Equal columns were the bug: five statuses meant `DONE` got a fifth of the pane however
/// many of the other four were empty, which on a half-screen board left about fourteen cells
/// for a title and ellipsised every card. Width goes where the cards are instead.
///
/// An empty column keeps a stub rather than disappearing. It is still a column — the cursor
/// walks into it, `<` and `>` move cards into it — and a board whose shape changes as cards
/// move is harder to aim at than one whose columns merely breathe.
fn column_widths(total: u16, labels: &[&str], counts: &[usize]) -> Vec<u16> {
    let columns = labels.len();
    if columns == 0 {
        return Vec::new();
    }
    debug_assert_eq!(columns, counts.len(), "one count per column");
    let equal_split = || {
        let each = total / columns as u16;
        let mut widths = vec![each; columns];
        // The remainder lands on the last column rather than being lost, which is what keeps
        // the sum exact for a width that does not divide evenly.
        widths[columns - 1] += total - each * columns as u16;
        widths
    };
    let filled: Vec<usize> = (0..columns).filter(|index| counts[*index] > 0).collect();
    if filled.is_empty() || filled.len() == columns {
        return equal_split();
    }
    let stub = |index: usize| -> u16 {
        u16::try_from(labels[index].chars().count() + 1)
            .unwrap_or(STUB_MAX)
            .clamp(STUB_MIN, STUB_MAX)
    };
    let stubs: u16 = (0..columns)
        .filter(|index| counts[*index] == 0)
        .map(stub)
        .sum();
    let Some(remainder) = total.checked_sub(stubs) else {
        return equal_split();
    };
    let floors = FILLED_MIN.saturating_mul(filled.len() as u16);
    let Some(surplus) = remainder.checked_sub(floors) else {
        return equal_split();
    };
    let cards: usize = filled.iter().map(|index| counts[*index]).sum();
    let mut widths = vec![0u16; columns];
    for index in 0..columns {
        if counts[index] == 0 {
            widths[index] = stub(index);
        }
    }
    // Floor division on every column but the last, which takes whatever is left. Each share
    // is at most `surplus * count / cards` and the counts sum to `cards`, so the running
    // total can never overrun the surplus and the subtraction below cannot underflow.
    let mut handed = 0u16;
    for (position, index) in filled.iter().enumerate() {
        let extra = if position + 1 == filled.len() {
            surplus - handed
        } else {
            u16::try_from(u32::from(surplus) * counts[*index] as u32 / cards as u32).unwrap_or(0)
        };
        handed += extra;
        widths[*index] = FILLED_MIN + extra;
    }
    widths
}
```

- [ ] **Step 4: Run the allocator tests**

Run: `cargo test --lib column_widths && cargo test --lib an_empty_column_stops && cargo test --lib a_pane_too_narrow`
Expected: PASS.

- [ ] **Step 5: Lay the columns out with it**

In `render_board_columns`, replace the `column_width` arithmetic and the `x` computation. The labels and counts must be built *before* the loop, because the allocator needs all of them at once:

```rust
    let mut counts = Vec::with_capacity(statuses.len());
    let mut labels = Vec::with_capacity(statuses.len());
    for status in statuses.iter() {
        let count = if status == ACTIVE_STATUS {
            active_entries(view, live).len()
        } else {
            view.cards(status).len()
        };
        labels.push(format!("{} · {count}", column_heading(status)));
        counts.push(count);
    }
    let borrowed: Vec<&str> = labels.iter().map(String::as_str).collect();
    let widths = column_widths(area.width, &borrowed, &counts);
    let mut x = area.x;
```

Then inside the loop, replace every use of `column_width` with `widths[index]`, and advance `x` at the end of each iteration:

```rust
        let column_width = widths[index];
        let width = usize::from(column_width.saturating_sub(1));
        // …existing body, unchanged except that `x` is the running offset…
        x += column_width;
```

Delete the now-dead `let x = area.x + column_width * index as u16;`.

- [ ] **Step 6: Fix the width-budget inconsistency in `card_lines`**

Two visually identical cards currently truncate at different lengths depending on whether an agent happens to be attached: the `Some(run)` branch subtracts `marker.len() + 3 + id.len()` from the budget and the `None` branch does not. Compute the budget once, above the match:

```rust
        let marker = if here { "›" } else { " " };
        let identifier = task.id.to_string();
        // One budget for both shapes. The badge branch spends its width on three extra cells
        // — a space, the glyph, a space — and the plain branch does not, but the *title* must
        // ellipsise at the same place either way or an agent attaching to a card silently
        // shortens its title.
        let prefix = marker.len() + 2 + identifier.len();
        let title_budget = width.saturating_sub(prefix + 2);
```

Use `title_budget` in both branches for the title's `ellipsise` call.

- [ ] **Step 7: Run the board tests and eyeball a rendered frame**

Run: `cargo test --lib board`
Expected: PASS. Some existing snapshot-style tests assert column positions; update the expected columns to the new layout, and where a test asserts a *truncated* title that now fits, assert the full title — that is the fix working, not a regression.

- [ ] **Step 8: Confirm the frame budget**

Run: `cargo test --release --lib render_breakdown -- --ignored --nocapture`
Expected: the `whole frame` rows stay within 10 % of 0.091 / 0.327 / 1.438 ms. The allocator runs once per board pane per frame and allocates one small `Vec`; if it shows up at all, note the number in the commit message.

- [ ] **Step 9: Commit**

```bash
git add src/dashboard.rs
git commit -m "fix: give a board column width in proportion to what is in it"
```

---

### Task 5: Cards that are finished stop shouting

Nothing ever leaves the board today: `load` reads every file in `kanban/tasks/` and there is no prune, expiry or delete path anywhere. A card moved to `done` is on the board forever.

**Files:**
- Modify: `src/board.rs:14-29` (`BoardTask`), `303-343` (`parse`), `780-850` (`BoardView`)
- Create: `src/board.rs` — `pub fn set_archived`
- Modify: `src/dashboard.rs:3204-3226` (`board_key`), `render_board_columns` (the archived footer)

**Interfaces:**
- Consumes: `column_widths` from Task 4.
- Produces:
  - `BoardTask { archived: bool, .. }`
  - `board::set_archived(directory: &Path, id: u64, archived: bool) -> Result<BoardTask, String>`
  - `BoardView::set_reveal(&mut self, reveal: bool)`, `BoardView::revealing(&self) -> bool`, `BoardView::archived_in(&self, status: &str) -> usize`

- [ ] **Step 1: Write the failing tests**

In `src/board.rs`'s test module:

```rust
/// Archiving adds the field when the file has none and rewrites it when it has one, and in
/// both cases leaves every other byte of the file alone.
#[test]
fn archiving_a_task_adds_or_rewrites_only_that_field() {
    let board = TestBoard::new();
    board.task(
        "001-a.md",
        "---\nid: 1\ntitle: 'Thing'\nstatus: done\npriority: medium\ntags:\n  - keep\n---\n\n# Outcome\n\nbody text\n",
    );
    let dir = board.tasks_dir();

    let archived = set_archived(&dir, 1, true).expect("archive");
    assert!(archived.archived);
    let text = fs::read_to_string(dir.join("001-a.md")).unwrap();
    assert!(text.contains("archived: true"), "{text}");
    assert!(text.contains("  - keep"), "the tags list must survive: {text}");
    assert!(text.contains("body text"), "the body must survive: {text}");
    assert_eq!(text.matches("archived:").count(), 1, "one field, not two: {text}");

    set_archived(&dir, 1, false).expect("unarchive");
    let text = fs::read_to_string(dir.join("001-a.md")).unwrap();
    assert!(text.contains("archived: false"), "{text}");
    assert_eq!(text.matches("archived:").count(), 1, "{text}");
    assert!(!load(&dir)[0].archived, "and it reads back as visible again");
}

/// A revealed board shows archived cards; a normal one does not, and says how many it is
/// holding back.
#[test]
fn archived_cards_are_hidden_until_revealed_and_counted_while_they_are() {
    let mut view = BoardView::new(vec![
        task_with(1, "done", false),
        task_with(2, "done", true),
        task_with(3, "done", true),
    ]);
    assert_eq!(view.cards("done").len(), 1);
    assert_eq!(view.archived_in("done"), 2);
    view.set_reveal(true);
    assert_eq!(view.cards("done").len(), 3);
    assert_eq!(view.archived_in("done"), 2, "the count is what is archived, revealed or not");
}
```

Add a `task_with(id, status, archived)` helper beside the module's existing `task` helper, and a `TestBoard::tasks_dir()` accessor if one is not already there — the module already has a `TestBoard` used by `set_status`'s tests, so follow its shape exactly.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib archiving_a_task && cargo test --lib archived_cards_are_hidden`
Expected: FAIL — `set_archived` and `archived` do not exist.

- [ ] **Step 3: Carry the field on the task**

In `src/board.rs`, add to `BoardTask`:

```rust
    /// Retired from the board without being deleted from the repository.
    ///
    /// The board had no terminal state: a card moved to `done` stayed in that column
    /// forever, because nothing prunes, expires or deletes. Absent means false, so every task
    /// file that predates this field — which is all of them — reads back exactly as before.
    pub archived: bool,
```

In `parse`, add the field beside the other three:

```rust
    let (mut id, mut title, mut status, mut priority, mut archived) =
        (None, None, None, None, false);
```
```rust
            "archived" => archived = value.eq_ignore_ascii_case("true"),
```
```rust
        archived,
```

Add `archived: false` to the `BoardTask` literal `create` returns, and to the one `set_status` returns (`..task` already carries it there — confirm rather than duplicate).

- [ ] **Step 4: Write `set_archived`**

Beside `set_status` in `src/board.rs`:

```rust
/// Retires a task from the board, or brings it back, rewriting only its `archived:` line.
///
/// Follows `set_status`'s shape and for the same reason — a board is shared with `kanban-md`,
/// with editors, and with whoever commits to it — with one difference: the field may not be
/// there at all, since every task written before this existed has no `archived:` line. So an
/// absent field is *inserted* immediately before the closing fence rather than treated as an
/// error, which is what `set_status` does with a missing `status:`.
pub fn set_archived(directory: &Path, id: u64, archived: bool) -> Result<BoardTask, String> {
    let task = load(directory)
        .into_iter()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    let text = fs::read_to_string(&task.file)
        .map_err(|error| format!("could not read the task: {error}"))?;
    let mut rewritten = String::with_capacity(text.len() + 20);
    let mut in_front_matter = false;
    let mut replaced = false;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim() == "---";
        if fence && in_front_matter && !replaced {
            // The closing fence, and no field was found on the way here: put one in above it,
            // where the rest of the front matter is.
            rewritten.push_str(&format!("archived: {archived}\n"));
            replaced = true;
        }
        if fence {
            in_front_matter = index == 0 || !in_front_matter;
        }
        if in_front_matter && !replaced && line.starts_with("archived:") {
            rewritten.push_str(&format!("archived: {archived}"));
            replaced = true;
        } else {
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    if !replaced {
        return Err(format!("task {id} has no front matter to archive"));
    }
    fs::write(&task.file, rewritten)
        .map_err(|error| format!("could not write the task: {error}"))?;
    Ok(BoardTask { archived, ..task })
}
```

- [ ] **Step 5: Filter and count in `BoardView`**

Add a `reveal: bool` field to `BoardView`, defaulting to `false` in `new`. Then:

```rust
    /// The cards in one column, in board order, minus anything archived unless revealed.
    ///
    /// Filtering *here* rather than at load is deliberate: `column_targets` builds the cursor's
    /// walk from this same call, so the cursor cannot disagree with the grid about how many
    /// cards a column has. A second filter anywhere else is how those two drift apart.
    pub fn cards(&self, status: &str) -> Vec<&BoardTask> {
        self.tasks
            .iter()
            .filter(|task| task.status == status)
            .filter(|task| self.reveal || !task.archived)
            .collect()
    }

    /// How many cards this column is holding back, revealed or not.
    pub fn archived_in(&self, status: &str) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == status && task.archived)
            .count()
    }

    pub fn set_reveal(&mut self, reveal: bool) {
        self.reveal = reveal;
    }

    pub fn revealing(&self) -> bool {
        self.reveal
    }
```

- [ ] **Step 6: Bind the three keys**

In `src/dashboard.rs`'s `board_key`, add arms to the `match key.code` block. `h`, `j`, `k` and `l` are cursor motion, so the obvious `h` for "hide" is unavailable:

```rust
            KeyCode::Char('v') => {
                let revealing = board.view.revealing();
                board.view.set_reveal(!revealing);
            }
            KeyCode::Char('a') => return self.archive_selected_task(),
            KeyCode::Char('A') => return self.archive_finished_tasks(),
```

And the two methods, beside `shift_task`:

```rust
    /// Archives the selected card, or brings it back if the board is revealing them.
    fn archive_selected_task(&mut self) -> UiCommand {
        let Some(board) = self.board.as_ref() else {
            return UiCommand::None;
        };
        if !board.writable {
            self.error = Some(
                "this is the repository's board — retire tasks with kanban-md so its history \
                 stays the repository's"
                    .into(),
            );
            return UiCommand::None;
        }
        let (Some(directory), Some(task)) = (self.board_dir.clone(), self.selected_task_id())
        else {
            return UiCommand::None;
        };
        let archived = !board.view.revealing();
        match crate::board::set_archived(&directory, task, archived) {
            Ok(_) => UiCommand::LoadBoard,
            Err(message) => {
                self.error = Some(message);
                UiCommand::None
            }
        }
    }

    /// Archives every card in `done` at once, which is the answer to a column that has been
    /// accumulating since the board was made.
    fn archive_finished_tasks(&mut self) -> UiCommand {
        let Some(board) = self.board.as_ref() else {
            return UiCommand::None;
        };
        if !board.writable {
            self.error = Some(
                "this is the repository's board — retire tasks with kanban-md so its history \
                 stays the repository's"
                    .into(),
            );
            return UiCommand::None;
        }
        let Some(directory) = self.board_dir.clone() else {
            return UiCommand::None;
        };
        let finished: Vec<u64> = board
            .view
            .cards("done")
            .iter()
            .filter(|task| !task.archived)
            .map(|task| task.id)
            .collect();
        if finished.is_empty() {
            self.error = Some("nothing in done to archive".into());
            return UiCommand::None;
        }
        let count = finished.len();
        for id in finished {
            if let Err(message) = crate::board::set_archived(&directory, id, true) {
                self.error = Some(message);
                return UiCommand::LoadBoard;
            }
        }
        self.error = Some(format!("archived {count} finished tasks"));
        UiCommand::LoadBoard
    }
```

Add `selected_task_id(&self) -> Option<u64>` if the module has no equivalent; `dispatch_selected_task` already resolves the cursor to a task, so factor that lookup out rather than writing a second one.

- [ ] **Step 7: Draw the count**

In `render_board_columns`, after the column's card paragraph, when `view.archived_in(status) > 0`, paint one row at the bottom of the column:

```rust
        let hidden = view.archived_in(status);
        if hidden > 0 && column_width > STUB_MAX {
            let note = if view.revealing() {
                format!("{hidden} archived · v hides")
            } else {
                format!("{hidden} archived · v reveals")
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    ellipsise(&note, width),
                    Style::default().fg(theme.muted),
                )),
                Rect::new(x, area.bottom().saturating_sub(1), column_width.saturating_sub(1), 1),
            );
        }
```

Render a revealed archived card in `theme.muted` rather than the normal card style, so a revealed board never looks like a normal one — add the branch inside `card_lines` where `card_style` is chosen.

- [ ] **Step 8: Run the tests**

Run: `cargo test`
Expected: PASS. Any existing test constructing a `BoardTask` literal now needs `archived: false`; add it rather than switching those literals to `..Default::default()`.

- [ ] **Step 9: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/board.rs src/dashboard.rs
git commit -m "feat: let a finished card be archived, so done stops accumulating forever"
```

---

### Task 6: The menu model — items, targets, and placement

Placement is arithmetic and worth testing on its own, before any of it is wired to a pointer. This task builds and tests the model; Task 7 attaches it to the mouse.

**Files:**
- Modify: `src/dashboard.rs` — new types near `Divider`/`PaneDrag` (around line 634)
- Test: `src/dashboard.rs` test module

**Interfaces:**
- Consumes: `UiCommand::Send` from Task 1.
- Produces:
  - `enum MenuTarget { Pane(String), Tab(String), SidebarWorkspace(String), SidebarAgent(String), BoardCard(u64), Canvas }`
  - `enum MenuEntry { Item(MenuItem), Separator }`
  - `struct MenuItem { label: &'static str, key: Option<&'static str>, action: MenuAction, enabled: bool }`
  - `struct ContextMenu { target: MenuTarget, entries: Vec<MenuEntry>, cursor: usize }`
  - `ContextMenu::place(&self, origin: (u16, u16), frame: Rect) -> Rect`
  - `ContextMenu::move_cursor(&mut self, delta: isize)`

- [ ] **Step 1: Write the failing tests**

```rust
/// A menu is never drawn partly off-screen: it flips rather than clipping, and clamps rather
/// than flipping when it cannot fit either way. All four corners, because each one exercises
/// a different pair of branches.
#[test]
fn a_menu_stays_inside_the_frame_from_every_corner() {
    let menu = ContextMenu::for_target(MenuTarget::Pane("a".into()), true);
    let frame = Rect::new(0, 0, 80, 24);
    for origin in [(1u16, 1u16), (78, 1), (1, 22), (78, 22), (40, 12)] {
        let placed = menu.place(origin, frame);
        assert!(
            placed.x >= frame.x && placed.right() <= frame.right(),
            "menu ran off the side from {origin:?}: {placed:?}"
        );
        assert!(
            placed.y >= frame.y && placed.bottom() <= frame.bottom(),
            "menu ran off the bottom from {origin:?}: {placed:?}"
        );
        assert!(placed.width > 0 && placed.height > 0, "{placed:?}");
    }
}

/// A frame smaller than the menu still yields a rectangle inside it.
#[test]
fn a_menu_too_tall_for_the_frame_is_clamped_to_it() {
    let menu = ContextMenu::for_target(MenuTarget::Pane("a".into()), true);
    let frame = Rect::new(0, 0, 12, 5);
    let placed = menu.place((6, 3), frame);
    assert!(placed.right() <= frame.right() && placed.bottom() <= frame.bottom(), "{placed:?}");
}

/// The cursor skips separators in both directions and stops at the ends rather than wrapping
/// into one. A cursor that can land on a separator is a menu where Enter does nothing.
#[test]
fn the_menu_cursor_never_lands_on_a_separator() {
    let mut menu = ContextMenu::for_target(MenuTarget::Pane("a".into()), true);
    for _ in 0..40 {
        menu.move_cursor(1);
        assert!(
            matches!(menu.entries[menu.cursor], MenuEntry::Item(_)),
            "cursor landed on a separator at {}", menu.cursor
        );
    }
    for _ in 0..40 {
        menu.move_cursor(-1);
        assert!(matches!(menu.entries[menu.cursor], MenuEntry::Item(_)));
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib a_menu_stays_inside`
Expected: FAIL — `ContextMenu` is not defined.

- [ ] **Step 3: Define the model**

```rust
/// What a right-click landed on. The menu's contents are a function of this and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuTarget {
    Pane(String),
    Tab(String),
    SidebarWorkspace(String),
    SidebarAgent(String),
    BoardCard(u64),
    Canvas,
}

/// What an item does when it is taken.
///
/// Every variant wraps something Dock already does. That is the rule this enum exists to
/// enforce: a menu is a second route to existing behaviour, never a place where a feature
/// lives that has no other way in — those are the features nobody finds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    Pane(PaneCommand),
    CopySelection,
    PasteLastCopy,
    ArchiveCard(u64),
    MoveCard(u64, isize),
    DispatchCard(u64),
    FocusPane(String),
    SwitchWorkspace(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuItem {
    label: &'static str,
    /// The key that also does this, shown right-aligned. `None` for things only the pointer
    /// can express.
    key: Option<&'static str>,
    action: MenuAction,
    enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuEntry {
    Item(MenuItem),
    Separator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextMenu {
    target: MenuTarget,
    entries: Vec<MenuEntry>,
    cursor: usize,
}
```

- [ ] **Step 4: Build the menus**

```rust
impl ContextMenu {
    /// The menu for one target. `has_selection` greys out the items that need one rather than
    /// hiding them: an item that appears and disappears is one a person cannot learn.
    fn for_target(target: MenuTarget, has_selection: bool) -> Self {
        let entries = match &target {
            MenuTarget::Pane(_) => vec![
                MenuEntry::Item(MenuItem { label: "Copy selection", key: Some("y"), action: MenuAction::CopySelection, enabled: has_selection }),
                MenuEntry::Item(MenuItem { label: "Paste last copy", key: Some("middle-click"), action: MenuAction::PasteLastCopy, enabled: true }),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem { label: "Split right", key: Some("Ctrl+B v"), action: MenuAction::Pane(PaneCommand::Split(SplitAxis::Vertical)), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Split down", key: Some("Ctrl+B h"), action: MenuAction::Pane(PaneCommand::Split(SplitAxis::Horizontal)), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Zoom", key: Some("Ctrl+B z"), action: MenuAction::Pane(PaneCommand::Zoom), enabled: true }),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem { label: "Rename", key: Some("Ctrl+B r"), action: MenuAction::Pane(PaneCommand::Rename), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Restart", key: Some("Ctrl+B R"), action: MenuAction::Pane(PaneCommand::Respawn), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Close pane", key: Some("Ctrl+B x"), action: MenuAction::Pane(PaneCommand::Close), enabled: true }),
            ],
            MenuTarget::Tab(id) | MenuTarget::SidebarWorkspace(id) => vec![
                MenuEntry::Item(MenuItem { label: "Switch to", key: None, action: MenuAction::SwitchWorkspace(id.clone()), enabled: true }),
                MenuEntry::Item(MenuItem { label: "New workspace", key: Some("Ctrl+B n"), action: MenuAction::Pane(PaneCommand::NewWorkspace), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Rename", key: Some("Ctrl+B r"), action: MenuAction::Pane(PaneCommand::Rename), enabled: true }),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem { label: "Close workspace", key: Some("Ctrl+B X"), action: MenuAction::Pane(PaneCommand::CloseWorkspace), enabled: true }),
            ],
            MenuTarget::SidebarAgent(run_id) => vec![
                MenuEntry::Item(MenuItem { label: "Focus its pane", key: None, action: MenuAction::FocusPane(run_id.clone()), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Resume", key: Some("Ctrl+B a"), action: MenuAction::Pane(PaneCommand::ResumeAgent), enabled: true }),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem { label: "Restart", key: Some("Ctrl+B R"), action: MenuAction::Pane(PaneCommand::Respawn), enabled: true }),
            ],
            MenuTarget::BoardCard(id) => vec![
                MenuEntry::Item(MenuItem { label: "Move left", key: Some("<"), action: MenuAction::MoveCard(*id, -1), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Move right", key: Some(">"), action: MenuAction::MoveCard(*id, 1), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Dispatch", key: Some("Enter"), action: MenuAction::DispatchCard(*id), enabled: true }),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem { label: "Archive", key: Some("a"), action: MenuAction::ArchiveCard(*id), enabled: true }),
            ],
            MenuTarget::Canvas => vec![
                MenuEntry::Item(MenuItem { label: "New workspace", key: Some("Ctrl+B n"), action: MenuAction::Pane(PaneCommand::NewWorkspace), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Task board", key: Some("Ctrl+B k"), action: MenuAction::Pane(PaneCommand::Board), enabled: true }),
                MenuEntry::Item(MenuItem { label: "What changed", key: Some("Ctrl+B g"), action: MenuAction::Pane(PaneCommand::Git), enabled: true }),
                MenuEntry::Item(MenuItem { label: "Every key", key: Some("Ctrl+B ?"), action: MenuAction::Pane(PaneCommand::Help), enabled: true }),
            ],
        };
        let mut menu = Self { target, entries, cursor: 0 };
        // The first entry is an item in every menu above, but the cursor is normalised anyway
        // so a menu edited later cannot open with Enter pointing at a rule.
        if matches!(menu.entries.first(), Some(MenuEntry::Separator)) {
            menu.move_cursor(1);
        }
        menu
    }
}
```

- [ ] **Step 5: Write placement and cursor movement**

```rust
impl ContextMenu {
    /// Widest label plus its key, plus borders and the gap between the two columns.
    fn width(&self) -> u16 {
        let widest = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => {
                    Some(item.label.chars().count() + item.key.map_or(0, |key| key.chars().count() + 3))
                }
                MenuEntry::Separator => None,
            })
            .max()
            .unwrap_or(8);
        u16::try_from(widest + 4).unwrap_or(u16::MAX)
    }

    fn height(&self) -> u16 {
        u16::try_from(self.entries.len() + 2).unwrap_or(u16::MAX)
    }

    /// Where to draw, given where the pointer was and how big the frame is.
    ///
    /// Down-and-right of the pointer by default, because that is where every pointer menu
    /// goes and where the hand expects it. Flipped to the other side when it would overflow,
    /// which keeps the pointer on a corner of the menu rather than inside it; clamped when it
    /// cannot fit on either side, because a menu drawn partly off-screen is a menu with items
    /// nobody can reach.
    fn place(&self, origin: (u16, u16), frame: Rect) -> Rect {
        let width = self.width().min(frame.width.max(1));
        let height = self.height().min(frame.height.max(1));
        let x = if origin.0 + width <= frame.right() {
            origin.0
        } else {
            origin.0.saturating_sub(width)
        };
        let y = if origin.1 + height <= frame.bottom() {
            origin.1
        } else {
            origin.1.saturating_sub(height)
        };
        let x = x.clamp(frame.x, frame.right().saturating_sub(width));
        let y = y.clamp(frame.y, frame.bottom().saturating_sub(height));
        Rect::new(x, y, width, height)
    }

    /// Moves the cursor, stepping over separators and stopping at the ends.
    fn move_cursor(&mut self, delta: isize) {
        let count = self.entries.len();
        if count == 0 {
            return;
        }
        let mut index = self.cursor;
        for _ in 0..count {
            let next = index as isize + delta;
            if next < 0 || next as usize >= count {
                return;
            }
            index = next as usize;
            if matches!(self.entries[index], MenuEntry::Item(_)) {
                self.cursor = index;
                return;
            }
        }
    }
}
```

- [ ] **Step 6: Run the placement tests**

Run: `cargo test --lib a_menu_stays_inside && cargo test --lib a_menu_too_tall && cargo test --lib the_menu_cursor_never`
Expected: PASS.

- [ ] **Step 7: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/dashboard.rs
git commit -m "feat: model the pointer menu — targets, items, and placement that stays on screen"
```

---

### Task 7: Right-click opens it, everywhere it means something

**Files:**
- Modify: `src/dashboard.rs:561-593` (`OverlayKind`, `OVERLAY_ORDER`)
- Modify: `src/dashboard.rs:5164-5167` (the right-click arm of `mouse`)
- Modify: `src/dashboard.rs` — overlay draw and key-routing sites derived from `OVERLAY_ORDER`

**Interfaces:**
- Consumes: everything Task 6 produced, plus `UiCommand::Send` from Task 1.
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing test**

```rust
/// Right-click opens a menu for whatever is under it, and middle-click still pastes.
///
/// Both halves in one test because they are one decision: the menu took right-click, so the
/// paste that used to live there had to keep a home, and a change that quietly dropped it
/// would pass a test that only checked the menu.
#[test]
fn right_click_opens_a_menu_and_middle_click_still_pastes() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let a = dashboard.pane_areas["a"];

    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: a.x + 2,
        row: a.y + 2,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(dashboard.menu.as_ref().map(|menu| &menu.target), Some(MenuTarget::Pane(id)) if id == "a"),
        "a right-click in a pane opens that pane's menu"
    );

    // Esc dismisses, and the menu must not have swallowed the pane's focus on the way.
    dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(dashboard.menu.is_none(), "Esc dismisses the menu");

    let pasted = dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Middle),
        column: a.x + 2,
        row: a.y + 2,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        !matches!(pasted, UiCommand::None) || dashboard.error.is_some(),
        "middle-click must still reach the paste path"
    );
}

/// A click outside the menu dismisses it rather than activating whatever it landed on.
#[test]
fn a_click_outside_the_menu_only_dismisses_it() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let a = dashboard.pane_areas["a"];
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: a.x + 2,
        row: a.y + 2,
        modifiers: KeyModifiers::NONE,
    });
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let focused_before = dashboard.workspace().unwrap().focused_pane_id.clone();
    let b = dashboard.pane_areas["b"];
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: b.x + 1,
        row: b.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(dashboard.menu.is_none(), "the click dismissed the menu");
    assert_eq!(
        dashboard.workspace().unwrap().focused_pane_id,
        focused_before,
        "and did not also focus the pane it landed on"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib right_click_opens_a_menu`
Expected: FAIL — `Dashboard` has no `menu` field.

- [ ] **Step 3: Add the surface**

Add three fields to `Dashboard`, beside `copy`:

```rust
    menu: Option<ContextMenu>,
    /// Where the pointer was when the menu was opened. Kept apart from the menu itself
    /// because `place` is a pure function of it and the frame, and because `Paste last copy`
    /// pastes at the point that was right-clicked rather than wherever the menu ended up.
    menu_origin: (u16, u16),
    /// The rectangle the last frame drew the menu into, so a click can be tested against what
    /// is on screen rather than against a rectangle recomputed from a stale frame size.
    menu_area: Rect,
```

Then the ninth overlay:

```rust
pub enum OverlayKind {
    Help,
    Rename,
    LaunchForm,
    Picker,
    Review,
    Board,
    Git,
    Copy,
    ContextMenu,
}

const OVERLAY_ORDER: [OverlayKind; 9] = [
    OverlayKind::Help,
    OverlayKind::Rename,
    OverlayKind::LaunchForm,
    OverlayKind::Picker,
    OverlayKind::Review,
    OverlayKind::Board,
    OverlayKind::Git,
    OverlayKind::Copy,
    // Last, so it draws over whatever it was opened on top of and gets first refusal on keys
    // while it is open. A menu is the most transient surface Dock has.
    OverlayKind::ContextMenu,
];
```

Add `OverlayKind::ContextMenu => self.menu.is_some()` to the `overlay_open` match at `src/dashboard.rs:1109`.

- [ ] **Step 4: Open it from the pointer**

Replace the right-click arm of `mouse`:

```rust
            MouseEventKind::Down(MouseButton::Middle) => {
                self.paste_last_copied(event.column, event.row)
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Re-targets rather than stacking: a right-click while a menu is open is a
                // request for a different menu, never for two.
                let target = self.menu_target_at(event.column, event.row);
                let has_selection = self
                    .copy
                    .as_ref()
                    .is_some_and(|mode| mode.session.selecting());
                self.menu = Some(ContextMenu::for_target(target, has_selection));
                self.menu_origin = (event.column, event.row);
                UiCommand::None
            }
```

Write `menu_target_at`, testing the recorded rectangles in the same order the frame draws them — board card, then tab strip, then sidebar row, then pane, then canvas:

```rust
    /// What is under the pointer, most specific first. The rectangles are the ones the last
    /// frame recorded, which is what makes this agree with what the user is looking at.
    fn menu_target_at(&self, column: u16, row: u16) -> MenuTarget {
        if let Some(id) = self.board_card_at(column, row) {
            return MenuTarget::BoardCard(id);
        }
        if let Some(workspace_id) = self.tab_at(column, row) {
            return MenuTarget::Tab(workspace_id);
        }
        if let Some(target) = self.sidebar_target_at(column, row) {
            return target;
        }
        if let Some(pane_id) = self.pane_at(column, row) {
            return MenuTarget::Pane(pane_id);
        }
        MenuTarget::Canvas
    }
```

Each of those four helpers has an existing left-click counterpart in `mouse` that already resolves the same rectangle — `tab_strip_area` and the per-tab rects, the sidebar's `sidebar_line_area`, `pane_areas`, and the board's card rows. Factor each out of the left-click arm into a named method and call it from both places rather than writing a second resolver; two lookups that can disagree about what is under the pointer is the bug this avoids.

- [ ] **Step 5: Route keys and clicks while it is open**

At the top of `key`, before every other overlay, and at the top of `mouse`:

```rust
        if let Some(menu) = self.menu.as_mut() {
            match key.code {
                KeyCode::Esc => self.menu = None,
                KeyCode::Up => menu.move_cursor(-1),
                KeyCode::Down => menu.move_cursor(1),
                KeyCode::Enter => return self.take_menu_item(),
                // Typing an item's own key takes it. The key column is not decoration — it is
                // the answer to "how do I do this without the menu next time", and a menu that
                // prints a key it will not accept is teaching one thing and doing another.
                KeyCode::Char(typed) => {
                    let matched = menu.entries.iter().position(|entry| match entry {
                        MenuEntry::Item(item) => item.enabled
                            && item
                                .key
                                .is_some_and(|key| key.ends_with(typed) && key.len() <= 2),
                        MenuEntry::Separator => false,
                    });
                    if let Some(index) = matched {
                        menu.cursor = index;
                        return self.take_menu_item();
                    }
                }
                _ => {}
            }
            return UiCommand::None;
        }
```

```rust
        if self.menu.is_some() {
            let area = self.menu_area;
            match event.kind {
                MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                    if let Some(index) = self.menu_row_at(event.column, event.row)
                        && let Some(menu) = self.menu.as_mut()
                        && matches!(menu.entries[index], MenuEntry::Item(_))
                    {
                        menu.cursor = index;
                    }
                    return UiCommand::None;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Plain arithmetic rather than `Rect::contains`, which would need a
                    // `layout::Position` import this file does not have; the module already
                    // tests rectangles this way in `grid_cell` and `clamp_cell`.
                    let inside = event.column >= area.x
                        && event.column < area.right()
                        && event.row >= area.y
                        && event.row < area.bottom();
                    if inside {
                        if let Some(index) = self.menu_row_at(event.column, event.row)
                            && let Some(menu) = self.menu.as_mut()
                            && matches!(menu.entries[index], MenuEntry::Item(_))
                        {
                            menu.cursor = index;
                            return self.take_menu_item();
                        }
                        return UiCommand::None;
                    }
                    // Outside: dismiss and stop. A click that both closes a menu and does
                    // whatever was underneath it is a click that does something the user did
                    // not ask for — they were aiming at the menu.
                    self.menu = None;
                    return UiCommand::None;
                }
                MouseEventKind::Down(MouseButton::Right) => {}
                _ => return UiCommand::None,
            }
        }
```

`self.menu_area` is recorded by the render pass, exactly as `pane_inner_areas` is.

- [ ] **Step 6: Take an item**

```rust
    /// Runs the item under the cursor and closes the menu.
    fn take_menu_item(&mut self) -> UiCommand {
        let Some(menu) = self.menu.take() else {
            return UiCommand::None;
        };
        let MenuEntry::Item(item) = &menu.entries[menu.cursor] else {
            return UiCommand::None;
        };
        if !item.enabled {
            return UiCommand::None;
        }
        match item.action.clone() {
            MenuAction::Pane(command) => self.pane_command(command),
            MenuAction::CopySelection => {
                self.copy_pointer_selection();
                UiCommand::None
            }
            MenuAction::PasteLastCopy => {
                let (column, row) = self.menu_origin;
                self.paste_last_copied(column, row)
            }
            MenuAction::ArchiveCard(id) => self.archive_card(id),
            MenuAction::MoveCard(id, delta) => self.move_card(id, delta),
            MenuAction::DispatchCard(id) => self.dispatch_card(id),
            MenuAction::FocusPane(run_id) => self.focus_run(&run_id),
            MenuAction::SwitchWorkspace(id) => self.switch_workspace(&id),
        }
    }
```

`pane_command` is the existing dispatcher at `src/dashboard.rs:2812`. `archive_card`, `move_card` and `dispatch_card` are the by-id forms of the cursor-driven methods Task 5 and `shift_task`/`dispatch_selected_task` already have — factor the id out of each rather than duplicating the body.

- [ ] **Step 7: Draw it**

Add a `render_context_menu` called from the overlay draw pass, last:

```rust
    /// A bordered popup at the pointer. Only backgrounds and text — no shadow, no animation:
    /// this is a terminal and the frame it is drawn over is a real thing the user is reading.
    fn render_context_menu(&mut self, frame: &mut Frame) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let area = menu.place(self.menu_origin, frame.area());
        self.menu_area = area;
        frame.render_widget(Clear, area);
        let mut lines = Vec::with_capacity(menu.entries.len());
        let inner_width = usize::from(area.width.saturating_sub(2));
        for (index, entry) in menu.entries.iter().enumerate() {
            lines.push(match entry {
                MenuEntry::Separator => Line::styled(
                    "─".repeat(inner_width),
                    Style::default().fg(self.theme.border),
                ),
                MenuEntry::Item(item) => {
                    let here = index == menu.cursor;
                    let colour = if !item.enabled {
                        self.theme.border
                    } else if here {
                        self.theme.surface
                    } else {
                        self.theme.text
                    };
                    let mut style = Style::default().fg(colour);
                    if here && item.enabled {
                        style = style.bg(self.theme.accent);
                    }
                    let key = item.key.unwrap_or("");
                    let gap = inner_width
                        .saturating_sub(item.label.chars().count() + key.chars().count() + 2);
                    Line::styled(
                        format!(" {}{}{} ", item.label, " ".repeat(gap), key),
                        style,
                    )
                }
            });
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .border_style(Style::default().fg(self.theme.border_focused))
                    .style(Style::default().bg(self.theme.panel)),
            ),
            area,
        );
    }
```

`self.theme.panel` arrives in Task 9; until then use `self.theme.surface` and change it there.

- [ ] **Step 8: Run the tests**

Run: `cargo test`
Expected: PASS. The `OVERLAY_ORDER` length changed from 8 to 9 — any test asserting the old length is asserting the thing that just changed; update it.

- [ ] **Step 9: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/dashboard.rs
git commit -m "feat: right-click a pane, tab, sidebar row or card for what it can do"
```

---

### Task 8: A sidebar that gets out of the way

**Files:**
- Modify: `src/dashboard.rs:1204-1216` (the sidebar/canvas split), `1670-1800` (`render_sidebar`)
- Modify: `src/keymap.rs:154-215` (`command_for`), `PaneCommand`

**Interfaces:**
- Consumes: nothing.
- Produces: `PaneCommand::ToggleSidebar`, `enum SidebarState { Full, Rail }`.

- [ ] **Step 1: Write the failing tests**

```rust
/// `Ctrl+B s` toggles the sidebar between full and a rail, and the rail keeps the one thing
/// the sidebar is for: which agents want something.
#[test]
fn the_sidebar_collapses_to_a_rail_that_still_shows_who_needs_you() {
    let mut dashboard = fixture_dashboard();
    dashboard.agents.insert("run_a".into(), (Some(AgentKind::Claude), AgentState::Blocked));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    let wide = dashboard.pane_areas["a"].width;

    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    assert!(
        dashboard.pane_areas["a"].width > wide,
        "collapsing the sidebar must give the canvas the width"
    );
    let painted = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
    assert!(
        painted.contains(AgentState::Blocked.glyph()),
        "a rail with no state glyph is a rail that has thrown away its only job"
    );
    assert!(
        !painted.contains("WORKSPACES"),
        "but the headings are gone: {painted:?}"
    );

    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    assert_eq!(dashboard.pane_areas["a"].width, wide, "and it comes back");
}

/// A terminal too narrow for both rails itself, so the canvas is never mostly sidebar.
#[test]
fn a_narrow_terminal_rails_the_sidebar_on_its_own() {
    let mut dashboard = fixture_dashboard();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| dashboard.render(frame)).unwrap();
    assert!(
        dashboard.pane_areas["a"].width + dashboard.pane_areas["b"].width > 80 - 28,
        "80 columns is under the threshold, so the sidebar should have railed itself"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib the_sidebar_collapses_to_a_rail`
Expected: FAIL — no `ToggleSidebar`.

- [ ] **Step 3: Bind the key**

In `src/keymap.rs`, add to `PaneCommand`:

```rust
    /// Collapse the sidebar to a rail, or bring it back.
    ToggleSidebar,
```

and to `command_for`, with the reasoning the file's other entries carry:

```rust
        // `s` for sidebar. `b` would be the obvious letter and is unavailable: the prefix is
        // Ctrl+B, so pressing it twice already means "send a literal Ctrl+B".
        KeyCode::Char('s') => PaneCommand::ToggleSidebar,
```

- [ ] **Step 4: Hold the state and split on it**

Add to `Dashboard`:

```rust
/// How much of the sidebar is showing.
///
/// A rail rather than nothing, because the sidebar's one irreplaceable job is saying that an
/// agent wants you, and a collapse that takes that away has traded a capability for width.
/// Three cells hold one state glyph per agent in the same blocked-first order the full list
/// uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SidebarState {
    #[default]
    Full,
    Rail,
}
```
```rust
    sidebar: SidebarState,
    /// True once the user has toggled it by hand, so the automatic rule below stops
    /// overriding a deliberate choice until the terminal is resized again.
    sidebar_chosen: bool,
    /// The rectangle the rail was last drawn into, so a click on it can expand it.
    sidebar_rail_area: Rect,
```

and the method `main.rs` calls on a resize:

```rust
    /// Lets the automatic rail rule take over again.
    ///
    /// A deliberate toggle outranks the automatic rule, but only until the geometry the rule
    /// is about actually changes — otherwise one collapse early in a session would mean a
    /// dashboard dragged to full screen kept a rail nobody wanted any more.
    pub fn forget_sidebar_choice(&mut self) {
        self.sidebar_chosen = false;
    }
```

In `render`, replace the fixed width:

```rust
        const FULL_SIDEBAR: u16 = 28;
        const RAIL_SIDEBAR: u16 = 3;
        // A full sidebar that would leave the canvas under sixty columns is a sidebar taking
        // more than it gives, so below that the rail is automatic — until the user says
        // otherwise, at which point their choice stands.
        if !self.sidebar_chosen {
            self.sidebar = if body.width < FULL_SIDEBAR + 60 {
                SidebarState::Rail
            } else {
                SidebarState::Full
            };
        }
        let sidebar_width = body.width.min(match self.sidebar {
            SidebarState::Full => FULL_SIDEBAR,
            SidebarState::Rail => RAIL_SIDEBAR,
        });
```

Handle the command in `pane_command`:

```rust
            PaneCommand::ToggleSidebar => {
                self.sidebar = match self.sidebar {
                    SidebarState::Full => SidebarState::Rail,
                    SidebarState::Rail => SidebarState::Full,
                };
                self.sidebar_chosen = true;
                UiCommand::None
            }
```

Clear `sidebar_chosen` on `Event::Resize` in `main.rs`'s event match, so the automatic rule resumes after a resize:

```rust
            Event::Resize(_, _) => {
                dashboard.forget_sidebar_choice();
                UiCommand::None
            }
```

- [ ] **Step 5: Draw the rail**

At the top of `render_sidebar`, branch:

```rust
        if matches!(self.sidebar, SidebarState::Rail) {
            return self.render_sidebar_rail(frame, area);
        }
```

```rust
    /// One glyph per agent, blocked first, in the order the full roster sorts.
    fn render_sidebar_rail(&mut self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(""), Line::from("")];
        for entry in self.agent_roster().into_iter().take(usize::from(area.height)) {
            lines.push(Line::styled(
                format!(" {}", entry.state.glyph()),
                Style::default().fg(self.theme.agent(entry.state)),
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
        self.sidebar_rail_area = area;
    }
```

Record `sidebar_rail_area` and, in `mouse`, expand on a left-click inside it. Use the roster type `agent_roster` already returns rather than inventing a new one.

- [ ] **Step 6: Read the initial state from the environment**

In `Dashboard`'s construction path, following `DOCK_CLIPBOARD`'s precedent:

```rust
    /// `DOCK_SIDEBAR=full|rail` picks the state a dashboard opens in. An environment variable
    /// rather than a config file for the reason `clipboard::preference` gives: this is a
    /// property of the terminal a dashboard was started in, not of the repository it is
    /// looking at.
    fn sidebar_from_env() -> (SidebarState, bool) {
        match std::env::var("DOCK_SIDEBAR").ok().as_deref().map(str::trim) {
            Some("rail") => (SidebarState::Rail, true),
            Some("full") => (SidebarState::Full, true),
            _ => (SidebarState::Full, false),
        }
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS. Several existing tests hardcode a 28-column sidebar in their geometry comments and expectations — `src/dashboard.rs:6288` names it explicitly. Those run at 100×30, which is above the threshold, so they should be unaffected; where one runs at 80×24 and now gets a rail, update the expected geometry and say why in the comment.

- [ ] **Step 8: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/dashboard.rs src/keymap.rs src/main.rs
git commit -m "feat: collapse the sidebar to a rail that still says who needs you"
```

---

### Task 9: Graphite & Cyan

**Files:**
- Modify: `src/theme.rs` (whole file)
- Modify: `src/dashboard.rs` — chrome backgrounds where `panel` now applies

**Interfaces:**
- Consumes: nothing.
- Produces: `Theme::panel`, `Theme::cool()`, `Theme::warm()` retained.

- [ ] **Step 1: Write the failing tests**

In `src/theme.rs`'s test module:

```rust
/// Relative luminance, as WCAG defines it.
fn luminance(colour: Color) -> f64 {
    let Color::Rgb(r, g, b) = colour else {
        panic!("every token in a Dock theme is an explicit RGB triple");
    };
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast(a: Color, b: Color) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn distance(a: Color, b: Color) -> f64 {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else { panic!("rgb") };
    let square = |x: u8, y: u8| (f64::from(x) - f64::from(y)).powi(2);
    (square(ar, br) + square(ag, bg) + square(ab, bb)).sqrt()
}

/// Every token has to clear 3:1 against both surfaces it can be painted on. `panel` sits
/// above `surface`, so a colour chosen only against the ground can go marginal on chrome.
#[test]
fn every_token_is_legible_on_both_surfaces() {
    let theme = Theme::cool();
    for (name, colour) in [
        ("text", theme.text), ("muted", theme.muted), ("accent", theme.accent),
        ("blocked", theme.blocked), ("working", theme.working), ("done", theme.done),
        ("idle", theme.idle), ("border", theme.border_focused),
    ] {
        for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
            let ratio = contrast(colour, surface);
            assert!(ratio >= 3.0, "{name} on {ground} is only {ratio:.2}:1");
        }
    }
}

/// The selection band's two floors, which pull in opposite directions: brighter makes the
/// band visible as a band and dimmer keeps the text on it readable.
#[test]
fn the_selection_band_clears_both_of_its_floors() {
    let theme = Theme::cool();
    assert!(contrast(theme.selection, theme.surface) >= 3.0);
    assert!(contrast(theme.text, theme.selection) >= 4.5);
}

/// The four agent states must stay far enough apart to be told apart at a glance.
///
/// Not theoretical: `working` and `idle` collided twice while this palette was being chosen,
/// because both mean "nothing is being asked of you" and both drift toward the same quiet
/// slate. This is what keeps them apart.
#[test]
fn the_agent_states_stay_far_apart() {
    let theme = Theme::cool();
    let states = [
        ("blocked", theme.blocked), ("working", theme.working),
        ("done", theme.done), ("idle", theme.idle),
    ];
    for (index, (name, colour)) in states.iter().enumerate() {
        for (other, second) in &states[index + 1..] {
            let apart = distance(*colour, *second);
            assert!(apart >= 60.0, "{name} and {other} are only {apart:.1} apart");
        }
        let from_accent = distance(*colour, theme.accent);
        assert!(from_accent >= 60.0, "{name} is only {from_accent:.1} from the accent");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --lib every_token_is_legible`
Expected: FAIL — `Theme::cool` and `Theme::panel` do not exist.

- [ ] **Step 3: Add the token and the palette**

In `src/theme.rs`, add to `Theme`:

```rust
    /// A surface that sits above `surface`.
    ///
    /// Chrome only — the sidebar, the overlays, the board pane, the footer — and never a
    /// terminal pane's body, where a background of Dock's choosing would fight every program
    /// that sets its own. Without this token every surface painted on the same flat ground
    /// and the whole dashboard read as one plane.
    pub panel: Color,
```

```rust
    /// "Graphite and cyan": a cool graphite ground with teal for structure — focus, the active
    /// tab, the keys you can press — and exactly one warm colour in the entire palette.
    ///
    /// That last part is the design. In `warm` the accent (232,168,88) and `working`
    /// (226,184,96) are nearly the same colour, and the accent is simultaneously the focused
    /// border, the active tab and every keybinding in the sidebar — so "an agent is working"
    /// competed for the same channel as "here is a key", and nothing amber could be urgent.
    /// Here rose is the only warm token there is, which makes `needs you` structurally
    /// incapable of being mistaken for chrome.
    pub const fn cool() -> Self {
        Self {
            accent: Color::Rgb(79, 209, 197),
            surface: Color::Rgb(18, 22, 26),
            panel: Color::Rgb(27, 32, 38),
            muted: Color::Rgb(124, 138, 145),
            border: Color::Rgb(38, 46, 51),
            border_focused: Color::Rgb(79, 209, 197),
            text: Color::Rgb(221, 228, 232),
            selection: Color::Rgb(58, 107, 120),
            blocked: Color::Rgb(242, 114, 107),
            working: Color::Rgb(53, 160, 153),
            done: Color::Rgb(122, 162, 247),
            idle: Color::Rgb(110, 118, 129),
        }
    }
```

Add `panel: Color::Rgb(26, 26, 29)` to `warm()` so both palettes carry every token, and switch `Default` to `cool()`. Read `DOCK_THEME` where the dashboard is constructed:

```rust
/// `DOCK_THEME=warm` keeps the old palette. Same shape as `DOCK_CLIPBOARD` and
/// `DOCK_SIDEBAR`, and for the same reason.
pub fn from_env() -> Theme {
    match std::env::var("DOCK_THEME").ok().as_deref().map(str::trim) {
        Some("warm") => Theme::warm(),
        _ => Theme::cool(),
    }
}
```

- [ ] **Step 4: Run the palette tests**

Run: `cargo test --lib theme`
Expected: PASS, including the two existing tests (`agent_states_map_to_distinct_colours`, `the_selection_background_is_distinct_from_every_other_token`) — run them against both palettes by parameterising over `[Theme::warm(), Theme::cool()]`.

- [ ] **Step 5: Paint the chrome on `panel`**

Give these a `Style::default().bg(self.theme.panel)`: the sidebar's block, every overlay block (help, rename, launch form, picker, review, board, git), the context menu from Task 7, and the board pane's interior. **Do not** set a background on `PseudoTerminal`'s rect or on the `Block` wrapping a terminal pane — a pane body belongs to the program running in it.

- [ ] **Step 6: Confirm nothing regressed visually or in cost**

Run: `cargo test && cargo test --release --lib render_breakdown -- --ignored --nocapture`
Expected: PASS; the frame stays within 10 % of baseline. A background style is one extra field on an existing `Block`, so it should not register at all.

- [ ] **Step 7: Look at it**

Run: `cargo install --path . --force && dock`
Expected: cool ground, teal focus border, a rose `needs you`. Check a pane running something with its own background — `vim` or `htop` — and confirm the pane body is that program's colours, not Dock's.

- [ ] **Step 8: Lints, then commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/theme.rs src/dashboard.rs
git commit -m "feat: repaint Dock in graphite and cyan, with one warm colour left in it"
```

---

## Final verification

- [ ] **Full suite:** `cargo test` — 598 lib + 42 bin, plus the new tests, all green.
- [ ] **Lints:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- [ ] **Frame budget:** `cargo test --release --lib render_breakdown -- --ignored --nocapture` — within 10 % of 0.091 / 0.327 / 1.438 ms.
- [ ] **The number this work exists to move:** add an `#[ignore]`d measurement beside `render_breakdown` that times press-to-first-highlight on an unfocused pane, and record it in the final commit message. Nothing measures this today, which is why the regression shipped.
- [ ] **By hand:** drag a selection across a busy pane and a quiet one; `Cmd+V` into another app and confirm the text arrives; right-click a pane, a tab, a sidebar row and a card; `Ctrl+B s`; open the board and press `a`, `A`, `v`.
