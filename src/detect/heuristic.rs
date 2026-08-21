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
