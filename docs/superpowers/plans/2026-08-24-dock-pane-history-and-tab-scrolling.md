# Deep Pane History and Tab Strip Scrolling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Dock panes hundreds of thousands of lines of scrollable history instead of only what arrived after the client attached, and make the workspace tab strip scroll so every workspace is clickable.

**Architecture:** The daemon already streams raw child bytes to subscribers precisely so a client's parser accumulates history by replay (`terminal/mod.rs:67`). This project widens that: `OutputLog` grows from a 1 MB undelivered-window buffer into a 16 MB per-pane history store, the attach seed replays a 256 KB prefix of it instead of the bare visible grid, and protocol v13 adds a `PaneHistory` request so the client can page further back on demand and rebuild its parser from the extended byte log. History stays in RAM. Separately and independently, `render_tabs` gains a scroll offset that follows the active tab.

**Tech Stack:** Rust, `vt100` 0.16, `ratatui`/`crossterm`, `serde_json` over a Unix socket, inline `#[cfg(test)] mod tests`.

**Spec:** `docs/superpowers/specs/2026-08-24-dock-pane-history-and-tab-scrolling-design.md`

## Global Constraints

- **History never touches disk.** `OutputLog` is documented in-memory only; a pane's output is every token, secret, and file body an agent printed. No task may add persistence.
- **Per-pane history budget: 16 MB default** (`PANE_HISTORY_BYTES`), overridable with `dockd --pane-history-bytes=N`.
- **Seed prefix: 256 KB.** Page-back chunk: **2 MB.**
- **Protocol version becomes 13** (`protocol.rs:11`), and the assertion at `protocol.rs:769` must be updated in the same commit as the bump.
- **All protocol structs carry `#[serde(deny_unknown_fields)]`**, matching every existing request struct.
- **Ordinary scrolling must never clone the grid.** Copy mode's freeze clones grid *and* scrollback (`vt.rs:206`); at 200k rows that cannot hold 60fps. Copy mode itself is unchanged.
- **Test naming follows the existing house style**: full descriptive sentences, e.g. `a_reader_that_keeps_up_receives_every_byte_exactly_once` (`terminal/mod.rs:196`).
- **Every task ends green**: `cargo test`, `cargo fmt --check`, `cargo clippy -- -D warnings`.

**Measurement convention — this repository already has one; do not invent a second.** Benchmarks are `#[test]` functions marked:

```rust
#[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
```

They **print numbers and assert nothing**, because a timing assertion on a laptop running a test suite is a flake generator. `measure_frame` (`dashboard.rs:10765`) reports the *fastest* of 7 rounds plus allocations and bytes per frame, and its doc comment explains why the minimum rather than the mean: *"noise only ever makes a round slower… a mean here moved by 40% between back-to-back runs of the same binary and hid a real 25% improvement."* Follow that shape exactly.

The three existing measurements this project must run before and after:

| Benchmark | Location | Covers |
|---|---|---|
| `measure_the_daemon_hot_path_under_a_dashboard_sized_load` | `server.rs:2664` | daemon idle + streaming, 16 panes |
| `measure_what_a_subscriber_whose_client_has_gone_still_costs` | `server.rs:2815` | subscriber cost |
| `render_measurement_of_a_busy_dashboard_at_three_terminal_sizes` | `dashboard.rs:10795` | the 60fps render path |

Run: `cargo test --release --lib -- --ignored --nocapture`. Record before/after numbers in the commit body of every task that touches a hot path. Per the standing rule, this is measured per feature, not audited at the end.

**Existing test helpers — reuse, do not duplicate.** `dashboard()` (`:5613`), `bound_dashboard()`, `benchmark_dashboard(workspaces, panes_each)` (`:10698`), `render_to_string(dashboard, width, height)` (`:5678`), `attach_event(run_id, bytes)` (`:5733`), `PANE_ROWS`/`PANE_COLS` (`:5751`), `registry_with_scrollback(rows)` and `exchange(&[..])`/`hello()` in `server.rs`.

---

### Task 1: The terminal layer learns to retain and report history

Raises the retention budget, adds the two `OutputLog` readers the history path needs, and adds the two `VtTerminal` getters every later task depends on. Terminal-layer only — nothing observable changes yet, which is what makes this safe to land first.

**Files:**
- Modify: `src/terminal/mod.rs:60-152` (constant, `OutputLog` readers, doc comments)
- Modify: `src/terminal/vt.rs:45-60` (`history_capacity` field), `:178-197` (getters)
- Modify: `src/runtime.rs:24`, `src/runtime.rs:148`, `src/runtime.rs:1891` (constant rename)
- Test: `src/terminal/mod.rs` and `src/terminal/vt.rs` inline `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub const PANE_HISTORY_BYTES: usize`; `OutputLog::tail(&self, max: usize) -> (u64, Vec<u8>)`; `OutputLog::before(&self, before: u64, max: usize) -> (u64, Vec<u8>, bool)`; `VtTerminal::history_rows(&mut self) -> usize`; `VtTerminal::history_capacity(&self) -> usize`. Task 3 uses `tail`, Task 4 uses `before`, Tasks 5 and 6 use both getters.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `src/terminal/mod.rs`:

```rust
#[test]
fn a_tail_returns_the_newest_bytes_and_the_sequence_they_begin_at() {
    let mut log = OutputLog::new(1024);
    log.append(b"oldest");
    log.append(b"middle");
    log.append(b"newest");
    let (from, bytes) = log.tail(12);
    assert_eq!(bytes, b"middlenewest");
    assert_eq!(from, 6);
}

#[test]
fn a_tail_keeps_the_newest_write_even_when_it_alone_exceeds_the_budget() {
    let mut log = OutputLog::new(1024);
    log.append(b"old");
    log.append(b"a_single_enormous_write");
    let (from, bytes) = log.tail(4);
    assert_eq!(bytes, b"a_single_enormous_write");
    assert_eq!(from, 3);
}

#[test]
fn a_tail_of_an_empty_log_begins_at_the_end_and_carries_nothing() {
    let log = OutputLog::new(1024);
    assert_eq!(log.tail(64), (0, Vec::new()));
}

#[test]
fn history_before_a_cursor_extends_backwards_without_a_gap() {
    let mut log = OutputLog::new(1024);
    log.append(b"aaaa");
    log.append(b"bbbb");
    log.append(b"cccc");
    // The client holds everything from sequence 8; ask for what precedes it.
    let (from, bytes, complete) = log.before(8, 4);
    assert_eq!(bytes, b"bbbb");
    assert_eq!(from, 4);
    assert!(!complete, "sequence 0 is still retained and unasked for");
    let (from, bytes, complete) = log.before(from, 4);
    assert_eq!(bytes, b"aaaa");
    assert_eq!(from, 0);
    assert!(complete, "nothing older is retained");
}

#[test]
fn history_before_a_cursor_inside_a_write_stops_exactly_at_the_cursor() {
    let mut log = OutputLog::new(1024);
    log.append(b"abcdefgh");
    // A cursor that is not a write boundary must not pull in bytes the caller already has,
    // and must not leave a gap between what it returns and where the caller starts.
    let (from, bytes, complete) = log.before(5, 64);
    assert_eq!(bytes, b"abcde");
    assert_eq!(from, 0);
    assert!(complete);
}

#[test]
fn history_before_the_oldest_retained_byte_is_empty_and_complete() {
    let mut log = OutputLog::new(8);
    log.append(b"aaaa");
    log.append(b"bbbb");
    log.append(b"cccc");
    let (_, dropped, _) = log.before(4, 64);
    assert!(dropped.is_empty(), "the first write was evicted at capacity");
}
```

And in `src/terminal/vt.rs`'s `mod tests`:

```rust
#[test]
fn a_terminal_reports_how_many_rows_of_history_it_actually_holds() {
    let mut term = VtTerminal::new(2, 20, 100);
    assert_eq!(term.history_rows(), 0);
    for line in 0..10 {
        term.feed(format!("line {line}\r\n").as_bytes());
    }
    assert!(term.history_rows() >= 8, "ten lines through a two-row screen");
}

#[test]
fn reading_the_history_row_count_leaves_the_viewport_where_it_found_it() {
    let mut term = VtTerminal::new(2, 20, 100);
    for line in 0..10 {
        term.feed(format!("line {line}\r\n").as_bytes());
    }
    term.scroll_by(3);
    assert_eq!(term.scroll_offset(), 3);
    let _ = term.history_rows();
    assert_eq!(
        term.scroll_offset(),
        3,
        "the clamp trick moves the offset to read it and must put it back"
    );
}

#[test]
fn a_terminal_reports_the_history_capacity_it_was_built_with() {
    let term = VtTerminal::new(2, 20, 100);
    assert_eq!(term.history_capacity(), 100, "vt100 does not expose this either");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib terminal::tests`
Expected: FAIL — `no method named 'tail' found`, `no method named 'before' found`.

- [ ] **Step 3: Implement the readers**

In `src/terminal/mod.rs`, replace the constant (currently at `:65`) and its doc comment:

```rust
/// How many bytes of raw child output each pane retains, and therefore how far back a person
/// can scroll.
///
/// This bounds two things that used to be one small thing. It is still the *undelivered*
/// window — a subscriber that has fallen further behind than this must re-seed, which
/// `OutputLog::since` reports rather than papering over — and it is now also the pane's
/// history: the seed a client is given is a replay of this log, so what is retained here is
/// what anyone can scroll back to. One number for both is right because a subscriber more
/// than 16 MB behind has genuinely stalled, and 16 MB of raw output is hundreds of thousands
/// of lines.
///
/// It is deliberately in-memory only. A pane's output is every token, secret, and file body
/// an agent printed, and scrollback depth is not worth writing that to disk for.
pub const PANE_HISTORY_BYTES: usize = 16 << 20;
```

Add both readers to `impl OutputLog`, after `since`:

```rust
    /// The newest `max` bytes or fewer, and the sequence they begin at.
    ///
    /// Whole writes, oldest-first, so a replay begins where the child began one — the closest
    /// thing to a safe parser entry point this log has. It is not a guarantee: a write
    /// boundary is not an escape-sequence boundary, so the oldest row of a replayed tail may
    /// carry one malformed glyph. The visible screen is repaired by `ScreenSync` regardless,
    /// which is what makes replaying an arbitrary tail safe at all.
    ///
    /// The newest write is always included even when it alone exceeds `max`, for the same
    /// reason `append` never drops it: a caller given nothing cannot make progress.
    pub fn tail(&self, max: usize) -> (u64, Vec<u8>) {
        let mut first = self.chunks.len();
        let mut bytes = 0;
        for (index, (_, chunk)) in self.chunks.iter().enumerate().rev() {
            if bytes + chunk.len() > max && bytes > 0 {
                break;
            }
            bytes += chunk.len();
            first = index;
        }
        let from = self
            .chunks
            .get(first)
            .map_or(self.end, |(start, _)| *start);
        let mut out = Vec::with_capacity(bytes);
        for (_, chunk) in self.chunks.iter().skip(first) {
            out.extend_from_slice(chunk);
        }
        (from, out)
    }

    /// The `max` bytes immediately preceding `before`, for a reader extending its history
    /// backwards.
    ///
    /// Where [`since`](Self::since) refuses with `None`, this clamps. That difference is the
    /// point: `since` serves the delta path, where skipping bytes renders as corruption with
    /// nothing to attribute it to, and this serves the history path, where "that is all I
    /// still have" is a true and useful answer.
    ///
    /// A `before` that falls inside a write is truncated to it rather than skipping that
    /// write, so the answer always abuts the caller's cursor exactly. Returns the sequence the
    /// answer begins at, and whether it reached the oldest byte still retained — once that is
    /// true there is nothing older to ask for.
    pub fn before(&self, before: u64, max: usize) -> (u64, Vec<u8>, bool) {
        let mut pieces: Vec<(u64, &[u8])> = Vec::new();
        let mut bytes = 0;
        for (start, chunk) in self.chunks.iter().rev() {
            if *start >= before {
                continue;
            }
            let usable = usize::try_from(before - start)
                .unwrap_or(chunk.len())
                .min(chunk.len());
            if usable == 0 {
                continue;
            }
            if bytes + usable > max && !pieces.is_empty() {
                break;
            }
            bytes += usable;
            pieces.push((*start, &chunk[..usable]));
        }
        let from = pieces.last().map_or(before, |(start, _)| *start);
        let mut out = Vec::with_capacity(bytes);
        for (_, piece) in pieces.iter().rev() {
            out.extend_from_slice(piece);
        }
        (from, out, from <= self.start())
    }
```

Then update the three references to the old constant name: the import at `src/runtime.rs:24` and its uses at `src/runtime.rs:148` and `src/runtime.rs:1891`.

In `src/terminal/vt.rs`, store the capacity on the struct (vt100 does not expose the configured value — `Grid::scrollback_len()` is private) and add both getters beside `scroll_offset` at `:187`:

```rust
    /// How many rows of history this terminal actually holds, as opposed to how many it is
    /// allowed to.
    ///
    /// `vt100` exposes no getter for it, but `set_scrollback` clamps to the real length
    /// (`grid.rs:198`), so setting it past the end and reading it back reports that length.
    /// The offset found on entry is restored, which is why this takes `&mut self` for what
    /// reads like a getter.
    pub fn history_rows(&mut self) -> usize {
        let saved = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let rows = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(saved);
        rows
    }

    /// Rows of history this terminal may retain, fixed when it was built. Recorded here
    /// because `vt100` keeps the configured capacity to itself.
    pub fn history_capacity(&self) -> usize {
        self.history_capacity
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib terminal:: && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Record the ceiling**

Run: `cargo test --lib 2>&1 | tail -3`
Record the total test count in the commit body. Note that the per-pane ceiling moved 1 MB → 16 MB and that it is a ceiling, not an allocation — `OutputLog::new` allocates an empty `VecDeque`, so an idle canvas costs nothing extra. No benchmark run is needed here: nothing on a hot path changed.

- [ ] **Step 6: Commit**

```bash
git add src/terminal/mod.rs src/terminal/vt.rs src/runtime.rs
git commit -m "feat: let a pane retain enough output to be worth scrolling

OutputLog was a 1MB undelivered-window buffer. It becomes the pane's
history store at 16MB, with two readers the history path needs: a tail
for the attach seed, and a clamping backwards reader for paging. The
comment saying it bounds only the undelivered window and not history is
no longer true, so it now says what it actually bounds."
```

---

### Task 2: The history budget becomes configurable

Plumbs `--pane-history-bytes` from `dockd` to the two places a `PaneOutput` is actually built for a real pane. `launch_fixture` deliberately keeps the constant: it is a test-only path, and threading a parameter through its dozens of call sites would be churn for no behaviour.

**Files:**
- Modify: `src/runtime.rs:124-149` (`OwnedRuntime::launch` and `launch_with_before_lifecycle_publish` signatures)
- Modify: `src/dispatch.rs:467-560` (`RuntimeRegistry` field and builder), `src/dispatch.rs:1406`, `src/dispatch.rs:2142` (call sites)
- Modify: `src/bin/dockd.rs:13-56` (flag parsing and usage string)
- Test: `src/dispatch.rs` inline `mod tests`

**Interfaces:**
- Consumes: `PANE_HISTORY_BYTES` from Task 1.
- Produces: `RuntimeRegistry::with_pane_history_bytes(self, bytes: usize) -> Self`; `RuntimeRegistry::pane_history_bytes(&self) -> usize`. Task 3 calls `pane_history_bytes()` to derive the announced row capacity.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/dispatch.rs`:

```rust
#[test]
fn a_registry_reports_the_pane_history_budget_it_was_built_with() {
    let dir = tempfile::tempdir().expect("temp dir");
    let registry = RuntimeRegistry::new(dir.path(), 2000).expect("registry");
    assert_eq!(
        registry.pane_history_bytes(),
        crate::terminal::PANE_HISTORY_BYTES,
        "an unconfigured registry uses the default budget"
    );
    let configured = RuntimeRegistry::new(dir.path(), 2000)
        .expect("registry")
        .with_pane_history_bytes(4 << 20);
    assert_eq!(configured.pane_history_bytes(), 4 << 20);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib a_registry_reports_the_pane_history_budget`
Expected: FAIL — `no method named 'pane_history_bytes'`.

- [ ] **Step 3: Implement the plumbing**

In `src/dispatch.rs`, add the field to `RuntimeRegistry` beside `scrollback_rows` (declared at `:248`):

```rust
    /// Bytes of raw output each pane retains, and therefore how far back a person can scroll.
    /// Separate from `scrollback_rows`, which is only what the daemon's own parser keeps: the
    /// daemon renders nothing, so its parser depth serves detection, and this serves people.
    pane_history_bytes: usize,
```

Initialise it to `crate::terminal::PANE_HISTORY_BYTES` in `with_capacity` (beside the `scrollback_rows` assignment at `:557`), and add both accessors next to `scrollback_rows()` (`:3256`):

```rust
    /// Bytes of raw output every pane retains. Announced to subscribers, in rows, so a
    /// client's replica is sized to hold the history it will be sent rather than the
    /// daemon's own parser depth.
    pub fn pane_history_bytes(&self) -> usize {
        self.pane_history_bytes
    }

    #[must_use]
    pub fn with_pane_history_bytes(mut self, bytes: usize) -> Self {
        self.pane_history_bytes = bytes;
        self
    }
```

In `src/runtime.rs`, add a `history_bytes: usize` parameter to `OwnedRuntime::launch` and `launch_with_before_lifecycle_publish`, immediately after `scrollback_rows`, and use it at `:148` in place of the constant. `launch_fixture` keeps `PANE_HISTORY_BYTES`.

Update the two call sites in `src/dispatch.rs` (`:1406` and `:2142`) to pass `self.pane_history_bytes`.

In `src/bin/dockd.rs`, add beside the existing `--scrollback-rows` arm (`:26`):

```rust
        } else if let Some(value) = argument.strip_prefix("--pane-history-bytes=") {
            pane_history_bytes = value
                .parse()
                .map_err(|_| "--pane-history-bytes must be a positive integer")?;
            if pane_history_bytes == 0 {
                return Err("--pane-history-bytes must be greater than zero".into());
            }
```

Declare `let mut pane_history_bytes = dock::terminal::PANE_HISTORY_BYTES;` beside `capacity` (`:13`), apply it with `.with_pane_history_bytes(pane_history_bytes)` where the registry is built, and add `[--pane-history-bytes=N]` to the usage string at `:53`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Verify the flag reaches the daemon**

Run: `cargo run --bin dockd -- --pane-history-bytes=0 2>&1 | head -2`
Expected: `--pane-history-bytes must be greater than zero`.
Run: `cargo run --bin dockd -- --pane-history-bytes=notanumber 2>&1 | head -2`
Expected: `--pane-history-bytes must be a positive integer`.

- [ ] **Step 6: Commit**

```bash
git add src/runtime.rs src/dispatch.rs src/bin/dockd.rs
git commit -m "feat: let the pane history budget be set, so it can be lowered

16MB a pane is a ceiling rather than an allocation, but it is the
daemon holding it and the daemon holds every pane at once. The flag is
how someone running a canvas full of chatty agents takes it back."
```

---

### Task 3: The seed carries history

The step that makes deep scrollback real. Also the step with the sharpest edge: replaying raw history invalidates the alternate-screen assumption the current seed rests on, and the comment at `server.rs:698-710` explicitly warned that this change would do that.

**Files:**
- Modify: `src/server.rs:698-724` (`PaneSubscriberView::seeded` and its doc comment)
- Modify: `src/server.rs:575` (the `scrollback_rows` the attach frame announces)
- Test: `src/server.rs` inline `mod tests`

**Interfaces:**
- Consumes: `OutputLog::tail` (Task 1), `RuntimeRegistry::pane_history_bytes` (Task 2).
- Produces: `const SEED_HISTORY_BYTES: usize = 256 * 1024;` in `src/server.rs`. Task 5 relies on the client's replica being sized to hold a full history.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/server.rs`, beside the existing `PaneOutput` tests at `:2095`:

```rust
#[test]
fn a_seed_carries_the_panes_history_and_not_just_its_visible_screen() {
    let mut output = PaneOutput::new(2, 20, 100, 4096);
    for line in 0..20 {
        output.feed(format!("line {line}\r\n").as_bytes());
    }
    let (_, _, bytes) = PaneSubscriberView::seeded(&output, 2, 20);
    let seeded = String::from_utf8_lossy(&bytes);
    assert!(
        seeded.contains("line 0"),
        "the seed must replay history, not just the two visible rows: {seeded:?}"
    );
}

#[test]
fn a_seed_whose_history_ends_in_the_alternate_screen_returns_a_primary_pane_to_primary() {
    let mut output = PaneOutput::new(4, 20, 100, 4096);
    output.feed(b"history\r\n");
    output.feed(b"\x1b[?1049h");     // a full-screen program starts
    output.feed(b"inside the program");
    output.feed(b"\x1b[?1049l");     // and exits, leaving the pane on primary
    assert!(!output.screen().alternate_screen());
    let (_, _, bytes) = PaneSubscriberView::seeded(&output, 4, 20);
    let mut replica = PaneScreen::new(4, 20, 100);
    replica.feed(&bytes);
    assert!(
        !replica.alternate_screen(),
        "a replica left in the alternate screen would paint over the user's history"
    );
}

#[test]
fn a_seed_for_a_pane_inside_the_alternate_screen_puts_the_replica_there() {
    let mut output = PaneOutput::new(4, 20, 100, 4096);
    output.feed(b"history\r\n");
    output.feed(b"\x1b[?1049h");
    output.feed(b"inside the program");
    assert!(output.screen().alternate_screen());
    let (_, _, bytes) = PaneSubscriberView::seeded(&output, 4, 20);
    let mut replica = PaneScreen::new(4, 20, 100);
    replica.feed(&bytes);
    assert!(replica.alternate_screen());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib a_seed_`
Expected: the first FAILs (the seed carries only the visible grid, so "line 0" is absent). The alternate-screen pair may pass incidentally today; they are regression cover for step 3 and must still pass after it.

- [ ] **Step 3: Replace the seed body**

In `src/server.rs`, add the constant above `PaneSubscriberView`:

```rust
/// How much retained output rides along with an attach frame.
///
/// Enough that scrolling up is instant for the distance anyone scrolls without thinking, and
/// small enough that attaching to a canvas of panes is not a stall: this is paid per pane, on
/// every client start and every re-seed. Everything older is paged in on demand.
const SEED_HISTORY_BYTES: usize = 256 * 1024;
```

Replace `seeded` and rewrite its doc comment — the existing text describes a rule this change replaces, and a reader who trusts it will reintroduce the bug:

```rust
    /// A fresh subscriber's starting point: a replay of the pane's recent history, followed by
    /// whatever correction that replay did not achieve on its own.
    ///
    /// Replaying raw history rather than sending the visible grid is what gives the client
    /// something to scroll back through: a repaint is cursor-addressed and never scrolls a row
    /// into scrollback, so a client seeded with one begins with no history at all.
    ///
    /// **The alternate screen is handled by comparison, not by assumption.** The replayed bytes
    /// may themselves contain `1049h`/`1049l`, so after replay this subscriber's parser can be
    /// in either buffer, and the older rule here — always land in a fresh primary buffer, so
    /// only `1049h` is ever needed — no longer holds. Instead the seed's own `ScreenSync` is
    /// asked which buffer the replay reached, and the corrective sequence is appended only when
    /// it disagrees with the live screen. Both directions matter: a replica left in the
    /// alternate screen paints a full-screen program over the user's history, and a replica left
    /// on primary renders a full-screen program into scrollback.
    fn seeded(output: &PaneOutput, rows: u16, cols: u16) -> (Self, u64, Vec<u8>) {
        let (from, mut bytes) = output.log().tail(SEED_HISTORY_BYTES);
        let mut view = Self {
            sync: ScreenSync::new(rows, cols),
            size: (rows, cols),
            offset: output.log().end(),
            epoch: output.log().epoch(),
        };
        view.sync.apply(&bytes);
        if view.sync.alternate_screen() != output.screen().alternate_screen() {
            let correction: &[u8] = if output.screen().alternate_screen() {
                b"\x1b[?1049h"
            } else {
                b"\x1b[?1049l"
            };
            bytes.extend_from_slice(correction);
            view.sync.apply(correction);
        }
        (view, from, bytes)
    }
```

The `from` it now returns is the sequence the replayed bytes begin at. Task 4 puts it on the wire as the client's paging cursor; in this task the single call site binds it as `_history_from`, so this signature is settled once rather than changed twice.

`ScreenSync` needs the accessor this uses. Add to `impl ScreenSync` in `src/terminal/mod.rs`, beside `cursor()`:

```rust
    /// Which buffer this subscriber's replica is in. The seed compares it against the live
    /// screen rather than assuming a fresh parser is on primary, because a replayed history
    /// can leave it in either.
    pub fn alternate_screen(&self) -> bool {
        self.sent.alternate_screen()
    }
```

The `let _ = from;` placeholder above is removed in Task 4, which is where `from` becomes the client's paging cursor. Leaving it unused here keeps this task's diff to one behaviour.

Finally, at `src/server.rs:575`, the announced `scrollback_rows` stops being the daemon's parser retention. Replace it with a row capacity derived from the byte budget, so a replica is sized to hold the history it will be sent:

```rust
                                // Rows the replica must retain to hold everything it can be
                                // sent. Derived from the byte budget at a deliberately
                                // pessimistic 8 bytes a row: over-sizing costs an empty
                                // VecDeque slot per row, under-sizing silently discards
                                // replayed history off the top, and only one of those is a bug
                                // a person would ever see.
                                scrollback_rows: u32::try_from(pane_history_bytes / 8)
                                    .unwrap_or(u32::MAX),
```

Thread `pane_history_bytes` into that function from `runtime.pane_history_bytes()` exactly as `scrollback_rows` is threaded today.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean. Pay attention to any pre-existing seed test that asserted the seed equals `state_bytes()` — if one exists it is now wrong and must be updated to assert the *rendered* screen matches rather than the bytes.

- [ ] **Step 5: Measure, before and after**

Run the existing daemon benchmarks on the commit *before* this task and again after, and record both:

```bash
cargo test --release --lib -- --ignored --nocapture 2>&1 | tee /tmp/after.txt
```

Attach is the path this task changes, and no existing benchmark covers it, so add one in `src/server.rs` following the house convention — printed, not asserted:

```rust
    /// What attaching a subscriber to a pane with a full history costs.
    ///
    /// This is paid per pane on every client start and every re-seed, so it is the number that
    /// decides whether the seed prefix is the right size. Fastest of several rounds, for the
    /// reason `measure_frame` gives: noise only ever makes a round slower.
    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_seeding_a_pane_with_its_history_costs() {
        let mut output = PaneOutput::new(40, 160, 100_000, PANE_HISTORY_BYTES);
        for line in 0..200_000 {
            output.feed(format!("line {line} of a long build log\r\n").as_bytes());
        }
        let mut fastest = f64::MAX;
        let mut size = 0;
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let (_, _, bytes) = PaneSubscriberView::seeded(&output, 40, 160);
            fastest = fastest.min(start.elapsed().as_secs_f64() * 1000.0);
            size = bytes.len();
        }
        println!("\nseed of a full pane: {size} bytes in {fastest:.2}ms");
    }
```

Record the printed figure in the commit body. If it is large enough to stall attaching a canvas, lower `SEED_HISTORY_BYTES` — the rest pages in on demand, so the prefix is a tuning knob, not a correctness one.

- [ ] **Step 6: Commit**

```bash
git add src/server.rs src/terminal/mod.rs
git commit -m "feat: seed a pane with its history, not just its visible screen

The client already built its replica with the daemon's retention and a
comment saying it holds exactly the history the daemon holds. The
capacity matched; the content never did, because the seed was the
visible grid. It is now a replay of the retained log.

The alternate-screen rule had to change with it. Replayed history can
carry 1049h/1049l, so a seed no longer always lands in a fresh primary
buffer, and the 1049l the old comment said was deliberately absent is
now required. The seed asks its own ScreenSync which buffer the replay
reached and corrects only on disagreement."
```

---

### Task 4: Protocol v13 — the `PaneHistory` request

Wire and daemon handler only. No client behaviour yet, so this lands green without any UI change.

**Files:**
- Modify: `src/protocol.rs:11` (version), `:16-35` (`Request`), `:265-285` (`Event::PaneAttached`), `Response` enum, `:769` (version assertion)
- Modify: `src/server.rs:384` region (request dispatch), `src/server.rs:575` (attach frame fields)
- Test: `src/protocol.rs` and `src/server.rs` inline `mod tests`

**Interfaces:**
- Consumes: `OutputLog::before` (Task 1), `OutputLog::epoch`, `RuntimeRegistry::with_run_output` (`dispatch.rs:3238`).
- Produces: `Request::PaneHistory(PaneHistoryRequest)`, `Response::PaneHistory { .. }`, and the `history_from`/`epoch` fields on `Event::PaneAttached`. Task 5 consumes all three.

- [ ] **Step 1: Write the failing tests**

In `src/protocol.rs` `mod tests`:

```rust
#[test]
fn a_pane_history_request_round_trips_and_rejects_unknown_fields() {
    let request = Request::PaneHistory(PaneHistoryRequest {
        run_id: "run_1".into(),
        before: 4096,
        max_bytes: 2 << 20,
    });
    let wire = serde_json::to_string(&request).expect("encode");
    assert_eq!(
        serde_json::from_str::<Request>(&wire).expect("decode"),
        request
    );
    assert!(
        serde_json::from_str::<Request>(
            r#"{"request":"pane_history","run_id":"r","before":0,"max_bytes":1,"extra":1}"#
        )
        .is_err(),
        "an unknown field must be refused like every other request"
    );
}

#[test]
fn an_attach_frame_carries_the_cursor_and_epoch_a_client_needs_to_page_back() {
    let event = Event::PaneAttached {
        run_id: "run_1".into(),
        revision: 4,
        rows: 40,
        cols: 120,
        scrollback_rows: 2000,
        history_from: 8192,
        epoch: 7,
        screen: String::new(),
    };
    let wire = serde_json::to_string(&event).expect("encode");
    assert!(wire.contains(r#""history_from":8192"#), "{wire}");
    assert!(wire.contains(r#""epoch":7"#), "{wire}");
}

#[test]
fn the_protocol_version_records_the_pane_history_request() {
    assert_eq!(PROTOCOL_VERSION, 13);
}
```

In `src/server.rs` `mod tests`, a handler test over the socket using the harness the neighbouring request tests already use — `exchange(&[&hello(), &serde_json::to_string(&request)])`, as at `server.rs:2001` — against a daemon holding one fixture pane. Model the setup on the nearest existing test that launches a pane and types into it, and assert:

```rust
    // A pane that has written a known amount, asked for history behind the cursor its attach
    // frame reported. `epoch` must match, `from` must not run ahead of the cursor, and a
    // fixture short enough to fit the budget must report that it is complete.
    match response {
        Response::PaneHistory { epoch, from, complete, bytes, .. } => {
            assert_eq!(epoch, attached_epoch, "the same byte stream");
            assert!(from <= attached_history_from);
            assert!(complete, "a short fixture retains everything it wrote");
            assert!(!STANDARD.decode(&bytes).expect("base64").is_empty());
        }
        other => panic!("expected history, got {other:?}"),
    }
```

Do not introduce a second daemon harness; `exchange`/`hello()` and `registry_with_scrollback` are what this module uses.

Also assert the refusal path, which has no fixture cost:

```rust
#[test]
fn history_for_a_run_the_daemon_does_not_have_is_refused_rather_than_answered_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = RuntimeRegistry::new(dir.path(), 2000).expect("registry");
    assert!(
        runtime
            .with_run_output("no-such-run", |output| output.log().end())
            .is_none(),
        "an empty answer and a missing pane must not look the same to a client"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib pane_history`
Expected: FAIL — `no variant named 'PaneHistory'`, `PROTOCOL_VERSION` is 12.

- [ ] **Step 3: Implement the protocol and handler**

In `src/protocol.rs`: bump `PROTOCOL_VERSION` to `13`; update the assertion at `:769`; add the variant to `Request`; add the struct beside the other request structs:

```rust
/// A request for output older than the caller already holds.
///
/// The caller names the sequence it starts at rather than an offset or a line count, because
/// the log is addressed by byte sequence and a line count cannot survive a resize. `epoch` in
/// the response is what makes a stale cursor safe: a run that restarted has a new byte stream,
/// and a cursor from the old one names a position in it that means nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneHistoryRequest {
    pub run_id: String,
    /// The sequence the caller's own history begins at. The answer ends exactly here.
    pub before: u64,
    /// An upper bound on the answer, clamped daemon-side to what the log can hold.
    pub max_bytes: u32,
}
```

Add to `Response`:

```rust
    /// Output older than the caller's cursor. `complete` says the answer reaches the oldest
    /// byte still retained, so there is nothing further back to ask for.
    PaneHistory {
        run_id: String,
        epoch: u64,
        from: u64,
        bytes: String,
        complete: bool,
    },
```

Add the two fields to `Event::PaneAttached` and extend its doc comment to say what they are for:

```rust
        /// The sequence the seeded bytes begin at: the caller's cursor for paging further
        /// back. Without it a client cannot name where its own history starts.
        history_from: u64,
        /// Identity of the byte stream these sequences belong to. A run that restarted gets a
        /// new one, so a client holding a cursor from before the restart discards it rather
        /// than paging into the middle of a different stream.
        epoch: u64,
```

Fill both at the emit site (`src/server.rs:575`) from the `from` value `seeded` now returns — change `seeded` to return `(Self, u64, Vec<u8>)` and drop the `let _ = from;` placeholder Task 3 left — and from `output.log().epoch()`.

Add the handler beside `Request::PaneResize` (`src/server.rs:384`):

```rust
            Ok(Request::PaneHistory(request)) => {
                // Clamped to what the log can hold: a request for more than that is not an
                // error, it is a caller that does not know the budget, and the honest answer
                // is everything there is.
                let max = (request.max_bytes as usize).min(runtime.pane_history_bytes());
                let served = runtime.with_run_output(&request.run_id, |output| {
                    let (from, bytes, complete) = output.log().before(request.before, max);
                    (output.log().epoch(), from, bytes, complete)
                });
                match served {
                    Some((epoch, from, bytes, complete)) => write_response(
                        stream,
                        &Response::PaneHistory {
                            run_id: request.run_id,
                            epoch,
                            from,
                            bytes: STANDARD.encode(&bytes),
                            complete,
                        },
                    )?,
                    None => write_response(
                        stream,
                        &Response::Error {
                            code: ErrorCode::NotFound,
                            message: format!("no live pane {}", request.run_id),
                        },
                    )?,
                }
            }
```

Use whichever `ErrorCode` variant the neighbouring handlers use for an unknown run; do not add a new one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo test --bins && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean. Every hand-written `Event::PaneAttached` literal in tests (`client.rs:502`, `:597`, `protocol.rs:832`, `:860`) needs the two new fields.

The dashboard's `attach_event` helper (`dashboard.rs:5733`) needs them too. Give it the defaults and add the explicit-cursor variant the next task's tests use, rather than a parallel helper:

```rust
    fn attach_event(run_id: &str, bytes: &[u8]) -> Event {
        attach_event_at(run_id, bytes, 0, 1)
    }

    /// An attach frame with a chosen paging cursor and epoch, for the tests that care where a
    /// replica's own history starts and which byte stream it belongs to.
    fn attach_event_at(run_id: &str, bytes: &[u8], history_from: u64, epoch: u64) -> Event {
        let mut source = crate::terminal::VtTerminal::new(PANE_ROWS, PANE_COLS, 0);
        source.feed(bytes);
        Event::PaneAttached {
            run_id: run_id.into(),
            revision: 1,
            rows: PANE_ROWS,
            cols: PANE_COLS,
            scrollback_rows: 2000,
            history_from,
            epoch,
            screen: STANDARD.encode(source.state_bytes()),
        }
    }

    /// One delta of raw child output. Revisions must be contiguous or `apply_event` drops the
    /// screen rather than advancing it into a corrupted grid, so this counts for the caller.
    fn delta_event(run_id: &str, revision: u64, bytes: &[u8]) -> Event {
        Event::PaneDelta {
            run_id: run_id.into(),
            revision,
            bytes: STANDARD.encode(bytes),
        }
    }
```

- [ ] **Step 5: Commit**

```bash
git add src/protocol.rs src/server.rs
git commit -m "feat: let a client ask for output older than it holds (protocol v13)

The subscription is one-way by design, so history cannot be pushed on
request; PaneHistory rides the ordinary request connection instead. The
attach frame now names where the seeded bytes start, which is the
cursor a client pages back from, and the log epoch, which is what makes
a cursor from a restarted run safe to throw away."
```

---

### Task 5: The client pages back

Where history from before the attach becomes reachable.

**Files:**
- Modify: `src/dashboard.rs:672-695` (`apply_event` `PaneAttached` arm), `:3792` (`scroll_pane`), `:4777` (wheel arm)
- Modify: `src/main.rs:654-680` (route the response back, as `Request::Queue` already is)
- Test: `src/dashboard.rs` inline `mod tests`

**Interfaces:**
- Consumes: `Request::PaneHistory`, `Response::PaneHistory`, `Event::PaneAttached { history_from, epoch }` (Task 4).
- Produces: `Dashboard::apply_pane_history_response(&mut self, response: Response)`; per-run state `PaneHistoryCursor { epoch: u64, from: u64, complete: bool }`. Task 6 relies on `self.screens` still being the only parser per run.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn scrolling_to_the_top_of_what_a_pane_holds_asks_for_what_came_before() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
    // Walk the viewport to the top of the replica's own history.
    for _ in 0..200 {
        dashboard.scroll_pane("run_1", 3);
    }
    let command = dashboard.mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(command, UiCommand::Request(request)
            if matches!(request.as_ref(), Request::PaneHistory(r) if r.before == 4096)),
        "at the top of its history the client must ask for the bytes before its cursor"
    );
}

#[test]
fn history_that_arrives_extends_the_pane_upwards_without_moving_what_is_on_screen() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
    for _ in 0..10 {
        dashboard.scroll_pane("run_1", 3);
    }
    let before = dashboard.screens["run_1"].scroll_offset();
    let visible = dashboard.screens["run_1"].screen().contents();
    dashboard.apply_pane_history_response(Response::PaneHistory {
        run_id: "run_1".into(),
        epoch: 1,
        from: 0,
        bytes: STANDARD.encode(b"older\r\nolder\r\nolder\r\n"),
        complete: true,
    });
    assert_eq!(
        dashboard.screens["run_1"].scroll_offset(),
        before,
        "the offset is measured from the bottom, so more history above must not move the view"
    );
    assert_eq!(
        dashboard.screens["run_1"].screen().contents(),
        visible,
        "the same rows must still be on screen"
    );
}

#[test]
fn a_pane_that_has_reached_the_oldest_retained_byte_stops_asking() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event_at("run_1", b"", 4096, 1));
    dashboard.apply_pane_history_response(Response::PaneHistory {
        run_id: "run_1".into(),
        epoch: 1,
        from: 0,
        bytes: STANDARD.encode(b"oldest\r\n"),
        complete: true,
    });
    for _ in 0..500 {
        dashboard.scroll_pane("run_1", 3);
    }
    assert!(
        matches!(
            dashboard.mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            UiCommand::None
        ),
        "there is nothing older, so scrolling must not keep asking"
    );
}

#[test]
fn history_from_a_restarted_run_is_discarded_rather_than_spliced_in() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event_at("run_1", b"", 4096, 2));
    let before = dashboard.screens["run_1"].screen().contents();
    dashboard.apply_pane_history_response(Response::PaneHistory {
        run_id: "run_1".into(),
        epoch: 1, // the previous incarnation of this pane
        from: 0,
        bytes: STANDARD.encode(b"bytes from a different stream\r\n"),
        complete: true,
    });
    assert_eq!(
        dashboard.screens["run_1"].screen().contents(),
        before,
        "a cursor from before a restart names a position in a stream that no longer exists"
    );
}
```

These use `bound_dashboard()`, `attach_event_at`, and `delta_event` — the first already exists, the other two are defined in Task 4 Step 4.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib dashboard::tests::scrolling_to_the_top dashboard::tests::history_`
Expected: FAIL — `no method named 'apply_pane_history_response'`.

- [ ] **Step 3: Implement client paging**

Add to `Dashboard`:

```rust
/// What a pane's replica holds, and where it would ask for more.
///
/// `from` is the sequence its own byte log begins at — the cursor a `PaneHistory` request
/// names. `complete` means the daemon has said there is nothing older retained, which is the
/// only thing that stops the client asking again.
struct PaneHistoryCursor {
    epoch: u64,
    from: u64,
    complete: bool,
}
```

with `history: HashMap<String, PaneHistoryCursor>` and `history_bytes: HashMap<String, Vec<u8>>` beside `screens`. The byte log is capped at the same budget the daemon announced, so the two never disagree about how much history exists.

In the `PaneAttached` arm (`:672`), record the cursor and seed the byte log with the decoded seed bytes.

In `scroll_pane` (`:3792`), after scrolling, report whether the viewport is now within one screen height of the top of what the parser holds — that predicate is what the wheel arm turns into a request. In the wheel arm (`:4777`), return the request instead of `UiCommand::None` when the predicate holds and the cursor is not `complete`:

```rust
                if let Some(run_id) = run_id {
                    self.scroll_pane(&run_id, delta);
                    if let Some(request) = self.history_request_for(&run_id) {
                        return UiCommand::Request(Box::new(request));
                    }
                }
                UiCommand::None
```

```rust
    /// A request for the next chunk of history, when this pane is scrolled near the top of
    /// what it holds and the daemon has not said that is everything.
    ///
    /// 2 MB rather than the whole budget: the rebuild below replays every byte this pane
    /// holds through a fresh parser, and that cost is paid on the keystroke that asks.
    /// Takes `&mut self` because `history_rows` does: reading the row count means moving the
    /// scroll offset to the clamp and putting it back.
    fn history_request_for(&mut self, run_id: &str) -> Option<Request> {
        let before = match self.history.get(run_id) {
            Some(cursor) if !cursor.complete => cursor.from,
            _ => return None,
        };
        let screen = self.screens.get_mut(run_id)?;
        let (rows, _) = screen.size();
        // Rows still above the viewport. One screen height of headroom, so the request goes
        // out just before the user reaches the top rather than at the moment they hit it.
        let above = screen.history_rows().saturating_sub(screen.scroll_offset());
        if above > usize::from(rows) {
            return None;
        }
        Some(Request::PaneHistory(PaneHistoryRequest {
            run_id: run_id.to_owned(),
            before,
            max_bytes: 2 << 20,
        }))
    }
```

The `before` value is read out and the `history` borrow dropped before `screens` is borrowed mutably; taking both at once does not compile.

```rust
    /// Splices older history in front of what this pane holds, by rebuilding its parser from
    /// the extended byte log.
    ///
    /// A parser cannot be prepended to, so this is the only way history enters a replica.
    /// The viewport survives for free: `scroll_offset` is measured from the bottom, so rows
    /// added above leave it pointing at the same content.
    pub fn apply_pane_history_response(&mut self, response: Response) {
        let Response::PaneHistory { run_id, epoch, from, bytes, complete } = response else {
            if let Response::Error { message, .. } = response {
                self.error = Some(message);
            }
            return;
        };
        let Some(cursor) = self.history.get_mut(&run_id) else {
            return;
        };
        if cursor.epoch != epoch {
            // The pane restarted between the request and the answer. These bytes belong to a
            // stream this replica is not showing.
            return;
        }
        let Ok(older) = STANDARD.decode(&bytes) else {
            return;
        };
        cursor.from = from;
        cursor.complete = complete;
        let Some(log) = self.history_bytes.get_mut(&run_id) else {
            return;
        };
        log.splice(0..0, older);
        let Some(screen) = self.screens.get_mut(&run_id) else {
            return;
        };
        let (rows, cols) = screen.size();
        let offset = screen.scroll_offset();
        let mut rebuilt = PaneScreen::new(rows, cols, screen.history_capacity());
        rebuilt.feed(log);
        rebuilt.scroll_by(offset as i32);
        *screen = rebuilt;
    }
```

`history_capacity()` is Task 1's getter on the outgoing screen — the capacity it was built with, which is the `scrollback_rows` the attach frame announced. Reading it off the screen being replaced means the rebuild cannot disagree with what it replaces, and no capacity field is needed on the cursor.

Append the decoded bytes of every `PaneDelta` to `history_bytes` too, trimming from the front at the announced budget, or the log falls behind the parser and a later rebuild loses recent output.

In `src/main.rs:672`, route the response exactly as the queue response already is:

```rust
                if matches!(request.as_ref(), Request::Queue(_)) {
                    dashboard.apply_queue_response(response);
                } else if matches!(request.as_ref(), Request::PaneHistory(_)) {
                    dashboard.apply_pane_history_response(response);
                } else {
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Measure the rebuild**

Add the measurement in `src/dashboard.rs` beside the other benchmarks, following the house convention — printed, not asserted:

```rust
    /// What a page-back costs, which is the cost of the keystroke that asks for it.
    ///
    /// A parser cannot be prepended to, so extending history means replaying every byte the
    /// pane holds through a fresh parser. This is the number that decides the 2 MB chunk size.
    #[test]
    #[ignore = "a measurement, not an assertion: cargo test --release --lib -- --ignored --nocapture"]
    fn measure_what_paging_back_through_a_panes_history_costs() {
        let log: Vec<u8> = (0..40_000)
            .flat_map(|line| format!("line {line} of a long build log\r\n").into_bytes())
            .collect();
        let mut fastest = f64::MAX;
        for _ in 0..7 {
            let mut screen = PaneScreen::new(40, 160, 200_000);
            let start = std::time::Instant::now();
            screen.feed(&log);
            fastest = fastest.min(start.elapsed().as_secs_f64() * 1000.0);
        }
        println!("\npage-back rebuild: {} bytes in {fastest:.2}ms", log.len());
    }
```

Run: `cargo test --release --lib -- --ignored --nocapture` and record the figure. Also re-run `render_measurement_of_a_busy_dashboard_at_three_terminal_sizes` before and after: this task adds a per-pane byte log, and the render path must be unchanged by it.

If the rebuild is slow enough to read as a freeze, lower the page-back chunk below 2 MB — do not change the architecture.

- [ ] **Step 6: Commit**

```bash
git add src/dashboard.rs src/main.rs
git commit -m "feat: let a pane scroll back into output that arrived before you did

A parser cannot be prepended to, so history enters a replica the only
way it can: the client keeps the bytes it was sent, asks for the 2MB
before them when the viewport nears the top, and rebuilds the parser
from the extended log. The viewport survives that for free, because
vt100 measures the scroll offset from the bottom."
```

---

### Task 6: Content stops sliding under a scrolled viewport

Depends only on Task 1's `history_rows`, so it could ship immediately after it. It is placed here because its value is only visible once there is depth to scroll through.

**Files:**
- Modify: `src/dashboard.rs:695-720` (`PaneDelta` arm)
- Test: `src/dashboard.rs` inline `mod tests`

**Interfaces:**
- Consumes: `VtTerminal::history_rows` (Task 1).
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing test**

In `src/dashboard.rs`:

```rust
#[test]
fn output_arriving_under_a_scrolled_pane_does_not_slide_it_downwards() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event("run_1", b""));
    for line in 0..50 {
        dashboard.apply_event(delta_event("run_1", line + 2, format!("line {line}\r\n").as_bytes()));
    }
    dashboard.scroll_pane("run_1", 10);
    let pinned = dashboard.screens["run_1"].screen().contents();
    for line in 50..60 {
        dashboard.apply_event(delta_event("run_1", line + 2, format!("line {line}\r\n").as_bytes()));
    }
    assert_eq!(
        dashboard.screens["run_1"].screen().contents(),
        pinned,
        "a person reading scrollback must not have it pulled out from under them"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib output_arriving_under_a_scrolled`
Expected: FAIL — the pinned rows have slid downwards under the new output.

- [ ] **Step 3: Implement**

In the `PaneDelta` arm of `apply_event`, sample either side of the feed and compensate only when the pane is scrolled:

```rust
                    // A scrolled viewport is an offset from the bottom, and nothing in vt100
                    // moves it when rows are appended — so without this, output arriving
                    // underneath drags whatever a person is reading downwards and off the
                    // screen. Deep history makes that the difference between usable and not.
                    let offset = screen.scroll_offset();
                    let before = if offset > 0 { screen.history_rows() } else { 0 };
                    screen.feed(&bytes);
                    if offset > 0 {
                        let grew = screen.history_rows().saturating_sub(before);
                        screen.scroll_by(i32::try_from(grew).unwrap_or(i32::MAX));
                    }
```

Sampling only when scrolled keeps the common case — a pane following live output — at exactly its current cost.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Confirm the following case did not get slower**

This adds two `history_rows` calls to the delta path, which is the daemon-to-client hot path, so it must be measured rather than reasoned about — the guard is `offset > 0`, but a guard that is wrong costs every frame.

Run before and after: `cargo test --release --lib -- --ignored --nocapture`
Compare `measure_the_daemon_hot_path_under_a_dashboard_sized_load` and `render_measurement_of_a_busy_dashboard_at_three_terminal_sizes`. Both should be unchanged, because an unscrolled pane takes neither sample. Record both numbers in the commit body.

- [ ] **Step 6: Commit**

```bash
git add src/dashboard.rs
git commit -m "fix: stop output dragging a scrolled pane out from under the reader

vt100 measures the scroll offset from the bottom and never adjusts it
when rows arrive, so a scrolled pane slid downwards under live output.
At 2000 rows that was a nuisance; at the depth panes now hold it makes
scrolling useless. The row count vt100 declines to expose is readable
by setting the offset past the end and reading the clamp back."
```

---

### Task 7: A pane says when it has stopped following

Keyboard scrolling and the indicator. Spec §4's scroll surface, which the spec's own build sequence omitted.

**Files:**
- Modify: `src/dashboard.rs` key handling (the `Ctrl+B` prefix table), pane chrome rendering, `:2209`/`:2244` (help text)
- Test: `src/dashboard.rs` inline `mod tests`

**Interfaces:**
- Consumes: `scroll_pane` (`:3792`), `history_request_for` (Task 5), `history_rows` (Task 6).
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_prefix_and_page_keys_scroll_the_focused_pane() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event("run_1", b""));
    for line in 0..100 {
        dashboard.apply_event(delta_event("run_1", line + 2, format!("line {line}\r\n").as_bytes()));
    }
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(
        dashboard.screens["run_1"].scroll_offset() > 0,
        "Ctrl+B PageUp must scroll the focused pane back"
    );
    dashboard.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    dashboard.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(
        dashboard.screens["run_1"].scroll_offset(),
        0,
        "Ctrl+B End must return the pane to following live output"
    );
}

#[test]
fn a_scrolled_pane_says_so_rather_than_looking_hung() {
    let mut dashboard = bound_dashboard();
    dashboard.apply_event(attach_event("run_1", b""));
    for line in 0..100 {
        dashboard.apply_event(delta_event("run_1", line + 2, format!("line {line}\r\n").as_bytes()));
    }
    dashboard.scroll_pane("run_1", 12);
    let rendered = render_to_string(&mut dashboard, 80, 24);
    assert!(
        rendered.contains("End to follow"),
        "a pane that stopped following live output is indistinguishable from a hung agent: {rendered}"
    );
}
```

`render_to_string` should reuse whatever the existing dashboard render tests use to rasterise a frame.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_prefix_and_page_keys a_scrolled_pane_says_so`
Expected: FAIL — the keys do nothing, the indicator is absent.

- [ ] **Step 3: Implement**

Add three arms to the `Ctrl+B` prefix table beside the existing pane commands, each resolving the focused pane's `run_id` the way `Tab` focus already does:

- `KeyCode::PageUp` → `scroll_pane(run_id, rows / 2)`, then return `history_request_for(run_id)` as a `UiCommand::Request` if it yields one — the keyboard must page history exactly as the wheel does, or scrolling by keyboard stops at the seed boundary.
- `KeyCode::PageDown` → `scroll_pane(run_id, -(rows / 2))`.
- `KeyCode::End` → set the offset to 0 by scrolling back by the current offset.

In the pane chrome, when `scroll_offset() > 0`, render into the pane's title row:

```rust
    // A pane that has stopped following live output looks exactly like a pane whose agent has
    // hung. It has to say which it is, and say how to undo it: someone who scrolled by
    // accident with the wheel has no other way to find out.
    format!("▲ {} rows · End to follow", offset)
```

Use `theme.muted`, and truncate with the existing `ellipsise` helper so a narrow pane drops the sentence rather than the title.

Add the keys to both help surfaces at `:2209` and `:2244`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: scroll a pane from the keyboard, and say when it has stopped following

Ctrl+B PageUp/PageDown/End, and a marker on any pane that is no longer
at the bottom. Without the marker a pane someone scrolled by accident
is indistinguishable from an agent that has hung, and the wheel makes
that accident easy."
```

---

### Task 8: The tab strip scrolls

Entirely independent of Tasks 1–7. Safe to build first if the pane work stalls.

**Files:**
- Modify: `src/dashboard.rs:1128-1200` (`render_tabs` and its doc comment), the `tab_areas` mouse arm at `:4578`, the wheel arm at `:4777`
- Test: `src/dashboard.rs` inline `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_active_tab_stays_fully_visible_however_many_workspaces_there_are() {
    let mut dashboard = benchmark_dashboard(12, 1);
    for index in 0..12 {
        dashboard.jump_to_workspace(index + 1);
        let rendered = render_to_string(&mut dashboard, 60, 24);
        let name = format!("{} ws{}", index + 1, index + 1);
        assert!(
            rendered.contains(&name),
            "workspace {} must be visible when it is active: {rendered}",
            index + 1
        );
        assert!(
            rendered.contains('✎') && rendered.contains('×'),
            "the active tab's own affordances must not be what falls off the edge: {rendered}"
        );
    }
}

#[test]
fn the_strip_marks_the_tabs_it_is_hiding_on_each_side() {
    let mut dashboard = benchmark_dashboard(12, 1);
    dashboard.jump_to_workspace(6);
    let rendered = render_to_string(&mut dashboard, 60, 24);
    assert!(rendered.contains('‹') && rendered.contains('›'));
}

#[test]
fn a_strip_that_fits_shows_no_markers() {
    let mut dashboard = benchmark_dashboard(2, 1);
    let rendered = render_to_string(&mut dashboard, 120, 24);
    assert!(!rendered.contains('‹') && !rendered.contains('›'));
}

#[test]
fn the_wheel_over_the_strip_scrolls_it_without_switching_workspace() {
    let mut dashboard = benchmark_dashboard(12, 1);
    let active = dashboard.workspace_index;
    let before = render_to_string(&mut dashboard, 60, 24);
    dashboard.mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 10,
        row: 2, // the tab strip row
        modifiers: KeyModifiers::NONE,
    });
    let after = render_to_string(&mut dashboard, 60, 24);
    assert_ne!(before, after, "the strip must move");
    assert_eq!(
        dashboard.workspace_index, active,
        "scrolling the strip must not switch workspace"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_active_tab_stays_fully_visible the_strip_marks the_wheel_over_the_strip a_strip_that_fits`
Expected: FAIL — later tabs never render, no markers exist.

- [ ] **Step 3: Implement**

Add `tab_scroll: usize` to `Dashboard`, and a `tab_strip_area: Option<Rect>` recorded during render so the wheel arm can test for it.

Rewrite `render_tabs` as two passes. First, measure every tab's width (`" {index} {name} "`, plus 3 for `✎` and 3 or 6 for close/confirm when it is the active tab). Then clamp `tab_scroll` so the active tab's full width fits between the reserved marker columns — walk `tab_scroll` up while the active tab overflows the right edge, and down while it is left of the first visible tab. Finally lay out from `tab_scroll`, exactly as today.

Replace the doc comment at `:1130`, which currently states the opposite behaviour:

```rust
    /// The workspace strip: one tab per workspace, numbered by the digit that jumps to it.
    ///
    /// The strip scrolls rather than truncating, and follows the active tab: a workspace you
    /// have jumped to is always visible, together with its own rename and close affordances,
    /// which are the last thing that should fall off an edge. `‹` and `›` mark tabs hidden on
    /// each side and are clickable. The numbers stay meaningful because they are positions,
    /// not labels.
```

Render `‹` at `area.x` when `tab_scroll > 0` and `›` at `area.right() - 1` when tabs remain, recording each as a hit target beside `tab_areas`. Lay tabs out between those two columns whether or not the markers are drawn, so the strip does not shift by a column as they appear.

In the mouse `Down(Left)` arm, handle the marker areas before `tab_areas` — they sit at the same row, and testing tabs first would swallow the click. In the wheel arm at `:4777`, test `tab_strip_area` before the pane areas and adjust `tab_scroll` by one per notch, returning `UiCommand::None`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, clean.

- [ ] **Step 5: Measure, then check it against a real canvas**

`render_tabs` runs every frame, and this adds a clamping pass over every workspace to it. Run before and after:

```bash
cargo test --release --lib -- --ignored --nocapture
```

Compare `render_measurement_of_a_busy_dashboard_at_three_terminal_sizes` — it builds a `benchmark_dashboard(4, 12)`, so the tab strip is in the frame it measures. Record the fastest-frame, allocation, and byte figures in the commit body.

Then run `dock`, open a dozen workspaces, and confirm: jumping by digit brings the tab into view; the wheel over the strip scrolls it; clicking `‹`/`›` scrolls; the active tab's `✎`/`×` are never clipped; a two-workspace canvas shows no markers.

- [ ] **Step 6: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: let the tab strip scroll, so every workspace is reachable by mouse

The strip stopped at the row edge and said so in a comment: a workspace
pushed off the end was reachable by digit and by Ctrl+B w, and by no
pointer at all. It now scrolls and follows the active tab, keeping that
tab's own rename and close affordances on screen rather than letting
them be the first thing clipped."
```

---

## Verification

After Task 8, before calling the project done:

- [ ] `cargo test` — record the total; it should be roughly 583 + ~30 new tests.
- [ ] `cargo test` a second and third time — this repository has had flaky signal tests; three clean runs is the bar B2 used.
- [ ] `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- [ ] `cargo test --release --lib -- --ignored --nocapture` — all five measurements (three existing, two added), compared against the figures recorded on the commit before Task 1. The render path must be unchanged; the daemon hot path must be unchanged; the two new ones are the project's own cost and are reported, not asserted.
- [ ] Against a live daemon: open a pane, run something that prints thousands of lines, quit the client, reopen it, and scroll up. History from before the client existed must be reachable — that is the whole point of the project and no unit test covers it end to end.
- [ ] Restart a pane while scrolled back in it and confirm the replica re-seeds rather than splicing the old stream's bytes into the new one.
- [ ] Record final memory: attach to eight busy panes and compare RSS against the same canvas before this project.
