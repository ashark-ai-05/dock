//! `dock verdict` — why a run got the verdict it got, and whether today's rules still agree.
//!
//! This is not optional polish: the spec's constraint is that a verdict which cannot be
//! re-derived by hand does not ship. `explain` prints every rule, the fact behind each one that
//! fired, and an explicit line for every rule that did not — the zeroes matter as much as the
//! sums. `recheck` compares the receipt's stored verdict against what today's rules would say,
//! without touching the stored receipt: an old receipt keeps the verdict it was given, and
//! disagreement is reported rather than silently rewritten.

use std::path::PathBuf;

use crate::receipt::{Finding, Receipt, Rule, Severity, Verdict};
use crate::rules::{evaluate, inert};
use crate::storage::LocalStore;

const USAGE: &str = "usage: dock verdict explain <run-id> [--dock-dir=PATH]\n   or: dock verdict recheck <run-id> [--dock-dir=PATH]";
const DEFAULT_DOCK_DIR: &str = ".dock/local";

/// The rule-name column's fixed width. Wide enough that most rule names fit inside it and their
/// status text lines up; a name longer than this (`check_mutated_worktree`, at 22) simply runs
/// past it rather than being truncated, since truncating a rule's own name is the one thing this
/// command must never do. `column` below still guarantees at least one separating space for
/// those overflowing names, so the status text never runs directly into the name.
const RULE_COLUMN_WIDTH: usize = 18;

/// `rule.name()`, left-padded to [`RULE_COLUMN_WIDTH`] — and, for the handful of names that
/// overflow the column, followed by exactly one space instead of none. Padding alone would glue
/// `check_mutated_worktree` straight onto its status word with no gap at all.
fn column(name: &str) -> String {
    let padded = format!("{name:<RULE_COLUMN_WIDTH$}");
    if padded.len() == name.len() {
        format!("{padded} ")
    } else {
        padded
    }
}

#[derive(Debug)]
enum Mode {
    Explain,
    Recheck,
}

#[derive(Debug)]
struct Arguments {
    mode: Mode,
    run_id: String,
    dock_dir: PathBuf,
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    let mode = match args.first().map(String::as_str) {
        Some("explain") => Mode::Explain,
        Some("recheck") => Mode::Recheck,
        Some(other) => return Err(format!("unknown subcommand {other:?}; {USAGE}")),
        None => return Err(format!("missing subcommand; {USAGE}")),
    };
    let mut run_id = None;
    let mut dock_dir = None;
    for argument in &args[1..] {
        if let Some(value) = argument.strip_prefix("--dock-dir=") {
            dock_dir = Some(PathBuf::from(value));
        } else if argument.starts_with("--") {
            return Err(format!("unknown option {argument:?}; {USAGE}"));
        } else if run_id.is_none() {
            run_id = Some(argument.clone());
        } else {
            return Err(format!("unexpected argument {argument:?}; {USAGE}"));
        }
    }
    let run_id = run_id.ok_or_else(|| format!("missing <run-id>; {USAGE}"))?;
    Ok(Arguments {
        mode,
        run_id,
        dock_dir: dock_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_DOCK_DIR)),
    })
}

/// Rule by rule, in `Rule::ALL` order: the fact behind each finding the receipt actually
/// carries, and an explicit line for every rule that did not contribute one — "did not fire" for
/// a rule that ran and found nothing, "inert" for a rule with no data source yet. This explains
/// the receipt's own stored verdict, from its own stored findings; it does not re-run the rules
/// (`recheck_text` does that, on purpose, separately).
pub(crate) fn explain_text(receipt: &Receipt) -> String {
    let mut lines = vec![
        format!(
            "{}  {} {}  (rules v{})",
            receipt.run_id,
            receipt.verdict.glyph(),
            receipt.verdict.label(),
            receipt.rules_version
        ),
        String::new(),
    ];
    for rule in Rule::ALL {
        let firings: Vec<&Finding> = receipt
            .findings
            .iter()
            .filter(|finding| finding.rule == rule)
            .collect();
        let padded = column(rule.name());
        if firings.is_empty() {
            let status = match inert(rule) {
                Some(reason) => format!("inert: {reason}"),
                None => "did not fire".to_owned(),
            };
            lines.push(format!("  {padded}{status}"));
        } else {
            let glyph = match rule.severity() {
                Severity::Failed => Verdict::Failed.glyph(),
                Severity::Look => Verdict::Look.glyph(),
            };
            for finding in firings {
                lines.push(format!("{glyph} {padded}{}", finding.fact));
            }
        }
    }
    lines.join("\n")
}

/// Compares the receipt's stored verdict against what today's rules would say, without
/// mutating the receipt — an old receipt keeps the verdict it was given, and this reports the
/// disagreement rather than papering over it.
pub(crate) fn recheck_text(receipt: &Receipt) -> String {
    let (today_verdict, today_findings) = evaluate(receipt);
    let mut lines = vec![
        format!(
            "stored: {} {} (rules v{})",
            receipt.verdict.glyph(),
            receipt.verdict.label(),
            receipt.rules_version
        ),
        format!(
            "today:  {} {} (rules v{})",
            today_verdict.glyph(),
            today_verdict.label(),
            crate::receipt::RULES_VERSION
        ),
    ];
    // A finding-level multiset diff, not a rule-level one: `destructive_command` and
    // `sensitive_new_file` can each push more than one `Finding` for the same `Rule`, with
    // different facts. Diffing on `finding.rule` alone would call two distinct findings on the
    // same rule "no disagreement" as long as that rule fired at all on both sides — exactly the
    // silent change `recheck` exists to catch. Matching each stored finding against an
    // unconsumed, *exactly equal* today's finding (order-independent, via `Finding`'s own
    // `PartialEq`) means two identical findings on both sides cancel one-for-one, a reordering
    // alone is never reported as a disagreement, and a finding that changed even by fact text
    // alone is reported as dropped, not shrugged off because its rule still fired elsewhere.
    let mut today_remaining: Vec<&Finding> = today_findings.iter().collect();
    let mut dropped: Vec<&Finding> = Vec::new();
    for finding in &receipt.findings {
        match today_remaining
            .iter()
            .position(|candidate| **candidate == *finding)
        {
            Some(index) => {
                today_remaining.remove(index);
            }
            None => dropped.push(finding),
        }
    }
    let added = today_remaining;
    lines.push(String::new());
    if added.is_empty() && dropped.is_empty() {
        lines.push("no disagreement — today's rules find exactly what was stored".to_owned());
    } else {
        for finding in &added {
            lines.push(format!(
                "+ {} now fires: {}",
                finding.rule.name(),
                finding.fact
            ));
        }
        for finding in &dropped {
            lines.push(format!(
                "- {} no longer fires (was: {})",
                finding.rule.name(),
                finding.fact
            ));
        }
    }
    lines.join("\n")
}

pub fn run(args: &[String]) -> Result<(), String> {
    let arguments = parse_arguments(args)?;
    let receipt = LocalStore::new(arguments.dock_dir).load_receipt(&arguments.run_id)?;
    let text = match arguments.mode {
        Mode::Explain => explain_text(&receipt),
        Mode::Recheck => recheck_text(&receipt),
    };
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::receipt::{
        CheckOutcome, CheckRun, Claimed, Finding, Observed, RECEIPT_SCHEMA_VERSION, RULES_VERSION,
        Receipt, Rule, ToolCall, UntrackedFile, Verdict, Witnessed,
    };

    /// One clean receipt to mutate: a real diff, a declared check that passed at head, no
    /// untracked files, no dangerous tool calls — the same shape `dock-receipt`'s own fixture
    /// uses, kept local because that one is crate-private.
    fn base_receipt() -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            run_id: "dock_test".into(),
            task_id: "TASK-1".into(),
            worktree: "/repo".into(),
            branch: "dock/test".into(),
            claimed: Claimed {
                summary: "added a retry".into(),
                question: None,
                checks: vec!["test".into()],
            },
            observed: Observed {
                base_sha: "aaaa111".into(),
                head_sha: "bbbb222".into(),
                changed_files: 2,
                insertions: 40,
                deletions: 3,
                untracked: vec![],
                untracked_error: None,
                tool_calls: vec![ToolCall {
                    at_unix_ms: 1_764_000_000_000,
                    tool: "Bash".into(),
                    detail: "cargo test".into(),
                }],
            },
            witnessed: Witnessed {
                checks: vec![CheckRun {
                    name: "test".into(),
                    command: vec!["cargo".into(), "test".into()],
                    outcome: CheckOutcome::Passed,
                    exit_code: Some(0),
                    duration_ms: 4_200,
                    sha_before: "bbbb222".into(),
                    sha_after: "bbbb222".into(),
                    dirty_before: false,
                    dirty_after: false,
                    tail: "test result: ok. 842 passed".into(),
                    reason: None,
                }],
            },
            decided: None,
            verdict: Verdict::Clear,
            findings: vec![],
            rules_version: RULES_VERSION,
        }
    }

    /// A receipt whose declared check ran and failed — evaluated through `dock_receipt::rules`
    /// so its stored `verdict` and `findings` are exactly what the rules produce, the way a real
    /// receipt's would be at record time.
    fn receipt_with_a_failed_check() -> Receipt {
        let mut receipt = base_receipt();
        receipt.witnessed.checks[0].outcome = CheckOutcome::Failed;
        receipt.witnessed.checks[0].exit_code = Some(1);
        receipt.witnessed.checks[0].tail = "test result: FAILED. thread panicked".into();
        let (verdict, findings) = crate::rules::evaluate(&receipt);
        receipt.verdict = verdict;
        receipt.findings = findings;
        receipt
    }

    /// Rule by rule, with the fact each one read, and the rules that did not fire listed as not
    /// firing — a reader has to be able to check the arithmetic, including the zeroes.
    #[test]
    fn explain_prints_every_rule_and_the_fact_behind_each_finding() {
        let text = super::explain_text(&receipt_with_a_failed_check());
        assert!(text.contains("✗ failed"), "{text}");
        assert!(text.contains("check_failed"), "{text}");
        assert!(text.contains("exit 1"), "{text}");
        assert!(text.contains("empty_diff        did not fire"), "{text}");
        assert!(text.contains("peer_conflict     inert"), "{text}");
        assert!(text.contains(&format!("rules v{RULES_VERSION}")), "{text}");
    }

    /// An old receipt keeps the verdict it was given; recheck reports the disagreement rather
    /// than rewriting history.
    #[test]
    fn recheck_reports_where_todays_rules_disagree_without_touching_the_receipt() {
        let stored = Receipt {
            rules_version: 0,
            verdict: Verdict::Clear,
            ..receipt_with_a_failed_check()
        };
        let text = super::recheck_text(&stored);
        assert!(text.contains("stored: ✓ clear (rules v0)"), "{text}");
        assert!(
            text.contains(&format!("today:  ✗ failed (rules v{RULES_VERSION})")),
            "{text}"
        );
    }

    /// A receipt where today's rules agree with what was stored says so plainly, rather than
    /// leaving a reader to infer agreement from an empty diff.
    #[test]
    fn recheck_says_so_when_todays_rules_agree_with_what_was_stored() {
        let text = super::recheck_text(&base_receipt());
        assert!(
            text.contains("no disagreement"),
            "an all-green receipt should report agreement: {text}"
        );
    }

    /// `sensitive_new_file` (and `destructive_command`) can push more than one `Finding` for the
    /// same `Rule`. A diff keyed on `finding.rule` alone would see `sensitive_new_file` present
    /// on both sides and call that "no disagreement" — even though the *specific* finding this
    /// receipt was given no longer fires. The stored receipt below carries two
    /// `sensitive_new_file` findings; today's rules, run against this receipt's own
    /// `observed.untracked` (which lists only one sensitive file), produce only one of them.
    /// `recheck` must name the one that vanished, by its own fact, not shrug it off because its
    /// rule fired elsewhere.
    #[test]
    fn recheck_reports_a_dropped_finding_even_when_its_rule_still_fires_elsewhere() {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "config/secret.pem".into(),
            bytes: Some(512),
        }];
        let (verdict, mut findings) = crate::rules::evaluate(&receipt);
        assert_eq!(
            findings,
            vec![Finding {
                rule: Rule::SensitiveNewFile,
                fact: "untracked file `config/secret.pem` matches `*.pem`".into(),
            }],
            "the fixture must fire exactly the one finding this test means to keep"
        );
        // A second `sensitive_new_file` finding the stored receipt carries but that today's
        // rules, run against this same `observed.untracked`, do not reproduce.
        findings.push(Finding {
            rule: Rule::SensitiveNewFile,
            fact: "untracked file `notes.txt` is 2000000 bytes, over the 1048576 byte limit".into(),
        });
        receipt.verdict = verdict;
        receipt.findings = findings;

        let text = super::recheck_text(&receipt);
        assert!(
            text.contains("notes.txt"),
            "the dropped finding must be named by its own fact, not just its rule: {text}"
        );
        assert!(
            !text.contains("no disagreement"),
            "a genuinely dropped finding is a disagreement: {text}"
        );
    }

    /// `explain` and `recheck` share the same subcommand dispatch: both need a subcommand, a
    /// run id, and accept an optional `--dock-dir=`.
    #[test]
    fn a_subcommand_and_a_run_id_are_required() {
        let arguments = super::parse_arguments(&[
            "explain".to_owned(),
            "dock_7".to_owned(),
            "--dock-dir=/tmp/state".to_owned(),
        ])
        .expect("explain with a run id and a dock-dir parses");
        assert!(matches!(arguments.mode, super::Mode::Explain));
        assert_eq!(arguments.run_id, "dock_7");
        assert_eq!(arguments.dock_dir, std::path::PathBuf::from("/tmp/state"));

        let arguments = super::parse_arguments(&["recheck".to_owned(), "dock_7".to_owned()])
            .expect("recheck defaults --dock-dir");
        assert!(matches!(arguments.mode, super::Mode::Recheck));
        assert_eq!(arguments.dock_dir, std::path::PathBuf::from(".dock/local"));

        let error = super::parse_arguments(&["explain".to_owned()]).unwrap_err();
        assert!(error.contains("run-id"), "{error}");

        let error = super::parse_arguments(&["bogus".to_owned()]).unwrap_err();
        assert!(error.contains("bogus"), "{error}");
    }
}
