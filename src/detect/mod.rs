pub(crate) mod heuristic;
pub mod process;

pub use heuristic::{
    ScreenRead, classify_screen, classify_screen_titled, read_screen, read_screen_titled,
};
pub mod manifest;
pub use process::agent_in_process_table;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    Aider,
    Devin,
    Kilo,
    Qoder,
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
            "aider" => Self::Aider,
            // Binary names read from each tool's own install instructions. Qoder is listed both
            // ways in the wild; an alias that matches nothing costs nothing, while a missing one
            // means the agent is simply never seen.
            "devin" => Self::Devin,
            "kilo" => Self::Kilo,
            "qoder" | "qodercli" => Self::Qoder,
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
            Self::Aider => "aider",
            Self::Devin => "devin",
            Self::Kilo => "kilo",
            Self::Qoder => "qoder",
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

    /// What the state means to the person reading the roster.
    ///
    /// A coloured glyph alone cannot carry this: it says something changed without saying what,
    /// and "your turn" is the one state worth crossing the room for.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Blocked => "needs you",
            Self::Working => "working",
            // Its turn is over and yours has started, but it is not stuck: it will wait as long
            // as it takes. Distinct from "needs you", which means it cannot continue at all.
            Self::Done => "your turn",
            Self::Idle => "idle",
        }
    }

    pub const fn glyph(self) -> char {
        match self {
            Self::Idle => '○',
            Self::Working => '◐',
            Self::Done => '◉',
            Self::Blocked => '◆',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentKind, AgentState, classify_screen, classify_screen_titled};
    use crate::terminal::VtTerminal;

    #[test]
    fn each_agent_state_has_its_own_glyph() {
        assert_eq!(AgentState::Idle.glyph(), '○');
        assert_eq!(AgentState::Working.glyph(), '◐');
        assert_eq!(AgentState::Done.glyph(), '◉');
        assert_eq!(AgentState::Blocked.glyph(), '◆');
        let glyphs = [
            AgentState::Idle.glyph(),
            AgentState::Working.glyph(),
            AgentState::Done.glyph(),
            AgentState::Blocked.glyph(),
        ];
        for (index, glyph) in glyphs.iter().enumerate() {
            assert!(
                !glyphs[index + 1..].contains(glyph),
                "a shared glyph cannot tell states apart"
            );
        }
    }

    #[test]
    fn every_agent_herdr_lists_is_recognised_by_its_own_binary_name() {
        // Breadth is the whole point of a multiplexer for agents: one it cannot name is one it
        // cannot show a state for, however good the rules behind that state are.
        for (executable, expected) in [
            ("claude", AgentKind::Claude),
            ("codex", AgentKind::Codex),
            ("copilot", AgentKind::Copilot),
            ("cursor-agent", AgentKind::Cursor),
            ("droid", AgentKind::Droid),
            ("kimi", AgentKind::Kimi),
            ("opencode", AgentKind::OpenCode),
            ("hermes", AgentKind::Hermes),
            ("pi", AgentKind::Pi),
            ("devin", AgentKind::Devin),
            ("kilo", AgentKind::Kilo),
            ("qoder", AgentKind::Qoder),
            ("qodercli", AgentKind::Qoder),
        ] {
            assert_eq!(
                AgentKind::from_executable(executable),
                Some(expected),
                "{executable} is not recognised"
            );
        }
        // And an unrelated binary is still not an agent.
        assert_eq!(AgentKind::from_executable("bash"), None);
    }

    /// End to end through OSC 2: the window title is a dedicated field, and a spinner in it is
    /// work even when the body has no patterns of its own. Concatenating that title onto the body
    /// is not how classification is asked — splash art on the first body row must not count.
    #[test]
    fn an_agent_that_only_says_it_is_working_in_its_title_is_read_as_working() {
        let mut screen = VtTerminal::new(24, 100, 100);
        screen.feed("\x1b]2;\u{2839} amp\x07".as_bytes());
        screen.feed("╰  gpt-5 thinking ─\r\n".as_bytes());
        assert_eq!(
            classify_screen_titled(
                AgentKind::Amp,
                &screen.visible_text(),
                screen.title().as_deref(),
            ),
            AgentState::Working,
            "the OSC title is the only evidence there is, and it is enough"
        );

        // When the turn ends the agent rewrites the title without the spinner, and the same screen
        // stops reading as work.
        screen.feed(b"\x1b]2;amp\x07");
        assert_ne!(
            classify_screen_titled(
                AgentKind::Amp,
                &screen.visible_text(),
                screen.title().as_deref(),
            ),
            AgentState::Working,
            "the spinner going away is what says the turn ended"
        );
    }

    /// The title spinner is a property of the window title, never of the first body row, for
    /// every agent Dock will launch. A welcome splash that happens to start with braille or a
    /// quarter-circle is idle chrome, not work.
    #[test]
    fn splash_art_on_the_first_body_row_is_never_a_working_title() {
        let splash = "\u{28ff}\u{28ff}\u{28ff}  welcome\n> \n";
        for agent in [
            AgentKind::Amp,
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Copilot,
        ] {
            assert_ne!(
                classify_screen(agent, splash),
                AgentState::Working,
                "{agent:?} must not treat body art as a spinner title"
            );
            let mut screen = VtTerminal::new(24, 80, 50);
            screen.feed(splash.replace('\n', "\r\n").as_bytes());
            assert!(screen.title().is_none() || screen.title().as_deref() == Some(""));
            assert_ne!(
                classify_screen_titled(agent, &screen.visible_text(), screen.title().as_deref()),
                AgentState::Working,
                "{agent:?} through a real PTY screen with no OSC title"
            );
        }
        let mut working = VtTerminal::new(24, 80, 50);
        working.feed("\x1b]2;\u{2839} amp\x07".as_bytes());
        working.feed(splash.replace('\n', "\r\n").as_bytes());
        assert_eq!(
            classify_screen_titled(
                AgentKind::Amp,
                &working.visible_text(),
                working.title().as_deref(),
            ),
            AgentState::Working,
            "the same splash with a real spinner title is still work"
        );
    }

    /// The bug this guards: every chooser pattern was matched against a cursor-anchored tail, and
    /// a chooser leaves the cursor on the highlighted option with its instructions underneath. The
    /// patterns were correct and unreachable, so the roster said "idle" while the agent waited.
    #[test]
    fn a_chooser_is_recognised_through_the_screen_the_agent_actually_painted() {
        let mut screen = VtTerminal::new(24, 100, 100);
        screen.feed(b"\xe2\x9c\xb3 Cogitated for 4s\r\n\r\n");
        screen.feed(b"What location would you like the weather for?\r\n\r\n");
        screen.feed(b"  1. Sydney, AU\r\n  2. Melbourne, AU\r\n  3. Brisbane, AU\r\n\r\n");
        screen.feed("Enter to select · ↑/↓ to navigate · Esc to cancel\r\n".as_bytes());
        // Where such a program leaves the cursor: on the highlighted option, not at the end.
        screen.feed(b"\x1b[5;3H");

        assert_eq!(
            classify_screen(AgentKind::Claude, &screen.text_tail(40)),
            AgentState::Idle,
            "the tail cannot see the chooser, which is the whole defect"
        );
        assert_eq!(
            classify_screen(AgentKind::Claude, &screen.visible_text()),
            AgentState::Blocked,
            "a chooser is the agent stuck, not the agent finished"
        );
    }

    use super::*;

    #[test]
    fn maps_known_executables_and_rejects_unknown_ones() {
        assert_eq!(
            AgentKind::from_executable("claude"),
            Some(AgentKind::Claude)
        );
        assert_eq!(AgentKind::from_executable("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::from_executable("amp"), Some(AgentKind::Amp));
        assert_eq!(
            AgentKind::from_executable("copilot"),
            Some(AgentKind::Copilot)
        );
        assert_eq!(
            AgentKind::from_executable("github-copilot-cli"),
            Some(AgentKind::Copilot)
        );
        assert_eq!(AgentKind::from_executable("aider"), Some(AgentKind::Aider));
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
        assert_eq!(
            classify_screen(AgentKind::Claude, tail),
            AgentState::Blocked
        );
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
    fn classifies_a_numbered_yes_no_prompt_on_a_later_line_as_blocked() {
        // Regression for the `^` anchor missing `(?m)`: without multiline mode this list,
        // sitting one line below the header, would never match and the pane would read Idle.
        let tail = "Select an option:\n  1. Yes\n  2. No\n";
        assert_eq!(
            classify_screen(AgentKind::Claude, tail),
            AgentState::Blocked
        );
    }

    #[test]
    fn does_not_misclassify_ordinary_terminal_output_containing_done_as_done() {
        assert_eq!(
            classify_screen(
                AgentKind::Claude,
                "Finished dev profile [unoptimized] target(s) in 2.3s"
            ),
            AgentState::Idle
        );
        assert_eq!(
            classify_screen(AgentKind::Claude, "npm WARN deprecated foo@1.0.0 ... done"),
            AgentState::Idle
        );
        assert_eq!(
            classify_screen(
                AgentKind::Claude,
                "commit 3fa91c2  fix: done with the parser refactor"
            ),
            AgentState::Idle
        );
    }

    #[test]
    fn classifies_a_genuine_completion_marker_as_done() {
        assert_eq!(
            classify_screen(AgentKind::Claude, "✓ All tests passed"),
            AgentState::Done
        );
    }

    #[test]
    fn finds_the_agent_running_beneath_one_process_group_leader_only() {
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
