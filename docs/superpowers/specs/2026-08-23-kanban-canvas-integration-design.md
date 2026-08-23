# Dock B — The board becomes a pane, and agents grow a queue

Status: proposed
Date: 2026-08-23
Sub-project: B of the post-P0 programme (A1 shipped, A2, B, C)

## Decision

Dock's kanban board is today a modal overlay that loads once when you open it, renders five hardcoded
columns, dispatches exactly one card into exactly one focused pane, and then goes away. The daemon has
never heard of it. Nothing about a live agent reaches it, and nothing about it survives a quit.

This project makes the board a **first-class surface on the canvas** — a pane with a kind, alongside the
terminal panes — showing two lanes: the **runs lane**, derived live from the agent panes that exist right
now, and the **backlog lane**, the Markdown files on disk. And it gives each agent pane a **per-agent FIFO
queue** whose next entry is fed to the agent automatically when that agent's turn ends.

Six requirements, taken as given:

1. Two lanes: an auto-populated runs lane **and** a manually curated backlog lane.
2. A kanban split is a real pane with no PTY. Panes gain a kind.
3. Realtime status in that split.
4. The existing popup overlay stays as an option.
5. Per-agent FIFO queue with auto-feed on idle.
6. CLI commands for the same operations.

**This spec must be built as two projects, not one.** Sections 1–7 (the board becomes a pane) and section 8
(the queue) share only the runs lane's rendering. The first is client-side plus one layout schema field; the
second is a new daemon subsystem carrying the entire safety burden. Shipping them in one branch would put a
schema migration, a UI restructure, a protocol bump, and an unattended-dispatch mechanism into one review.
The build sequence in §13 keeps them separable.

---

## Verified foundation

Everything below was read from the source, not inferred. Line numbers are from the working tree on
2026-08-23; `src/dispatch.rs` was being edited concurrently, so its numbers may have drifted by tens of lines.

**Every pane is unconditionally PTY-backed.** `launch_pane_shell` (`dispatch.rs:521`) is called from pane
create (`:887`), pane split (`:1058`), and `revive_restored_panes` (`:600`). Its doc comment: *"Every Dock
pane is a working terminal from the moment it exists."*

**But the codebase already handles a pane with no run, everywhere.** `queue_resize`
(`dashboard.rs:1248`) returns early on `run_id: None`. `Dashboard::send_to_pane` (`dashboard.rs:2856`)
drops input for a pane with no run. `render_node` (`dashboard.rs:1126`) already has a `None` arm that paints
a placeholder. `pane_input` (`dispatch.rs:2252`) refuses a pane whose `pane_run` is `None`. **The Board pane
is a much smaller change than "every pane is a terminal" suggests**, because the no-run path is already
defensive.

**`DurablePane` persists only `pane_id` and `name`** (`layout.rs:93-98`). `run_id` and `runtime` are
deliberately process-local: `into_runtime` (`layout.rs:812`) hands every restored pane
`run_id: None, runtime: Restored`.

**Three properties of agent-state detection are load-bearing for §8. They are cited as *behaviour*, not as
line numbers or step ordering, on purpose:** the state machine was being rewritten by a concurrent
status-oscillation fix while this spec was written, and it changed shape under the reading. Every claim below
was re-verified against the tree after that rewrite landed, and each is a property the module's own doc
comments state as an intent rather than an implementation detail — so the safety argument in §8 survives
further tuning of the constants.

**(i) `AgentState::Idle` does not mean "the agent is idle." It means no agent was detected in this pane** —
a plain shell. The variant meaning "the agent finished its turn and is waiting for you" is **`Done`**,
labelled `"your turn"` (`detect/mod.rs:118-122`). Requirement 5's "goes to your turn/idle" therefore keys off
`Done`, and `Idle` must be an explicit **refusal**. The rewrite strengthened this: a screen classification
that matches no rule now means "no idea" and is explicitly forbidden from falling through to a state, so a
resolved `Idle` can only come from the absence of an agent.

**(ii) A state the agent reported about itself overrides anything read off its screen.** Reports arrive via
`report_agent_state`, fed by `dock agent-state`, installed by `dock hooks --install`; they are held in
`reported_states` on `RuntimeRegistry` — the same struct the queue will live on, so the queue can read them
without any protocol change. The rewrite also made a report *commit* rather than latch forever: if an agent
stops reporting, inference resumes from where the agent actually was. That is strictly better for §8 — an
agent whose hooks fall away cannot leave a stale `Done` sitting there authorising a feed.

**(iii) Silence is the classifier's only positive evidence that a turn ended.** Screen text is trusted to say
an agent has *stopped* and deliberately never to say it is going; the `Working` claim comes from bytes
arriving, and the `Done` claim comes from bytes having stopped. So **an agent that simply goes quiet is
classified as finished** — currently after roughly two seconds (`WORKING_SILENCE` of 1200ms of silence, then
`STATE_DWELL` of 600ms for that answer to hold), against 1.3s before the rewrite.

**This is the single most important fact in this document, and §8.4 is built on it.** The oscillation fix
made the signal much *steadier* — hysteresis on every non-`Blocked` transition, burst detection that
distinguishes a footer clock from generation — but steadier is not the same as correct. An agent that pauses
two seconds on a network call, or runs a tool that prints nothing, is still indistinguishable from a finished
one by bytes alone, because there is nothing in the bytes to distinguish. No amount of tuning changes that,
which is why §8.4 declines to act on this signal by default rather than proposing a better threshold.

**`AgentStateChanged` is edge-triggered per subscriber** (`server.rs:526-539`) and carries only
`{ run_id, agent, state }` — no pane id, no workspace id. Its dedupe map is local to one subscriber
connection, so it is **not** a daemon-global transition signal. The `Dashboard::apply_event` arm
(`dashboard.rs:361`) does one map write into `self.agents` and does **not** set `needs_refresh`.

**Programme gates are a dependency mechanism, not a queue.** `gate_snapshot` (`dispatch.rs:1711`) derives
state purely from two files on disk; `Ready` requires a human `Decide` on an upstream run. `release_gate`
(`dispatch.rs:1492`) is called from exactly one place, `server.rs:330`, the `Request::ReleaseGate` handler.
Nothing evaluates gates on a tick; a gate becoming `Ready` is a passive fact, not an event. §8.1 works
through why this cannot carry the queue.

**418 inline `#[test]`s**, in `#[cfg(test)] mod tests` at the bottom of each file, named as full behavioural
sentences. No `tests/` directory, no integration test harness.

---

## Scope

In scope:

- `PaneKind { Terminal, Board }` on `PaneLayout`, persisted, with a `layout.json` schema migration.
- A Board pane that renders the two lanes and takes keys, with no PTY.
- The board overlay kept, rendering the same two lanes from the same view struct.
- `BoardTask` gains exactly one field: `body`.
- Status columns become the union of the known list and whatever is on the board, fixing the invisible
  `needs-input` card.
- A per-pane FIFO queue in the daemon, persisted, with auto-feed on a hook-reported end of turn.
- `dock queue` as a hand-parsed subcommand.
- Protocol v11 (B1) and v12 (B2).
- Four defect fixes folded in (§10) — one of which (§10.2) ships as its own small project B3 — plus one
  deferral (§10.3, `kanban.rs`) awaiting a check only the user can run.

Explicitly out of scope, with reasons in §12: a filesystem-watch dependency, a `BoardChanged` protocol event,
a YAML parser, `tags`/`depends_on`/`class`/`created`/`updated` on `BoardTask`, card age colouring, WIP limits,
manual reordering inside a column, an overlay trait refactor, additional pane kinds, cross-pane work pools,
queue priorities, and automatic board moves on agent completion.

---

## 1. Architecture

```
                 ┌───────────────────────── dockd ─────────────────────────┐
 board files ──▶ │  (never sees the board)                                 │
 (client reads)  │  RuntimeRegistry                                        │
                 │    runs ──▶ PTY ──▶ agent state (screen + hooks)        │
                 │    layout ──▶ PaneLayout { kind, run_id, … }            │
                 │    queues ──▶ PaneQueue per (workspace, pane) ──────┐   │
                 │                        ▲                            │   │
                 │            queue_tick (250ms) ◀── reported_states    │   │
                 │                        │                            ▼   │
                 │                        └────────────── pane_input(bytes)│
                 └────────┬─────────────────────────────────────┬──────────┘
                          │ Event::AgentStateChanged            │ Event::QueueChanged
                          │ Event::PaneAttached / PaneDelta     │
                          ▼                                     ▼
   ┌──────────────────────────── dock (TUI client) ──────────────────────────┐
   │  board::load(dir) ──▶ Vec<BoardTask>  ──┐                               │
   │  self.agents  (live, pushed)  ──────────┼──▶ BoardView { runs, backlog }│
   │  self.runs    (external_task_ref) ──────┘         │                     │
   │  mtime poll (500ms) ──▶ UiCommand::LoadBoard      ├──▶ Board pane       │
   │                                                   └──▶ board overlay    │
   └─────────────────────────────────────────────────────────────────────────┘
```

Three invariants shape everything:

**The daemon stays board-blind.** It has never read a task file and it does not start now. The client reads
the board; queue entries carry their prompt text verbatim, resolved client-side before they are sent. This
keeps the daemon's authority exactly where the codebase already put it — `TerminalLaunchRequest`'s doc
comment (`protocol.rs:36`) is an explicit statement that the dashboard's launch authority "cannot carry
repository, task, worktree, executable, argument, environment, or shell data", and a daemon-side filesystem
watcher over a client-named directory would be a new authority for a benefit the client can get for free.

**The queue is daemon-side.** It is durable across a client quit, it must work with no TUI open (requirement
6), and its trigger is a state the daemon computes. A client-side queue reproduces the `dispatched_tasks`
defect (§10.4) at larger scale.

**A Board pane has no run, and therefore no PTY, no resize announcement, no input path, and no scrollback.**
Not "a PTY we ignore" — no run at all.

---

## 2. `PaneKind` and the layout

### 2.1 Fork: a third `LayoutNode` variant, or a field on `PaneLayout`

**Option A — `LayoutNode::Widget { pane_id, kind }`.** Makes the kind visible in the topology.
Every tree walker grows an arm: `validate_node`, `remove_leaf`, `first_leaf`, the `render_node` recursion,
focus traversal, divider collection. Each new arm says "identical to `Pane`". Six places to keep in sync, one
of which (`validate_node`) is a security boundary.

**Option B — `PaneLayout { …, kind: PaneKind }`.** `LayoutNode::Pane { pane_id }` stays a pure leaf
reference. Every walker is untouched. `render_node` already does `let pane = &workspace.panes[pane_id];`
(`dashboard.rs:1096`) before it renders anything, so the kind is one field access away at exactly the point
it is needed.

**Recommendation: Option B.** A board pane occupies a rectangle, takes focus, resizes, and closes exactly
like a terminal pane — that is the whole point of requirement 2. Topology is not where it differs. And when
the diff pane and log pane arrive, they add `PaneKind` variants rather than `LayoutNode` variants, which is
the generalisation the user was buying.

```rust
/// What a pane is for. A pane is a rectangle that takes focus, splits, resizes and closes; its
/// kind decides only what gets drawn inside it and whether it owns a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    /// A Dock-owned PTY. Every pane was this before board panes existed, which is why it is
    /// the default a layout written by an older version deserialises into.
    #[default]
    Terminal,
    /// The task board. No run, no PTY, no scrollback; drawn from the client's own board load
    /// and its live agent roster.
    Board,
}
```

`PaneLayout` gains `pub kind: PaneKind`; `DurablePane` gains `#[serde(default)] kind: PaneKind`.
`into_runtime` (`layout.rs:812`) carries it through instead of dropping it — unlike `run_id` and `runtime`,
the kind is durable topology, not process state.

### 2.2 The `layout.json` migration

Current shape (`layout.rs:79-98`): `DurableLayout { schema_version: u16, workspaces }` with
`#[serde(deny_unknown_fields)]` on every struct in the tree, and `load` refusing anything where
`schema_version != 1` (`layout.rs:169-171`). A refusal is not an error to the user: it falls into
`quarantine_invalid_layout` and the daemon starts with **zero workspaces**.

**Forward compatibility (new binary, old file): free.** `deny_unknown_fields` rejects *extra* fields, not
*missing* ones. A v1 file has no `kind`; `#[serde(default)]` supplies `Terminal`. Every restored pane is a
terminal, which is exactly what it was. No shim, no rewrite-on-load. The first `persist()` after any layout
change silently upgrades the file.

**Backward compatibility (old binary, new file): impossible, and that must be stated rather than papered
over.** An old binary sees `kind` as an unknown field, fails the parse, quarantines the file, and starts
empty. There is no serde configuration that avoids this, because `deny_unknown_fields` is the whole point of
the hardened loader.

Three ways to respond:

- **Drop `deny_unknown_fields` from `DurablePane`.** Rejected. The hardening on this file (symlink refusal,
  uid check, `O_NOFOLLOW`, quarantine) exists because `layout.json` names the panes a daemon will spawn
  shells into. Loosening it for a downgrade convenience is the wrong trade.
- **Write a v1-shaped file when no pane is a Board.** Rejected as premature: it doubles the write path
  forever to protect a downgrade that stops working the moment the user actually creates a board pane.
- **Bump to `schema_version: 2`, accept `1..=2` on read, always write `2`, and document the downgrade
  loss.** Recommended.

The version check changes from `!= 1` to a range, and — separately worth fixing — a *future* version should
be refused with a distinct message rather than falling into the same quarantine path as corruption, so an
operator reading the log can tell "you downgraded" from "your file was mangled":

```rust
const LAYOUT_SCHEMA_VERSION: u16 = 2;

if value.schema_version > LAYOUT_SCHEMA_VERSION {
    return Err(format!(
        "layout metadata was written by a newer Dock (schema {}); this build understands up to {}",
        value.schema_version, LAYOUT_SCHEMA_VERSION
    ));
}
if value.schema_version == 0 {
    return Err("unsupported layout metadata schema version".into());
}
```

What is actually lost on downgrade is one workspace topology, and `layout.json` already discards `run_id`
and `runtime` on every load — the panes come back as empty rectangles that get fresh shells. The blast radius
is "your split arrangement". Acceptable, documented, no shim. **No downgrade migration will be written.**

### 2.3 Keeping a Board pane away from the PTY machinery

| Site | Change |
|---|---|
| `launch_pane_shell` (`dispatch.rs:521`) | Early return when the pane's kind is `Board`. Placed **inside** the function, not at its three call sites, so no future caller can forget. |
| `revive_restored_panes` (`dispatch.rs:582`) | Covered by the above; a restored Board pane gets no shell. |
| `check_launch_target` (`layout.rs:559`) | Refuse a `Board` pane with its own message — `"that pane is a board; split a terminal pane to launch here"` — rather than the misleading `"pane already has a run"`. |
| `check_bind_capacity` (`layout.rs:539`) | **Unchanged.** A board pane occupies a rectangle and must count against `MAX_PANES_PER_WORKSPACE`. |
| `WorkspaceRequest::Respawn` | Refuse for a Board pane; there is nothing to respawn. |
| `queue_resize` (`dashboard.rs:1248`) | **Unchanged.** It already returns early on `run_id: None`, and a Board pane's `run_id` is permanently `None`. |
| `pane_input` (`dispatch.rs:2252`) | **Unchanged.** `pane_run` returns `None`, producing `InvalidBinding`. |
| `Dashboard::send_to_pane` (`dashboard.rs:2856`) | **Unchanged.** Already drops input for a pane with no run. |
| `render_node` (`dashboard.rs:1091`) | Branch on `pane.kind` inside the existing `LayoutNode::Pane` arm; `Board` renders the lanes, `Terminal` keeps today's body verbatim. |

Six touch points, four of them one line. This is the payoff for Option B in §2.1.

### 2.4 Creating one

`WorkspaceRequest::Split` gains `#[serde(default)] kind: PaneKind`. One field on an existing variant, not a
new request. Old clients omit it and get `Terminal`; the protocol bump (§7) covers the reverse.

`PaneCommand::SplitBoard` bound to `Ctrl+B B` — split the focused pane and make the new half a board.
`Ctrl+B b` keeps opening the overlay, so requirement 4 costs nothing.

Closing a Board pane is `Ctrl+B x` like any other pane, which is `WorkspaceRequest::Close` unchanged. There
is no "convert this pane to a board" command: it would need a kind-change request, a PTY teardown, and an
answer to what happens to scrollback. Split and close is enough. **Cut.**

---

## 3. The two-lane card model

### 3.1 What the lanes are

The **backlog lane** is the task files: the existing column view, unchanged in kind. "Manually curated" is
already satisfied — `n` creates a card, `<`/`>` move it between columns, and both are gated on
`is_personal` so a repository's board stays kanban-md's. Nothing new is required here beyond §3.3's status
fix. Say this plainly rather than inventing curation features.

The **runs lane** is derived, never stored: one row per live agent pane, from data the client already
holds — `self.runs` (`Vec<RuntimeSnapshot>`, carrying `external_task_ref`) and `self.agents`
(`HashMap<run_id, (Option<AgentKind>, AgentState)>`, fed by pushed events).

They are laid out as **two stacked regions, not two column sets**: the runs lane across the top of the
board pane, the backlog columns below. A run is not a status, and appending a "runs" pseudo-column would
put the same card in two columns at once.

The lanes are joined in one direction: **a backlog card whose id matches a live run's `external_task_ref`
is badged in place with that run's agent state and queue depth.** That badge is what makes the board answer
"what is actually happening" without inventing a sixth column.

**The join is display-only, and that is a rule, not an implementation detail.** `needs-input` on the
repository board means "the agent is blocked and wants you" — the same thing `AgentState::Blocked` means, and
the runs lane shows it within a frame, because `Blocked` is the one state exempted from the classifier's
hysteresis precisely so a stuck agent can say so immediately. It is therefore tempting to have the board move
the card to `needs-input` when its agent goes `Blocked`.

**Dock must not.** Dock measures; the agent reports; a status change is the agent's act or the user's, never
Dock's inference. `dispatch_prompt`'s existing doc comment already makes the argument — *"'looks done' is a
regex over a screen, and the board is the durable record of what happened — moving a real task on a guess is
how a board stops being trustworthy"* — and §8.4's finding that a two-second pause reads as a finished turn
shows the guess is worse than that comment assumed. The badge is free because it is derived and vanishes when
the run does; a status write is durable and outlives whatever misread produced it.

So: **the runs lane and the card badge may display any agent state, including `Blocked`. Nothing in this
design writes a status to a card file except an explicit human act (`<`/`>`, `dock task move`) or the
dispatch claim in §10.5.** The agent moves its own card because the prompt tells it to, which is a report, not
an inference.

This is independent of §3.3's column fix, which is what makes a `needs-input` card *visible and reachable at
all*. That fix is needed whether or not anything ever moves a card there — today such a card is invisible
even when a human moved it by hand.

```rust
/// A row in the runs lane. Assembled per frame from the run list and the agent roster; nothing
/// here is stored, because a run that ends must leave the lane the moment its run does.
pub struct RunLaneRow {
    pub run_id: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub agent: Option<AgentKind>,
    pub state: AgentState,
    /// The board card this run is bound to, when the daemon's binding says so.
    pub task_id: Option<u64>,
    pub queued: usize,
    pub auto_feed: bool,
}
```

`BoardView` grows `runs: Vec<RunLaneRow>` and a `focus: Lane` cursor so `Tab` moves between the lanes.
`BoardView` stays terminal-free — that property is why its edge cases are testable today and it is not
being given up.

### 3.2 What `BoardTask` must grow

The files on disk carry `id, title, status, priority, created, updated, started, tags` (a list), and
`class`. The temptation is to capture all of it. The requirement-driven answer is one field.

| Field | Verdict | Why |
|---|---|---|
| **`body: String`** | **Add** | Required. `dispatch_prompt` (`main.rs:732`) sends only the title, so the card's actual acceptance criteria never reach the agent. A queue that feeds prompts unattended needs the real text, not a headline. This is the one field with a requirement behind it. |
| `class` | Cut | Only meaningful with `config.yml`'s `wip_limit` / `bypass_column_wip`, which is WIP-limit machinery Dock does not have and this project is not adding. |
| `created` / `updated` | Cut | Their only use is `config.yml`'s `tui.age_thresholds` colouring. That needs RFC 3339 arithmetic across UTC offsets, which means a `chrono`/`time` dependency for a cosmetic feature in none of the six requirements. |
| `tags` | Cut | Nothing routes on them. The runs lane is derived from panes, not tags. |
| `depends_on` | Cut | The only field that maps onto the existing gate machinery — and per-agent FIFO is not a dependency graph (§8.1). The natural extension point, deliberately not extended now. |

**`BoardTask` grows exactly one field.**

### 3.3 `parse()` stays; the status list changes

**`parse()` (`board.rs:255-287`) is not replaced.** Its deliberate refusal to be a YAML parser — reading only
unindented `key: value` lines between the fences, so `tags:`'s indented items cannot be mistaken for keys — is
correct, and every field this project needs is already an unindented scalar. `body` is captured by
remembering the offset after the closing fence and taking the rest of the file trimmed. A real YAML parser
would mean a new dependency (`serde_yaml` is unmaintained) and would put a reformatter next to
`set_status`'s byte-preserving rewrite, which exists precisely so Dock never reformats a file it shares.

**The status list is the real bug.** `board.rs::STATUSES` (`:166`) is
`[backlog, todo, in-progress, review, done]`; `kanban/config.yml` declares
`[backlog, in-progress, needs-input, review, done]`. `BoardView::cards()` filters by `STATUSES`, so a
`needs-input` card is invisible in the column view and unreachable by `<`/`>` even though `load()` sorts it
correctly. Three fixes:

- **(a) Correct the constant** to match `config.yml`. Breaks any personal board that used `todo`, and is
  wrong again the next time someone edits `config.yml`.
- **(b) Read `statuses:` from `<tasks_dir>/../config.yml`.** The entries are `- name: backlog` — indented
  list items, exactly what `parse()` refuses to read. Needs a second scanner for a second file format.
- **(c) Union.** `BoardView::new` computes its columns as `STATUSES` plus every status actually present on
  the board that `STATUSES` does not know, ordered by `STATUS_ORDER` position then alphabetically —
  which is the rule `status_rank` (`board.rs:246`) already applies to sorting.

**Recommendation: (c).** It fixes the bug for *any* unknown status rather than for `needs-input` specifically,
needs no config parsing and no new format, breaks no existing board, and reuses an ordering rule the file
already states. `BoardView` gains `statuses: Vec<String>`; `cards`, `selected`, `move_column`, `clamp_row`
and `follow` index that instead of the constant.

`set_status` must accept the same union. It already calls `load(directory)` first, so it can compute it
without a signature change — a typo is still refused, a legitimate board status is accepted.

---

## 4. Realtime

Requirement 3 asks the split to reflect agent state as it changes. The board pane draws from two sources.

**Agent state is already realtime and needs no protocol change.** `Event::AgentStateChanged` is pushed on
every transition and lands in `self.agents` (`dashboard.rs:361`). The Board pane assembles `RunLaneRow` from
`self.agents` and `self.runs` on each frame, so it is live the moment it renders. `external_task_ref` — the
run→card join — lives on `RuntimeSnapshot` in `self.runs`, refreshed by `PaneState`/`LayoutChanged`, and
does not change during a turn. Nothing is stale.

One caveat found while verifying: the `AgentStateChanged` arm does not set `needs_refresh`. That is correct
and must stay correct — a state transition does not invalidate the run list, and setting it would put a
daemon round trip behind every flicker of a busy agent's classifier.

**Board files: the client polls its own mtimes.** No `notify` dependency, no daemon watcher.

The alternative — the daemon owns the board and pushes `BoardChanged` — was considered and rejected. It buys
one watcher for N clients, of which there is realistically one; it costs a new crate, a new thread, and a new
daemon authority to walk a directory a client names. Against that, the client already runs a 16ms event loop.
A tasks directory is a dozen small files. Once every 500ms the loop does one `read_dir` and folds
`(entry count, max mtime, total length)` into a fingerprint; a change queues `UiCommand::LoadBoard`. Length
is in the fingerprint because mtime granularity is one second on HFS+ and an edit inside the same second
would otherwise be missed. The manual refresh key stays as the escape hatch.

**Therefore: there is no `BoardChanged` event, and requirement 3 costs nothing on the wire.** The v11 bump
in §7.1 is for `Split.kind` and `external_task_ref`; the v12 bump is for the queue. Neither is for realtime.

---

## 5. The board overlay

Requirement 4 keeps the overlay. It renders the **same `BoardView`** as the Board pane, in a popup rect, with
the same `board_key` routing. There is no second implementation and no second data path — the pane and the
overlay differ only in the `Rect` they are handed and in whether `Esc` closes them.

This is the only reason the overlay is cheap to keep. If keeping it meant a second renderer, it would be
worth arguing about.

---

## 6. The overlay stack: a position

**Do not refactor it. Fix the disagreement, and make it structurally impossible to reintroduce.**

The hazard is real and this project makes it worse. Render order (`dashboard.rs:510-530`) is
`launch, help, rename, picker, review, git, board`. Key order (`dashboard.rs:1412-1462`) is
`help, rename, launch, picker, review, board, git`. Two pairs are transposed. Today at most one overlay is
ever open so nothing is observable — but this project puts a Board *pane* on screen underneath a board
*overlay*, and the next surface added by someone reading one list will pick the wrong precedence.

**Against the full refactor** (`trait Overlay { render, key }` and a `Vec<Box<dyn Overlay>>`): eight
independent surfaces with hand-written state, landing in the same branch as a layout schema migration and a
new daemon subsystem. That is how a project stops being reviewable. The overlays also genuinely differ —
copy mode owns every key, the picker owns every printable one, the review overlay has a nested compose mode —
and a trait that accommodates all of that is a trait that says very little.

**The fix that removes the hazard, in about forty lines:** one ordered constant, and both sites derive from
it.

```rust
/// Every overlay, in the one order that governs both drawing and key routing. Two hardcoded
/// lists disagreed for eight surfaces; deriving both from this makes a ninth impossible to
/// get half right.
const OVERLAY_ORDER: [OverlayKind; 8] = [
    OverlayKind::Help, OverlayKind::Rename, OverlayKind::LaunchForm, OverlayKind::Picker,
    OverlayKind::Review, OverlayKind::Board, OverlayKind::Git, OverlayKind::Copy,
];

fn open_overlays(&self) -> impl Iterator<Item = OverlayKind> + '_ { … }
```

`render` iterates it forward (later entries draw on top); `key` iterates it and dispatches to the first open
one. No trait, no boxing, not one `render_*` or `key_*` function touched. One test —
`every_open_overlay_takes_keys_in_the_same_order_it_is_drawn` — asserts the two derivations agree, which is
the assertion nobody could write before.

And note the design already helps: **the Board pane is not an overlay**, so the eight stay eight.

---

## 7. Protocol v11 and v12

The two projects ship separately, so they bump separately. **B1 is v11**, carrying two fields on existing
requests. **B2 is v12**, carrying the whole queue surface. One combined bump would force B2's shape to be
settled before B1 could ship, which is the coupling §13 exists to avoid.

### 7.1 v11 — B1

```rust
// WorkspaceRequest::Split gains one field.
Split { workspace_id, pane_id, new_pane_id, axis, #[serde(default)] kind: PaneKind },

// TerminalLaunchRequest gains one field (see §10.4).
#[serde(default)] pub external_task_ref: String,
```

That is the entire v11 delta. No new request, no new response, no new event — because §4 established that
realtime board rendering needs none.

### 7.2 v12 — B2

```rust
// Request — one new variant, an inner tagged enum in the shape WorkspaceRequest already uses.
Queue(QueueRequest),

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueRequest {
    Inspect,
    /// `prompt` is the literal text fed to the agent. The daemon never resolves a task id: it
    /// has never read a board file and this does not change that.
    Add { workspace_id: String, pane_id: String, prompt: String, label: String },
    Remove { workspace_id: String, pane_id: String, entry_id: u64 },
    Clear { workspace_id: String, pane_id: String },
    /// Arm or disarm auto-feed for one pane. `enabled: true` is refused when the pane's agent has
    /// never reported a state and the daemon is on the default trust setting (§8.4 guard 4).
    SetAuto { workspace_id: String, pane_id: String, enabled: bool },
    /// The kill switch. Daemon-wide, persisted, and independent of every pane's own arming.
    SetPaused { paused: bool },
}

// Response
Queues { queues: Vec<PaneQueueSnapshot>, paused: bool },

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneQueueSnapshot {
    pub workspace_id: String,
    pub pane_id: String,
    pub run_id: Option<String>,
    pub auto_feed: bool,
    pub awaiting_ack: bool,
    /// Why auto-feed last declined to fire, so a stalled queue explains itself instead of
    /// looking broken.
    pub holding_because: Option<String>,
    pub entries: Vec<QueueEntrySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueEntrySnapshot {
    pub entry_id: u64,
    pub label: String,
    /// First QUEUE_PREVIEW_BYTES of the prompt, never the whole thing: a full listing of
    /// sixteen 8 KiB prompts across several panes would exceed MAX_MESSAGE_BYTES.
    pub preview: String,
    pub bytes: usize,
}

// Event — pushed so an open board pane sees a drain without polling.
QueueChanged { workspace_id: String, pane_id: String },

// ProgrammeSnapshot gains one field, so `dock-programme` shows gates and queues together
// rather than making the operator hold two mental models of "queued work".
pub queues: Vec<PaneQueueSnapshot>,
```

`QUEUE_PREVIEW_BYTES = 120`, which is what `preview` is truncated to.

`Event::QueueChanged` sets `needs_refresh` on the client, because unlike agent state, queue depth lives
only in the daemon and nothing else would tell the client it changed.

`QueueRequest::Inspect` returns every queue rather than one, so the runs lane fills in one round trip.

---

## 8. The per-agent FIFO queue

### 8.1 Why this is not built on the programme gates

The brief asked for a concrete reason or none at all. There are five, each verified:

1. **A gate launches a new run; the queue feeds an existing one.** `release_gate` (`dispatch.rs:1492`) calls
   `dispatch_with_gate_authorization(request, true, None)` — a fresh `DispatchRequest`, a fresh `run_id`, and
   `launch_target: None` so a pane is bound at release time. Requirement 5 is "the next queued prompt is
   *submitted* to that agent", which is `pane_input` into a live PTY. Different verb, different object.
2. **A durable gate cannot carry a prompt.** `validate_durable_adapter` (`dispatch.rs:2802`) rejects any gate
   whose `dispatch.adapter.arguments` is non-empty: *"durable programme gates require an argument-free
   built-in adapter; raw commands and explicit executable paths are not persisted."* The prompt **is** the
   arguments. A queue entry is unrepresentable as a durable gate.
3. **A gate becomes `Ready` only on a human decision.** `gate_snapshot` (`dispatch.rs:1711`) requires a stored
   handoff from the exact upstream run *and* a `Decide` whose route matches. Auto-feed-on-end-of-turn has no
   upstream run and no human decision. Making `AgentState::Done` satisfy `GateState::Ready` would silently
   redefine what every existing `dock-programme` gate means.
4. **Nothing evaluates gates on a tick.** `release_gate` has exactly one non-test caller, the
   `Request::ReleaseGate` handler at `server.rs:330`. There is no sweep to hook auto-feed into.
5. **A gate has no ordering key.** `programme.gates` is a `HashMap`, `DurableProgrammeGate` has no timestamp
   and no sequence field, and the on-disk form is `{run_id}.json` listed by `read_dir` and sorted
   lexicographically for display only. FIFO order cannot be recovered from the existing durable format.

Two further constraints confirm the shape is wrong even if the above were fixable: `queue_gated` refuses
unless the upstream run is live in *this* daemon (`dispatch.rs:1440`), and `restore_durable_gate`
(`dispatch.rs:2887`) rejects absolute paths and re-roots everything under the state dir — while Dock's
dispatched worktrees are absolute and sit beside the repository.

**What is reused is the durability pattern, not the semantics.** `PaneQueue` persistence copies
`save_programme_gate`/`list_programme_gates`/`quarantine_programme_gate` exactly: a directory at `0o700`,
one JSON file per queue, `schema_version` + `deny_unknown_fields`, atomic temp-write + rename + directory
fsync, a `Result<_, String>` per record so one corrupt file does not abort the listing, and quarantine on
parse failure. And both are visible in one place: `ProgrammeSnapshot` gains `queues`, so
`dock-programme` shows gates and queues together.

### 8.2 Shape and keying

A new module, `src/queue.rs`, owns the queue's state machine and nothing else — no runtime handle, no
socket, no clock. `dispatch.rs` owns the wiring; `storage.rs` owns the file.

```rust
/// Which "the agent finished" signal auto-feed is willing to act on. See §8.4 guard (4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoFeedTrust {
    /// Only a state the agent reported through `dock agent-state`. The default, because the
    /// screen classifier calls a 1.3-second pause `Done`.
    Reported,
    /// The screen classifier as well. Opt-in, for agents with no hooks.
    Screen,
}

/// One pane's queue of prompts. Deliberately holds no handle to a runtime, a PTY, or a clock —
/// every rule below is decided by `poll` from its arguments, so the whole safety surface is
/// unit-testable without a process.
pub struct PaneQueue {
    entries: VecDeque<QueueEntry>,
    next_entry_id: u64,
    /// Off unless a human armed it, and off again after any restart. See §8.5.
    auto_feed: bool,
    /// True from the moment a prompt is fed until the agent is next seen Working. While true
    /// nothing else is fed, so a misfire costs exactly one prompt rather than the whole queue.
    awaiting_ack: bool,
    /// The last state observed, so a feed keys off a transition rather than a level.
    last_state: Option<AgentState>,
    /// When the current non-Working state began, for the settle delay.
    settled_since: Option<Instant>,
    last_fed_at: Option<Instant>,
    holding_because: Option<String>,
}
```

**Keyed by `(workspace_id, pane_id)`, not `run_id`.** A run dies and is replaced by resume, respawn, or a
daemon restart; the pane is the identity the user thinks in and the one `layout.json` persists. A pane
shell's run id is itself derived from the pane (`pane_shell_run_id(workspace_id, pane_id)`). The trigger
arrives per run, and the registry already resolves run → binding → `(workspace_id, pane_id)`.

Stored on `RuntimeRegistry` as `queues: Mutex<HashMap<(String, String), PaneQueue>>`, beside
`programme` and `layout`.

### 8.3 The tick

The existing 16ms loop in `stream_events` is a *subscriber* loop with a *per-connection* dedupe map. Driving
auto-feed from it would mean a queue that only advances while a TUI is attached, and advances N times with N
clients. Both are wrong.

**A dedicated daemon thread ticking every 250ms**, calling `RuntimeRegistry::queue_tick()`. Auto-feed is not
latency-critical; a quarter second after an agent finishes is invisible, and 250ms keeps the cost of the
process-table walk (already TTL'd at 500ms, `dispatch.rs:3172`) in the noise.

`queue_tick` reads `pulse()`'s `(run_id, agent, agent_state)`, maps each run to its pane through the layout,
and calls `PaneQueue::poll`. A feed is `runtime.pane_input(workspace_id, pane_id, bytes)` — the same
function the client's keystrokes go through, with all four of its binding re-validations intact. `pane_input`
appends nothing, so the queue supplies the trailing `\n` itself.

### 8.4 The trigger, and every guard on it

The signal is `AgentState::Done`. Six conditions must all hold. Each exists because of a specific way the
signal is wrong.

**(1) Edge, not level.** Feed only on a transition into `Done` from `Working`. A level trigger would refeed
every 250ms while an agent sits waiting.

**(2) The pane must have been `Working` at least once since the last feed.** Otherwise a pane created next to
a queue that already has entries drains the whole thing before anyone has typed anything.

**(3) `agent` must not be `None`, equivalently the state must not be `Idle`.** Per foundation property (i),
a resolved `Idle` means *no agent was detected* — a plain shell. Feeding a shell would type a sentence at a
`$` prompt and press return. This is the sharpest hazard in the whole design and it gets its own explicit
refusal with its own message, not a silent skip.

**(4) The `Done` must be hook-reported, unless the user opted into trusting the screen.** This is the answer
to "what happens when state detection is wrong", and it is the most important decision in §8.

Per foundation property (iii), **silence is the classifier's only positive evidence that a turn ended** — an
agent that simply goes quiet is called finished, currently after about two seconds. Auto-feed keyed off that
would fire mid-turn, routinely, on exactly the agents that pause to think or to wait on something that prints
nothing. The concurrent oscillation fix made the signal considerably steadier and pushed the threshold from
roughly 1.3s to roughly 1.8s, and **neither change affects this argument**: the failure is not that the
threshold is too short, it is that there is nothing in the byte stream that distinguishes a thinking agent
from a finished one. A longer threshold trades false feeds for slow ones without ever reaching correct.

Per foundation property (ii), a state the agent reported about itself overrides the screen, and those reports
live on the same struct the queue will. So the queue acts on the signal that comes from the agent's own turn
boundaries, and treats the inferred one as advisory:

```
dockd --auto-feed-trust=reported   (default)  — only a hook-reported Done fires a feed
dockd --auto-feed-trust=screen               — the classifier's Done fires a feed too
```

Under the default, arming auto-feed on a pane whose agent has never reported a state fails **loudly** at
arm time: *"this agent has not reported its state; run `dock hooks --install` in its worktree, or start
dockd with --auto-feed-trust=screen"*. A queue that silently never fires is worse than one that refuses to
be armed.

**None of this depends on a constant that the concurrent work may still tune.** The queue reads whether a
state was reported or inferred; it does not reimplement, wrap, or second-guess the classifier's timings.

**(5) A settle delay.** `QUEUE_SETTLE = 3s` of continuously non-`Working` state before a feed.

This **stacks on top of** the classifier's own hysteresis rather than duplicating it, and is not redundant
with it. The classifier's dwell exists to stop the *roster* flickering — it asks "has this answer held long
enough to be worth showing a person". `QUEUE_SETTLE` asks a different and stricter question: "has this
answer held long enough to be worth acting on unattended". A state good enough to paint is not automatically
good enough to send words to an agent, and the two thresholds must be free to move apart. Under
`--auto-feed-trust=screen` this is what converts a brief misclassification into a non-event; under the
default it is cheap insurance against a hook that fires early.

**(6) A minimum interval.** `QUEUE_MIN_INTERVAL = 10s` between two feeds into the same pane, so even a
detector flapping at tick rate cannot drain a queue.

And the guard that makes the whole thing self-limiting:

**`awaiting_ack`.** From the moment a prompt is fed until that pane is next observed `Working`, nothing else
is fed. If the agent never picked the prompt up — because the feed went somewhere unexpected, or the agent
was not really finished — the queue **stalls visibly** with `holding_because: Some("fed a prompt that the
agent has not started working on")`, rather than piling on. Under a broken detector the worst case is
**one** wrong prompt, ever, until a human looks.

```rust
impl PaneQueue {
    /// Everything auto-feed decides, decided here, from arguments. Returns the prompt to feed,
    /// or None with `holding_because` set to a sentence the runs lane can show verbatim.
    pub fn poll(
        &mut self,
        agent: Option<AgentKind>,
        state: AgentState,
        reported: bool,
        trust: AutoFeedTrust,
        paused: bool,
        now: Instant,
    ) -> Option<String> { … }
}
```

### 8.5 Safety, against the standing decision

The recorded decision: *"Dock's safety invariant is now specific, not absolute: one repository mutation
(`git worktree add`) is permitted; extend it deliberately."* Auto-feed removes the human from the loop, so
the question is whether it extends that mutation.

**It does not, and this falls out of the design rather than being bolted onto it.** A queue entry is text
fed into an agent that is *already running* in a worktree that *already exists*. Auto-feed never constructs
a `DispatchRequest`, never calls `git::ensure_worktree`, never creates a branch, never binds a run, never
creates a pane. The only path that mutates a repository stays exactly where it is: an explicit human dispatch
(`Enter` on a card, or the `dock-dispatch` binary), which creates at most one worktree per card as it does
today.
**An auto-feeding queue of depth sixteen creates zero worktrees.**

That is the honest answer to "an auto-feeding queue could create many worktrees unattended": it structurally
cannot. Any future change that lets a queue entry *launch* rather than *feed* would extend the invariant and
must be argued separately.

The remaining exposure is that Dock puts words in front of an agent unattended, and that agent can do
anything its own permissions allow. Against that:

**Auto-feed is opt-in per pane, and off by default. This is a settled decision, confirmed by the user.**
`auto_feed: false` on every new queue. Queueing is always allowed and is harmless; *auto*-feeding is one
deliberate act — `a` on the runs lane, or `dock queue arm <pane>` — and the failure mode of getting it wrong
is an agent doing work nobody asked for. Requirement 5 asked for auto-feed on idle; it did not ask for it to
be the default, and under the standing invariant that repository mutation is *"specific, not absolute … extend
it deliberately"*, **arming is the deliberate act**. A pane that feeds on its own because nobody turned it off
is not a deliberate extension of anything.

Consequence for the implementation, stated so it is not optimised away later: there is no configuration key,
environment variable, or flag that makes arming the default. `--auto-feed-trust` chooses *which signal* an
armed pane believes (§8.4 guard 4); it does not arm anything.

**Auto-feed is off again after any daemon restart**, and this rule stands alongside the opt-in default rather
than being implied by it. Queues are restored; `auto_feed` is forced to `false`
on load, with `holding_because: Some("auto-feed was disarmed by a restart; arm it again when you are
watching")`. A daemon that comes back from a crash and immediately starts feeding prompts to agents is
precisely the unattended behaviour the standing decision guards against.

**A kill switch at two levels.** `dock queue pause` sets a daemon-wide, persisted `paused` flag that
suppresses every feed regardless of per-pane arming; `Ctrl+B Q` toggles the same. Resuming is explicit.
It is deliberately persisted, so pausing before you walk away survives a restart in the safe direction.

**Depth caps, refusing rather than dropping.** `MAX_QUEUE_DEPTH = 16` per pane, `MAX_QUEUED_TOTAL = 64`
across the daemon (mirroring `MAX_PANES_PER_WORKSPACE`), `MAX_PROMPT_BYTES = 8192` per entry. Exceeding any
of them is an error to the caller, never a silent drop of the oldest — a queue that discards work is worse
than one that says no.

**Any feed failure disarms that pane.** If `pane_input` returns an error, `auto_feed` goes to `false` and
`holding_because` records the daemon's own message. Retrying into a pane whose binding just changed is how
one wrong feed becomes many.

### 8.6 Durability

`<state_dir>/queues/<workspace_id>_<pane_id>.json`, mode `0600` in a `0700` directory, written atomically
(temp + rename + fsync of file and parent), `schema_version: 1`, `#[serde(deny_unknown_fields)]`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurablePaneQueue {
    pub schema_version: u16,
    pub workspace_id: String,
    pub pane_id: String,
    pub next_entry_id: u64,
    pub entries: Vec<DurableQueueEntry>,
}
```

What is **not** persisted, and why: `auto_feed` (§8.5), `awaiting_ack`, `last_state`, `settled_since`,
`last_fed_at`. All of them describe the last few seconds of a process that no longer exists. Restoring them
would let a pre-restart observation authorise a post-restart feed.

The daemon-wide `paused` flag is persisted separately, at `<state_dir>/queues/paused`.

On load: parse failure quarantines the file to `queues-quarantine/` and continues, exactly as
`quarantine_programme_gate` does. A queue whose `(workspace_id, pane_id)` is no longer in the layout is
dropped, so a closed pane does not leave a file forever.

One reconciliation note taken from the gate machinery: gates reconcile against the dispatch receipt because
a crash mid-release could double-launch. **The queue needs no equivalent**, because feeding is not
launching — a prompt fed twice is a duplicate message an agent can be told to ignore, not a duplicate
process. This asymmetry is deliberate and is the reason the queue's restart story is four lines instead of
forty.

### 8.7 What is fed

The entry's prompt verbatim plus `\n`. When a card is enqueued, the client builds the prompt from
`dispatch_prompt(task_id, title)` **plus the card body** — which is the whole point of §3.2's one new field.
The daemon stores and replays text; it does not know a task from a shopping list.

---

## 9. `dock queue`

Hand-parsed in the existing convention: one more arm in `run_noninteractive_legacy` (`main.rs:97`), no clap,
`strip_prefix("--flag=")` for options, positionals filtered by `!starts_with("--")` exactly as
`task_command` (`main.rs:1158`) does.

```
dock queue list [--pane=<id>] [--workspace=<id>]
dock queue add <pane> "<prompt>"      # literal text
dock queue add <pane> --task=<id>     # the card's title + body, resolved here
dock queue remove <pane> <entry-id>
dock queue clear <pane>
dock queue arm <pane>                 # turn auto-feed ON for this pane
dock queue disarm <pane>              # turn it off again
dock queue pause | dock queue resume  # daemon-wide kill switch, independent of arming
```

**`arm` is a separate verb rather than `auto <pane> on|off`, and separate from `add`.** Two reasons, both
following from §8.5. First, queueing and auto-feeding are different acts with different risk: `add` is
harmless, `arm` is the one that lets Dock act without a human present, and a CLI that spells them as one
command with a flag invites arming by habit. Second, `arm` has a **precondition that can fail** — under the
default trust setting a pane whose agent has never reported a state is refused (§8.4 guard 4) — and a verb
that can be refused should not be a modifier on a verb that cannot.

`arm` and `disarm` are per pane. Nothing arms every pane at once; there is no `--all`. A person arming four
agents types four commands, which is proportionate to what they are authorising.

One difference from `dock task` that must be stated in the code rather than discovered: **`dock task` writes
files; `dock queue` talks to the daemon.** `--task=<id>` is resolved client-side via `board::load` against
`--board=` / `$DOCK_BOARD`, exactly as `dock task show` does, and only the resulting text crosses the socket.
`queue_command` opens a `Client` the way `dock-dispatch` does; `run_noninteractive_legacy` gains its first
socket-using arm, which is worth a comment.

Argument parsing goes in a pure `fn parse_queue_command(args: &[String]) -> Result<QueueCommand, String>`,
separate from the socket call, so every usage error is a unit test. `task_command` is not testable today
because parsing and I/O are one function; the new command should not repeat that, and `task_command` is not
being refactored to match — that is a different change.

Not added, deliberately: `dock queue dispatch` as a *new* verb. Dispatching a card into a fresh pane is what
`dock-dispatch` and the TUI's `Enter` already do. The queue's verbs are about text destined for a pane that
already exists.

---

## 10. Defects folded in

**10.1 `needs-input` is invisible.** Fixed by §3.3(c). Test:
`a status the constant does not know is still a column the cursor can reach`.

**10.2 Amp and Copilot dispatch into silence.** `prompt_arguments` (`adapter.rs:176`) returns `Vec::new()`
for Amp, Copilot, Fixture, Generic and Shell, so dispatching a card to Amp opens a pane with no task in it.

The obvious move — enqueue it as the pane's first queue entry — **is wrong, and worth saying why**, because
it is the first thing anyone will reach for. Every guard in §8.4 is about a *running* agent: guard (2)
requires the pane to have been `Working` since the last feed, and guard (4) requires a hook-reported `Done`.
An agent that has just been launched has done neither. The entry would sit there holding, which is a worse
failure than the silence it was meant to fix.

An *opening* prompt is a different problem from a *queued* one, and gets its own small mechanism:
**client-side, one shot, no daemon change.** After a successful launch into a pane whose adapter took no
prompt argument, the client remembers `(run_id, prompt)`. It already receives `Event::AgentStateChanged`
for that run; on the **first** transition into `Done` — the agent's TUI is up and waiting — it sends one
`UiCommand::PaneInput` with the prompt and forgets the pairing. First transition only, so a later turn
never resends. If the run ends first, the pairing is dropped with it.

That is roughly thirty lines, touches no adapter, no protocol and no daemon, and is independent of both B1
and B2 — so it ships as **B3** (§13), not folded into either.

**10.3 Dead code.** Two deletions, both verified safe, and one deferral:

- **`src/app.rs`** is declared in neither `lib.rs` nor `main.rs` and is not compiled. `BoardFixture`,
  `Task` and `TaskState` in `model.rs:4-49,145-173` exist only to serve it; grep confirms no other reference
  anywhere in `src/`, `scripts/` or `README.md`. **Delete all four.**
- **`PickerPurpose::Task`** — the *picker* path is unreachable (nothing assigns
  `self.picker = Some((PickerPurpose::Task, _))`), but the *arm* is load-bearing:
  `take_picked(PickerPurpose::Task, key)` (`dashboard.rs:2422`) is the only place a `TaskDispatch` is
  assembled, called directly by `dispatch_selected_task`. **Move that arm's body into
  `fn task_dispatch_for(&mut self, task_key: &str) -> UiCommand` and drop the enum variant**, so
  `PickerPurpose` has only reachable variants.
- **`src/kanban.rs`** (`KanbanMdAdapter`) — **deferred, not deleted.** It is binary-private (`mod kanban;`
  in `main.rs` only), reachable only through `dock --kanban-dir=`, and shells out to a `kanban-md` binary
  that `board.rs`'s own module doc observes is absent from most machines. Nothing inside the repository
  references either the module or the flag beyond the module itself and the one arm in
  `run_noninteractive_legacy` (`main.rs:160-161`) that reaches it.

  The user has not yet confirmed that nothing *outside* the repository invokes it, so **this project leaves
  both in place.** Deleting a documented entry point on the strength of an in-repo grep is exactly the kind
  of tidy-up that costs someone a script they had forgotten writing.

  **The check that unblocks it**, for the user to run when convenient:

  ```sh
  # Shell history, across the usual shells.
  grep -rn -- '--kanban-dir\|kanban-md\|kbmd' ~/.zsh_history ~/.bash_history ~/.local/share/fish 2>/dev/null

  # Anything scripted, scheduled, or aliased.
  grep -rn -- '--kanban-dir' ~/bin ~/.local/bin ~/.config ~/.zshrc ~/.bashrc ~/.zshenv 2>/dev/null
  crontab -l 2>/dev/null | grep -- '--kanban-dir'

  # Other checkouts and any repo that might shell out to dock.
  grep -rn -- '--kanban-dir' ~/Development 2>/dev/null

  # And whether the binary it wraps is even installed.
  command -v kanban-md kbmd
  ```

  If all of those come back empty, deletion is a four-line change — the module, the `mod kanban;`
  declaration, the `--kanban-dir=` arm, and its `README` mention — and can be done at any time, by
  preference **after B1 ships**, so it is never entangled with the schema migration.

  **Nothing in this design depends on that deletion.** See the note immediately below.

**On the single-writer property.** An earlier draft argued for deleting `kanban.rs` on the grounds that this
project makes `board.rs` the sole writer of task files, and that two writers with different claim semantics
is a question the queue design would otherwise have to answer. **That question does not in fact arise**, and
the property this design needs is narrower than "only one writer exists".

`KanbanMdAdapter` is reachable *only* from `dock --kanban-dir=`, a one-shot non-interactive command that
lists or claims and then exits. It is not reachable from the TUI, from the Board pane, from a dispatch, from
the queue, or from the daemon — which has never read a task file and does not start now (§1). So on every
path this design uses, `board.rs` is the only writer, and that is true whether or not the adapter is
deleted.

The board is *already* a shared artefact — `kanban-md` writes it, editors write it, whoever commits to the
repository writes it — which is precisely why `set_status` rewrites one line byte-preservingly and why
`is_personal` gates every write. A third-party writer is a condition this module was built for, not a new
hazard the queue introduces. Deleting the adapter would tidy the codebase; it would not change a single rule
in this spec.

**10.4 The card↔run pairing is lost on quit.** `task_of` (`dashboard.rs:976`) prefers the daemon's
`external_task_ref` and falls back to `dispatched_tasks`, a client-local map. The fallback exists because
`TerminalLaunchRequest` has no task field, so an unbound dispatch records the pairing nowhere durable — and
the two-lane runs lane depends on that pairing.

Fix: **`TerminalLaunchRequest` gains `#[serde(default)] external_task_ref: String`**, and `terminal_launch`
(`dispatch.rs:462`) puts it into the `RunBinding` it already builds, where it currently hardcodes
`String::new()`. Then `dispatched_tasks` and the fallback in `task_of` are deleted. A bonus falls out:
`runtime.rs:715` exports a non-empty `external_task_ref` as `DOCK_TASK`, so `dock task move` starts working
inside an unbound pane too.

**This widens a deliberately closed shape and must be argued, not slipped in.** The doc comment at
`protocol.rs:36` says the request "cannot carry repository, task, worktree, executable, argument,
environment, or shell data" — and `task` is in that list. What makes it acceptable is that
`external_task_ref` selects nothing: `terminal_launch` still derives both repository and worktree from
`runtime_directory`, and the ref is recorded and echoed, never resolved and never executed. What makes it
*safe* is a validation that keeps it that way: **at most 64 bytes of `[A-Za-z0-9_-]`**, enforced in
`terminal_launch` before it reaches the binding, so it can never become a path. The doc comment is rewritten
to say so:

> Its closed shape deliberately cannot carry repository, worktree, executable, argument, environment, or
> shell data. The task reference it does carry is an opaque bounded label — recorded in the binding and
> exported as `DOCK_TASK`, never resolved, never a path.

The alternative — persist `dispatched_tasks` client-side — duplicates state the daemon already has a field
for, and leaves a second client with no idea what the first dispatched.

**10.5 A failed dispatch leaves the card claimed.** `dispatch_task` (`main.rs:775-783`) moves the card to
`in-progress` *before* the profile check, the worktree creation, and the daemon round trip. A dispatch that
fails at any of those leaves a card claimed by nothing. **Move the claim to after the daemon accepts.** The
claim is still best-effort and still gated on `is_personal`; it just stops being speculative.

---

## 11. Error handling

The codebase's existing convention holds: `Result<_, String>` with a sentence a person can act on, surfaced
through `dashboard.error` (which doubles as the status line), and `(ErrorCode, String)` across the socket.

New failures and what each says:

| Situation | Response |
|---|---|
| Split-as-board past `MAX_PANES_PER_WORKSPACE` | The existing capacity message, unchanged. A board pane is a pane. |
| Launch or respawn into a Board pane | `"that pane is a board; split a terminal pane to launch here"` |
| `layout.json` from a newer schema | Distinct message from corruption (§2.2), so a downgrade is legible in the log. |
| Queue add past depth | `"this pane already holds 16 queued prompts; remove one before adding another"` — refuse, never drop. |
| Queue add past `MAX_PROMPT_BYTES` | `"that prompt is N bytes; the limit is 8192"` |
| Arming auto-feed with no reported state, under the default trust | `"this agent has not reported its state; run \`dock hooks --install\` in its worktree, or start dockd with --auto-feed-trust=screen"` |
| Arming auto-feed on a pane with no agent | `"nothing in that pane looks like an agent; auto-feed would type into a shell"` |
| Auto-feed held | Not an error. `holding_because` carries one sentence, rendered on the runs lane, so a stalled queue explains itself. |
| A feed's `pane_input` fails | Disarm that pane, record the daemon's own message in `holding_because`. |
| A corrupt queue file | Quarantine, continue, one line to stderr — the gate machinery's behaviour exactly. |

`ErrorCode` gains **`QueueRefused`** rather than reusing `GateBlocked`. `GateBlocked` already carries five
distinct meanings (sealed-terminal, queued-direct-dispatch, already-releasing, unauthorised-release,
not-ready); a sixth would make it useless for diagnosis.

---

## 12. What is cut, and why

- **A filesystem-watch crate.** A dozen files and a 500ms mtime fingerprint from a loop that already runs.
- **`Event::BoardChanged`.** Both realtime sources are already live client-side (§4).
- **A YAML parser.** Every field needed is an unindented scalar; `parse()`'s deliberate narrowness is a
  feature and `set_status`'s byte-preserving rewrite depends on nothing reformatting the file.
- **`tags`, `depends_on`, `class`, `created`, `updated` on `BoardTask`.** No requirement routes on any of
  them. `body` alone has a requirement behind it.
- **Card age colouring.** Needs RFC 3339 arithmetic across offsets, so a date dependency, for cosmetics.
- **WIP limits, `classes`, `expedite` bypass.** `config.yml` declares them; Dock has no WIP concept and this
  project is not adding one.
- **Manual reordering inside a column.** The files carry no order field and Dock does not own the
  repository's board. Status plus id is the order.
- **The overlay trait refactor.** §6 — the hazard is removed by a constant, and the refactor belongs in its
  own branch.
- **`LayoutNode::Widget`, and any pane kind beyond `Terminal` and `Board`.** `PaneKind` grows when a second
  use exists, which is the whole argument for choosing it over a node variant.
- **A cross-pane work pool.** Per-agent FIFO is what was asked for.
- **Priorities inside a queue.** FIFO means FIFO. `remove` plus `add` covers reordering at depth 16.
- **Automatic board moves, on completion or on `Blocked`.** Not merely cut but stated as a rule in §3.1:
  Dock may *display* any agent state against a card and must never *write* one. `dispatch_prompt`'s doc
  comment already makes the argument — *"'looks done' is a regex over a screen, and the board is the durable
  record of what happened — moving a real task on a guess is how a board stops being trustworthy"* — and
  §8.4 shows the guess is worse than that comment assumed, because silence alone reads as a finished turn.
  The prompt keeps telling the agent to move its own card, which is a report rather than an inference.
  Confirmed by the user (§16.3), including for the `Blocked` → `needs-input` mapping, which is exact and
  therefore the most tempting case.
- **A `layout.json` downgrade shim.** §2.2.
- **"Convert this pane to a board".** Split and close is enough (§2.4).
- **`dock queue dispatch`.** Dispatching into a fresh pane already has two front doors.
- **The opening prompt as a queue entry.** §10.2 — a starting agent satisfies none of the queue's guards,
  and making it satisfy them would weaken them for every running agent.

---

## 13. Build sequence

Seven steps. Each compiles, each ships, each is reviewable alone.

**Project B1 — the board becomes a pane**

1. **Board data.** `BoardTask.body`; the status union in `BoardView` and `set_status`; §10.5's claim
   reordering. One file plus a few lines of `main.rs`. No new surface, no protocol change.
2. **Dead code.** Delete `app.rs` and `model::{BoardFixture, Task, TaskState}`; fold `PickerPurpose::Task`
   into `task_dispatch_for`. Pure deletion, no behaviour change. Deliberately before the structural work so
   the next steps are not navigating around it. **`kanban.rs` and `--kanban-dir=` stay** (§10.3): no later
   step touches either, and no step is blocked by their presence.
3. **Overlay order.** `OVERLAY_ORDER` + the derivation + the one test. ~40 lines. Independent of everything
   else and worth landing before a ninth surface exists.
4. **`PaneKind`.** Layout schema v2, the six touch points of §2.3, `WorkspaceRequest::Split.kind`,
   `Ctrl+B B`, and the Board pane rendering both lanes from `self.agents` + `self.runs`. Realtime
   (requirement 3) falls out with no further work. Protocol v11 opens here (§7.1).
5. **`external_task_ref` on `TerminalLaunchRequest`** (§10.4), with its bounded-label validation and its
   rewritten doc comment; delete `dispatched_tasks`. The second half of v11.

At the end of B1, requirements 1, 2, 3 and 4 are met. The runs lane shows no queue depth yet, because there
are no queues; the field renders as blank.

**Project B2 — the queue**

6. **`PaneQueue` and `dock queue`.** Protocol v12. The pure module and its tests first, then storage, then
   `queue_tick`, then `Request::Queue` / `Event::QueueChanged`, then the CLI, then the runs-lane arming key.
   Requirements 5 and 6.

Step 6 is larger than steps 1–5 combined and carries the entire safety argument. **It should be a separate
branch with a separate review**, and the runs lane's `queued` / `auto_feed` fields are the only coupling
between the two projects.

**Project B3 — the opening prompt** (independent of both, order-free)

7. **The Amp/Copilot silence fix** (§10.2): a client-side one-shot `PaneInput` on an agent's first `Done`.
   No protocol change, no daemon change, no dependency on the queue.

**Not sequenced: the `kanban.rs` deletion.** It is not a step in B1, B2 or B3, and none of them read, call,
compile against, or route around it. If the user's check in §10.3 comes back empty it becomes a standalone
four-line commit at any later point; if it comes back with a hit, nothing in this plan changes.

---

## 14. Testing

418 inline tests, `#[cfg(test)] mod tests` at file bottom, names that are behavioural sentences. Same
convention throughout; no `tests/` directory is introduced.

The design's testability is not incidental. `BoardView` is terminal-free today and stays that way;
`PaneQueue::poll` is deliberately given no runtime, no PTY and no clock, so **every safety rule in §8.4 and
§8.5 is a unit test with no process in it**. That is the main reason `poll` takes `now: Instant` as an
argument.

**Board (`board.rs`)**
- `a card body is read whole and the front matter scanner is left alone`
- `a status the constant does not know is still a column the cursor can reach`
- `a card whose status is only on the board can still be moved with the arrows`
- `this repositorys own board parses` — the existing test, extended to assert every card has a body

**Layout (`layout.rs`)**
- `a layout written before pane kinds existed loads with every pane a terminal`
- `a layout written by a newer schema is refused with a message that says so`
- `a board pane survives a restart and is still a board`
- `a board pane counts against the workspace pane limit`

**Dispatch (`dispatch.rs`)**
- `a board pane is never given a shell on create split or revive`
- `launching into a board pane is refused with a message about splitting a terminal`
- `a terminal launch records its task reference in the binding`
- `a task reference longer than the label limit is refused`

**Queue (`queue.rs`)** — the centre of gravity
- `a queue does not feed a pane that has never been working`
- `a queue feeds the first entry when a working agent reports it is done`
- `a one frame flicker to done does not feed the queue`
- `a shell pane with no agent is never auto fed`
- `a screen inferred done does not feed the queue under the default trust setting`
- `a queue feeds nothing more until the agent is seen working again`
- `two feeds into the same pane are at least ten seconds apart`
- `a paused daemon feeds nothing however armed a pane is`
- `a queue with entries feeds nothing until its pane is armed`
- `auto feed is off after a restart even if it was armed before`
- `a failed feed disarms the pane and says why`
- `a queue refuses a seventeenth entry rather than dropping the first`
- `a prompt over the byte limit is refused rather than truncated`
- `a queue that is holding explains itself in one sentence`

**Storage (`storage.rs`)**
- `a queue file written by an unknown schema is quarantined rather than obeyed`
- `a queue whose pane is gone is dropped on load`

**Dashboard (`dashboard.rs`)**
- `every open overlay takes keys in the same order it is drawn`
- `the runs lane shows one row per live agent and none for a shell`
- `a backlog card bound to a live run is badged with that runs state`

**Opening prompt (`dashboard.rs`, B3)**
- `an adapter that takes no prompt argument is sent its task on the agents first done`
- `a second turn does not resend the opening prompt`
- `an opening prompt is dropped when its run ends before the agent is ready`

**CLI (`main.rs`)**
- `dock queue add without a prompt explains the shape of the command`
- `dock queue arm names the hooks command when the agent has never reported a state`
- `dock queue rejects a verb it does not have`
- against `parse_queue_command`, which is pure for exactly this reason.

---

## 15. Acceptance

- `Ctrl+B B` splits a board into the canvas; it takes focus, resizes with `Ctrl+B` arrows, and closes with
  `Ctrl+B x` like any other pane, and it never spawns a shell.
- That board survives quitting and reopening the TUI, and a daemon restart.
- The runs lane shows every live agent pane with its state, and the state changes on screen within one frame
  of the agent changing, with no keypress.
- A `needs-input` card is visible and reachable with `<` and `>`.
- Dispatching a card sends the agent the card's body, not just its title.
- `dock queue add <pane> --task=7` from another terminal appears in the open board pane without a refresh.
- A queue with entries in it feeds **nothing** until the pane is armed: a fresh pane's `auto_feed` is false,
  and no setting, flag or environment variable makes it true.
- `dock queue arm <pane>` on a hooked agent succeeds; the same command on a pane whose agent has never
  reported a state is **refused with the message naming `dock hooks --install`**, not silently accepted.
- Once armed, an agent finishing its turn receives the next queued prompt within a few seconds;
  `dock queue disarm <pane>` stops it again.
- `dock queue pause` stops every feed daemon-wide regardless of arming, and survives a restart.
- After a daemon restart every pane is unarmed, whatever it was before, and says so.
- Dispatching a card to Amp puts the task in front of it instead of opening a silent pane (B3).
- `cargo test` passes with no test removed.

---

## 16. Decisions taken

The three questions this spec opened have been answered by the user. **No open question remains**, and
nothing in this document is deferred to the reader. They are recorded here because each one is a decision a
future reader will otherwise re-litigate, and because two of them are the kind that look like oversights
once the reasoning is out of sight.

**1. Auto-feed is opt-in per pane, off by default. Settled.** Requirement 5 asked for auto-feed on idle
without saying whether it should be on by default; under the standing invariant that repository mutation is
*"specific, not absolute … extend it deliberately"*, **arming is the deliberate act**. §8.5 states this as a
rule with a consequence: no configuration key, flag, or environment variable makes arming the default, and
`--auto-feed-trust` chooses only *which signal* an already-armed pane believes. Every other guard — the
forced disarm after restart, the depth caps, the settle delay, the minimum interval, `awaiting_ack`, the
two-level kill switch — stands unchanged and independently.

**2. `src/kanban.rs` and `--kanban-dir=` stay, pending a check only the user can run. Deferred, not
rejected.** In-repo grep is clean, but the user has not yet confirmed that no script, alias, cron entry, or
other checkout invokes `dock --kanban-dir=`. §10.3 carries the exact commands. The important part is
structural rather than procedural: **no step of B1, B2 or B3 reads, calls, compiles against, or routes
around either**, and the single-writer property the queue actually needs holds regardless of whether the
adapter exists (§10.3, "On the single-writer property"). This is a tidy-up waiting on a fact, not a blocked
dependency — if the check comes back with a hit, nothing in this plan changes.

**3. `needs-input` means "the agent is blocked and wants you" — and Dock still must not write it. Settled.**
The mapping onto `AgentState::Blocked` is exact, which is what makes the temptation real. §3.1 makes the
rule explicit: **the runs lane and the card badge may display any agent state, including `Blocked`; nothing
in this design writes a status to a card file except an explicit human act or the dispatch claim in §10.5.**
Dock measures, the agent reports, and a status change stays the agent's act or the user's — never Dock's
inference. A badge is derived and vanishes with its run; a status write is durable and outlives whatever
misread produced it.

This is **independent of** §3.3's column fix, and the two should not be confused when reviewing. The union
fix is what makes a `needs-input` card visible and reachable at all — today such a card is invisible in the
column view and unreachable by `<`/`>` **even when a human moved it there by hand**. That bug wants fixing
whether or not anything ever moves a card automatically, and fixing it is not a step towards doing so.

---

## 17. Risks accepted

- **A downgrade loses one workspace topology** (§2.2). No shim; the failure is legible in the log.
- **`TerminalLaunchRequest` gains a field a doc comment previously excluded** (§10.4). Bounded to 64 bytes of
  `[A-Za-z0-9_-]`, never resolved, never a path; the comment is rewritten to say exactly that.
- **Auto-feed acts on a signal that can be wrong.** The residual exposure after every guard in §8.4 is one
  wrong prompt per pane, once, until a human looks — because `awaiting_ack` blocks everything further until
  the pane is next seen `Working`. It is not zero, and the design does not claim it is.
- **Board-file changes are noticed within ~500ms, not instantly** (§4). A human-scale artefact polled at
  human scale; the manual refresh key remains.
- **The agent-state classifier is being tuned concurrently.** Every claim §8 rests on is cited as behaviour
  rather than as a constant or a line number (see "Verified foundation"), and the queue reads only *whether*
  a state was reported or inferred — it does not reimplement, wrap, or second-guess the classifier's
  timings. Re-tuning `WORKING_SILENCE`, `STATE_DWELL`, `SUSTAINED_OUTPUT` or `BURST_GAP` cannot invalidate
  the safety argument, because that argument turns on the absence of evidence in the byte stream rather than
  on any threshold applied to it.
