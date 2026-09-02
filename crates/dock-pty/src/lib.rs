//! Real PTYs: the terminal emulator, key encoding, and the process groups Dock owns.
// Spawns `$SHELL`, agent executables named by a manifest, clipboard helpers, and `ps` when
// checking whether an owned process group still has a live member — all into PTYs and
// process groups Dock owns. Dock wrote the argv; an agent binary is named, never composed,
// plus `env`, `sleep` and `sh` as fixture processes in tests.
#![allow(clippy::disallowed_methods)]
pub mod clipboard;
pub mod runtime;
pub mod terminal;
