# Responsive selection, pointer menus, and a cool palette

Seven changes, one sequence. Four are repairs to things that already exist and feel
wrong; three add surfaces Dock does not have. They ship as one branch because they
touch the same three files — `dashboard.rs`, `board.rs`, `theme.rs` — and splitting
them would mean three rounds of conflict in the largest file in the repository.

## The measurement that shaped this

The obvious theory about the selection lag was that rendering costs too much. It does
not. `render_breakdown_of_a_busy_dashboard_by_the_work_it_does`, on a dashboard with
twelve panes, forty-eight agents and a twenty-four-card board:

| terminal | whole frame | share of a 16.7 ms frame |
|---|---|---|
| 80×24 | 0.091 ms | 0.5 % |
| 200×50 | 0.327 ms | 2.0 % |
| 400×100 | 1.438 ms | 8.6 % |

The event loop repaints unconditionally at ~60fps — `main.rs:588`, reached by the
`continue` at `main.rs:625` when the poll times out — and that turns out to be
affordable at every size Dock is used at. **Conditional repainting is therefore out of
scope.** It would buy a fraction of a percent of a frame and cost a class of bug where
a missed dirty-flag freezes the UI. That is the wrong trade and this document records
it as deliberately declined.

The lag is latency, not throughput, and it is concentrated in two places.

**A press that starts a selection can block on the daemon.** `Dashboard::mouse`
returns `UiCommand::Request(Workspace(Focus))` for a press in an unfocused pane
(`dashboard.rs:5159`). `main.rs` handles that by painting, then *blocking* on
`client.request`, then calling `refresh`, which blocks on three more round trips —
`Workspace(Inspect)`, `Inspect`, `Queue(Inspect)`. The runtime inspect can shell out
to `ps` (`runtime.rs:667`). So the first click into a pane you were not already
focused on pays four synchronous round trips and possibly a process spawn before the
drag can begin. Someone already found and fixed exactly this for the *already-focused*
case; the comment at `dashboard.rs:5145` describes the symptom in the user's own
words. The unfocused case was left.

**The first drag of a gesture copies the whole screen.** `drag_selection` calls
`PaneScreen::snapshot` (`vt.rs:231`), which clones a `vt100::Screen` — grid *and*
scrollback. At the daemon's default 2000 retained rows (`dockd.rs:13`) and a
120-column pane that is roughly 7.7 MB, allocated and copied on the frame where the
pointer first moves.

Everything below is aimed at those two, and at six other things that are simply
missing or wrong.

## 1 · A press must never wait for the daemon

Add a fire-and-forget command variant beside the existing ones:

```rust
enum UiCommand {
    None,
    Request(Box<Request>),   // blocking; the answer is the product
    Send(Box<Request>),      // new: painted, sent, not waited on
    Requests(Vec<Request>),
    PaneInput(Vec<u8>),
    Quit,
}
```

`Send` paints the optimistic local result and hands the request to `client.send`,
which `PaneResize` already uses for precisely this reason (`main.rs:594` explains it).
It does **not** call `refresh` — focus was already applied locally at
`dashboard.rs:5159`, and the daemon's own event stream is what reconciles the rest.

Focus-on-press becomes `Send`. Nothing else changes semantics: a refused focus is not
lost, because `Client` counts unread replies and `take_deferred_error` surfaces them on
the next drain (`main.rs:616`).

**Risk, stated plainly:** a focus that the daemon rejects now shows up a frame or two
later instead of immediately. That is the correct trade for a gesture whose whole value
is that it feels instant, and the failure is visible rather than silent.

## 2 · Freeze a pane only when something actually moves

Copy mode's freeze exists so live output cannot scroll text out from under a selection
in progress. On an idle pane — which is most panes, most of the time — there is no live
output, so the clone buys nothing and costs 7.7 MB.

Make the freeze lazy:

```rust
enum SelectionScreen {
    /// Nothing has arrived for this run since the gesture began; read the live parser.
    Live,
    /// Output arrived, so the grid the selection was made against was captured first.
    Frozen(PaneSnapshot),
}
```

`CopyMode::frozen: PaneSnapshot` becomes `CopyMode::screen: SelectionScreen`, with a
resolver that hands back a `&vt100::Screen` from whichever arm is live. Promotion from
`Live` to `Frozen` happens at exactly four moments, each of which is a point where the
live screen is about to stop being the grid the user pointed at:

1. `apply_event` is about to feed a `PaneDelta` into the parser for this run — snapshot
   *before* applying it.
2. The wheel scrolls during a selection, which needs scrollback the live viewport is not
   showing.
3. Keyboard copy mode is entered deliberately (`Ctrl+B [`), which is a request to walk
   history and should snapshot eagerly, as today.
4. A pane resize or re-attach — though `end_copy_mode_for` already ends the mode there,
   so this is an assertion rather than a branch.

Idle pane: zero clones for the whole gesture. Busy pane: one clone, at the same moment
and for the same reason as today. The `PaneSnapshot` type stays deliberately non-`Clone`
(`vt.rs:299`), and that comment stays true.

## 3 · A selection that behaves the way selections behave

Four gaps, all small:

- **Shift+click extends** the standing selection from its anchor instead of starting a
  new one. Every terminal does this; Dock does not.
- **Dragging past the pane edge scrolls it**, one row per motion event beyond the
  boundary, promoting the selection to `Frozen` as it goes. Today the drag clamps
  (`clamp_cell`) and the selection simply stops growing.
- **Esc unwinds one level at a time.** With a selection standing, Esc clears the
  selection and stays in copy mode; a second Esc leaves copy mode, as it does today.
  This is the pattern the board overlay and copy mode already follow, and the comment at
  `dashboard.rs:3180` states it as a rule: abandoning a half-made thing should not also
  close what is behind it.
- **The clipboard actually reaches the clipboard.** `DOCK_CLIPBOARD` defaults to
  `Osc52` (`clipboard.rs:47`), which is write-only and which Terminal.app disables
  outright — hence the notice in the bug report: *"copied 49 characters · OSC 52 (asked
  the terminal; it cannot acknowledge)"*. Change the default so that when
  `DOCK_CLIPBOARD` is unset **and** a helper (`pbcopy`, `wl-copy`, `xclip`) is on
  `PATH`, both routes run — which is what `ClipboardPreference::Both` already means and
  what `copy_with` already implements. Over SSH, where no helper exists, the behaviour
  is unchanged and so is the honest notice. Explicit values keep overriding.

  This is the single largest intuitiveness win in the document and it is a change to
  one default.

## 4 · The board spends its width where the cards are

`render_board_columns` divides width equally: `column_width = area.width /
statuses.len()` (`dashboard.rs:5532`). Five columns, so `DONE` gets a fifth of the pane
however many of the other four are empty — about 19 usable cells, of which the marker
and `#N ` take five, which is why titles ellipsise at fourteen characters.

Replace with a pure, testable allocator:

```rust
/// Widths for each column, left to right, summing to exactly `total`.
fn column_widths(total: u16, counts: &[usize]) -> Vec<u16>
```

Rules, in order:

1. An empty column gets a **stub**: the width of its rendered heading — `column_heading`
   plus `" · 0"` — plus one cell of gutter, clamped to 8–12. `BACKLOG · 0` is eleven
   cells and so takes the ceiling; `TODO · 0` takes eight. It stays visible and stays a
   drop target; it just stops hoarding.
2. Non-empty columns split the remainder, each guaranteed a floor of 18 cells where the
   arithmetic allows, the rest distributed in proportion to card count.
3. If `total` cannot cover even the stubs, fall back to today's equal division. A
   narrow pane degrades to the current behaviour rather than to a panic.

**Invariants, as property tests:** the widths sum to exactly `total`; none is zero; the
order matches `statuses`; the function is total over `0..=u16::MAX` and any count
vector.

While in here, fix a real inconsistency: `card_lines` subtracts `marker.len() + 3 +
task.id.to_string().len()` from the width budget for a card that has a live run, but the
no-run branch ellipsises against the full `width` (`dashboard.rs:5660`). Two visually
identical cards truncate at different lengths depending on whether an agent happens to be
attached. One budget, computed once.

## 5 · Cards that are finished stop shouting

Today nothing ever leaves the board. `board::load` reads every file in `kanban/tasks/`,
`STATUSES` has no terminal state beyond `done`, and there is no prune, expiry or delete
path anywhere in the module. A card moved to `done` is on that board forever. That is
the honest answer to "when do items in the done column disappear": **they do not.**

Give them a way out that is a fact about the task rather than a fact about one person's
view:

- **`archived: true`** in the task's YAML frontmatter; absent means false.
- **`board::set_archived(dir, id, bool)`**, following `set_status`'s existing
  read-modify-write shape so both paths agree about how a task file is edited.
- **`BoardView`** filters archived cards out unless revealed. Counts in the headings
  count the visible ones.
- **A footer row under a column that is hiding some:** `12 archived · v reveals`.
  Revealed archived cards render in `muted`, so a revealed board never looks like a
  normal one.

Board keys — chosen against what `board_key` already binds, where `h`/`j`/`k`/`l` are
cursor motion (`dashboard.rs:3209`), so the obvious `h` for "hide" is not available:

| key | does |
|---|---|
| `a` | archive the selected card, or unarchive it if revealed |
| `A` | archive every card in `DONE` |
| `v` | reveal / re-hide archived cards |

**Archiving respects `board_is_personal`.** The repository's own `kanban/` board is
managed by kanban-md and Dock refuses to write to it (`dashboard.rs:3108`). Archiving
is a write, so it refuses with the same sentence in the same shape as `create_task`'s.

Explicitly **not** doing: auto-archive on a timer. A background mutation of files that
git tracks, triggered by a clock rather than by a person, is not something to do to
someone's repository without being asked.

## 6 · Right-click, everywhere it means something

A ninth entry in `OVERLAY_ORDER` (`dashboard.rs:584`), which is the one list that
governs both drawing and key routing — the comment there explains why adding a surface
anywhere else would be a bug.

```rust
struct ContextMenu {
    origin: (u16, u16),        // where the pointer was
    target: MenuTarget,
    items: Vec<MenuItem>,      // separators included
    cursor: usize,
}

enum MenuTarget {
    Pane(String), Tab(String), SidebarWorkspace(String),
    SidebarAgent(String), BoardCard(u32), Canvas,
}
```

Every `MenuItem` carries a label, the key that also does it, and an action that wraps an
existing `PaneCommand` or `Request`. **No menu item invents behaviour** — the menu is a
second route to things Dock already does, which is what keeps it from becoming a place
where features hide.

| right-click on | items |
|---|---|
| pane body or border | Copy selection · Paste last copy · — · Split right · Split down · Zoom · — · Rename · Restart · Close pane |
| workspace tab | New workspace · Rename · — · Close workspace |
| sidebar workspace row | Switch to · Rename · Close |
| sidebar agent row | Focus its pane · Resume · — · Restart |
| board card | Move to column ▸ · Dispatch · — · Archive |
| empty canvas | New workspace · Task board · What changed · Every key |

Behaviour: the popup is placed at the pointer and flipped left or up when it would
overflow the frame, then clamped — a menu is never drawn partly off-screen. `↑`/`↓`
move, `Enter` activates, `Esc` dismisses, and typing an item's key hint activates it
directly. Hover moves the cursor, a left-click activates, a click outside dismisses, and
a right-click somewhere else re-targets rather than stacking.

**Middle-click keeps `paste_last_copied`** (`dashboard.rs:5164`), which is the X11 and
tmux convention, and the pane menu carries the same action so the gesture is
discoverable rather than folklore.

## 7 · A sidebar that gets out of the way

`Ctrl+B s` — verified free against `command_for` (`keymap.rs:154`), where `b` itself is
unavailable because pressing the prefix twice already means "send a literal Ctrl+B".

Two states, not two-and-zero:

- **Full**, 28 columns, as today (`dashboard.rs:1204`).
- **Rail**, 3 columns, showing one glyph per agent in the existing blocked-first order.

The rail rather than a width of zero is the whole design. The sidebar's one
irreplaceable job is telling you an agent needs you, and a collapse that takes that away
trades a real capability for width. `● ◍ ○` in a three-cell strip keeps it.

Clicking the rail expands it; a chevron at the top of the full sidebar collapses it.
The rail is automatic when a full sidebar would leave the canvas under 60 columns —
that is, below a terminal width of 88 — so a narrow terminal is not mostly sidebar. An
explicit toggle wins over the automatic rule until the terminal is resized again.

Initial state comes from `DOCK_SIDEBAR=full|rail`, which is how every other knob in Dock
is spelled and why `clipboard.rs:40` chose an environment variable over a config file.
Within a session the toggle is remembered; it is not written to disk. Dock has no
client-side preferences store and this feature does not justify inventing one.

## 8 · Graphite & Cyan

Add one token. `Theme` has no way to say "this surface sits above the ground", so panes,
sidebar and overlays all paint on the same flat `surface` and the UI reads as one plane.

```rust
/// A surface that sits above `surface`. Chrome only — never a terminal pane body,
/// where a background would fight programs that set their own.
pub panel: Color,
```

The palette, replacing `Theme::warm()` as the default and keeping it available under
`DOCK_THEME=warm`:

| token | value | means |
|---|---|---|
| `surface` | `#12161a` | the ground |
| `panel` | `#1b2026` | chrome above it |
| `text` | `#dde4e8` | |
| `muted` | `#7c8a91` | |
| `border` | `#262e33` | |
| `accent` / `border_focused` | `#4fd1c5` | structure: focus, active tab, pressable keys |
| `selection` | `#3a6b78` | |
| `blocked` — *needs you* | `#f2726b` | the only warm colour in the palette |
| `done` — *your turn* | `#7aa2f7` | |
| `working` | `#35a099` | deliberately recessive |
| `idle` | `#6e7681` | |

The reasoning worth keeping: today `accent` (232,168,88) and `working` (226,184,96) are
nearly the same colour, and `accent` is simultaneously the focused border, the active
tab, and every keybinding in the sidebar. So "an agent is working" competes for the same
channel as "here is a key you can press", and nothing amber can be urgent. Making rose
the *only* warm colour means `needs you` cannot be confused with chrome.

**Tests, since these are numbers rather than taste:**

- every token reaches 3:1 against both `surface` and `panel` — the floor this palette
  meets at 3.57:1 in its worst case (`idle` on `panel`);
- `selection` clears 3:1 against `surface` (3.08) and carries `text` at 4.5:1 (4.60) —
  the two floors `theme.rs:21` already commits to;
- the four agent-state colours stay at least 60 RGB units apart. This one is not
  theoretical: `working` and `idle` collided twice while the palette was being chosen,
  because both mean "nothing is being asked of you" and both drift toward the same quiet
  slate. They are 74.8 apart now, and a test is what keeps them there.

A Nord-style lighter ground was costed and rejected on evidence rather than taste: its
ground is light enough that a selection band bright enough to clear 3:1 leaves text
below 4.5:1, at every text lightness searched, and its `needs you` red lands at 3.56:1
on a panel — marginal, for the one colour that must never be.

## Order of work

Repairs before additions, and the regression first.

1. Fire-and-forget focus (§1)
2. Lazy freeze (§2)
3. Selection behaviour and the clipboard default (§3)
4. Board column widths (§4)
5. Archive (§5)
6. Context menus (§6)
7. Sidebar rail (§7)
8. Palette (§8)

§8 is independent of everything above it and could move earlier if the visual change is
wanted sooner. §6 is the largest single piece and depends on nothing but §1.

## What done means

- 598 lib and 42 bin tests green; `cargo fmt` and `cargo clippy` clean.
- `render_breakdown` and `measure_frame` re-run and compared against the baseline in
  this document. The frame may not regress by more than 10 % at any of the three sizes.
- A new `#[ignore]`d measurement for gesture start — press-to-first-highlight on an
  unfocused pane — recorded before and after §1 and §2, since that is the number this
  work exists to move and nothing currently measures it.
- New tests: `column_widths` invariants, archive round-trip through the file, menu
  placement clamping at every frame edge, and the three palette floors above.

## Out of scope

- Conditional / dirty-flag rendering — measured unnecessary, above.
- Reading the system clipboard. OSC 52 is write-only and Dock cannot ask.
- Reordering cards within a column.
- A light theme.
- Auto-archiving on a schedule.
