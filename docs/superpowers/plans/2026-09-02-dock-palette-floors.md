# Palette Floors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every palette test run against every shipped theme, fix the one shipped theme that fails, and put a floor under the structural line and the glyph vocabulary so none of it can silently regress.

**Architecture:** `theme.rs` already holds a private test-module toolkit (`luminance`, `contrast`, `distance`) and four palette assertions, every one of which is hardcoded to `Theme::cool()`. This plan introduces a single `Theme::all()` enumerator and rewrites each assertion as a loop over it. That change alone turns `warm`'s existing defect into a red test, which is then fixed. Two new floors follow: `border` ≥ 2:1 as a structural line, and a `glyph` vocabulary module whose constants replace the three spellings of "close" now scattered across render code.

**Tech Stack:** Rust 2024 edition, `ratatui` 0.30 (`ratatui::style::Color`), the crate's own `#[cfg(test)]` module in `src/theme.rs`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-02-dock-receipts-design.md` — section 6, "The visual system", and row 0 of section 9.

## Global Constraints

- **No colour may be constructed outside `src/theme.rs`.** This is stated at the top of that file and is the rule this plan reinforces. Every colour is an explicit `Color::Rgb(r, g, b)`; `luminance` panics on any other variant by design.
- **Contrast floor for text-like tokens: 3.0:1** against both `surface` and `panel`.
- **Contrast floor for structural lines (`border`, `border_focused`): 2.0:1** against both grounds. Structural lines cannot clear 3:1 by design; 2:1 is the floor that distinguishes "dim" from "absent".
- **Separation floor for agent-state colours: 60.0** RGB Euclidean units from each other and from `accent`.
- **Selection band floors: 3.0:1** for the band against `surface`, **4.5:1** for `text` on the band.
- Tests live in the existing `#[cfg(test)] mod tests` in `src/theme.rs` unless a task says otherwise.
- Run the full suite with **`cargo test`**, not `cargo test --lib`. 44 tests live in `src/main.rs`, which is the `dock` binary target and is invisible to `--lib`. Every task must leave it green.
- **Test-count arithmetic**, so a wrong expectation never reads as a failure. Before this plan: **843 registered** = 831 passed + 12 ignored, across the lib (787 passed + 12 ignored) and bin (44 passed) targets. This plan adds **six** tests — one in Task 1, one in Task 4, one in Task 5, three in Task 6 — ending at **849 registered = 837 passed + 12 ignored**. Tasks 2, 3 and 7 rewrite or verify; they add none.
- Gates are pass/fail and must be **read**, not grepped for a line count: `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`. A clippy failure once reached `main` because a count of matching lines looked like zero.
- Measurement harnesses are `#[ignore]`d tests, run deliberately. Grep for them with `#\[ignore` — **not** `#\[ignore\]`, which misses the `#[ignore = "reason"]` form most of them use.
- This plan touches no render code paths and must not change any frame timing. No measurement run is required.

---

### Task 1: Enumerate the shipped palettes

Every palette assertion names `Theme::cool()` by hand, so a second palette can drift without failing anything. One enumerator fixes that permanently and is the seam every later task loops over.

**Files:**
- Modify: `src/theme.rs` — add `Theme::all()` beside `warm()` and `cool()`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub const fn Theme::all() -> [(&'static str, Theme); 2]` — name paired with palette, so a failing assertion can say *which* theme broke. Tasks 2, 3, 4 and 6 all iterate this.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/theme.rs`:

```rust
    /// Every palette Dock ships has to be reachable from one place, or a test that means
    /// "this rule holds" quietly degrades into "this rule holds for the palette somebody
    /// remembered to name".
    #[test]
    fn every_shipped_palette_is_enumerated() {
        let names: Vec<&str> = Theme::all().iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["warm", "cool"]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme::tests::every_shipped_palette_is_enumerated`
Expected: FAIL to compile — `no function or associated item named 'all' found for struct 'Theme'`.

- [ ] **Step 3: Write minimal implementation**

Add to the `impl Theme` block in `src/theme.rs`, immediately after `cool()`:

```rust
    /// Every palette Dock ships, paired with its name.
    ///
    /// The palette assertions below loop over this rather than naming a theme, because the
    /// alternative already failed once: `the_agent_states_stay_far_apart` was written
    /// against `cool` alone, and `warm` shipped for weeks with `working` 18.9 units from
    /// `accent` against a floor of 60.
    pub const fn all() -> [(&'static str, Self); 2] {
        [("warm", Self::warm()), ("cool", Self::cool())]
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib theme::tests::every_shipped_palette_is_enumerated`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/theme.rs
git commit -m "test: name every shipped palette in one place"
```

---

### Task 2: Every palette keeps its agent states apart — and warm does not

This is the defect. `the_agent_states_stay_far_apart` enforces a 60-unit separation and runs against `cool()` only. `warm.working` is `Rgb(226, 184, 96)` and `warm.accent` is `Rgb(232, 168, 88)`: **18.9 apart**. The doc comment on `cool()` already names this collision as the reason `cool` was written; `warm` was never fixed.

**Files:**
- Modify: `src/theme.rs` — the `warm()` constructor and `the_agent_states_stay_far_apart`

**Interfaces:**
- Consumes: `Theme::all()` from Task 1.
- Produces: nothing new; `warm.working` changes value to `Rgb(168, 120, 56)`.

- [ ] **Step 1: Write the failing test**

Replace the body of `the_agent_states_stay_far_apart` in `src/theme.rs` with a loop over every palette. The assertion messages gain the theme name, because "working and accent are only 18.9 apart" without a theme name sends the reader to the wrong constructor:

```rust
    /// The four agent states must stay far enough apart to be told apart at a glance, in
    /// every palette.
    ///
    /// Not theoretical: `working` and `idle` collided twice while `cool` was being chosen,
    /// and `warm` shipped with `working` 18.9 from its accent because this test named one
    /// palette by hand.
    #[test]
    fn the_agent_states_stay_far_apart() {
        for (theme_name, theme) in Theme::all() {
            let states = [
                ("blocked", theme.blocked),
                ("working", theme.working),
                ("done", theme.done),
                ("idle", theme.idle),
            ];
            for (index, (name, colour)) in states.iter().enumerate() {
                for (other, second) in &states[index + 1..] {
                    let apart = distance(*colour, *second);
                    assert!(
                        apart >= 60.0,
                        "{theme_name}: {name} and {other} are only {apart:.1} apart"
                    );
                }
                let from_accent = distance(*colour, theme.accent);
                assert!(
                    from_accent >= 60.0,
                    "{theme_name}: {name} is only {from_accent:.1} from the accent"
                );
            }
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme::tests::the_agent_states_stay_far_apart`
Expected: FAIL with `warm: working is only 18.9 from the accent`.

- [ ] **Step 3: Write minimal implementation**

In `Theme::warm()` in `src/theme.rs`, change the `working` line and document why:

```rust
            // Separated from the accent by value within the same amber hue, which is how
            // `cool` solves the identical problem (its `working` sits 70.8 from its accent).
            // The previous Rgb(226,184,96) was 18.9 from this palette's accent — an agent
            // that was working looked exactly like ordinary chrome.
            //
            // accent 86.2 · blocked 70.7 · done 173.8 · idle 83.5 · 4.82:1 on surface, 4.47:1 on panel.
            working: Color::Rgb(168, 120, 56),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib theme::tests::the_agent_states_stay_far_apart`
Expected: PASS.

Then confirm the new value did not break the contrast rule, which still names `cool` at this point and so would not have caught it:

Run: `cargo test --lib theme::tests`
Expected: PASS, all theme tests.

- [ ] **Step 5: Commit**

```bash
git add src/theme.rs
git commit -m "fix: warm's working state was 18.9 from its own accent"
```

---

### Task 3: Every palette keeps its text legible, and its selection band on both floors

Two more assertions hardcode `cool()`. `warm` happens to pass both today — measured, not assumed — so this task adds no fix, only the guard that keeps the third and fourth palettes from shipping broken.

**Files:**
- Modify: `src/theme.rs` — `every_token_is_legible_on_both_surfaces`, `the_selection_band_clears_both_of_its_floors`

**Interfaces:**
- Consumes: `Theme::all()` from Task 1.
- Produces: nothing.

- [ ] **Step 1: Write the failing test**

Replace both assertions in `src/theme.rs` with palette-looping versions:

```rust
    /// Every token has to clear 3:1 against both surfaces it can be painted on, in every
    /// palette. `panel` sits above `surface`, so a colour chosen only against the ground
    /// can go marginal on chrome.
    #[test]
    fn every_token_is_legible_on_both_surfaces() {
        for (theme_name, theme) in Theme::all() {
            for (name, colour) in [
                ("text", theme.text),
                ("muted", theme.muted),
                ("accent", theme.accent),
                ("blocked", theme.blocked),
                ("working", theme.working),
                ("done", theme.done),
                ("idle", theme.idle),
            ] {
                for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
                    let ratio = contrast(colour, surface);
                    assert!(
                        ratio >= 3.0,
                        "{theme_name}: {name} on {ground} is only {ratio:.2}:1"
                    );
                }
            }
        }
    }

    /// The selection band's two floors, which pull in opposite directions: brighter makes
    /// the band visible as a band and dimmer keeps the text on it readable.
    #[test]
    fn the_selection_band_clears_both_of_its_floors() {
        for (theme_name, theme) in Theme::all() {
            let band = contrast(theme.selection, theme.surface);
            assert!(band >= 3.0, "{theme_name}: band on surface is {band:.2}:1");
            let on_band = contrast(theme.text, theme.selection);
            assert!(on_band >= 4.5, "{theme_name}: text on band is {on_band:.2}:1");
        }
    }
```

Note that `border_focused` has been removed from the legibility list. It is a structural line, not text, and Task 4 gives it the floor it actually needs. Leaving it under a 3:1 text floor was the reason plain `border` had to be excluded from this test with an apologetic comment.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme::tests::every_token_is_legible_on_both_surfaces theme::tests::the_selection_band_clears_both_of_its_floors`
Expected: PASS immediately. `warm` clears every floor: lowest is `idle` on `panel` at 3.86:1, and its selection band measures 3.01:1 / 4.64:1.

This is a guard, not a fix, so a green run is the correct outcome. To prove the guard actually bites, temporarily change `warm`'s `muted` to `Color::Rgb(60, 58, 56)`, re-run, and confirm it reports:

```
warm: muted on surface is only 1.65:1
```

Note which ground is named. The inner loop asserts against `surface` before `panel`, and `assert!` stops at the first failure, so `surface` (1.65:1) fires even though `panel` is the worse of the two at 1.53:1. Revert that edit before continuing.

- [ ] **Step 3: Write minimal implementation**

None required — Step 1 is the whole change. Do not skip Step 2's temporary-break check; a guard nobody has seen fail is not known to work.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib theme::tests`
Expected: PASS, and confirm `warm`'s `muted` is back to `Color::Rgb(122, 118, 112)`.

- [ ] **Step 5: Commit**

```bash
git add src/theme.rs
git commit -m "test: legibility and selection floors hold for every palette"
```

---

### Task 4: A floor under the structural line

`border` draws the pane grid, the tab separator, the menu separator, and the board's column rules. The 2026-08-30 review measured it at 1.32:1 in `cool` — "not dim, it is gone" — and commit `a99d44a` fixed it to `Rgb(70, 82, 90)`, now 2.26:1, **and added `unfocused_borders_clear_two_to_one_on_both_surfaces` to hold it there**. So the floor is already enforced. What that guard does not do is cover `border_focused`, name the failing theme, or iterate `Theme::all()`. This task widens it on those three axes and deletes the original, which the widened version strictly supersedes.

The spec considered adding a separate `border_pane` token and **rejected it**: both palettes already clear 2:1, so a new token would buy a property the palette has. Only the test was missing.

**Files:**
- Modify: `src/theme.rs` — new test in the existing test module

**Interfaces:**
- Consumes: `Theme::all()` from Task 1.
- Produces: nothing.

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/theme.rs`:

```rust
    /// The structural lines: the pane grid, the tab separator, the menu rule, the board's
    /// column rules. 3:1 is the wrong floor for these — they are not text and cannot clear
    /// it by design — but 1.32:1 is what `cool.border` measured before `a99d44a`, and at
    /// that ratio a grid of twelve panes photographs as one undifferentiated field of text.
    ///
    /// 2:1 is the line between dim and absent, and this is what holds it.
    #[test]
    fn every_structural_line_clears_two_to_one() {
        for (theme_name, theme) in Theme::all() {
            for (name, colour) in [("border", theme.border), ("border_focused", theme.border_focused)] {
                for (ground, surface) in [("surface", theme.surface), ("panel", theme.panel)] {
                    let ratio = contrast(colour, surface);
                    assert!(
                        ratio >= 2.0,
                        "{theme_name}: {name} on {ground} is only {ratio:.2}:1"
                    );
                }
            }
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme::tests::every_structural_line_clears_two_to_one`
Expected: PASS. Measured values are `warm.border` 2.254:1 / 2.092:1 and `cool.border` 2.263:1 / 2.041:1; both `border_focused` values are far above the floor. `cool.border` on `panel` clears by only 0.041, so this guard is genuinely load-bearing.

To prove the guard bites, temporarily restore the pre-`a99d44a` value by setting `cool`'s `border` to `Color::Rgb(38, 46, 51)`, re-run, and confirm it reports `cool: border on surface is only 1.32:1`. Revert before continuing.

- [ ] **Step 3: Write minimal implementation**

None required. Confirm `cool`'s `border` is back to `Color::Rgb(70, 82, 90)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib`
Expected: PASS, all 843 tests plus the new ones.

- [ ] **Step 5: Commit**

```bash
git add src/theme.rs
git commit -m "test: a structural line is dim, not absent, in every palette"
```

---

### Task 5: One spelling per mark

Three glyphs mean "close" (`×` in the pane control, `✘` on the tab, `✗` on an exited pane title), two mean "cursor" (`›` in four places, `"> "` in the review queue), and the same three diff numbers are typeset with U+2212 in the git overlay and ASCII hyphen in the review queue. Individually trivial; collectively the difference between "designed" and "accumulated".

`theme.rs` already enforces the palette. This puts the typography under the same roof.

**Files:**
- Modify: `src/theme.rs` — new `pub mod glyph` at the end of the file, outside the test module

**Interfaces:**
- Consumes: nothing.
- Produces: `theme::glyph::{CLOSE, CURSOR, ELLIPSIS, MINUS, SEPARATOR}` — all `&'static str`. Row 3 of the spec (the Split Spine) and every later render change uses these instead of literals.

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/theme.rs`:

```rust
    /// The typographic vocabulary, asserted the way the palette is.
    ///
    /// Not decoration: `×`, `✘` and `✗` all currently mean "close" in different render
    /// functions, and the same three diff numbers are set with U+2212 in one overlay and an
    /// ASCII hyphen in another. A reader cannot learn a mark that is spelled three ways.
    #[test]
    fn the_glyph_vocabulary_has_one_spelling_per_mark() {
        use super::glyph;
        assert_eq!(glyph::CLOSE, "×");
        assert_eq!(glyph::CURSOR, "›");
        assert_eq!(glyph::ELLIPSIS, "…");
        assert_eq!(glyph::SEPARATOR, " · ");
        // U+2212 MINUS SIGN, not U+002D HYPHEN-MINUS: it is the same width as `+` in every
        // monospace face worth using, so `+120 −33` lines up in a column and `+120 -33`
        // does not.
        assert_eq!(glyph::MINUS, "\u{2212}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib theme::tests::the_glyph_vocabulary_has_one_spelling_per_mark`
Expected: FAIL to compile — `failed to resolve: could not find 'glyph' in the crate root`.

- [ ] **Step 3: Write minimal implementation**

Add at the end of `src/theme.rs`, before the `#[cfg(test)]` module:

```rust
/// Dock's typographic vocabulary.
///
/// The palette rule at the top of this file — no colour outside this module — exists so a
/// reader can learn one visual language. Marks are the same kind of claim: one glyph per
/// meaning, declared once, so a render function reaches for a constant rather than typing
/// whichever close-box character came to mind.
pub mod glyph {
    /// Close or dismiss. Not `✘`, not `✗`: both read as "failed", and Dock now spends `✗`
    /// on a verdict where failure is exactly what it means.
    pub const CLOSE: &str = "×";
    /// The selected row, in every list Dock draws.
    pub const CURSOR: &str = "›";
    /// One character, not three dots.
    pub const ELLIPSIS: &str = "…";
    /// U+2212 MINUS SIGN. Same advance width as `+` in a monospace face, so a diff stat
    /// column stays a column.
    pub const MINUS: &str = "\u{2212}";
    /// The mark in `d·ock`, and the separator between every pair of facts in a title.
    pub const SEPARATOR: &str = " · ";
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib theme::tests::the_glyph_vocabulary_has_one_spelling_per_mark`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/theme.rs
git commit -m "feat: one spelling per mark, declared beside the palette"
```

---

### Task 6: The verdict's three shapes are three shapes

The spec's verdict has three states rendered as `✓` / `!` / `✗`. `AgentState::glyph` already has a test asserting its four states are four distinct characters (`detect/mod.rs:156`); the verdict needs the same guarantee before any render code depends on it, and the two vocabularies must not collide — a `✗` verdict beside a `◆` state must never be confusable.

**Files:**
- Create: `src/verdict.rs`
- Modify: `src/lib.rs` — register the module

**Interfaces:**
- Consumes: `AgentState::glyph` from `src/detect/mod.rs` (for the collision assertion only).
- Produces: `pub enum Verdict { Clear, Look, Failed }` with `pub const fn glyph(self) -> char` and `pub const fn label(self) -> &'static str`. Row 2 of the spec computes it; row 3 renders it.

- [ ] **Step 1: Write the failing test**

Create `src/verdict.rs`:

```rust
//! What Dock concluded about a receipt, and how it is drawn.
//!
//! The verdict is arithmetic over evidence, never judgement of it: the rules that produce
//! it land with the receipt store. This module is only the vocabulary, declared early so
//! the shapes are settled before anything renders them.

/// Dock's conclusion about one receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Every declared check was witnessed green at head, and no finding fired.
    Clear,
    /// One or more findings, each named, none fatal.
    Look,
    /// A declared check ran and exited non-zero.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState;
    use std::collections::HashSet;

    /// Three verdicts, three shapes. Colour is not enough: roughly 8% of men have a
    /// red-green deficiency, and a terminal tool travels as a compressed screenshot where
    /// hue is the first thing lost.
    #[test]
    fn the_three_verdicts_are_three_shapes() {
        let shapes: HashSet<char> = [Verdict::Clear, Verdict::Look, Verdict::Failed]
            .into_iter()
            .map(Verdict::glyph)
            .collect();
        assert_eq!(shapes.len(), 3);
    }

    /// A verdict and an agent state are drawn in the same spine, one under the other. If
    /// any glyph appeared in both vocabularies, a row would be ambiguous about which
    /// question it was answering.
    #[test]
    fn no_verdict_shape_collides_with_an_agent_state_shape() {
        let states: HashSet<char> = [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Done,
            AgentState::Blocked,
        ]
        .into_iter()
        .map(AgentState::glyph)
        .collect();
        for verdict in [Verdict::Clear, Verdict::Look, Verdict::Failed] {
            assert!(
                !states.contains(&verdict.glyph()),
                "{:?} draws as {}, which is already an agent state",
                verdict,
                verdict.glyph()
            );
        }
    }

    /// Every verdict says what it means in words, because the spine is read by people who
    /// have not yet learned the shapes.
    #[test]
    fn every_verdict_has_a_label() {
        assert_eq!(Verdict::Clear.label(), "clear");
        assert_eq!(Verdict::Look.label(), "look");
        assert_eq!(Verdict::Failed.label(), "failed");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

First register the module. `src/lib.rs` lists modules alphabetically and currently ends with `pub mod theme;`, so `verdict` goes last:

```rust
pub mod theme;
pub mod verdict;
```

Run: `cargo test --lib verdict::`
Expected: FAIL to compile — `no function or associated item named 'glyph' found for enum 'Verdict'`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/verdict.rs`, after the enum:

```rust
impl Verdict {
    /// The shape, chosen so the three survive greyscale and a compressed screenshot.
    ///
    /// None of these may collide with `AgentState::glyph`, which draws `○ ◐ ◉ ◆` one row
    /// away in the same spine. The circles are a fill gradient of progress; the verdict
    /// marks are deliberately not circles at all.
    pub const fn glyph(self) -> char {
        match self {
            Self::Clear => '✓',
            Self::Look => '!',
            Self::Failed => '✗',
        }
    }

    /// The word, for readers who have not yet learned the shape.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Look => "look",
            Self::Failed => "failed",
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib verdict::`
Expected: PASS, three tests.

Run: `cargo test --lib`
Expected: PASS, whole suite.

- [ ] **Step 5: Commit**

```bash
git add src/verdict.rs src/lib.rs
git commit -m "feat: three verdict shapes that no agent state can be mistaken for"
```

---

### Task 7: Prove the suite is green and record the baseline

The plan's claim is that nothing here touched a render path. That is worth checking rather than asserting, and the numbers become the baseline row 3 is measured against.

**Files:**
- Modify: none. This task produces a commit message and nothing else.

**Interfaces:**
- Consumes: everything above.
- Produces: a recorded render baseline for the Split Spine work to compare against.

- [ ] **Step 1: Run the whole suite**

Run: `cargo test`
Expected: PASS. **837 passed + 12 ignored = 849 registered**, across the lib and bin targets. That is the pre-plan 843 plus the six this plan adds: one in Task 1, one in Task 4, one in Task 5, and three in Task 6. Tasks 2 and 3 rewrite existing tests rather than adding any.

Do not use `cargo test --lib` here: it silently omits the 44 tests in `src/main.rs`.

- [ ] **Step 2: Run the render measurement three times**

Run, three times, taking the fastest of the three (this machine's mean swings ~40% under load, which has hidden a real 25% change before):

```bash
cargo test --release --lib -- --ignored --nocapture render_measurement
```

Expected: a frame time well inside the 16.7 ms budget, unchanged from before this plan. `warm`'s `working` colour changed value; nothing changed shape, so a difference here would mean something unexpected happened.

- [ ] **Step 3: Confirm no colour escaped the module**

The rule at the top of `theme.rs` is "no colour may be hardcoded outside this module", and it is broken in two forms, only one of which is currently clean.

Run: `grep -rn "Color::Rgb" src --include='*.rs' | grep -v "^src/theme.rs"`
Expected: no output. Verified zero as of `5a9c09c`. If a line appears, a colour has been constructed outside `theme.rs`.

Run: `grep -rn "Color::Indexed" src --include='*.rs'`
Expected: exactly five lines, all in `src/dashboard.rs` — one at `:8420` mapping the board's age rungs, and four in its tests asserting the default rung values 242/34/226/196.

That is a **known, unfixed violation**, not a regression from this plan: the board paints staleness from raw ANSI-256 indices read out of `board_config.rs`, so Dock's flagship surface uses colours that exist nowhere else in the product. Spec section 6 resolves it by moving the ramp into the theme as the `age: [Color; 5]` token, and that lands in row 3 with the rest of the board's render work. Record the five lines and move on; do not fix it here, and do not let a *sixth* appear.

- [ ] **Step 4: Record the baseline**

```bash
git commit --allow-empty -F - <<'MSG'
test: palette floors hold for every shipped theme

843 + 6 tests green. Render measurement unchanged: this plan touched
no render path, only the values and the assertions over them.

Baseline for the Split Spine work, fastest of three runs:
  <paste the render_measurement figures here>
MSG
```

---

## Self-Review

**Spec coverage.** Row 0 of section 9 reads: *"Palette tests parameterised over every shipped theme; `warm.working` fixed; `border` floor enforced; vocabulary consts."* Task 1 builds the enumerator; Tasks 2 and 3 parameterise all four existing assertions; Task 2 fixes `warm.working`; Task 4 enforces the `border` floor; Task 5 adds the vocabulary constants. Task 6 adds the verdict vocabulary, which section 2 of the spec requires and which must exist before row 2 computes it or row 3 draws it. Task 7 records the baseline section 6 requires before the spine lands. No row-0 requirement is unclaimed.

**Placeholder scan.** Every step carries the actual code. The one deliberate blank is the figures pasted into Task 7's commit message, which cannot be known before the run.

**Type consistency.** `Theme::all()` returns `[(&'static str, Theme); 2]` in Task 1 and is destructured as `(theme_name, theme)` in Tasks 2, 3 and 4. `Verdict::glyph` returns `char`, matching `AgentState::glyph`, so the collision test in Task 6 compares like with like. `glyph::*` constants are `&'static str` throughout.

**One thing later plans must honour.** Task 5 declares the vocabulary but changes no render code — the existing `✘`, `✗` and `"> "` literals are still in `dashboard.rs`. Replacing them is deliberately deferred to row 3, which rewrites those render functions anyway; doing it here would mean editing them twice. A later plan that touches a render function must use `theme::glyph::*` rather than a literal.
