# `dock-receipt` Implementation Plan — row 2 of the delivery order

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Dock produce the thing it exists for — a receipt per run whose *witnessed* column contains a command Dock ran, an exit code Dock observed, and the SHA it ran at, none of which the agent could have written; and a verdict that is arithmetic over those facts.

**Architecture:** One new crate, `dock-receipt`, holding the check declaration reader, the process runner, and the rule set. The durable shapes — `Receipt`, `Finding`, `Verdict` — live in `dock-model` beside `HandoffPacket`, because the daemon, the CLI and the UI all read them and none of those may depend on the crate that spawns processes. A check is named by the agent and *resolved* by Dock through one map lookup into `.dock/checks.toml`; that lookup is the entire containment argument. The runner is reached from `dispatch::submit_handoff`, which already collects the observed column's git facts and already refuses a handoff whose binding drifted.

**Tech Stack:** Rust 2024, Cargo workspace. Two new dependencies, both in `dock-receipt` only: `toml` (declaration parsing) and `nix` (already pinned workspace-wide at `=0.29.0`, for `killpg` on timeout).

**Spec:** `docs/superpowers/specs/2026-09-02-dock-receipts-design.md` — sections 1 (the receipt), 2 (the verdict), 3 (the check runner), 7 (architecture), 11 (testing), 12 (success criteria). Row 2 of section 9.

---

## Global Constraints

- **An agent can never write to *witnessed*; Dock can never write to *decided*.** That is the trust model, and Task 8 puts it in the README in those words. Any design question in this plan is answered by asking which column the data belongs to.
- **Dock may execute a command declared by the repository or the user — never one an agent composed.** An agent references a *name*. An unknown name is recorded `unwitnessed` with the reason, and no process is spawned. There is one test whose whole job is to prove the no-spawn half.
- **`run` is an argv array, never a shell string.** No `sh -c`, no interpolation, no glob, no chaining. A pipeline is a committed script declared as `run = ["./scripts/ci.sh"]`.
- **`dock-receipt` is the only crate permitted to execute argv Dock did not write.** It therefore carries `#![allow(clippy::disallowed_methods)]` with a comment saying exactly that, in the shape `dock-git/src/lib.rs:6` and `dock-pty/src/lib.rs:2` already use. `dock-model`, `dock-ui` and `dock-testing` keep `#![deny(...)]` and must stay unable to spawn.
- **`clear` is earned.** A run that declared no checks fires `no_checks_declared` and can never be `clear`. This is load-bearing: it is what stops the verdict drifting toward "usually green".
- **The verdict never decides, never scores, never changes silently.** It ranks and explains. The receipt stores the verdict, the findings *and* `rules_version`.
- **Never auto-answer a permission prompt; never summarise or judge with a model.** No LLM anywhere in this work. The verdict must be re-derivable by hand from the printed lines.
- **Gates are pass/fail and must be read, not grepped for a line count:** `cargo test --workspace`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`.
- **Baseline is 842 passed + 12 ignored** (as of `9a44444`). This plan *adds* tests; the total must only ever go up. A drop means a moved module left its test behind.
- **Every task ends with `cargo test --workspace` green and a commit.** No task may leave the tree broken for the next one.
- **Three decisions were taken before this plan and are not open:** the `toml` crate rather than a hand-rolled reader (argv correctness in the one crate that runs foreign argv); `[checks] auto` inside `.dock/checks.toml` rather than a new `dock.toml` (spec §3 says `dock.toml`; this plan deliberately deviates and Task 8 amends that line); and the durable activity log in this row rather than deferred (it makes the observed column real and `destructive_command` live).
- **Use `git mv` for the one file that moves** (`crates/dock-ui/src/verdict.rs`), so `git log --follow` still reaches its history.
- Every new crate is `edition = "2024"`, `version = "0.1.0"`, `license = "MIT"`.

## File structure

```
crates/dock-model/src/receipt.rs      NEW  Receipt, Claimed, Observed, Witnessed, Decided,
                                           CheckRun, CheckOutcome, ToolCall, Finding, Rule,
                                           Severity, Verdict (moved here from dock-ui)
crates/dock-model/src/env.rs          NEW  environment_is_allowed — the child-environment policy,
                                           moved down out of dock-pty so both spawners share it
crates/dock-model/src/storage.rs      MOD  save_receipt / load_receipt / list_receipts
crates/dock-model/src/protocol.rs     MOD  ReportAgentStateRequest.tool_detail; v17 → v18
crates/dock-receipt/src/lib.rs        NEW  crate docs + the exec-surface exemption comment
crates/dock-receipt/src/declaration.rs NEW `.dock/checks.toml` + `~/.config/dock/checks.toml`
crates/dock-receipt/src/runner.rs     NEW  spawn, cwd, env, stdin, timeout, group kill, tail
crates/dock-receipt/src/rules.rs      NEW  the nine rules and the verdict
crates/dock-pty/src/runtime.rs        MOD  use dock_model::env::environment_is_allowed
crates/dock-daemon/src/dispatch.rs    MOD  activity ring holds ToolCall; submit_handoff writes
                                           a receipt and runs the declared checks
src/cli/verdict.rs                    NEW  dock verdict explain | recheck
src/main.rs                           MOD  the `verdict` verb; `--check=` becomes name-only
README.md                             MOD  the trust model, in the spec's own words
```

---

### Task 1: The receipt's shape, and `Verdict` moves down

`Verdict` currently lives in `dock-ui`, which nothing below the UI may depend on. Spec §7 puts `Receipt`, `Finding` and `Verdict` in `dock-model`, and every consumer — daemon, CLI, UI — can reach it there.

**Files:**
- Create: `crates/dock-model/src/receipt.rs`
- Modify: `crates/dock-model/src/lib.rs`, `crates/dock-model/src/storage.rs`
- Move: `crates/dock-ui/src/verdict.rs` → folded into `crates/dock-model/src/receipt.rs` (`git mv` first, then merge)
- Modify: `crates/dock-ui/src/lib.rs`

**Interfaces:**
- Produces: `dock_model::receipt::{Receipt, Claimed, Observed, Witnessed, Decided, CheckRun, CheckOutcome, ToolCall, Finding, Rule, Severity, Verdict, RECEIPT_SCHEMA_VERSION, RULES_VERSION}`; `LocalStore::{save_receipt, load_receipt, list_receipts}`.

- [ ] **Step 1: Move the file, so history follows it**

```bash
git mv crates/dock-ui/src/verdict.rs crates/dock-model/src/receipt.rs
git log --follow --oneline -3 -- crates/dock-model/src/receipt.rs   # must show commits from before this plan
```

- [ ] **Step 2: Write the failing test**

Append to `crates/dock-model/src/receipt.rs`, inside its existing `mod tests` (the three `Verdict` tests that came with the file stay exactly as they are):

```rust
    fn receipt_fixture() -> Receipt {
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
                untracked: vec![".env.local".into()],
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
        let older = Receipt { rules_version: RULES_VERSION - 1, ..receipt_fixture() };
        assert_ne!(older.rules_version, receipt.rules_version);
        assert_eq!(older.verdict, receipt.verdict);
    }
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p dock-model receipt`
Expected: FAIL — `cannot find type Receipt in this scope`.

- [ ] **Step 4: Write the types**

At the top of `crates/dock-model/src/receipt.rs`, above the moved `Verdict`. Every struct is `#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]` with `#[serde(deny_unknown_fields)]`; `Verdict` additionally gains `Serialize, Deserialize` with `#[serde(rename_all = "snake_case")]`.

```rust
//! What a run left behind, and what Dock concluded about it.
//!
//! Four authored columns and one derived one. **Nothing may be written across columns.** The
//! agent writes `claimed` and can never write `witnessed`; Dock writes `witnessed` and can never
//! write `decided`. That is the whole trust model, and these are separate types so that it is a
//! property of the shape rather than of everyone's good intentions.

/// The receipt format. Bumped when a field's meaning changes, never for an addition that
/// defaults.
pub const RECEIPT_SCHEMA_VERSION: u16 = 1;

/// The rule set that produced a verdict. Stored in the receipt so an old receipt can show the
/// verdict it was given while `dock verdict recheck` reports where today's rules disagree.
pub const RULES_VERSION: u16 = 1;

/// What the agent said. Dock never writes here.
pub struct Claimed {
    pub summary: String,
    pub question: Option<String>,
    /// The *names* of checks the agent asks Dock to run. Never a command: a name is looked up in
    /// `.dock/checks.toml`, and one map lookup is the whole containment argument.
    pub checks: Vec<String>,
}

/// What git and the hook payloads saw. Neither the agent nor the reviewer writes here.
pub struct Observed {
    pub base_sha: String,
    pub head_sha: String,
    pub changed_files: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Paths, not contents. `sensitive_new_file` reads these names and nothing else.
    pub untracked: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// One tool call a hook reported, with the time it happened.
pub struct ToolCall {
    pub at_unix_ms: u64,
    pub tool: String,
    /// The identifying argument — a path for an edit, the command line for a shell call. Capped
    /// at `TOOL_DETAIL_LIMIT` bytes by whoever records it.
    pub detail: String,
}

/// What Dock ran and watched. The agent can never write here; that is the point of the product.
pub struct Witnessed {
    pub checks: Vec<CheckRun>,
}

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

#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    /// Could not run: unknown name, timeout, spawn error, unpermitted environment.
    Unwitnessed,
}

/// What the human decided. Dock never writes here.
pub struct Decided {
    pub route: crate::model::ReviewRoute,
    pub at_unix_ms: u64,
    pub note: String,
}

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
pub struct Finding {
    pub rule: Rule,
    /// The fact, in words a reader can check against the receipt by hand. A finding that cannot
    /// be re-derived from the lines above it does not ship.
    pub fact: String,
}

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
```

Then in `crates/dock-model/src/lib.rs` add `pub mod receipt;`, and in `crates/dock-ui/src/lib.rs` delete `pub mod verdict;`. Fix the two `use` sites in `dock-ui` that referred to `crate::verdict::Verdict` — find them with `grep -rn "verdict::" crates/dock-ui/src/`.

- [ ] **Step 5: Run the test and watch it pass**

Run: `cargo test -p dock-model receipt` — PASS, including the three moved `Verdict` tests.
Run: `cargo test -p dock-ui` — PASS, with three fewer tests than before (they moved).

- [ ] **Step 6: Give the store a receipt drawer**

In `crates/dock-model/src/storage.rs`: add `Receipt` to the `crate::model`/`crate::receipt` import, add `Receipt` to `CreateKind` (`Self::Receipt => "receipt"`), and add three methods modelled exactly on `save_handoff_record` / `load_handoff_record` / `list_handoff_inbox`:

```rust
    /// `.dock/local/receipts/<run_id>.json`, 0600, written once.
    ///
    /// Append-only in the sense that matters: `atomic_save` hard-links onto the destination and
    /// refuses an existing name, so a receipt cannot be rewritten after the fact by anything —
    /// including Dock.
    pub fn save_receipt(&self, receipt: &Receipt) -> Result<PathBuf, String> {
        if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err("unsupported receipt schema version".into());
        }
        self.atomic_save("receipts", &receipt.run_id, receipt, CreateKind::Receipt)
    }

    pub fn load_receipt(&self, run_id: &str) -> Result<Receipt, String> {
        let receipt: Receipt = self.load("receipts", run_id)?;
        if receipt.run_id != run_id {
            return Err("stored receipt run_id does not match its requested filename".into());
        }
        Ok(receipt)
    }

    /// Every receipt that parsed, plus the ones that did not, so one corrupt file cannot hide
    /// the rest — the same contract `list_handoff_inbox` has, for the same reason.
    pub fn list_receipts(&self) -> Result<(Vec<Receipt>, Vec<(String, String)>), String> {
        // Body mirrors `list_handoff_inbox`: read_dir, NotFound → empty, strip `.json`,
        // load_receipt per entry, Ok → records, Err → skipped.
    }
```

- [ ] **Step 7: Test the drawer**

In `storage.rs`'s `mod tests`, beside the existing handoff-storage tests:

```rust
    #[test]
    fn a_receipt_is_written_once_at_0600_and_read_back_whole() {
        let store = LocalStore::new(temporary_root("receipt-store"));
        let receipt = receipt_fixture();
        let path = store.save_receipt(&receipt).expect("save receipt");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.load_receipt(&receipt.run_id).unwrap(), receipt);
        // A second write of the same run is refused rather than silently overwriting evidence.
        assert!(store.save_receipt(&receipt).is_err());
    }
```

Reuse whatever the existing storage tests use for a temporary root — read the top of `storage.rs`'s `mod tests` and follow it; do not invent a second convention.

- [ ] **Step 8: Verify and commit**

```bash
cargo test --workspace 2>&1 | grep "^test result:"   # sum: must exceed 842 passed, 0 failed
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: the receipt has a shape, and the verdict moves to where every reader can reach it"
```

---

### Task 2: `.dock/checks.toml`, and the lookup that is the containment argument

No process is spawned in this task. It ends with a resolver that turns a name into an argv or into a refusal with a sentence.

**Files:**
- Create: `crates/dock-receipt/Cargo.toml`, `crates/dock-receipt/src/lib.rs`, `crates/dock-receipt/src/declaration.rs`
- Modify: `Cargo.toml` (workspace member is already `crates/*`; add nothing), `clippy.toml` (nothing — the crate opts out at its own root)

**Interfaces:**
- Consumes: nothing from Task 1 yet.
- Produces: `dock_receipt::declaration::{Checks, Check, Resolved, load, load_permits}` with `Checks::resolve(&self, name: &str) -> Resolved`.

- [ ] **Step 1: The manifest**

`crates/dock-receipt/Cargo.toml`:

```toml
[package]
name = "dock-receipt"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Declared checks: the only crate that executes argv Dock did not write."

[dependencies]
nix = { version = "=0.29.0", features = ["process", "signal"] }
serde = { version = "1", features = ["derive"] }
toml = "0.9"
dock-git = { path = "../dock-git", version = "0.1.0" }
dock-model = { path = "../dock-model", version = "0.1.0" }

[dev-dependencies]
dock-testing = { path = "../dock-testing", version = "0.1.0" }
```

`crates/dock-receipt/src/lib.rs`:

```rust
//! Declared checks: reading them, running them, and deciding what they add up to.
//!
//! Dock may execute a command **declared by the repository or the user** — never one an agent
//! composed — in the run's bound worktree, at a pinned SHA, under a cleared and allowlisted
//! environment. An agent names a check; the name is looked up in `.dock/checks.toml` and an
//! unknown one is recorded `unwitnessed` rather than run. Dock still never stages, commits,
//! rebases, merges, pushes, or removes a worktree.
// The exec surface. This is the one crate whose argv comes from a file Dock did not write, which
// is exactly why the rest of the workspace denies `Command::new` and this crate is the only
// place a reviewer has to look. See spec section 7.
#![allow(clippy::disallowed_methods)]

pub mod declaration;
pub mod rules;
pub mod runner;
```

(Add the `rules` and `runner` modules as empty files now so the crate compiles; Tasks 3 and 4 fill them.)

- [ ] **Step 2: Write the failing tests**

`crates/dock-receipt/src/declaration.rs`, `mod tests`:

```rust
    const SAMPLE: &str = r#"
[checks]
auto = false

[check.test]
run     = ["cargo", "test", "--locked"]
timeout = "10m"

[check.publish]
run       = ["npm", "publish"]
needs_env = ["NPM_TOKEN"]
"#;

    #[test]
    fn a_declared_name_resolves_to_the_argv_the_repository_wrote() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        assert!(!checks.auto);
        let Resolved::Check(check) = checks.resolve("test", &[]) else {
            panic!("`test` is declared and needs no environment");
        };
        assert_eq!(check.run, ["cargo", "test", "--locked"]);
        assert_eq!(check.timeout, Duration::from_secs(600));
    }

    /// The whole containment argument, in one assertion: a name nobody declared produces a
    /// refusal carrying the name, and there is nothing here that could become a command.
    #[test]
    fn an_undeclared_name_is_a_refusal_that_names_itself() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        let Resolved::Unwitnessed(reason) = checks.resolve("typo", &[]) else {
            panic!("an undeclared name must never resolve to a command");
        };
        assert_eq!(reason, "no check named `typo` in .dock/checks.toml");
    }

    /// The repository may name a secret; only the user may permit it. An unpermitted request is
    /// refused in words rather than silently running without the variable.
    #[test]
    fn a_secret_the_user_has_not_permitted_is_refused_by_name() {
        let checks = Checks::parse(SAMPLE).expect("parse checks");
        let Resolved::Unwitnessed(reason) = checks.resolve("publish", &[]) else {
            panic!("an unpermitted variable must not run");
        };
        assert_eq!(
            reason,
            "`NPM_TOKEN` was requested by .dock/checks.toml and is not permitted in your user config."
        );
        assert!(matches!(
            checks.resolve("publish", &["NPM_TOKEN".to_owned()]),
            Resolved::Check(_)
        ));
    }

    /// A shell string is not an argv array, and the difference is the feature.
    #[test]
    fn a_run_that_is_not_an_argv_array_is_rejected_at_parse_time() {
        assert!(Checks::parse("[check.x]\nrun = \"cargo test && rm -rf /\"").is_err());
        assert!(Checks::parse("[check.x]\nrun = []").is_err());
    }

    /// Unknown keys are rejected rather than ignored: a declaration Dock half-understands is a
    /// declaration whose author believes something Dock is not doing.
    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_shrug() {
        assert!(Checks::parse("[check.x]\nrun = [\"true\"]\nshell = true").is_err());
    }

    #[test]
    fn a_timeout_defaults_to_ten_minutes_and_understands_s_m_h() {
        assert_eq!(parse_timeout(None).unwrap(), Duration::from_secs(600));
        assert_eq!(parse_timeout(Some("90s")).unwrap(), Duration::from_secs(90));
        assert_eq!(parse_timeout(Some("5m")).unwrap(), Duration::from_secs(300));
        assert_eq!(parse_timeout(Some("1h")).unwrap(), Duration::from_secs(3600));
        assert!(parse_timeout(Some("soon")).is_err());
        assert!(parse_timeout(Some("0m")).is_err());
    }
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p dock-receipt declaration`
Expected: FAIL — `cannot find struct Checks`.

- [ ] **Step 4: Implement the reader**

```rust
//! `.dock/checks.toml`, committed; and `~/.config/dock/checks.toml`, never committed.
//!
//! The repository declares what may run. The user declares which of their environment variables
//! a repository is allowed to see. Neither file may name the other's business, and an agent
//! writes to neither.

use std::{collections::BTreeMap, path::Path, time::Duration};

use serde::Deserialize;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The parsed declaration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    /// Whether checks run automatically at handoff. `r` from the receipt rail works either way.
    pub auto: bool,
    checks: BTreeMap<String, Check>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub run: Vec<String>,
    pub timeout: Duration,
    pub needs_env: Vec<String>,
}

/// What a name resolved to: something Dock may run, or a sentence saying why it may not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Check(Check),
    Unwitnessed(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    checks: Option<Settings>,
    #[serde(default)]
    check: BTreeMap<String, Declaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default = "yes")]
    auto: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    /// An argv array. `serde` rejects a bare string here, which is the point: no `sh -c`, so no
    /// interpolation, no glob, no chaining. A pipeline is a committed script.
    run: Vec<String>,
    timeout: Option<String>,
    #[serde(default)]
    needs_env: Vec<String>,
}

const fn yes() -> bool { true }

impl Checks {
    /// Reads `<repository>/.dock/checks.toml`. A repository with no file has no checks, which is
    /// not an error — it is the state that earns `no_checks_declared`.
    pub fn load(repository_root: &Path) -> Result<Self, String> { /* read_to_string, NotFound → Self::none(), then parse */ }

    pub fn parse(source: &str) -> Result<Self, String> {
        let file: File = toml::from_str(source)
            .map_err(|error| format!("could not read .dock/checks.toml: {error}"))?;
        let mut checks = BTreeMap::new();
        for (name, declaration) in file.check {
            if declaration.run.is_empty() {
                return Err(format!("check `{name}` declares an empty command"));
            }
            checks.insert(name.clone(), Check {
                name,
                timeout: parse_timeout(declaration.timeout.as_deref())?,
                run: declaration.run,
                needs_env: declaration.needs_env,
            });
        }
        Ok(Self { auto: file.checks.map_or(true, |settings| settings.auto), checks })
    }

    /// One map lookup. This is the containment argument: an agent supplies `name`, and the only
    /// thing a name can become is a value already in this map or a refusal.
    pub fn resolve(&self, name: &str, permitted: &[String]) -> Resolved {
        let Some(check) = self.checks.get(name) else {
            return Resolved::Unwitnessed(format!("no check named `{name}` in .dock/checks.toml"));
        };
        if let Some(missing) = check.needs_env.iter().find(|name| !permitted.contains(name)) {
            return Resolved::Unwitnessed(format!(
                "`{missing}` was requested by .dock/checks.toml and is not permitted in your user config."
            ));
        }
        Resolved::Check(check.clone())
    }
}

/// `~/.config/dock/checks.toml`'s `[permit] env`, following the same home convention
/// `dock_detect::manifest::override_dir` already uses. Absent file, empty list, no error.
pub fn load_permits() -> Result<Vec<String>, String> { /* HOME/.config/dock/checks.toml, [permit] env */ }

fn parse_timeout(value: Option<&str>) -> Result<Duration, String> {
    let Some(value) = value else { return Ok(DEFAULT_TIMEOUT) };
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        _ => return Err(format!("timeout `{value}` must end in s, m or h")),
    };
    let seconds: u64 = number.parse().map_err(|_| format!("timeout `{value}` is not a number"))?;
    if seconds == 0 {
        return Err(format!("timeout `{value}` is zero, which would witness nothing"));
    }
    Ok(Duration::from_secs(seconds * scale))
}
```

- [ ] **Step 5: Verify and commit**

```bash
cargo test -p dock-receipt          # the six declaration tests pass
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: a check is a name the repository declared, and nothing else"
```

---

### Task 3: The runner, which is the only place Dock runs someone else's argv

**Files:**
- Create: `crates/dock-model/src/env.rs`
- Modify: `crates/dock-model/src/lib.rs`, `crates/dock-pty/src/runtime.rs`
- Create: `crates/dock-receipt/src/runner.rs`

**Interfaces:**
- Consumes: `declaration::Check` (Task 2); `dock_model::receipt::{CheckRun, CheckOutcome}` (Task 1); `dock_git::GitAdapter::facts` for the SHA pins.
- Produces: `dock_receipt::runner::{run, Lane, TAIL_LINES, TAIL_BYTES}` with
  `pub fn run(check: &Check, worktree: &Path, permitted_env: &[String], run_id: &str) -> CheckRun`.

- [ ] **Step 1: Move the environment policy down**

`environment_is_allowed` is private in `crates/dock-pty/src/runtime.rs:945`. Two crates now need the same answer, and the policy is about environment variables rather than about PTYs. Move the function and its doc comment verbatim into `crates/dock-model/src/env.rs` as `pub fn environment_is_allowed(key: &std::ffi::OsStr) -> bool`, add `pub mod env;` to `dock-model`'s lib, and change `runtime.rs` to call `dock_model::env::environment_is_allowed`. `apply_child_environment` stays in `dock-pty`: it takes a `Command`, and `dock-model` may not touch one.

Move the existing environment-allowlist test with it. Run `cargo test --workspace` — the total must not drop.

- [ ] **Step 2: Write the failing tests**

`crates/dock-receipt/src/runner.rs`, `mod tests`. These are real processes, per spec §11:

```rust
    fn check(name: &str, run: &[&str], timeout: Duration) -> Check {
        Check { name: name.into(), run: run.iter().map(|a| (*a).to_owned()).collect(),
                timeout, needs_env: Vec::new() }
    }

    #[test]
    fn a_check_that_passes_is_witnessed_green_with_the_sha_it_ran_at() {
        let repo = fixture_repo("runner-pass");
        let outcome = run(&check("ok", &["true"], Duration::from_secs(5)), &repo, &[], "run_1");
        assert_eq!(outcome.outcome, CheckOutcome::Passed);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.sha_before, outcome.sha_after);
        assert!(!outcome.sha_before.is_empty(), "a check with no SHA witnesses nothing");
    }

    #[test]
    fn a_check_that_fails_carries_its_code_and_the_tail_of_what_it_said() {
        let repo = fixture_repo("runner-fail");
        let outcome = run(
            &check("no", &["sh", "-c", "echo boom >&2; exit 3"], Duration::from_secs(5)),
            &repo, &[], "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Failed);
        assert_eq!(outcome.exit_code, Some(3));
        assert!(outcome.tail.contains("boom"), "{:?}", outcome.tail);
    }

    /// The tail is capped so a receipt cannot become a log file.
    #[test]
    fn a_loud_check_is_cut_to_the_last_lines_rather_than_stored_whole() {
        let repo = fixture_repo("runner-loud");
        let outcome = run(
            &check("loud", &["sh", "-c", "seq 1 5000"], Duration::from_secs(20)),
            &repo, &[], "run_1",
        );
        assert!(outcome.tail.len() <= TAIL_BYTES, "{} bytes", outcome.tail.len());
        assert!(outcome.tail.lines().count() <= TAIL_LINES);
        // The *last* lines, because that is where a failure says why.
        assert!(outcome.tail.contains("5000"), "{:?}", outcome.tail);
    }

    /// A check that outlives its timeout is unwitnessed, and its whole process group is gone —
    /// not just the process Dock spawned.
    #[test]
    fn a_check_that_overruns_is_killed_by_the_group_and_recorded_unwitnessed() {
        let repo = fixture_repo("runner-timeout");
        let outcome = run(
            &check("slow", &["sh", "-c", "sleep 30 & sleep 30"], Duration::from_millis(300)),
            &repo, &[], "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Unwitnessed);
        assert!(outcome.reason.as_deref().is_some_and(|r| r.contains("timed out")));
        assert!(outcome.duration_ms < 10_000, "the kill did not happen promptly");
    }

    /// A check may not see a credential-shaped variable it was not permitted. This extends the
    /// allowlist test that moved to `dock-model` in step 1 to the process that actually runs.
    #[test]
    fn a_check_cannot_see_a_credential_the_user_did_not_permit() {
        let repo = fixture_repo("runner-env");
        unsafe { std::env::set_var("NPM_TOKEN", "super-secret") };
        let outcome = run(
            &check("env", &["sh", "-c", "echo [$NPM_TOKEN][$PATH]"], Duration::from_secs(5)),
            &repo, &[], "run_1",
        );
        unsafe { std::env::remove_var("NPM_TOKEN") };
        assert!(!outcome.tail.contains("super-secret"), "{:?}", outcome.tail);
        assert!(outcome.tail.contains("[]["), "PATH must survive: {:?}", outcome.tail);
    }

    /// A check that asks for the keyboard hangs, times out, and is recorded — it does not get
    /// the terminal, and it does not block the daemon waiting for someone to type.
    #[test]
    fn a_check_that_reads_stdin_gets_end_of_file_rather_than_the_keyboard() {
        let repo = fixture_repo("runner-stdin");
        let outcome = run(
            &check("ask", &["sh", "-c", "read answer; echo [$answer]"], Duration::from_secs(5)),
            &repo, &[], "run_1",
        );
        assert_eq!(outcome.outcome, CheckOutcome::Failed);  // `read` fails at EOF
        assert!(outcome.tail.contains("[]"), "{:?}", outcome.tail);
    }

    /// `cwd` is the bound worktree, never wherever the daemon happens to be.
    #[test]
    fn a_check_runs_in_the_bound_worktree() {
        let repo = fixture_repo("runner-cwd");
        let outcome = run(&check("where", &["pwd"], Duration::from_secs(5)), &repo, &[], "run_1");
        assert!(outcome.tail.contains(&repo.display().to_string()), "{:?}", outcome.tail);
    }
```

`fixture_repo` creates a temporary git repository with one commit, exactly as `dispatch.rs`'s `Repo::new` does — read that and follow it rather than inventing a second fixture shape.

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p dock-receipt runner` — FAIL, `cannot find function run`.

- [ ] **Step 4: Implement the runner**

Key mechanics, each of which a test above pins:

```rust
use std::os::unix::process::CommandExt as _;   // process_group

pub const TAIL_LINES: usize = 200;
pub const TAIL_BYTES: usize = 64 * 1024;

pub fn run(check: &Check, worktree: &Path, permitted_env: &[String], run_id: &str) -> CheckRun {
    // 1. Pin before. `GitAdapter::new(worktree).facts("HEAD")` gives head_sha and, via
    //    status_entries, whether the tree was dirty. A worktree whose facts cannot be read is
    //    unwitnessed with that error as the reason — a check with no SHA witnesses nothing.
    // 2. Spawn. `Command::new(&check.run[0]).args(&check.run[1..])`
    //    .current_dir(worktree)                    — never the primary checkout
    //    .stdin(Stdio::null())                     — never interactive
    //    .stdout(Stdio::piped()).stderr(Stdio::piped())
    //    .process_group(0)                         — Dock owns the group, as it does for a pane
    //    then env: `env_clear()`, re-add the pairs `environment_is_allowed` keeps, plus each
    //    name in `check.needs_env` that appears in `permitted_env`, plus `DOCK_RUN_ID=run_id`.
    // 3. Drain both pipes on their own threads into a bounded tail buffer. This is not optional:
    //    a check that fills the 64 KB pipe buffer while Dock waits on `wait()` deadlocks, and
    //    `seq 1 5000` in the tests is enough to do it.
    // 4. Wait with a deadline: a thread does `child.wait()` and sends the status down a channel;
    //    the caller does `recv_timeout(check.timeout)`. On timeout, `killpg(pid, SIGTERM)`, wait
    //    5s, then `killpg(pid, SIGKILL)`, and record Unwitnessed with
    //    `format!("timed out after {}s", check.timeout.as_secs())`.
    // 5. Pin after, the same way as step 1, and build the `CheckRun`.
}
```

The lane limit — `min(4, available_parallelism / 2)`, at least 1 — is a `static` semaphore in this module, acquired around step 2–4, so eight simultaneous handoffs do not fork-bomb the machine. One check at a time *per run* falls out of the caller running a run's checks in sequence.

- [ ] **Step 5: Verify and commit**

```bash
cargo test -p dock-receipt
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: Dock runs the check itself, and writes down what it saw"
```

---

### Task 4: The nine rules, and a verdict a reader can re-derive

**Files:**
- Create: `crates/dock-receipt/src/rules.rs`

**Interfaces:**
- Consumes: `dock_model::receipt::{Receipt, Finding, Rule, Severity, Verdict, CheckOutcome}`.
- Produces: `pub fn evaluate(receipt: &Receipt) -> (Verdict, Vec<Finding>)` and
  `pub fn inert(rule: Rule) -> Option<&'static str>`.

- [ ] **Step 1: Write the failing table test**

The verdict is a pure function, so it is table-tested: one case per rule firing, one per rule not firing, one for severity precedence, one for "no checks declared can never be clear".

```rust
    /// Every rule fires on the case it names, and none of the others fire with it.
    #[test]
    fn each_rule_fires_on_its_own_fact_and_nothing_else() {
        for (rule, receipt) in [
            (Rule::CheckFailed, receipt_with_a_failed_check()),
            (Rule::CheckStale, receipt_with_a_check_green_at_an_older_sha()),
            (Rule::CheckUnwitnessed, receipt_with_an_undeclared_check_name()),
            (Rule::CheckMutatedWorktree, receipt_with_a_check_that_dirtied_the_tree()),
            (Rule::NoChecksDeclared, receipt_with_no_checks()),
            (Rule::EmptyDiff, receipt_with_a_claim_and_no_diff()),
            (Rule::DestructiveCommand, receipt_with_a_git_reset_hard_in_its_tool_calls()),
            (Rule::SensitiveNewFile, receipt_with_an_untracked_pem()),
        ] {
            let (_, findings) = evaluate(&receipt);
            let fired: Vec<Rule> = findings.iter().map(|finding| finding.rule).collect();
            assert!(fired.contains(&rule), "{} did not fire: {fired:?}", rule.name());
        }
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
        assert!(findings.len() >= 2, "the looks are still reported: {findings:?}");
    }

    /// Every finding names the fact that produced it, because a verdict that cannot be
    /// re-derived by hand does not ship.
    #[test]
    fn every_finding_carries_the_fact_it_read() {
        let (_, findings) = evaluate(&receipt_with_a_failed_check());
        let failed = findings.iter().find(|f| f.rule == Rule::CheckFailed).unwrap();
        assert!(failed.fact.contains("test"), "{:?}", failed.fact);
        assert!(failed.fact.contains("exit 1"), "{:?}", failed.fact);
    }

    /// A rule with no data source yet says so, rather than silently never firing.
    #[test]
    fn peer_conflict_is_declared_inert_until_the_ledger_exists() {
        assert_eq!(inert(Rule::PeerConflict), Some("no ledger yet — row 5 supplies it"));
        assert_eq!(inert(Rule::CheckFailed), None);
    }
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p dock-receipt rules` — FAIL, `cannot find function evaluate`.

- [ ] **Step 3: Implement the rules**

Each rule reads only the receipt. The exact conditions, from spec §2:

| Rule | Condition, in fields |
|---|---|
| `check_failed` | any `CheckRun.outcome == Failed` |
| `check_stale` | any `CheckRun.sha_after != observed.head_sha` |
| `check_unwitnessed` | any `CheckRun.outcome == Unwitnessed` — the fact is its `reason` |
| `check_mutated_worktree` | any `CheckRun` where `dirty_before != dirty_after` or `sha_before != sha_after` |
| `no_checks_declared` | `claimed.checks.is_empty()` |
| `empty_diff` | `!claimed.summary.trim().is_empty()` and `observed.base_sha == observed.head_sha` and `observed.untracked.is_empty()` |
| `peer_conflict` | inert — no data source until row 5 |
| `destructive_command` | any `ToolCall` whose `detail` contains `rm -rf`, `git reset --hard`, `git push`, or `git clean` |
| `sensitive_new_file` | any `observed.untracked` path whose file name matches `.env*`, `*.pem`, `id_*` |

The verdict: `Failed` if any finding's severity is `Failed`, else `Look` if any finding at all, else `Clear`.

Two notes the implementer must not lose. `sensitive_new_file`'s "larger than 1 MB" clause needs a file size, which the receipt does not carry — record the size in `Observed.untracked` as a `(path, bytes)` pair when Task 6 assembles it, or drop the clause and say so in the report. Do not silently implement half the rule. And `destructive_command` matches on a substring of the *detail*, which is truncated at record time; Task 5 sets that limit, so pick it there with this rule in mind.

- [ ] **Step 4: Verify and commit**

```bash
cargo test -p dock-receipt
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: the verdict is arithmetic over evidence, and says which fact it read"
```

---

### Task 5: The observed column gets a clock

Hook activity is currently a truncated string in an in-memory ring (`dispatch.rs:304`), replaced on every report. The receipt needs tool calls with timestamps, and `destructive_command` needs the command rather than a 72-character summary of it.

**Files:**
- Modify: `crates/dock-model/src/protocol.rs`, `crates/dock-daemon/src/dispatch.rs`, `crates/dock-daemon/src/hook.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `dock_model::receipt::ToolCall` (Task 1).
- Produces: `Dispatcher::tool_calls(&self, run_id: &str) -> Vec<ToolCall>`.

- [ ] **Step 1: Write the failing tests**

In `protocol.rs`'s tests (the version assertion at line 855 is the one to update) and `dispatch.rs`'s tests:

```rust
    /// The field is additive and defaulted, so a client built against v17 still parses.
    #[test]
    fn a_state_report_may_carry_the_untruncated_tool_detail() {
        assert_eq!(PROTOCOL_VERSION, 18);
        let request: ReportAgentStateRequest = serde_json::from_str(
            r#"{"type":"report_agent_state","run_id":"dock_1","state":"working","session_id":"s","tool_name":"Bash","activity":"Bash git reset --hard","tool_detail":"git reset --hard HEAD~3"}"#,
        ).expect("parse v18 report");
        assert_eq!(request.tool_detail.as_deref(), Some("git reset --hard HEAD~3"));
        // A v17 report has no such field and must still parse.
        let older: ReportAgentStateRequest = serde_json::from_str(
            r#"{"type":"report_agent_state","run_id":"dock_1","state":"working","session_id":"s"}"#,
        ).expect("parse v17 report");
        assert_eq!(older.tool_detail, None);
    }
```

```rust
    /// The ring keeps what a receipt needs — when, what tool, and what it was pointed at — and
    /// stays bounded, because a long run must cost the same as a short one.
    #[test]
    fn the_activity_ring_keeps_timestamped_tool_calls_and_stays_bounded() {
        let registry = registry();
        for index in 0..(ACTIVITY_RING_CAPACITY + 25) {
            registry.report_agent_state(state_report("run_1", "Bash", &format!("echo {index}")));
        }
        let calls = registry.tool_calls("run_1");
        assert_eq!(calls.len(), ACTIVITY_RING_CAPACITY);
        assert!(calls.first().unwrap().at_unix_ms <= calls.last().unwrap().at_unix_ms);
        assert!(calls.last().unwrap().detail.contains("echo 224"));
    }

    /// The detail is capped, because a receipt is not a transcript — but the cap is generous
    /// enough that `destructive_command` still sees the command it has to match.
    #[test]
    fn a_very_long_tool_detail_is_cut_rather_than_stored_whole() {
        let registry = registry();
        registry.report_agent_state(state_report("run_1", "Bash", &"x".repeat(9_000)));
        assert!(registry.tool_calls("run_1")[0].detail.len() <= TOOL_DETAIL_LIMIT);
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p dock-model protocol && cargo test -p dock-daemon activity_ring` — FAIL on the missing field and the missing method.

- [ ] **Step 3: Implement**

- `protocol.rs`: add `#[serde(default, skip_serializing_if = "Option::is_none")] pub tool_detail: Option<String>` to `ReportAgentStateRequest`; bump `PROTOCOL_VERSION` to `18`. Additive and defaulted, so the old dashboard and the new daemon still speak.
- `hook.rs`: add `pub fn tool_detail(payload: &HookPayload) -> Option<String>` returning the same field `tool_detail` already finds for `activity_summary`, but untruncated. `activity_summary` keeps its 72-character cut — that one is for a roster row.
- `main.rs`: the hook path at line ~1675 sends both.
- `dispatch.rs`: change `activity_rings: Mutex<HashMap<String, VecDeque<String>>>` to hold `ToolCall`, with `const ACTIVITY_RING_CAPACITY: usize = 200;` and `const TOOL_DETAIL_LIMIT: usize = 2_000;`. `latest_activity` keeps returning a `String` for the roster — build it from the last entry. Add `pub fn tool_calls(&self, run_id: &str) -> Vec<ToolCall>`.

- [ ] **Step 4: Verify and commit**

```bash
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: what the agent did, with a clock on it"
```

---

### Task 6: A handoff produces a receipt

This is where the four columns meet. `submit_handoff` (`dispatch.rs:3494`) already validates the binding and collects `GitFacts`; it now also runs the declared checks and writes the receipt.

**Files:**
- Modify: `crates/dock-daemon/src/dispatch.rs`, `crates/dock-daemon/Cargo.toml` (add `dock-receipt`), `crates/dock-model/src/model.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces: a `Receipt` at `.dock/local/receipts/<run_id>.json` for every handoff.

- [ ] **Step 1: `Check` loses its claim**

`model::Check { name, passed }` lets the agent assert a result. That is the claim the product exists to replace. Change `HandoffPacket.checks` to `Vec<String>` — names only. Update `validate_concise_safe_packet`'s length guards (`check.name.len()` becomes `name.len()`), `main.rs`'s `--check=` parsing (drop the `:pass` / `:fail` suffix handling at line 2093 and the comment above it), and the dashboard's review overlay where it renders `Check`. Delete `model::Check`.

Write the test first:

```rust
    /// An agent names a check. It cannot report one: `passed` was a claim wearing the costume of
    /// evidence, and the witnessed column is where that question is answered now.
    #[test]
    fn a_handoff_names_checks_and_cannot_assert_their_results() {
        let unknown = r#"{"schema_version":1,"run_id":"r","task_id":"t","workspace_id":"w",
            "pane_id":"p","worktree":"wt","branch":"b","base_sha":"sha","summary":"s",
            "question":null,"checks":[{"name":"test","passed":true}]}"#;
        assert!(serde_json::from_str::<HandoffPacket>(unknown).is_err());
    }
```

- [ ] **Step 2: Write the failing integration test**

In `dispatch.rs`'s tests, following `strict_handoff_attaches_current_git_evidence_and_routes_explicit_decisions`:

```rust
    /// Success criterion 1: the receipt shows a command Dock ran, an exit code Dock observed and
    /// the SHA it ran at — none of which the agent could have written.
    #[test]
    fn a_handoff_writes_a_receipt_the_agent_could_not_have_forged() {
        let repo = Repo::new("handoff-receipt");
        fs::create_dir_all(repo.root.join("fixture/.dock")).unwrap();
        fs::write(
            repo.root.join("fixture/.dock/checks.toml"),
            "[check.test]\nrun = [\"true\"]\ntimeout = \"1m\"\n",
        ).unwrap();
        let registry = registry_for(&repo);
        let snapshot = registry.dispatch(repo.request("dock_receipt_1")).unwrap();
        let mut packet = packet_for(&snapshot);
        packet.checks = vec!["test".into()];

        registry.submit_handoff(packet).expect("submit handoff");

        let receipt = LocalStore::new(&repo.state).load_receipt("dock_receipt_1").unwrap();
        let witnessed = &receipt.witnessed.checks[0];
        assert_eq!(witnessed.command, ["true"]);
        assert_eq!(witnessed.exit_code, Some(0));
        assert_eq!(witnessed.sha_after, receipt.observed.head_sha);
        assert_eq!(receipt.verdict, Verdict::Clear);
    }

    /// Success criterion 3: a repository with no `.dock/checks.toml` is never shown a tick.
    #[test]
    fn a_repository_that_declared_nothing_is_never_shown_a_tick() {
        // ... same shape, no checks.toml, packet.checks empty ...
        assert_eq!(receipt.verdict, Verdict::Look);
        assert!(receipt.findings.iter().any(|f| f.rule == Rule::NoChecksDeclared));
    }

    /// Success criterion 2: a stale green is caught. The check passes, then the agent edits.
    #[test]
    fn an_edit_after_a_green_check_is_caught_as_stale() {
        // ... run the check, then commit another file in the worktree, then submit ...
        assert!(receipt.findings.iter().any(|f| f.rule == Rule::CheckStale));
        assert_eq!(receipt.verdict, Verdict::Look);
    }

    /// The containment claim, asserted rather than described: a name nobody declared is recorded
    /// and never spawned.
    #[test]
    fn an_undeclared_check_name_is_recorded_unwitnessed_and_never_run() {
        // ... packet.checks = vec!["definitely-not-declared".into()] ...
        assert_eq!(witnessed.outcome, CheckOutcome::Unwitnessed);
        assert!(witnessed.command.is_empty(), "nothing may have been spawned");
        assert_eq!(
            witnessed.reason.as_deref(),
            Some("no check named `definitely-not-declared` in .dock/checks.toml")
        );
    }
```

- [ ] **Step 3: Run them and watch them fail**

Run: `cargo test -p dock-daemon receipt` — FAIL, no receipt is written.

- [ ] **Step 4: Implement**

In `submit_handoff`, after the existing `GitAdapter::facts` call and its binding checks, and before `save_handoff_record`:

1. `let checks = dock_receipt::declaration::Checks::load(Path::new(&snapshot.repository_root))?` — a read failure is not a refused handoff; it becomes a single `check_unwitnessed` finding carrying the parse error.
2. If `checks.auto`, resolve and run each name in `packet.checks` in order, one at a time. If not, record every name `Unwitnessed` with the reason `"checks.auto is false; press r to run them"`.
3. Assemble `Observed` from `facts` plus `self.tool_calls(&packet.run_id)`, and the untracked path list from `GitAdapter::review`'s `files` where `untracked`.
4. `let (verdict, findings) = dock_receipt::rules::evaluate(&receipt)` — build the receipt with `Verdict::Look` and empty findings first, evaluate, then write the result back before saving. `decided` is `None`: Dock never writes that column.
5. `self.store.save_receipt(&receipt)`.

A failure to write the receipt must not lose the handoff: the handoff record saves first, and a receipt failure is reported as a diagnostic on the run rather than as a rejected handoff.

- [ ] **Step 5: Verify and commit**

```bash
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: a handoff now leaves a receipt behind"
```

---

### Task 7: `dock verdict explain`, and `dock verdict recheck`

A verdict that cannot be re-derived by hand does not ship, so this is not optional polish.

**Files:**
- Create: `src/cli/verdict.rs`
- Modify: `src/cli/mod.rs`, `src/main.rs` (the `VERBS` table)

- [ ] **Step 1: Write the failing test**

```rust
    /// Rule by rule, with the fact each one read, and the rules that did not fire listed as not
    /// firing — a reader has to be able to check the arithmetic, including the zeroes.
    #[test]
    fn explain_prints_every_rule_and_the_fact_behind_each_finding() {
        let text = explain_text(&receipt_with_a_failed_check());
        assert!(text.contains("✗ failed"), "{text}");
        assert!(text.contains("check_failed"), "{text}");
        assert!(text.contains("exit 1"), "{text}");
        assert!(text.contains("empty_diff        did not fire"), "{text}");
        assert!(text.contains("peer_conflict     inert"), "{text}");
        assert!(text.contains("rules v1"), "{text}");
    }

    /// An old receipt keeps the verdict it was given; recheck reports the disagreement rather
    /// than rewriting history.
    #[test]
    fn recheck_reports_where_todays_rules_disagree_without_touching_the_receipt() {
        let stored = Receipt { rules_version: 0, verdict: Verdict::Clear, ..receipt_with_a_failed_check() };
        let text = recheck_text(&stored);
        assert!(text.contains("stored: ✓ clear (rules v0)"), "{text}");
        assert!(text.contains("today:  ✗ failed (rules v1)"), "{text}");
    }
```

- [ ] **Step 2: Run and watch it fail; then implement**

`dock verdict explain <run>` and `dock verdict recheck <run>` read `LocalStore::load_receipt` directly from the state directory, the way `--load-handoff` at `main.rs:257` already does — no daemon round trip and no protocol change. Add one `Verb` to the `VERBS` table with the summary `"why a run got the verdict it got"`.

- [ ] **Step 3: Verify and commit**

```bash
cargo test --workspace 2>&1 | grep "^test result:"
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -m "feat: dock verdict explain, so the arithmetic can be checked by hand"
```

---

### Task 8: Say it in the README, and measure what it cost

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-09-02-dock-receipts-design.md`

- [ ] **Step 1: The trust model, in those words**

Add to `README.md`, in the spec's own sentences: *An agent can never write to `witnessed`. Dock can never write to `decided`.* Plus the `.dock/checks.toml` example from spec §3 verbatim, and the sentence that a repository which declares no checks is never shown a `✓`.

- [ ] **Step 2: Amend the spec line this plan deviated from**

Spec §3 says `checks.auto = false` lives in `dock.toml`. It lives in a `[checks]` table inside `.dock/checks.toml`. Edit that line and add one sentence saying why: one file rather than two, the switch beside the thing it switches off, and a repository that declares no checks needs no config at all.

- [ ] **Step 3: Measure, before and after**

Frame time is judged against a 16.7 ms budget, measured with the existing harness rather than a later audit:

```bash
cargo test --release --workspace -- --ignored --nocapture render_measurement
cargo test --release --workspace -- --ignored --nocapture --test-threads=1 measure_
```

Record the fastest of three runs in the commit message, next to the figures Task 8 of the split plan recorded. Nothing in row 2 paints, so the expectation is no change; a regression here means something in the daemon's hot path got slower, and that is worth knowing before row 3 starts drawing receipts.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "docs: the trust model belongs in the README, in these words"
```

---

## Self-Review

**Spec coverage.** §1 the receipt → Tasks 1 (shape, storage) and 6 (assembly). §2 the verdict → Task 4, with `explain`/`recheck` in Task 7 and "never decides / never scores" carried as Global Constraints. §3 the check runner → Tasks 2 (declaration, secrets) and 3 (contract, refusals). §7 architecture → the crate created in Task 2 with the exec-surface comment; `Verdict` moved down in Task 1. §11 testing → the runner's real-process tests (Task 3), the rule table (Task 4), the no-spawn containment test (Task 6), and the palette tests are untouched. §12 success criteria 1–3 are the three named tests in Task 6; criterion 5 is Task 8. Criterion 4 (one manifest per agent) is row 4, not this row, and is deliberately absent.

**Deliberate omissions, each recorded rather than forgotten.** `peer_conflict` is implemented as declared-inert because its data source is the ledger in row 5 — spec §9 blesses exactly this treatment. The receipt rail and the `r` key are row 3; this row reaches the runner only through handoff, which is why `checks.auto = false` currently means "nothing runs" rather than "runs on `r`" — Task 6 records that in the reason string so it reads as pending rather than broken.

**Placeholder scan.** Two implementation bodies are given as numbered mechanics rather than literal code: the runner's spawn/drain/wait sequence (Task 3 step 4) and `submit_handoff`'s assembly (Task 6 step 4). Both list every step, every field and every failure mode, and both are pinned by tests written first. `Checks::load` and `load_permits` are one-line summaries of file reading whose shape is fixed by `dock_detect::manifest::override_dir`, which the step names. Everything else is real code.

**Type consistency.** `CheckRun`, `CheckOutcome`, `ToolCall`, `Finding`, `Rule`, `Severity` and `Verdict` are defined once in Task 1 and used with those exact field names in Tasks 3, 4, 6 and 7. `Checks::resolve(name, permitted) -> Resolved` is defined in Task 2 and called with that signature in Task 6. `TOOL_DETAIL_LIMIT` is introduced in Task 5 and referenced by Task 4's note about `destructive_command`'s substring match. `model::Check` is deleted in Task 6 step 1, and that is the only breaking change to an existing durable shape in this plan.

**One defect found and fixed inline.** Task 4's `sensitive_new_file` cannot implement the spec's "larger than 1 MB" clause from the receipt as shaped in Task 1: `Observed.untracked` is a list of paths with no sizes. Task 4 names the choice — carry `(path, bytes)` or drop the clause — and forbids implementing half the rule silently. Task 6 step 3 is where the size would be collected.
