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

#[cfg(test)]
mod tests {
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
