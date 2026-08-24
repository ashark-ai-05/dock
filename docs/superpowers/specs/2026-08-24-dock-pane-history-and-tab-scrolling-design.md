# Dock — Deep pane history, and a tab strip that scrolls

Status: proposed
Date: 2026-08-24
Sub-project: standalone, after B (board pane + queue, shipped)

## Decision

Two scrolling surfaces are incomplete, for unrelated reasons, and this spec finishes both.

**Panes already scroll, into almost nothing.** The wheel works, the daemon retains 2000 rows, and the client
builds its replica with exactly the daemon's retention — but the seed the client is given is the *visible
grid only*, so the replica starts empty and accumulates only what arrives after it attached. Scroll up in a
pane you just opened and there is nothing above the fold, however much that agent printed before you looked.
Every re-seed throws away what had accumulated.

**The tab strip does not scroll at all.** `render_tabs` lays tabs left to right and `break`s at the row edge.
A workspace pushed off the end stays reachable by digit and by `Ctrl+B w`, and is unreachable by mouse.

After this project: a pane holds **16 MB of retained output per pane** — hundreds of thousands of lines,
far deeper than anyone scrolls — delivered as a 256 KB prefix at attach and paged back on demand; and the tab
strip scrolls horizontally, following focus, with every workspace clickable.

Four decisions, taken as given, each chosen against a stated alternative:

1. **History lives in RAM, bounded by bytes.** Not on disk. `OutputLog`'s doc comment states the invariant —
   *"deliberately in-memory only… nothing here is ever written to a durable record"* — and a pane's output is
   the least appropriate thing in Dock to persist: it is every token, secret, and file body an agent printed.
   The invariant stands. "Infinite" therefore means *deeper than you will scroll*, not literally unbounded.
2. **Not unbounded in RAM.** The daemon holds every pane at once; one runaway build loop would take the whole
   canvas down.
3. **A prefix at attach, the rest on demand.** Not the whole log at attach: 8 panes × MBs, base64'd and
   parsed through vt100 on every client start and every reconnect, is a visible startup stall.
4. **Scrolling does not freeze the pane.** Copy mode's freeze clones the grid *and* the scrollback
   (`vt.rs:206`); at 200k rows that clone cannot meet the 60fps budget.

---

## Verified foundation

Read from the source on 2026-08-24, not inferred.

**Subscribers are sent the child's raw bytes, not repaints.** `OutputLog`'s doc comment (`terminal/mod.rs:67`)
states why: *"A repaint is cursor-addressed and therefore never scrolls, so a client fed repaints can never
accumulate history no matter how much output the pane produced; a client fed the original bytes scrolls
exactly as the daemon's terminal did."* **History is therefore already reconstructible by replay.** This
project does not invent a history mechanism; it widens one that exists and delivers it.

**`OutputLog` is a 1 MB ring of raw bytes** (`PANE_OUTPUT_LOG_BYTES`, `terminal/mod.rs:65`), addressed by a
monotonic byte sequence, carrying an `epoch` (`:87`) that changes when a run restarts so a stale reader
re-seeds rather than being handed bytes from the middle of a new stream. `since(from)` returns `None` rather
than a gap when a reader has fallen further behind than the log retains (`:130`).

**The seed is the visible grid.** `PaneSubscriberView::seeded` (`server.rs:709`) sends
`output.screen().state_bytes()`, which is vt100's `state_formatted()` (`vt.rs:88`) — the screen, no
scrollback. This single fact is why deep client history does not exist today.

**The client already intends to hold the daemon's history.** `apply_event`'s `PaneAttached` arm
(`dashboard.rs:684`) builds `PaneScreen::new(rows, cols, scrollback_rows)` with the comment *"The daemon's own
retention, so this replica holds exactly the history the daemon holds."* The capacity matches; the content
does not. The intent is already in the code.

**`ScreenSync` repairs a replica that drifted** (`terminal/mod.rs:25`). `delta_from` reports the difference
between what a subscriber was sent and the live screen, so the daemon can append a correcting repaint. **A
seed that is not perfectly faithful self-heals rather than corrupting the screen** — this is what makes
replaying an arbitrary tail safe.

**The alternate-screen guarantee is load-bearing and fragile.** `seeded` emits `\e[?1049h` when the pane is in
the alternate screen, and the comment at `server.rs:698-710` states there is *deliberately* no matching
`1049l`, *"only safe because `Dashboard::apply_event` rebuilds the client's parser on every `PaneAttached`, so
a seed always lands in a fresh primary buffer"*, and warns that *"reusing an existing parser across a
re-attach — to preserve its accumulated history, say — would strand a client that was in the alternate
screen."* That warning names this project. §2 answers it.

**Subscriptions are strictly one-way.** *"A subscribed connection is one-way: the daemon acknowledges nothing
and never reads again"* (`client.rs:495`). A history request cannot ride the subscription. `Client::request`
(`client.rs:100`) is a separate one-shot connection and is where it goes.

**vt100's scrollback offset is measured from the bottom, and it self-adjusts as rows arrive.**
`grid.set_scrollback` is `self.scrollback_offset = rows.min(self.scrollback.len())` (`grid.rs:198`), and
`scroll_up` bumps the offset for every row it pushes into scrollback while the view is scrolled
(`grid.rs:571-574`). So a scrolled-up pane stays pinned to the content the reader is looking at, and a
rebuilt parser with more history above keeps pointing at the same content too (§4).

**Corrected 2026-08-25.** An earlier revision of this section claimed "nothing touches `scrollback_offset`
when rows are appended", and §4 accordingly specified a drift-compensation fix. That was wrong — read from
`set_scrollback` alone without grepping for other writers — and the compensation would have double-counted
vt100's own increment, un-pinning the very content it was meant to hold. The behaviour is now pinned by a
characterisation test instead, because `terminal/mod.rs:14` names `PaneScreen` a swap point for the terminal
engine and a replacement engine that lacked this would break scrolling everywhere with no other alarm.

**vt100 exposes no scrollback-length getter.** `Grid::scrollback_len()` (`grid.rs:190`) returns the configured
*capacity* and `Grid` is private; `Screen::scrollback()` (`screen.rs:122`) returns the current offset. The
actual retained row count is reachable through the public API only because `set_scrollback` clamps: set it to
`usize::MAX`, read it back, restore. §4 depends on this.

**Protocol is at v12** (`protocol.rs:11`), asserted at `protocol.rs:769`. `Request` has 18 variants
(`protocol.rs:16`). Structs use `#[serde(deny_unknown_fields)]`.

**The daemon default is 2000 rows**, `dockd --scrollback-rows=N` (`dockd.rs:13`, `:26`).

**`render_tabs` truncates by design** (`dashboard.rs:1133`), documented at `:1130`: *"Tabs are laid out left to
right and simply stop when the row runs out, rather than scrolling."* The active tab carries a `✎` rename
affordance and a two-step `×` close, each guarded by `x.saturating_add(3) <= area.right()`. `tab_areas`
records hit targets for the mouse.

---

## 1. `OutputLog` becomes the history store

`PANE_OUTPUT_LOG_BYTES` grows from 1 MB to a **16 MB default**, configurable as
`dockd --pane-history-bytes=N`, plumbed exactly as `--scrollback-rows` already is.

Its doc comment must change, and the change is the point. Today it reads *"It bounds only the **undelivered**
window, not history: the scrollback the user scrolls through lives in each side's parser, not here."* After
this project one number bounds both. That is acceptable — a subscriber more than 16 MB behind has genuinely
stalled and must re-seed regardless — but the comment must say so rather than leave a reader with a
description that is no longer true.

Two readers join `since()`:

- **`tail(max: usize) -> (u64, Vec<u8>)`** — the newest `≤max` bytes and the sequence they begin at, snapped
  **to a chunk boundary**. Chunks are whole writes, so a boundary is the closest thing to a safe parser
  entry point the log has.
- **`before(before: u64, max: usize) -> (u64, Vec<u8>, bool)`** — the `max` bytes *ending at* `before`,
  clamping where `since()` refuses, returning the sequence actually served and whether it reached the oldest
  retained byte. Backwards rather than forwards: a client extending its history needs the bytes that abut
  what it already holds, and a `before` falling inside a write is truncated to it rather than skipped, so
  the answer never leaves a gap.

`since()` keeps its `None` contract untouched: it serves the delta path, where a gap is a correctness bug.
The clamping reader serves the history path, where a short answer is the honest one.

**A tail may begin mid-escape-sequence.** Chunk boundaries are write boundaries, not sequence boundaries. The
visible screen is repaired by `ScreenSync` regardless; the cost is at most one malformed glyph in the oldest
history row. Accepted, and documented where `tail` is defined rather than discovered later.

## 2. The seed carries history — and the alternate-screen trap

`PaneSubscriberView::seeded` replays `log.tail(SEED_HISTORY_BYTES)` — **256 KB**, a few thousand lines —
instead of `state_bytes()`. `ScreenSync` then diffs against the live screen and appends a correction, so the
visible result is exact even when the tail began mid-history.

The buffer handling cannot be carried over unchanged, and this is the sharpest edge in the project. Replaying
raw history may itself contain `1049h`/`1049l` transitions, so after replay the client's parser can be in
*either* buffer, and the current code's assumption — a seed always lands in a fresh primary buffer — no longer
holds. The `1049l` the comment says is deliberately absent becomes necessary.

The fix uses machinery already in `seeded`, which builds a `ScreenSync` and calls `view.sync.apply(&bytes)`:

1. Replay the tail into the seed's own `ScreenSync`.
2. Compare that replica's `alternate_screen()` against the live screen's.
3. Append `\e[?1049h` or `\e[?1049l` **only when they disagree**.
4. Let the existing drift correction finish the job.

This is precise rather than heuristic — the daemon knows exactly which buffer its replica of the client
landed in — and it preserves the guarantee the comment protects. The comment must be rewritten to describe
the new rule, because a future reader who trusts the old one will reintroduce the bug.

## 3. Protocol v13

```rust
Request::PaneHistory(PaneHistoryRequest { run_id: String, before: u64, max_bytes: u32 })

Response::PaneHistory {
    run_id: String,
    epoch: u64,
    from: u64,
    bytes: String,   // base64
    complete: bool,  // `from` is the oldest byte retained; stop asking
}
```

Carried on `Client::request`'s one-shot connection, leaving the subscription one-way as documented.

`Event::PaneAttached` gains two fields:

- **`history_from: u64`** — the sequence the seed's bytes begin at. The client's paging cursor.
- **`epoch: u64`** — already tracked daemon-side (`OutputLog::epoch`), never sent. Without it a client
  holding a cursor from a restarted run would page into the middle of a different byte stream, which is the
  exact failure `epoch` was introduced to prevent.

`max_bytes` is clamped daemon-side. A client asking for 16 MB in one request is served 16 MB; the clamp
exists so a malformed request cannot ask for more than the log can hold.

## 4. Client — paging back, without a freeze

**The client keeps its own capped byte log per pane**, mirroring what it was sent. Paging back is then local:
fetch the older chunk, prepend, rebuild the parser from the whole log. One round trip, then an in-process
rebuild. The alternative — asking the daemon for `from`-to-`end` and re-sending bytes the client already has
— makes every page-back re-transmit the entire history.

The client's log is capped at the **same byte budget the daemon announces**, so the two never disagree about
how much history exists and the client cannot accumulate more than the daemon can re-serve.

Per-run state: `{ epoch, from, complete }`. When a scroll leaves the viewport **within one screen height of
the top** of what the parser holds, and `complete` is not set, the client requests the next **2 MB**. An
`epoch` mismatch in the response discards it.

**No in-flight guard is needed.** `run_dashboard` (`main.rs:654`) sends a `UiCommand::Request` with a
blocking `client.request(&request)` and handles the response before reading the next input event, so a
second scroll cannot arrive while a history request is outstanding. A failed request therefore leaves
`from` unmoved and the next scroll simply retries, which is the behaviour we want.

**The viewport survives the rebuild for free.** Because `scrollback_offset` is measured from the bottom
(`grid.rs:198`), adding rows above leaves the same offset pointing at the same content. Nothing needs to be
recomputed; the property needs a test so a later change cannot quietly break it.

**Drift while scrolled would make deep scrolling pointless — and vt100 already prevents it.** This section
originally asserted that nothing adjusts `scrollback_offset` when rows are appended, and specified a fix.
That was wrong: `Grid::scroll_up` bumps the offset for every row it pushes into scrollback while the view is
scrolled (`grid.rs:571-574`), so a scrolled pane stays pinned on its own. Adding a second increment on top of
vt100's would have un-pinned the content the fix was meant to hold — measured landing at offset 30 where 20
was correct. **Corrected 2026-08-25**; what ships instead is a characterisation test pinning vt100's
behaviour, because `terminal/mod.rs:14` names `PaneScreen` a swap point for the terminal engine and a
replacement lacking this would break every scrolled pane with no other alarm.

The row count is readable through the public API, since `set_scrollback` clamps to the actual length. This
becomes a method on `VtTerminal` (`terminal/vt.rs`), beside the existing `scroll_by` and `scroll_offset`, so
the clamp trick is stated once with its reasoning rather than repeated at each call site:

```rust
fn history_rows(screen: &mut Screen) -> usize {
    let saved = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let rows = screen.scrollback();
    screen.set_scrollback(saved);
    rows
}
```

`history_rows` survives the withdrawn compensation because §4's paging needs it for a different question:
whether the replica is already holding as many rows as it is allowed to, which is the local stopping
condition that keeps a client from asking for history it could never display. O(1), and no clone.

**This is why scrolling does not freeze the pane.** Copy mode freezes by cloning the grid and the scrollback
(`vt.rs:206`, *"costs a full copy of the grid **and** the scrollback"*). Correct at 2000 rows, unaffordable
at 200k on a path that must hold 60fps. Copy mode itself is unchanged and still freezes — it is entered
deliberately and briefly — but ordinary scrolling must not.

**Client parser capacity** must stop being the daemon's 2000-row parser retention, which would silently
discard replayed history above 2000 rows. **The `PaneAttached.scrollback_rows` field is reused, not
replaced**: the daemon keeps sending that field and changes what it puts in it, from its own parser retention
to a row capacity derived from the byte budget. No protocol change is needed, which is what lets §7 step 2
ship before v13. The field's meaning is unchanged — *the history this replica should hold* — which is what
the comment at `dashboard.rs:684` already claims it to be; only the daemon's parser retention and the
client's replica capacity stop being the same number, and the field name is now slightly narrow for what it
carries. Renaming it is deliberately deferred to v13 in step 3 rather than done twice.

**Scroll surface.** The wheel is unchanged. Added: `Ctrl+B PgUp` / `Ctrl+B PgDn` for half a page, and
`Ctrl+B End` to snap back to following live output. While `scroll_offset() > 0` the pane chrome shows
`▲ 1,240 rows · End to follow`, because a pane that has silently stopped following live output is
indistinguishable from a hung agent.

## 5. The tab strip scrolls

`render_tabs` gains a `tab_scroll` first-visible index, held on the dashboard and clamped every frame so the
active tab is **fully** visible — including its `✎` and `×` affordances, which must not be what falls off the
edge. The existing `break` at the row edge becomes a skip-then-break window over the same layout pass.

`‹` and `›` render in reserved edge columns when tabs are hidden on that side, and are clickable hit targets
recorded like `tab_areas`. The wheel over the strip's area scrolls one tab per notch.

Digits and `Ctrl+B w` are untouched. Tab numbers stay positional, as `:1130` says.

## 6. Testing

**`OutputLog`** — eviction at the new cap; `tail` snapping to a chunk boundary; `tail` when one write exceeds
the whole budget; `before` clamping below the oldest byte, and truncating to a cursor that falls inside a write; `complete` true only at the oldest retained
byte; `since()`'s `None` contract unchanged.

**Seed** — the seed contains history, not just the grid; `ScreenSync` correction still lands the visible
screen exactly. **Alternate-screen normalisation asserted in both directions**: daemon in alt with a tail
ending in primary, and daemon in primary with a tail ending in alt. That second case is the regression that
would paint a full-screen program over a user's history, and it is the one the old comment warned about.

**Protocol** — v13 round-trip; `deny_unknown_fields` rejection; the `assert_eq!(PROTOCOL_VERSION, 12)` at
`protocol.rs:769` updated.

**Client** — paging fires at the top and exactly once while pending; `complete` stops it; an `epoch` mismatch
discards the response; the scroll offset is preserved across a rebuild; and a characterisation test pins
vt100's own offset compensation, which is what keeps content still while deltas arrive.

**Tabs** — the active tab is fully visible including affordances at every scroll position; markers appear and
disappear with hidden tabs; marker clicks and wheel scroll the strip; a single tab still renders with no
markers.

**Perf, per the standing rule: measured before and after, not audited later.** There is no `benches/`
directory in this repository, so this project adds timing tests on the three paths it touches rather than
inventing a harness: seed construction at a full log, parser rebuild on page-back, and the render path that
must hold 60fps. The 16 MB × pane-count memory cost is measured and reported, not asserted.

## 7. Build sequence

Each step ends green and shippable.

1. **`OutputLog` capacity and readers** — `tail`, `before`, the `--pane-history-bytes` flag, the doc
   comment correction. Daemon-only, no protocol change, nothing observable yet.
2. **Seed carries history** — `seeded` replays the tail; alternate-screen normalisation and its two-direction
   tests. Client parser capacity derived from the byte budget. **Deep scrollback works from this step on**,
   for history produced after attach.
3. **Protocol v13** — `PaneHistory` request and response, the two new `PaneAttached` fields, version bump.
   Wire only; no client behaviour yet.
4. **Client paging** — the client-side byte log, page-back on scroll, epoch and `complete` handling, offset
   preservation. **History from before attach becomes reachable here.**
5. **Drift guard** — a characterisation test pinning vt100's own offset compensation. Originally specified as
   a compensation Dock would implement; the investigation found vt100 already does it and that a second
   increment would un-pin the content, so what remains is the test and no production change.
6. **Tab strip** — entirely independent of 1–5, and safe to build in parallel or first if the pane work
   stalls.

## 8. Risks

**16 MB × pane count is real memory.** Eight panes is 128 MB worst case, though a typical pane holds far
less — the log is bytes, not vt100 cell grids, and the cap is a ceiling rather than an allocation. Measured
in step 1 and reported; the flag exists so it can be lowered.

**Page-back hitches on a large rebuild.** Replaying 16 MB through vt100 is not free. Mitigated by paging in
2 MB chunks rather than the whole log, and measured in step 4. If it proves visible, the fallback is a
smaller chunk, not a different architecture.

**The alternate-screen rule is the one place a mistake corrupts rather than degrades.** It gets the
two-direction tests in step 2 and a rewritten comment, because the existing comment explicitly anticipated
this change and a future reader trusting the old text will reintroduce the bug.
