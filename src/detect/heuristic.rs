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

/// Screens that mean the agent is sitting at its own input box with nothing left to do but wait
/// for the user.
///
/// Per-agent, because the only dependable evidence is the chrome each CLI paints around that box,
/// and no two paint the same one. Every pattern here was read from a real session captured through
/// a PTY, never guessed: a wrong one reports an agent as wanting attention it does not want, which
/// is the failure mode this whole roster exists to avoid.
///
/// An agent with no verified pattern gets none. It keeps the previous behaviour rather than
/// inheriting another agent's chrome, which would be a guess wearing the costume of a fact.
fn awaiting_input_set(agent: AgentKind) -> Option<&'static RegexSet> {
    // Cached per agent like every other set here: this runs for every pane on every screen tick,
    // so compiling a pattern here would be a cost paid thousands of times to answer the same
    // question.
    static CLAUDE: OnceLock<RegexSet> = OnceLock::new();
    static CODEX: OnceLock<RegexSet> = OnceLock::new();
    match agent {
        // The mode footer sits directly under Claude Code's input box and is painted whenever it
        // is accepting typing — on first launch and again after it finishes answering.
        AgentKind::Claude => Some(set(&[r"(?i)\(shift\+tab to cycle\)"], &CLAUDE)),
        // Codex prints this placeholder inside its input box only while that box is empty, which
        // makes it a narrower but stricter signal than Claude's: it cannot fire on a half-typed
        // message, and equally it will not fire on one, so a half-typed prompt reads as idle.
        AgentKind::Codex => Some(set(&[r"(?i)ask codex to do anything"], &CODEX)),
        _ => None,
    }
}

/// Classifies an agent from the tail of its screen.
///
/// Order is the whole design. Working is tested before waiting, because an agent mid-turn still
/// paints the input chrome it will return to, and reporting a busy agent as needing attention is
/// the most expensive mistake available here. An explicit question outranks both: it is the one
/// state where the agent has genuinely stopped.
///
/// Waiting at an empty prompt is reported as `Blocked` rather than `Idle`. Both mean the same
/// thing to the person reading the roster — it is your turn — and `Blocked` is what sorts to the
/// top and colours for attention. Unknown output stays `Idle`, since a wrong call to attention is
/// worse than a missed one the next tick will catch.
pub fn classify_screen(agent: AgentKind, tail: &str) -> AgentState {
    static BLOCKED: OnceLock<RegexSet> = OnceLock::new();
    static WORKING: OnceLock<RegexSet> = OnceLock::new();
    static DONE: OnceLock<RegexSet> = OnceLock::new();
    if set(BLOCKED_PATTERNS, &BLOCKED).is_match(tail) {
        return AgentState::Blocked;
    }
    if set(WORKING_PATTERNS, &WORKING).is_match(tail) {
        return AgentState::Working;
    }
    if awaiting_input_set(agent).is_some_and(|waiting| waiting.is_match(tail)) {
        return AgentState::Blocked;
    }
    if set(DONE_PATTERNS, &DONE).is_match(tail) {
        return AgentState::Done;
    }
    AgentState::Idle
}

#[cfg(test)]
mod awaiting_input_tests {
    use super::*;

    /// The footer Claude Code paints under its input box, captured from a real session through a
    /// PTY rather than written from memory.
    const CLAUDE_AWAITING: &str = concat!(
        "❯ Try \"edit protocol.rs to...\"\n",
        "────────────────────────────────\n",
        "  Opus 5 (1M context) · ⎇ main · dock · $0.00 · 6s\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );

    /// The same session mid-turn: the input chrome is still painted, and the interrupt hint is
    /// the only thing that distinguishes it.
    const CLAUDE_WORKING: &str = concat!(
        "✻ Thinking… (7s · esc to interrupt)\n",
        "  Opus 5 (1M context) · ⎇ main · dock · $0.12 · 31s\n",
        "  ⏵⏵ auto mode on (shift+tab to cycle)\n",
    );

    #[test]
    fn claude_waiting_at_its_prompt_is_reported_as_wanting_the_user() {
        // The reported bug: an agent that had answered and was waiting showed as Idle, so the
        // roster gave no sign that it was the user's turn.
        assert_eq!(
            classify_screen(AgentKind::Claude, CLAUDE_AWAITING),
            AgentState::Blocked
        );
    }

    #[test]
    fn a_working_agent_is_never_reported_as_wanting_the_user() {
        // Claude paints the same input chrome while it works, so waiting must be tested after
        // working. Getting this backwards would call every busy agent to the top of the roster.
        assert_eq!(
            classify_screen(AgentKind::Claude, CLAUDE_WORKING),
            AgentState::Working
        );
    }

    #[test]
    fn an_explicit_question_still_outranks_everything() {
        let asking = format!("Do you want to proceed?\n{CLAUDE_AWAITING}");
        assert_eq!(
            classify_screen(AgentKind::Claude, &asking),
            AgentState::Blocked
        );
    }

    #[test]
    fn codex_waiting_at_its_empty_prompt_is_reported_as_wanting_the_user() {
        // Captured from a real Codex session: the placeholder is painted only while the input box
        // is empty.
        let idle = concat!(
            "› Ask Codex to do anything\n",
            "  gpt-5.6-sol default · ~/Development/dock\n",
        );
        assert_eq!(classify_screen(AgentKind::Codex, idle), AgentState::Blocked);
        // …and Claude must not answer for Codex's chrome either.
        assert_eq!(classify_screen(AgentKind::Amp, idle), AgentState::Idle);
    }

    #[test]
    fn an_agent_with_no_verified_pattern_does_not_borrow_claudes() {
        // Amp paints different chrome and none of it has been captured yet. Inheriting Claude's
        // would be a guess presented as a fact.
        for agent in [AgentKind::Amp, AgentKind::Copilot] {
            assert_eq!(
                classify_screen(agent, CLAUDE_AWAITING),
                AgentState::Idle,
                "{agent:?} must not inherit another agent's prompt chrome"
            );
        }
    }

    #[test]
    fn unrelated_output_is_still_idle_rather_than_a_guess() {
        assert_eq!(
            classify_screen(AgentKind::Claude, "cargo build\n   Compiling dock v0.1.0\n"),
            AgentState::Idle
        );
    }
}
