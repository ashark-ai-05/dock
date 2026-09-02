# Workspace Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn one 55,000-line crate into a Cargo workspace of seven library crates plus the existing binaries, so that "only one crate may execute argv Dock did not author" becomes a rule tools check rather than a convention reviewers remember.

**Architecture:** Extract bottom-up along the verified dependency order, one crate per task, with `cargo test` green after every task. The technique that keeps this cheap is a **re-export shim**: after moving `src/detect/` into `crates/dock-detect/`, the root crate adds `pub use dock_detect as detect;`, so every existing `crate::detect::AgentKind` path in the other 50 files keeps resolving untouched. Call-site churn is therefore near zero, and each task's diff is a move plus one `Cargo.toml` plus one shim line.

**Tech Stack:** Rust 2024 edition, Cargo workspaces. No new external dependencies — every crate's dependency set below was read off the real imports, not guessed.

**Spec:** `docs/superpowers/specs/2026-09-02-dock-receipts-design.md` — section 7 "Architecture" and row 1 of section 9.

## Global Constraints

- **`dashboard.rs` is NOT dissolved by this plan.** It moves whole into `dock-ui` as one large module. Spec section 8 records why: the Split Spine row rewrites those surfaces anyway, and cutting the file up here means cutting it up twice. Do not split it, do not reorganise it, do not "tidy while you're in there."
- **The published identity must not change.** The root package stays `dock-tui`, its lib target stays `name = "dock"`, and the binaries stay `dock` and `dockd`. `cargo install --path . --locked` must still produce those two binaries. This is the install story; breaking it breaks the README.
- **Every task ends with `cargo test` green.** Not `cargo test --lib` — 44 tests live in `src/main.rs`, the `dock` binary target, and `--lib` skips them silently. The workspace form is `cargo test --workspace`.
- **Gates are pass/fail and must be read, not grepped for a line count:** `cargo test --workspace`, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`. A clippy failure once reached `main` because a count of matching lines looked like zero.
- **Test count is the invariant.** 836 passed + 12 ignored = 848 registered before this plan. This plan moves tests between targets; the **total must not change**. After each task, sum the per-target `test result:` lines and confirm 836/12. A drop means a test module was left behind by a move, which compiles fine and silently deletes coverage.
- **Use `git mv`, never copy-and-delete.** History has to follow these files; a copy makes `git log --follow` useless on a 19,000-line file.
- **No behaviour changes.** No renamed public items, no changed signatures, no "while I'm here" fixes. If you find a bug, report it; do not fix it.
- **`pub(crate)` becoming `pub` is expected, and is not a behaviour change.** An item marked `pub(crate)` is private to its crate, so the moment its callers are in a different crate it must widen to `pub` or the workspace does not compile. This is an inherent cost of splitting and it will recur in Tasks 4, 5 and 6. Widen it, and add a one-line comment at each site saying it is public because of the crate boundary rather than because the API grew on purpose. Do **not** work around it by moving the caller, duplicating the helper, or re-exporting through a shim — those are worse than an honest `pub`. Do report every widening in your task report so the reviewer can confirm each one was forced rather than convenient.
- **Use `sed -E`, and plain parentheses, for every alternation.** macOS ships BSD `sed`, which does not understand GNU's `\(a\|b\)` syntax — it does not error, it silently matches nothing, so the rewrite appears to succeed and the build then fails somewhere unrelated. Write `sed -E -i '' -e 's/crate::(a|b)::/dock_x::\1::/g'`. Verify a rule fires before trusting it: `echo "crate::board::X" | sed -E 's/crate::(board|model)::/dock_model::\1::/'` must print the rewritten form.
- **Every crate's manifest in this plan is a starting point, not a specification.** They were derived from a per-file scan of top-of-file `use` lines, which misses fully-qualified calls (`vt100::Parser`), test-only imports, and braced forms. Task 5 had to add `vt100` and `serde_json` that the plan omitted. Add what the compiler demands, remove nothing the compiler does not complain about, and list every deviation in your report.
- **A `sed` cannot rewrite a braced import, and the biggest files use them.** `use crate::{ adapter::{…}, board::{…}, detect::{…} }` does not contain the string `crate::adapter::`, so every `sed 's/crate::X::/dock_x::X::/'` in this plan silently skips it. These blocks must be split by hand, one `use` per destination crate. Task 4 did this correctly in `protocol.rs` — follow that shape:

  ```rust
  // before
  use crate::{
      adapter::{AdapterCapabilities, AdapterId},
      detect::{AgentKind, AgentState},
      layout::{LayoutSnapshot, PaneKind},
  };

  // after: siblings stay in crate::{}, everything else is lifted out by crate
  use crate::{
      adapter::{AdapterCapabilities, AdapterId},
      layout::{LayoutSnapshot, PaneKind},
  };
  use dock_detect::{AgentKind, AgentState};
  ```

  Before running any `sed`, list the braced blocks you must split: `grep -n "use crate::{" <files>`. `dashboard.rs` has one spanning five destination crates; `dispatch.rs` and `server.rs` have their own. After the `sed`, `cargo build` is what proves you caught them all — a missed one is a compile error, not a silent bug, so build early.
- **Doc-link counts in this plan are estimates.** Where a task says "five broken doc links", treat the number as a hint, not a specification: verify against the pre-task commit and fix what is actually broken. Reporting "the plan said five, I found two, here is the evidence" is the correct outcome; inventing three more edits to match a number is not.
- Every crate is `edition = "2024"`, `version = "0.1.0"`, `license = "MIT"`, and inherits nothing it does not use.

## Verified dependency order

Each crate depends only on crates above it. Every edge was checked against real imports.

```
dock-testing    (dev-dependency of everything; depends on nothing)
dock-detect     regex, serde, serde_json
dock-git        std only
dock-model   →  dock-detect                    base64, notify, serde, serde_json
dock-pty     →  dock-detect, dock-model        crossterm, nix, vt100
dock-ui      →  dock-detect, dock-model, dock-git   base64, crossterm, ratatui, tui_term
dock-daemon  →  all of the above               base64, regex, serde, serde_json
dock-tui        the root crate: binaries `dock` and `dockd`, plus `src/main.rs` and `src/cli/`
```

Two constraints that dictate crate membership, both verified:
- **`board` and `board_config` reference each other** (`board::STATUSES` is defined as `board_config::KANBAN_MD_STATUSES`; `board_config`'s default reads back through `board::STATUSES`). Legal inside one crate, fatal across two. Both go in `dock-model` and must not be separated.
- **`protocol` references `queue`** (the `From` impls for `AutoFeedTrust`). Both go in `dock-model`.

---

### Task 1: `dock-testing`, and the workspace that holds it

Nothing else can start before this. `src/testing.rs` exports `budget`, `budget_millis` and `deadline`, used in **42 places** across `runtime`, `dispatch`, `client` and `server` — four modules that become four different crates. It is declared `#[cfg(test)] pub mod testing;`, which makes it invisible outside its own crate, so the first move that crosses a boundary breaks all 42 call sites at once.

**Files:**
- Create: `crates/dock-testing/Cargo.toml`, `crates/dock-testing/src/lib.rs`
- Modify: `Cargo.toml` (root — add `[workspace]`), `src/lib.rs` (drop the `testing` module)
- Delete: `src/testing.rs` (via `git mv`)

**Interfaces:**
- Consumes: nothing.
- Produces: crate `dock-testing` exporting `pub fn budget(seconds: u64) -> Duration`, `pub fn budget_millis(milliseconds: u64) -> Duration`, `pub fn deadline(seconds: u64) -> Instant`. Every later task's crate takes it as a `[dev-dependencies]` entry. Note it is a NORMAL crate, not `cfg(test)`-gated — a dev-dependency is already only compiled for tests.

- [ ] **Step 1: Move the file and create the crate**

```bash
mkdir -p crates/dock-testing/src
git mv src/testing.rs crates/dock-testing/src/lib.rs
```

Create `crates/dock-testing/Cargo.toml`:

```toml
[package]
name = "dock-testing"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Timing budgets for Dock's tests. Not published; a dev-dependency of every Dock crate."
publish = false
```

- [ ] **Step 2: Make the root a workspace and depend on it**

In the root `Cargo.toml`, add above `[package]`:

```toml
[workspace]
members = ["crates/*"]
```

and under `[dev-dependencies]`:

```toml
dock-testing = { path = "crates/dock-testing" }
```

- [ ] **Step 3: Point the 42 call sites at the new crate**

Remove `#[cfg(test)] pub mod testing;` from `src/lib.rs`. Then rewrite every reference. The call sites all read `crate::testing::` or `testing::` after a `use crate::testing`:

```bash
grep -rln "crate::testing" src/ | xargs sed -i '' 's/crate::testing::/dock_testing::/g'
grep -rn "use crate::testing" src/            # expect no output; fix any by hand
```

Then check for bare `testing::` left behind by a `use` statement you removed:

```bash
grep -rn "\btesting::" src/ | grep -v dock_testing
```

Expect no output.

- [ ] **Step 4: Verify**

Run: `cargo test --workspace`
Expected: **836 passed + 12 ignored** across all targets. The count must not move — you relocated a helper, not a test.

Run: `cargo fmt --check` — expect no output.
Run: `cargo clippy --workspace --all-targets -- -D warnings` — read it; expect no warnings.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: dock-testing is a crate, not a cfg(test) module

Its three timing helpers are used in 42 places across four modules that
are about to become four crates, and a cfg(test) module is invisible
across a crate boundary. Extracting it first is what lets every later
extraction compile."
```

---

### Task 2: `dock-detect` and `dock-pty`, together

These two are extracted in one task because they are entangled in a way that makes either order impossible alone: `dock-pty` depends on `dock-detect` normally (`runtime.rs` stores `AgentKind` and `AgentState` in its run records), and `dock-detect` depends on `dock-pty` **only inside `#[cfg(test)]`** (`detect/mod.rs:153` and `detect/heuristic.rs:260` both `use crate::terminal::VtTerminal`).

Cargo forbids cycles between normal dependencies but **permits them through `[dev-dependencies]`**, so the pair is legal — but only once both crates exist. Extract either alone and it does not compile.

**Files:**
- Create: `crates/dock-detect/Cargo.toml`, `crates/dock-pty/Cargo.toml`
- Move: `src/detect/` → `crates/dock-detect/src/`, `src/terminal/` → `crates/dock-pty/src/terminal/`, `src/runtime.rs` → `crates/dock-pty/src/runtime.rs`
- Modify: `src/lib.rs` (shims), root `Cargo.toml`

**Interfaces:**
- Consumes: `dock-testing` from Task 1.
- Produces: crates `dock_detect` and `dock_pty`. The root crate re-exports both under their old names, so `crate::detect::…` and `crate::terminal::…` keep resolving everywhere else.

- [ ] **Step 1: Move both**

`src/runtime.rs` does **not** move in this task, even though it belongs in `dock-pty`. Its line 728 calls `board::resolve_tasks_dir`, and `dock-model` does not exist until Task 4, so `dock-pty` would have nowhere to point. It moves in Task 4 Step 5. Move only `src/terminal/` here.

```bash
mkdir -p crates/dock-detect/src crates/dock-pty/src
git mv src/detect/mod.rs crates/dock-detect/src/lib.rs
git mv src/detect/heuristic.rs src/detect/manifest.rs src/detect/process.rs crates/dock-detect/src/
rmdir src/detect
git mv src/terminal crates/dock-pty/src/terminal
git mv src/clipboard.rs crates/dock-pty/src/clipboard.rs
```

`clipboard.rs` comes here, not to `dock-ui`. It writes OSC 52 escape sequences to the terminal's file descriptor and spawns `pbcopy` / `wl-copy` / `xclip` — terminal and OS integration, not drawing. Putting it in `dock-ui` would make that crate a process-spawner and break Task 7's lint.

Create `crates/dock-pty/src/lib.rs`:

```rust
//! Real PTYs: the terminal emulator, key encoding, and the process groups Dock owns.
//!
//! `runtime` joins this crate in Task 4, once `dock-model` exists for its one board call.
pub mod clipboard;
pub mod terminal;
```

- [ ] **Step 2: Write both manifests**

`crates/dock-detect/Cargo.toml`:

```toml
[package]
name = "dock-detect"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Which coding agent is in a pane, and whether it is working, blocked, done or idle."

[dependencies]
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
# A dependency cycle, and a legal one: detect needs a real terminal only to test
# classification against captured screens. Cargo forbids cycles among normal
# dependencies and permits them among dev-dependencies.
dock-pty = { path = "../dock-pty" }
```

`crates/dock-pty/Cargo.toml`:

```toml
[package]
name = "dock-pty"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Dock's PTYs: vt100 emulation, key encoding, and owned process groups."

[dependencies]
base64 = "0.23"          # OSC 52 clipboard payloads
crossterm = "0.29"
nix = { version = "=0.29.0", features = ["process", "signal", "term"] }
vt100 = "0.16"
dock-detect = { path = "../dock-detect" }

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
```

`dock-pty` gains a `dock-model` dependency in Task 4, when `runtime.rs` joins it. It does not need one yet.

- [ ] **Step 3: Add the shims**

In root `Cargo.toml` `[dependencies]`:

```toml
dock-detect = { path = "crates/dock-detect" }
dock-pty = { path = "crates/dock-pty" }
```

In `src/lib.rs`, replace `pub mod detect;` and `pub mod terminal;` with:

```rust
// The extracted crates keep their old paths so every `crate::detect::…` and
// `crate::terminal::…` call site in this crate resolves unchanged.
pub use dock_detect as detect;
pub use dock_pty::{clipboard, terminal};
```

- [ ] **Step 4: Fix paths inside the moved code**

Inside `crates/dock-detect/`, `crate::` now means `dock_detect`. Its own `crate::detect::X` self-references become `crate::X`:

```bash
sed -i '' 's/crate::detect::/crate::/g' crates/dock-detect/src/*.rs
```

Its two test-only terminal imports become the dev-dependency:

```bash
sed -i '' 's/use crate::terminal::VtTerminal;/use dock_pty::terminal::VtTerminal;/' crates/dock-detect/src/lib.rs crates/dock-detect/src/heuristic.rs
```

Inside `crates/dock-pty/src/terminal/`, `crate::terminal::` self-references become `crate::terminal::` still (the module is nested under the new lib root, so these are unchanged) — but verify with a build rather than assuming.

- [ ] **Step 5: Fix the five broken doc links**

Five rustdoc `[crate::…]` links now point across a crate boundary and become broken intra-doc links. Find and repoint them:

```bash
grep -rn "\[\`crate::" crates/ src/ --include='*.rs'
```

For each, either repoint it at the new crate path (`dock_pty::terminal::VtTerminal`) or, where the target is no longer reachable, unlink it — turn `` [`crate::dispatch`] `` into plain `` `dispatch` ``. Do not delete the sentence.

Confirm with: `cargo doc --workspace --no-deps 2>&1 | grep -i "unresolved link"` — expect no output.

- [ ] **Step 6: Verify**

Run: `cargo test --workspace`
Expected: **836 passed + 12 ignored**, summed across every target. Confirm the sum; the per-target split will differ from before because tests moved with their modules.

Run: `cargo fmt --check` — no output.
Run: `cargo clippy --workspace --all-targets -- -D warnings` — read it.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: extract dock-detect and dock-pty

Together, because they are entangled: dock-pty needs AgentKind and
AgentState, and dock-detect needs a real terminal to test screen
classification. The second edge is test-only, which makes it legal as
a dev-dependency — Cargo forbids cycles among normal dependencies and
permits them among dev ones. Neither extracts alone."
```

---

### Task 3: `dock-git`

The smallest and most isolated extraction: `git.rs` imports nothing from the crate and nothing external. Doing it here, before the large `dock-model` move, proves the workspace plumbing on a file where a mistake is obvious.

**Files:**
- Create: `crates/dock-git/Cargo.toml`, `crates/dock-git/src/lib.rs` (from `src/git.rs`)
- Modify: `src/lib.rs`, root `Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: crate `dock_git`, re-exported as `crate::git`.

- [ ] **Step 1: Move**

```bash
mkdir -p crates/dock-git/src
git mv src/git.rs crates/dock-git/src/lib.rs
git mv src/files.rs crates/dock-git/src/files.rs
```

Add `pub mod files;` to the top of `crates/dock-git/src/lib.rs`.

`files.rs` comes here, not to `dock-ui`. Its own doc comment says it lists "the files under a directory, for the file picker" — and it does that by running `git -C <root> ls-files`. It is a Git query wearing a picker's name. In `dock-ui` it would make that crate a process-spawner and break Task 7's lint.

- [ ] **Step 2: Manifest**

```toml
[package]
name = "dock-git"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Read-only Git facts: SHA, diffstat, dirty state, worktrees. Never a mutation."

[dependencies]

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
```

Note the empty `[dependencies]`: this crate is std-only, `files.rs` included. It spawns `git` and `delta`, both with argv fixed at compile time.

- [ ] **Step 3: Shim**

Root `Cargo.toml`: `dock-git = { path = "crates/dock-git" }`.
`src/lib.rs`: replace `pub mod git;` and `pub mod files;` with:

```rust
pub use dock_git as git;
pub use dock_git::files;
```

- [ ] **Step 4: Verify**

Run: `cargo test --workspace` → **836 passed + 12 ignored**.
Run: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`. Read both.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: extract dock-git, which depends on nothing

Std-only, and it spawns git and delta with argv fixed at compile time.
Smallest possible proof that the workspace plumbing is right."
```

---

### Task 4: `dock-model`, and `runtime.rs` finally moves

The big one. `protocol`, `queue`, `storage`, `model`, `board`, `board_config`, `board_watch`, `paths`, `adapter` and `layout` move together, because three dependencies among them cannot be separated:

- `board::STATUSES` is defined as `board_config::KANBAN_MD_STATUSES`, and `board_config`'s default reads back through `board::STATUSES`.
- `protocol` holds `From` impls converting `queue::AutoFeedTrust` to its wire form.
- `protocol` imports types from `adapter` and `layout` — `use crate::{adapter::{AdapterCapabilities, AdapterId, …}, layout::{LayoutSnapshot, PaneKind, …}}`. Both would otherwise be `dock-daemon`, which depends on `dock-model`; keeping them there is a cycle. Both are leaves, so they come here.

Once `dock-model` exists, `runtime.rs` can finally move into `dock-pty` — its one outside call is `board::resolve_tasks_dir` (`runtime.rs:728`).

**Files:**
- Create: `crates/dock-model/Cargo.toml`, `crates/dock-model/src/lib.rs`
- Move into `crates/dock-model/src/`: `protocol.rs`, `queue.rs`, `storage.rs`, `model.rs`, `board.rs`, `board_config.rs`, `board_watch.rs`, `paths.rs`, `adapter.rs`, `layout.rs`
- Move: `src/runtime.rs` → `crates/dock-pty/src/runtime.rs`
- Modify: `src/lib.rs`, root `Cargo.toml`, `crates/dock-pty/Cargo.toml`, `crates/dock-pty/src/lib.rs`

**Interfaces:**
- Consumes: `dock-detect` (queue and model both reference `AgentKind`/`AgentState`).
- Produces: crate `dock_model`, whose modules are re-exported individually so `crate::protocol::…`, `crate::board::…` and the rest keep resolving.

- [ ] **Step 1: Move the eight modules**

```bash
mkdir -p crates/dock-model/src
git mv src/protocol.rs src/queue.rs src/storage.rs src/model.rs \
       src/board.rs src/board_config.rs src/board_watch.rs src/paths.rs \
       crates/dock-model/src/
```

Create `crates/dock-model/src/lib.rs`:

```rust
//! Dock's durable shapes: the wire protocol, the board, the queue, and what is stored on disk.
//!
//! `board` and `board_config` reference each other, and `protocol` references `queue`. Those
//! cycles are why these eight modules are one crate rather than several: legal within a
//! crate, impossible across one.
pub mod board;
pub mod board_config;
pub mod board_watch;
pub mod model;
pub mod paths;
pub mod protocol;
pub mod queue;
pub mod storage;
```

- [ ] **Step 2: Manifest**

```toml
[package]
name = "dock-model"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Dock's durable shapes: wire protocol, board, prompt queue, and on-disk state."

[dependencies]
base64 = "0.23"
notify = "6.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
dock-detect = { path = "../dock-detect" }

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
```

- [ ] **Step 3: Fix internal paths**

Inside `crates/dock-model/`, sibling references lose their old prefixes. These modules referred to each other as `crate::protocol::`, `crate::board::` and so on — which still resolve, because they are still siblings under a crate root. What changes is references *out*: `crate::detect::` becomes `dock_detect::`.

```bash
sed -i '' 's/crate::detect::/dock_detect::/g' crates/dock-model/src/*.rs
grep -rn "crate::" crates/dock-model/src/ | grep -vE "crate::(board|board_config|board_watch|model|paths|protocol|queue|storage)"
```

The grep must return nothing but doc links. Fix anything else by hand.

- [ ] **Step 4: Shim**

Root `Cargo.toml`: `dock-model = { path = "crates/dock-model" }`.
In `src/lib.rs`, replace the eight `pub mod` lines with:

```rust
// Re-exported individually rather than as one `dock_model` module, so that every existing
// `crate::protocol::…` and `crate::board::…` path in this crate resolves unchanged.
pub use dock_model::{board, board_config, board_watch, model, paths, protocol, queue, storage};
```

- [ ] **Step 5: Move `runtime.rs` into `dock-pty`**

```bash
git mv src/runtime.rs crates/dock-pty/src/runtime.rs
```

Add to `crates/dock-pty/Cargo.toml` `[dependencies]`:

```toml
dock-model = { path = "../dock-model" }
```

Add to `crates/dock-pty/src/lib.rs`: `pub mod runtime;`

Fix its outward references:

```bash
sed -E -i '' -e 's/crate::detect::/dock_detect::/g' \
          -e 's/crate::board::/dock_model::board::/g' \
          -e 's/crate::terminal::/crate::terminal::/g' \
          crates/dock-pty/src/runtime.rs
sed -i '' 's/crate::testing::/dock_testing::/g' crates/dock-pty/src/runtime.rs
```

In `src/lib.rs`, replace `pub mod runtime;` with `pub use dock_pty::runtime;`.

- [ ] **Step 6: Verify**

Run: `cargo test --workspace` → **836 passed + 12 ignored**. This is the task most likely to lose a test module silently; sum the per-target lines and confirm before continuing.

Run: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`. Read both.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: extract dock-model, and move runtime into dock-pty

Eight modules move together because two pairs among them reference each
other: board <-> board_config, and protocol -> queue. Those cycles are
legal inside a crate and impossible across one.

runtime.rs could not move in the earlier task because its one outside
call is board::resolve_tasks_dir. With dock-model extracted it has
somewhere to point, so dock-pty is now complete."
```

---

### Task 5: `dock-ui`

Everything that draws. `dashboard.rs` moves **whole and untouched** — see Global Constraints.

**Files:**
- Create: `crates/dock-ui/Cargo.toml`, `crates/dock-ui/src/lib.rs`
- Move into `crates/dock-ui/src/`: `theme.rs`, `verdict.rs`, `dashboard.rs`, `copy.rs`, `picker.rs`, `keymap.rs`, `attention.rs`

`clipboard.rs` and `files.rs` are NOT here — they moved to `dock-pty` (Task 2) and `dock-git` (Task 3) respectively, because both spawn processes and `dock-ui` must not.
- Modify: `src/lib.rs`, root `Cargo.toml`

**Interfaces:**
- Consumes: `dock-detect`, `dock-model`, `dock-git`.
- Produces: crate `dock_ui`, modules re-exported individually.

- [ ] **Step 1: Move**

```bash
mkdir -p crates/dock-ui/src
git mv src/theme.rs src/verdict.rs src/dashboard.rs src/copy.rs src/picker.rs \
       src/keymap.rs src/attention.rs \
       crates/dock-ui/src/
```

Create `crates/dock-ui/src/lib.rs`:

```rust
//! Everything Dock draws: the palette, the widgets, and the dashboard.
//! Nothing here spawns a process; Task 7 makes that a build failure rather than a promise.
pub mod attention;
pub mod copy;
pub mod dashboard;
pub mod keymap;
pub mod picker;
pub mod theme;
pub mod verdict;
```

- [ ] **Step 2: Manifest**

```toml
[package]
name = "dock-ui"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Dock's rendering: palette, widgets, dashboard. Draws; never spawns."

[dependencies]
base64 = "0.23"
crossterm = "0.29"
ratatui = "0.30"
tui-term = "0.3"
dock-detect = { path = "../dock-detect" }
dock-git = { path = "../dock-git" }
dock-model = { path = "../dock-model" }
dock-pty = { path = "../dock-pty" }

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
```

`dock-ui` depends on `dock-pty` for exactly two reasons, both in `dashboard.rs`: it renders a pane's screen, and it yanks to the clipboard. It does not spawn anything itself, which is what Task 7 enforces.

- [ ] **Step 3: Fix outward paths**

```bash
cd crates/dock-ui/src
sed -E -i '' -e 's/crate::detect::/dock_detect::/g' \
          -e 's/crate::git::/dock_git::/g' \
          -e 's/crate::(adapter|board|board_config|board_watch|layout|model|paths|protocol|queue|storage)::/dock_model::\1::/g' \
          -e 's/crate::(terminal|clipboard)::/dock_pty::\1::/g' \
          -e 's/crate::files::/dock_git::files::/g' \
          *.rs
cd -
```

- [ ] **Step 4: Shim**

Root `Cargo.toml`: `dock-ui = { path = "crates/dock-ui" }`.
`src/lib.rs`: replace the nine `pub mod` lines with:

```rust
pub use dock_ui::{attention, copy, dashboard, keymap, picker, theme, verdict};
```

- [ ] **Step 5: Verify**

Run: `cargo test --workspace` → **836 passed + 12 ignored**.
Run: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`. Read both.

Then confirm `dashboard.rs` moved unmodified:

```bash
git diff --stat HEAD~0 -- crates/dock-ui/src/dashboard.rs
git log --follow --oneline -1 -- crates/dock-ui/src/dashboard.rs
```

The second must show the file's history survived the move.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: extract dock-ui

dashboard.rs moves whole and untouched. The Split Spine row rewrites
those surfaces anyway; cutting the file up here means cutting it up
twice."
```

---

### Task 6: `dock-daemon`, and the crate that spawns nothing

The last extraction, and the one that makes the safety claim structural. What remains in the root crate afterwards is `main.rs` and `src/cli/` — the binaries.

**Files:**
- Create: `crates/dock-daemon/Cargo.toml`, `crates/dock-daemon/src/lib.rs`
- Move into `crates/dock-daemon/src/`: `dispatch.rs`, `server.rs`, `client.rs`, `discovery.rs`, `hook.rs`

`adapter.rs` and `layout.rs` are NOT here. Task 4 moved them into `dock-model`, because `protocol.rs` imports types from both (`use crate::{adapter::{…}, layout::{…}}`) and `protocol` is `dock-model`. Leaving them for this task would have made `dock-model` depend on `dock-daemon`, which already depends on `dock-model` — a cycle. Both are dependency leaves (`adapter` imports nothing from the crate; `layout` imports only `adapter`), so they sit in `dock-model` without pulling anything else in.
- Modify: `src/lib.rs`, root `Cargo.toml`

**Interfaces:**
- Consumes: every crate above.
- Produces: crate `dock_daemon`, modules re-exported individually so `src/cli/` and `src/main.rs` resolve unchanged.

- [ ] **Step 1: Move**

```bash
mkdir -p crates/dock-daemon/src
git mv src/dispatch.rs src/server.rs src/client.rs \
       src/discovery.rs src/hook.rs \
       crates/dock-daemon/src/
```

Create `crates/dock-daemon/src/lib.rs`:

```rust
//! The daemon: the socket server, dispatch, and the hooks agents report through.
//!
//! `adapter` and `layout` live in `dock-model`, not here — `protocol` imports from both, and
//! `protocol` is `dock-model`.
pub mod client;
pub mod discovery;
pub mod dispatch;
pub mod hook;
pub mod server;
```

- [ ] **Step 2: Manifest**

```toml
[package]
name = "dock-daemon"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Dock's daemon: socket server, dispatch, worktree binding, agent adapters."

[dependencies]
base64 = "0.23"
nix = { version = "=0.29.0", features = ["process", "signal", "term"] }
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
dock-detect = { path = "../dock-detect" }
dock-git = { path = "../dock-git" }
dock-model = { path = "../dock-model" }
dock-pty = { path = "../dock-pty" }

[dev-dependencies]
dock-testing = { path = "../dock-testing" }
```

- [ ] **Step 3: Fix outward paths**

```bash
cd crates/dock-daemon/src
sed -E -i '' -e 's/crate::detect::/dock_detect::/g' \
          -e 's/crate::git::/dock_git::/g' \
          -e 's/crate::(adapter|board|board_config|board_watch|layout|model|paths|protocol|queue|storage)::/dock_model::\1::/g' \
          -e 's/crate::(terminal|runtime|clipboard)::/dock_pty::\1::/g' \
          -e 's/crate::testing::/dock_testing::/g' \
          *.rs
cd -
```

- [ ] **Step 4: Shim**

Root `Cargo.toml`: `dock-daemon = { path = "crates/dock-daemon" }`.
`src/lib.rs` becomes, in full:

```rust
//! `dock-tui` is now a thin crate: the two binaries and their command surface.
//!
//! Every module below lives in a workspace crate and is re-exported here under the name it
//! had when it was a module, so `crate::protocol::…` and the rest keep resolving in
//! `src/main.rs` and `src/cli/`.
pub mod cli;

pub use dock_daemon::{client, discovery, dispatch, hook, server};
pub use dock_detect as detect;
pub use dock_git as git;
pub use dock_model::{
    adapter, board, board_config, board_watch, layout, model, paths, protocol, queue, storage,
};
pub use dock_git::files;
pub use dock_pty::{clipboard, runtime, terminal};
pub use dock_ui::{attention, copy, dashboard, keymap, picker, theme, verdict};
```

- [ ] **Step 5: Verify, including the install story**

Run: `cargo test --workspace` → **836 passed + 12 ignored**.
Run: `cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`. Read both.

Then confirm the published identity survived — this is the constraint that breaks the README if missed:

```bash
cargo build --release
ls -la target/release/dock target/release/dockd
```

Both binaries must exist.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: extract dock-daemon; dock-tui is now just the binaries

What remains in the root crate is main.rs and src/cli/. Every other
module lives in a workspace crate and is re-exported under its old name,
so no call site outside the moved files changed."
```

---

### Task 7: Make the exec surface a lint, not a convention

This is the point of the whole plan. Spec section 7: *"`dock-receipt` is the only crate that executes argv Dock did not write, and `dock-model` and `dock-ui` may not spawn at all."* Until this task, that is prose. After it, it fails the build.

`dock-receipt` does not exist yet — it lands in the next plan. What this task installs is the half that can be checked today: **`dock-model`, `dock-ui` and `dock-testing` may not spawn a process at all.**

**Files:**
- Create: `clippy.toml` (workspace root)
- Modify (deny): `crates/dock-model/src/lib.rs`, `crates/dock-ui/src/lib.rs`, `crates/dock-testing/src/lib.rs`
- Modify (opt out, with a stated reason each): `crates/dock-git/src/lib.rs`, `crates/dock-pty/src/lib.rs`, `crates/dock-daemon/src/lib.rs`, `src/lib.rs`, `src/main.rs`

**Interfaces:**
- Consumes: the crates from Tasks 1–6.
- Produces: a build that fails if a drawing or data crate learns to spawn.

- [ ] **Step 1: Write the failing test — by breaking it deliberately**

Before installing the lint, prove it will bite. Add to `crates/dock-ui/src/theme.rs`, temporarily:

```rust
pub fn temporary_probe() {
    let _ = std::process::Command::new("echo");
}
```

- [ ] **Step 2: Install the lint**

The mechanism is the opposite of what an earlier draft of this plan described, and the real
one is better. `clippy::disallowed_methods` is **warn-by-default the moment `clippy.toml`
populates its list** — verified on clippy 0.1.93 / rustc 1.93.1. So the workspace default is
*no crate may spawn a process*, and a crate that legitimately does must opt OUT explicitly.

That is a stronger posture than the opt-in deny first planned: a crate added to this
workspace next year is enforced by default rather than silently exempt, and every exception
is a greppable line carrying its own justification.

Create `clippy.toml` at the workspace root:

```toml
disallowed-methods = [
  { path = "std::process::Command::new", reason = "this crate may not spawn a process; see spec section 7" },
]
```

Add to the top of `crates/dock-model/src/lib.rs`, `crates/dock-ui/src/lib.rs` and
`crates/dock-testing/src/lib.rs` — after the `//!` doc comment, before any `use`:

```rust
// A crate that only holds shapes, or only draws, has no business starting a process. The
// workspace already warns; this makes it an error even when clippy runs without -D warnings.
#![deny(clippy::disallowed_methods)]
```

Then add the four opt-outs, each stating what it spawns and who wrote the argv. At the top of
`crates/dock-git/src/lib.rs`:

```rust
// Spawns `git` and `delta`, both with argv fixed at compile time. Dock wrote every argument.
#![allow(clippy::disallowed_methods)]
```

`crates/dock-pty/src/lib.rs`:

```rust
// Spawns `$SHELL`, agent executables named by a manifest, and clipboard helpers — into PTYs
// and process groups Dock owns. Dock wrote the argv; the agent binary is named, not composed.
#![allow(clippy::disallowed_methods)]
```

`crates/dock-daemon/src/lib.rs`:

```rust
// Spawns `git` and `ps` with argv fixed at compile time.
#![allow(clippy::disallowed_methods)]
```

`src/lib.rs` and `src/main.rs` in the root crate (`main.rs` needs its own, being a separate
crate root):

```rust
// The binaries: spawn `dockd`, and `cargo` when rebuilding a sibling daemon in a dev tree.
#![allow(clippy::disallowed_methods)]
```

**When `dock-receipt` lands in the next plan it gets no opt-out and no deny** — it will carry
a narrower, per-call allowance at the single site that runs a declared check, so that the one
place executing argv Dock did not write is the one place a reader has to look.

- [ ] **Step 3: Confirm it fires**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: FAIL, naming `temporary_probe` in `crates/dock-ui/src/theme.rs` and citing the reason string — and failing **only** there. If it also fails inside `dock-git`, `dock-pty`, `dock-daemon` or the root crate, an opt-out from Step 2 is missing.

Quote the real failure in your report. A lint nobody has watched fail is not known to work.

- [ ] **Step 4: Remove the probe and confirm green**

Delete `temporary_probe` entirely. Then:

Run: `cargo clippy --workspace --all-targets -- -D warnings` — read it; expect no warnings.
Run: `cargo test --workspace` → **836 passed + 12 ignored**.
Run: `cargo fmt --check` — no output.

Confirm the probe is gone: `grep -rn "temporary_probe" crates/ src/` — expect no output.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: dock-model, dock-ui and dock-testing may not spawn a process

The checkable half of the spec's exec-surface rule, installed as a
clippy deny rather than a convention. dock-receipt lands next and will
be the only crate permitted to run argv Dock did not author; these
three may not run any.

Watched it fail before trusting it: a probe calling Command::new in
dock-ui failed the build with the reason string, then was removed."
```

---

### Task 8: Prove nothing was lost, and record the baseline

Seven tasks moved roughly twenty modules between eight crates. The claim is that behaviour is unchanged and no coverage was silently dropped. That is worth checking rather than asserting.

**Files:** none. This task produces a commit message.

- [ ] **Step 1: The count, and where it lives**

Run: `cargo test --workspace 2>&1 | grep "^test result:"`

Sum the `passed` and `ignored` columns across every line. Expected: **836 passed, 0 failed, 12 ignored**. Same totals as before the plan, redistributed across more targets.

If the total is short, a test module was left behind by a move. That compiles cleanly and silently deletes coverage, so a short count is a failure, not a rounding difference. Report it; do not adjust.

- [ ] **Step 2: The measurement harnesses still run**

The fourteen `#[ignore]`d measurements are the regression harness. Grep for them with `#\[ignore` — **not** `#\[ignore\]`, which misses the `#[ignore = "reason"]` form most of them use:

```bash
grep -rc "#\[ignore" crates/ src/ --include='*.rs' | grep -v ":0"
```

Then run two of them to prove they survived relocation:

```bash
cargo test --release --workspace -- --ignored --nocapture render_measurement
cargo test --release --workspace -- --ignored --nocapture --test-threads=1 measure_
```

- [ ] **Step 3: The install story**

```bash
cargo build --release --locked
ls -la target/release/dock target/release/dockd
```

Both binaries must exist. The root package must still be `dock-tui` with lib name `dock`:

```bash
grep -A2 "^\[package\]" Cargo.toml | head -3
grep -A2 "^\[lib\]" Cargo.toml
```

- [ ] **Step 4: History survived the moves**

```bash
git log --follow --oneline -3 -- crates/dock-ui/src/dashboard.rs
git log --follow --oneline -3 -- crates/dock-model/src/queue.rs
```

Each must show commits from before this plan. If either shows only the move commit, `git mv` was not used and the history of a large file is now unreachable.

- [ ] **Step 5: Record the baseline**

```bash
git commit --allow-empty -F - <<'MSG'
refactor: one crate became eight, with nothing lost

836 tests green and 12 ignored — the same totals as before the split,
redistributed across more targets. Both binaries build. git log --follow
still reaches through the moves.

Render measurement after the split, fastest of the runs:
  <paste the figures here>

The point was not tidiness. dock-model, dock-ui and dock-testing now
fail the build if they learn to spawn a process, which is the half of
the exec-surface rule that can be checked before dock-receipt exists.
MSG
```

---

## Self-Review

**Spec coverage.** Section 7 names eight library crates and a dependency order; Tasks 1–6 create all of them except `dock-receipt`, which section 9 places in row 2, not row 1. Section 7's lint requirement is Task 7. Section 8's "`dashboard.rs` moves whole" is a Global Constraint and is verified in Task 5 Step 5. Section 7's three module-graph constraints — `board ↔ board_config`, `protocol → queue`, and the test-only `detect → terminal` — each dictate a task boundary and are cited where they do. The `testing.rs` prerequisite is Task 1.

**Placeholder scan.** Every step carries its real command or code. The only deliberate blank is the render figures pasted into Task 8's commit message, which cannot be known before the run.

**A defect found and fixed inline.** Task 2 originally moved `runtime.rs` into `dock-pty` alongside `terminal/`. It cannot: `runtime.rs:728` calls `board::resolve_tasks_dir`, and `dock-model` does not exist until Task 4. Task 2 now moves only `src/terminal/` and carries an explicit correction; `runtime.rs` moves in Task 4 Step 5, once it has somewhere to point.

**Type and path consistency.** The re-export shims are the load-bearing mechanism, and they compose: each task adds its own line to `src/lib.rs`, and Task 6 Step 4 states the final file in full so the accumulated shims can be checked against one authoritative version rather than reconstructed from six diffs.

**A known risk this plan does not eliminate.** Task 5 asserts `dock-ui` does not need `dock-pty`, on the grounds that `tui_term` renders from a screen rather than a PTY. That was inferred from imports, not proven by building. Step 3 makes the failure explicit — if the `crate::terminal::` substitution fires, the assumption was wrong — and instructs the implementer to add the dependency and say so rather than work around it.
