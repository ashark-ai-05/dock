# conductor — product spec

Version 0.3.0-draft · status: design, not yet implemented

Changes from 0.1.0-dev are marked **[changed]** or **[new]**. Reasons are in `ASSESSMENT.md` §4.
0.3 records the owner's decisions: conductor wraps herdr for interactive work and also
runs headless for CI and overnight runs; every kind of agentic work is a YAML workflow;
development ships first; observability is OpenTelemetry.

---

## 1. What it is

A command-line orchestrator for agentic software work: development, QA, troubleshooting,
analysis, and ad-hoc questions. It runs agents from a YAML workflow, either in **herdr**
panes (a person can watch and step in) or **headless** (CI, overnight). Every run
produces a **receipt**: what was asked, who did what, which checks ran and which could
have failed, what was not checked, and a record that can't be silently rewritten.
Correctness checks, observability and receipts are part of the engine, not add-ons.

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
| **Operator** (anyone running the system: on-call, QA, analyst) | Asks agents ad-hoc questions, runs QA and troubleshooting workflows | An answer whose every claim is graded against evidence it can re-run, not a well-written guess |
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

## 5. Workflow kinds **[changed]**

Everything is a YAML workflow (§13.1). A *kind* is a built-in template: a default set of
stages and the checks its receipt is allowed to rely on. Custom workflows mix stages and
checks freely. The rule for every kind: **a receipt may only say "passed" about a check
that could have failed.** Where no such check exists, the receipt says "graded" or
"not checked", never "verified".

| Kind | Input | Output | Checks that can fail | What the receipt can claim | Release |
|---|---|---|---|---|---|
| `build` | ticket, spec | diff, PR | frozen tests, red-first, scope, mutation on diff, flaky | pass or fail per check | **v0.1** |
| `adhoc` | one question (`conductor ask "…"`) | answer with provenance | claims re-executed against cited commands or queries | each claim graded; the answer as a whole is never "verified" | v0.2 |
| `qa` | a build or change plus a test charter | findings, reproductions, test report | each reported bug has a reproduction that fails on the change and is re-run by the engine; each charter item has an executed test | each finding reproduced or not; charter coverage | v0.2 |
| `troubleshoot` | symptom, alert | timeline, root cause | citation re-execution, temporal ordering, falsifier (§13.3) | each claim `verified` / `correlated` / `refuted` / `unverified` / `asserted` | v0.3 |
| `analyse` | question plus dataset or query source | claims with numbers | dataset hash plus query re-execution reproduces every number | each claim reproduced or not | v0.3 |

`troubleshoot` was called `investigate` in 0.2. Kinds share the engine, receipt format,
observability and cost ledger. They differ only in stages, checks and where the output
is delivered.

---

## 6. Determinism

| Claim                                   | Achievable | Notes |
|-----------------------------------------|------------|-------|
| same input → same generation            | No  | LLMs. Never claimed. |
| same recorded evidence → same verdict   | Yes | The verdict is a pure function over **recorded** gate outputs. |
| same gate re-executed → same result     | No **[new]** | Gates run code: tests flake, live data changes. Handled by §9.4, not assumed away. |
| same run → same recorded history        | Yes | Hash-chained, append-only, anchored externally (§11.3). |

### Where LLM calls occur

| Layer | LLM calls | Deterministic |
|---|---|---|
| DAG resolution, scheduling, retry ladder | 0 | yes |
| Budget, scope and capability checks | 0 | yes |
| Manifest, hash chain, git notes | 0 | yes |
| Deterministic gates (test, lint, mutants, hash, scope) | 0 | logic yes; execution may flake (§9.4) |
| Citation gate (commands, promql, logql, kubectl) | 0 | logic yes, data live |
| Claim and assertion parsing | 0 | schema plus a small assertion grammar (§13.3), never inference |
| Acknowledge check | 0 | set equality over a structured `ack.yaml` (§10) |
| Agent stages, **including the `plan` planner** **[changed]** | 1 prompt each | no |
| LLM judge gate (opt-in) | 1 | no, advisory only, cannot fail a run |

Hard rules:

1. **No LLM calls in the control plane.** The thing that decides never guesses. Routing,
   retries, admission and budget halts are rule-based.
2. **Anything an agent emits is parsed against a schema, never interpreted.** A schema
   failure triggers a deterministic retry.
3. **[new] Natural language never reaches the engine directly.** `conductor plan "…"` runs a
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
  history (§11.3).

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

## 8. Executors and the evidence plane **[changed]**

The same workflow runs under either executor. The engine never depends on which one.

| Executor | For | Mechanism | Ships |
|---|---|---|---|
| `herdr` | interactive: watch, step in, pick up where an agent is blocked | herdr CLI for mutations (split, start, prompt); herdr socket read-only for events | **v0.1** |
| `headless` | CI, overnight, batch | `claude -p --output-format stream-json`, `codex exec --json`, … | **v0.1** |
| `api` | advisory judge gate only | provider API | v0.3 |

`--executor herdr|headless` picks one. The default is `headless` when a CI environment is
detected and `herdr` otherwise.

**Evidence never comes from herdr's agent state.** Herdr infers state from process names
and terminal output, which is good for display and scheduling, but a guess. The evidence
plane is the same under both executors:

| Fact | Source | Label in the receipt |
|---|---|---|
| Turn started or ended, tool called, file written | Agent hooks (Claude Code and Codex `PreToolUse`, `PostToolUse`, `Stop`, `UserPromptSubmit`), installed by conductor per run | `observed` |
| Tokens, model, full conversation | The agent's own session log (the hook payload gives `transcript_path`), or the headless JSON stream | `measured` |
| Checks passed or failed | The engine's own process (§9.3) | `witnessed` |
| Files changed, commits | git | `observed` |
| Agent state from herdr | herdr socket | `inferred`, used for scheduling and display only |

An agent kind with no hooks falls back to herdr's state for *when* to look, and every
completion is confirmed only by outputs (§10 step 5). Its receipt says `inferred` for turn
boundaries and `unavailable` for usage.

- `conductor doctor` checks the herdr version and protocol against the pinned range, and
  for each agent kind reports headless support, hook support, auth mode, and whether a
  session log exists.
- **Licence check.** Driving a seat-licensed agent from automation, especially CI, may be
  restricted by the vendor's terms. `doctor` reports the auth mode it detected; CI runs
  default to API-key auth.

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
- Citation gates (`adhoc`, `troubleshoot`, `analyse`): re-run every cited query, evaluate its assertion with the
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
  chain head in a commit trailer (§11.3). Readable without this tool.
- Materialised evidence for `troubleshoot` and `analyse`: query results are stored, not just the queries,
  because observability data expires. Subject to §9.8.
- `evidence_completeness` declares gaps. **[changed]** Anything the engine cannot observe is
  labelled `unavailable`, never guessed. Anything taken from herdr's state is labelled
  `inferred`.

### 9.6 Cost
- Usage comes from each agent's own session log or JSON stream and is labelled `measured`.
  With no source it is `unavailable`, never estimated from terminal text.
- Admission-time limits: wall clock, prompt count, stage count, **mutation wall clock
  [new]**. Dollar caps are advisory, because spent tokens cannot be refunded.

### 9.7 Control
- **[changed]** Capabilities per stage (`write`, `frozen`, `deny`) are **verified after the
  stage** by the scope gate, and attested in the manifest. Prevention (OS sandbox) is an
  optional v0.3 layer. The attestation reports only what was checked. Tool-call counts
  appear only when hooks or the headless stream supply them.
- Human stages are first-class actors with recorded decisions and rationale.
- Live intervention (herdr executor): pause, inject guidance, rerun fresh, accept with reason.
  Every intervention is a chain event.
- Resumable: state is persisted after every transition.

### 9.8 Data handling **[new]**
- All run data stays local. Delivery (PR, ticket comment) is the only outbound write, and
  it contains hashes and summaries, never materialised evidence blobs.
- Evidence blobs are redacted by configured patterns before hashing. Redaction is recorded,
  so a redacted blob is still verifiable against its recorded hash.
- Retention defaults to 90 days for blobs. Manifests and chain heads are kept indefinitely.

### 9.9 Observability **[new]**

One event stream feeds two sinks:

- **Receipts**: durable, hash-chained, anchored in git (§11.3). The record of what happened.
- **OpenTelemetry**: live traces, metrics and logs over OTLP. How operators watch runs
  and spot trends.

| Signal | Content |
|---|---|
| Traces | One trace per run; spans for stage → attempt → agent turn → tool call, and for every check. Uses the OpenTelemetry GenAI semantic conventions for model, token and tool attributes |
| Metrics | Runs by kind and verdict, check pass/fail/flaky rates, catch rate, retries per rung, tokens and wall clock by stage and agent kind, `inferred` vs `observed` share |
| Logs | Every chain event, with `run_id`, `stage_id` and `receipt` hash as attributes |

- **Recommendation:** export plain OTLP and don't bind to a vendor. If you have no
  backend yet, use the Grafana stack (Tempo for traces, Loki for logs, Prometheus or Mimir
  for metrics). It can be self-hosted, and troubleshoot workflows later cite the same
  Prometheus and Loki, so one stack serves both. If a team already runs Datadog,
  Honeycomb or similar, point OTLP at it.
- Export is off by default and turned on with `CONDUCTOR_OTLP_ENDPOINT` or config.
  Conductor never phones home.
- Prompts and agent output are **not** exported unless content capture is turned on.
  Redaction (§9.8) runs before export.
- `conductor trace <run>` renders a run's spans locally without any backend. Each receipt
  carries its trace id; each trace carries its receipt hash.

### 9.10 Reuse
- `promote` turns a successful `adhoc` or `plan` run into a reusable workflow file.
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
  3  DISPATCH     headless: spawn process
                  herdr: split pane, start agent, prompt
                          precondition: agent state idle|done
                        |
  4  OBSERVE      hooks (Stop) or headless JSON stream mark the turn end;
                  herdr state only says when to look
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
(herdr or any other), because they infer agent state from the terminal.

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

The herdr executor lives behind one adapter, pinned to a herdr protocol range and covered
by contract tests that run against a real herdr. The verified herdr-0.8.2 specifics
(timeout bounds 3000 < t ≤ 300000 ms, `agent_not_ready` keeps the name usable, etc.) stay
inside the adapter, not in the engine. Public herdr docs describe protocol 15 while this
spec was verified against protocol 20: the protocol moves, so `doctor` refuses an
unpinned version rather than guessing.

### 11.2 Pane and tab control **[new]**

Both the orchestrator and the agents it runs can open, close and drive herdr tabs and
panes. Every such action goes through one door so it is checked against policy and
recorded.

**Who can do what**

| Actor | Can | Default limits |
|---|---|---|
| Orchestrator | open a tab per run; open a pane per stage; close a pane when its stage passes; keep failed or blocked panes open; focus, rename, move | only tabs and panes it created |
| Agent (from inside its pane) | open panes in its own run's tab (a shell, a dev server, a log tail); send input to and read from those panes; close panes it opened | its own run's tab only; cannot touch other runs' panes or the conductor pane; cannot start another agent unless the workflow allows it |
| Human | anything, from herdr or from the conductor pane | recorded as `human` events |

**How agents reach it**

- Per run, conductor installs a short **conductor skill** (instructions plus the commands
  below) into the agent's context, next to the hooks it already installs, and sets
  `CONDUCTOR_RUN`, `CONDUCTOR_STAGE` and `CONDUCTOR_SOCKET` in the pane's environment.
- The agent calls `conductor pane open --cmd "cargo watch -x test"`, `conductor pane
  send`, `conductor pane read`, `conductor pane close`, or `conductor tab list`.
  Conductor checks the request against the workflow's `herdr` policy, performs it through
  the herdr adapter, and writes an `observed` event naming the stage and agent that asked.
- A refused request returns the reason (for example "pane 3.1 belongs to another run"), and
  the refusal is also recorded.

**Calls that bypass conductor**

Agents can also call herdr's own CLI directly. Conductor subscribes to herdr's read-only
event stream, so a pane or tab it didn't create or approve is still seen. It is recorded
as `unmanaged`, listed under **not checked** in the receipt, and flagged live in the
conductor pane. It is never silently adopted.

**Starting other agents**

Starting another agent from inside a stage is off by default, because it blurs who did
what. When a workflow allows it (`herdr.agents_may_start_agents: true`), the new agent
becomes a named sub-stage with its own hooks, evidence and line in the receipt. Frozen
paths and the scope check apply to every pane working in the run's worktree, so a
sub-agent can't edit the locked tests either.

**Headless runs**

Under the headless executor, `conductor pane open` starts the same command as a managed
background process: its output is captured, it is stopped at stage end, and it is
recorded the same way. A workflow that uses panes still runs in CI unchanged.

### 11.3 Record integrity **[new]**
- The chain head is written to a `Conductor-Chain:` trailer on the run's final commit. Once
  pushed, rewriting the record means rewriting pushed history, which the remote and every
  clone would notice.
- Commits are signed with the user's existing git signing config (SSH or GPG) when present.
  Conductor manages no keys of its own. The manifest records whether commits were signed.

---

## 12. Architecture

```
  conductor run | ask | plan | promote | verify | recall | trace | doctor
                          |
            ENGINE  (deterministic, zero LLM calls)
     workflow loader (base SHA) --> scheduler --> stage loop --> check runner
                                        |                          |
                                   budget, scope              event stream
                                   & admission                 |        |
                          |                     |          receipts   OTLP
                     EXECUTORS              CHECK RUNNERS
              herdr    (interactive)       process  (test, lint, mutants)
              headless (CI, overnight)     scope    (diff vs declared globs)
              api      (judge only)        query    (commands, promql, logql, kubectl)
                                           schema   (ack.yaml, claims.yaml)
                          |
                     EVIDENCE PLANE  (same under every executor)
              agent hooks · agent session logs · git · engine-run checks
                          |
                              STORES
              worktrees  artifacts  evidence blobs  git notes  run index  state.json
```

---

## 13. Data contracts

### 13.1 Workflow (`*.yaml`, read from base SHA)

```yaml
id: feature-with-independent-tests
version: 1
kind: build
requires:
  conductor: ">=0.1"
  herdr_protocol: "20"      # checked only when --executor herdr

runtime:
  artifact_root: ".conductor/{{run_id}}"
  max_parallel: 3                  # parallel runs, each in its own herdr tab
  approvals: manual

herdr:                             # §11.2
  tab_per_run: true
  close_passed_panes: true
  agent_control: own_tab           # none | own_tab
  agents_may_start_agents: false

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
- `environment.executor`: `{ "kind": "headless" | "herdr", "version", "protocol" }`.
- `stages[].turns[]`: each with `source: observed | inferred`, from hooks or herdr.
- `trace_id`: the OpenTelemetry trace for the run (§9.9).
- `stages[].gates[]` as an array, each with `verdict: pass | fail | flaky` and `runs[]`.
- `stages[].scope`: `{ "declared": {...}, "changed_files": [...], "violations": [...] }`.
- `stages[].mutation`: `{ "tool", "in_diff": true, "caught", "missed", "timeout", "unviable", "score" }`.
- `capability_attestation` limited to checked facts. Tool-call counts appear only with a
  hook or headless source, otherwise `"tool_calls": "unavailable"`.
- `integrity`: `{ "chain_head", "anchored_in_commit", "commits_signed": true|false }`.
- `not_checked`: the list printed in the PR body (§7).

### 13.3 Claims and assertion grammar (`adhoc`, `troubleshoot`, `analyse`) **[changed]**

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
- **[new]** Proving causation. `troubleshoot` grades causal claims only up to temporal
  precedence plus a falsifier that did not fire.
- **[new]** Hosted service, phoning home, credential storage. (OTLP export goes only where
  the user points it.)
- **[new]** Our own terminal multiplexer. herdr provides it.

## 15. Open risks **[new]**

| Risk | Mitigation or test |
|---|---|
| Frontier agents already run tests before saying done, which erodes catch rate | Measure catch rate in the pilot. Kill criterion in ASSESSMENT §6 |
| Vendor terms restrict automated seat-licence use | `doctor` reports auth mode; CI defaults to API keys; terms documented per vendor |
| Mutation testing too slow on real repos | In-diff only, own wall-clock budget, timeout recorded as `unavailable`, not pass |
| Agent CLIs change headless flags or JSON shapes | Executor adapters pinned per version; `doctor` detects drift |
| herdr protocol or CLI changes under us | One adapter, pinned protocol range, contract tests against real herdr, `doctor` refuses unknown versions; headless keeps working if herdr breaks |
| Agent hooks change shape | Parser tolerant of unknown fields; junk never fails a hook; `doctor` checks a hook round-trip |
| Name collision: other tools are called "Conductor" | Owner chose to keep the name; check package-name availability before first release |

## 16. Build order **[changed]**

**v0.1 — "verified development, watched or overnight"**
YAML workflow loader (base SHA) · `build` kind · both executors: `herdr` and `headless`
for claude and codex · evidence plane: hook install per run, session-log reader ·
sequential stages, one worktree per stage · ack check · scope and frozen-path check ·
`command_assert` with `cargo_json` and red-first asserts · in-diff mutation · flaky
verdict · retry ladder (in-context, fresh) · receipt: hash-chained manifest anchored in a
commit trailer · `verify` · OTLP export plus `conductor trace` · local report ·
minimal `doctor` (herdr protocol, hooks, auth).

**v0.2 — "operators"**
`adhoc` kind (`conductor ask`) with claim grading · `qa` kind · `junit_xml` (non-Rust) ·
PR delivery with check table and not-checked list · `check-pr` for CI · hook guard (block
writes to frozen paths live) · human stages · resume · cross-kind retry · `verify --rerun`.

**v0.3 — "troubleshoot and analyse"**
`troubleshoot` kind with command, promql, logql and kubectl citation checks · `analyse`
kind · data handling (§9.8) · challenger stage · `plan` · `promote` · `recall` · parallel
stages with disjoint scopes · optional OS sandbox · advisory judge · `audit` export.
