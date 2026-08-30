# Dock — a design review

*What it looks like, what it already knows, and the one thing I'd bet the product on.*

Read against `main` at `e0a57db`. Every claim below is cited to a line. I did not build or
test — another agent is in `src/cli/`, `src/main.rs` and `README.md` right now.

---

## The one-page argument

Dock's code is better than Dock looks, and Dock knows far more than Dock says.

The care in this repository is unusual. `theme.rs` computes WCAG contrast in its own test
suite. `card_lines` reserves a fixed cell for a priority mark **so that titles in a column
stay in a straight line whether or not any card is urgent** (`dashboard.rs:7972-7986`).
`fit_scroll_marker` shares a title-row budget between two independently-drawn ratatui titles
because otherwise the second would paint over the first (`dashboard.rs:3170-3178`). This is
typesetting, not layout.

And yet: **every unfocused pane border in Dock is drawn at 1.32:1 contrast** — I compute it
below — which is to say the grid separating your twelve panes is, in a screenshot, not there.
**Two of the four agent states share a glyph** (`detect/mod.rs:127-133`), so "needs you" and
"working" are distinguishable by hue alone. The board — the single most screenshot-worthy
surface in the product — paints its cards from raw ANSI-256 indices that live outside the
theme entirely (`dashboard.rs:7837`, `board_config.rs:24`).

On the feature side the imbalance is starker. Dock recognises 20 agents and can *launch* four
(`detect/mod.rs:12-33` vs `adapter.rs:10-20`). Dock is handed `session_id`, `transcript_path`,
`tool_name` and `cwd` by every Claude Code hook and throws all of it away unread
(`main.rs:1376-1432` reads argv only). Dock has a fully-built, twelve-guard, unit-testable
engine for typing prompts into an agent at its turn boundary while nobody is watching
(`queue.rs`) — and the README does not mention it once.

So the work is not "add features". It is **finish the sentences the code has already started**,
and then spend a week making the result photograph well. Screenshots are how a terminal tool
spreads, and Dock currently has no screenshot.

---

# Part 1 — Aesthetics

## 1.1 What Dock actually looks like, surface by surface

Honest read. "Expensive" means a stranger would assume it was designed; "cheap" means they
would assume it was assembled.

| Surface | Reads as | Why |
|---|---|---|
| Pane title | **expensive** | glyph · label · location, budgeted against a scroll marker and a `COPY` prefix that both share the same row (`dashboard.rs:3140-3273`) |
| Tab strip | **expensive** | real chips (`panel` bg on inactive, accent bg on active), a drawn `│` separator rather than a blank column, and reserved gutters so `‹`/`›` never shift the strip (`dashboard.rs:2213-2296`) |
| Sidebar, full | **expensive** | fixed label column, right-aligned state word, an indented overflow line for the workspace when it does not fit inline (`dashboard.rs:2508-2570`) |
| Sidebar, rail | **expensive** | it is exactly one idea, drawn once (`dashboard.rs:2626+`) |
| Which-key footer | **expensive** | grows the footer to four rows rather than truncating the binding table (`dashboard.rs:1898-1918`) |
| Copy mode | **expensive** | `COPY` in the border *and* the footer swaps its whole vocabulary |
| Context menu | **expensive** | four-way style table for (cursor × enabled), reasoned in a comment against measured contrast (`dashboard.rs:6524-6540`) |
| **Pane grid** | **cheap** | 1.32:1 borders; twelve panes read as one undifferentiated field of text |
| **Board** | **cheap** | five full-width rules read as a table header; cards have no left edge; the palette is not Dock's |
| **Overlays** | **cheap** | four arbitrary widths, one visual weight, no scrim, no hierarchy |
| **Empty states** | **cheap** | one grey sentence in a box captioned `RUNTIME` |

Two-thirds expensive. But the cheap third is *exactly* the third that ends up in a screenshot:
the canvas, the board, and whatever modal is open.

## 1.2 The five defects, in the order I would fix them

### (1) The pane grid is invisible. This is the whole reason Dock photographs flat.

`Theme::cool()` sets `border: Rgb(38, 46, 51)` over `surface: Rgb(18, 22, 26)`
(`theme.rs:71, 68`). Running theme.rs's own `contrast()` on that pair by hand:

```
L(surface) = 0.2126·0.00605 + 0.7152·0.00800 + 0.0722·0.01033 = 0.00775
L(border)  = 0.2126·0.01942 + 0.7152·0.02732 + 0.0722·0.03313 = 0.02606
contrast   = (0.02606 + 0.05) / (0.00775 + 0.05) = 1.32 : 1
```

The codebase already knows this number for the other surface. `dashboard.rs:6526-6529`, on the
context menu:

> `border` is 1.19:1 on `panel`, which is not dim, it is gone

That reasoning was applied to one disabled menu row and never applied to **every unfocused
pane edge in the product**, which is 1.32:1. `every_token_is_legible_on_both_surfaces`
(`theme.rs:213-238`) explicitly excludes `border` from the 3:1 floor on the grounds that it is
"a structural line rather than text and cannot clear 3:1 by design". True — and 3:1 is the
wrong floor for it. **2:1 is the right floor, and nothing enforces any floor at all.**

The consequence compounds: `border` is also the colour of the board's column rules
(`dashboard.rs:7700-7706`), the tab separator (`dashboard.rs:2265`), the menu separator
(`dashboard.rs:6512`), and the em-dash placeholder in an empty column
(`dashboard.rs:7929`). Dock's entire structural line vocabulary is drawn in a colour it has
documented as absent.

**Fix.** Split the token. `border` stays where a line is a hint; add `border_pane` for load-
bearing structure, and give it a test.

```rust
// theme.rs
/// The pane grid. Structure, not a hint: this is the only line telling a person where one
/// terminal ends and the next begins, and `border` at 1.32:1 was not telling them.
pub border_pane: Color,   // cool(): Rgb(72, 84, 93) → 2.34:1 on surface

#[test]
fn every_structural_line_clears_two_to_one() {
    for theme in [Theme::warm(), Theme::cool()] {
        assert!(contrast(theme.border_pane, theme.surface) >= 2.0);
    }
}
```

Cost: one token, one field in the `Block` at `dashboard.rs:3239-3243`, one test. Half an hour.
It is the highest-leverage half hour available in this repository.

### (2) Two of four states share a glyph. Dock is colour-only where it matters most.

```rust
// detect/mod.rs:127-133
pub const fn glyph(self) -> char {
    match self {
        Self::Blocked | Self::Working => '●',
        Self::Done => '◍',
        Self::Idle => '○',
    }
}
```

`the_agent_states_stay_far_apart` (`theme.rs:246-271`) enforces a 60-unit RGB distance between
every pair of state colours, and comments that `working` and `idle` "collided twice while this
palette was being chosen". Enormous rigour on hue. **Zero rigour on shape.** The two states
the product exists to distinguish — *it is stuck* versus *it is fine* — are the same character.

This costs three ways. It is invisible to the ~8% of men with a red-green deficiency. It is
invisible in a greyscale or heavily-compressed screenshot, which is how a terminal tool travels.
And it means the sidebar row must always spell the word out to be readable, which is why
`state.label()` is carried beside every glyph (`dashboard.rs:2504-2507`) — an honest workaround
for a defect one line up the stack.

**Fix.** Make the circles a fill gradient of *progress*, and give the interrupt a different
shape entirely, so it can never be confused with any amount of progress:

```
  ○   idle        an empty circle: nothing is happening
  ◐   working     partially filled: this is underway
  ◉   done        filled with a ring: its turn is over, yours has started
  ◆   needs you   not a circle at all: this one is stuck
```

Read the four in a row with the colour stripped out — `○ ◐ ◉ ◆` — and the ranking is still
legible. That is the test `theme.rs` never wrote. Add it:

```rust
#[test]
fn the_four_states_are_four_shapes() {
    let glyphs: Vec<char> = [Blocked, Working, Done, Idle].map(AgentState::glyph).into();
    assert_eq!(glyphs.iter().collect::<HashSet<_>>().len(), 4);
}
```

Note the one test that must be updated: `dashboard.rs:11841` counts cells whose
`symbol() == "●"` with `fg == blocked`.

### (3) The board's palette is not Dock's palette

`theme.rs:5-6` opens with: *"No colour may be hardcoded outside this module."* It is violated
in the single most important place.

```rust
// dashboard.rs:7837
.map(|rung| Color::Indexed(rung.colour))
```

`AgeThreshold.colour` is a raw ANSI-256 index read from a YAML file
(`board_config.rs:20-25`), defaulting to `242, 34, 226, 208, 196` — grey, **bright green**,
**bright yellow**, orange, red (`board_config.rs:58-77`). Against `cool()`'s graphite-and-teal,
34 and 226 are colours that appear nowhere else in the product. So the board — the surface a
prospective user is most likely to see in a README — is the one surface whose colours are
someone else's.

Worse, the semantics collide. A card untouched for a day is **red**. An agent that needs you
is **red** (`theme.rs:79`). Two different reds, two unrelated meanings, in the same frame. The
whole point of the `cool()` palette, stated at `theme.rs:57-64`, was that "rose is the only
warm token there is, which makes `needs you` structurally incapable of being mistaken for
chrome". The board hands that back.

**Fix, and it is a design decision not a colour swap:** *age should recede, not shout.* A stale
card is not urgent; it is forgotten. Express it as a descent toward `muted`, and let the board
config declare rung *positions* while the theme declares the ramp:

```rust
// theme.rs
/// Five steps from `text` to `muted`. A card that has not moved for a week does not become
/// more important; it becomes quieter. Red is spent on `blocked` and may not be respent.
pub age: [Color; 5],
```

Before / after, same board:

```
  BACKLOG · 4                        BACKLOG · 4
  ────────────                       ────────────
  › #3  write the docs        ←red   › #3  write the docs        ← faintest grey
    #5  fix the retry pat…  ←yellow    #5  fix the retry pat…    ← mid grey
    #8  bump the deps       ←green     #8  bump the deps         ← full text
```

The right-hand column is the one where an agent's rose `◆` is the only saturated thing on
screen, which is the entire palette thesis, honoured.

### (4) The overlays are one visual weight, and there are eight of them

Eight overlays (`OVERLAY_ORDER`), four widths — 48 (`rename`, `dashboard.rs:3572`), 58
(`picker` 2322, `launch` 5812), 72 (`help` 3492, `review` 4472), 96 (`git` 3931) — and *every
one of them* is a centred box, `panel` background, `border_focused` (accent) border, ALLCAPS
title. A one-line rename prompt carries exactly the same visual authority as a full git diff.

Also: `render_help` (3491) and `render_rename` (3571) never call `Clear`, while `picker`
(2332), `review` (4482), `git` (3939) and the menu (6506) do. It happens not to matter because
the `Paragraph` paints `panel` across its area, but it is four surfaces doing something three
others explicitly do not.

And a straight bug worth naming: `render_rename` computes `subject` as either `"Pane"` or
`"Workspace"` (`dashboard.rs:3581-3585`) — that fix landed in commit `26012a1` — while the
border title is still hardcoded `" RENAME FOCUSED PANE "` (`dashboard.rs:3603`). Rename a
workspace and the box says you are renaming a pane. The footer, meanwhile, says `"RENAME · type
a pane name"` (`dashboard.rs:1985`). Three surfaces, one of them right.

**Fix.** Three tiers, and delete a whole overlay class:

- **Prompt** — rename, confirm-close, the "why" note. These are *one line of typing*. They do
  not need a box. Dock already has a place where the eye is trained to look for a line of
  keys: the footer, which already grows to four rows on demand (`dashboard.rs:1898-1911`).
  Dock the prompt there. That removes two overlays and a whole class of centred-box.

  ```
  ├────────────────────────────────────────────────────────────────────────────┤
  │ RENAME WORKSPACE   api-svc█                        Enter saves · Esc cancels│
  ╰────────────────────────────────────────────────────────────────────────────╯
  ```

- **Panel** — picker, launch, review. Centred, `panel`, accent border. Keep exactly as is.
- **Reader** — help and git diff. You read these *against* the panes, not instead of them.
  Dock them full-height to the right, half-width. A centred 96×26 box over the code it is a
  diff of is the least useful place it could be.

- **Add a scrim under Panel and Reader.** You cannot alpha-blend a character grid, but you can
  walk the buffer behind the popup and set every cell's `fg` to `border`, leaving the text
  present but recessed. At 400×100 that is ~40k cell mutations — measured against a 1.438 ms
  frame (`docs/…-cool-palette-design.md:14-19`), a style-only buffer walk is on the order of
  0.05 ms. Trivially affordable, and it is the single change that most makes a TUI look
  expensive.

  This does *not* contradict `dashboard.rs:6497` — *"no shadow, no animation: this is a
  terminal and the frame it is drawn over is a real thing the user is reading"*. That is right
  about a **context menu**, which is drawn at the pointer over content you are actively using.
  It is wrong about a **modal**: nobody reads a pane while the git diff is open. Keep the rule
  for the menu, drop it for Panel and Reader.

### (5) Small incoherences in the glyph and punctuation language

These are individually trivial and collectively the difference between "designed" and
"accumulated".

| Meaning | Glyphs in use | Where |
|---|---|---|
| close / dismiss | `×`, `✘`, `✗` | pane control `dashboard.rs:3220`; tab close `2247`; exited pane title `3149` |
| cursor / selected | `›`, `> ` | sidebar `2470`, cards `7950`, active `8101`, picker `2377` — but `"> "` in the review queue `4501` |
| minus in a diff stat | `−` U+2212, `-` ASCII | git `3952` uses `−`; review `4550` uses `-`; same three numbers, two typographies |

`d·ock` and the ` · ` separator (16 occurrences) are the same mark. **That is Dock's
typographic identity and it should be stated as a rule**, alongside: one cursor glyph (`›`),
one close glyph (`×`), one ellipsis (`…`), one rounded border, U+2212 for every numeric minus.
A `const` block in `theme.rs` beside `border_type()` would make the vocabulary as enforceable
as the palette already is.

## 1.3 Typography and rhythm: where the whitespace is doing work, and where it is absence

**Working well.** The sidebar's fixed `label_width` and right-aligned state word
(`dashboard.rs:2528-2534`) create a real column. `column_widths` allocating proportionally to
content with `ACTIVE` as a `lead` (`dashboard.rs:7262, 7617`) is a genuinely better answer than
equal columns. The pane title's budget arithmetic — reserve the scroll marker *first*, then
ellipsise the title into what is left (`dashboard.rs:3170-3178`) — is the kind of thing you
only write after being bitten.

**Not working.** Three things.

**The board is set as a table, not as cards.** A full-width rule under every heading
(`dashboard.rs:7699-7712`) is the visual grammar of a spreadsheet header. Cards then hang under
those rules with no left edge and no gutter, so a column with two cards and a column with nine
have the same visual weight. The active column is marked by *colour alone* (accent vs border on
that rule) — and per defect (1), the inactive rule is at 1.32:1, so what you actually see is
one rule and four nothings.

Give the rule a **weight**, not just a hue, and give cards a gutter:

```
    ACTIVE · 2           BACKLOG · 4            REVIEW · 1           DONE · 5
    ══════════           ───────────            ──────────           ────────
  › ◆ #14 wire the         #3  write the docs   ◉ #2  add tests      #7  …
        parser             #5  fix the retry
    ◐ #9  retry path           path
                           #8  bump the deps
```

`══` under the column the cursor is in, `──` under the rest. Two cells of gutter before every
card so the id column is a column. It survives greyscale, it survives a screenshot, and it is a
change to one `.repeat()` call.

**The empty-column placeholder is `"  —"` in `border`** (`dashboard.rs:7929`) — an em dash at
1.32:1, which is a blank column that took a line to say so. Either say something (`nothing
here`) in `muted`, or say nothing at all. A glyph nobody can see is the worst of both.

**The header spends the accent on the logo.** ` d·ock ` is rendered accent + BOLD
(`dashboard.rs:2096-2101`) on every frame. It is the most saturated thing on screen and the
least actionable. Everywhere else in Dock accent means *this is a key you can press* or *this
is where you are*. Set the wordmark in `text`, and give the reclaimed attention to the thing
that actually changes what you do next.

## 1.4 The screenshot problem, and the thing I would draw

Ask what Dock can say that nothing else can. It is not "here are panes". It is:

> **You have been the bottleneck for one hour and forty-eight minutes today.**

Dock is the only tool in the category that can know this. It receives
`Event::AgentStateChanged { run_id, agent, state }` at the exact transition edge
(`protocol.rs:321-325`, emitted by the diff at `server.rs:652-663`) and currently does nothing
with it but overwrite a map (`dashboard.rs:1413`). No timestamp is kept. No history exists
anywhere in the product.

Keep the edges. Draw the day.

```
╭ LEDGER · today ────────────────────────────────────────────── Esc closes ─╮
│            09:00      10:00      11:00      12:00      13:00      14:00   │
│                                                                           │
│  claude #14  ▄▄▄▄▄▄████████◆◆◆◆◆◆◆▄▄▄▄▄▄▄▄▄▄▄████◆◆◆◆◆◆◆◆◆◆◆◆◆◆████▄▄▄     │
│  codex  #9   ░░░░████████████████▄▄▄██████████████▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄     │
│  amp    #22  ░░░░░░░░░░░░░░░░░░░░░░░░████◆◆◆◆████████████████████████▄▄     │
│                                                                           │
│  ████ working    ◆◆◆◆ needs you    ▄▄▄▄ your turn    ░░░░ not running     │
│                                                                           │
│  5h26m elapsed · agents worked 3h38m · waited on you 1h48m                │
│  longest single wait 34m — claude #14, 11:06 → 11:40                      │
╰───────────────────────────────────────────────────────────────────────────╯
```

That is the screenshot. It is honest, it is slightly uncomfortable, it is impossible for a tool
that does not own the PTYs, and it is *cheap*: a ring buffer of `(run_id, state, instant)`
appended on an event Dock already receives, bucketed to one cell per column at render time.
Only drawn when open, so it costs zero frames otherwise.

One caution, and it is the whole design of the feature: **this must never become a score.** No
percentage, no target, no streak, no comparison across days. It is a record, in the same voice
as the handoff queue's "what was claimed beside what was observed". The moment it grades the
user it becomes a thing people turn off.

Storage: `.dock/local` at 0700/0600 already holds durable state. A ledger record holds run ids,
agent kinds, states and timestamps — no terminal output, no commands, no credentials — which is
the same class of thing the layout records are permitted to hold under **Safety**. Say so
explicitly in that bullet when it ships; a new file under `.dock/local` that the README does
not account for is a crack in the safety story, which is Dock's best asset.

## 1.5 Motion: exactly two things move, and neither of them loops

The event loop repaints unconditionally at ~60fps — `event::poll(Duration::from_millis(16))`
at `main.rs:688`, `continue` to the top of the loop, redraw. A busy frame is 1.438 ms at
400×100 (`docs/…-cool-palette-design.md:14-19`). So animation has **no plumbing cost at all**;
the frames are already being painted. Which makes restraint a choice rather than a limitation,
and it should be an aggressive one.

**Never animate.** Anything that changes layout — no sliding panels, no eased widths, no
growing boxes. In a character grid, motion that reflows is not delight, it is nausea, and it
races the pointer-target rectangles that every render function carefully records
(`pane_control_areas`, `sidebar_agent_areas`, `board_card_areas`). Never animate anything
inside a pane body: the agent's own spinner is already moving there and a second moving thing
is competition. Never animate more than one thing at once.

**Animate exactly two things.**

1. **A one-shot on entry to `needs you`.** ~400 ms, three steps, then settled forever: the
   sidebar row's background steps `accent → blocked → none`. A one-shot is a notification; a
   loop is a distraction. This is the *only* moment in Dock that costs the user money, and it
   is currently indistinguishable from any other repaint.
2. **The wait clock, at 1 Hz.** `4m12s` ticking beside a blocked agent is the honest motion —
   it changes because the world changed, not because a timer fired. A column of live durations
   is also, incidentally, the single most "instrument-like" thing you can put in a TUI.

**Specifically do not** rotate a `◐◓◑◒` spinner per working agent. Twelve rotating glyphs in a
sidebar is a casino. If you want one, allow it only for the focused pane's agent, at 2 fps.

## 1.6 Empty states and the first sixty seconds

The empty canvas today:

```rust
// dashboard.rs:1868-1877
Paragraph::new("No workspace yet. Press Ctrl+B n to create one.")
    .block(Block::default().title(" RUNTIME "))
```

`RUNTIME` is the exact word `render_header`'s own doc comment condemns —
*"a word that means nothing to anyone using Dock"* (`dashboard.rs:2001-2007`) — surviving in the
one place a first-time user is guaranteed to look. And the sentence is grey-on-graphite
(`muted`) in a 1.32:1 box: the least legible thing on the least populated screen.

The `START HERE` menu in the sidebar (`dashboard.rs:2570-2596`) is the right instinct, in the
wrong place: it is in the 28-column rail while the entire canvas sits empty. Put the invitation
where the space is.

```
╭───────────────────────────────────────────────────────────────────────────╮
│                                                                           │
│                                                                           │
│                             d·ock                                         │
│                                                                           │
│              Every pane is a real terminal. Dock watches them             │
│              and tells you which agent needs you.                         │
│                                                                           │
│                 Ctrl+B n    a workspace to work in                        │
│                 Ctrl+B l    launch an agent here                          │
│                 Ctrl+B k    the task board — dispatch one                 │
│                 Ctrl+B ?    everything else                               │
│                                                                           │
│                                                                           │
╰───────────────────────────────────────────────────────────────────────────╯
```

Keys in accent, prose in `text` not `muted`, vertically centred, no caption on the box. That is
the first-run card the install spec asks for (`docs/…-install-and-first-run-design.md`), and it
costs one function.

Same principle for the other three empty states, which are currently one grey line each:
`"nothing on this board yet · n adds a card"` (`dashboard.rs:7515`), `"  no match"`
(`dashboard.rs:2355`), `"none running"` (`dashboard.rs:2566`). An empty state is the most
common state a new user sees. It should be the most designed, not the least.

## 1.7 A prerequisite the theme roadmap is missing: there is no light palette

The roadmap has `theme = "auto"` reading the host's background via OSC 11 and following
Omarchy's current theme live. Both `Theme::warm()` and `Theme::cool()` are dark-ground
(`surface` = `Rgb(18,18,20)` / `Rgb(18,22,26)`). There is nothing to switch *to*.

This matters more than it sounds, because of a decision documented at `theme.rs:14-19`: `panel`
is painted on chrome and deliberately **never on a pane body**, so a program's own background
shows through. On a light terminal, Dock renders dark chrome around light pane bodies. Not a
palette mismatch — a broken-looking application.

Two things follow. First, a light palette has to be *designed*, not inverted: `blocked`
`Rgb(242,114,107)` on white is a pastel that fails the 3:1 floor the test suite already
enforces. Second, `every_token_is_legible_on_both_surfaces` (`theme.rs:213`) currently only
tests `Theme::cool()` — `warm` is already drifting unchecked, which is precisely the failure
mode the `agent_states_map_to_distinct_colours` test at `theme.rs:143` was parameterised over
both palettes to prevent. Parameterise the contrast test the same way *before* adding a third
palette, or the third will ship broken.


---

# Part 2 — What is missing, and what is already 80% built

The roadmap (install, resurrection, attention routing, themes, race, `dock.toml`, palette,
snap) is settled and I do not re-litigate it. Everything below is outside it.

Ordered by leverage, not by size.

## 2.1 The headline is not true yet: 20 agents detected, 4 agents launchable

`AgentKind` has 20 variants (`detect/mod.rs:12-33`). `AdapterId` has seven, of which four are
real agents (`adapter.rs:10-19`). **The two enums have no conversion between them.** There is
no `impl From<AgentKind> for AdapterId` anywhere in the tree.

The consequences run through everything:

| Capability | Coverage |
|---|---|
| Recognised in the roster | 20 agents |
| Launchable from `Ctrl+B l` | 4 (amp, claude, codex, copilot) |
| Resumable by `Ctrl+B a` | 3 (`adapter.rs:152-167`; copilot is `None`) |
| Dispatchable with an opening prompt | 2 on argv (`adapter.rs:177-194`), 2 by typing (`204-212`) |
| Has an `awaiting` pattern set | 4 of 20 (`heuristic.rs:152-162`: claude, codex, gemini, qwen) |

So a user who runs `droid`, `opencode`, `aider` or `kiro` sees Dock name their agent in the
sidebar, and then finds that dispatching a card to it, resuming it, or reading its state
correctly are all unavailable — with no message saying why, because the two taxonomies never
meet to produce one.

**This is the most important feature work in the repository** and it is not on the roadmap.
The README's "**20 agents**" is the first bold claim a stranger reads, and today it means only
"appears in a list".

The fix is also the fix for a second problem. The per-agent knowledge that *does* exist —
executable name, resume flag, prompt argv, prompt-is-typed, awaiting patterns — is scattered
across a Rust `match` in `adapter.rs`, a Rust `match` in `detect/mod.rs`, and a JSON manifest
in `detect/manifest.rs` that carries **only three regex arrays and nothing else**
(`manifest.rs:27-42`). One agent's facts live in three places, two of which need a
recompile.

**Collapse them into one file per agent, and make it the same file the user can already
override.** `~/.config/dock/agents/droid.json`:

```json
{ "schema": 2,
  "executables": ["droid"],
  "label": "droid",
  "launch":  { "prompt_argv": "positional" },
  "resume":  { "argv": ["--continue"], "verified": "2026-08-30 from droid --help" },
  "detect":  { "blocked": ["(?i)allow this\\?"], "awaiting": ["^\\s*›\\s*$"] } }
```

Now adding an agent is a pull request against a data directory, not against `dashboard.rs`.
Now the `verified` field carries the honesty that `adapter.rs:152-167` currently expresses in
a beautiful comment ("*a wrong flag does not fail loudly, it starts a brand new session while
the user believes they resumed one*") — that comment becomes a field, and an unverified agent
can still be *contributed* while Dock refuses to resume it.

And it turns Dock's real moat — detection-as-data — into something with **distribution**.
Ship `dock agents update` pulling from a repo of community manifests. herdr's 21 agent
manifests are already a public corpus to check against. This is the one place where Dock can
win on ecosystem rather than on code.

## 2.2 Dock installs a hook, then reads only argv. This is the single biggest latent capability.

`dock hooks --install` writes six Claude Code hooks (`main.rs:1670-1681`). Claude Code delivers
each one a JSON object **on stdin** containing `session_id`, `transcript_path`, `cwd`,
`hook_event_name`, and — for `PreToolUse` — `tool_name` and `tool_input`.

`agent_state_command` (`main.rs:1376-1432`) **reads only its argv and never touches stdin.**
It sends `ReportAgentStateRequest { run_id, state }` — two fields, `deny_unknown_fields`
(`protocol.rs:391-396`) — and every other fact is discarded before it is ever seen.

Read it, and four things become possible at once, none of which a screen-reading competitor
can do at all:

**(a) The roster stops saying "working" and starts saying what.** `tool_name` plus the path
out of `tool_input` is exactly the sentence a person wants:

```
 AGENTS
 ◆ claude #14      4m12s   needs you
     wants to run  rm -rf target/
 ◐ codex  #9        18m    working
     editing       src/runtime.rs
 ◉ amp    #22       2m     your turn
     ran 14 tools this turn
 ○ zsh                     idle
```

That second line is the demo. It is not inferrable from a screen at any regex quality, and it
costs one field on an existing message.

**(b) Resurrection becomes exact, not best-effort.** The roadmap's resurrection item plans to
resume via `--continue`, and `adapter.rs:146-151` documents the flaw honestly: *"two panes
running the same agent in the same directory share a 'most recent', so resuming one of them can
land on the other's session."* With `session_id` captured per run, that ambiguity disappears —
`claude --resume <id>` names the right transcript. Capture it and the roadmap item ships
correct instead of approximate.

**(c) `transcript_path` gives Dock durable history it currently reinvents.** The roadmap plans
to persist pane scrollback to disk. For hooked agents, the agent is *already* persisting a far
better record and telling Dock where it is. Scrollback persistence is still needed for shells
and unhooked agents; for the ones that report, don't duplicate their work — point at it.

**(d) File-level conflict warnings across worktrees** — see §2.6.

Cost: one optional field on `ReportAgentStateRequest`, a stdin read in a CLI command,
a bounded per-run ring of recent activity in the daemon. It does not touch the render loop, it
does not touch git, and it does not weaken any safety claim. **Do this first.**

## 2.3 Dock's most agent-native feature is completely undocumented

`src/queue.rs` is a per-pane prompt queue with an auto-feed engine that types the next queued
prompt into an agent **at its turn boundary, while nobody is watching**. It is:

- gated by six named guards with fixed refusal sentences the user can read
  (`queue.rs:66-79`, `poll()` at `369-457`);
- clock-free by construction so the entire safety surface is unit-testable
  (`queue.rs:1-12`, 28 tests);
- default-refusing on inferred state — it acts only on a state the agent *reported*, and says
  so in words: *"this done was inferred from the screen rather than reported by the agent"*
  (`AutoFeedTrust::Reported`, `queue.rs:88-95`, guard at `427-429`);
- disarmed by a daemon restart, unconditionally, with a message
  (`DISARMED_BY_RESTART`, `queue.rs:63`);
- rate-limited to one feed per ten seconds so that even a completely broken detector cannot
  drain a queue (`QUEUE_MIN_INTERVAL`, `queue.rs:36`);
- durable across restarts (`storage.rs:300-338`) with a quarantine path for corrupt files.

**The README does not contain the word "queue" except in "review queue".** `dock queue` has
eight subcommands (`main.rs:2015-2100`) and appears in no `--help` output (§2.8). Its only TUI
surface is one letter in the board *pane* (`a arms auto-feed`, `dashboard.rs:8263`) — not the
board *overlay*, which is the one people actually open.

This is the "queue up tonight's work and go to bed" feature. It is the sharpest possible answer
to *what should an agent-native terminal do that a human-native one shouldn't*, and Dock built
it and then hid it.

**Ship it.** Three things:

1. **`Ctrl+B q`** — a queue surface: what is queued per pane, what is armed, and the refusal
   sentence when something is held. The sentences are already written and already reach the
   client through `holding_because` (`dispatch.rs:3007`).
2. **Say it in the pane title.** The title already carries state, label, location and a scroll
   marker with a shared budget. Add depth:
   ```
   ╭─ ◐ claude · #14 · dock ──────────────────────── 3 queued ⏵ armed ─╮
   ```
3. **Put it in the README's first screen**, above the board. It is a better lead than the
   board is, because every multiplexer has some kanban integration and nothing else has this.

One thing to fix while shipping it: `AutoFeedTrust` is set exactly once at daemon start from
`dockd --auto-feed-trust=screen` (`dockd.rs:86`, `dispatch.rs:2756`) and **no protocol message
can change it**. So the answer to "why won't my queue fire" is "restart the daemon with a flag
that is not in `dock --help`". Add `Queue(SetTrust)` beside the `SetAuto`/`SetPaused` that
already exist (`protocol.rs:197-226`).

## 2.4 Dependency gating: Dock built both halves and never joined them

Two independent dependency mechanisms exist, neither reaching a user.

**Half one — on the board.** `BoardTask.depends_on` is parsed from front matter
(`board.rs:415-425`) and used for exactly one thing: a `⇣` glyph on a card
(`dashboard.rs:7592-7593`, drawn at `7975-7979`). Nothing prevents dispatching a card whose
dependencies are unfinished. There is no `dock task` flag to *set* a dependency, and no CLI
display of one.

**Half two — in the daemon.** A complete gated-dispatch subsystem: `QueueGated`,
`ReleaseGate`, `InspectProgramme`, `DurableProgrammeGate` with schema v2, three storage
directories, quarantine handling, `GateState`, `DependencyGateSnapshot`,
`RepositoryPortfolioSnapshot` (`protocol.rs:476-635`, `storage.rs:136-292`,
`dispatch.rs:3748+`). Grepping `dashboard.rs` for `Programme` or `Gate` returns **nothing**.
It is reachable only from the `dock-programme` binary — which **hardcodes
`AdapterId::Fixture`** (`bin/dock-programme.rs:86`), so the whole feature can only be exercised
against the test adapter.

Join them. `Enter` on a card with unmet dependencies should not launch; it should queue a gate
that releases when the last dependency reaches `done`. That is `QueueGated` + `ReleaseGate`,
already written and already durable, wired to a board field already parsed. And it makes
"dispatch these six cards, they'll start in the right order" a one-key gesture — which is the
thing people actually want from a board with agents on it.

While there: the board's own `config.yml` declares `wip_limit`, `claim_timeout`, `priorities`
and `classes`, and `board_config.rs:200` reads past all of them. A board that declares
`wip_limit: 2` and watches Dock start five agents is a board Dock is lying to.

## 2.5 One board. `boards()` exists and has zero callers.

`board::boards()` (`board.rs:119`) enumerates every board under `~/.dock/boards` as
`(name, tasks_dir)` and **has no call site anywhere in the tree** — it is referenced only in
three `dashboard.rs` comments. The dashboard shows exactly one board: the repository's, or the
workspace's.

That is fine for one repository and wrong for the way Dock is actually used, which — per the
`dock.toml` roadmap item — is several. `Ctrl+B k` should open with a board picker when more
than one exists, using the picker overlay that is already built and already generic over
`PickerPurpose` (`dashboard.rs:2318-2321`). It is an hour of work against a function that has
been sitting finished.

## 2.6 The thing no competitor can do: advisory file claims across worktrees

Dock's whole dispatch model puts each task in its own worktree on its own branch. That is the
right isolation and it has one predictable failure: two agents, two worktrees, both rewriting
`src/dashboard.rs`, and neither finds out until a human merges.

Dock is the only thing in the room that can see both. With §2.2's hook payload it sees them
*live* — `PreToolUse` carries `tool_name` and `tool_input`, so an `Edit` on a path is a fact
Dock holds the instant it is attempted, with no polling and no git.

```
 AGENTS
 ◐ claude #14        editing  src/dashboard.rs
 ◐ codex  #9         editing  src/dashboard.rs   ⚠ also open in claude #14, 6m
```

And a CLI an agent can consult itself, since every pane already gets `DOCK_RUN`,
`DOCK_PANE`, `DOCK_BOARD` and `DOCK_SOCKET` (`runtime.rs:714-733`):

```bash
dock peers                      # what else is running, on what, and what it is touching
dock peers --touching src/dashboard.rs
```

**Advisory only.** Never a lock, never a refusal, never a write. Dock reports; the agent or
the human decides. That keeps it entirely inside the safety invariant — no repository mutation,
no adoption — while being a coordination primitive that a tool which does not own the PTYs
cannot build.

This is also the correct answer to "what should an agent-native terminal do that a
human-native one shouldn't": **make the fleet legible to its own members.** A human
multiplexer has no reason for panes to know about each other. An agent fleet has every reason.

## 2.7 Detection rules are data, and the promise has two holes

The README's strongest paragraph is *"When the state is wrong, fix it yourself"*. Two things
undercut it.

**Edited rules do not take effect until the daemon restarts.** `manifest::resolve` caches into
a process-lifetime `OnceLock<Mutex<HashMap>>` with `Box::leak` (`manifest.rs:96-131`) and
never invalidates. Nothing tells the user. `dock detect claude` re-reads the file in a fresh
process and prints the *new* rules, so the CLI confirms a change the running daemon is not
using — the worst possible failure, because the diagnostic actively lies. Add an mtime check,
or a `Queue`-style protocol message to reload. This is a bug, but it is the bug that makes the
feature feel real, so fix it as part of shipping the feature.

**A broken rules file is silent on the hot path.** `Ok(None) | Err(_) => (Source::BuiltIn, …)`
(`manifest.rs:107-109`) — a typo in your override silently reverts you to built-ins, and the
only way to find out is to run a different command. The README says *"an unknown key is refused
and names the valid ones, because a typo in a rules file is exactly where silence costs most"*.
That is true of `deny_unknown_fields` at parse time and false of the daemon at runtime. Surface
it: one `error` in the dashboard footer, which already exists and already outranks the standing
hints (`dashboard.rs:1948-1950`).

**And a capability worth adding while you are there.** The manifest compiles to `RegexSet`
(`manifest.rs:120-121`), which cannot capture. So Dock runs regexes over every agent screen
sixty times a second and can only ever answer yes/no. A `capture` section — one `Regex` per
named value — would let a rules file extract *values* from a screen with no new machinery:

```json
{ "schema": 2,
  "capture": { "model": "model:\\s*(\\S+)", "tokens": "(\\d+(?:\\.\\d+)?k?)\\s+tokens" } }
```

The test fixture at `heuristic.rs:298` is a real Claude footer containing `$0.00 · 6s`. Dock
already has that text in hand, every tick, and has no way to read a number out of it.

## 2.8 Five things that are broken enough that fixing them reads as a feature

1. **Half of `dock`'s commands are not in `--help`.** `VERBS` (`main.rs:107-128`) lists four:
   `agent`, `dispatch`, `inspect`, `workspace` — the *internal* scripting surface. `task`,
   `queue`, `detect`, `hooks`, `handoff` are handled by string comparison below it
   (`main.rs:165-196`) and appear nowhere. The five undocumented ones are the five a human
   uses. Given that the install work is precisely about the first sixty seconds, this is
   the cheapest possible win.

2. **One stale file permanently poisons the review inbox.** `save_handoff` (bare packet) and
   `save_handoff_record` (packet + evidence) write to the *same path*
   (`storage.rs:46` vs `69-74`), and `list_handoff_records` deserializes every file in the
   directory as a `HandoffRecord` with `deny_unknown_fields`, propagating the first parse error
   (`storage.rs:86-105`). One file left by the legacy `dock --save-handoff=` flag
   (`main.rs:202-210`) makes `Ctrl+B i` return `ErrorCode::Internal` forever, until someone
   deletes it by hand. Skip-and-report unreadable entries the way the queue restore already
   does (`dispatch.rs:4155-4166`).

3. **The git overlay prints the same number twice under two labels.** `git.rs:185` sets
   `status_entries: changed_files`, and `render_git` renders
   `"{changed_files} files  +{ins} −{del}   {status_entries} uncommitted"`
   (`dashboard.rs:3948-3960`). A user sees `7 files +120 −33 7 uncommitted`. Either measure
   `git status --porcelain` entries properly or delete the field — a number that is
   structurally always equal to another number is worse than no number.

4. **A handoff is refused if the agent created a file.** `GitFacts` errors out when any
   untracked file exists (`git.rs:172-177`). Agents create files constantly. The evidence half
   of the handoff — Dock's best differentiator, "what was claimed beside what was observed" —
   is unavailable for most real work. Count untracked files as evidence (`N new files`) rather
   than treating them as a reason to refuse.

5. **The README says protocol v10; `protocol.rs:11` says 13.** The help overlay renders the
   constant and is right (`dashboard.rs:3524`). One line.

Two more that are pure deletion, and deletion is design: **`AdapterCapabilities` is six booleans
that are all-false for all seven adapters** (`adapter.rs:229-239`), which makes `ProviderState`
permanently `Unknown` in every snapshot and every receipt on the wire; and
**`LifecycleOperation::Attach` and `Focus` are accepted and do nothing** (`dispatch.rs:2059`).
Inert scaffolding on a public protocol is a promise you will be held to. Cut both before
release — after a release they cost a version bump.

## 2.9 Local-first: the moat, but "local" is the wrong axis

Dock is a Unix-socket client/server with a JSON line protocol, a version handshake, an
admission-controlled 32-client limit, and per-message size caps (`server.rs:25-176`,
`protocol.rs:11-12`). It is already a *networked* program in every respect except the socket
family. There is no auth beyond filesystem permissions — `0700` runtime dir, `0600` socket
(`paths.rs:13`, `server.rs:131-135`), no `SO_PEERCRED` check — so any process running as the
same uid can issue any request, including `PaneInput`.

Three doors, and my opinion on each.

**Yes: make the client remotable, keep the daemon local.** The daemon must stay where the
repository is; that is the whole safety story. But there is no reason the *dashboard* must run
on the same machine. `dock attach ssh://host/path` — forward the socket, run the TUI locally.
Everything Dock already did for SSH (OSC 52 as the default clipboard route, precisely because
it is *"the only route that works over SSH"*, `README` clipboard table) says this is the
intended direction. This is a day of work and it is the whole of "remote".

**No: multiplayer.** Two humans driving one agent fleet is a coordination problem nobody has
yet, and solving it costs identity, presence, conflict resolution on `PaneInput`, and a reason
for the `0600` socket to become something else. Every one of those is a load-bearing wall.

**Absolutely not: a web view.** A read-only browser dashboard is the single most requested and
most destructive feature available here. It requires a listening TCP socket, therefore auth,
therefore accounts, therefore a service — and the sentence "Dock touches your repository in
exactly one way" stops being checkable by reading a README. Dock's safety story is its best
asset and it is *auditable* precisely because everything is a local socket and a `git worktree
add`. Do not trade that for a URL.

If pressed on remote *visibility* rather than remote *control*, the answer is the roadmap's own
`dock snap` — render the dashboard to SVG, write it to a file, let the user put it wherever
they like. Push, not pull. No listener.

**One thing to add before any of this: check the peer.** `SO_PEERCRED` / `getpeereid` on
accept, refuse a uid that is not yours. Filesystem permissions are the right control and a
second, explicit one costs ten lines and is the thing a security-minded reader looks for.

## 2.10 What Dock should refuse, in writing

The **Safety** section is Dock's best writing and its best moat. Extend it with the refusals
that are about *judgement* rather than about git, because those are the ones a competitor
will violate first and the ones users will ask for hardest:

- **Never auto-answer a permission prompt.** This will be the most-requested feature Dock ever
  receives — "just let it approve `git status`" — and it is the one that ends the product. The
  moment Dock decides on the user's behalf, "needs you" stops meaning anything and the whole
  attention model collapses. Refuse it by name, in the README, so the answer is a policy and
  not a backlog item.
- **Never summarise an agent's output with another model.** No LLM in the multiplexer. Dock
  reports what it measured; it does not have opinions. This is also what keeps it free,
  offline, and installable in one command.
- **No telemetry, no accounts, no phone-home, ever.** Today there is none — no OTel, no
  counters, no log sink, failures go to stderr. Say so, because saying so is worth more than
  having it.
- **Never be a chat UI.** Every agent already has one. Dock's job is the space *between* them.
- **Never move a task because an agent looks finished.** Already stated and already right
  (`README`); it belongs in the same list as the others.

A written refusal is a feature. It is the thing that makes someone trust a tool enough to give
it their whole terminal.

---

# Part 3 — What I would do, in order

## The first week (all of it is small, all of it ships)

| # | Change | Cost | Why first |
|---|---|---|---|
| 1 | `border_pane` at ≥2:1, with a test | 30 min | Every screenshot from now on. Nothing else changes the product's appearance this much per line. |
| 2 | Four distinct state glyphs `○ ◐ ◉ ◆` | 1 h | Fixes an accessibility defect and a screenshot defect with one function. |
| 3 | Board age colours into the theme, as a recede-not-shout ramp | 2 h | Stops Dock's flagship surface being the one that isn't Dock's. |
| 4 | Every command in `--help`; README's `v10` → `v13` | 1 h | Half the CLI is currently invisible during the exact week the install story ships. |
| 5 | Empty canvas → a real first-run card; kill `RUNTIME` | 2 h | It is the first screen and it is the worst one. |
| 6 | The rename-overlay title bug; one `×`; one `›`; one `−` | 1 h | The difference between designed and accumulated. |

## The month, in dependency order

7. **Read the hook payload** (§2.2). Everything below is better with it, and two roadmap items
   (resurrection, attention routing) get correct instead of approximate.
8. **Ship the prompt queue** (§2.3) — `Ctrl+B q`, depth in the pane title, README lead, and a
   protocol message for `AutoFeedTrust`.
9. **The ledger** (§1.4) — the screenshot, and the answer to "who was the bottleneck" that the
   roadmap lists as a smaller item. Richer once (7) lands.
10. **Unify `AgentKind` and `AdapterId` into one manifest per agent** (§2.1), and stand up
    `dock agents update`. This is the ecosystem play.
11. **Overlay tiers + scrim** (§1.2.4), and dock the one-line prompts into the footer.
12. **Advisory peer awareness** (§2.6) — `dock peers`, the ⚠ on the roster row.
13. **Join the two halves of dependency gating** (§2.4).
14. `SO_PEERCRED`; the handoff-inbox poisoning fix; untracked files as evidence; cut
    `AdapterCapabilities` and the no-op lifecycle ops before release.

## Traps — things I would not do, ranked by how attractive they look

1. **A web dashboard.** §2.9. Costs the safety story, which is the only thing that cannot be
   copied.
2. **Auto-approving permission prompts.** §2.10. Kills the attention model, which is the
   product.
3. **A light theme made by inverting the dark one.** `blocked` `Rgb(242,114,107)` on white
   fails the 3:1 floor the test suite already enforces. And parameterise the contrast test over
   both existing palettes *first* — `warm` is drifting unchecked today (`theme.rs:213`).
4. **Adding a 21st detected agent** before the existing 20 can be launched, resumed and
   dispatched. Breadth on the roster is a number; breadth on the verbs is a product.
5. **Any looping animation.** The 60fps repaint (`main.rs:688`) makes motion free, which is
   exactly why it needs a written budget: one 400ms one-shot on entry to `needs you`, and a
   1 Hz wait clock. Nothing else.
6. **Conditional repainting.** Already declined with reasoning in
   `docs/…-cool-palette-design.md:20-25`. It is still the right call; do not reopen it because
   the ledger adds a surface.
7. **Turning the ledger into a score.** The moment it has a target, a streak or a percentage,
   it becomes a thing people turn off, and Dock's only irreplaceable dataset goes with it.

---

## The bet

**Stop reading screens where the agent will tell you, and then draw what it said.**

Dock already installs six hooks into Claude Code and then reads only its own argv
(`main.rs:1376-1432`), discarding `session_id`, `transcript_path`, `cwd`, `tool_name` and
`tool_input` — every one of which arrives on stdin, unasked-for, on every turn of every hooked
agent. Screen-scraping is Dock's fallback and it is doing work the agent has already done.

Read that payload, and the roster stops saying **working** and starts saying
**editing `src/runtime.rs`, fourteen tools this turn, four minutes**. Resumption stops being
"the most recent session in this directory, probably" and becomes a session id. Two agents
about to collide on the same file become visible before the merge instead of after it. And the
day becomes a thing you can draw:

```
  claude #14  ▄▄▄▄▄▄████████◆◆◆◆◆◆◆▄▄▄▄▄▄▄▄▄▄▄████◆◆◆◆◆◆◆◆◆◆◆◆◆◆████▄▄▄
  codex  #9   ░░░░████████████████▄▄▄██████████████▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄
  amp    #22  ░░░░░░░░░░░░░░░░░░░░░░░░████◆◆◆◆████████████████████████▄▄

  5h26m elapsed · agents worked 3h38m · waited on you 1h48m
```

None of it is inferrable from a terminal screen at any regex quality, so none of it can be
copied by a tool built on screen-reading. It costs one optional field on an existing protocol
message, one stdin read, and a ring buffer. And it is the only thing in this review that would
make someone post a screenshot of a terminal multiplexer.
