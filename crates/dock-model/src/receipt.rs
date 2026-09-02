//! What a run left behind, and what Dock concluded about it.
//!
//! Four authored columns and one derived one. **Nothing may be written across columns.** The
//! agent writes `claimed` and can never write `witnessed`; Dock writes `witnessed` and can never
//! write `decided`. That is the whole trust model, and these are separate types so that it is a
//! property of the shape rather than of everyone's good intentions.

use serde::{Deserialize, Serialize};

/// The receipt format. Bumped when a field's meaning changes, never for an addition that
/// defaults.
pub const RECEIPT_SCHEMA_VERSION: u16 = 1;

/// The rule set that produced a verdict. Stored in the receipt so an old receipt can show the
/// verdict it was given while `dock verdict recheck` reports where today's rules disagree.
pub const RULES_VERSION: u16 = 1;

/// What the agent said. Dock never writes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claimed {
    pub summary: String,
    pub question: Option<String>,
    /// The *names* of checks the agent asks Dock to run. Never a command: a name is looked up in
    /// `.dock/checks.toml`, and one map lookup is the whole containment argument.
    pub checks: Vec<String>,
}

/// What git and the hook payloads saw. Neither the agent nor the reviewer writes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observed {
    pub base_sha: String,
    pub head_sha: String,
    pub changed_files: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Paths and sizes, not contents. `sensitive_new_file` reads these and nothing else.
    pub untracked: Vec<UntrackedFile>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// One untracked path, with the size Dock could read for it at record time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UntrackedFile {
    pub path: String,
    /// `None` when the size could not be read — a race with the agent, a permissions error, a
    /// symlink to nowhere. Never `0`: zero reads as "small" and would silently exempt the file
    /// from `sensitive_new_file`'s size clause for exactly the file whose size Dock could not
    /// determine.
    pub bytes: Option<u64>,
}

/// One tool call a hook reported, with the time it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub at_unix_ms: u64,
    pub tool: String,
    /// The identifying argument — a path for an edit, the command line for a shell call. Capped
    /// at `TOOL_DETAIL_LIMIT` bytes by whoever records it.
    pub detail: String,
}

/// What Dock ran and watched. The agent can never write here; that is the point of the product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Witnessed {
    pub checks: Vec<CheckRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    pub name: String,
    /// The argv Dock actually spawned, copied from the declaration rather than from the request.
    pub command: Vec<String>,
    pub outcome: CheckOutcome,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub sha_before: String,
    pub sha_after: String,
    pub dirty_before: bool,
    pub dirty_after: bool,
    /// The last `TAIL_LINES` lines / `TAIL_BYTES` bytes of combined output, whichever binds first.
    pub tail: String,
    /// Why a check is `Unwitnessed`. `None` for a check that ran.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    /// Could not run: unknown name, timeout, spawn error, unpermitted environment.
    Unwitnessed,
}

/// What the human decided. Dock never writes here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decided {
    pub route: crate::model::ReviewRoute,
    pub at_unix_ms: u64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u16,
    pub run_id: String,
    pub task_id: String,
    pub worktree: String,
    pub branch: String,
    pub claimed: Claimed,
    pub observed: Observed,
    pub witnessed: Witnessed,
    pub decided: Option<Decided>,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub rules_version: u16,
}

/// One rule that fired, and the fact that made it fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub rule: Rule,
    /// The fact, in words a reader can check against the receipt by hand. A finding that cannot
    /// be re-derived from the lines above it does not ship.
    pub fact: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    CheckFailed,
    CheckStale,
    CheckUnwitnessed,
    CheckMutatedWorktree,
    NoChecksDeclared,
    EmptyDiff,
    PeerConflict,
    DestructiveCommand,
    SensitiveNewFile,
}

/// How bad a rule's finding is. The verdict is the maximum severity present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Look,
    Failed,
}

impl Rule {
    /// The name printed by `dock verdict explain`, and the name in the spec's table.
    pub const fn name(self) -> &'static str {
        match self {
            Self::CheckFailed => "check_failed",
            Self::CheckStale => "check_stale",
            Self::CheckUnwitnessed => "check_unwitnessed",
            Self::CheckMutatedWorktree => "check_mutated_worktree",
            Self::NoChecksDeclared => "no_checks_declared",
            Self::EmptyDiff => "empty_diff",
            Self::PeerConflict => "peer_conflict",
            Self::DestructiveCommand => "destructive_command",
            Self::SensitiveNewFile => "sensitive_new_file",
        }
    }

    /// Only a declared check that ran and failed is fatal. Everything else asks for a look.
    pub const fn severity(self) -> Severity {
        match self {
            Self::CheckFailed => Severity::Failed,
            _ => Severity::Look,
        }
    }

    /// Every rule, so `dock verdict explain` can list the ones that did *not* fire and the rule
    /// table test cannot silently miss one.
    pub const ALL: [Self; 9] = [
        Self::CheckFailed,
        Self::CheckStale,
        Self::CheckUnwitnessed,
        Self::CheckMutatedWorktree,
        Self::NoChecksDeclared,
        Self::EmptyDiff,
        Self::PeerConflict,
        Self::DestructiveCommand,
        Self::SensitiveNewFile,
    ];
}

/// Dock's conclusion about one receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// A receipt fixture shared by this module's tests and `storage`'s: one receipt, fully
/// populated, so a round-trip or storage test exercises every field rather than the ones its
/// author remembered to set.
#[cfg(test)]
pub(crate) fn fixture() -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        run_id: "dock_01J9".into(),
        task_id: "DOCK-7".into(),
        worktree: "/repo/fixture".into(),
        branch: "dock/fixture".into(),
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
            untracked: vec![UntrackedFile {
                path: ".env.local".into(),
                bytes: Some(42),
            }],
            tool_calls: vec![ToolCall {
                at_unix_ms: 1_764_000_000_000,
                tool: "Bash".into(),
                detail: "cargo test".into(),
            }],
        },
        witnessed: Witnessed {
            checks: vec![CheckRun {
                name: "test".into(),
                command: vec!["cargo".into(), "test".into(), "--locked".into()],
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
        verdict: Verdict::Look,
        findings: vec![Finding {
            rule: Rule::SensitiveNewFile,
            fact: "untracked file `.env.local` matches `.env*`".into(),
        }],
        rules_version: RULES_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::fixture as receipt_fixture;
    use super::*;
    use dock_detect::AgentState;
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

    /// A receipt is durable evidence, so it round-trips exactly and refuses anything it was not
    /// designed to hold — a transcript smuggled into a field nobody declared included.
    #[test]
    fn a_receipt_round_trips_and_refuses_fields_it_never_declared() {
        let receipt = receipt_fixture();
        let encoded = serde_json::to_string(&receipt).expect("serialize receipt");
        let decoded: Receipt = serde_json::from_str(&encoded).expect("deserialize receipt");
        assert_eq!(decoded, receipt);
        assert!(
            serde_json::from_str::<Receipt>(
                &encoded.replace(r#""run_id""#, r#""raw_transcript":"no","run_id""#)
            )
            .is_err()
        );
    }

    /// The four authored columns are separate types on purpose: nothing may be written across
    /// them, and a shape that let one column reach into another would make that unenforceable.
    #[test]
    fn the_verdict_and_its_rules_version_travel_with_the_receipt() {
        let receipt = receipt_fixture();
        assert_eq!(receipt.rules_version, RULES_VERSION);
        // A receipt written by an older rule set keeps the verdict it was given.
        let older = Receipt {
            rules_version: RULES_VERSION - 1,
            ..receipt_fixture()
        };
        assert_ne!(older.rules_version, receipt.rules_version);
        assert_eq!(older.verdict, receipt.verdict);
    }
}
