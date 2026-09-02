# Dock — the receipts layer

*Design specification. 2026-09-02.*

## Decision

Dock is the **verifiable record of what coding agents actually did**. It owns the PTYs
because owning them is what makes the record unforgeable, and it owns nothing else.

The multiplexer is substrate. The product is the receipt.

Runtime parity with `herdr` is an explicit **non-goal** — see *Positioning*. This is a
standing decision, not a backlog item, and it exists in writing so that "we should add
remote attach / plugins / a detection service" is answered by this document rather than
re-litigated each quarter.

## Positioning

`herdr` (Apache-2.0, `brew install herdr`) solves the same runtime problem in the same
language and is ahead on it: remote SSH attach, a plugin marketplace, a Windows beta,
named agents with `agent prompt --wait --until blocked`, and detection shipped as
versioned data fetched from `herdr.dev/agent-detection/index.toml`.

It also stops, deliberately, at the runtime boundary. Its own agent skill states that
`unknown` "does not prove completion". It has no task board, no handoff, no git evidence,
no worktree-per-task binding, no dependency gates.

That boundary is where Dock starts.

| | herdr | Dock |
|---|---|---|
| Server/client, detach, persistence | yes | yes |
| Agent state (working/blocked/done/idle) | yes | yes |
| Remote attach, plugins, Windows | yes | **not pursued** |
| Detection as a fetched service | yes | **not pursued** |
| Task board bound to worktrees | no | yes |
| Handoff: claimed vs. observed | no | yes |
| **Checks Dock ran and witnessed** | no | **this spec** |
| **A verdict derived from evidence** | no | **this spec** |

Dock competes on evidence, not on terminals.

## Scope

**In scope.** A task that already exists → dispatch to a bound worktree → agent works →
checks run → a receipt with a verdict → a human decision. The Split Spine UI that keeps
delivery state and live panes visible simultaneously. One manifest per supported agent.
A theme system in which every surface is painted from the same tokens.

**Out of scope.** Intake: Dock does not write specs, PRDs, or decompose requirements —
that needs an LLM and Dock has none. Integration: Dock does not merge, push, or open pull
requests. Multi-repo programmes, remote attach, plugins, a detection CDN, and Windows are
deferred; the programme gating code stays in the tree, unwired.

---

## 1. The receipt

A receipt is an immutable record written when a run reaches a terminal state. It has four
authored parts and one derived one. **Nothing may be written across columns.**

| Part | Written by | Contents |
|---|---|---|
| **claimed** | the agent | summary text; the *names* of checks it asks Dock to run |
| **observed** | git + hook payloads | base SHA → head SHA, diffstat, changed files, untracked files, tool calls and files touched with timestamps |
| **witnessed** | Dock | per check: command, exit code, duration, SHA before, SHA after, output tail, outcome |
| **decided** | the human | accepted / changes-requested, timestamp, optional note |
| *verdict* | derived | a pure function of the four above plus a versioned rule set |

An agent can never write to *witnessed*. Dock can never write to *decided*. That is the
whole trust model and it belongs in the README in those words.

**Storage.** `.dock/local/receipts/<run_id>.json`, mode 0600, append-only,
schema-versioned, parsed with `deny_unknown_fields`, using the corrupt-file quarantine
path `storage.rs` already implements. No terminal transcript. No command output beyond the
capped tail. No credentials.

**The ledger is the receipt's index.** A ring buffer of `(run_id, state, instant)`
appended on `Event::AgentStateChanged` — which the daemon already emits at the exact
transition edge and currently discards — provides the time axis. Receipts are the events
on it. The "draw the day" view and the verification store are one subsystem, not two.

## 2. The verdict

The verdict makes approval fast without making it thoughtless. It is **arithmetic over
evidence, not judgment of it**: a fixed rule set over facts Dock already holds, producing
a result the reader can re-derive by hand from the same lines. There is no model involved,
and there never will be (see *Refusals*).

### Rules

Each rule fires a **finding** naming the fact that produced it. The verdict is the maximum
severity present.

| Rule | Fires when | Severity |
|---|---|---|
| `check_failed` | a declared check exited non-zero | failed |
| `check_stale` | a check's `sha_after` differs from head SHA — files changed after it went green | look |
| `check_unwitnessed` | a declared check could not run: unknown name, timeout, spawn error, unpermitted env | look |
| `check_mutated_worktree` | tracked files differ between the check's before and after states | look |
| `no_checks_declared` | the run declared zero checks | look |
| `empty_diff` | a non-empty claim, but base == head and nothing untracked | look |
| `peer_conflict` | a file in this diff is open in another live run | look |
| `destructive_command` | a hook payload shows an executed `rm -rf`, `git reset --hard`, `git push`, or `git clean` | look |
| `sensitive_new_file` | an untracked file matching `.env*`, `*.pem`, `id_*`, or larger than 1 MB | look |

`check_stale` and `peer_conflict` are only computable because Dock owns the PTYs and reads
hook payloads. They are the two rules a screen-reading tool cannot implement at any regex
quality.

### Verdicts

    ✓ clear     every declared check witnessed green at head, no findings
    ! look      one or more findings, each named, none fatal
    ✗ failed    a declared check ran and exited non-zero

Three shapes, distinguishable without colour, legible in a compressed screenshot.

### `clear` is earned by the repository

A run that declared no checks fires `no_checks_declared` and therefore **can never be
`clear`**. Dock does not hand out a green tick for work it did not witness.

This is the load-bearing rule. It inverts the incentive correctly — the way to earn fast
approvals is to write `.dock/checks.toml` — and it defuses the rubber-stamp failure mode,
because the verdict cannot drift toward "usually clear" when clear is expensive.

### Constraints on the verdict

- **Never decides.** It never accepts, never moves a card, never releases a gate. It ranks
  and explains; the human presses the key. Same family as *never auto-answer a permission
  prompt*.
- **Never scores.** No percentage, grade, streak, target, or comparison between agents or
  across days. Identical to the constraint the ledger is bound by, for the same reason: the
  moment it grades the user it becomes a thing people turn off.
- **Always explains.** `dock verdict explain <run>` prints rule by rule with the fact each
  rule read. A verdict that cannot be re-derived by hand does not ship.
- **Never changes silently.** The receipt stores the verdict, the findings, *and* the
  rule-set version. An old receipt shows the verdict it was given; `dock verdict recheck`
  reports where today's rules disagree.

### Bulk accept

`A` in the review queue accepts every `✓`, after listing exactly what it is about to accept
and how many. `!` and `✗` are never bulk-acceptable at any count.

This is safe here and nowhere else: Dock's accept is a recorded decision plus a card status
rewrite in a markdown file. No merge, no push, nothing irreversible.

## 3. The check runner

### The honest statement of what changes

Dock already spawns processes: `git`, `ps`, `/bin/sh`, `delta`, and clipboard helpers. The
invariant was never "Dock spawns nothing"; it was **"Dock spawns only tools it chose, with
arguments it wrote."**

The new invariant, stated precisely:

> Dock may execute a command **declared by the repository or the user** — never one an
> agent composed — in the run's bound worktree, at a pinned SHA, under a cleared and
> allowlisted environment. Dock still never stages, commits, rebases, merges, pushes, or
> removes a worktree. `git worktree add` remains the sole Dock-authored repository
> mutation.

### Declaration

```toml
# .dock/checks.toml — committed to the repository
[check.test]
run     = ["cargo", "test", "--locked"]
timeout = "10m"

[check.lint]
run     = ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
timeout = "5m"
```

An agent references a **name**: `dock handoff "added retry" --check=test --check=lint`. An
unknown name is recorded `unwitnessed` with the reason *"no check named `typo` in
.dock/checks.toml"*. An agent cannot cause Dock to run a string it composed; the whole
containment argument is one map lookup.

`run` is an **argv array, never a shell string**. No `sh -c`, therefore no interpolation,
no glob, no chaining. A pipeline is written as a committed script and declared as
`run = ["./scripts/ci.sh"]`, which makes it a reviewable file rather than a config line.

### Runner contract

| | |
|---|---|
| Where | the run's bound worktree; `cwd` set explicitly; never the primary checkout |
| When | automatically at handoff, or on `r` from the receipt rail. Never on a screen heuristic, never on a timer |
| Pinning | head SHA and dirty state captured before spawn and after exit; both stored |
| Environment | `apply_child_environment` — cleared, allowlisted, plus this run's `DOCK_*` |
| Bounds | per-check timeout (default 10m); output tail capped at 200 lines / 64 KB; one check at a time per run |
| Capacity | a global lane limit, default `min(4, cores / 2)`, so eight simultaneous handoffs do not fork-bomb the machine |
| Signals | on timeout, SIGTERM to the process group, SIGKILL after 5s. Dock owns the group, as it does for a pane |
| Result | `exit_code`, `duration`, `sha_before`, `sha_after`, `tail`, `outcome ∈ {passed, failed, unwitnessed}` |

`checks.auto = false` in `dock.toml` disables automatic running; `r` still works.

### Secrets: the repository may name, only the user may permit

The environment allowlist strips credential-shaped variables, so a check needing
`NPM_TOKEN` would fail. The fix must not be "repositories may request environment
variables" — that is a committed file asking for the reader's secrets.

```toml
# ~/.config/dock/checks.toml — user-level, never committed
[permit]
env = ["NPM_TOKEN"]
```

The repository declares `needs_env = ["NPM_TOKEN"]`; Dock passes it only if the user
config permits that name. Unpermitted is never silent — the finding reads *"`NPM_TOKEN`
was requested by .dock/checks.toml and is not permitted in your user config."*

### Runner refusals

- Never a command an agent composed. Named checks only.
- Never a shell. argv arrays only.
- Never interactive: stdin is `/dev/null`. A check that prompts hangs, times out, and is
  recorded `unwitnessed`. It does not get the keyboard.
- Never on the primary checkout. A run without a bound worktree cannot be witnessed.
- Never automatic acceptance. A green check is evidence, not a decision.

## 4. The Split Spine

Dock opens on delivery state and live terminals at once. A permanent left spine carries
tasks and agents with state and wait clocks; the right canvas is live panes; a bottom rail
carries the receipt for whatever is selected. `Ctrl+B z` zooms either side to full.

```
╭ d·ock ────────────────────────────────── main · 3 agents · 1 needs you ─╮
│ IN PROGRESS  3    │ ◐ claude · #14 · wire the parser                    │
│ ◆ #14 parser 4m12s│                                                     │
│ ◐ #9  retry   18m │ > running cargo test --lib                          │
│ ◐ #22 docs     2m │   test detect::osc_title ... ok                     │
│                   │                                                     │
│ REVIEW       3    │                                                     │
│ ✗ #2  add tests   ├─────────────────────────────────────────────────────┤
│ ! #5  retry path  │ ◐ codex · #9 · retry path                           │
│ ✓ #8  bump deps   │                                                     │
│                   │ > sed -i 's/retry/backoff/' src/runtime.rs          │
│ DONE today   5    │                                                     │
├───────────────────┴─────────────────────────────────────────────────────┤
│ ! #5  retry path · codex · ../dock-5 @ a3f19c2 · +120 −33 · 7 files     │
│   ✓ test     cargo test          exit 0  12.4s  763 passed   @ 8c21ef0  │
│   ! stale    src/runtime.rs edited 14:58, after test ran at 14:52       │
│   ! peer     src/runtime.rs also open in claude #14 (6m)                │
│   claimed "added retry with backoff"                                    │
│   r re-run checks · o open pane · d diff · Enter accept · c changes     │
╰─────────────────────────────────────────────────────────────────────────╯
```

The review queue sorts by verdict severity — `✗` and `!` first, `✓` last — so "what needs
me" is a glance rather than a read. `r` re-runs the checks in place, which is what makes a
`check_stale` finding actionable rather than merely accusatory.

The spine is 30 columns. A task row is glyph + id + roughly twelve characters + clock. That
budget is deliberate: it is what stops the spine becoming a second sidebar full of prose.

## 5. Agent manifests

Today there are 23 variants in `AgentKind`, 4 real agents in `AdapterId`, and no conversion
between the enums anywhere in the tree. One agent's facts live in three places — a `match`
in `adapter.rs`, a `match` in `detect/mod.rs`, and a JSON manifest carrying only three
regex arrays — two of which need a recompile to change.

Collapse them into one file per agent, which is the same file a user can already override.

```json
{ "schema": 3,
  "id": "droid", "label": "droid", "executables": ["droid"],
  "launch":  { "argv": [], "prompt": "positional" },
  "resume":  { "argv": ["--continue"], "verified": "2026-09-02 · droid --help" },
  "detect":  { "blocked": ["(?i)allow this\\?"], "awaiting": ["^\\s*›\\s*$"] },
  "capture": { "model": "model:\\s*(\\S+)" } }
```

- **Capability is derived, not declared.** `resume` present means resumable. Absent means
  `Ctrl+B a` refuses *and names the missing field* — a general honest answer rather than a
  hardcoded special case.
- **`verified` is provenance.** The warning currently written as a comment in `adapter.rs`
  — a wrong resume flag does not fail loudly, it starts a new session while the user
  believes they resumed one — becomes a field. An unverified agent can be contributed and
  detected while Dock still refuses to resume it.
- **`capture` closes a real hole.** Manifests compile to `RegexSet`, which cannot capture,
  so Dock runs regexes over every screen sixty times a second and can only answer yes/no.
  One `Regex` per named value extracts `model`, `tokens`, or `cost` from a footer Dock
  already holds.

**No detection CDN.** Fetching manifests would mean executing `launch.argv` from a
downloaded file, which is a supply-chain surface, and would trade away "one binary, no
network, no accounts" — which `herdr`, as a service, structurally cannot claim. Manifests
ship in the binary; `~/.config/dock/agents/*.json` overrides any of them; new agents arrive
by pull request.

Two bugs make the current promise false and are fixed here: `manifest::resolve` caches into
a process-lifetime `OnceLock` and never invalidates, so `dock detect claude` prints rules
the running daemon is not using — a diagnostic that actively lies; and a broken override
falls back to built-ins silently on the hot path, so a typo is undetectable from inside the
product.

## 6. The visual system

The principle is Omarchy's, and it is not a colour: **omakase**. One theme propagates to
every surface, and the curation is the product. Dock ships four themes — two dark, two
light — each a data file that must pass the same test suite. `theme = "auto"` reads the
host background via OSC 11. No arbitrary per-token user colours in this release.

### Tokens

`theme.rs` opens with *"No colour may be hardcoded outside this module"*, and the board
violates it by painting from raw ANSI-256 indices out of `board_config.rs`. Three
additions:

| Token | Rationale |
|---|---|
| `age: [Color; 5]` | The board's staleness ramp, descending toward `muted`. A stale card is not urgent, it is forgotten. Today it goes red and collides with `blocked` on the one surface a stranger screenshots. |
| `passed` | The verdict's green. `look` reuses `working` and `failed` reuses `blocked`; only "witnessed green" is a meaning the palette lacks. |

**No `border_pane` token.** An earlier draft of this spec proposed one, on the strength of
the 2026-08-30 review measuring `cool.border` at 1.32:1 against `surface` — "not dim, gone".
Commit `a99d44a` already fixed it: `cool.border` is now `Rgb(70,82,90)`, measuring **2.26:1**
on `surface` and 2.04:1 on `panel`, and `warm.border` measures 2.25:1. Both clear the 2:1
floor a structural line needs. Splitting the token would add a concept to buy a property the
palette already has. What is missing is only a **test**, so it cannot silently regress again.

### A shipped palette is currently broken

`the_agent_states_stay_far_apart` requires every state colour to sit ≥ 60 RGB units from
every other and from the accent — and it runs against `Theme::cool()` only. Measured against
`Theme::warm()`:

    warm.working (226,184,96)  vs  warm.accent (232,168,88)  =  18.9      FAILS

This is not hypothetical drift. `theme.rs`'s own doc comment names it — *"in `warm` the
accent and `working` are nearly the same colour"* — as the reason `cool` was written. `warm`
was never fixed, and no test covers it, so Dock ships a theme in which "an agent is working"
is indistinguishable from ordinary chrome.

The fix separates by **value within the amber hue**, which is how `cool` solves the identical
problem (its `working` sits 70.8 from its accent at the same hue):

    warm.working := Rgb(168, 120, 56)
      accent 86.2 · blocked 70.7 · done 129.6 · idle 74.6 · contrast 4.47:1 on both grounds

Row 0 parameterises every palette test over every shipped theme, so the third and fourth
palettes cannot ship broken the way the second did.

### Vocabulary

A `const` block beside the palette, so the typography is as enforceable as the colour: one
cursor `›`, one close `×`, one ellipsis `…`, U+2212 for every numeric minus, ` · ` as the
separator. Today `×`, `✘`, and `✗` all mean close, and the git overlay and review queue
typeset the same three numbers two different ways.

### Overlay tiers

Eight overlays currently share one visual weight, so a one-line rename prompt carries the
authority of a full diff.

- **Prompt** (rename, confirm) — into the footer, which already grows on demand. This
  deletes a whole class of centred box.
- **Panel** (picker, launch) — centred, as today.
- **Reader** (help, diff, ledger) — docked full-height right, half-width. A diff is read
  *against* the code, not on top of it.
- **Scrim** under Panel and Reader: walk the buffer behind and set every cell's `fg` to
  `border`. Roughly 40k cell mutations, on the order of 0.05 ms. It is the cheapest change
  that makes a TUI look designed. The existing "no shadow, no animation" rule stays in
  force for the context menu, which is drawn over content actively being read.

### Motion

Exactly two things move. A **one-shot on entry to `needs you`** — about 400 ms, three
steps, then settled forever; a one-shot is a notification, a loop is a distraction. And the
**wait clock at 1 Hz**, which changes because the world changed. Nothing else animates, and
nothing that reflows animates at all.

### Light palette

Designed, not inverted: `blocked` at `Rgb(226,106,94)` on white is a pastel that fails the
3:1 floor the suite already enforces. `every_token_is_legible_on_both_surfaces` currently
tests only `cool()`, so `warm` is drifting unchecked. **Parameterise that test over all
palettes before adding a third**, or the third ships broken.

### Speed, measured

The standing rule is to measure before and after every feature against a 16.7 ms budget,
using the harness the 2026-08-23 audit left rather than writing a new one each time. That
harness is `#[ignore]`d tests run deliberately, not `benches/` — there is no criterion
dependency and none is wanted, because an ignored test can reach private render internals
that an external bench target cannot.

Fourteen exist and they are in good health, covering render (whole frame and a breakdown by
the work it does), the daemon hot path, a subscriber whose client has gone, the queue tick
over every run, classification of one pane screen, board load, copy-mode freeze, pane byte
log, history seeding and paging, and press-and-drag. Run them with:

    cargo test --release --lib -- --ignored --nocapture measure_
    cargo test --release --lib -- --ignored --nocapture render_measurement

Row 0 therefore adds nothing to the harness. It only extends it where this work creates new
cost:

- `render_measurement_of_a_busy_dashboard_at_three_terminal_sizes` gains the Split Spine and
  receipt rail once row 3 lands, so the spine's cost is separable from the canvas's.
- A new `measure_what_running_a_declared_check_costs` lands with row 2: spawn-to-result
  overhead for a trivial check, so the cost Dock adds is separable from the cost of the
  check itself.

Both are written in the row that creates the surface they measure, not ahead of it.

Measurements are reported as the **fastest of several rounds, never the mean** — this
machine's mean swings around 40% between identical runs under load, which has already
hidden a real 25% change once.

**One question is reopened once that harness exists, and not before.** The event loop
repaints unconditionally every 16 ms whether or not anything changed — roughly 8.6% of a
core, burned continuously, on a laptop. Conditional repaint was declined earlier with
reasoning and that decision stands until there is a measurement; with a bench harness it
stops being an argument and becomes a number.

## 7. Architecture

A Cargo workspace, split along seams the code already has. The split is not tidiness: it is
how the new safety claim becomes checkable.

```
dock-model     protocol, board, storage, Receipt, Finding, Verdict, checks.toml
dock-git       read-only git facts: SHA, diffstat, dirty, worktrees
dock-pty       terminal, vt, keys, PTY spawn and process groups
dock-detect    manifests, heuristics, state classification
dock-receipt   checks: the only crate that runs argv it did not author
dock-ui        theme, widgets, the Split Spine
dock / dockd   binaries
```

Four crates spawn processes and the distinction between them is the safety claim, so state
it exactly rather than as "only one crate spawns anything", which is false:

| Crate | Spawns | Argv author |
|---|---|---|
| `dock-git` | `git`, `delta` | Dock, fixed at compile time |
| `dock-detect` | `ps` | Dock, fixed at compile time |
| `dock-pty` | `$SHELL`, agent executables | Dock, from a manifest field, into a PTY it owns |
| `dock-receipt` | declared checks | **the repository or the user**, read from `checks.toml` |

So the enforceable rule is: **`dock-receipt` is the only crate that executes argv Dock did
not write**, and `dock-model` and `dock-ui` may not spawn at all. Both halves are lints —
a `disallowed-methods` entry on `std::process::Command::new` in `dock-model` and `dock-ui`,
and a review-gated allowlist comment required at each construction site in the other four.
The safety claim then fails the build rather than a code review, which is what makes it
trustworthy to a stranger reading the README.

## 8. Kept, rewritten, cut

**Ported unchanged.** `queue.rs` (28 tests, clock-free by construction), `theme.rs`,
`detect/`, `terminal/vt.rs` and `keys.rs`, `git.rs`, `protocol.rs`, `storage.rs`,
`board.rs`, `copy.rs`, `clipboard.rs`, `keymap.rs`. This is the accumulated knowledge —
settled against real PTY bytes — and none of it is retyped.

**Rewritten.** `dashboard.rs`: 19,294 lines, 565 functions, a 6,500-line `impl Dashboard`.
It becomes `dock-ui`, one file per surface. The home screen is changing anyway; this is the
only moment the dissolution costs nothing extra.

**New.** `dock-receipt`.

**Cut before release.** `AdapterCapabilities` — six booleans, all false for all seven
adapters, which makes `ProviderState` permanently `Unknown` in every snapshot and every
receipt on the wire. And `LifecycleOperation::Attach` / `Focus`, which are accepted and
return a snapshot without doing anything. Inert scaffolding on a public protocol is a
promise that will be held against you; after a release, removing it costs a version bump.

## 9. Delivery order

Each row ships and is usable before the next starts.

| | Ships | Why here |
|---|---|---|
| 0 | Palette tests parameterised over every shipped theme; `warm.working` fixed; `border` floor enforced; vocabulary consts | A shipped palette is currently broken and no test catches it |
| 1 | Workspace split; `dashboard.rs` dissolved into `dock-ui` | Prerequisite for 2–5. No user-visible change; all 843 tests stay green |
| 2 | `dock-receipt`: `checks.toml`, runner, receipt store, nine rules, verdict | The product |
| 3 | Split Spine and receipt rail; overlay tiers; scrim; `border_pane`; `age` ramp | The product becomes visible |
| 4 | One manifest per agent; derived capability; cache invalidation | "All agents" becomes true rather than a roster count |
| 5 | Ledger; `dock peers`; light palette; `theme = "auto"` | The screenshot, and the data the last two rules need |

Row 5 supplies `peer_conflict`. Until it lands, that rule is inert and says so rather than
silently never firing.

## 10. Refusals, in writing

A written refusal is a feature. These are policy, not backlog.

- **Never auto-answer a permission prompt.** This will be the most-requested feature Dock
  ever receives. The moment Dock decides on the user's behalf, "needs you" stops meaning
  anything and the attention model collapses.
- **Never run a command an agent composed.** Named, repository-declared checks only.
- **Never summarise or judge with a model.** No LLM in the multiplexer. The verdict is
  arithmetic over evidence and must be re-derivable by hand. This is also what keeps Dock
  free, offline, and installable in one command.
- **Never accept, merge, push, or move a task on Dock's own judgment.** The verdict ranks;
  the human decides.
- **Never turn the ledger or the verdict into a score.** No percentage, target, streak, or
  comparison.
- **No telemetry, no accounts, no phone-home, ever.** There is none today — no exporter, no
  counters, no log sink. Saying so is worth more than merely having it.
- **Never be a chat UI.** Every agent already has one. Dock's job is the space between
  them.
- **No web dashboard.** It requires a listening TCP socket, therefore auth, therefore
  accounts, therefore a service — and the sentence "Dock touches your repository in exactly
  one way" stops being checkable by reading a README. If remote *visibility* is wanted, the
  answer is rendering a snapshot to a file the user can put wherever they like. Push, never
  pull; no listener.

## 11. Testing

- **The verdict is a pure function**, so the rule set is table-tested: one case per rule
  firing, one per rule not firing, one for severity precedence, one for "no checks declared
  can never be clear".
- **The runner is tested against real processes** — exit codes, timeout and SIGKILL
  escalation, output-tail truncation, `cwd` binding, and an assertion that a check cannot
  see a credential-shaped variable, extending the existing environment-allowlist test.
- **Containment is tested structurally**: `std::process::Command` is denied outright in
  `dock-model` and `dock-ui`, and every construction site in the other four crates carries
  a required allowlist comment naming who authored the argv. The safety claim fails the
  build rather than a review. One test asserts that a check name absent from `checks.toml`
  produces `unwitnessed` and never a spawn.
- **Contrast and vocabulary are tested**, parameterised across every shipped palette:
  `border_pane` ≥ 2:1, the four state glyphs distinct as characters, the three verdict
  glyphs distinct as characters, and no colour constructed outside `theme.rs`.
- **Benchmarks gate the visual work**: frame time recorded before and after each row of the
  delivery order, against a 16.7 ms budget.

## 12. Success criteria

1. A run finishes and its receipt shows a command Dock ran, an exit code Dock observed, and
   the SHA it ran at — none of which the agent could have written.
2. A stale green is caught: an agent edits after its tests pass, and the verdict says so.
3. A repository with no `.dock/checks.toml` is never shown a `✓`.
4. Every claim in the README about agent support is derivable from a manifest file rather
   than from a roster count.
5. Frame time at 400×100 with twelve panes is measured, recorded, and inside budget.
