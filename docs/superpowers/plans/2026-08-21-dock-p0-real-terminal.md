# Dock P0 Real Terminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every Dock pane a real, correctly-sized terminal that renders agent TUIs faithfully, driven by a push protocol and a `Ctrl+B` prefix keymap.

**Architecture:** The daemon owns a `vt100::Parser` per run (`live`) and each subscribed client keeps a second parser (`sent`) advanced only by transmitted deltas; `live.screen().state_diff(sent.screen())` is the wire format. The client feeds deltas into its own parser and renders through `tui_term::PseudoTerminal`. PTY size is bound to pane geometry via `TIOCSWINSZ` plus `SIGWINCH` to the owned process group. Keystrokes encode to bytes and are written without awaiting a reply.

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, nix 0.29, vt100 0.16, tui-term 0.3, base64 0.23, serde/serde_json, regex.

**Spec:** `docs/superpowers/specs/2026-08-21-dock-p0-real-terminal-design.md`

## Global Constraints

- Branch: `slice/p0-real-terminal`. Baseline is 130 passing tests; **none may regress**.
- Every gate must pass before each commit: `cargo fmt --check`, `cargo test --all-targets`, `cargo clippy --all-targets --all-features -- -D warnings`.
- Licence hygiene: Dock is MIT. Herdr is **AGPL-3.0-or-later** — never read or copy its source. Work only from public documentation.
- Only these dependencies may be added: `vt100 = "0.16"`, `tui-term = "0.3"`, `base64 = "0.23"`. All are MIT or MIT/Apache-2.0.
- Safety invariants that must survive unchanged: signals go only to an `OwnedProcessGroup` created by Dock's own launch; no adoption API; durable layout records never contain screen content, command vectors, PIDs, or PGIDs.
- Protocol version becomes `7`. Every protocol struct keeps `deny_unknown_fields`.
- Emulator swap point is the type alias `pub type PaneScreen = VtTerminal;` in `src/terminal/mod.rs`. No other module names `VtTerminal` or `vt100` directly.
- Use `state_diff` / `state_formatted`, never `contents_diff` / `contents_formatted` — only the `state_*` family carries cursor position.

---

## File Structure

| Path | Responsibility | Task |
|---|---|---|
| `src/terminal/mod.rs` | `PaneScreen` alias, `ScreenSync`, re-exports | 1 |
| `src/terminal/vt.rs` | `VtTerminal` + `TerminalHooks` (OSC 2/7/133) | 1 |
| `src/terminal/keys.rs` | `encode_key` — crossterm `KeyEvent` to PTY bytes | 2 |
| `src/keymap.rs` | `Keymap` prefix state machine, `PaneCommand` | 3 |
| `src/detect/mod.rs` | `AgentKind`, `AgentState`, `attention_rank` | 4 |
| `src/detect/process.rs` | PGID-scoped process-tree classification | 4 |
| `src/detect/heuristic.rs` | Screen-tail regex rules to `AgentState` | 4 |
| `src/theme.rs` | `Theme` semantic tokens and glyphs | 5 |
| `src/runtime.rs` | Replace `Scrollback`; winsize at open; `resize` | 6 |
| `src/protocol.rs` | v7 snapshot, `PaneResize`, `Subscribe`, `Event` | 7 |
| `src/adapter.rs` | `AdapterId::Shell` | 8 |
| `src/dispatch.rs` | `pane_resize`, shell auto-launch, snapshot wiring | 9 |
| `src/server.rs` | Stream mode, `PaneResize` routing | 10 |
| `src/main.rs` | Event-stream reader thread, input path | 11 |
| `src/dashboard.rs` | `PseudoTerminal` panes, theme, sidebar, which-key | 12 |
| `docs/`, `README.md`, `scripts/` | Parity matrix and smoke scripts | 13 |

---

### Task 1: Terminal emulation core

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs:1-13`
- Create: `src/terminal/mod.rs`
- Create: `src/terminal/vt.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub type PaneScreen = VtTerminal;`
  - `VtTerminal::new(rows: u16, cols: u16, scrollback_rows: usize) -> VtTerminal`
  - `VtTerminal::feed(&mut self, bytes: &[u8])`
  - `VtTerminal::resize(&mut self, rows: u16, cols: u16)`
  - `VtTerminal::size(&self) -> (u16, u16)`
  - `VtTerminal::cursor(&self) -> (u16, u16)`
  - `VtTerminal::alternate_screen(&self) -> bool`
  - `VtTerminal::state_bytes(&self) -> Vec<u8>`
  - `VtTerminal::text_tail(&self, rows: u16) -> String`
  - `VtTerminal::title(&self) -> Option<String>`
  - `VtTerminal::cwd(&self) -> Option<String>`
  - `VtTerminal::last_exit_status(&self) -> Option<i32>`
  - `VtTerminal::screen(&self) -> &vt100::Screen` (crate-internal; only `terminal` and `dashboard` may call)
  - `ScreenSync::new(rows: u16, cols: u16) -> ScreenSync`
  - `ScreenSync::delta_from(&mut self, live: &VtTerminal) -> Vec<u8>`
  - `ScreenSync::resize(&mut self, rows: u16, cols: u16)`

- [ ] **Step 1: Add dependencies**

```bash
cd /Users/krunal/Development/dock
cargo add vt100@0.16 tui-term@0.3 base64@0.23
```

- [ ] **Step 2: Write the failing tests**

Create `src/terminal/vt.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_styled_output_and_reports_geometry() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.feed(b"\x1b[1;32mgreen\x1b[0m plain");
        assert_eq!(term.size(), (24, 80));
        assert!(!term.alternate_screen());
        assert!(term.text_tail(1).contains("green plain"));
    }

    #[test]
    fn captures_osc_title_cwd_and_command_exit_status() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.feed(b"\x1b]2;my title\x07");
        term.feed(b"\x1b]7;file://host/Users/krunal/dock\x07");
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07false\x1b]133;C\x07\x1b]133;D;1\x07");
        assert_eq!(term.title().as_deref(), Some("my title"));
        assert_eq!(term.cwd().as_deref(), Some("/Users/krunal/dock"));
        assert_eq!(term.last_exit_status(), Some(1));
    }

    #[test]
    fn resize_changes_reported_size() {
        let mut term = VtTerminal::new(24, 80, 100);
        term.resize(40, 120);
        assert_eq!(term.size(), (40, 120));
    }

    #[test]
    fn sync_converges_through_sgr_alt_screen_and_resize() {
        let mut live = VtTerminal::new(24, 80, 100);
        let mut sync = ScreenSync::new(24, 80);
        let writes: Vec<&[u8]> = vec![
            b"\x1b[1;32m$ cargo build\x1b[0m\r\n",
            b"\x1b[2J\x1b[H",
            b"\x1b[?1049h",
            b"\x1b[3;10Halt-screen TUI\x1b[K",
            b"\x1b[?1049l",
            b"back to normal\r\n",
        ];
        for write in writes {
            live.feed(write);
            let delta = sync.delta_from(&live);
            sync.apply(&delta);
            assert_eq!(sync.state_bytes(), live.state_bytes());
        }
        live.resize(30, 100);
        sync.resize(30, 100);
        live.feed(b"after resize");
        let delta = sync.delta_from(&live);
        sync.apply(&delta);
        assert_eq!(sync.state_bytes(), live.state_bytes());
        assert_eq!(sync.cursor(), live.cursor());
    }

    #[test]
    fn idle_screen_produces_an_empty_delta() {
        let mut live = VtTerminal::new(24, 80, 100);
        live.feed(b"settled output\r\n");
        let mut sync = ScreenSync::new(24, 80);
        sync.apply(&sync.delta_from(&live).clone());
        assert!(sync.delta_from(&live).is_empty());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib terminal::vt 2>&1 | tail -20`
Expected: FAIL — `cannot find type VtTerminal`.

- [ ] **Step 4: Implement `VtTerminal` and `TerminalHooks`**

Prepend to `src/terminal/vt.rs` (above the test module):

```rust
use std::sync::{Arc, Mutex};

/// OSC state that `vt100` does not model itself. Captured through `Callbacks` so shell
/// integration (OSC 7 working directory, OSC 133 semantic prompts) costs no extra parsing.
#[derive(Debug, Default, Clone)]
pub struct ShellSignals {
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub last_exit_status: Option<i32>,
}

#[derive(Default)]
struct TerminalHooks(Arc<Mutex<ShellSignals>>);

impl vt100::Callbacks for TerminalHooks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        let mut signals = self.0.lock().unwrap_or_else(|p| p.into_inner());
        signals.title = Some(String::from_utf8_lossy(title).into_owned());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let parts: Vec<String> = params
            .iter()
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        let mut signals = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match parts.first().map(String::as_str) {
            // OSC 7 reports the working directory as a file:// URL.
            Some("7") => signals.cwd = parts.get(1).and_then(|url| parse_cwd(url)),
            // OSC 133;D;<code> marks the end of a command with its exit status.
            Some("133") if parts.get(1).map(String::as_str) == Some("D") => {
                signals.last_exit_status = parts.get(2).and_then(|code| code.parse().ok());
            }
            _ => {}
        }
    }
}

fn parse_cwd(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = rest.find('/').map(|index| &rest[index..])?;
    Some(path.to_owned())
}

pub struct VtTerminal {
    parser: vt100::Parser<TerminalHooks>,
    signals: Arc<Mutex<ShellSignals>>,
}

impl VtTerminal {
    pub fn new(rows: u16, cols: u16, scrollback_rows: usize) -> Self {
        let signals = Arc::new(Mutex::new(ShellSignals::default()));
        let hooks = TerminalHooks(Arc::clone(&signals));
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback_rows, hooks),
            signals,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Full screen state including cursor, sufficient to reconstruct this terminal exactly.
    pub fn state_bytes(&self) -> Vec<u8> {
        self.parser.screen().state_formatted()
    }

    pub fn text_tail(&self, rows: u16) -> String {
        let (height, width) = self.size();
        let start = height.saturating_sub(rows);
        self.parser
            .screen()
            .contents_between(start, 0, height, width)
    }

    pub fn title(&self) -> Option<String> {
        self.signals.lock().unwrap_or_else(|p| p.into_inner()).title.clone()
    }

    pub fn cwd(&self) -> Option<String> {
        self.signals.lock().unwrap_or_else(|p| p.into_inner()).cwd.clone()
    }

    pub fn last_exit_status(&self) -> Option<i32> {
        self.signals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_exit_status
    }

    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}
```

- [ ] **Step 5: Implement `ScreenSync`**

Create `src/terminal/mod.rs`:

```rust
mod vt;

pub use vt::{ShellSignals, VtTerminal};

/// Single swap point for the terminal engine. `rio-vt` can replace `VtTerminal` here
/// without touching any caller once it is mature enough to depend on.
pub type PaneScreen = VtTerminal;

/// Tracks what a single subscriber has already been sent, so the daemon can transmit
/// only the difference. An unchanged screen yields an empty delta and therefore no traffic.
pub struct ScreenSync {
    sent: VtTerminal,
}

impl ScreenSync {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            sent: VtTerminal::new(rows, cols, 0),
        }
    }

    /// Bytes that transform this subscriber's view into `live`. Does not mutate `self`;
    /// call `apply` once the bytes have actually been transmitted.
    pub fn delta_from(&self, live: &VtTerminal) -> Vec<u8> {
        live.screen().state_diff(self.sent.screen())
    }

    pub fn apply(&mut self, delta: &[u8]) {
        self.sent.feed(delta);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.sent.resize(rows, cols);
    }

    pub fn state_bytes(&self) -> Vec<u8> {
        self.sent.state_bytes()
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.sent.cursor()
    }
}
```

Register the module in `src/lib.rs` by adding `pub mod terminal;` in alphabetical position (after `pub mod storage;`).

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib terminal 2>&1 | tail -20`
Expected: PASS — 5 tests.

- [ ] **Step 7: Run all gates**

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
```
Expected: all green; 135 tests total.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/terminal/
git commit -m "feat: add vt100 terminal emulation core with OSC shell signals"
```

---

### Task 2: Key encoding

**Files:**
- Create: `src/terminal/keys.rs`
- Modify: `src/terminal/mod.rs` (add `mod keys;` and re-export)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `pub struct KeyEncoding { pub application_cursor: bool }`
  - `pub fn encode_key(key: KeyEvent, encoding: KeyEncoding) -> Option<Vec<u8>>`
  - `pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8>`

- [ ] **Step 1: Write the failing tests**

Create `src/terminal/keys.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn normal() -> KeyEncoding {
        KeyEncoding { application_cursor: false }
    }

    fn encode(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, modifiers), normal()).expect("encodable key")
    }

    #[test]
    fn encodes_plain_characters_and_control_combinations() {
        assert_eq!(encode(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(encode(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![0x03]);
        assert_eq!(encode(KeyCode::Char('b'), KeyModifiers::CONTROL), vec![0x02]);
        assert_eq!(encode(KeyCode::Char('a'), KeyModifiers::ALT), vec![0x1b, b'a']);
    }

    #[test]
    fn encodes_named_keys_that_agent_tuis_depend_on() {
        assert_eq!(encode(KeyCode::Esc, KeyModifiers::NONE), vec![0x1b]);
        assert_eq!(encode(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(encode(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(encode(KeyCode::BackTab, KeyModifiers::NONE), b"\x1b[Z");
        assert_eq!(encode(KeyCode::Backspace, KeyModifiers::NONE), vec![0x7f]);
        assert_eq!(encode(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(encode(KeyCode::Home, KeyModifiers::NONE), b"\x1b[H");
        assert_eq!(encode(KeyCode::End, KeyModifiers::NONE), b"\x1b[F");
        assert_eq!(encode(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(encode(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(encode(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
    }

    #[test]
    fn arrow_keys_respect_application_cursor_mode() {
        assert_eq!(encode(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        let app = KeyEncoding { application_cursor: true };
        let up = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), app).unwrap();
        assert_eq!(up, b"\x1bOA");
    }

    #[test]
    fn bracketed_paste_wraps_text_only_when_enabled() {
        assert_eq!(encode_paste("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        assert_eq!(encode_paste("hi", false), b"hi".to_vec());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib terminal::keys 2>&1 | tail -20`
Expected: FAIL — `cannot find function encode_key`.

- [ ] **Step 3: Implement the encoder**

Prepend to `src/terminal/keys.rs`:

```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyEncoding {
    /// DECCKM. Agent and editor TUIs switch this on, changing arrow keys from CSI to SS3.
    pub application_cursor: bool,
}

/// Translates a crossterm key into the bytes a PTY expects. Returns `None` for keys with
/// no terminal representation, which callers must drop rather than send as empty input.
pub fn encode_key(key: KeyEvent, encoding: KeyEncoding) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let mut bytes = match key.code {
        KeyCode::Char(character) => {
            if control {
                vec![control_byte(character)?]
            } else {
                character.to_string().into_bytes()
            }
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Home => cursor_key(b'H', encoding),
        KeyCode::End => cursor_key(b'F', encoding),
        KeyCode::Up => cursor_key(b'A', encoding),
        KeyCode::Down => cursor_key(b'B', encoding),
        KeyCode::Right => cursor_key(b'C', encoding),
        KeyCode::Left => cursor_key(b'D', encoding),
        KeyCode::F(number) => function_key(number)?,
        _ => return None,
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// Wraps pasted text so the receiving application can tell it apart from typing.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn control_byte(character: char) -> Option<u8> {
    match character {
        ' ' => Some(0),
        'a'..='z' => Some(character as u8 - b'a' + 1),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn cursor_key(final_byte: u8, encoding: KeyEncoding) -> Vec<u8> {
    let introducer: &[u8] = if encoding.application_cursor {
        b"\x1bO"
    } else {
        b"\x1b["
    };
    let mut bytes = introducer.to_vec();
    bytes.push(final_byte);
    bytes
}

fn function_key(number: u8) -> Option<Vec<u8>> {
    let sequence: &[u8] = match number {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(sequence.to_vec())
}
```

Add `mod keys;` and `pub use keys::{encode_key, encode_paste, KeyEncoding};` to `src/terminal/mod.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib terminal::keys 2>&1 | tail -20`
Expected: PASS — 4 tests.

- [ ] **Step 5: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/terminal/
git commit -m "feat: encode crossterm key events into PTY byte sequences"
```

---

### Task 3: Prefix keymap state machine

**Files:**
- Create: `src/keymap.rs`
- Modify: `src/lib.rs` (add `pub mod keymap;`)

**Interfaces:**
- Consumes: `encode_key`, `KeyEncoding` from Task 2.
- Produces:
  - `pub enum PaneCommand { NewWorkspace, Split(SplitAxis), Focus(FocusDirection), Resize(i16), Zoom, Rename, Close, Launch, Detach, Help, Quit }`
  - `pub enum FocusDirection { Next, Previous, Left, Right, Up, Down }`
  - `pub enum KeyOutcome { Passthrough(Vec<u8>), Command(PaneCommand), PendingPrefix, Ignored }`
  - `pub struct Keymap { pending: bool }`
  - `Keymap::new() -> Keymap`
  - `Keymap::handle(&mut self, key: KeyEvent, encoding: KeyEncoding) -> KeyOutcome`
  - `Keymap::is_pending(&self) -> bool`
  - `Keymap::hints() -> &'static [(&'static str, &'static str)]`

- [ ] **Step 1: Write the failing tests**

Create `src/keymap.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn prefix() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
    }

    fn plain(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
    }

    #[test]
    fn ordinary_keys_pass_straight_through_to_the_pane() {
        let mut keymap = Keymap::new();
        assert_eq!(
            keymap.handle(plain('q'), KeyEncoding::default()),
            KeyOutcome::Passthrough(b"q".to_vec())
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn escape_is_forwarded_and_never_intercepted() {
        let mut keymap = Keymap::new();
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            keymap.handle(escape, KeyEncoding::default()),
            KeyOutcome::Passthrough(vec![0x1b])
        );
    }

    #[test]
    fn prefix_then_command_produces_a_command_and_clears_pending() {
        let mut keymap = Keymap::new();
        assert_eq!(keymap.handle(prefix(), KeyEncoding::default()), KeyOutcome::PendingPrefix);
        assert!(keymap.is_pending());
        assert_eq!(
            keymap.handle(plain('q'), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::Quit)
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn double_prefix_sends_a_literal_control_b_to_the_pane() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(prefix(), KeyEncoding::default()),
            KeyOutcome::Passthrough(vec![0x02])
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn unknown_key_after_prefix_is_ignored_and_clears_pending() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(keymap.handle(plain('§'), KeyEncoding::default()), KeyOutcome::Ignored);
        assert!(!keymap.is_pending());
    }

    #[test]
    fn published_hints_cover_every_documented_binding() {
        let keys: Vec<&str> = Keymap::hints().iter().map(|(key, _)| *key).collect();
        for expected in ["n", "h", "v", "z", "r", "x", "l", "d", "?", "q"] {
            assert!(keys.contains(&expected), "missing hint for {expected}");
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib keymap 2>&1 | tail -20`
Expected: FAIL — `cannot find type Keymap`.

- [ ] **Step 3: Implement the state machine**

Prepend to `src/keymap.rs`:

```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    layout::SplitAxis,
    terminal::{KeyEncoding, encode_key},
};

/// `Ctrl+B`, matching tmux and Herdr so the binding is the least surprising available.
const PREFIX: u8 = 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Next,
    Previous,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneCommand {
    NewWorkspace,
    Split(SplitAxis),
    Focus(FocusDirection),
    Resize(i16),
    Zoom,
    Rename,
    Close,
    Launch,
    Detach,
    Help,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    Passthrough(Vec<u8>),
    Command(PaneCommand),
    PendingPrefix,
    Ignored,
}

#[derive(Debug, Default)]
pub struct Keymap {
    pending: bool,
}

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn handle(&mut self, key: KeyEvent, encoding: KeyEncoding) -> KeyOutcome {
        if self.pending {
            self.pending = false;
            // A second prefix means the user wants a literal Ctrl+B in the pane.
            if is_prefix(key) {
                return KeyOutcome::Passthrough(vec![PREFIX]);
            }
            return match command_for(key) {
                Some(command) => KeyOutcome::Command(command),
                None => KeyOutcome::Ignored,
            };
        }
        if is_prefix(key) {
            self.pending = true;
            return KeyOutcome::PendingPrefix;
        }
        match encode_key(key, encoding) {
            Some(bytes) => KeyOutcome::Passthrough(bytes),
            None => KeyOutcome::Ignored,
        }
    }

    /// The published binding table. Rendered as a which-key hint bar while the prefix is
    /// pending, which is the discoverability property Zellij is consistently praised for.
    pub fn hints() -> &'static [(&'static str, &'static str)] {
        &[
            ("n", "workspace"),
            ("h", "split ⇋"),
            ("v", "split ⇵"),
            ("←↑→↓", "focus"),
            ("+/-", "resize"),
            ("z", "zoom"),
            ("r", "rename"),
            ("x", "close"),
            ("l", "launch"),
            ("d", "detach"),
            ("?", "help"),
            ("q", "quit"),
        ]
    }
}

fn is_prefix(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b')
}

fn command_for(key: KeyEvent) -> Option<PaneCommand> {
    Some(match key.code {
        KeyCode::Char('n') => PaneCommand::NewWorkspace,
        KeyCode::Char('h') => PaneCommand::Split(SplitAxis::Horizontal),
        KeyCode::Char('v') => PaneCommand::Split(SplitAxis::Vertical),
        KeyCode::Char('z') => PaneCommand::Zoom,
        KeyCode::Char('r') => PaneCommand::Rename,
        KeyCode::Char('x') => PaneCommand::Close,
        KeyCode::Char('l') => PaneCommand::Launch,
        KeyCode::Char('d') => PaneCommand::Detach,
        KeyCode::Char('?') => PaneCommand::Help,
        KeyCode::Char('q') => PaneCommand::Quit,
        KeyCode::Char('+') => PaneCommand::Resize(50),
        KeyCode::Char('-') => PaneCommand::Resize(-50),
        KeyCode::Tab => PaneCommand::Focus(FocusDirection::Next),
        KeyCode::Left => PaneCommand::Focus(FocusDirection::Left),
        KeyCode::Right => PaneCommand::Focus(FocusDirection::Right),
        KeyCode::Up => PaneCommand::Focus(FocusDirection::Up),
        KeyCode::Down => PaneCommand::Focus(FocusDirection::Down),
        _ => return None,
    })
}
```

Add `pub mod keymap;` to `src/lib.rs` after `pub mod git;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib keymap 2>&1 | tail -20`
Expected: PASS — 6 tests.

- [ ] **Step 5: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/keymap.rs src/lib.rs
git commit -m "feat: add Ctrl+B prefix keymap state machine with which-key hints"
```

---

### Task 4: Agent detection (heuristic tier)

**Files:**
- Create: `src/detect/mod.rs`
- Create: `src/detect/process.rs`
- Create: `src/detect/heuristic.rs`
- Modify: `src/lib.rs` (add `pub mod detect;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum AgentKind { Claude, Codex, Amp, Copilot, OpenCode, Gemini, Cursor, Droid, Qwen, Kimi, Kiro, Hermes, Pi, Antigravity, Vibe, Omp }`
  - `AgentKind::from_executable(name: &str) -> Option<AgentKind>`
  - `AgentKind::label(self) -> &'static str`
  - `pub enum AgentState { Blocked, Working, Done, Idle }`
  - `AgentState::attention_rank(self) -> u8` (lower sorts first)
  - `AgentState::glyph(self) -> char`
  - `pub fn classify_screen(agent: AgentKind, tail: &str) -> AgentState`
  - `pub fn agent_in_process_table(table: &str, pgid: i32) -> Option<AgentKind>`

- [ ] **Step 1: Write the failing tests**

Create `src/detect/mod.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_executables_and_rejects_unknown_ones() {
        assert_eq!(AgentKind::from_executable("claude"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::from_executable("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::from_executable("amp"), Some(AgentKind::Amp));
        assert_eq!(AgentKind::from_executable("copilot"), Some(AgentKind::Copilot));
        assert_eq!(AgentKind::from_executable("github-copilot-cli"), Some(AgentKind::Copilot));
        assert_eq!(AgentKind::from_executable("zsh"), None);
    }

    #[test]
    fn attention_order_puts_blocked_first_and_idle_last() {
        let mut states = vec![
            AgentState::Idle,
            AgentState::Done,
            AgentState::Blocked,
            AgentState::Working,
        ];
        states.sort_by_key(|state| state.attention_rank());
        assert_eq!(
            states,
            vec![
                AgentState::Blocked,
                AgentState::Working,
                AgentState::Done,
                AgentState::Idle
            ]
        );
    }

    #[test]
    fn classifies_a_permission_prompt_as_blocked() {
        let tail = "Do you want to proceed?\n  1. Yes\n  2. No\n";
        assert_eq!(classify_screen(AgentKind::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classifies_active_work_and_falls_back_to_idle() {
        assert_eq!(
            classify_screen(AgentKind::Claude, "✳ Thinking… (12s · esc to interrupt)"),
            AgentState::Working
        );
        assert_eq!(classify_screen(AgentKind::Claude, "› "), AgentState::Idle);
    }

    #[test]
    fn finds_the_agent_running_inside_one_process_group_only() {
        // pid ppid pgid comm
        let table = "\
  501   1  501 zsh
  777 501  501 claude
  902   1  902 codex
";
        assert_eq!(agent_in_process_table(table, 501), Some(AgentKind::Claude));
        assert_eq!(agent_in_process_table(table, 902), Some(AgentKind::Codex));
        assert_eq!(agent_in_process_table(table, 123), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib detect 2>&1 | tail -20`
Expected: FAIL — `cannot find type AgentKind`.

- [ ] **Step 3: Implement `AgentKind` and `AgentState`**

Prepend to `src/detect/mod.rs`:

```rust
mod heuristic;
mod process;

pub use heuristic::classify_screen;
pub use process::agent_in_process_table;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Claude,
    Codex,
    Amp,
    Copilot,
    OpenCode,
    Gemini,
    Cursor,
    Droid,
    Qwen,
    Kimi,
    Kiro,
    Hermes,
    Pi,
    Antigravity,
    Vibe,
    Omp,
}

impl AgentKind {
    pub fn from_executable(name: &str) -> Option<Self> {
        Some(match name {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "amp" => Self::Amp,
            "copilot" | "github-copilot-cli" => Self::Copilot,
            "opencode" => Self::OpenCode,
            "gemini" => Self::Gemini,
            "cursor-agent" => Self::Cursor,
            "droid" => Self::Droid,
            "qwen" => Self::Qwen,
            "kimi" => Self::Kimi,
            "kiro" => Self::Kiro,
            "hermes" => Self::Hermes,
            "pi" => Self::Pi,
            "antigravity" => Self::Antigravity,
            "vibe" => Self::Vibe,
            "omp" => Self::Omp,
            _ => return None,
        })
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Amp => "amp",
            Self::Copilot => "copilot",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Cursor => "cursor",
            Self::Droid => "droid",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
            Self::Kiro => "kiro",
            Self::Hermes => "hermes",
            Self::Pi => "pi",
            Self::Antigravity => "antigravity",
            Self::Vibe => "vibe",
            Self::Omp => "omp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Blocked,
    Working,
    Done,
    Idle,
}

impl AgentState {
    /// Sort key for the sidebar. Blocked agents are the only ones that cost the user
    /// throughput while they wait, so they always surface first.
    pub const fn attention_rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Working => 1,
            Self::Done => 2,
            Self::Idle => 3,
        }
    }

    pub const fn glyph(self) -> char {
        match self {
            Self::Blocked | Self::Working => '●',
            Self::Done => '◍',
            Self::Idle => '○',
        }
    }
}
```

- [ ] **Step 4: Implement the process layer**

Create `src/detect/process.rs`:

```rust
use crate::detect::AgentKind;

/// Finds the agent executable running inside one Dock-owned process group.
///
/// Scoping to the pane's own PGID is what keeps this honest: Dock only ever classifies
/// processes it launched, so this can never become an adoption path for arbitrary PIDs.
/// `table` is the output of `ps -axo pid=,ppid=,pgid=,comm=`.
pub fn agent_in_process_table(table: &str, pgid: i32) -> Option<AgentKind> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _pid = fields.next()?;
            let _ppid = fields.next()?;
            let row_pgid: i32 = fields.next()?.parse().ok()?;
            let command = fields.next()?;
            (row_pgid == pgid).then_some(command)
        })
        .filter_map(|command| {
            let executable = std::path::Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(command);
            AgentKind::from_executable(executable)
        })
        .next()
}
```

- [ ] **Step 5: Implement the heuristic layer**

Create `src/detect/heuristic.rs`:

```rust
use std::sync::OnceLock;

use regex::RegexSet;

use crate::detect::{AgentKind, AgentState};

/// Screen-tail rules. This is the zero-configuration tier: it works for every agent on
/// first run with nothing installed. P1 replaces the producer with exact hook-reported
/// state for agents that support it, leaving `AgentState` itself unchanged.
const BLOCKED_PATTERNS: &[&str] = &[
    r"(?i)do you want to (proceed|continue)",
    r"(?i)\[y/n\]",
    r"(?i)press enter to continue",
    r"(?i)waiting for (your )?(input|approval)",
    r"(?i)allow this (tool|command)",
    r"(?i)^\s*[1-9]\.\s+(yes|no)\b",
];

const WORKING_PATTERNS: &[&str] = &[
    r"(?i)esc to interrupt",
    r"(?i)\b(thinking|working|running|generating|compiling|analyzing)\b\s*[.…]",
    r"(?i)tokens?\s*·",
];

const DONE_PATTERNS: &[&str] = &[
    r"(?i)\b(done|completed|finished)\b",
    r"(?i)all tests passed",
];

fn set(patterns: &[&str], cell: &'static OnceLock<RegexSet>) -> &'static RegexSet {
    cell.get_or_init(|| RegexSet::new(patterns).expect("embedded patterns must compile"))
}

/// Classifies an agent from the tail of its screen. Unknown output is `Idle` rather than a
/// guess: a wrong `Blocked` sends the user to a pane that does not need them, which is worse
/// than a missed one they will see on the next tick.
pub fn classify_screen(_agent: AgentKind, tail: &str) -> AgentState {
    static BLOCKED: OnceLock<RegexSet> = OnceLock::new();
    static WORKING: OnceLock<RegexSet> = OnceLock::new();
    static DONE: OnceLock<RegexSet> = OnceLock::new();
    if set(BLOCKED_PATTERNS, &BLOCKED).is_match(tail) {
        return AgentState::Blocked;
    }
    if set(WORKING_PATTERNS, &WORKING).is_match(tail) {
        return AgentState::Working;
    }
    if set(DONE_PATTERNS, &DONE).is_match(tail) {
        return AgentState::Done;
    }
    AgentState::Idle
}
```

Add `pub mod detect;` to `src/lib.rs` after `pub mod dashboard;`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib detect 2>&1 | tail -20`
Expected: PASS — 5 tests.

- [ ] **Step 7: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/detect/ src/lib.rs
git commit -m "feat: add heuristic agent detection with attention ordering"
```

---

### Task 5: Theme

**Files:**
- Create: `src/theme.rs`
- Modify: `src/lib.rs` (add `pub mod theme;`)

**Interfaces:**
- Consumes: `AgentState` from Task 4.
- Produces:
  - `pub struct Theme { pub accent, surface, muted, border, border_focused, text, blocked, working, done, idle: Color }`
  - `Theme::warm() -> Theme`
  - `Theme::agent(&self, state: AgentState) -> Color`
  - `Theme::border_type() -> BorderType`

- [ ] **Step 1: Write the failing test**

Create `src/theme.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;

    #[test]
    fn agent_states_map_to_distinct_colours() {
        let theme = Theme::warm();
        let colours = [
            theme.agent(AgentState::Blocked),
            theme.agent(AgentState::Working),
            theme.agent(AgentState::Done),
            theme.agent(AgentState::Idle),
        ];
        for (index, colour) in colours.iter().enumerate() {
            assert!(
                !colours[index + 1..].contains(colour),
                "state colours must be distinguishable"
            );
        }
    }

    #[test]
    fn focused_and_unfocused_borders_differ() {
        let theme = Theme::warm();
        assert_ne!(theme.border, theme.border_focused);
        assert_eq!(Theme::border_type(), BorderType::Rounded);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme 2>&1 | tail -20`
Expected: FAIL — `cannot find type Theme`.

- [ ] **Step 3: Implement the theme**

Prepend to `src/theme.rs`:

```rust
use ratatui::{style::Color, widgets::BorderType};

use crate::detect::AgentState;

/// Semantic tokens rather than raw colours, so P4 can load alternative palettes as data
/// without touching any render code. No colour may be hardcoded outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub surface: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focused: Color,
    pub text: Color,
    pub blocked: Color,
    pub working: Color,
    pub done: Color,
    pub idle: Color,
}

impl Theme {
    /// "Warm terminal-modern": a restrained amber-and-teal accent pair over a neutral
    /// surface, with saturation reserved for agent state so attention is never ambiguous.
    pub const fn warm() -> Self {
        Self {
            accent: Color::Rgb(232, 168, 88),
            surface: Color::Rgb(18, 18, 20),
            muted: Color::Rgb(122, 118, 112),
            border: Color::Rgb(58, 56, 54),
            border_focused: Color::Rgb(232, 168, 88),
            text: Color::Rgb(226, 222, 214),
            blocked: Color::Rgb(226, 106, 94),
            working: Color::Rgb(226, 184, 96),
            done: Color::Rgb(122, 176, 214),
            idle: Color::Rgb(108, 122, 114),
        }
    }

    pub const fn agent(&self, state: AgentState) -> Color {
        match state {
            AgentState::Blocked => self.blocked,
            AgentState::Working => self.working,
            AgentState::Done => self.done,
            AgentState::Idle => self.idle,
        }
    }

    pub const fn border_type() -> BorderType {
        BorderType::Rounded
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::warm()
    }
}
```

Add `pub mod theme;` to `src/lib.rs` after `pub mod storage;`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib theme 2>&1 | tail -20`
Expected: PASS — 2 tests.

- [ ] **Step 5: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/theme.rs src/lib.rs
git commit -m "feat: add warm terminal-modern theme tokens"
```

---

### Task 6: Runtime emulation and PTY sizing

**Files:**
- Modify: `src/runtime.rs:40-60` (delete `Scrollback`)
- Modify: `src/runtime.rs:88-190` (`launch`/`launch_with_before_lifecycle_publish` signatures)
- Modify: `src/runtime.rs:207-289` (`snapshot`)
- Modify: `src/runtime.rs:488-608` (`launch_child_with_before_spawn`, `read_pty`)

**Interfaces:**
- Consumes: `PaneScreen`, `ScreenSync` from Task 1.
- Produces:
  - `OwnedRuntime::launch(binding, adapter, scrollback_rows: usize, size: PtySize) -> OwnedRuntime`
  - `pub struct PtySize { pub rows: u16, pub cols: u16 }`
  - `OwnedRuntime::resize(&self, size: PtySize) -> Result<(), String>`
  - `OwnedRuntime::with_screen<T>(&self, f: impl FnOnce(&PaneScreen) -> T) -> T`
  - `RuntimeSnapshot` no longer carries `scrollback*`; it carries `rows`, `cols`, `title`, `cwd` (Task 7 defines the struct).

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/runtime.rs`:

```rust
#[test]
fn child_observes_the_requested_pty_size_and_a_later_resize() {
    let runtime = OwnedRuntime::launch_fixture(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "stty size; trap 'stty size' WINCH; sleep 5".into(),
        ],
        200,
        PtySize { rows: 30, cols: 100 },
    );
    wait_for_screen_text(&runtime, "30 100", Duration::from_secs(3));
    runtime.resize(PtySize { rows: 42, cols: 120 }).expect("resize owned pty");
    wait_for_screen_text(&runtime, "42 120", Duration::from_secs(3));
    let _ = runtime.stop();
}

#[test]
fn emulated_screen_renders_styled_output_rather_than_escape_text() {
    let runtime = OwnedRuntime::launch_fixture(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '\\033[1;32mgreen\\033[0m\\n'; sleep 5".into(),
        ],
        200,
        PtySize { rows: 24, cols: 80 },
    );
    wait_for_screen_text(&runtime, "green", Duration::from_secs(3));
    runtime.with_screen(|screen| {
        // The escape sequence must have been consumed by the emulator, not left as text.
        assert!(!screen.text_tail(24).contains("\u{1b}["));
        assert!(!screen.text_tail(24).contains("[1;32m"));
    });
    let _ = runtime.stop();
}

#[test]
fn resizing_an_exited_run_is_a_no_op_rather_than_an_error() {
    let runtime =
        OwnedRuntime::launch_fixture(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()], 200,
            PtySize { rows: 24, cols: 80 });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !runtime.lifecycle_is_terminal() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(runtime.resize(PtySize { rows: 40, cols: 100 }).is_ok());
}

fn wait_for_screen_text(runtime: &OwnedRuntime, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if runtime.with_screen(|screen| screen.text_tail(60).contains(needle)) {
            return;
        }
        assert!(Instant::now() < deadline, "never observed {needle:?} on the owned screen");
        thread::sleep(Duration::from_millis(20));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib runtime 2>&1 | tail -20`
Expected: FAIL — `cannot find type PtySize`.

- [ ] **Step 3: Replace `Scrollback` with the emulator**

In `src/runtime.rs`, delete the `Scrollback` struct and its `impl` (lines 40-60) and replace with:

```rust
use crate::terminal::PaneScreen;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl PtySize {
    fn to_winsize(self) -> nix::pty::Winsize {
        nix::pty::Winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }
}
```

Change the `OwnedRuntime` field `scrollback: Arc<Mutex<Scrollback>>` to `screen: Arc<Mutex<PaneScreen>>`, and add two fields:

```rust
    /// Retained clone of the PTY master purely so the pane can be resized. The reader and
    /// writer threads each own their own clone.
    pty_control: Option<Arc<File>>,
    size: Mutex<PtySize>,
```

Thread `scrollback_rows: usize` and `size: PtySize` through `launch`, `launch_with_before_lifecycle_publish`, and `launch_fixture`, constructing:

```rust
let screen = Arc::new(Mutex::new(PaneScreen::new(size.rows, size.cols, scrollback_rows)));
```

- [ ] **Step 4: Open the PTY at the requested size and keep a control handle**

In `launch_child_with_before_spawn`, replace `openpty(None, None)` with:

```rust
let winsize = size.to_winsize();
let pty = openpty(Some(&winsize), None)
    .map_err(|error| format!("could not allocate Dock-owned PTY: {error}"))?;
```

After `let master = File::from(pty.master);` add a third clone retained for resize:

```rust
let pty_control = master
    .try_clone()
    .map_err(|error| format!("could not clone PTY master for resize: {error}"))?;
```

Extend `ChildLaunch` to `(Child, u32, UnixStream, SyncSender<Vec<u8>>, Arc<File>)` and return `Arc::new(pty_control)` from it, storing it in the `pty_control` field at every construction site.

Change `read_pty` to feed the emulator:

```rust
fn read_pty(mut master: File, screen: Arc<Mutex<PaneScreen>>) {
    let mut buffer = [0_u8; 4096];
    while let Ok(count) = master.read(&mut buffer) {
        if count == 0 {
            break;
        }
        match screen.lock() {
            Ok(mut screen) => screen.feed(&buffer[..count]),
            Err(_) => break,
        }
    }
}
```

- [ ] **Step 5: Implement `resize` and `with_screen`**

Add to `impl OwnedRuntime`:

```rust
    /// Resizes the owned PTY and notifies the owned process group. A terminated run is a
    /// no-op rather than an error, and a stale PGID is never signalled — the group token can
    /// only originate from Dock's own successful launch.
    pub fn resize(&self, size: PtySize) -> Result<(), String> {
        {
            let mut current = self.size.lock().unwrap_or_else(|p| p.into_inner());
            if *current == size {
                return Ok(());
            }
            *current = size;
        }
        self.screen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resize(size.rows, size.cols);
        if self.lifecycle_is_terminal() {
            return Ok(());
        }
        let Some(control) = self.pty_control.as_ref() else {
            return Ok(());
        };
        let winsize = size.to_winsize();
        // SAFETY: the fd is a PTY master this runtime opened and still owns.
        let result = unsafe {
            nix::libc::ioctl(
                control.as_raw_fd(),
                nix::libc::TIOCSWINSZ as nix::libc::c_ulong,
                &winsize,
            )
        };
        if result == -1 {
            return Err(format!(
                "could not resize Dock-owned PTY: {}",
                Error::last_os_error()
            ));
        }
        self.signal(Signal::SIGWINCH)
    }

    pub fn with_screen<T>(&self, apply: impl FnOnce(&PaneScreen) -> T) -> T {
        let screen = self.screen.lock().unwrap_or_else(|p| p.into_inner());
        apply(&screen)
    }
```

Update `snapshot` to drop the four `scrollback*` fields and instead read `rows`, `cols`, `title`, `cwd` from the screen. Leave the remaining fields untouched.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib runtime 2>&1 | tail -30`
Expected: PASS. `dispatch.rs` will still fail to compile until Task 9; that is expected and Task 7 through 9 close it. If iterating task-by-task, temporarily satisfy callers with `PtySize { rows: 24, cols: 80 }`.

- [ ] **Step 7: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/runtime.rs
git commit -m "feat: emulate owned PTY output and bind PTY size to pane geometry"
```

---

### Task 7: Protocol v7

**Files:**
- Modify: `src/protocol.rs:1-15` (`PROTOCOL_VERSION`)
- Modify: `src/protocol.rs:14-30` (`Request`)
- Modify: `src/protocol.rs:45-63` (`DashboardProfile`)
- Modify: `src/protocol.rs:75-81` (`PaneInputRequest`)
- Modify: `src/protocol.rs:328-353` (`RuntimeSnapshot`)

**Interfaces:**
- Consumes: `AgentKind`, `AgentState` from Task 4.
- Produces:
  - `pub const PROTOCOL_VERSION: u16 = 7;`
  - `Request::PaneResize(PaneResizeRequest)`, `Request::Subscribe(SubscribeRequest)`
  - `pub struct PaneResizeRequest { workspace_id: String, pane_id: String, rows: u16, cols: u16 }`
  - `pub struct SubscribeRequest {}`
  - `PaneInputRequest.input: String` now carries **base64** bytes; helpers `PaneInputRequest::encode(&[u8]) -> String` and `PaneInputRequest::decode(&self) -> Result<Vec<u8>, String>`
  - `DashboardProfile::Shell`
  - `pub enum Event { PaneAttached { run_id, revision, screen: String }, PaneDelta { run_id, revision, bytes: String }, PaneState { run_id, state: ProcessState }, AgentStateChanged { run_id, agent: Option<AgentKind>, state: AgentState }, LayoutChanged }` (`screen`/`bytes` are base64)
  - `RuntimeSnapshot` fields: removed `scrollback`, `scrollback_bytes`, `scrollback_capacity_bytes`, `scrollback_truncated`; added `rows: u16`, `cols: u16`, `agent: Option<AgentKind>`, `agent_state: AgentState`, `title: Option<String>`, `cwd: Option<String>`

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/protocol.rs`:

```rust
#[test]
fn protocol_version_is_seven() {
    assert_eq!(PROTOCOL_VERSION, 7);
}

#[test]
fn pane_input_round_trips_arbitrary_key_bytes() {
    let raw = vec![0x1b, b'[', b'A', 0x00, 0xff];
    let request = PaneInputRequest {
        workspace_id: "w".into(),
        pane_id: "p".into(),
        input: PaneInputRequest::encode(&raw),
    };
    assert_eq!(request.decode().expect("decodes"), raw);
}

#[test]
fn snapshot_no_longer_carries_scrollback_and_reports_geometry() {
    let encoded = serde_json::to_string(&snapshot_fixture()).expect("serialize");
    assert!(!encoded.contains("scrollback"));
    assert!(encoded.contains("\"rows\":24"));
    assert!(encoded.contains("\"cols\":80"));
}

#[test]
fn events_round_trip_losslessly() {
    let event = Event::PaneDelta {
        run_id: "dock_1".into(),
        revision: 7,
        bytes: "aGk=".into(),
    };
    let encoded = serde_json::to_string(&event).expect("serialize");
    let decoded: Event = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(decoded, event);
}

#[test]
fn resize_request_rejects_unknown_fields() {
    let json = r#"{"workspace_id":"w","pane_id":"p","rows":24,"cols":80,"extra":1}"#;
    assert!(serde_json::from_str::<PaneResizeRequest>(json).is_err());
}
```

Add a `snapshot_fixture()` helper in the same test module constructing a `RuntimeSnapshot` with `rows: 24, cols: 80` and every other field filled with placeholder values.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib protocol 2>&1 | tail -20`
Expected: FAIL — `PROTOCOL_VERSION` is 6 and `Event` does not exist.

- [ ] **Step 3: Implement the protocol changes**

Set `pub const PROTOCOL_VERSION: u16 = 7;`. Add the two request variants and their structs, add `Shell` to `DashboardProfile` and its `From<DashboardProfile> for AdapterId` arm, and add:

```rust
use base64::{Engine as _, engine::general_purpose::STANDARD};

impl PaneInputRequest {
    /// Key bytes are base64 so raw control sequences survive JSON transport intact.
    pub fn encode(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    pub fn decode(&self) -> Result<Vec<u8>, String> {
        STANDARD
            .decode(&self.input)
            .map_err(|error| format!("pane input is not valid base64: {error}"))
    }
}

/// Pushed by the daemon to subscribed clients. Replaces polling entirely: an unchanged
/// pane produces no event at all, where the previous protocol re-sent full scrollback
/// for every run five times a second regardless of activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    PaneAttached { run_id: String, revision: u64, screen: String },
    PaneDelta { run_id: String, revision: u64, bytes: String },
    PaneState { run_id: String, state: ProcessState },
    AgentStateChanged {
        run_id: String,
        agent: Option<crate::detect::AgentKind>,
        state: crate::detect::AgentState,
    },
    LayoutChanged,
}
```

Update `RuntimeSnapshot` per the Interfaces block.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib protocol 2>&1 | tail -20`
Expected: PASS — 5 new tests.

- [ ] **Step 5: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/protocol.rs
git commit -m "feat: raise protocol to v7 with push events, resize, and binary pane input"
```

---

### Task 8: Shell adapter

**Files:**
- Modify: `src/adapter.rs:8-17` (`AdapterId`)
- Modify: `src/adapter.rs:98-120` (`default_executable`, `declared_capabilities`)
- Modify: `src/runtime.rs` (`environment_is_allowed`)

**Interfaces:**
- Consumes: nothing.
- Produces: `AdapterId::Shell`, resolving `$SHELL` and falling back to `/bin/sh`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/adapter.rs`:

```rust
#[test]
fn shell_adapter_resolves_a_login_shell() {
    let resolved = AdapterSelection {
        id: AdapterId::Shell,
        executable: None,
        arguments: vec!["-l".into()],
    }
    .resolve()
    .expect("shell must resolve on any supported platform");
    assert_eq!(resolved.id, AdapterId::Shell);
    assert_eq!(resolved.command.last().map(String::as_str), Some("-l"));
}

#[test]
fn shell_adapter_declares_no_provider_lifecycle() {
    assert!(!AdapterId::Shell.declared_capabilities().provider_lifecycle);
}
```

Add to `src/runtime.rs` tests:

```rust
#[test]
fn child_environment_allows_colour_capability_variables() {
    assert!(environment_is_allowed(std::ffi::OsStr::new("COLORTERM")));
    assert!(environment_is_allowed(std::ffi::OsStr::new("TERM")));
    assert!(!environment_is_allowed(std::ffi::OsStr::new("AWS_SECRET_ACCESS_KEY")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib adapter 2>&1 | tail -20`
Expected: FAIL — no variant `Shell`.

- [ ] **Step 3: Implement the adapter**

Add `Shell` to `AdapterId`. In `default_executable`, return `None` for `Shell` and handle it explicitly in `resolve` before the generic branch:

```rust
        if self.id == AdapterId::Shell {
            let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let executable = find_executable(&shell)
                .ok_or_else(|| format!("shell {shell:?} was not found or is not executable"))?;
            let mut command = vec![executable.display().to_string()];
            command.extend(self.arguments.clone());
            return Ok(ResolvedAdapter {
                id: AdapterId::Shell,
                executable,
                command,
                capabilities: AdapterCapabilities::NONE,
            });
        }
```

Add the `Shell => AdapterCapabilities::NONE` arm to `declared_capabilities`.

In `src/runtime.rs`, extend `environment_is_allowed` to include `"COLORTERM"` so agent TUIs get truecolor:

```rust
        "COLORTERM" | "HOME" | "LANG" | "LOGNAME" | "PATH" | "SHELL" | "TERM" | "TMPDIR" | "USER"
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib adapter 2>&1 | tail -20; cargo test --lib runtime::tests::child_environment 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/adapter.rs src/runtime.rs
git commit -m "feat: add shell adapter and allow COLORTERM for owned runs"
```

---

### Task 9: Registry resize, shell auto-launch, and snapshot wiring

**Files:**
- Modify: `src/dispatch.rs:33-56` (`RuntimeRegistry` fields)
- Modify: `src/dispatch.rs:209-230` (`new`, `with_capacity`)
- Modify: `src/dispatch.rs:333-377` (`terminal_launch`)
- Modify: `src/dispatch.rs:494-700` (`workspace`)
- Modify: `src/dispatch.rs:1527-1552` (`inspect`)

**Interfaces:**
- Consumes: `PtySize`, `OwnedRuntime::resize`, `OwnedRuntime::with_screen` (Task 6); `AdapterId::Shell` (Task 8); `classify_screen`, `agent_in_process_table` (Task 4).
- Produces:
  - `RuntimeRegistry::pane_resize(&self, workspace_id: &str, pane_id: &str, rows: u16, cols: u16) -> Result<(), (ErrorCode, String)>`
  - `RuntimeRegistry::with_run_screen<T>(&self, run_id: &str, f: impl FnOnce(&PaneScreen) -> T) -> Option<T>`
  - `RuntimeRegistry::new(state_dir, scrollback_rows: usize)` — capacity is now **rows**, default 2000.
  - `WorkspaceRequest::Create` and `::Split` auto-launch a `Shell` run into the new pane.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/dispatch.rs`:

```rust
#[test]
fn creating_a_workspace_launches_a_shell_so_the_pane_is_never_inert() {
    let registry = registry();
    registry
        .workspace(WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    let layout = registry.layout();
    let workspace = &layout.workspaces[0];
    let pane = &workspace.panes["p1"];
    assert!(pane.run_id.is_some(), "new pane must be bound to a shell run");
    assert_eq!(pane.runtime, PaneRuntime::Running);
}

#[test]
fn splitting_a_pane_launches_a_shell_in_the_new_pane() {
    let registry = registry();
    registry
        .workspace(WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    registry
        .workspace(WorkspaceRequest::Split {
            workspace_id: "w1".into(),
            pane_id: "p1".into(),
            new_pane_id: "p2".into(),
            axis: SplitAxis::Vertical,
        })
        .expect("split pane");
    let layout = registry.layout();
    assert!(layout.workspaces[0].panes["p2"].run_id.is_some());
}

#[test]
fn pane_resize_requires_a_live_owned_binding_and_reports_why() {
    let registry = registry();
    let error = registry
        .pane_resize("missing", "pane", 24, 80)
        .expect_err("unbound pane must be refused");
    assert_eq!(error.0, ErrorCode::InvalidBinding);
    assert!(error.1.contains("not bound"));
}

#[test]
fn pane_resize_reaches_the_exact_owned_runtime() {
    let registry = registry();
    registry
        .workspace(WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    registry.pane_resize("w1", "p1", 40, 120).expect("resize owned pane");
    let snapshot = registry.inspect(None).expect("inspect");
    let run = snapshot.iter().find(|run| run.pane_id == "p1").expect("bound run");
    assert_eq!((run.rows, run.cols), (40, 120));
}

#[test]
fn snapshots_report_agent_identity_and_state_without_screen_content() {
    let registry = registry();
    registry
        .workspace(WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    let snapshot = registry.inspect(None).expect("inspect");
    let encoded = serde_json::to_string(&snapshot).expect("serialize");
    assert!(!encoded.contains("scrollback"));
    let run = &snapshot[0];
    assert_eq!(run.agent, None, "a plain shell is not an agent");
    assert_eq!(run.agent_state, AgentState::Idle);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dispatch 2>&1 | tail -30`
Expected: FAIL — `no method named pane_resize`.

- [ ] **Step 3: Rename the capacity field and thread `PtySize`**

Rename `scrollback_capacity: usize` to `scrollback_rows: usize` throughout `RuntimeRegistry`, defaulting to `2000` in `new`. Add a `pane_sizes: Mutex<HashMap<String, PtySize>>` field recording the last known size per pane, defaulting to `PtySize { rows: 24, cols: 80 }` so a run launched before its pane has been measured still starts at a sane size.

Every `OwnedRuntime::launch` call site passes `self.scrollback_rows` and the pane's recorded `PtySize`.

- [ ] **Step 4: Implement `pane_resize` and `with_run_screen`**

```rust
    /// Resizes the PTY behind one Dock-owned pane. Records the size even when no run is
    /// bound yet, so a run launched later starts at the correct geometry rather than 80x24.
    pub fn pane_resize(
        &self,
        workspace_id: &str,
        pane_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), (ErrorCode, String)> {
        if rows == 0 || cols == 0 {
            return Err((
                ErrorCode::InvalidBinding,
                "pane size must be at least one row and one column".into(),
            ));
        }
        let size = PtySize { rows, cols };
        let run_id = self
            .layout
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pane_run(workspace_id, pane_id)
            .ok_or_else(|| {
                (
                    ErrorCode::InvalidBinding,
                    "pane is not bound to a live Dock-owned run".into(),
                )
            })?;
        self.pane_sizes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(format!("{workspace_id}/{pane_id}"), size);
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&run_id)
            .and_then(RuntimeSlot::active)
            .cloned();
        let Some(entry) = entry else {
            return Ok(());
        };
        entry
            .runtime
            .resize(size)
            .map_err(|message| (ErrorCode::UnsupportedOperation, message))
    }

    pub fn with_run_screen<T>(
        &self,
        run_id: &str,
        apply: impl FnOnce(&PaneScreen) -> T,
    ) -> Option<T> {
        let entry = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(run_id)
            .and_then(RuntimeSlot::active)
            .cloned()?;
        Some(entry.runtime.with_screen(apply))
    }
```

- [ ] **Step 5: Auto-launch a shell on create and split**

Extract the existing pane-binding logic into a private helper and call it from both arms:

```rust
    /// Every Dock pane is a working terminal from the moment it exists. This is a
    /// Dock-created PTY in a Dock-created process group like any other owned run, so the
    /// no-adoption invariant is untouched.
    fn launch_pane_shell(&self, workspace_id: &str, pane_id: &str, directory: &Path) {
        let run_id = format!("dock_sh_{workspace_id}_{pane_id}");
        let request = DispatchRequest {
            repository_root: directory.display().to_string(),
            external_task_ref: String::new(),
            run_id: run_id.clone(),
            worktree: directory.display().to_string(),
            adapter: AdapterSelection {
                id: AdapterId::Shell,
                executable: None,
                arguments: vec!["-l".into()],
            },
        };
        let binding = RunBinding {
            binding_kind: BindingKind::Terminal,
            repository_root: directory.to_path_buf(),
            external_task_ref: String::new(),
            run_id,
            worktree: directory.to_path_buf(),
            branch: String::new(),
            base_sha: String::new(),
            workspace_id: workspace_id.to_owned(),
            pane_id: pane_id.to_owned(),
        };
        // A shell that fails to launch must not fail workspace creation; the pane shows the
        // FailedToLaunch diagnostic and remains operable for close and relaunch.
        let _ = self.dispatch_with_binding(
            request,
            false,
            Some((workspace_id.to_owned(), pane_id.to_owned())),
            Some(binding),
        );
    }
```

Call it at the end of the `Create` and `Split` arms of `workspace`, using the daemon's runtime directory.

- [ ] **Step 6: Wire agent identity into snapshots**

In `inspect`, after building each `RuntimeSnapshot`, populate `agent` and `agent_state`:

```rust
        let agent = snapshot
            .process_group_id
            .and_then(|pgid| process_table().and_then(|table| agent_in_process_table(&table, pgid)));
        let agent_state = match agent {
            Some(kind) => self
                .with_run_screen(&snapshot.run_id, |screen| {
                    classify_screen(kind, &screen.text_tail(40))
                })
                .unwrap_or(AgentState::Idle),
            None => AgentState::Idle,
        };
        snapshot.agent = agent;
        snapshot.agent_state = agent_state;
```

with a module-level helper that shells out once per inspect rather than once per run:

```rust
fn process_table() -> Option<String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,comm="])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib dispatch 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 8: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/dispatch.rs
git commit -m "feat: auto-launch pane shells, resize owned PTYs, and report agent state"
```

---

### Task 10: Server stream mode and resize routing

**Files:**
- Modify: `src/server.rs:138-313` (`handle_connection_with_timeout`)

**Interfaces:**
- Consumes: `Event`, `PaneResizeRequest`, `SubscribeRequest` (Task 7); `RuntimeRegistry::pane_resize`, `with_run_screen` (Task 9); `ScreenSync` (Task 1).
- Produces: a connection that, after `Subscribe`, writes newline-delimited `Event` frames until the client disconnects.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/server.rs`:

```rust
#[test]
fn subscribe_streams_an_attach_snapshot_then_deltas() {
    let runtime = registry();
    runtime
        .workspace(crate::protocol::WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    let responses = exchange(
        &[
            &serde_json::to_string(&Request::Hello(HelloRequest { version: 7 })).unwrap(),
            &serde_json::to_string(&Request::Subscribe(SubscribeRequest {})).unwrap(),
        ],
        &runtime,
    );
    let events = collect_events(&responses);
    assert!(
        events.iter().any(|event| matches!(event, Event::PaneAttached { .. })),
        "first frame for a bound pane must be a full attach snapshot"
    );
}

#[test]
fn resize_request_is_routed_to_the_registry() {
    let runtime = registry();
    runtime
        .workspace(crate::protocol::WorkspaceRequest::Create {
            workspace_id: "w1".into(),
            name: "Daily".into(),
            pane_id: "p1".into(),
        })
        .expect("create workspace");
    let responses = exchange(
        &[
            &serde_json::to_string(&Request::Hello(HelloRequest { version: 7 })).unwrap(),
            &serde_json::to_string(&Request::PaneResize(PaneResizeRequest {
                workspace_id: "w1".into(),
                pane_id: "p1".into(),
                rows: 40,
                cols: 120,
            }))
            .unwrap(),
        ],
        &runtime,
    );
    assert!(!matches!(responses[1], Response::Error { .. }));
}

#[test]
fn version_six_clients_are_refused_with_an_actionable_message() {
    let runtime = registry();
    let responses = exchange(
        &[&serde_json::to_string(&Request::Hello(HelloRequest { version: 6 })).unwrap()],
        &runtime,
    );
    match &responses[0] {
        Response::Error { code, message } => {
            assert_eq!(*code, ErrorCode::ProtocolMismatch);
            assert!(message.contains("7"));
        }
        other => panic!("expected protocol mismatch, got {other:?}"),
    }
}
```

Add a `collect_events(&[Response]) -> Vec<Event>` helper that parses `Response::Stream { event }` frames.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib server 2>&1 | tail -30`
Expected: FAIL — no `Request::Subscribe`.

- [ ] **Step 3: Route `PaneResize`**

Add to the request loop in `handle_connection_with_timeout`, alongside the existing `PaneInput` arm:

```rust
            Ok(Request::PaneResize(request)) => match runtime.pane_resize(
                &request.workspace_id,
                &request.pane_id,
                request.rows,
                request.cols,
            ) {
                Ok(()) => write_response(stream, &Response::Ack)?,
                Err((code, message)) => write_response(stream, &Response::Error { code, message })?,
            },
```

Change the existing `PaneInput` arm to decode base64 first:

```rust
            Ok(Request::PaneInput(request)) => match request.decode() {
                Ok(bytes) => match runtime.pane_input(&request.workspace_id, &request.pane_id, &bytes) {
                    Ok(count) => write_response(stream, &Response::Accepted { bytes: count })?,
                    Err((code, message)) => {
                        write_response(stream, &Response::Error { code, message })?
                    }
                },
                Err(message) => write_response(
                    stream,
                    &Response::Error { code: ErrorCode::InvalidBinding, message },
                )?,
            },
```

- [ ] **Step 4: Implement stream mode**

Add a `Subscribe` arm that takes over the connection:

```rust
            Ok(Request::Subscribe(_)) => {
                return stream_events(stream, runtime);
            }
```

and the streaming loop:

```rust
/// Pushes screen deltas until the client disconnects. Each run gets a `ScreenSync`
/// tracking what this subscriber has already seen, so an unchanged pane costs nothing.
fn stream_events(stream: &mut UnixStream, runtime: &RuntimeRegistry) -> Result<(), String> {
    let mut syncs: HashMap<String, ScreenSync> = HashMap::new();
    let mut revisions: HashMap<String, u64> = HashMap::new();
    let mut states: HashMap<String, (Option<AgentKind>, AgentState)> = HashMap::new();
    loop {
        let snapshots = runtime.inspect(None).unwrap_or_default();
        for snapshot in &snapshots {
            let revision = revisions.entry(snapshot.run_id.clone()).or_default();
            let attached = syncs.contains_key(&snapshot.run_id);
            let sync = syncs
                .entry(snapshot.run_id.clone())
                .or_insert_with(|| ScreenSync::new(snapshot.rows, snapshot.cols));
            let Some(delta) = runtime.with_run_screen(&snapshot.run_id, |screen| {
                if !attached {
                    return screen.state_bytes();
                }
                sync.delta_from(screen)
            }) else {
                continue;
            };
            if delta.is_empty() {
                // An idle pane produces no traffic at all.
            } else {
                sync.apply(&delta);
                *revision += 1;
                let encoded = STANDARD.encode(&delta);
                let event = if attached {
                    Event::PaneDelta {
                        run_id: snapshot.run_id.clone(),
                        revision: *revision,
                        bytes: encoded,
                    }
                } else {
                    Event::PaneAttached {
                        run_id: snapshot.run_id.clone(),
                        revision: *revision,
                        screen: encoded,
                    }
                };
                write_response(stream, &Response::Stream { event })?;
            }
            let current = (snapshot.agent, snapshot.agent_state);
            if states.get(&snapshot.run_id) != Some(&current) {
                states.insert(snapshot.run_id.clone(), current);
                write_response(
                    stream,
                    &Response::Stream {
                        event: Event::AgentStateChanged {
                            run_id: snapshot.run_id.clone(),
                            agent: current.0,
                            state: current.1,
                        },
                    },
                )?;
            }
        }
        thread::sleep(Duration::from_millis(16));
    }
}
```

Add `Response::Stream { event: Event }` and `Response::Ack` to `src/protocol.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib server 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 6: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/server.rs src/protocol.rs
git commit -m "feat: stream pane deltas over the socket and route pane resize"
```

---

### Task 11: Client event stream and input path

**Files:**
- Modify: `src/client.rs` (add a streaming connection)
- Modify: `src/main.rs:433-531` (`run_dashboard`)

**Interfaces:**
- Consumes: `Event` (Task 7), `Keymap`/`KeyOutcome` (Task 3), `PaneScreen` (Task 1).
- Produces:
  - `Client::subscribe(socket: &Path) -> Result<mpsc::Receiver<Event>, String>`
  - `Dashboard::apply_event(&mut self, event: Event)`
  - `Dashboard::screens: HashMap<String, PaneScreen>`

- [ ] **Step 1: Write the failing test**

Add to `src/dashboard.rs` tests:

```rust
#[test]
fn attach_then_delta_events_reconstruct_the_pane_screen() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut dashboard = dashboard();
    let mut source = dock::terminal::VtTerminal::new(24, 80, 0);
    source.feed(b"first line\r\n");
    dashboard.apply_event(Event::PaneAttached {
        run_id: "run_1".into(),
        revision: 1,
        screen: STANDARD.encode(source.state_bytes()),
    });
    let mut sync = dock::terminal::ScreenSync::new(24, 80);
    sync.apply(&source.state_bytes());
    source.feed(b"second line\r\n");
    let delta = sync.delta_from(&source);
    dashboard.apply_event(Event::PaneDelta {
        run_id: "run_1".into(),
        revision: 2,
        bytes: STANDARD.encode(&delta),
    });
    let rendered = dashboard.screen_text("run_1").expect("screen present");
    assert!(rendered.contains("first line"));
    assert!(rendered.contains("second line"));
}

#[test]
fn a_revision_gap_drops_the_screen_so_the_client_re_attaches() {
    let mut dashboard = dashboard();
    dashboard.apply_event(Event::PaneAttached {
        run_id: "run_1".into(),
        revision: 1,
        screen: String::new(),
    });
    dashboard.apply_event(Event::PaneDelta {
        run_id: "run_1".into(),
        revision: 9,
        bytes: String::new(),
    });
    assert!(dashboard.screen_text("run_1").is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::attach_then_delta 2>&1 | tail -20`
Expected: FAIL — `no method named apply_event`.

- [ ] **Step 3: Implement `apply_event` and `screen_text`**

Add to `Dashboard`:

```rust
    pub screens: HashMap<String, PaneScreen>,
    revisions: HashMap<String, u64>,
```

```rust
    /// Feeds a pushed event into the client's own emulator. A non-contiguous revision means
    /// this client missed bytes, so the screen is dropped and the daemon re-attaches it with
    /// a full snapshot rather than rendering a corrupted grid.
    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::PaneAttached { run_id, revision, screen } => {
                let mut terminal = PaneScreen::new(24, 80, 0);
                if let Ok(bytes) = STANDARD.decode(&screen) {
                    terminal.feed(&bytes);
                }
                self.screens.insert(run_id.clone(), terminal);
                self.revisions.insert(run_id, revision);
            }
            Event::PaneDelta { run_id, revision, bytes } => {
                let expected = self.revisions.get(&run_id).map(|value| value + 1);
                if expected != Some(revision) {
                    self.screens.remove(&run_id);
                    self.revisions.remove(&run_id);
                    return;
                }
                if let (Some(terminal), Ok(decoded)) =
                    (self.screens.get_mut(&run_id), STANDARD.decode(&bytes))
                {
                    terminal.feed(&decoded);
                    self.revisions.insert(run_id, revision);
                }
            }
            Event::AgentStateChanged { run_id, agent, state } => {
                self.agents.insert(run_id, (agent, state));
            }
            Event::PaneState { .. } | Event::LayoutChanged => self.needs_refresh = true,
        }
    }

    #[cfg(test)]
    pub fn screen_text(&self, run_id: &str) -> Option<String> {
        self.screens.get(run_id).map(|screen| screen.text_tail(24))
    }
```

- [ ] **Step 4: Implement `Client::subscribe`**

Add to `src/client.rs`:

```rust
    /// Opens a second connection dedicated to pushed events and returns a receiver fed by a
    /// reader thread, so the render loop never blocks on the socket.
    pub fn subscribe(socket: &Path) -> Result<mpsc::Receiver<Event>, String> {
        let mut stream = Self::connect(socket)?;
        stream.request(&Request::Subscribe(SubscribeRequest {}))?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("dock-event-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stream.into_inner());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(Response::Stream { event }) = serde_json::from_str(&line)
                        && sender.send(event).is_err()
                    {
                        break;
                    }
                    line.clear();
                }
            })
            .map_err(|error| format!("could not start event reader: {error}"))?;
        Ok(receiver)
    }
```

- [ ] **Step 5: Replace polling with the event loop in `run_dashboard`**

Delete the `event::poll(...) { refresh(...) }` fallback at `src/main.rs:459-462`. Replace the loop body with:

```rust
        while let Ok(event) = events.try_recv() {
            dashboard.apply_event(event);
        }
        if dashboard.take_refresh() {
            refresh(client, &mut dashboard)?;
        }
        terminal.draw(|frame| dashboard.render(frame)).map_err(|e| e.to_string())?;
        for (workspace_id, pane_id, rows, cols) in dashboard.take_pending_resizes() {
            let _ = client.request(&Request::PaneResize(PaneResizeRequest {
                workspace_id,
                pane_id,
                rows,
                cols,
            }));
        }
        if !event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? {
            continue;
        }
```

Key handling routes through `UiCommand::PaneInput(bytes)`, sent without awaiting the reply so keypress-to-paint never waits for the daemon.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib dashboard 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 7: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/client.rs src/main.rs src/dashboard.rs
git commit -m "feat: drive the dashboard from pushed events instead of polling"
```

---

### Task 12: Dashboard rendering

**Files:**
- Modify: `src/dashboard.rs:116-192` (`render`)
- Modify: `src/dashboard.rs:193-208` (`render_header`)
- Modify: `src/dashboard.rs:210-275` (`render_sidebar`)
- Modify: `src/dashboard.rs:276-356` (`render_node`)
- Modify: `src/dashboard.rs:391-424` (`render_help`)
- Modify: `src/dashboard.rs:448-547` (`key`)
- Modify: `src/dashboard.rs:1255-1745` (rewrite the 13 existing tests)

**Interfaces:**
- Consumes: `Theme` (Task 5), `Keymap`/`KeyOutcome`/`PaneCommand` (Task 3), `AgentState` (Task 4), `PaneScreen` (Task 1).
- Produces: rendering only; no new public API beyond Task 11's.

- [ ] **Step 1: Write the failing tests**

Replace the 13 tests in `src/dashboard.rs` with equivalents asserting the new contract. Add these:

```rust
#[test]
fn a_bound_pane_renders_emulated_screen_content_not_binding_facts() {
    let mut dashboard = dashboard();
    dashboard.apply_event(attach_event("run_1", b"hello from the shell\r\n"));
    let rendered = render_to_string(&mut dashboard, 100, 30);
    assert!(rendered.contains("hello from the shell"));
    assert!(!rendered.contains("No Dock-owned run bound"));
}

#[test]
fn keys_reach_the_pane_and_the_prefix_opens_command_mode() {
    let mut dashboard = dashboard();
    let outcome = dashboard.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(matches!(outcome, UiCommand::PaneInput(bytes) if bytes == b"x"));
    let pending = dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(pending, UiCommand::None);
    assert!(dashboard.prefix_pending());
}

#[test]
fn which_key_hints_appear_only_while_the_prefix_is_pending() {
    let mut dashboard = dashboard();
    assert!(!render_to_string(&mut dashboard, 100, 30).contains("split"));
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(render_to_string(&mut dashboard, 100, 30).contains("split"));
}

#[test]
fn sidebar_lists_agents_with_blocked_first() {
    let mut dashboard = dashboard();
    dashboard.apply_event(Event::AgentStateChanged {
        run_id: "run_idle".into(),
        agent: Some(AgentKind::Amp),
        state: AgentState::Idle,
    });
    dashboard.apply_event(Event::AgentStateChanged {
        run_id: "run_blocked".into(),
        agent: Some(AgentKind::Claude),
        state: AgentState::Blocked,
    });
    let rendered = render_to_string(&mut dashboard, 100, 30);
    let claude = rendered.find("claude").expect("claude listed");
    let amp = rendered.find("amp").expect("amp listed");
    assert!(claude < amp, "blocked agents must sort above idle ones");
}

#[test]
fn resizing_a_pane_queues_exactly_one_resize_request() {
    let mut dashboard = dashboard();
    render_to_string(&mut dashboard, 100, 30);
    let first = dashboard.take_pending_resizes();
    assert_eq!(first.len(), 1);
    render_to_string(&mut dashboard, 100, 30);
    assert!(dashboard.take_pending_resizes().is_empty(), "unchanged geometry must not re-send");
}
```

Add a `render_to_string(&mut Dashboard, width: u16, height: u16) -> String` helper using `ratatui::backend::TestBackend`, and an `attach_event(run_id, bytes)` helper.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dashboard 2>&1 | tail -30`
Expected: FAIL.

- [ ] **Step 3: Render panes through `PseudoTerminal`**

Replace the `LayoutNode::Pane` arm of `render_node`. The pane body becomes the emulated screen; binding facts move to the title:

```rust
            LayoutNode::Pane { pane_id } => {
                self.pane_areas.insert(pane_id.clone(), area);
                let pane = &workspace.panes[pane_id];
                let focused = workspace.focused_pane_id == *pane_id;
                let run_id = pane.run_id.as_deref();
                let (agent, state) = run_id
                    .and_then(|id| self.agents.get(id).copied())
                    .unwrap_or((None, AgentState::Idle));
                let label = agent.map_or_else(|| pane.name.clone(), |kind| kind.label().to_owned());
                let title = format!(" {} {} · {} ", state.glyph(), label, self.pane_location(pane));
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(Theme::border_type())
                    .title(title)
                    .title_style(Style::default().fg(self.theme.agent(state)))
                    .border_style(Style::default().fg(if focused {
                        self.theme.border_focused
                    } else {
                        self.theme.border
                    }));
                let inner = block.inner(area);
                self.queue_resize(&workspace.workspace_id, pane_id, inner);
                frame.render_widget(block, area);
                match run_id.and_then(|id| self.screens.get(id)) {
                    Some(screen) => frame.render_widget(
                        PseudoTerminal::new(screen.screen()).cursor(
                            tui_term::widget::Cursor::default().visibility(focused),
                        ),
                        inner,
                    ),
                    None => frame.render_widget(
                        Paragraph::new("starting…").style(Style::default().fg(self.theme.muted)),
                        inner,
                    ),
                }
            }
```

`queue_resize` records `(workspace_id, pane_id, inner.height, inner.width)` only when it differs from the last recorded geometry for that pane.

- [ ] **Step 4: Route keys through the keymap**

Replace the body of `Dashboard::key` so forms and help take priority, then:

```rust
        match self.keymap.handle(key, self.encoding_for_focused_pane()) {
            KeyOutcome::Passthrough(bytes) => UiCommand::PaneInput(bytes),
            KeyOutcome::Command(command) => self.run_command(command),
            KeyOutcome::PendingPrefix | KeyOutcome::Ignored => UiCommand::None,
        }
```

`run_command` maps each `PaneCommand` onto the existing `split`, `focus_next`, `rename`, `close`, `open_launch`, `resize_keyboard` methods. Delete `input_mode` and every reference to it.

- [ ] **Step 5: Apply the theme to header, sidebar, and footer**

Replace every hardcoded `Color::` in `src/dashboard.rs` with a `self.theme` token. The sidebar AGENTS section sorts by `state.attention_rank()` then label. The footer shows `Keymap::hints()` when `self.keymap.is_pending()`, otherwise a one-line summary ending in `Ctrl+B ? help`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib dashboard 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 7: Run all gates and commit**

```bash
cargo fmt && cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets 2>&1 | grep -E "^test result"
git add src/dashboard.rs
git commit -m "feat: render emulated panes with theme, agent roster, and which-key hints"
```

---

### Task 13: Documentation, parity matrix, and smoke scripts

**Files:**
- Modify: `README.md`
- Modify: `docs/terminal-runtime-parity.md`
- Modify: `scripts/smoke-slice61-macos.sh`
- Modify: `scripts/smoke-slice62-nongit-macos.sh`

**Interfaces:**
- Consumes: everything.
- Produces: documentation matching shipped behaviour.

- [ ] **Step 1: Update the parity matrix**

Set these rows to their new status: "Terminal input" becomes **Shipped** with "Full VT emulation via `vt100`; unprefixed keys reach the PTY; `Ctrl+B` is the command prefix; `Esc` is forwarded". "Themes/configuration" becomes **Partial** — "Warm terminal-modern theme shipped; loading alternative palettes is deferred". Add new rows for "PTY resize" (Shipped), "Agent state detection" (Shipped, heuristic tier), and "Shell panes" (Shipped). Change the "Bounded live scrollback" row to describe a **row** budget.

- [ ] **Step 2: Update the README**

Replace the keymap paragraph with the `Ctrl+B` table. Delete the sentence describing `i` input mode. Add a short "Agent awareness" section describing the four states and that detection is display-only for external processes. State the protocol version is 7 and that a v6 daemon must be stopped.

- [ ] **Step 3: Update the smoke scripts**

Replace any `i`-prefixed input assertions with `Ctrl+B` sequences. Add an assertion to `smoke-slice62-nongit-macos.sh` that a freshly created workspace has a running shell without an explicit launch.

- [ ] **Step 4: Run the full verification suite**

```bash
cargo fmt --check
cargo test --all-targets 2>&1 | grep -E "^test result"
cargo clippy --all-targets --all-features -- -D warnings
scripts/smoke-slice5-macos.sh
scripts/smoke-slice6-macos.sh
scripts/smoke-slice61-macos.sh
scripts/smoke-slice62-nongit-macos.sh
```
Expected: all green.

- [ ] **Step 5: Manual acceptance walkthrough**

Confirm each item from the spec's acceptance list:

```bash
cd /tmp && mkdir -p dock-accept && cd dock-accept
cargo run --manifest-path /Users/krunal/Development/dock/Cargo.toml --bin dock
```

1. A shell pane accepts input immediately in a non-Git directory.
2. `vim` runs correctly including alternate screen and `Esc`.
3. `claude` or `codex` redraws correctly after `Ctrl+B v` changes its width.
4. Four panes each accept typing with no perceptible lag.
5. The sidebar shows agent identity and state, blocked first.
6. `Ctrl+B ?` lists bindings; `Ctrl+B Ctrl+B` types a literal `^B`.

- [ ] **Step 6: Commit**

```bash
git add README.md docs/ scripts/
git commit -m "docs: describe the real-terminal runtime, prefix keymap, and agent states"
```

---

## Self-Review

**Spec coverage.** Every spec section maps to a task: emulation and OSC capture → 1; key encoding → 2; prefix keymap → 3; detection → 4; theme → 5; PTY sizing and `Scrollback` removal → 6; protocol v7 → 7; shell adapter → 8; shell auto-launch, resize routing, snapshot wiring → 9; push stream → 10; client event loop and input path → 11; rendering and the 13 rewritten tests → 12; parity matrix, README, smoke scripts, acceptance list → 13. Error-handling requirements are covered by the resize no-op test (Task 6), the revision-gap test (Task 11), the shell-launch-failure comment (Task 9), and the v6 refusal test (Task 10).

**Deliberate deviation from the spec.** The spec sketched `PaneTerminal` as a trait with `delta_since(&self, prev: &Self)`. That signature cannot be used through a trait object, and a probe showed the dual-parser approach is both simpler and sufficient. The swap point is therefore the `pub type PaneScreen = VtTerminal;` alias, which preserves the stated intent — one-line replacement by `rio-vt` — at zero vtable cost. The spec was amended to match (commit follows this plan).

**Second deviation.** The probe proved `contents_diff` loses cursor position, so `state_diff`/`state_formatted` are mandatory. Both the Global Constraints section here and the amended spec now state this explicitly.

**Type consistency.** `PaneScreen` is used identically in Tasks 1, 6, 9, 11, 12. `AgentState`/`AgentKind` are defined in Task 4 and consumed in 5, 7, 9, 10, 12. `PtySize` is defined in Task 6 and consumed in 9. `KeyEncoding` is defined in Task 2 and consumed in 3 and 12. `Event` is defined in Task 7 and consumed in 10, 11, 12. `ScreenSync` is defined in Task 1 and consumed in 10 and 11. `Response::Stream` and `Response::Ack` are added in Task 10 and consumed in 11.

**Known ordering hazard.** Task 6 changes `OwnedRuntime::launch`'s signature, which `src/dispatch.rs` calls; the tree does not fully compile again until Task 9. Task 6 Step 6 states this and gives the temporary shim. Executors running strict per-task gates should treat Tasks 6 through 9 as one gate boundary.
