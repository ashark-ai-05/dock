use std::sync::OnceLock;

use regex::RegexSet;

use crate::{AgentKind, AgentState};

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
    //
    // The *submission* affordance, never the cancellation one. An agent offers a way to submit
    // exactly while it is holding a question open; it offers a way to cancel whenever there is
    // anything at all to cancel, which includes its own turn. "esc to cancel" used to sit in this
    // list and was the single worst rule in the file: GitHub Copilot prints it for the whole
    // duration of a response, so a Copilot pane read `Blocked` — "needs you" — from the moment it
    // started generating until it stopped, and no chooser could be told apart from ordinary work
    // because one rule answered both. It now lives under `WORKING_PATTERNS`, where the affordance
    // it actually describes belongs.
    //
    // The four spellings are the ones real agents paint: "enter to select", "enter to confirm",
    // "enter to submit", "enter accept".
    r"(?i)enter\s+(?:to\s+)?(?:select|confirm|submit|accept)\b",
    r"(?i)(↑/↓|up/down) to navigate",
];

const WORKING_PATTERNS: &[&str] = &[
    // The interrupt affordance, which is the most broadly true thing an agent's screen says about
    // itself: it advertises a way to interrupt exactly while there is something to interrupt.
    // Widened from a literal "esc to interrupt" to the family, because the intervening words are
    // the only part that varies between agents — "esc to cancel", "esc cancel", "esc again to
    // cancel", "esc interrupt". `\s` rather than a literal space so the phrase survives being
    // wrapped by a narrow pane, which `visible_text` joins with a newline.
    r"(?i)\besc\b(?:\s+\w+){0,3}\s+(?:cancel|interrupt)\b",
    r"(?i)\b(thinking|working|running|generating|compiling|analyzing)\b\s*[.…]",
    r"(?i)tokens?\s*·",
    // The title spinner is deliberately *not* here; see `title_says_working`.
];

/// Whether the agent's *window title* (OSC 0/2) carries a spinner.
///
/// This is the one piece of evidence that does not depend on reading chrome off the screen: it
/// survives scrolling, survives the footer being off-screen, and survives an agent whose body
/// Dock has no patterns for — which is how Amp, which has no rules of its own, gets a working
/// state.
///
/// It must not look at the first body row. Amp's idle welcome splash leads with braille art, and
/// treating that row as a title made every idle Amp pane read as working. The spinner counts only
/// when it is the OSC/window title (or a dedicated title field passed in beside the body).
///
/// Code rather than a pattern in `WORKING_PATTERNS`, and the reason is measured. Written as a
/// regex this is `\A[ \t]*[…]\s` — anchored, so it should cost nothing. In a `RegexSet` it costs
/// a great deal: the set matches every alternative at once against one haystack, and a pattern
/// with no literal to search for denies the whole set its literal prefilter. Adding it took
/// `WORKING_PATTERNS` from 0.0031ms to 0.1057ms per pane per pass — a 34x rise paid on every
/// screen, to run an anchored test that reads at most three characters. Here it reads those three
/// characters and stops.
///
/// Both glyph families are spinners and nothing else — braille for most agents, the quarter
/// circles Claude Code moved to in 2.1.228. Claude's other title glyph, `✳`, is deliberately
/// absent because it marks the *finished* title, and `·` is absent because it is ordinary
/// punctuation in agent output, as the token-count rule above shows.
fn title_says_working(title: Option<&str>) -> bool {
    let Some(title) = title else {
        return false;
    };
    let mut glyphs = title.trim_start_matches([' ', '\t']).chars();
    let Some('\u{2800}'..='\u{28FF}' | '\u{25D0}'..='\u{25D3}') = glyphs.next() else {
        return false;
    };
    // A spinner is a glyph the title leads with, not one that happens to open a word.
    glyphs.next().is_none_or(char::is_whitespace)
}

// Deliberately conservative: `Done` and `Idle` are adjacent low-attention ranks, so missing
// a completion costs the user almost nothing, while a false `Done` on a busy pane (e.g. any
// screen containing the word "done", such as `cargo` output or a commit message) is constant
// visible noise. Only match structured completion statements, never a bare substring.
const DONE_PATTERNS: &[&str] = &[
    r"(?mi)^\s*[✓✔√]\s",
    r"(?mi)^\s*(all\s+)?(tasks?|tests?|builds?|checks?)\s+(have\s+)?(passed|completed|succeeded)\b",
    r"(?mi)^\s*(task|work)\s+(is\s+)?(complete|completed|finished)\b",
];

/// The chrome Claude Code paints when, and only when, it is between turns.
///
/// Two rules rather than one because this is the single piece of evidence that a Claude pane has
/// handed the turn back, and one regex over one string is a single point of failure: the roster
/// flipped to "working" every time the footer hint rotated or a frame was sampled part-way through
/// a repaint. Both tolerate whitespace where the footer has spaces, because a narrow pane wraps
/// that line and [`visible_text`](dock_pty::terminal::VtTerminal::visible_text) joins rows with a
/// newline — `\s` matches it, and the leading indent of the row it wrapped onto, so the phrase
/// survives being split at any of its spaces.
///
/// Safe to widen only in this direction. `classify_screen` tests working before awaiting, so a
/// mid-turn Claude — which paints this same footer — is caught by "esc to interrupt" first. A
/// pattern that is also painted *while working* would not be, and would report a streaming agent
/// as finished.
const CLAUDE_AWAITING_PATTERNS: &[&str] = &[
    r"(?i)shift\s*\+\s*tab\s+to\s+cycle",
    r"(?i)\?\s+for\s+shortcuts",
];

/// Gemini CLI's empty input box, which Qwen Code inherited when it forked from it.
const GEMINI_AWAITING_PATTERNS: &[&str] = &[r"(?i)type\s+your\s+message\s+or\s+@path"];

fn set(patterns: &[&str], cell: &'static OnceLock<RegexSet>) -> &'static RegexSet {
    cell.get_or_init(|| RegexSet::new(patterns).expect("embedded patterns must compile"))
}

/// The rules compiled into Dock, as `(blocked, working, awaiting)`.
///
/// The awaiting patterns are per-agent, because the only dependable evidence is the chrome each
/// CLI paints around its input box and no two paint the same one. Every one was read from a real
/// session captured through a PTY, never guessed: a wrong one reports an agent as wanting
/// attention it does not want, which is the failure this roster exists to avoid. An agent with no
/// verified pattern gets none rather than inheriting another's, which would be a guess wearing the
/// costume of a fact.
///
/// Going without one is no longer fatal to an agent's roster entry: the caller reads a pane that
/// has genuinely fallen silent as finished whether or not it recognised any chrome, so a missing
/// pattern now costs a slower answer rather than a permanently wrong one. It used to cost the
/// latter, which is why every agent below Codex read as working forever.
///
/// These are the starting point, not the last word: a manifest under
/// `~/.config/dock/agent-detection/<agent>.json` replaces any of the three, so an agent that
/// respells its prompts after this release is a file somebody edits rather than a version of Dock
/// somebody waits for.
pub(crate) fn built_in(
    agent: AgentKind,
) -> (
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
) {
    (
        BLOCKED_PATTERNS,
        WORKING_PATTERNS,
        match agent {
            AgentKind::Claude => CLAUDE_AWAITING_PATTERNS,
            AgentKind::Codex => &[r"(?i)ask codex to do anything"],
            // Gemini CLI paints its placeholder only while the input box is empty and no turn is
            // running, which is exactly the question being asked here.
            AgentKind::Gemini => GEMINI_AWAITING_PATTERNS,
            // Qwen Code is a fork of Gemini CLI and inherited its input box unchanged. This is the
            // one place borrowing another agent's chrome is a fact rather than a guess.
            AgentKind::Qwen => GEMINI_AWAITING_PATTERNS,
            _ => &[],
        },
    )
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
/// What one look at a pane's screen established.
///
/// Two answers rather than one because they are not worth the same. `state` is read from chrome
/// anywhere on the screen, and the screen keeps whatever the last turn left on it — so it is
/// trusted to say an agent has *stopped* and never that it is going. `title_working` is read from
/// the one line the agent actively rewrites as it changes state, which makes it the only part of
/// a screen whose *absence* is evidence too. `dispatch` treats them accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRead {
    /// The state the screen's chrome argues for, or `Idle` when no rule matched — which means
    /// "nothing recognised", not "nothing happening".
    pub state: AgentState,
    /// The agent's terminal title carries a spinner, so the agent says it is mid-turn.
    pub title_working: bool,
}

impl From<AgentState> for ScreenRead {
    /// A screen that said only this, with nothing known about its title.
    fn from(state: AgentState) -> Self {
        Self {
            state,
            title_working: false,
        }
    }
}

/// Everything one pass over a screen can establish. See [`classify_screen`] for the ordering.
pub fn read_screen(agent: AgentKind, tail: &str) -> ScreenRead {
    read_screen_titled(agent, tail, None)
}

/// Like [`read_screen`], with the OSC/window title kept apart from the body.
pub fn read_screen_titled(agent: AgentKind, tail: &str, title: Option<&str>) -> ScreenRead {
    ScreenRead {
        state: classify_screen_titled(agent, tail, title),
        title_working: title_says_working(title),
    }
}

pub fn classify_screen(agent: AgentKind, tail: &str) -> AgentState {
    classify_screen_titled(agent, tail, None)
}

/// Classifies a screen when the window title is known independently of the body.
pub fn classify_screen_titled(agent: AgentKind, tail: &str, title: Option<&str>) -> AgentState {
    static DONE: OnceLock<RegexSet> = OnceLock::new();
    // Through the manifest, so a rule someone edited is the rule that runs.
    let rules = crate::manifest::resolve(agent);
    if rules.blocked.is_match(tail) {
        return AgentState::Blocked;
    }
    if title_says_working(title) || rules.working.is_match(tail) {
        return AgentState::Working;
    }
    if rules.awaiting.is_match(tail) {
        return AgentState::Done;
    }
    if set(DONE_PATTERNS, &DONE).is_match(tail) {
        return AgentState::Done;
    }
    AgentState::Idle
}

/// What one pane's classification actually costs, since the daemon's own measurement harness
/// cannot reach it.
///
/// `RuntimeRegistry::resolve_agent` only reads a screen when an agent is running under the pane,
/// and no test in this repository can conjure one into the process table — a measurement driven
/// through the registry drives pane shells, where detection finds nothing and this code is never
/// entered. So the cost is measured here, against a real captured screen, and multiplied by the
/// callers who were paying it: before the memo was split, every pane on the screen paid this twice
/// a second whether or not a byte had arrived, simply because a new process table had landed.
///
///     cargo test --release --lib -- --ignored --nocapture measure_what_classifying
#[cfg(test)]
mod classification_cost {
    use super::*;
    use dock_pty::terminal::VtTerminal;
    use std::time::Instant;

    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_classifying_one_pane_screen_costs() {
        // A pane the size a dashboard actually gives one, filled the way an agent fills it.
        let mut screen = VtTerminal::new(40, 160, 2_000);
        screen.feed(b"\x1b]0;claude - dock\x07");
        for line in 0..38 {
            screen.feed(
                format!("  {line:3}  read src/dispatch.rs, and thought about what it said\r\n")
                    .as_bytes(),
            );
        }
        screen.feed("  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle)\r\n".as_bytes());

        let mut built = Vec::new();
        let mut classified = Vec::new();
        for _ in 0..2_000 {
            let started = Instant::now();
            let text = screen.classifiable_text();
            built.push(started.elapsed());
            let started = Instant::now();
            let read = read_screen(AgentKind::Claude, &text);
            classified.push(started.elapsed());
            assert_eq!(read.state, AgentState::Done);
        }
        let mean = |samples: &[std::time::Duration]| {
            samples.iter().map(|s| s.as_secs_f64()).sum::<f64>() / samples.len() as f64 * 1_000.0
        };
        println!(
            "classifiable_text() {:.4}ms + read_screen() {:.4}ms = {:.4}ms per pane per pass, \
             {} bytes of screen",
            mean(&built),
            mean(&classified),
            mean(&built) + mean(&classified),
            screen.classifiable_text().len(),
        );
    }
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
    fn claudes_footer_is_still_recognised_when_a_narrow_pane_wraps_it() {
        // A pane too narrow for the footer wraps it, and `visible_text` joins rows with a newline,
        // so the phrase arrives split. Anchoring on the exact bracketed string missed every one of
        // these, and each miss was a frame the roster spent saying "working" about a pane that was
        // waiting for its user.
        for wrapped in [
            "  ⏵⏵ auto mode on (shift+tab\n  to cycle)\n",
            "  ⏵⏵ auto mode on (shift+\n  tab to cycle)\n",
            "  ⏵⏵ plan mode on (shift+tab to\n  cycle)\n",
        ] {
            assert_eq!(
                classify_screen(AgentKind::Claude, wrapped),
                AgentState::Done,
                "a wrapped footer is the same footer: {wrapped:?}"
            );
        }
    }

    #[test]
    fn claude_between_turns_is_recognised_by_more_than_one_piece_of_its_chrome() {
        // The hint under the input box, which Claude paints while it is waiting for a person. One
        // rule over one string was a single point of failure: when it missed, nothing else in the
        // file could say the agent had stopped.
        assert_eq!(
            classify_screen(AgentKind::Claude, "  ? for shortcuts\n"),
            AgentState::Done
        );
    }

    #[test]
    fn a_working_claude_is_not_talked_into_finishing_by_the_wider_patterns() {
        // The reason widening is safe: working is tested first, and a mid-turn Claude paints the
        // interrupt hint alongside the very footer the rules above match.
        let working = format!("{CLAUDE_WORKING}  ? for shortcuts\n");
        assert_eq!(
            classify_screen(AgentKind::Claude, &working),
            AgentState::Working
        );
    }

    #[test]
    fn gemini_and_the_fork_that_borrowed_its_input_box_both_report_a_finished_turn() {
        // Qwen Code forked Gemini CLI and kept the input box, so this is the one case where two
        // agents sharing chrome is an observation rather than a guess.
        let idle = concat!(
            "╭──────────────────────────────────────────╮\n",
            "│ > Type your message or @path/to/file     │\n",
            "╰──────────────────────────────────────────╯\n",
            "  ~/Development/dock (main*)   gemini-3-pro (98% context left)\n",
        );
        for agent in [AgentKind::Gemini, AgentKind::Qwen] {
            assert_eq!(classify_screen(agent, idle), AgentState::Done, "{agent:?}");
        }
        // And it stays theirs: an agent whose chrome nobody has captured gets no answer from it.
        assert_eq!(classify_screen(AgentKind::Amp, idle), AgentState::Idle);
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

#[cfg(test)]
mod affordance_tests {
    use super::*;

    /// The bug: "esc to cancel" was a `Blocked` rule, and GitHub Copilot prints it for the entire
    /// duration of a response. A Copilot pane therefore read "needs you" from the moment it began
    /// generating until it stopped — and because the same rule also fired on its chooser, nothing
    /// downstream could tell the two apart. Both halves are asserted here; the second is the one
    /// that makes the first safe to change.
    #[test]
    fn an_interrupt_offer_is_work_and_a_submit_offer_is_a_question() {
        let generating = "Working on it\n  esc to cancel\n";
        assert_eq!(
            classify_screen(AgentKind::Copilot, generating),
            AgentState::Working,
            "an agent offering to be interrupted has something to interrupt"
        );

        let choosing = "Allow this edit?\n  enter to select · esc to cancel\n";
        assert_eq!(
            classify_screen(AgentKind::Copilot, choosing),
            AgentState::Blocked,
            "the submission affordance is what says a question is open"
        );
    }

    /// Every spelling of the interrupt offer that a real agent paints, including the wrapped form
    /// a narrow pane produces, where `visible_text` has joined the rows with a newline.
    #[test]
    fn the_interrupt_offer_is_recognised_however_it_is_spelled() {
        for footer in [
            "esc to interrupt",
            "esc to cancel",
            "esc cancel",
            "esc again to cancel",
            "esc interrupt",
            "(esc\nto interrupt)",
        ] {
            assert_eq!(
                classify_screen(AgentKind::Copilot, footer),
                AgentState::Working,
                "{footer:?} offers an interrupt"
            );
        }
    }

    /// …and every spelling of the submission offer, which outranks it.
    #[test]
    fn the_submission_offer_is_recognised_however_it_is_spelled() {
        for footer in [
            "enter to select",
            "enter to confirm",
            "enter to submit",
            "enter accept",
        ] {
            assert_eq!(
                classify_screen(AgentKind::Copilot, footer),
                AgentState::Blocked,
                "{footer:?} holds a question open"
            );
        }
    }

    /// The signal that needs no per-agent chrome at all. Amp has no body patterns in this file and
    /// never will have every agent's; the spinner it writes into its terminal title is enough on
    /// its own. The title is a dedicated field, not the first body row.
    #[test]
    fn a_spinner_in_the_title_is_work_for_an_agent_with_no_rules_of_its_own() {
        // Amp's real working title, and the body Dock cannot read anything from.
        let body = "╰  gpt-5 thinking ─\n";
        assert_eq!(
            classify_screen_titled(AgentKind::Amp, body, Some("⠹ amp")),
            AgentState::Working,
            "the title answers when the body cannot"
        );

        // Claude Code's 2.1.228 spinner is a quarter circle rather than braille.
        assert_eq!(
            classify_screen_titled(AgentKind::Claude, "some output\n", Some("◐ dock")),
            AgentState::Working
        );
    }

    /// Amp's idle welcome splash leads with braille art on the first body row. That is not a
    /// window title, and classifying it as one made every idle Amp pane read as working.
    #[test]
    fn a_splash_row_of_braille_art_is_idle_not_a_working_title() {
        let splash = "⣿⣿⣿  amp\n\n> \n";
        assert_eq!(
            classify_screen(AgentKind::Amp, splash),
            AgentState::Idle,
            "braille on the first body row is splash chrome, not OSC"
        );
        assert_eq!(
            classify_screen_titled(AgentKind::Amp, splash, None),
            AgentState::Idle
        );
    }

    /// The anchor is the whole safety argument for the title rule. A spinner glyph is only
    /// evidence when it is *the title*, and `·` — Claude's other title glyph — is ordinary
    /// punctuation everywhere else.
    #[test]
    fn a_glyph_that_is_not_the_title_is_not_a_spinner() {
        // Braille further down the screen is somebody's output, not a title.
        assert_eq!(
            classify_screen(AgentKind::Amp, "amp\nsome output ⠹ here\n"),
            AgentState::Idle
        );
        // Claude's idle title glyph must not read as work.
        assert_eq!(
            classify_screen_titled(AgentKind::Amp, "idle\n", Some("✳ amp")),
            AgentState::Idle,
            "`✳` marks the finished title, not a spinner"
        );
        // A spinner leads the title; it does not open a word in it.
        assert_eq!(
            classify_screen_titled(AgentKind::Amp, "idle\n", Some("⠹amp")),
            AgentState::Idle
        );
        // A title that is nothing but the spinner is still the spinner.
        assert_eq!(
            classify_screen_titled(AgentKind::Amp, "", Some("⠹")),
            AgentState::Working
        );
        // The same glyph as the first *body* line, with no title field, is not work.
        assert_eq!(classify_screen(AgentKind::Amp, "⠹"), AgentState::Idle);
    }
}
