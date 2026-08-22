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
    /// Background for the copy-mode selection run. A background token rather than a
    /// foreground one, so highlighted text keeps whatever colour the program gave it.
    pub selection: Color,
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
            selection: Color::Rgb(58, 84, 102),
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
    fn the_selection_background_is_distinct_from_every_other_token() {
        let theme = Theme::warm();
        for other in [
            theme.accent,
            theme.surface,
            theme.muted,
            theme.border,
            theme.border_focused,
            theme.text,
            theme.blocked,
            theme.working,
            theme.done,
            theme.idle,
        ] {
            assert_ne!(
                theme.selection, other,
                "a highlight that matches another token is not a highlight"
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
