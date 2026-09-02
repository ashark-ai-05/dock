//! What Dock concluded about a receipt, and how it is drawn.
//!
//! The verdict is arithmetic over evidence, never judgement of it: the rules that produce
//! it land with the receipt store. This module is only the vocabulary, declared early so
//! the shapes are settled before anything renders them.

/// Dock's conclusion about one receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Every declared check was witnessed green at head, and no finding fired.
    Clear,
    /// One or more findings, each named, none fatal.
    Look,
    /// A declared check ran and exited non-zero.
    Failed,
}

impl Verdict {
    /// The shape, chosen so the three survive greyscale and a compressed screenshot.
    ///
    /// None of these may collide with `AgentState::glyph`, which draws `○ ◐ ◉ ◆` one row
    /// away in the same spine. The circles are a fill gradient of progress; the verdict
    /// marks are deliberately not circles at all.
    pub const fn glyph(self) -> char {
        match self {
            Self::Clear => '✓',
            Self::Look => '!',
            Self::Failed => '✗',
        }
    }

    /// The word, for readers who have not yet learned the shape.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Look => "look",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;
    use std::collections::HashSet;

    /// Three verdicts, three shapes. Colour is not enough: roughly 8% of men have a
    /// red-green deficiency, and a terminal tool travels as a compressed screenshot where
    /// hue is the first thing lost.
    #[test]
    fn the_three_verdicts_are_three_shapes() {
        let shapes: HashSet<char> = [Verdict::Clear, Verdict::Look, Verdict::Failed]
            .into_iter()
            .map(Verdict::glyph)
            .collect();
        assert_eq!(shapes.len(), 3);
    }

    /// A verdict and an agent state are drawn in the same spine, one under the other. If
    /// any glyph appeared in both vocabularies, a row would be ambiguous about which
    /// question it was answering.
    #[test]
    fn no_verdict_shape_collides_with_an_agent_state_shape() {
        let states: HashSet<char> = [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Done,
            AgentState::Blocked,
        ]
        .into_iter()
        .map(AgentState::glyph)
        .collect();
        for verdict in [Verdict::Clear, Verdict::Look, Verdict::Failed] {
            assert!(
                !states.contains(&verdict.glyph()),
                "{:?} draws as {}, which is already an agent state",
                verdict,
                verdict.glyph()
            );
        }
    }

    /// Every verdict says what it means in words, because the spine is read by people who
    /// have not yet learned the shapes.
    #[test]
    fn every_verdict_has_a_label() {
        assert_eq!(Verdict::Clear.label(), "clear");
        assert_eq!(Verdict::Look.label(), "look");
        assert_eq!(Verdict::Failed.label(), "failed");
    }
}
