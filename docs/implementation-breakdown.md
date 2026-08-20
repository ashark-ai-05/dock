# Dock V0.1 — implementation breakdown

## Outcome

A local-first Rust/Ratatui handoff desk that binds one `kanban-md` task to one Herdr-managed run and Git worktree. A human can receive a structured handoff, inspect task-contextual Git/check evidence, route a decision to an agent/reviewer, open the exact worktree in LazyGit, then explicitly mark the task done. Dock never infers completion or merges code automatically.

## Product boundaries

- **kanban-md:** canonical Markdown task contract, claim, and task status.
- **Herdr:** canonical managed pane/workspace/runtime state.
- **Git:** canonical worktree, branch, diff, commit, and merge facts.
- **delta:** preferred diff renderer, not a source of state.
- **LazyGit:** human-controlled Git operations, not driven programmatically by Dock.
- **Dock:** a binding and handoff layer, not a terminal multiplexer, Git client, Kanban replacement, agent surveillance system, cloud service, or vendor wrapper.

## Proposed Linear structure

### Milestone 1 — Deterministic handoff vertical slice

1. **Dock: product boundary and integration contract**
   - Define adapter ownership and no-inference policy.
   - Acceptance: a versioned product contract names all source-of-truth boundaries and prohibited behaviours.

2. **Dock: versioned task/run binding and durable handoff packet format**
   - Define `run_id`, task ID/path, Herdr pane/workspace reference, Git worktree/branch/base SHA, declared checks, explicit handoff, and human decision record.
   - Out: raw transcript persistence and secrets.
   - Acceptance: valid/invalid fixtures and round-trip serialization tests.

3. **Dock: kanban-md adapter with atomic task claim/status transitions**
   - Read task Markdown and invoke only supported `kanban-md` claim/move paths.
   - Acceptance: fixture CLI contract proves Dock never creates an unclaimed duplicate run.

4. **Dock: Herdr runtime adapter spike and deterministic pane binding**
   - Use the supported Herdr CLI/socket surface to bind to known managed panes; do not terminal-scrape.
   - Acceptance: fixture and live opt-in smoke prove working/blocked/idle is displayed as runtime state, not task completion.

5. **Dock: Git worktree/fact adapter and delta diff renderer**
   - Resolve repository, worktree, branch, base/head, changed files, and checkable diff.
   - Render through delta when available with a safe fallback.
   - Acceptance: temporary Git repository tests and no shell-injection path construction.

6. **Dock: Rust workspace and Ratatui interaction shell**
   - Keyboard-first wide/medium/narrow foundation, no adapters required.
   - Acceptance: deterministic fixture UI, navigation tests, and screenshot/VHS-ready run.

7. **Dock: dispatch-to-handoff end-to-end vertical slice using fixture adapters**
   - Bind task → run → worktree, receive explicit handoff, display status and checks.
   - Depends on: 2–6.
   - Acceptance: one runnable fixture demo and tests covering a blocked run, unverified check, and handoff arrival.

### Milestone 2 — Human review and routing

8. **Dock: attention inbox and handoff detail screen**
   - Prioritized by explicit decision/review/failure events; contextual evidence before raw detail.
   - Depends on: 2, 7.

9. **Dock: review routing and open-in-LazyGit command**
   - Produce a compact review packet and launch LazyGit at the bound worktree only on explicit user action.
   - Depends on: 5, 8.
   - Acceptance: no automatic staging, commit, merge, or push.

10. **Dock: end-to-end fixture demo, safety tests, VHS walkthrough, and documentation**
    - Demo input → handoff → review route → LazyGit intent → explicit completion.
    - Depends on: 9.

## Dependency graph

```text
1 ─────────────────────────────────────────────────────────────┐
2 ───────┬── 3 ─┐                                               │
         ├── 4 ─┼── 7 ── 8 ── 9 ── 10                          │
         └── 5 ─┘               ▲                               │
6 ──────────────────────────────┘                               │
```

## Current implementation status

Implemented before real adapters:

- Rust/Ratatui application shell;
- fixture-backed task/run/handoff/check model;
- deterministic state transitions for accept-scope and request-changes;
- explicit LazyGit command intent rather than automated Git action;
- unit tests proving no inferred merge/completion path;
- interactive terminal smoke test.

Next safe code slice: extract the current fixture model into a versioned `HandoffPacket` contract with schema/round-trip tests. Do not connect to Herdr, kanban-md, or LazyGit until the adapter contracts are approved and fixture-tested.
