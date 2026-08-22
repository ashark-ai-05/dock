# Dock A1 Copy Mode and Mouse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore the ability to read scrollback and copy text out of a pane, which `EnableMouseCapture` took away in P0.

**Architecture:** Entirely client-side. The dashboard already holds a full `vt100` replica per run, and `vt100` already retains bounded scrollback and can render a scrolled view through the same methods `tui-term` reads. So scrolling is an offset on the replica, selection is a pair of grid coordinates, and extraction is `Screen::contents_between`. No protocol change, no daemon change.

**Tech Stack:** Rust 2024, ratatui 0.30, tui-term 0.3, vt100 0.16, crossterm 0.29, base64 0.23.

**Spec:** `docs/superpowers/specs/2026-08-22-dock-a1-copy-mode-and-mouse-design.md`

## Global Constraints

- Baseline is **247 passing** (239 lib + 5 + 1 + 2) at `04b4248`. No existing test may regress.
- Gates before every commit: `cargo fmt --check`, `cargo test --all-targets`, `cargo clippy --all-targets --all-features -- -D warnings`.
- **No new dependencies.** `base64` is already present for OSC 52.
- **No protocol change.** A1 must not touch `src/protocol.rs`, `src/server.rs`, `src/dispatch.rs`, or `src/runtime.rs`. If a task appears to need one, stop and report it.
- **The scrollback offset value is not stable; the viewport is.** Verified by probe: feeding one line while the offset was 10 left the top visible row unchanged and moved the offset to 11 — `vt100` auto-adjusts to pin the view. Never assert on `scrollback()` returning a specific number. Assert on visible rows, or on `scrollback() == 0` versus `!= 0`.
- Copy mode is modal and must be visibly signalled. P0 deleted an invisible input mode for exactly this reason; do not reintroduce one.
- Releasing a drag finalises a selection but never writes to the clipboard. Yank is always an explicit `y`.
- SEVEN known pre-existing flaky tests, all real-subprocess timing races: `crash_after_spawn_before_receipt_is_guarded_and_never_retried`, `lifecycle_signals_only_the_registered_owned_group_and_restart_replaces_it`, `restart_reserves_global_and_repository_capacity_then_terminalizes_launch_failure`, `receipt_failure_after_launch_rolls_back_exact_runtime_binding_and_capacity`, `runtime::tests::child_observes_the_requested_pty_size_and_a_later_resize`, `an_exited_shell_is_announced_even_though_its_screen_stops_changing`, and the `smoke-slice62-nongit-macos.sh` script. If only these fail, re-run.
- `touch src/*.rs src/**/*.rs` before verifying — Cargo's mtime fingerprint has served stale builds on this machine.
- Rust edition 2024. Comments explain *why*, not *what*.

---

## File Structure

| Path | Responsibility | Task |
|---|---|---|
| `src/terminal/vt.rs` | Expose scrollback control and selection extraction on `VtTerminal` | 1 |
| `src/copy.rs` | `CopySession` — cursor, anchor, selection maths, search. Pure, no rendering | 2, 5 |
| `src/clipboard.rs` | OSC 52 encoding and the `pbcopy` fallback | 3 |
| `src/dashboard.rs` | Wheel handling, copy-mode key routing, drag selection, render the viewport and mode indicator | 4, 6, 7 |
| `src/keymap.rs` | Bind `[` after the prefix | 6 |
| `README.md`, `docs/terminal-runtime-parity.md` | Document copy mode and its limitations | 8 |

---

### Task 1: Scrollback and selection on `VtTerminal`

**Files:**
- Modify: `src/terminal/vt.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `VtTerminal::scroll_by(&mut self, delta: i32)` — positive scrolls back into history, negative toward live; clamped by `vt100`.
  - `VtTerminal::scroll_offset(&self) -> usize`
  - `VtTerminal::scroll_to_live(&mut self)`
  - `VtTerminal::is_scrolled(&self) -> bool`
  - `VtTerminal::selection_text(&self, from: (u16, u16), to: (u16, u16)) -> String` — order-independent; `from`/`to` are `(row, col)` in the currently visible grid.
  - `VtTerminal::visible_row(&self, row: u16) -> String`

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/terminal/vt.rs`:

```rust
fn filled(rows: u16, cols: u16, lines: usize) -> VtTerminal {
    let mut term = VtTerminal::new(rows, cols, 100);
    for index in 1..=lines {
        term.feed(format!("line {index}\r\n").as_bytes());
    }
    term
}

#[test]
fn scrolling_back_shows_older_rows_and_returning_to_live_restores_the_tail() {
    let mut term = filled(5, 40, 20);
    assert!(!term.is_scrolled());
    term.scroll_by(10);
    assert!(term.is_scrolled());
    assert_eq!(term.visible_row(0).trim(), "line 7");
    term.scroll_to_live();
    assert!(!term.is_scrolled());
    assert_eq!(term.scroll_offset(), 0);
}

#[test]
fn the_viewport_is_pinned_while_scrolled_even_as_new_output_arrives() {
    let mut term = filled(5, 40, 20);
    term.scroll_by(10);
    let before = term.visible_row(0);
    term.feed(b"NEW OUTPUT\r\n");
    // vt100 auto-adjusts the offset to hold the view still, so assert on the ROW, never on
    // the offset number, which legitimately changes.
    assert_eq!(term.visible_row(0), before);
    assert!(term.is_scrolled());
}

#[test]
fn scrolling_is_clamped_at_both_ends() {
    let mut term = filled(5, 40, 20);
    term.scroll_by(9_999);
    let top = term.visible_row(0);
    term.scroll_by(9_999);
    assert_eq!(term.visible_row(0), top, "already at the oldest row");
    term.scroll_by(-9_999);
    assert_eq!(term.scroll_offset(), 0);
    term.scroll_by(-9_999);
    assert_eq!(term.scroll_offset(), 0, "cannot scroll past live output");
}

#[test]
fn selection_text_is_order_independent_and_spans_rows() {
    let mut term = filled(5, 40, 20);
    term.scroll_by(10);
    let forward = term.selection_text((0, 0), (2, 39));
    let reversed = term.selection_text((2, 39), (0, 0));
    assert_eq!(forward, reversed);
    assert!(forward.contains("line 7"));
    assert!(forward.contains("line 9"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib terminal::vt 2>&1 | tail -20`
Expected: FAIL — `no method named scroll_by`.

- [ ] **Step 3: Implement**

Add to `impl VtTerminal` in `src/terminal/vt.rs`:

```rust
    /// Moves the viewport through retained scrollback. Positive goes back into history.
    ///
    /// `vt100` clamps to the rows actually retained and, while the offset is non-zero, adjusts it
    /// as new output arrives so the visible rows stay put. That is why callers must treat the
    /// offset as opaque: only "zero versus non-zero" is meaningful.
    pub fn scroll_by(&mut self, delta: i32) {
        let current = i64::try_from(self.parser.screen().scrollback()).unwrap_or(i64::MAX);
        let target = (current + i64::from(delta)).max(0);
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_to_live(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset() != 0
    }

    pub fn visible_row(&self, row: u16) -> String {
        let (_, cols) = self.size();
        self.parser
            .screen()
            .contents_between(row, 0, row, cols)
    }

    /// Text between two points of the visible grid, in reading order regardless of which
    /// point was anchored first.
    pub fn selection_text(&self, from: (u16, u16), to: (u16, u16)) -> String {
        let (start, end) = if (from.0, from.1) <= (to.0, to.1) {
            (from, to)
        } else {
            (to, from)
        };
        self.parser
            .screen()
            .contents_between(start.0, start.1, end.0, end.1)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib terminal::vt 2>&1 | tail -20`
Expected: PASS — 4 new tests.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/terminal/vt.rs
git commit -m "feat: expose scrollback navigation and selection text on VtTerminal"
```

---

### Task 2: `CopySession` selection state machine

**Files:**
- Create: `src/copy.rs`
- Modify: `src/lib.rs` (add `pub mod copy;` after `pub mod client;`)

**Interfaces:**
- Consumes: nothing from Task 1 (pure state; the dashboard joins it to a terminal).
- Produces:
  - `pub struct CopySession { pub run_id: String, .. }`
  - `CopySession::new(run_id: String, cursor: (u16, u16)) -> CopySession`
  - `CopySession::cursor(&self) -> (u16, u16)`
  - `CopySession::anchor(&self) -> Option<(u16, u16)>`
  - `CopySession::selecting(&self) -> bool`
  - `CopySession::move_cursor(&mut self, rows: i32, cols: i32, bounds: (u16, u16))`
  - `CopySession::set_cursor(&mut self, cursor: (u16, u16), bounds: (u16, u16))`
  - `CopySession::begin_selection(&mut self)`
  - `CopySession::selection(&self) -> Option<((u16, u16), (u16, u16))>`

- [ ] **Step 1: Write the failing tests**

Create `src/copy.rs` containing only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: (u16, u16) = (24, 80);

    #[test]
    fn a_new_session_has_a_cursor_and_no_selection() {
        let session = CopySession::new("run".into(), (5, 10));
        assert_eq!(session.cursor(), (5, 10));
        assert_eq!(session.anchor(), None);
        assert!(!session.selecting());
        assert_eq!(session.selection(), None);
    }

    #[test]
    fn cursor_movement_is_clamped_to_the_grid() {
        let mut session = CopySession::new("run".into(), (0, 0));
        session.move_cursor(-5, -5, BOUNDS);
        assert_eq!(session.cursor(), (0, 0), "cannot move above or left of the grid");
        session.move_cursor(9_999, 9_999, BOUNDS);
        assert_eq!(session.cursor(), (23, 79), "clamped to the last cell");
    }

    #[test]
    fn beginning_a_selection_anchors_at_the_cursor_and_tracks_movement() {
        let mut session = CopySession::new("run".into(), (2, 3));
        session.begin_selection();
        assert!(session.selecting());
        assert_eq!(session.anchor(), Some((2, 3)));
        session.move_cursor(2, 0, BOUNDS);
        assert_eq!(session.selection(), Some(((2, 3), (4, 3))));
    }

    #[test]
    fn set_cursor_drives_selection_for_a_mouse_drag() {
        let mut session = CopySession::new("run".into(), (1, 1));
        session.begin_selection();
        session.set_cursor((7, 20), BOUNDS);
        assert_eq!(session.selection(), Some(((1, 1), (7, 20))));
        session.set_cursor((9_999, 9_999), BOUNDS);
        assert_eq!(session.selection(), Some(((1, 1), (23, 79))));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib copy 2>&1 | tail -20`
Expected: FAIL — `cannot find type CopySession`.

- [ ] **Step 3: Implement**

Prepend to `src/copy.rs`:

```rust
/// Copy mode's selection state for one pane. Deliberately pure: it knows grid coordinates and
/// nothing about terminals, so the maths can be tested without a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySession {
    pub run_id: String,
    cursor: (u16, u16),
    anchor: Option<(u16, u16)>,
}

impl CopySession {
    pub fn new(run_id: String, cursor: (u16, u16)) -> Self {
        Self {
            run_id,
            cursor,
            anchor: None,
        }
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.cursor
    }

    pub fn anchor(&self) -> Option<(u16, u16)> {
        self.anchor
    }

    pub fn selecting(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn begin_selection(&mut self) {
        self.anchor = Some(self.cursor);
    }

    pub fn move_cursor(&mut self, rows: i32, cols: i32, bounds: (u16, u16)) {
        let row = i64::from(self.cursor.0) + i64::from(rows);
        let col = i64::from(self.cursor.1) + i64::from(cols);
        self.set_cursor_absolute(row, col, bounds);
    }

    pub fn set_cursor(&mut self, cursor: (u16, u16), bounds: (u16, u16)) {
        self.set_cursor_absolute(i64::from(cursor.0), i64::from(cursor.1), bounds);
    }

    /// Selection endpoints in the order they were made. Callers order them for extraction;
    /// `VtTerminal::selection_text` is order-independent.
    pub fn selection(&self) -> Option<((u16, u16), (u16, u16))> {
        self.anchor.map(|anchor| (anchor, self.cursor))
    }

    fn set_cursor_absolute(&mut self, row: i64, col: i64, bounds: (u16, u16)) {
        let last_row = i64::from(bounds.0.saturating_sub(1));
        let last_col = i64::from(bounds.1.saturating_sub(1));
        self.cursor = (
            u16::try_from(row.clamp(0, last_row)).unwrap_or(0),
            u16::try_from(col.clamp(0, last_col)).unwrap_or(0),
        );
    }
}
```

Add `pub mod copy;` to `src/lib.rs` after `pub mod client;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib copy 2>&1 | tail -20`
Expected: PASS — 4 tests.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/copy.rs src/lib.rs
git commit -m "feat: add the copy-mode selection state machine"
```

---

### Task 3: Clipboard via OSC 52

**Files:**
- Create: `src/clipboard.rs`
- Modify: `src/lib.rs` (add `pub mod clipboard;` after `pub mod client;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn osc52(text: &str) -> Vec<u8>`
  - `pub enum ClipboardRoute { Osc52, Command(&'static str) }`
  - `pub fn copy(text: &str) -> Result<ClipboardRoute, String>`

- [ ] **Step 1: Write the failing tests**

Create `src/clipboard.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    #[test]
    fn osc52_wraps_base64_of_the_selection() {
        let sequence = osc52("hello");
        let text = String::from_utf8(sequence).expect("osc 52 is ascii");
        assert!(text.starts_with("\x1b]52;c;"), "got {text:?}");
        assert!(text.ends_with('\x07'), "got {text:?}");
        let payload = text
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        assert_eq!(STANDARD.decode(payload).expect("valid base64"), b"hello");
    }

    #[test]
    fn osc52_survives_multi_line_and_non_ascii_selections() {
        let selection = "line 1\nline 2\né 🎉";
        let text = String::from_utf8(osc52(selection)).expect("osc 52 is ascii");
        let payload = text
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        let decoded = STANDARD.decode(payload).expect("valid base64");
        assert_eq!(String::from_utf8(decoded).expect("utf8"), selection);
    }

    #[test]
    fn an_empty_selection_still_produces_a_well_formed_sequence() {
        let text = String::from_utf8(osc52("")).expect("osc 52 is ascii");
        assert_eq!(text, "\x1b]52;c;\x07");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib clipboard 2>&1 | tail -20`
Expected: FAIL — `cannot find function osc52`.

- [ ] **Step 3: Implement**

Prepend to `src/clipboard.rs`:

```rust
use std::{
    io::Write,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Which path actually put the text on the clipboard. Reported to the user so a silent
/// no-op is impossible — OSC 52 is disabled by default in some terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRoute {
    Osc52,
    Command(&'static str),
}

/// OSC 52 asks the *host* terminal to set its clipboard, so it works over SSH where a local
/// helper binary would not.
pub fn osc52(text: &str) -> Vec<u8> {
    let mut sequence = b"\x1b]52;c;".to_vec();
    sequence.extend_from_slice(STANDARD.encode(text).as_bytes());
    sequence.push(0x07);
    sequence
}

/// Writes the selection to the system clipboard, preferring OSC 52 and falling back to a
/// platform helper. Returns which route succeeded.
pub fn copy(text: &str) -> Result<ClipboardRoute, String> {
    let mut stdout = std::io::stdout();
    if stdout.write_all(&osc52(text)).is_ok() && stdout.flush().is_ok() {
        return Ok(ClipboardRoute::Osc52);
    }
    for helper in ["pbcopy", "wl-copy", "xclip"] {
        if let Ok(mut child) = Command::new(helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let wrote = child
                .stdin
                .as_mut()
                .is_some_and(|stdin| stdin.write_all(text.as_bytes()).is_ok());
            let _ = child.wait();
            if wrote {
                return Ok(ClipboardRoute::Command(helper));
            }
        }
    }
    Err("could not reach the system clipboard by OSC 52 or a helper".into())
}
```

Add `pub mod clipboard;` to `src/lib.rs` after `pub mod client;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib clipboard 2>&1 | tail -20`
Expected: PASS — 3 tests.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/clipboard.rs src/lib.rs
git commit -m "feat: add OSC 52 clipboard with a helper-command fallback"
```

---

### Task 4: Scroll wheel scrolls the pane under the pointer

**Files:**
- Modify: `src/dashboard.rs` — the `MouseEventKind` match in `Dashboard::mouse`

**Interfaces:**
- Consumes: `VtTerminal::scroll_by`, `scroll_to_live`, `is_scrolled` (Task 1).
- Produces: no new public API.

- [ ] **Step 1: Write the failing test**

Add to `src/dashboard.rs` tests:

```rust
#[test]
fn the_wheel_scrolls_the_pane_under_the_pointer_and_returning_to_live_resumes_following() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b""));
    if let Some(screen) = dashboard.screens.get_mut("run_1") {
        for index in 1..=60 {
            screen.feed(format!("line {index}\r\n").as_bytes());
        }
    }
    render_to_string(&mut dashboard, 100, 30);
    let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
    let (column, row) = (area.x + 2, area.y + 2);

    let scrolled = dashboard.mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(scrolled, UiCommand::None, "scrolling costs no daemon request");
    assert!(
        dashboard.screens["run_1"].is_scrolled(),
        "the wheel must move the viewport into history"
    );

    for _ in 0..40 {
        dashboard.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert!(
        !dashboard.screens["run_1"].is_scrolled(),
        "scrolling back to the bottom resumes following live output"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::the_wheel_scrolls 2>&1 | tail -20`
Expected: FAIL — the assertion that the pane is scrolled.

- [ ] **Step 3: Implement**

In `Dashboard::mouse`, replace the `_ => UiCommand::None` catch-all with wheel arms before it:

```rust
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                // Three rows per notch matches what terminals send for a single wheel click.
                let delta = if event.kind == MouseEventKind::ScrollUp { 3 } else { -3 };
                let run_id = self
                    .pane_areas
                    .iter()
                    .find(|(_, area)| contains(**area, event.column, event.row))
                    .and_then(|(pane_id, _)| self.workspace()?.panes.get(pane_id))
                    .and_then(|pane| pane.run_id.clone());
                if let Some(screen) = run_id.and_then(|id| self.screens.get_mut(&id)) {
                    screen.scroll_by(delta);
                }
                UiCommand::None
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib dashboard::tests::the_wheel_scrolls 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/dashboard.rs
git commit -m "feat: scroll a pane's history with the mouse wheel"
```

---

### Task 5: Scrollback search

**Files:**
- Modify: `src/copy.rs`

**Interfaces:**
- Consumes: `CopySession` (Task 2).
- Produces:
  - `CopySession::search_query(&self) -> Option<&str>`
  - `CopySession::begin_search(&mut self)`
  - `CopySession::push_search(&mut self, character: char)`
  - `CopySession::pop_search(&mut self)`
  - `CopySession::cancel_search(&mut self)`
  - `pub fn find_matches(rows: &[String], query: &str) -> Vec<(u16, u16)>`
  - `CopySession::jump_to_match(&mut self, matches: &[(u16, u16)], forward: bool, bounds: (u16, u16)) -> bool`

- [ ] **Step 1: Write the failing tests**

Add to `src/copy.rs`'s test module:

```rust
    fn rows() -> Vec<String> {
        vec![
            "alpha beta".to_string(),
            "gamma".to_string(),
            "beta again beta".to_string(),
        ]
    }

    #[test]
    fn find_matches_returns_every_hit_in_reading_order() {
        assert_eq!(
            find_matches(&rows(), "beta"),
            vec![(0, 6), (2, 0), (2, 11)]
        );
        assert_eq!(find_matches(&rows(), "nothing"), Vec::new());
        assert_eq!(find_matches(&rows(), ""), Vec::new(), "an empty query matches nothing");
    }

    #[test]
    fn jumping_cycles_forward_and_backward_and_wraps() {
        let matches = find_matches(&rows(), "beta");
        let mut session = CopySession::new("run".into(), (0, 0));
        assert!(session.jump_to_match(&matches, true, BOUNDS));
        assert_eq!(session.cursor(), (0, 6));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (2, 0));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (2, 11));
        session.jump_to_match(&matches, true, BOUNDS);
        assert_eq!(session.cursor(), (0, 6), "wraps to the first hit");
        session.jump_to_match(&matches, false, BOUNDS);
        assert_eq!(session.cursor(), (2, 11), "wraps backward to the last hit");
    }

    #[test]
    fn jumping_with_no_matches_reports_failure_and_leaves_the_cursor_alone() {
        let mut session = CopySession::new("run".into(), (4, 4));
        assert!(!session.jump_to_match(&[], true, BOUNDS));
        assert_eq!(session.cursor(), (4, 4));
    }

    #[test]
    fn a_search_query_is_edited_and_cancelled() {
        let mut session = CopySession::new("run".into(), (0, 0));
        assert_eq!(session.search_query(), None);
        session.begin_search();
        assert_eq!(session.search_query(), Some(""));
        session.push_search('a');
        session.push_search('b');
        assert_eq!(session.search_query(), Some("ab"));
        session.pop_search();
        assert_eq!(session.search_query(), Some("a"));
        session.cancel_search();
        assert_eq!(session.search_query(), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib copy 2>&1 | tail -20`
Expected: FAIL — `cannot find function find_matches`.

- [ ] **Step 3: Implement**

Add a `search: Option<String>` field to `CopySession` (default `None` in `new`), then add:

```rust
/// Every occurrence of `query` across the visible rows, in reading order. Case-sensitive,
/// matching what a user typing an exact string expects.
pub fn find_matches(rows: &[String], query: &str) -> Vec<(u16, u16)> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let Ok(row_index) = u16::try_from(index) else {
            break;
        };
        let mut from = 0;
        while let Some(found) = row[from..].find(query) {
            let column = from + found;
            if let Ok(column) = u16::try_from(column) {
                matches.push((row_index, column));
            }
            from = column + query.len();
        }
    }
    matches
}

impl CopySession {
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_deref()
    }

    pub fn begin_search(&mut self) {
        self.search = Some(String::new());
    }

    pub fn push_search(&mut self, character: char) {
        if let Some(query) = self.search.as_mut() {
            query.push(character);
        }
    }

    pub fn pop_search(&mut self) {
        if let Some(query) = self.search.as_mut() {
            query.pop();
        }
    }

    pub fn cancel_search(&mut self) {
        self.search = None;
    }

    /// Moves the cursor to the next or previous match, wrapping at both ends. Returns false
    /// when there is nothing to jump to, so the caller can report "no matches" rather than
    /// silently doing nothing.
    pub fn jump_to_match(
        &mut self,
        matches: &[(u16, u16)],
        forward: bool,
        bounds: (u16, u16),
    ) -> bool {
        if matches.is_empty() {
            return false;
        }
        let cursor = self.cursor;
        let target = if forward {
            matches
                .iter()
                .find(|candidate| **candidate > cursor)
                .or_else(|| matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|candidate| **candidate < cursor)
                .or_else(|| matches.last())
        };
        if let Some(target) = target.copied() {
            self.set_cursor(target, bounds);
            return true;
        }
        false
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib copy 2>&1 | tail -20`
Expected: PASS — 8 tests total in this module.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/copy.rs
git commit -m "feat: add scrollback search to copy mode"
```

---

### Task 6: Enter copy mode and route its keys

**Files:**
- Modify: `src/keymap.rs` — add `PaneCommand::CopyMode`, bind `[`, add a hint
- Modify: `src/dashboard.rs` — `copy: Option<CopySession>` field, `run_command` arm, key routing

**Interfaces:**
- Consumes: `CopySession`, `find_matches` (Tasks 2, 5); `VtTerminal::selection_text`, `visible_row`, `scroll_by` (Task 1); `clipboard::copy`, `ClipboardRoute` (Task 3).
- Produces:
  - `Dashboard::copy_mode(&self) -> bool`
  - `Dashboard::copy_status(&self) -> Option<String>` — the mode indicator text.

- [ ] **Step 1: Write the failing tests**

Add to `src/dashboard.rs` tests:

```rust
#[test]
fn the_prefix_then_bracket_enters_copy_mode_and_escape_leaves_it() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
    assert!(!dashboard.copy_mode());
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    assert!(dashboard.copy_mode(), "Ctrl+B [ must enter copy mode");
    dashboard.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!dashboard.copy_mode(), "Esc always leaves copy mode");
}

#[test]
fn copy_mode_keys_never_reach_the_pane() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"hello world\r\n"));
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    for key in ['h', 'j', 'k', 'l', 'v', 'y'] {
        let outcome = dashboard.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        assert!(
            !matches!(outcome, UiCommand::PaneInput(_)),
            "{key} must not be forwarded to the PTY while in copy mode"
        );
    }
}

#[test]
fn yanking_a_selection_reports_which_clipboard_route_was_used() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"copy me\r\n"));
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    dashboard.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    dashboard.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    dashboard.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(!dashboard.copy_mode(), "yanking leaves copy mode");
    let notice = dashboard.error.clone().unwrap_or_default();
    assert!(
        notice.contains("copied") || notice.contains("clipboard"),
        "the yank must say what happened, got {notice:?}"
    );
}

#[test]
fn copy_mode_is_refused_on_a_pane_with_no_run() {
    let mut dashboard = dashboard();
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    assert!(!dashboard.copy_mode());
    assert!(
        dashboard.error.is_some(),
        "an impossible command must explain itself rather than doing nothing"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dashboard::tests::the_prefix_then_bracket 2>&1 | tail -20`
Expected: FAIL — `no method named copy_mode`.

- [ ] **Step 3: Implement the binding**

In `src/keymap.rs`, add `CopyMode` to `PaneCommand`, add to `command_for`:

```rust
        KeyCode::Char('[') => PaneCommand::CopyMode,
```

and add `("[", "copy mode")` to `Keymap::hints()`. **Note:** `[` is currently bound to `PaneCommand::Workspace(-1)`. Move workspace-previous to `,` and workspace-next to `.`, update both hints, and update the tests that assert `[`/`]` cycling — copy mode owning `[` matches tmux and is the stronger claim on the key.

- [ ] **Step 4: Implement the dashboard state**

Add to `Dashboard`:

```rust
    /// Copy mode's session, if active. Client-local: reading history costs the daemon nothing.
    copy: Option<CopySession>,
```

Add `copy_mode()`, `copy_status()`, and a `copy_key(&mut self, key: KeyEvent) -> UiCommand` that handles motion (`hjkl`/arrows), `g`/`G`, `v`, `y`, `/`, `n`/`N`, and `Esc`/`q`. In `Dashboard::key`, dispatch to `copy_key` **before** the keymap, so copy mode's keys cannot reach the PTY. The `PaneCommand::CopyMode` arm in `run_command` refuses with an error when the focused pane has no run.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib dashboard 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/keymap.rs src/dashboard.rs
git commit -m "feat: enter copy mode with the prefix and route its keys"
```

---

### Task 7: Render the copy viewport, selection highlight, and mode indicator

**Files:**
- Modify: `src/dashboard.rs` — `render_node`'s pane arm, the footer, and `Dashboard::mouse` for drag selection
- Modify: `src/theme.rs` — add a `selection` token

**Interfaces:**
- Consumes: everything from Tasks 1-6.
- Produces: no new public API.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn copy_mode_is_visibly_signalled_in_the_pane_and_footer() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"visible text\r\n"));
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE));
    let rendered = render_to_string(&mut dashboard, 100, 30);
    assert!(rendered.contains("COPY"), "the pane must say it is in copy mode");
    assert!(rendered.contains('y'), "the footer must publish the yank binding");
}

#[test]
fn dragging_across_a_pane_selects_without_writing_to_the_clipboard() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"drag over me\r\n"));
    render_to_string(&mut dashboard, 100, 30);
    let area = *dashboard.pane_areas.get("a").expect("pane a is rendered");
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: area.x + 2,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: area.x + 8,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(dashboard.copy_mode(), "a drag inside a pane enters copy mode");
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: area.x + 8,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        dashboard.copy_mode(),
        "releasing finalises the selection but stays in copy mode"
    );
    assert!(
        dashboard.error.is_none(),
        "releasing a drag must not write to the clipboard"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dashboard::tests::copy_mode_is_visibly 2>&1 | tail -20`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add `pub selection: Color` to `Theme` with a value distinct from every existing token, returned by `Theme::warm()`.

In `render_node`'s pane arm, when `self.copy.as_ref().is_some_and(|s| s.run_id == run_id)`:
- prefix the pane title with `COPY ` styled `self.theme.accent`
- render the selected cells with `self.theme.selection` as background, drawing over the `PseudoTerminal` output for the selected range
- draw a visible cursor block at `session.cursor()`

In `footer_line`, when in copy mode show `hjkl move · v select · y yank · / search · n/N next/prev · Esc exit` instead of the normal hints.

In `Dashboard::mouse`, add drag handling **inside a pane** (distinct from the existing divider drag): `Down` inside a pane area enters copy mode anchored at that cell; `Drag` calls `set_cursor`; `Up` leaves the session intact and does not yank.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib dashboard 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Run all gates and commit**

```bash
touch src/*.rs src/**/*.rs
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/dashboard.rs src/theme.rs
git commit -m "feat: render the copy viewport, selection and mode indicator"
```

---

### Task 8: Documentation and real-binary acceptance

**Files:**
- Modify: `README.md`, `docs/terminal-runtime-parity.md`

- [ ] **Step 1: Update the keymap table**

Add `[` copy mode to the README's keys table. Move the workspace-cycling row to `,`/`.` to match Task 6. Add a short "Copy and scrollback" section: wheel scrolls history, drag selects, `Ctrl+B [` for keyboard copy mode, `y` yanks, `/` searches.

- [ ] **Step 2: Update the parity matrix**

Change "Pane swap | Deferred" and neighbours as appropriate, and add rows: "Copy mode | Shipped", "Scrollback navigation | Shipped", "Scrollback search | Shipped". Document the two limitations verbatim from the spec: programs inside a pane cannot receive mouse events because Dock consumes them, and OSC 52 is terminal-dependent and disabled by default in some terminals.

- [ ] **Step 3: Real-binary acceptance**

Run the binary under a PTY and confirm each of the spec's six acceptance criteria:

```bash
cargo run --bin dock
```

1. The wheel scrolls a pane's history; scrolling to the bottom resumes following live output.
2. Dragging across pane text selects it; `y` puts it on the system clipboard.
3. `Ctrl+B [` then `v`, motion, `y` does the same from the keyboard.
4. `/` finds a string in scrollback and `n` cycles matches.
5. Copy mode is visibly signalled and always exits on `Esc`.
6. A busy pane (run `yes` in it) can be scrolled back and read without the view jumping.

Report what you observed for each. Both Critical defects in P0 were invisible to the test suite and surfaced only this way.

- [ ] **Step 4: Run the full verification suite and commit**

```bash
cargo fmt --check
cargo test --all-targets 2>&1 | grep -E "^test result"
cargo clippy --all-targets --all-features -- -D warnings
scripts/smoke-slice5-macos.sh
scripts/smoke-slice6-macos.sh
scripts/smoke-slice61-macos.sh
scripts/smoke-slice62-nongit-macos.sh
git add README.md docs/
git commit -m "docs: describe copy mode, scrollback navigation and search"
```

---

## Self-Review

**Spec coverage.** Wheel scrolling → Task 4. Keyboard copy mode → Tasks 2, 6. Mouse drag selection → Task 7. Search → Task 5. OSC 52 with fallback → Task 3. Mode indicator → Tasks 6, 7. Pinning → Task 1 (asserted on rows, never on the offset). Error handling: refusal on a pane with no run → Task 6; clipboard route reported → Tasks 3, 6. Acceptance criteria → Task 8.

**Known conflict, resolved.** `[` is currently bound to `PaneCommand::Workspace(-1)`. Task 6 moves workspace cycling to `,`/`.` and takes `[` for copy mode, matching tmux. This changes an existing binding, its hints, and its tests — called out explicitly in Task 6 Step 3 rather than left for the implementer to discover.

**Type consistency.** `CopySession` is defined in Task 2 and consumed in 5, 6, 7. `find_matches` is defined in Task 5 and consumed in 6. `VtTerminal::scroll_by`/`scroll_offset`/`is_scrolled`/`selection_text`/`visible_row` are defined in Task 1 and consumed in 4, 6, 7. `ClipboardRoute` is defined in Task 3 and consumed in 6. `Theme::selection` is added in Task 7 and used only there.

**Placeholder scan.** No TBD or TODO. Task 6 Step 4 and Task 7 Step 3 describe rendering and key routing in prose rather than complete code, because both are edits threaded through existing large match statements where a literal block would be misleading. Every method name, binding, and behaviour they must produce is stated exactly.
