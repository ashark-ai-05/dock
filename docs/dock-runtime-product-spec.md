# Dock Runtime — outcome specification

## Decision

Dock is a local-first, provider-neutral coding-agent runtime and control plane. It owns the runtime role while integrating the daily workflow strengths of `kanban-md`, Git, `delta`, and LazyGit.

Dock works naturally for one repository. A multi-repository programme is the same model with more than one repository binding; users do not create a separate “programme mode” for ordinary single-repository work.

Dock is better than a terminal multiplexer only when it makes explicit delivery control possible: ownership, capacity, task/run/worktree bindings, cross-repository dependencies, handoffs, evidence, and human decisions.

## User and workflow

### Current workflow

A developer opens several terminals and worktrees, starts agent CLIs manually, watches terminal text, switches among disconnected task/runtime/Git tools, and manually remembers which upstream work unblocks downstream work.

### Target workflow

A developer opens a repository with Dock or runs multiple repository bindings under one local daemon. Dock creates and owns its workspaces, PTYs, panes, process groups, worktrees, and supported agent runs. The Control Pane presents current runtime facts, Git evidence, explicit handoffs, capacity, and dependency gates. The developer directs consequential delivery decisions deliberately.

## In scope

- A local `dockd` daemon, versioned local control protocol, and Ratatui Control Pane.
- Dock-owned workspaces, tabs, panes, PTYs, process groups, layout, terminal attachment, bounded scrollback, detach/reattach, and recovery.
- macOS and Linux first.
- Explicit agent adapters for supported coding-agent CLIs; first candidates are Amp, Claude Code, Codex CLI, and GitHub Copilot CLI.
- Single-repository use with no programme ceremony; concurrent multi-repository operation when additional repositories are opened.
- Explicit bindings among external task, repository, worktree, branch/base SHA, run, agent, pane, and process group.
- Repository task-source adapters, beginning with `kanban-md`; a task source retains its task truth.
- Git worktree/status/branch/base/head/diff/check evidence; `delta`-quality colour diff presentation.
- Explicit launch or embed handoff to LazyGit for mature interactive Git operations.
- Versioned handoff packets, attention routing, and human decision records.
- Dock-owned cross-repository programme graph, explicit dependency gates, global/per-repository capacity limits, and human-reserved review capacity.
- Local configuration, themes, notifications, extension/plugin surface, and CLI/API parity adequate for automation.

## Out of scope

- Hosted/cloud control plane, remote multi-user service, telemetry by default, or external credential storage.
- Windows support in the first release.
- Automatic staging, committing, rebasing, merging, pushing, deploying, or permission escalation.
- Terminal-text inference of task ownership, dependency completion, quality, or agent intent.
- Raw terminal transcript persistence by default.
- Replacing every task system or reproducing every advanced LazyGit interaction before Dock’s core control loop works.
- Managing processes that Dock did not launch or that a human did not explicitly import.

## Constraints

### Privacy and security

- All runtime state remains local by default.
- Dock stores no API keys or agent credentials; supported agents retain their own authentication flows.
- Process environment is allowlisted per run/repository; Repo A does not silently inherit Repo B context.
- Local socket access is user-scoped and protocol messages are versioned and validated.
- Durable records reject unknown fields where safe, redact secrets, and do not store raw agent transcripts by default.

### Source-of-truth boundaries

| Domain | Authority |
|---|---|
| Task cards, claims, per-repository task state | configured task source, initially `kanban-md` |
| Worktree, branch, commit, merge facts | Git |
| Full interactive Git mutation | human through LazyGit or another explicit client |
| Runtime workspaces, panes, PTYs, process groups, run recovery | Dock |
| Programme graph, capacity, run bindings, handoffs, dependency gates | Dock |
| Diff rendering | Dock, compatible with `delta` presentation |

### Operational boundary

Dock may stop or restart only a process group it owns. Uncertain runtime state remains `unknown`; it never becomes `completed` by inference.

## Behavioural requirements

### Repository opening

- Given a local Git repository, when a developer opens it with Dock, then Dock creates or restores an owned workspace and Control Pane without requiring a programme setup.
- Given a second repository, when a developer opens it, then Dock adds an isolated repository binding while preserving the first repository’s process, task, worktree, and environment boundaries.

### Runtime ownership

- Given a Dock-created run, when Dock launches a supported agent, then the run is associated with an owned pane, PTY, process group, worktree, agent adapter, and explicit task binding.
- Given a reconnecting Control Pane, when `dockd` is still alive, then current owned runtime state and bounded scrollback are restored.
- Given an unknown or external process, when a user requests management, then Dock requires explicit import/attachment and never claims implicit ownership.

### Agent lifecycle

- Given a configured supported agent binary and valid user authentication, when the developer dispatches a task, then Dock starts the agent in its bound worktree and reports runtime facts separately from task facts.
- Given an unsupported agent, when a developer uses a generic process launch profile, then Dock reports process-level state only and does not claim provider-specific lifecycle semantics.
- Given a Dock-owned agent, when the developer stops or restarts it, then Dock acts only on its owned process group.

### Handoffs and evidence

- Given a running agent, when it emits a valid explicit handoff, then Dock validates, stores, and routes the structured packet to the Control Pane.
- Given a handoff, when a developer reviews it, then Dock shows bound repository/worktree/Git facts and declared checks before the raw terminal view.
- Given a handoff question, when the developer records a decision, then Dock persists the decision and only releases configured downstream dependency gates.

### Git and task workflow

- Given a bound worktree, when a developer opens review, then Dock shows status, branch, base/head, changed-file facts, colour diff, and declared check evidence.
- Given a request for advanced Git mutation, when the developer explicitly chooses it, then Dock opens LazyGit at the exact bound worktree; Dock does not issue the Git mutation itself.
- Given a `kanban-md` task source, when Dock claims or moves a task, then it uses the task source’s supported atomic command path rather than editing task Markdown directly.

### Cross-repository programmes

- Given one repository, then all core runtime, agent, handoff, and Git features work with a single repository binding.
- Given two or more repositories, when a developer declares a dependency edge, then Dock shows the edge and blocks downstream dispatch until its declared upstream handoff/decision condition is met.
- Given capacity limits, when a dispatch would exceed a global or per-repository limit, then Dock refuses the dispatch with an actionable explanation and does not create a partial runtime.

## Failure and edge behaviour

- Missing agent binary: do not create a pane/run; state the missing executable and suggested adapter/configuration action.
- Agent launch failure: retain the failed run record, capture bounded redacted diagnostic facts, and offer retry only after explicit action.
- Daemon crash/restart: recover only persisted owned session metadata; never attach arbitrary matching processes by name.
- Socket protocol mismatch: refuse the client connection with an upgrade-compatible error.
- Worktree/Git failure: do not start an agent without a verified bound worktree; retain a failed dispatch receipt.
- Dependency mismatch/unknown handoff schema: leave downstream work gated and surface the validation reason.
- Task adapter failure: leave task state untouched and show the external command failure as evidence.

## Acceptance evidence

- Automated domain, protocol, PTY/process-ownership, task-adapter, Git, and dependency-gate tests.
- A local two-repository / three-agent end-to-end demonstration: one upstream explicit handoff gates a downstream dispatch until a human decision releases it.
- A single-repository demonstration using the same daemon and Control Pane without programme configuration.
- Daemon restart/reconnect demonstration proves owned-pane recovery.
- Fixture agents permit deterministic tests without credentials; at least one supported real agent is manually smoke-tested.
- Terminal/VHS walkthrough, screenshot, and reviewed local artefacts.
- `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, and non-interactive CLI smoke commands pass.
- Prohibited behaviour tests demonstrate no automatic Git mutation, no unowned-process control, no task-completion inference, and no raw secret/transcript persistence.

## Assumptions and open questions

- **Confirmed:** Dock owns the runtime/pane lifecycle, targets macOS + Linux first, and supports multi-repository programmes.
- **Confirmed:** a programme is 1..N repositories; a single repository is implicit.
- **Confirmed:** external task systems retain task-source authority; Dock owns relationships and programme control.
- **Working assumption:** first-class adapters will launch Amp, Claude Code, Codex CLI, and GitHub Copilot CLI, subject to each CLI’s stable local launch/resume behaviour.
- **Open:** exact terminal emulation/rendering library and daemon packaging approach; decide after a bounded PTY/reconnect spike.
- **Open:** whether LazyGit appears as a child/embedded terminal surface or is launched as a focused Dock pane first; initial implementation should preserve the explicit human-action boundary either way.
- **Open:** release definition for full terminal-runtime parity; maintain a visible capability matrix before making replacement claims publicly.

## First vertical slice

A developer starts `dockd`, opens two repositories, and connects one Control Pane. Dock creates three owned agent panes across those repositories. One explicit upstream handoff from Repository A is a declared prerequisite for a Repository B task; until a human records the configured decision, Dock visibly blocks downstream dispatch. The same daemon supports opening only one repository without programme setup.

### Current Slice 5 persistence boundary

Owned process state and bounded scrollback remain in daemon memory only. Reconnect works while the same daemon remains alive; restart does not reattach processes. Queued dependency gates persist atomically in owner-only local state, use relative path bindings, and are re-canonicalized and validated before the daemon accepts them after restart. Owner-only durable receipts reserve run identity and retain bounded structured launch evidence without raw PTY output, commands, or absolute repository/worktree paths. Strict handoff and human-decision records remain private local structured evidence.

## Approval gate

This product specification authorises planning and the documentation reframe only. Runtime implementation, dependencies, external workflow creation, releases, and Git publication require an approved execution slice.
