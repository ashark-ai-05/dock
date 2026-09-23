# conductor — product spec

Version 0.2.0-draft · status: revised after product review (see `ASSESSMENT.md`), not yet implemented

Changes from 0.1.0-dev are marked **[changed]** or **[new]**. Reasons are in `ASSESSMENT.md` §4.

---

## 1. What it is

A command-line tool that runs AI coding agents as verifiable work units. You give it a
unit of work; it runs agents in isolated contexts, checks the result with deterministic
gates the agent cannot edit, and records what happened in a form someone else can audit.

## 2. Problem

Agent output is cheap to produce and expensive to trust. The time saved generating code is
spent reviewing it. Most agent tools optimise how fast code is generated or how many agents
run in parallel. Few make the output checkable by someone who did not watch it being made.

Two failure modes matter:

- **Self-certified code.** Code that compiles and passes tests written by the same agent,
  in the same context, that wrote the code. The tests prove nothing.
- **Unverifiable reasoning.** An investigation that produces a confident, well-written root
  cause nobody can check. Wrong causation survives review because it reads well.

## 3. Who it is for **[new]**

| Persona | Situation | What they need from conductor |
|---|---|---|
| **Reviewer / tech lead** (primary) | Reviews agent-authored PRs; review is the team's bottleneck | A PR that says what was proven, by what, and what was *not* checked |
| **Engineer running agents** | Hands tickets to Claude Code / Codex / Copilot | One command that runs author → tester → implementer with separation of duties, and stops on failure instead of reporting "done" |
| **Audit / risk owner** (buyer in regulated orgs) | Must show how AI-generated changes were controlled | A record readable without this tool, that cannot be silently rewritten once pushed |

Not for: interactive pair-programming with one agent. Plain Claude Code or Codex is better
there, and conductor adds nothing.

## 4. Success metrics **[new]**

Measured on the pilot corpus (ASSESSMENT.md §6) against the same tickets run through a
plain agent.

| Metric | Definition | v0.1 target |
|---|---|---|
| Catch rate | Runs where the agent reported done but a gate failed ÷ all runs | ≥ 15%, otherwise gates only repeat what the agent already checked |
| Test load-bearing | Mutation score of accepted tests on changed code | ≥ 20 points above plain-agent baseline |
| Review time | Median reviewer minutes per merged PR | −30% vs baseline |
| Escaped defects | Reverts or fix commits touching the change within 30 days | Not worse than baseline |
| Verdict reproducibility | `verify` reproduces the recorded verdict | 100% (invariant, not a target) |

## 5. Modes **[changed]**

| Mode        | Input        | Output                 | Primary gate                          | Status |
|-------------|--------------|------------------------|---------------------------------------|--------|
| build       | ticket, spec | code diff, PR          | frozen tests + mutation score + scope | v0.1 |
| investigate | symptom, alert | root cause, RCA      | citation re-execution + falsifier     | separate track, gated on discovery (§16) |

`analyse` and `adhoc` are cut. Neither had a defined gate that could fail. Modes share the
engine, evidence model and cost ledger, and differ only in gates and delivery.

---

## 6. Determinism

| Claim                                   | Achievable | Notes |
|-----------------------------------------|------------|-------|
| same input → same generation            | No  | LLMs. Never claimed. |
| same recorded evidence → same verdict   | Yes | The verdict is a pure function over **recorded** gate outputs. |
| same gate re-executed → same result     | No **[new]** | Gates run code: tests flake, live data changes. Handled by §9.4, not assumed away. |
| same run → same recorded history        | Yes | Hash-chained, append-only, anchored externally (§11.2). |

### Where LLM calls occur

| Layer | LLM calls | Deterministic |
|---|---|---|
| DAG resolution, scheduling, retry ladder | 0 | yes |
| Budget, scope and capability checks | 0 | yes |
| Manifest, hash chain, git notes | 0 | yes |
| Deterministic gates (test, lint, mutants, hash, scope) | 0 | logic yes; execution may flake (§9.4) |
| Citation gate (promql, logql, kubectl) | 0 | logic yes, data live |
| Claim and assertion parsing | 0 | schema plus a small assertion grammar (§13.3), never inference |
| Acknowledge check | 0 | set equality over a structured `ack.yaml` (§10) |
| Agent stages, **including the `ask` planner** **[changed]** | 1 prompt each | no |
| LLM judge gate (opt-in) | 1 | no, advisory only, cannot fail a run |

Hard rules:

1. **No LLM calls in the control plane.** The thing that decides never guesses. Routing,
   retries, admission and budget halts are rule-based.
2. **Anything an agent emits is parsed against a schema, never interpreted.** A schema
   failure triggers a deterministic retry.
3. **[new] Natural language never reaches the engine directly.** `conductor ask "…"` runs a
   *planner agent stage* whose output is a workflow YAML. The engine validates it, a human
   approves it (or policy auto-approves listed templates), and only then does it run. This
   removes the contradiction between "no config, one sentence" and rule 1.

---

## 7. Trust model **[new]**

| Party | Trusted for | Not trusted for |
|---|---|---|
| Engine | Scheduling, gate execution, evidence writing | — |
| Repository at base SHA | Gate declarations, workflow, protected paths | — |
| Agent | Nothing it says about itself | Claims of completion, test adequacy, or which files it read |
| Human | Decisions, recorded with rationale | — |

What the engine **guarantees**:

- Gate declarations and workflow files are read from the **base commit**, never from the
  agent's worktree. The agent can edit any file it was given, including those.
- A stage's changed files are compared with its declared write scope after it finishes
  (§9.2). Out-of-scope writes fail the stage, whatever the agent intended.
- Gates run in the engine's process, never in an agent pane.
- Once the chain head is pushed, the record cannot be rewritten without rewriting pushed
  history (§11.2).

What it does **not** guarantee (stated in every PR body):

- It does not sandbox the agent. The agent runs with the user's permissions and can read
  anything they can, secrets included. OS sandboxing is optional (v0.3).
- Gates run the code the agent wrote. A gate is only as strong as the tests and mutants
  behind it.
- `verify` proves the verdict follows from the recorded evidence. It does not prove the
  evidence reflects reality.

The agent is modelled as **untrusted but not hostile**. It will game any gate it can
(Goodhart), for example by weakening a test or widening a mock, but it is not assumed to
attack the host. Defences target gaming.

---

## 8. Execution tiers **[changed]**

Agents are driven through an `Executor` interface. There is no native agent SDK.

| Tier | Mechanism | Turn boundaries | Usage | Auth | Ships |
|---|---|---|---|---|---|
| B | Agent headless mode, JSON out (`claude -p --output-format stream-json`, `codex exec --json`, …) | real | measured | seat or API key | **v0.1** |
| A | Terminal pane via **dock** (reference) or herdr (adapter) | inferred | unavailable | seat licence | v0.2 |
| C | Provider API direct | real | measured | API key | judge gate only |

- **Tier B is the default and ships first.** It has real turn boundaries, measured usage,
  runs in CI and needs no screen inference. Tier A is for a human who wants to watch or
  intervene. It is a presentation choice, not a correctness dependency.
- **Every engine invariant must hold under tier B alone.** No gate, confirm step or retry
  may depend on pane state.
- `conductor doctor` probes each installed agent kind and reports its tiers, auth mode, and
  whether a usage log exists.
- **[new] Licence check.** Driving a seat-licensed agent from automation, and especially
  from CI, may be restricted by the vendor's terms. `doctor` reports the auth mode it
  detected. Documentation names the terms per vendor. CI runs default to API-key auth.

---

## 9. Capabilities

### 9.1 Execution
- Each stage runs in its own OS process and git worktree. "Isolated" means isolated
  **context** (conversation, working tree). It does not mean a filesystem or network sandbox (§7).
- Declarative workflows, or `ask` via the planner stage (§6 rule 3).
- **[changed]** Parallel fan-out only across distinct worktrees, and only when the stages'
  declared write scopes don't intersect (glob intersection, deterministic). This replaces
  the undefined "briefing overlap threshold".

### 9.2 Separation of duties **[new]**, the core of build mode
- The test author and the implementer are **different stages in fresh contexts**. The
  implementer never sees the test author's conversation, only its committed outputs.
- Paths produced by an earlier stage can be marked `frozen`. A later stage that modifies
  a frozen path fails the **scope gate**, which runs `git diff --name-only` against the
  declared `write` globs.
- The red gate needs a failure **for the right reason**: tests must compile and must
  fail with assertion failures, not panics or compile errors, and each new test must
  reference a symbol named in the spec's interface section. `assert!(false)` fails this.
- Mutation testing (§9.3) is what finally shows the tests are load-bearing. The red
  gate is only a cheap filter.

### 9.3 Verification
- Deterministic gates: compile, test, lint, exit code, file hash, schema, **scope**.
- Test output parsers: `cargo_json` (v0.1) and **`junit_xml` [new]** (v0.2). JUnit XML covers
  pytest, jest, go test, JUnit and most other runners. Without it, the product only
  serves Rust users.
- Differential gates: **mutation score on changed lines only** (`cargo-mutants --in-diff`
  in v0.1), coverage delta, benchmark regression. Full-crate mutation takes hours and
  would blow every wall-clock budget.
- Citation gates (investigate): re-run every cited query, evaluate its assertion with the
  grammar in §13.3, **and** evaluate its falsifier.
- Adversarial review: a challenger agent in a fresh context, tasked with disproof. Its
  output is a list of claims, graded like any other. It cannot pass or fail a run by assertion.
- Retry ladder: in-context → fresh → cross-kind. Each retry receives the gate failure
  as input. Cross-kind is skipped, and recorded as skipped, when no second kind is installed.

### 9.4 Flaky gates **[new]**
- A gate may declare `reruns: n`. Pass means all runs pass. Fail means all runs fail.
  Mixed results produce a **`flaky`** verdict, which does not pass and is reported
  separately so nobody reads it as the agent's fault.
- The engine never retries the *agent* because of a flaky gate. The retry ladder applies
  only to deterministic failures.

### 9.5 Evidence
- Hash-chained, append-only record: every command's argv, exit code, stdout/stderr hash and
  state transition.
- Git-native for code: one commit per stage, metadata in `git notes --ref=conductor`,
  chain head in a commit trailer (§11.2). Readable without this tool.
- Materialised evidence for investigate: query results are stored, not just the queries,
  because observability data expires. Subject to §9.8.
- `evidence_completeness` declares gaps. **[changed]** Anything the engine cannot observe is
  labelled `unavailable`, never inferred. In tier A that includes tool calls, files read and
  usage.

### 9.6 Cost
- Usage comes from each agent's own session log or JSON stream and is labelled `measured`.
  With no source it is `unavailable`, never estimated from terminal text.
- Admission-time limits: wall clock, prompt count, stage count, **mutation wall clock
  [new]**. Dollar caps are advisory, because spent tokens cannot be refunded.

### 9.7 Control
- **[changed]** Capabilities per stage (`write`, `frozen`, `deny`) are **verified after the
  stage** by the scope gate, and attested in the manifest. Prevention (OS sandbox) is an
  optional v0.3 layer. The attestation reports only what was checked. Tool-call counts
  appear only when tier B supplies them.
- Human stages are first-class actors with recorded decisions and rationale.
- Live intervention (tier A, v0.2): pause, inject guidance, rerun fresh, accept with reason.
  Every intervention is a chain event.
- Resumable: state is persisted after every transition.

### 9.8 Data handling **[new]**
- All run data stays local. Delivery (PR, ticket comment) is the only outbound write, and
  it contains hashes and summaries, never materialised evidence blobs.
- Evidence blobs are redacted by configured patterns before hashing. Redaction is recorded,
  so a redacted blob is still verifiable against its recorded hash.
- Retention defaults to 90 days for blobs. Manifests and chain heads are kept indefinitely.

### 9.9 Reuse
- `promote` turns a successful ad-hoc run into a reusable workflow file.
- `recall` searches past runs by symptom with lexical search over manifests and claims. No
  embeddings in v0.x.
- `verify` re-evaluates a past run's verdict from stored evidence, with zero model calls and
  zero live queries, and must reproduce it. `verify --rerun` re-executes the gates and
  reports drift separately. Drift is information, not a failure of `verify`.

---

## 10. Process flow **[changed]**

Per stage:

```
  1  BRIEF        objective, input files (content-hashed as evidence of what was
                  provided — not of what was read), write scope, frozen paths,
                  definition of done, declared outputs
                        |
  2  ACKNOWLEDGE  agent writes ack.yaml: output ids + paths it will produce,
                  paths it will modify (globs). Engine checks: outputs set-equal to
                  brief; modify globs ⊆ write scope. Deterministic.
                  mismatch --> reissue brief once, then halt
                        |
  3  DISPATCH     tier B: spawn headless process
                  tier A: split pane, start agent, prompt
                          precondition: agent state idle|done
                        |
  4  OBSERVE      tier B: process exit + JSON stream
                  tier A: wait for settled state
                  blocked --> surface question, await human
                  unknown --> INDETERMINATE, escalate, never success
                        |
  5  CONFIRM      declared outputs exist, content hash changed from brief
                        |
  6  GATE         scope gate first (cheapest, catches gaming),
                  then declared gates, in the engine's process
                        |
                  pass   --> 7
                  flaky  --> halt, record, escalate (no agent retry)
                  fail   --> retry ladder
                               1. in-context
                               2. fresh
                               3. cross-kind (if installed)
                             exhausted --> halt, record, escalate
                        |
  7  RECORD       commit, git note, evidence blobs, usage, chain entry
                        |
  8  NEXT         scheduler picks next ready stage
```

Run close:

```
  all gates passed --> deliver
                       build: PR whose body has the gate table,
                              mutation score, frozen paths, and the
                              "not checked" list from §7
                   --> write manifest; put chain head in the final
                       commit's trailer
  otherwise        --> no PR; manifest + a local report with the failing gate
```

---

## 11. Executors

### 11.1 Pane executor contract **[changed]**

Formerly "Herdr integration constraints". These invariants apply to **any** pane executor
(dock or herdr), because both infer agent state from the terminal.

| Invariant | Why |
|---|---|
| Never wait for a literal `done`; accept `idle \| done` | "done" can mean "idle but the tab was never viewed" |
| Never focus a pane the engine is waiting on; record a human focus as an event | Focusing marks the tab seen, which changes the reported state |
| `unknown` is INDETERMINATE, never success | Screen inference can fail silently |
| Prompt only in `idle \| done`; confirm through outputs, not status | The wait tracks agent lifecycle, not the turn you sent |
| `blocked` is normal control flow | The agent is asking a question |
| Stall (no state change after N s): re-read the pane, then retry once | Prompts can be dropped |
| Identity is `stage_id + agent name`; pane id is a timestamped observation | Pane ids change when panes move |
| Never gate on pane output matching; gates run in-process | Output matching can hit text that was already there |
| Transcripts are corroborating only | Alt-screen rows never reach scrollback |
| Mutations go through the executor's CLI; any socket is read-only | CLIs are the documented surface |

**dock** is the reference pane executor. `dock split`, `dock prompt`, `dock read` and
`dock wait --until=` already exist, the project owns it, and its protocol can be pinned
(`requires.dock_protocol`). The herdr adapter keeps the verified herdr-0.8.2 specifics
(timeout bounds 3000 < t ≤ 300000 ms, `agent_not_ready` keeps the name usable, etc.)
inside the adapter, not in the engine.

### 11.2 Record integrity **[new]**
- The chain head is written to a `Conductor-Chain:` trailer on the run's final commit. Once
  pushed, rewriting the record means rewriting pushed history, which the remote and every
  clone would notice.
- Commits are signed with the user's existing git signing config (SSH or GPG) when present.
  Conductor manages no keys of its own. The manifest records whether commits were signed.

---

## 12. Architecture

```
  conductor run | ask | promote | verify | recall | doctor
                          |
            ENGINE  (deterministic, zero LLM calls)
     workflow loader (base SHA) --> scheduler --> stage loop --> gate runner
                                        |                          |
                                   budget, scope              evidence writer
                                   & admission
                          |                     |
                     EXECUTORS              GATE RUNNERS
              headless (tier B, default)  process  (test, lint, mutants)
              pane     (tier A: dock|herdr) scope    (diff vs declared globs)
              api      (judge only)        query    (promql, logql, kubectl)
                                           schema   (ack.yaml, claims.yaml)
                          |                     |
                              STORES
              worktrees  artifacts  evidence blobs  git notes  run index  state.json
```

---

## 13. Data contracts

### 13.1 Workflow (`*.yaml`, read from base SHA)

```yaml
id: feature-with-independent-tests
version: 1
mode: build
requires:
  conductor: ">=0.1"
  executor: headless          # or: pane (dock_protocol: 18) in v0.2

runtime:
  artifact_root: ".conductor/{{run_id}}"
  max_parallel: 1
  approvals: manual

budget:
  advisory_max_cost_usd: 5.00
  max_stage_wall_clock_sec: 900
  max_mutation_wall_clock_sec: 600
  max_prompts_per_stage: 6
  max_total_wall_clock_sec: 3600
  on_exceeded: halt_before_next_stage

defaults:
  retries: { max: 2, ladder: [in_context, fresh, cross_kind] }
  on_failure: abort
  on_blocked: escalate

stages:
  - id: spec
    agent: { kind: claude }
    prompt_file: prompts/spec.md
    inputs: { ticket: PTT-1234 }
    scope: { write: ["{{artifact_root}}/spec.md"] }
    outputs:
      - { id: spec, path: "{{artifact_root}}/spec.md", required: true }
    gate: { type: schema, ref: spec, schema: schemas/spec.json }   # interface section required

  - id: tests
    depends_on: [spec]
    agent: { kind: codex }            # different kind from implementer, when available
    prompt_file: prompts/tdd.md
    inputs: { spec: "{{stages.spec.outputs.spec.path}}" }
    scope: { write: ["tests/**"] }
    outputs:
      - { id: tests, path: "tests/generated_tests.rs", required: true }
    gate:
      type: command_assert
      command: ["cargo", "test", "--no-fail-fast", "--message-format", "json"]
      parser: cargo_json
      assert:
        - "compiled == true"
        - "tests_run > 0"
        - "tests_failed == tests_new"          # every new test fails
        - "failures.kind all == 'assertion'"   # not panics / compile errors
        - "tests_new reference spec.interface" # each touches a spec'd symbol

  - id: implement
    depends_on: [tests]
    agent: { kind: claude }
    context: fresh                    # never sees the tests stage's conversation
    prompt_file: prompts/implement.md
    scope:
      write: ["src/**"]
      frozen: ["{{stages.tests.outputs.tests.path}}"]
      deny: ["git.push"]
    gates:
      - { type: scope }
      - type: command_assert
        command: ["cargo", "test", "--message-format", "json"]
        parser: cargo_json
        reruns: 2
        assert: ["tests_failed == 0"]
      - { type: mutation, tool: cargo_mutants, in_diff: true, min_score: 0.7 }
    on_failure: { action: retry, max: 2, feedback: gate_output }

teardown:
  - { id: close_executors, always: true }
```

### 13.2 Manifest (`manifest.json`) **[changed]**

As in 0.1.0-dev, with:
- `environment.executor` replacing the herdr fields: `{ "kind": "headless" | "dock" | "herdr", "version", "protocol" }`.
- `stages[].gates[]` as an array, each with `verdict: pass | fail | flaky` and `runs[]`.
- `stages[].scope`: `{ "declared": {...}, "changed_files": [...], "violations": [...] }`.
- `stages[].mutation`: `{ "tool", "in_diff": true, "caught", "missed", "timeout", "unviable", "score" }`.
- `capability_attestation` limited to checked facts. Tool-call counts appear only with a
  tier B source, otherwise `"tool_calls": "unavailable"`.
- `integrity`: `{ "chain_head", "anchored_in_commit", "commits_signed": true|false }`.
- `not_checked`: the list printed in the PR body (§7).

### 13.3 Claims and assertion grammar (investigate) **[changed]**

```yaml
claims:
  - id: c4
    statement: "Consumer lag on rulesengine-kafka drove p99 from 40ms to 3.2s"
    type: causal
    evidence:
      - id: lag
        kind: promql
        datasource: prometheus-prod
        expr: 'max(kafka_consumergroup_lag{group="ptt-rulesengine"})'
        window: { from: "2026-09-23T04:00Z", to: "2026-09-23T04:30Z" }
        assert: { op: ">", value: 480000, sustained: "11m" }
      - id: p99
        kind: promql
        datasource: prometheus-prod
        expr: 'histogram_quantile(0.99, rate(http_request_duration_seconds_bucket{svc="rules"}[1m]))'
        window: { from: "2026-09-23T04:00Z", to: "2026-09-23T04:30Z" }
        assert: { op: ">", value: 1.0, sustained: "5m" }
    ordering: { before: lag, after: p99, min_lead: "30s" }   # temporal precedence
    falsifier:
      any:
        - { evidence: lag, assert: { op: "<", value: 480000, sustained: "11m" } }
        - { ordering: { before: p99, after: lag } }
    confidence: 0.78          # agent-reported, displayed, never used by the engine
```

Assertions are a closed grammar (`op`, `value`, `sustained`, `ordering`), evaluated by the
engine. Free-text assertions are rejected by the schema.

Grades are assigned by the engine:

| Grade | Meaning |
|---|---|
| `verified` | every evidence assertion held, ordering held, falsifier did not fire |
| `correlated` **[new]** | evidence held, but the claim is `causal` and has no ordering or falsifier; shown, not used as the conclusion |
| `refuted` **[new]** | the falsifier fired |
| `unverified` | re-execution disagreed, or the data expired |
| `asserted` | no citation; kept as a lead, never in the conclusion |

A `causal` claim can reach `verified` only with temporal ordering and a falsifier that did
not fire. That is the most a query can establish. The RCA says so.

---

## 14. Out of scope

- Reproducible generation.
- A native agent SDK.
- Token usage for agents that expose no usage log.
- Replacing review. Conductor narrows what review must cover and states what it did not check.
- **[new]** Sandboxing the host from the agent (optional v0.3 layer, not a guarantee).
- **[new]** Proving causation. Investigate mode grades causal claims only up to temporal
  precedence plus a falsifier that did not fire.
- **[new]** Hosted service, telemetry, credential storage.

## 15. Open risks **[new]**

| Risk | Mitigation or test |
|---|---|
| Frontier agents already run tests before saying done, which erodes catch rate | Measure catch rate in the pilot. Kill criterion in ASSESSMENT §6 |
| Vendor terms restrict automated seat-licence use | `doctor` reports auth mode; CI defaults to API keys; terms documented per vendor |
| Mutation testing too slow on real repos | In-diff only, own wall-clock budget, timeout recorded as `unavailable`, not pass |
| Agent CLIs change headless flags or JSON shapes | Executor adapters pinned per version; `doctor` detects drift |
| Name collision: other agent tools are already called "Conductor" | Resolve before public release (ASSESSMENT §5) |

## 16. Build order **[changed]**

**v0.1 — "independent tests, proven"** (build mode, Rust, tier B only)
headless executor for claude and codex · workflow read from base SHA · sequential stages,
one worktree per stage · ack check · scope and frozen-path gate · `command_assert` with
`cargo_json` and the red-for-the-right-reason asserts · in-diff mutation gate · flaky
verdict · retry ladder (in-context, fresh) · hash-chained manifest anchored in a commit
trailer · usage provenance · `verify` (stored evidence only) · `--output jsonl` · local
report on failure.

**v0.2 — "usable on a team"**
`junit_xml` parser (non-Rust repos) · PR delivery with gate table and not-checked list ·
pane executor on dock (watch plus intervention) · human stages · resume · cross-kind retry ·
`doctor` · git-notes evidence · `verify --rerun`.

**v0.3 — "scale"**
parallel stages with disjoint scopes · `ask` planner stage · `promote` · `recall` · herdr
adapter · optional OS sandbox · advisory judge gate.

**Investigate track**: starts only after ≥5 SRE discovery interviews confirm that
citation-graded RCAs would change a postmortem decision. Then: promql and logql
connectors, claim ledger, assertion grammar, challenger stage, data handling (§9.8).
