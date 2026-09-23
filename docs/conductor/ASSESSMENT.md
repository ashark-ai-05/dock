# conductor — product assessment

A product-owner review of spec 0.1.0-dev. It covers how useful the tool is, who would
pay for it, what was wrong in the draft, and how to find out quickly whether it is worth
building. The revised spec is `SPEC.md` (0.2.0-draft).

---

## 1. Verdict

**Useful for a narrow audience, and only if the verification part leads.** Running
several agents in panes and worktrees is crowded and getting cheaper. The part nobody
does well is **separation of duties plus proof that the tests carry weight** (independent
test author, frozen tests, mutation score on the diff), recorded in a form an auditor can
read. That is the product. Everything else is supporting infrastructure.

| Question | Answer |
|---|---|
| Does the problem exist? | Yes. Review is the bottleneck for teams using agents, and agent-written tests are often shallow. |
| Is it solved already? | Partly. CI runs tests, and agents run tests before saying done. Few tools check *who* wrote the tests, whether they were changed afterwards, or whether they would catch a mutant. |
| Who feels it most? | (1) Tech leads reviewing a high volume of agent PRs. (2) Regulated teams (finance, health) who must show how AI changes were controlled. |
| Who doesn't need it? | A solo developer pairing with one agent. For them it is overhead. |
| Biggest threat | Agents are getting better at checking their own work, which erodes the catch rate. The pilot must measure this before the build goes further. |

## 2. Where the value actually is

Ranked by how different it is from what already exists, and how much a reviewer cares:

1. **Frozen, independently written tests with a scope gate.** The implementer cannot
   touch the tests. This is cheap to build (`git diff --name-only` plus globs), it is
   deterministic, and it directly addresses failure mode #1. The draft left it out.
2. **Mutation score on changed lines.** It answers "are these tests load-bearing?"
   with a number. Nobody else in this space puts it in the PR.
3. **A "not checked" list in the PR.** It turns review from "read everything" into
   "read what wasn't proven". This is the review-time win.
4. **A tamper-evident record anchored in git.** It sells to audit and risk owners. For
   everyone else it is a nice-to-have.
5. **Retry ladder.** Useful, but agents already loop on test failures. It is expected,
   not a reason to switch.
6. **Terminal panes and live intervention.** Good for demos and trust-building. Others
   already do it (dock, herdr, and several worktree-per-agent managers). It is not a
   reason to buy.

**Troubleshooting** (`troubleshoot`, formerly "investigate") could be *more* valuable than development. Nobody verifies RCAs, and
wrong causation is expensive. But it needs connectors, access to production data, and a
different buyer (SRE). Build mode can be proven locally with repos you already have.
It is planned for v0.3, after development and ad-hoc questions have proven the engine.

## 3. Positioning

> Conductor makes agent-written code reviewable: independent tests the implementer can't
> touch, a mutation score that proves they carry weight, and a record your auditor can
> read without installing anything.

Not: "run many agents in parallel". Not: "deterministic AI". The draft's
determinism section is correct, but buyers hear it as a claim about the AI. Lead with
separation of duties.

## 4. Gaps in 0.1.0-dev and how 0.2.0-draft fixes them

| # | Gap in the draft | Why it matters | Fix (SPEC section) |
|---|---|---|---|
| 1 | No target user, no success metric | Nobody can say whether v0.1 worked | Personas §3, metrics §4 |
| 2 | v0.1 relied on herdr's inferred state (no real turn boundaries, no usage data) | Receipts built on a guess | herdr kept for panes; turns and usage come from agent hooks and session logs; headless for CI §8 |
| 3 | Red gate `tests_failed > 0` is passed by `assert!(false)` | Repeats the failure mode the product exists to prevent | Red for the right reason, plus mutation in v0.1 §9.2 |
| 4 | Nothing stops the implementer from editing the tests | The biggest gaming route, left open | `frozen` paths plus scope gate §9.2 |
| 5 | Gate declarations could live in the agent's worktree | The agent can rewrite its own gates | Workflow and gates read from base SHA §7 |
| 6 | ACKNOWLEDGE "diffs" a natural-language restatement | Either useless (exact echo) or needs an LLM, which breaks rule 1 | Structured `ack.yaml`, set equality §10 |
| 7 | "Single natural-language command, no config" versus "no LLM in control plane" | Contradiction | `ask` is a planner *agent stage* whose YAML is validated and approved §6 |
| 8 | "Briefing overlap threshold" undefined | Can't be implemented or tested | Disjoint write-scope globs §9.1 |
| 9 | Citation gate checks facts, while the claim type is `causal` | Wrong causation survives, which is failure mode #2 | Ordering plus a falsifier that is evaluated; new grades `correlated` and `refuted` §13.3 |
| 10 | `asserted: "max > 480000 sustained 11m"` is free text | Parsing it means inference, which breaks rule 2 | Closed assertion grammar §13.3 |
| 11 | Capability "enforcement by environment construction" and attested tool-call counts in tier A | Claims more than can be enforced or observed | Verify after the stage, attest only what was checked, `unavailable` otherwise §9.7 |
| 12 | Hash chain without an external anchor | Whoever holds the file can rewrite it all | Chain head in a commit trailer; the user's signing config §11.3 |
| 13 | "Same evidence → same verdict" silent on flaky gates | Flakes turn into agent retries and wasted spend | `reruns`, `flaky` verdict, no agent retry §9.4 |
| 14 | `verify` implied more than it proves | An auditor could over-trust it | States its limit; `--rerun` reports drift separately §9.9 |
| 15 | No threat model; "no context bleed" read like a security claim | Over-promises | Trust model §7; isolation means context, not sandbox |
| 16 | Materialised production query results with no data policy | PII and compliance blocker for troubleshooting and analysis | Local-only, redaction, retention §9.8 |
| 17 | Seat-licence automation assumed fine | Vendor terms may forbid it, especially in CI | `doctor` reports auth; CI uses API keys §8 |
| 18 | Only `cargo_json` | Rust-only means a small market | `junit_xml` in v0.2 §9.3 |
| 19 | Full mutation testing on the budget clock | Hours per run, so the gate would always time out | In-diff only, own budget §9.3, §9.6 |
| 20 | Four modes; `analyse` and `adhoc` had no gate that could fail | Scope spread with no verification story | Every kind must name checks that can fail; `adhoc` receipts grade claims and never say "verified" §5 |
| 21 | Depends on herdr (external, protocol undocumented) while you own dock, which does the same job | Two tools to maintain, and the dependency is on the one you don't control | Dock discarded; herdr behind one pinned adapter, headless alongside; evidence from hooks and session logs, never herdr state §8, §11.1 |
| 22 | Must-read files "hashed" read as proof they were read | Can't be observed | Hash records what was *provided* §10 |

## 5. Name

"Conductor" is already used by other developer tools, including an agent-orchestration app
and a well-known workflow-orchestration engine. That hurts search, package names, and
possibly trademarks. The owner chose to keep "conductor"; check package-name
availability (crates.io, Homebrew) before the first release.

## 6. How to find out cheaply whether it's worth building

**Pilot:** 20 real tickets from 2–3 repositories you control, each run twice. Once with a
plain agent (Claude Code or Codex, told to write tests and implement), once through
conductor v0.1.

| Measure | Keep going if | Stop or pivot if |
|---|---|---|
| Catch rate (agent said done, a gate failed) | ≥ 15% | < 5%: gates only repeat the agent's own checks |
| Mutation score, conductor vs plain | ≥ 20 points higher | Within 5 points: separation of duties isn't changing test quality |
| Reviewer minutes per PR (blind, same reviewer) | −30% | No change: the PR body isn't helping review |
| Wall clock per ticket | ≤ 2× plain | > 4×: too slow to adopt |

Also run **5 conversations with audit or risk owners** in a regulated org: "Would this
record satisfy your AI-change control?" A yes decides the buyer. A no means build mode
is a developer tool only, and pricing follows from that.

## 7. What this means for the dock codebase

Dock as a product is discarded. Only its verification and git pieces carry over as library
code: the check runner, declarations read from the base commit, verdict rules, worktree
facts, hook parsing and install, and agent detection. The PTY, daemon, TUI, board, queue
and programme are dropped. See `PRODUCT.md` §7. Pane executors (SPEC §11.1) are therefore
a later, optional adapter, not something the project owns.
