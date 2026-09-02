//! The verdict is arithmetic over evidence, never judgement of it.
//!
//! Nine rules, each reading only the receipt's own columns, each producing a [`Finding`] that
//! names the field it read. A verdict is the maximum [`Severity`] among the findings that fired
//! — nothing more clever than that — because a reader who does not trust Dock's conclusion must
//! be able to re-derive it by hand from the receipt's own lines. `dock verdict explain` (a later
//! task) prints the rules that did not fire alongside the ones that did, for the same reason.

use std::path::Path;

use dock_model::receipt::{CheckOutcome, CheckRun, Finding, Receipt, Rule, Severity, Verdict};

/// A file larger than this is flagged by `sensitive_new_file` regardless of its name. Measured
/// in the binary sense (2^20 bytes) because that is what `ls -l` and `du` report.
const SENSITIVE_FILE_SIZE_LIMIT_BYTES: u64 = 1_048_576;

/// Substrings of a tool call's `detail` that mark it destructive. `detail` is truncated at
/// record time, so these are matched with `str::contains` rather than an anchored parse — a
/// prefix match still finds them even if the tail of a long command line was cut off.
const DESTRUCTIVE_COMMAND_SUBSTRINGS: [&str; 4] =
    ["rm -rf", "git reset --hard", "git push", "git clean"];

/// Why a rule never fires today, for a rule whose data source does not exist yet. `evaluate`
/// consults this implicitly by simply never calling such a rule's check; this function exists so
/// `dock verdict explain` can say *why* a rule is silent instead of leaving a reader to wonder
/// whether it was checked and found clean.
pub fn inert(rule: Rule) -> Option<&'static str> {
    match rule {
        Rule::PeerConflict => Some("no ledger yet — row 5 supplies it"),
        _ => None,
    }
}

/// Runs every rule against `receipt` and rolls the findings up into a verdict. Pure: the same
/// receipt always produces the same answer, which is what lets `dock verdict recheck` compare an
/// old receipt's stored verdict against what today's rules would say.
pub fn evaluate(receipt: &Receipt) -> (Verdict, Vec<Finding>) {
    let mut findings = Vec::new();
    check_failed(receipt, &mut findings);
    check_stale(receipt, &mut findings);
    check_unwitnessed(receipt, &mut findings);
    check_mutated_worktree(receipt, &mut findings);
    no_checks_declared(receipt, &mut findings);
    empty_diff(receipt, &mut findings);
    // peer_conflict is inert — see `inert` above.
    destructive_command(receipt, &mut findings);
    sensitive_new_file(receipt, &mut findings);

    let verdict = if findings
        .iter()
        .any(|finding| matches!(finding.rule.severity(), Severity::Failed))
    {
        Verdict::Failed
    } else if findings.is_empty() {
        Verdict::Clear
    } else {
        Verdict::Look
    };
    (verdict, findings)
}

/// A declared check that Dock ran and that exited non-zero. The only rule that is `Failed`
/// rather than `Look`.
fn check_failed(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for check in &receipt.witnessed.checks {
        if check.outcome == CheckOutcome::Failed {
            let exit = fmt_exit_code(check);
            findings.push(Finding {
                rule: Rule::CheckFailed,
                fact: format!("check `{}` failed with exit {exit}", check.name),
            });
        }
    }
}

/// A check that ran, but not at the commit the receipt is reporting on — the green tick it
/// earned belongs to an earlier state of the tree.
///
/// A check with no after-pin at all is skipped rather than flagged. An empty pin is the absence
/// of an answer, not the answer "somewhere else", and it would otherwise fire on every check that
/// never reached a spawn. The test is the pin rather than the outcome on purpose: a check that
/// timed out is `Unwitnessed` *and* carries a real pin, and its tree may genuinely have moved.
fn check_stale(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for check in &receipt.witnessed.checks {
        if !check.sha_after.is_empty() && check.sha_after != receipt.observed.head_sha {
            findings.push(Finding {
                rule: Rule::CheckStale,
                fact: format!(
                    "check `{}` ran at `{}`, but head is now `{}`",
                    check.name, check.sha_after, receipt.observed.head_sha
                ),
            });
        }
    }
}

/// A check Dock could not run at all — an unknown name, a timeout, a spawn error, an
/// unpermitted environment variable. The fact is the reason `Witnessed` recorded for it.
fn check_unwitnessed(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for check in &receipt.witnessed.checks {
        if check.outcome == CheckOutcome::Unwitnessed {
            let reason = check.reason.as_deref().unwrap_or("no reason recorded");
            findings.push(Finding {
                rule: Rule::CheckUnwitnessed,
                fact: format!("check `{}` unwitnessed: {reason}", check.name),
            });
        }
    }
}

/// A check that left the worktree in a different state than it found it — dirty when it wasn't,
/// or on a different commit. A check is supposed to observe, not mutate.
///
/// Skipped for a check with no after-pin, for the same reason [`check_stale`] skips one: without
/// a pin on the far side there is nothing to compare, and reading the absence as a difference
/// would accuse a check that never ran of having moved the tree.
fn check_mutated_worktree(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for check in &receipt.witnessed.checks {
        if check.sha_after.is_empty() {
            continue;
        }
        if check.dirty_before != check.dirty_after || check.sha_before != check.sha_after {
            let mut changes = Vec::new();
            if check.dirty_before != check.dirty_after {
                changes.push(format!(
                    "dirty went from {} to {}",
                    check.dirty_before, check.dirty_after
                ));
            }
            if check.sha_before != check.sha_after {
                changes.push(format!(
                    "sha moved from `{}` to `{}`",
                    check.sha_before, check.sha_after
                ));
            }
            findings.push(Finding {
                rule: Rule::CheckMutatedWorktree,
                fact: format!(
                    "check `{}` mutated the worktree: {}",
                    check.name,
                    changes.join(", ")
                ),
            });
        }
    }
}

/// A run that asked for no checks at all. The load-bearing rule of the whole design: without it
/// a receipt with nothing to fail would read as clean, when in truth nothing was witnessed.
fn no_checks_declared(receipt: &Receipt, findings: &mut Vec<Finding>) {
    if receipt.claimed.checks.is_empty() {
        findings.push(Finding {
            rule: Rule::NoChecksDeclared,
            fact: "claimed.checks is empty — no checks were declared".into(),
        });
    }
}

/// A claim of work with nothing behind it: the tree is at the same commit it started from and
/// no untracked file appeared either.
fn empty_diff(receipt: &Receipt, findings: &mut Vec<Finding>) {
    let claimed_something = !receipt.claimed.summary.trim().is_empty();
    let no_diff = receipt.observed.base_sha == receipt.observed.head_sha;
    let no_new_files = receipt.observed.untracked.is_empty();
    if claimed_something && no_diff && no_new_files {
        findings.push(Finding {
            rule: Rule::EmptyDiff,
            fact: format!(
                "claimed `{}` but base `{}` equals head `{}` with no untracked files",
                receipt.claimed.summary.trim(),
                receipt.observed.base_sha,
                receipt.observed.head_sha
            ),
        });
    }
}

/// A tool call whose command line matches a pattern Dock treats as destructive by name alone,
/// regardless of what it actually did to the tree.
fn destructive_command(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for call in &receipt.observed.tool_calls {
        if let Some(pattern) = DESTRUCTIVE_COMMAND_SUBSTRINGS
            .iter()
            .find(|pattern| call.detail.contains(*pattern))
        {
            findings.push(Finding {
                rule: Rule::DestructiveCommand,
                fact: format!(
                    "tool call `{}` contains `{pattern}`: `{}`",
                    call.tool, call.detail
                ),
            });
        }
    }
}

/// An untracked file whose name looks like a secret, or whose size crosses the limit a secret is
/// unlikely to. A size Dock could not read is never treated as small — see `UntrackedFile::bytes`
/// — so this only fires on size when a size is actually known.
fn sensitive_new_file(receipt: &Receipt, findings: &mut Vec<Finding>) {
    for file in &receipt.observed.untracked {
        let name = file_name(&file.path);
        if let Some(pattern) = sensitive_name_pattern(name) {
            findings.push(Finding {
                rule: Rule::SensitiveNewFile,
                fact: format!("untracked file `{}` matches `{pattern}`", file.path),
            });
        } else if let Some(bytes) = file.bytes
            && bytes > SENSITIVE_FILE_SIZE_LIMIT_BYTES
        {
            findings.push(Finding {
                rule: Rule::SensitiveNewFile,
                fact: format!(
                    "untracked file `{}` is {bytes} bytes, over the {SENSITIVE_FILE_SIZE_LIMIT_BYTES} byte limit",
                    file.path
                ),
            });
        }
    }
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// The name-matching half of `sensitive_new_file`, spec §2's `.env*`, `*.pem`, `id_*`.
fn sensitive_name_pattern(name: &str) -> Option<&'static str> {
    if name.starts_with(".env") {
        Some(".env*")
    } else if name.ends_with(".pem") {
        Some("*.pem")
    } else if name.starts_with("id_") {
        Some("id_*")
    } else {
        None
    }
}

fn fmt_exit_code(check: &CheckRun) -> String {
    match check.exit_code {
        Some(code) => code.to_string(),
        None => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dock_model::receipt::{
        Claimed, Observed, RECEIPT_SCHEMA_VERSION, RULES_VERSION, ToolCall, UntrackedFile,
        Witnessed,
    };

    /// One clean receipt every fixture below starts from: a real diff, a declared check that
    /// passed at head, no untracked files, no dangerous tool calls. Every fixture changes
    /// exactly the field its name promises, so a reader can see what makes each case fire.
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

    fn receipt_all_green() -> Receipt {
        base_receipt()
    }

    fn receipt_with_a_failed_check() -> Receipt {
        let mut receipt = base_receipt();
        receipt.witnessed.checks[0].outcome = CheckOutcome::Failed;
        receipt.witnessed.checks[0].exit_code = Some(1);
        receipt.witnessed.checks[0].tail = "test result: FAILED. thread panicked".into();
        receipt
    }

    fn receipt_with_a_check_green_at_an_older_sha() -> Receipt {
        let mut receipt = base_receipt();
        // Ran and passed, but at the base commit — head has since moved on.
        receipt.witnessed.checks[0].sha_before = "aaaa111".into();
        receipt.witnessed.checks[0].sha_after = "aaaa111".into();
        receipt
    }

    fn receipt_with_an_undeclared_check_name() -> Receipt {
        let mut receipt = base_receipt();
        receipt.witnessed.checks[0] = CheckRun {
            name: "lint".into(),
            command: vec![],
            outcome: CheckOutcome::Unwitnessed,
            exit_code: None,
            duration_ms: 0,
            sha_before: "bbbb222".into(),
            sha_after: "bbbb222".into(),
            dirty_before: false,
            dirty_after: false,
            tail: String::new(),
            reason: Some("unknown check name `lint`".into()),
        };
        receipt
    }

    /// What a check Dock declined to run leaves behind: no command, no pins, and a sentence
    /// saying why. `sha_after` empty is the shape both `check_stale` and `check_mutated_worktree`
    /// have to skip.
    fn receipt_with_a_check_that_never_pinned_a_sha() -> Receipt {
        let mut receipt = base_receipt();
        receipt.witnessed.checks[0] = CheckRun {
            name: "lint".into(),
            command: vec![],
            outcome: CheckOutcome::Unwitnessed,
            exit_code: None,
            duration_ms: 0,
            sha_before: String::new(),
            sha_after: String::new(),
            dirty_before: false,
            dirty_after: false,
            tail: String::new(),
            reason: Some("no check named `lint` in .dock/checks.toml".into()),
        };
        receipt
    }

    fn receipt_with_a_check_that_dirtied_the_tree() -> Receipt {
        let mut receipt = base_receipt();
        receipt.witnessed.checks[0].dirty_before = false;
        receipt.witnessed.checks[0].dirty_after = true;
        receipt
    }

    fn receipt_with_no_checks() -> Receipt {
        let mut receipt = base_receipt();
        receipt.claimed.checks = vec![];
        receipt.witnessed.checks = vec![];
        receipt
    }

    fn receipt_with_a_claim_and_no_diff() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.base_sha = "aaaa111".into();
        receipt.observed.head_sha = "aaaa111".into();
        receipt.witnessed.checks[0].sha_before = "aaaa111".into();
        receipt.witnessed.checks[0].sha_after = "aaaa111".into();
        receipt
    }

    fn receipt_with_a_git_reset_hard_in_its_tool_calls() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.tool_calls = vec![ToolCall {
            at_unix_ms: 1_764_000_000_000,
            tool: "Bash".into(),
            detail: "git reset --hard HEAD~1".into(),
        }];
        receipt
    }

    fn receipt_with_an_untracked_pem() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "config/secret.pem".into(),
            bytes: Some(512),
        }];
        receipt
    }

    fn receipt_with_a_failed_check_and_a_sensitive_file() -> Receipt {
        let mut receipt = receipt_with_a_failed_check();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "config/secret.pem".into(),
            bytes: Some(512),
        }];
        receipt
    }

    /// A plainly-named file over the size limit. `sensitive_new_file`'s size clause is the
    /// entire subject of the controller ruling that put a byte count on `UntrackedFile` in the
    /// first place — this fixture is the one proof that the clause actually fires, rather than
    /// only ever being reached by a name that would have fired anyway.
    fn receipt_with_an_oversized_untracked_file() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "fixtures/recording.bin".into(),
            bytes: Some(1_048_577),
        }];
        receipt
    }

    /// The same plain name, at exactly the limit rather than past it. The condition is `>`, not
    /// `>=`, so this must not fire — and 1,048,576 already exceeds a limit quietly narrowed to
    /// 1,000,000, so this fixture also pins the constant itself, not just the comparison.
    fn receipt_with_an_untracked_file_at_exactly_the_size_limit() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "fixtures/recording.bin".into(),
            bytes: Some(1_048_576),
        }];
        receipt
    }

    /// A plain name whose size Dock could not read. `None` must not be coerced into "small" or
    /// "large" by the size clause — it must simply not participate in it.
    fn receipt_with_an_untracked_file_of_unknown_size() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "fixtures/recording.bin".into(),
            bytes: None,
        }];
        receipt
    }

    /// A sensitive name whose size Dock could not read. The name clause is independent of the
    /// size clause, so an unread size must not suppress it.
    fn receipt_with_an_untracked_pem_of_unknown_size() -> Receipt {
        let mut receipt = base_receipt();
        receipt.observed.untracked = vec![UntrackedFile {
            path: "config/secret.pem".into(),
            bytes: None,
        }];
        receipt
    }

    /// Every rule fires on the case it names, and no fixture accidentally fires a second rule
    /// beside the one it was built to demonstrate.
    #[test]
    fn each_rule_fires_on_the_fact_it_names_and_nothing_else() {
        for (rule, receipt) in [
            (Rule::CheckFailed, receipt_with_a_failed_check()),
            (
                Rule::CheckStale,
                receipt_with_a_check_green_at_an_older_sha(),
            ),
            (
                Rule::CheckUnwitnessed,
                receipt_with_an_undeclared_check_name(),
            ),
            (
                Rule::CheckMutatedWorktree,
                receipt_with_a_check_that_dirtied_the_tree(),
            ),
            (Rule::NoChecksDeclared, receipt_with_no_checks()),
            (Rule::EmptyDiff, receipt_with_a_claim_and_no_diff()),
            (
                Rule::DestructiveCommand,
                receipt_with_a_git_reset_hard_in_its_tool_calls(),
            ),
            (Rule::SensitiveNewFile, receipt_with_an_untracked_pem()),
        ] {
            let (_, findings) = evaluate(&receipt);
            let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
            assert_eq!(
                fired,
                vec![rule],
                "{} fired alongside or instead of the rule it names: {fired:?}",
                rule.name()
            );
        }
    }

    /// `sensitive_new_file`'s size clause fires on its own, on a name that matches none of
    /// `.env*`, `*.pem`, `id_*` — proving the clause is implemented, not just present in a name
    /// match's shadow.
    #[test]
    fn an_oversized_untracked_file_fires_on_size_alone() {
        let (_, findings) = evaluate(&receipt_with_an_oversized_untracked_file());
        let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
        assert_eq!(fired, vec![Rule::SensitiveNewFile], "{fired:?}");
    }

    /// The limit is exclusive: a file of exactly 1,048,576 bytes is not "larger than" it, and
    /// 1,048,576 already exceeds a limit quietly narrowed to 1,000,000, so a wrong `>=` or a
    /// wrong constant both show up here.
    #[test]
    fn an_untracked_file_at_exactly_the_size_limit_does_not_fire() {
        let (_, findings) = evaluate(&receipt_with_an_untracked_file_at_exactly_the_size_limit());
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// An unread size must not be coerced into "large": a plain name with no known size does
    /// not fire.
    #[test]
    fn an_untracked_file_of_unknown_size_and_a_plain_name_does_not_fire() {
        let (_, findings) = evaluate(&receipt_with_an_untracked_file_of_unknown_size());
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// An unread size must not suppress the name clause either: a sensitive name still fires
    /// even when its size is unknown.
    #[test]
    fn an_untracked_file_of_unknown_size_still_fires_on_a_matching_name() {
        let (_, findings) = evaluate(&receipt_with_an_untracked_pem_of_unknown_size());
        let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
        assert_eq!(fired, vec![Rule::SensitiveNewFile], "{fired:?}");
    }

    /// A clean run fires nothing at all, which is the only way to earn a tick.
    #[test]
    fn a_run_with_witnessed_green_checks_at_head_is_clear() {
        let (verdict, findings) = evaluate(&receipt_all_green());
        assert_eq!(verdict, Verdict::Clear);
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The load-bearing rule. A repository that declared nothing cannot be handed a tick for
    /// work Dock did not witness, however clean everything else looks.
    #[test]
    fn a_run_that_declared_no_checks_can_never_be_clear() {
        let (verdict, findings) = evaluate(&receipt_with_no_checks());
        assert_eq!(verdict, Verdict::Look);
        assert!(findings.iter().any(|f| f.rule == Rule::NoChecksDeclared));
    }

    /// The verdict is the maximum severity present, so one failure outranks any number of looks.
    #[test]
    fn one_failed_check_outranks_every_look_beside_it() {
        let (verdict, findings) = evaluate(&receipt_with_a_failed_check_and_a_sensitive_file());
        assert_eq!(verdict, Verdict::Failed);
        assert!(
            findings.len() >= 2,
            "the looks are still reported: {findings:?}"
        );
    }

    /// Every finding names the fact that produced it, because a verdict that cannot be
    /// re-derived by hand does not ship.
    #[test]
    fn every_finding_carries_the_fact_it_read() {
        let (_, findings) = evaluate(&receipt_with_a_failed_check());
        let failed = findings
            .iter()
            .find(|f| f.rule == Rule::CheckFailed)
            .unwrap();
        assert!(failed.fact.contains("test"), "{:?}", failed.fact);
        assert!(failed.fact.contains("exit 1"), "{:?}", failed.fact);
    }

    /// A check that never reached a spawn has no after-pin, and an absent pin is the absence of
    /// an answer. `check_stale` asks whether a check ran at the head the receipt reports; a check
    /// that ran nowhere did not run somewhere else, and saying so would print a sentence — "ran
    /// at ``" — that no reader could check against the receipt.
    #[test]
    fn a_check_with_no_after_pin_is_never_called_stale() {
        let receipt = receipt_with_a_check_that_never_pinned_a_sha();
        let (_, findings) = evaluate(&receipt);
        let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
        assert_eq!(fired, vec![Rule::CheckUnwitnessed], "{findings:?}");
    }

    /// The same absent pin, and the same reason: with nothing on the far side to compare against,
    /// reading the gap as a difference would accuse a check that never ran of moving the tree.
    #[test]
    fn a_check_with_no_after_pin_is_never_called_a_mutation() {
        // A before-pin and no after-pin is exactly what a spawn failure leaves behind: the
        // worktree was read, and then nothing happened.
        let mut receipt = receipt_with_a_check_that_never_pinned_a_sha();
        receipt.witnessed.checks[0].sha_before = "bbbb222".into();
        receipt.witnessed.checks[0].dirty_before = true;
        let (_, findings) = evaluate(&receipt);
        let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
        assert_eq!(fired, vec![Rule::CheckUnwitnessed], "{findings:?}");
    }

    /// A timed-out check is `Unwitnessed` and *does* carry a real after-pin, so neither rule may
    /// key on the outcome: one that really did move the tree still has to be caught.
    #[test]
    fn a_timed_out_check_that_moved_the_tree_is_still_caught() {
        let mut receipt = receipt_with_a_check_that_never_pinned_a_sha();
        receipt.witnessed.checks[0].sha_before = "bbbb222".into();
        receipt.witnessed.checks[0].sha_after = "cccc333".into();
        receipt.witnessed.checks[0].reason = Some("timed out after 600s".into());
        let (verdict, findings) = evaluate(&receipt);
        let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
        assert_eq!(
            fired,
            vec![
                Rule::CheckStale,
                Rule::CheckUnwitnessed,
                Rule::CheckMutatedWorktree
            ],
            "{findings:?}"
        );
        assert_eq!(verdict, Verdict::Look);
    }

    /// A rule with no data source yet says so, rather than silently never firing.
    #[test]
    fn peer_conflict_is_declared_inert_until_the_ledger_exists() {
        assert_eq!(
            inert(Rule::PeerConflict),
            Some("no ledger yet — row 5 supplies it")
        );
        assert_eq!(inert(Rule::CheckFailed), None);
    }
}
