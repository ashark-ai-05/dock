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
    // A chooser is the state that matters most and the one the yes/no rule cannot see: its
    // options are content ("Melbourne, AU", "Sydney, AU"), not the words yes and no. These match
    // the dialog's own chrome instead, which is fixed text an agent does not write by accident.
    // Matching the numbered options themselves was the alternative and would fire on any prose
    // list an agent happened to print, which is the false call to attention this file exists to
    // avoid.
    r"(?i)enter to select",
    r"(?i)(↑/↓|up/down) to navigate",
    r"(?i)esc to cancel",
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
/// Waiting at an empty prompt is `Done`, not `Blocked`. The two look alike — in both the agent has
/// stopped and it is your turn — but they are not worth the same to the person reading the roster.
/// `Blocked` means the agent asked something and cannot continue without an answer: a permission
/// prompt, a chooser. `Done` means it finished its turn and will wait indefinitely. Reporting the
/// second as the first was tried, and it makes every finished agent shout for attention until
/// nothing in the roster means anything. Unknown output stays `Idle`, since a wrong call to
/// attention is worse than a missed one the next tick will catch.
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
        return AgentState::Done;
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
    fn claude_that_has_finished_its_turn_is_done_rather_than_blocking() {
        // It is your turn, but the agent is not stuck: it answered and will wait indefinitely.
        // Reporting this as Blocked was tried and made every finished agent shout for attention,
        // which leaves nothing in the roster meaning anything.
        assert_eq!(
            classify_screen(AgentKind::Claude, CLAUDE_AWAITING),
            AgentState::Done
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
    fn a_chooser_whose_options_are_not_yes_or_no_still_reports_as_blocking() {
        // Captured from a real pane: Claude replaced its input box with a selection dialog, so
        // none of the input-box chrome was on screen and the yes/no rule could not see options
        // that are place names. It read as idle while the agent sat waiting.
        let chooser = concat!(
            "Which location should I get the weather for?\n",
            "\n",
            "❯ 1. Melbourne, AU\n",
            "     Local timezone in this session is AEST, so Melbourne is a likely fit.\n",
            "  2. Sydney, AU\n",
            "  3. San Francisco, US\n",
            "\n",
            "Enter to select · ↑/↓ to navigate · Esc to cancel\n",
        );
        assert_eq!(
            classify_screen(AgentKind::Claude, chooser),
            AgentState::Blocked
        );
        // The chooser chrome is not Claude's alone, so every agent gets the benefit.
        assert_eq!(
            classify_screen(AgentKind::Amp, chooser),
            AgentState::Blocked
        );
    }

    #[test]
    fn a_chooser_outranks_a_finished_turn_because_only_one_of_them_is_stuck() {
        // Both screens carry Claude's input chrome. The chooser is the one that cannot continue
        // without an answer, and it is the only one worth calling a person over.
        let chooser = format!("{CLAUDE_AWAITING}\nEnter to select · Esc to cancel\n");
        assert_eq!(
            classify_screen(AgentKind::Claude, &chooser),
            AgentState::Blocked
        );
        assert_eq!(
            classify_screen(AgentKind::Claude, CLAUDE_AWAITING),
            AgentState::Done
        );
        // And attention order keeps the stuck one above the finished one.
        assert!(AgentState::Blocked.attention_rank() < AgentState::Done.attention_rank());
    }

    #[test]
    fn a_numbered_list_in_ordinary_prose_is_not_mistaken_for_a_chooser() {
        // The reason this keys on dialog chrome rather than on the options: agents print numbered
        // lists constantly, and every one of them would otherwise call the user over.
        let prose = concat!(
            "Here is the plan:\n",
            "1. Read the config\n",
            "2. Patch the parser\n",
            "3. Run the tests\n",
        );
        assert_eq!(classify_screen(AgentKind::Claude, prose), AgentState::Idle);
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
    fn codex_that_has_finished_its_turn_is_done_rather_than_blocking() {
        // Captured from a real Codex session: the placeholder is painted only while the input box
        // is empty.
        let idle = concat!(
            "› Ask Codex to do anything\n",
            "  gpt-5.6-sol default · ~/Development/dock\n",
        );
        assert_eq!(classify_screen(AgentKind::Codex, idle), AgentState::Done);
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
