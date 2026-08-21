use std::sync::OnceLock;

use regex::RegexSet;

use crate::detect::{AgentKind, AgentState};

/// Screen-tail rules. This is the zero-configuration tier: it works for every agent on
/// first run with nothing installed. P1 replaces the producer with exact hook-reported
/// state for agents that support it, leaving `AgentState` itself unchanged.
// `^`/`$` anchor to the whole haystack, not per-line, unless `(?m)` is set. `text_tail` is
// always multi-line, so any pattern anchoring within a line (not just at the very start of
// the tail) must carry `(?m)`. Audited every pattern below: only the numbered-choice
// pattern used `^` without it.
const BLOCKED_PATTERNS: &[&str] = &[
    r"(?i)do you want to (proceed|continue)",
    r"(?i)\[y/n\]",
    r"(?i)press enter to continue",
    r"(?i)waiting for (your )?(input|approval)",
    r"(?i)allow this (tool|command)",
    r"(?mi)^\s*[1-9]\.\s+(yes|no)\b",
];

const WORKING_PATTERNS: &[&str] = &[
    r"(?i)esc to interrupt",
    r"(?i)\b(thinking|working|running|generating|compiling|analyzing)\b\s*[.…]",
    r"(?i)tokens?\s*·",
];

// Deliberately conservative: `Done` and `Idle` are adjacent low-attention ranks, so missing
// a completion costs the user almost nothing, while a false `Done` on a busy pane (e.g. any
// screen containing the word "done", such as `cargo` output or a commit message) is constant
// visible noise. Only match structured completion statements, never a bare substring.
const DONE_PATTERNS: &[&str] = &[
    r"(?mi)^\s*[✓✔√]\s",
    r"(?mi)^\s*(all\s+)?(tasks?|tests?|builds?|checks?)\s+(have\s+)?(passed|completed|succeeded)\b",
    r"(?mi)^\s*(task|work)\s+(is\s+)?(complete|completed|finished)\b",
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
