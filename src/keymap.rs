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
    /// Move the visible workspace by this many places. Negative is earlier.
    Workspace(i8),
    /// Show every workspace at once and jump to one by name.
    WorkspacePicker,
    /// Complete a path from the focused pane's directory into that pane.
    FilePicker,
    /// Relaunch the agent that last ran here, continuing its most recent session.
    ResumeAgent,
    /// Show the handoffs agents are waiting on a human decision for.
    Review,
    /// Show the task board and dispatch an agent onto one of its tasks.
    Board,
    /// Split the focused pane and make the new half a board on the canvas.
    ///
    /// Distinct from [`Board`](Self::Board), which opens the same board as a popup over the
    /// canvas. Both are kept: the overlay is what you want when the board is a thing you glance
    /// at, and a pane is what you want when it is a thing you work from.
    SplitBoard,
    /// Show what has changed in the focused pane's worktree.
    Git,
    /// Jump straight to the workspace in this 1-based position, if it exists.
    WorkspaceJump(u8),
    Resize(i16),
    Zoom,
    Rename,
    Close,
    /// Close the visible workspace, and everything running in it.
    CloseWorkspace,
    /// Give an exited pane a fresh shell. The keyboard recovery path out of a dead pane.
    Respawn,
    Launch,
    /// Freeze the focused pane's viewport for keyboard selection and yanking.
    CopyMode,
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
            ("n", "new"),
            // Paired rather than listed separately: the bar has two rows and every column spent
            // repeating the word "split" is a column the last binding needs to stay visible.
            ("h/v", "split ⇋⇵"),
            // One entry for the focus keys: Shift reversing a cycle and arrows being directional
            // are both obvious, and spelling them out cost the columns the last binding needs.
            ("Tab/S-Tab ←↑→↓", "focus"),
            (",/. w 1-9", "workspace"),
            ("f", "file"),
            ("a", "resume"),
            ("i", "review"),
            // Paired like `h/v`: the same board, as a popup or as a pane on the canvas. A
            // separate entry for the second one pushed the last binding out of the two-row bar.
            ("k/B", "board ▤"),
            ("g", "git"),
            ("[", "copy mode"),
            ("+/-", "resize"),
            ("z", "zoom"),
            ("r", "rename"),
            ("x", "close"),
            ("R", "restart"),
            ("l", "launch"),
            ("d", "leave · runs keep running"),
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
        // Uppercase for the same reason `R` is: closing a workspace takes every pane in it, and
        // that must not be one slipped finger away from closing a single pane with `x`.
        KeyCode::Char('X') => PaneCommand::CloseWorkspace,
        // Uppercase so it cannot be reached by the same finger slip that hits `r` for rename:
        // respawning is a state change, and `r` is already taken.
        KeyCode::Char('R') => PaneCommand::Respawn,
        KeyCode::Char('l') => PaneCommand::Launch,
        KeyCode::Char('d') => PaneCommand::Detach,
        KeyCode::Char('?') => PaneCommand::Help,
        KeyCode::Char('q') => PaneCommand::Quit,
        // `[` is tmux's copy-mode key and copying is the far more frequent act, so workspace
        // cycling moved to the unshifted `,`/`.` pair rather than keeping the bracket.
        KeyCode::Char('[') => PaneCommand::CopyMode,
        KeyCode::Char(',') => PaneCommand::Workspace(-1),
        KeyCode::Char('.') => PaneCommand::Workspace(1),
        // `w` is tmux's window list, and a workspace is the nearest thing Dock has to a window.
        KeyCode::Char('w') => PaneCommand::WorkspacePicker,
        // `f` for file. The picker completes a path into the pane rather than deciding what to do
        // with it, so `vim ` first opens it and an agent prompt simply gains the path.
        KeyCode::Char('f') => PaneCommand::FilePicker,
        // `a` for agent. Distinct from `R`, which always gives a plain shell: Dock records
        // explicit decisions rather than inferring them, so continuing a conversation is asked
        // for by name and never guessed at from what happened to be running.
        KeyCode::Char('a') => PaneCommand::ResumeAgent,
        // `i` for inbox. The one queue in Dock a person rather than a process drains.
        KeyCode::Char('i') => PaneCommand::Review,
        // `k` for kanban.
        KeyCode::Char('k') => PaneCommand::Board,
        // Shifted, for the same reason `R` and `X` are: this one changes the canvas, and `b` is
        // the prefix key itself — pressing it twice already means "send a literal Ctrl+B".
        KeyCode::Char('B') => PaneCommand::SplitBoard,
        KeyCode::Char('g') => PaneCommand::Git,
        // Cycling is fine for two workspaces and miserable for eight. A digit is the only way to
        // reach a distant workspace in constant time. `0` is deliberately unbound: the positions
        // are 1-based on screen, so binding it would place a tenth workspace under a key that
        // reads as the first.
        KeyCode::Char(position @ '1'..='9') => PaneCommand::WorkspaceJump(position as u8 - b'0'),
        KeyCode::Char('+') => PaneCommand::Resize(50),
        KeyCode::Char('-') => PaneCommand::Resize(-50),
        KeyCode::Tab => PaneCommand::Focus(FocusDirection::Next),
        // Shift+Tab cycles backwards nearly everywhere it exists, and without it
        // `FocusDirection::Previous` has no key at all.
        KeyCode::BackTab => PaneCommand::Focus(FocusDirection::Previous),
        KeyCode::Left => PaneCommand::Focus(FocusDirection::Left),
        KeyCode::Right => PaneCommand::Focus(FocusDirection::Right),
        KeyCode::Up => PaneCommand::Focus(FocusDirection::Up),
        KeyCode::Down => PaneCommand::Focus(FocusDirection::Down),
        _ => return None,
    })
}

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
        assert_eq!(
            keymap.handle(prefix(), KeyEncoding::default()),
            KeyOutcome::PendingPrefix
        );
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
        assert_eq!(
            keymap.handle(plain('§'), KeyEncoding::default()),
            KeyOutcome::Ignored
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn shift_tab_after_the_prefix_cycles_focus_backwards() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyEncoding::default()
            ),
            KeyOutcome::Command(PaneCommand::Focus(FocusDirection::Previous))
        );
        assert!(!keymap.is_pending());
    }

    #[test]
    fn comma_and_period_after_the_prefix_cycle_workspaces() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain('.'), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::Workspace(1))
        );
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain(','), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::Workspace(-1))
        );
    }

    #[test]
    fn the_bracket_after_the_prefix_opens_copy_mode_and_no_longer_cycles_workspaces() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain('['), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::CopyMode)
        );
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain(']'), KeyEncoding::default()),
            KeyOutcome::Ignored,
            "the closing bracket lost its binding when workspace cycling moved to ,/."
        );
    }

    #[test]
    fn published_hints_cover_every_documented_binding() {
        let keys: Vec<&str> = Keymap::hints().iter().map(|(key, _)| *key).collect();
        for expected in [
            "n",
            "h/v",
            "z",
            "r",
            "R",
            "x",
            "l",
            "d",
            "?",
            "q",
            "Tab/S-Tab ←↑→↓",
            "f",
            "a",
            "i",
            // The board's two surfaces share one entry, as `h/v` does: same board, once as a
            // popup over the canvas and once as a pane on it.
            "k/B",
            "g",
            "[",
            // The three ways to reach a workspace share one entry: the footer is two rows, and
            // listing them separately pushed the last published binding off the end of it.
            ",/. w 1-9",
        ] {
            assert!(keys.contains(&expected), "missing hint for {expected}");
        }
    }

    #[test]
    fn the_board_pane_key_is_the_shifted_form_of_the_board_overlay_key() {
        // Requirement 4 keeps the overlay, so the two surfaces must not share a key. `b` itself
        // is unavailable — it is the prefix, and pressing it twice already means "send a literal
        // Ctrl+B" — which is why the pane is on the shifted `B` beside the overlay's `k`.
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain('k'), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::Board)
        );
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(
                KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
                KeyEncoding::default()
            ),
            KeyOutcome::Command(PaneCommand::SplitBoard)
        );
        // And an unshifted `b` after the prefix is still the literal Ctrl+B it always was.
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(prefix(), KeyEncoding::default()),
            KeyOutcome::Passthrough(vec![0x02])
        );
    }

    #[test]
    fn a_digit_jumps_to_a_workspace_by_position_and_zero_stays_unbound() {
        let mut keymap = Keymap::new();
        for (character, position) in [('1', 1_u8), ('5', 5), ('9', 9)] {
            keymap.handle(prefix(), KeyEncoding::default());
            assert_eq!(
                keymap.handle(plain(character), KeyEncoding::default()),
                KeyOutcome::Command(PaneCommand::WorkspaceJump(position))
            );
        }
        // Positions are 1-based on screen, so `0` would put a tenth workspace under a key that
        // reads as the first. It reaches the pane as ordinary input instead.
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain('0'), KeyEncoding::default()),
            KeyOutcome::Ignored
        );
    }

    #[test]
    fn w_opens_the_workspace_picker() {
        let mut keymap = Keymap::new();
        keymap.handle(prefix(), KeyEncoding::default());
        assert_eq!(
            keymap.handle(plain('w'), KeyEncoding::default()),
            KeyOutcome::Command(PaneCommand::WorkspacePicker)
        );
    }
}
