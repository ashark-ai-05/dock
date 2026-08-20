# Dock Runtime — vertical-slice delivery plan

This plan implements Dock as a local-first coding-agent runtime and control plane. A single repository is one repository binding; a multi-repository programme adds bindings, capacity, and explicit dependency edges without changing the base runtime model.

The outcome contract is [`dock-runtime-product-spec.md`](dock-runtime-product-spec.md).

## Delivery principles

- Deliver a complete, observable user outcome in each slice.
- Use fixture agents and temporary Git repositories for deterministic tests before real-agent smoke tests.
- Dock manages only Dock-owned process groups; never infer authority from PID names or terminal text.
- Keep Git/task systems authoritative at their sources. Dock stores explicit bindings and decisions.
- Preserve the human gate for consequential Git changes and cross-repository dependency release.

## Slice 0 — Product reframe and parity contract

### User outcome
A potential user can understand exactly why Dock replaces the combined Herdr + task + Git workflow instead of being another dashboard.

### Scope
- Rewrite product positioning, source-of-truth boundaries, safety model, and roadmap.
- Maintain a feature-parity matrix for Herdr daily-workflow capability.
- Link the runtime product spec and testable acceptance evidence.

### Out of scope
- Runtime code or dependency changes.

### Acceptance checks
- [ ] README does not describe Dock as a narrow handoff desk or deny terminal-runtime ownership.
- [ ] Product spec distinguishes current capability from target capability.
- [ ] Herdr parity claims are not presented as implemented until verified.
- [ ] Documentation review checks no promise requires terminal-text inference or automatic Git mutation.

### Definition of done
Documentation is internally consistent and committed as one product-reframe milestone.

## Slice 1 — One owned runtime, reconnectable Control Pane

### User outcome
As a developer, I can start `dockd`, create a Dock-owned workspace and pane running a fixture command, detach the UI, and reconnect to observe its continuing state.

### Scope
- Local daemon lifecycle, versioned Unix-socket handshake, Control Pane connection state.
- One owned PTY/process group, bounded scrollback, attach/detach/reconnect.
- Non-interactive inspect command for fixture testability.

### Out of scope
- Real agent adapters, worktrees, external task systems, multi-repository graph, or terminal-emulator parity.

### Acceptance checks
- [ ] Protocol/domain tests reject version mismatch and malformed messages.
- [ ] Fixture process remains live through client reconnect.
- [ ] `dockd` never identifies or stops a non-Dock process.
- [ ] CLI/TUI demo shows running, stopped, and failed owned-process state.
- [ ] Failure path leaves an actionable persisted receipt without raw transcript capture.

### Risks / dependencies
PTY and process-group semantics must work on macOS and Linux. Start with an implementation spike bounded to one platform CI fixture plus documented portability checks.

## Slice 2 — Open one repository and dispatch a bound fixture run

### User outcome
As a developer, I can open one local Git repository, choose a task, create/choose an isolated worktree, and dispatch a fixture agent into a Dock-owned pane bound to that task/worktree.

### Scope
- Repository binding and local persistence.
- Verified Git worktree/branch/base SHA facts.
- `kanban-md` adapter integration for explicit atomic claim where configured.
- Dispatch receipt links task, repo, worktree, pane, process group, and run.

### Out of scope
- Provider-specific agent semantics, multi-repository dependencies, Git mutation, or automatic task completion.

### Acceptance checks
- [ ] Temporary Git repo integration test proves a valid binding is created.
- [ ] Invalid worktree or task claim prevents process launch.
- [ ] Fixture agent runs in the expected working directory only.
- [ ] Control Pane displays Git facts and bound-run identity.
- [ ] Single-repository demo requires no programme setup.

## Slice 3 — Explicit handoff and human evidence loop

### User outcome
As a developer, I can receive a strict handoff from a bound run, inspect Git/delta/check evidence, and deliberately accept scope or request a change.

### Scope
- Daemon-owned handoff storage and Control Pane attention inbox.
- Existing strict `HandoffPacket` contract becomes daemon/client protocol payload.
- Git facts, colour diff surface, declared checks, explicit human decision record.

### Out of scope
- Parsing terminal output for completion, automatic Git mutation, cross-repository dependency release.

### Acceptance checks
- [ ] Unknown/future packet fields and mismatched run IDs are rejected.
- [ ] A handoff shown in the UI maps to real bound Git facts.
- [ ] Decision records are explicit and do not imply task completion.
- [ ] A terminal/VHS fixture demo shows dispatch → handoff → review → route.

## Slice 4 — First-class agent adapters and safe lifecycle control

### User outcome
As a developer, I can select a supported agent adapter, launch it in a bound Dock pane, focus/attach/interrupt/stop it, and see honest lifecycle facts.

### Scope
- Provider-neutral adapter contract and generic process fallback.
- First-class adapter discovery/launch profiles for Amp, Claude Code, Codex CLI, and GitHub Copilot CLI.
- Explicit capability declarations and lifecycle operations only where each provider supports them.

### Out of scope
- Credential handling, semantic terminal scraping, arbitrary process adoption, automatic quality inference.

### Acceptance checks
- [ ] Missing binary fails before pane/run creation.
- [ ] Fixture adapter validates the lifecycle contract deterministically.
- [ ] At least one installed real agent is manually smoke-tested after user-authenticated setup.
- [ ] Stop/restart affects only the Dock-owned process group.
- [ ] Unsupported agents remain process-level `unknown` rather than falsely classified.

## Slice 5 — Two repositories, capacity limits, and one dependency gate

### User outcome
As a developer, I can open two repositories under one Dock runtime, see three active/queued runs, and use an explicit upstream handoff plus human decision to release a gated downstream dispatch.

### Scope
- Repository portfolio view.
- Programme graph, dependency-edge contract, global/per-repository capacity limits, human-reserved review capacity.
- Deterministic dispatch refusal/release path.

### Out of scope
- Automatic dependency discovery, task-system replication, multi-user remote coordination.

### Acceptance checks
- [ ] Two repositories retain separate worktrees, process groups, environment allowlists, and task bindings.
- [ ] Third dispatch respects configured global/per-repo capacity policy.
- [ ] Downstream run cannot start before valid upstream handoff plus configured human decision.
- [ ] Release action starts only the intended downstream Dock-owned run.
- [ ] End-to-end fixture test and recorded two-repository terminal walkthrough pass.

## Slice 6 — Herdr daily-workflow parity

### User outcome
As a former Herdr user, I can use Dock as my daily runtime without losing essential workspace/pane/session ergonomics.

### Scope
- Workspace/tab/pane split, swap, focus, resize, rename, close, layout persistence, zoom.
- Scrollback/history replay, restart recovery, themes/configuration, notifications.
- CLI/socket API completeness for implemented runtime features.
- Explicitly scoped mouse interactions where they improve pane/layout work.

### Out of scope
- Unbounded terminal-emulator feature parity or features not proven valuable in the controlled Dock workflow.

### Acceptance checks
- [ ] Published parity matrix marks every daily-required Herdr capability as shipped, intentionally different, or deferred.
- [ ] Layout/session recovery manual demo passes after client restart and daemon restart where supported.
- [ ] API and CLI contract tests cover workspace/pane lifecycle.
- [ ] No legacy Herdr runtime is required for the Dock end-to-end demo.

## Slice 7 — Full Git workflow bridge, plugins, and release gate

### User outcome
As a developer, I can review work inside Dock, open the exact worktree in LazyGit for full interactive Git actions, and extend safe local workflows without compromising Dock’s ownership model.

### Scope
- Native Git evidence/control views and explicit LazyGit handoff or embedded pane strategy.
- Local plugin/extension contract with permissions and event hooks.
- Security review, source-redaction checks, documentation, packaging, demo, and release readiness.

### Out of scope
- Automatic Git mutation or hosted marketplace/service.

### Acceptance checks
- [ ] LazyGit receives the exact intended worktree and no Git mutation occurs before explicit human action.
- [ ] Plugin permissions require declaration and do not expose arbitrary runtime secrets.
- [ ] Full quality gate and a clean-machine install/demo procedure pass.
- [ ] README distinguishes implemented feature parity from roadmap items.

## Dependency order

```text
0 ── 1 ── 2 ── 3 ── 4 ── 5 ── 6 ── 7
                  │         │
                  └─────────┴── required two-repo programme proof
```

## Current status

Completed before this reframe:

- Ratatui fixture Control-Pane prototype;
- strict versioned handoff-packet model and local atomic storage;
- `kanban-md` read/atomic-claim adapter;
- Git facts and `delta` renderer adapter;
- explicit LazyGit launch intent;
- documented task contract and local smoke tests.

These are foundations only. Dock does **not yet** own a PTY runtime, launch real coding agents, provide Herdr parity, or operate multi-repository programme gates.
