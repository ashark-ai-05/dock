//! Real PTYs: the terminal emulator, key encoding, and the process groups Dock owns.
// Spawns `/bin/sh` on every pane launch — a guardian wrapper that then `exec`s the real
// target, so `$SHELL` and the agent binary are exec targets inside it rather than arguments
// to `Command::new` here. Also spawns clipboard helpers, and `ps` when checking whether an
// owned process group still has a live member. Everything lands in a PTY and process group
// Dock owns, and Dock wrote every argv; an agent binary is named, never composed. Tests
// additionally spawn `env`, `sleep` and `sh` as fixtures.
#![allow(clippy::disallowed_methods)]
pub mod clipboard;
pub mod runtime;
pub mod terminal;
