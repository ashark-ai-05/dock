# conductor — product definition

Status: **for your confirmation. Nothing here is built yet.**
Companion docs: `SPEC.md` (engine contract), `ASSESSMENT.md` (why, risks, pilot).

---

## 1. One line

**Agent-written changes your reviewer can trust without re-deriving them.** Conductor runs
coding agents with separation of duties, proves the tests carry weight, and hands the
reviewer a PR that says what was proven and what was not.

## 2. Product principles

Every feature below was checked against these. A feature that breaks one is cut.

1. **The agent never grades its own work.** Different stages, fresh contexts, frozen outputs.
2. **Say "unavailable" rather than guess.** Anything not observed is labelled, never estimated.
3. **The reviewer's time is the product.** Every artifact exists to shrink what a human must read.
4. **Readable without us.** Records live in git and plain JSON. Uninstalling conductor loses nothing.
5. **Fail loudly and early.** A run that can't prove itself stops with a report. It never opens a
   hopeful PR.
6. **Zero setup beyond the repo.** No server, no account, no dashboard to begin with.

## 3. Users and jobs

| User | Job to be done | Moment of value |
|---|---|---|
| **Engineer** | "Turn this ticket into a PR I'm not embarrassed to send" | Run halts with *which* test was weak, instead of a green PR that is wrong |
| **Reviewer** | "Tell me where to look" | PR lists surviving mutants and unchecked areas; they read 3 places, not 30 files |
| **Tech lead** | "Set the bar once for every agent PR" | Repo policy (min mutation score, protected paths) enforced on every run and in CI |
| **Audit / risk** | "Show me AI changes were controlled" | One command exports every AI change with its gates, decisions and anchor |

## 4. Journeys

### J1 First run: from install to a verified change in under 15 minutes
```
$ conductor init
  repo        rust (cargo) · test runner: cargo test · mutation: cargo-mutants ✓
  agents      claude 2.x (headless ✓, hooks ✓) · codex (headless ✓, hooks ✓)
  auth        claude: seat licence · codex: API key
  wrote       .conductor/policy.yaml   .conductor/workflows/build.yaml
  next        conductor run -m "add a --json flag to `status`"
```
`init` writes defaults a user can commit unchanged. `doctor` is the same check without writing.

### J2 Engineer runs a ticket
```
$ conductor run --spec tickets/PTT-1234.md
  run 01JBXQ7  build  base a1b2c3
  ✓ spec        claude   1m12s   interface: 3 symbols
  ✓ tests       codex    2m40s   7 new tests, all failing on assertions
  ✗ implement   claude   4m03s   attempt 1 (in-context)
      mutation 0.52 < 0.70  — 11 of 23 mutants survived in src/status.rs
  ↻ implement   claude   3m10s   attempt 2 (fresh) — given the survivors
  ✓ implement             mutation 0.83 · scope ok · tests 7/7 · reruns 2/2
  → PR #412 opened · usage 212k tokens (measured) · 11m05s
```
Inputs accepted: `--spec file`, `-m "text"`, `--issue 123` (GitHub issue), later Linear and Jira.

### J3 Run halts. The human decides
```
  ✗ implement   halted after 3 attempts
      gate  scope   src/../tests/generated_tests.rs modified (frozen)
  options
    conductor resume 01JBXQ7 --guide "don't touch tests; fix parse_flags"
    conductor accept 01JBXQ7 --reason "..."   (recorded; PR marks gate overridden)
    conductor abandon 01JBXQ7
  report  .conductor/runs/01JBXQ7/report.md
```

### J4 Reviewer reads the PR
The PR body is generated. It is the most important screen in the product:

```
## Verified by conductor · run 01JBXQ7
| Gate                 | Result | Detail                                   |
|----------------------|--------|------------------------------------------|
| Tests independent    | ✓      | written by codex (stage `tests`), frozen |
| Red before green     | ✓      | 7/7 failed on assertions before impl     |
| Tests pass           | ✓      | 7/7, 2 reruns, no flakes                 |
| Mutation (diff only) | 0.83   | 19/23 caught                             |
| Scope                | ✓      | changed src/** only                      |
| Protected paths      | ✓      | CI, policy, migrations untouched         |

### Look here — surviving mutants
- src/status.rs:88  `>` → `>=`   (boundary not tested)
- src/status.rs:102 removed call to `flush()`

### Not checked
- behaviour on non-UTF-8 paths (no test mentions it)
- agent had read access to the whole machine (no sandbox)

Record: chain 9f3a… anchored in 4e1c0d2 · `conductor verify 01JBXQ7`
```

### J5 Tech lead sets the bar
```yaml
# .conductor/policy.yaml — read from the base commit, never the agent's worktree
min_mutation_score: 0.7
protected: [".github/**", ".conductor/**", "migrations/**", "Cargo.lock"]
required_gates: [independent_tests, red_first, tests, mutation, scope]
agents: { allowed: [claude, codex], tests_and_impl_must_differ: prefer }
reruns: 2
```
**`conductor check-pr`** runs in CI as a required status. It fails a PR that claims conductor
provenance when:
- the record is missing or invalid,
- the PR diff differs from the diff that was verified (someone pushed after verification), or
- the policy wasn't met.

Without this check, the PR body is decoration.

### J6 Audit
```
$ conductor audit --since 2026-07-01 --format csv
  142 AI-authored PRs · 139 all gates · 3 overridden (reasons attached) · 0 unanchored
```

## 5. Feature map

Priority: **M** must (the product fails without it), **S** should, **C** could.

### Intake
| Feature | P | Release |
|---|---|---|
| `--spec file`, `-m text` | M | 0.1 |
| GitHub issue intake | S | 0.2 |
| Linear / Jira intake | C | 0.3 |

### Workflow
| Feature | P | Release |
|---|---|---|
| Built-in `build` workflow: spec → tests → implement | M | 0.1 |
| Custom workflow YAML (read from base SHA) | M | 0.1 |
| `fix` workflow: reproduce bug as failing test → fix | S | 0.2 |
| `refactor` workflow: behaviour unchanged, mutation score must not drop | S | 0.2 |
| Parallel stages with disjoint scopes | C | 0.3 |
| `ask` planner stage → YAML → approval | C | 0.3 |

### Execution
| Feature | P | Release |
|---|---|---|
| Headless executor: claude, codex | M | 0.1 |
| Worktree per stage, fresh context per stage | M | 0.1 |
| **Hook ledger**: record every tool call via Claude/Codex hooks | S | 0.2 |
| **Hook guard**: `PreToolUse` denies writes to frozen or protected paths *as they happen* | S | 0.2 |
| More kinds (copilot, gemini, amp) as their headless modes allow | C | 0.3 |
| OS sandbox (Landlock / sandbox-exec) | C | 0.3 |

Hook guard turns "detected after" into "prevented" for the two main agents, at no
sandbox cost. The scope gate stays as the backstop for agents without hooks.

### Gates
| Feature | P | Release |
|---|---|---|
| Scope, frozen and protected paths | M | 0.1 |
| Red-for-the-right-reason | M | 0.1 |
| Tests pass with `reruns`, flaky verdict | M | 0.1 |
| Mutation score, diff only (cargo-mutants) | M | 0.1 |
| Stale / mutated-worktree / destructive-command / sensitive-new-file | S | 0.1 (reused from dock) |
| `junit_xml` parser: pytest, jest, go, JUnit | S | 0.2 |
| Mutation for other languages (Stryker, mutmut, PIT) | S | 0.2–0.3 |
| Lint / typecheck / coverage delta | S | 0.2 |
| Advisory LLM judge | C | 0.3 |

### Retry and control
| Feature | P | Release |
|---|---|---|
| Retry ladder: in-context → fresh, failure fed back | M | 0.1 |
| Surviving mutants fed back as retry guidance | M | 0.1 |
| `resume --guide`, `accept --reason`, `abandon` | M | 0.1 |
| Budgets (wall clock, prompts, mutation time) | M | 0.1 |
| Cross-kind retry | S | 0.2 |
| Human approval stage (e.g. approve spec before tests) | S | 0.2 |

### Output and evidence
| Feature | P | Release |
|---|---|---|
| Live progress (plain TTY, `--output jsonl`) | M | 0.1 |
| Local `report.md` on every run | M | 0.1 |
| Manifest + hash chain anchored in commit trailer | M | 0.1 |
| `verify` from stored evidence | M | 0.1 |
| PR delivery with the J4 body | S | 0.2 |
| `check-pr` CI status + GitHub Action | S | 0.2 |
| Usage ledger by run / stage / kind | S | 0.2 |
| `audit` export | C | 0.3 |
| `recall`, `promote` | C | 0.3 |

### Won't do (for now)
Hosted service or dashboard · a terminal multiplexer or pane UI (dock is discarded) ·
investigate mode (separate track, see ASSESSMENT) · auto-merge · IDE plugin.

## 6. Why a run costs what it costs

A single `build` run costs three agent stages plus mutation, so it is slower and more
expensive than a plain agent. The product only works if that trade is explicit:
- `run` prints wall clock and measured tokens every time.
- `--fast` skips mutation and marks it **not checked** in the PR. It never hides the skip.
- The pilot target is ≤ 2× plain-agent wall clock (ASSESSMENT §6).

## 7. What we take from dock

Dock as a product is discarded. These parts carry over as a library, not as features:

| From dock | Becomes |
|---|---|
| `dock-receipt::runner`: process-group runner, drained pipes, hard timeouts, env allowlist, SHA pinned before and after | Gate runner core |
| `dock-receipt::declaration`: checks read from repo root, never the agent's worktree | Gate and policy loading from base SHA |
| `dock-receipt::rules` + `verdict explain` | Verdict engine with "explain every rule and the fact it read" |
| `dock-git`: worktree add, diffstat, facts | Stage worktrees, scope gate |
| `dock-daemon::hook` + `hooks --install` merge logic | Hook ledger and hook guard |
| `dock-detect` roster by binary | `doctor` / `init` agent detection |
| `dock-model::env` allowlist | Stage environment construction |
| `dock-testing` timeout scaling | Test utilities |

Dropped: PTY runtime, VT emulator, daemon/socket protocol, TUI dashboard, kanban board,
queue, programme, layout persistence, copy mode. That is roughly 80% of the dock codebase.

## 8. Questions for you

These change what gets built first:

1. **First user:** you and your team, or an external audience? The examples (PTT-1234,
   rulesengine-kafka) suggest a finance team. If so, the audit journey (J6) moves up.
2. **Languages:** is Rust your real target, or should v0.1 use `junit_xml` so it works on
   your main codebase from day one?
3. **Agents:** which seat licences and API keys do you have? Claude plus Codex is assumed.
4. **Delivery:** GitHub only, or GitLab/Bitbucket too?
5. **Distribution:** open source CLI, internal tool, or commercial? This decides whether
   `check-pr` and `audit` are free or paid.
6. **Name:** keep "conductor" (collides with existing tools) or choose another before any
   code exists?
7. **Repo:** build in this repository (clear out dock, keep the reused crates) or a fresh
   repository that copies them?
