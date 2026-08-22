# Dock A1 — Copy mode, scrollback, and mouse selection

Status: proposed
Date: 2026-08-22
Sub-project: A1 of the post-P0 programme (A1, A2, B, C)

## Decision

P0 enabled `EnableMouseCapture` so panes could be focused by click and dividers dragged. That
takes mouse events away from the host terminal, which removed the user's ability to **select and
copy text out of a pane**. Every multiplexer that captures the mouse ships copy mode in the same
change; Dock shipped the capture without the replacement.

A1 restores what capture took away, and adds the scrollback navigation the emulator already
supports but nothing exposes. It is deliberately the smallest sub-project: no protocol change, no
structural change, entirely client-side.

## Problem

Three distinct gaps, all reported by the user as "no mouse support":

1. **Clicking** — clickable sidebar targets were recorded at rows that were never drawn.
   **Already fixed** in `04b4248`: the roster listed every run rather than every agent, so each
   shell pane pushed `LAUNCH AGENT` and `dismiss all` off the sidebar while their rectangles
   stayed recorded. `clickable_row` now refuses to record an undrawn row, and the sidebar no
   longer wraps. This spec records it for completeness; no further work.
2. **Scrolling** — `MouseEventKind::ScrollUp`/`ScrollDown` reach `Dashboard::mouse` and fall
   through to `_ => UiCommand::None`. The wheel does nothing.
3. **Selecting** — there is no way to get text out of a pane at all, by mouse or keyboard.

Meanwhile each pane retains a bounded scrollback (default 2000 rows) that no user-facing feature
reads.

## Verified foundation

These were confirmed by reading `vt100-0.16.2` source, not inferred:

- `Screen::set_scrollback(rows: usize)` sets a scrollback offset, **clamped to the actual
  scrollback size**, and it "affects the return values of methods called on the screen: for
  instance, `screen.cell(0, 0)` will return the top left corner of the screen after taking the
  scrollback offset into account."
- `Screen::scrollback() -> usize` reports the current offset.
- `Screen::contents_between(start_row, start_col, end_row, end_col)` is documented as "useful for
  things like determining the contents of a clipboard selection."

Two consequences that shape the design:

- **Rendering needs no change.** `tui_term::PseudoTerminal` reads the screen through the same
  methods, so setting a scrollback offset makes it draw the scrolled view automatically.
- **Selection extraction is a library call**, not a hand-rolled grid walk.

## Scope

In scope:

- Scroll wheel scrolls the focused pane's scrollback.
- Keyboard copy mode: `Ctrl+B [` to enter, `hjkl`/arrows to move, `v` to select, `y` to yank,
  `Esc`/`q` to exit.
- Mouse drag selection inside a pane, yielding the same selection model as keyboard copy mode.
- Search within scrollback: `/`, then `n`/`N` to cycle matches.
- Clipboard write via OSC 52, with a `pbcopy` fallback on macOS.
- A visible mode indicator so the user always knows keys are not reaching the shell.

Explicitly out of scope:

- ~~Any protocol change. A1 is client-side; the client already holds a full parser replica.~~
  **Superseded.** This premise was falsified by probe during implementation: the client's
  replica is a *repaint mirror*, not an append-fed terminal. `state_diff` emits
  cursor-addressed repaints, and `vt100` only pushes rows into scrollback from its scroll
  path, so the replica's history was permanently empty and the wheel did nothing. Scrollback
  therefore required a protocol change: **v8** forwards the raw PTY bytes a subscriber has
  not yet seen, so the client mirrors the daemon's terminal and builds identical history.
  Copy mode over the visible screen remains entirely client-side as originally designed.
- Rectangular/block selection. Line and character selection only.
- Selection history or multiple registers.
- Mouse reporting passthrough to programs inside the pane (an agent TUI that wants its own mouse
  handling). Deferred, and noted below as a known limitation.
- The A2 workspace model (tabs, named layouts, per-project workspaces, picker).

## Architecture

A new client-side module, `src/copy.rs`, owns the selection state machine. `Dashboard` gains one
field:

```rust
copy: Option<CopySession>,   // None = live pane, Some = copy mode
```

```rust
pub struct CopySession {
    pane_id: String,
    cursor: (u16, u16),          // row, col within the visible grid
    anchor: Option<(u16, u16)>,  // Some once `v` or a drag begins
    search: Option<SearchState>,
}
```

Scroll offset lives on the pane's replica, not in `CopySession`, because the wheel must work
**without** entering copy mode.

### Data flow

```
wheel / hjkl ──▶ set_scrollback(offset)  ──▶ PseudoTerminal renders scrolled view
   v / drag  ──▶ CopySession.anchor
        y    ──▶ contents_between(anchor, cursor) ──▶ OSC 52 ──▶ host clipboard
```

### The pinning problem

The client's replica keeps receiving deltas while the user is scrolled back. If new output moved
the view, reading old output would be impossible on a busy pane.

**Resolved by probe: `vt100` pins the view itself.** Feeding one new line while the offset was
10 left the top visible row unchanged at `line 7` — and moved the offset to **11**. The emulator
auto-adjusts the offset so the viewport stays still as new output pushes content upward.

Two consequences, both load-bearing:

- The client does **not** need to re-apply the offset after each `feed()`. Set it once.
- **The offset number is not stable; the viewport is.** Any code that treats a specific offset as
  an invariant (`assert_eq!(screen.scrollback(), 10)` after feeding) will be wrong. The only
  meaningful state is `scrollback() == 0` (following live output) versus `!= 0` (pinned). Tests
  must assert on the visible rows, never on the offset value.

Clamping was also confirmed: `set_scrollback(99999)` clamped to the 17 rows actually retained, and
`set_scrollback(0)` returns to following live output. `contents_between` reads correctly across a
scrolled view, returning `"line 8\nline 9\nline 10"` for a three-row selection.

### Clipboard

OSC 52 first: it needs no dependency, and works over SSH, which matters because A2 and beyond
point at remote use. Some terminals disable it by default, so a `pbcopy` fallback runs on macOS
when OSC 52 is unavailable. The yank reports which path was used in the footer, so a silent
no-op is impossible.

## Interaction

| Context | Key / gesture | Action |
|---|---|---|
| live pane | wheel up/down | scroll scrollback by 3 rows |
| live pane | drag | enter copy mode, anchor at the press point, cursor follows the pointer |
| copy mode | release drag | finalise the selection and stay in copy mode |
| live pane | `Ctrl+B [` | enter copy mode at the cursor |
| copy mode | `h j k l`, arrows | move cursor |
| copy mode | `g` / `G` | top / bottom of the visible viewport |
| copy mode | `v` | start selection at cursor |
| copy mode | `y` | yank selection, exit copy mode |
| copy mode | `/` | search; `n` / `N` cycle matches |
| copy mode search | `Esc` | cancel the prompt, stay in copy mode |
| copy mode | `Esc`, `q` | exit without yanking |

Releasing a drag finalises the selection but does **not** write to the clipboard. Yanking is
always an explicit `y`, so a stray drag can never overwrite what the user copied earlier —
the same reason tmux separates selection from yank. The selection stays adjustable by motion
keys after release.

`Esc` moves strictly outward, one level per press: from the search prompt it cancels the
prompt, and from copy mode it leaves the mode. "Never trap the user" means a bounded, small
number of presses always reaches the live pane — not that a single press must escape every
level at once. Dock already applies this to forms, where `Esc` cancels a rename rather than
quitting the dashboard; the search prompt is the same shape of nested state and must behave
the same way.

Copy mode is modal and must **say so**: the pane title shows `COPY` and the footer shows the
copy-mode bindings, for the same reason P0 deleted the old invisible input mode.

Shift-drag remains the escape hatch for the host terminal's own selection in terminals that
implement it. Dock cannot control that and must not claim it works; the README should mention it
as terminal-dependent.

## Error handling

- A wheel event on a pane with no scrollback is a no-op, not an error.
- `y` with no selection yanks the cursor's line rather than failing.
- A clipboard write that fails on both paths reports the failure in the footer.
- Entering copy mode on a pane with no run is refused with a reason, matching how other pane
  commands behave.
- Copy mode must not swallow input silently: while active, keys that are not bindings are ignored
  and the mode indicator explains why.

## Testing

- Selection maths over a known grid: single line, multi-line, reversed (anchor after cursor),
  and a selection spanning the scrollback boundary.
- Scroll clamping at both ends: offset never exceeds the retained rows and never goes negative.
- Pinning: feeding deltas while scrolled back leaves the offset unchanged and the replica correct.
- Search: hit ordering, wrap-around, and no-match behaviour.
- OSC 52 payload shape, including base64 of a multi-line selection.
- A real-PTY check that `y` puts the expected bytes on the system clipboard — both Critical
  defects in P0 were invisible to the test suite and surfaced only from driving the binary.

## Acceptance

1. The wheel scrolls a pane's history, and scrolling to the bottom resumes following live output.
2. Dragging across pane text selects it; `y` puts it on the system clipboard.
3. `Ctrl+B [` then `v`, motion, `y` does the same from the keyboard.
4. `/` finds a string in scrollback and `n` cycles matches.
5. Copy mode is visibly signalled and always exits on `Esc`.
6. A busy pane can be scrolled back and read without the view jumping.

## Known limitations to document

- Programs inside a pane cannot receive mouse events; Dock consumes them. An agent TUI wanting
  its own mouse handling will not get it until mouse passthrough is built.
- OSC 52 is terminal-dependent and disabled by default in some terminals.
