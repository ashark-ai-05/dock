//! Real PTYs: the terminal emulator, key encoding, and the process groups Dock owns.
// Spawns `$SHELL`, agent executables named by a manifest, and clipboard helpers — into PTYs
// and process groups Dock owns. Dock wrote the argv; the agent binary is named, not composed.
#![allow(clippy::disallowed_methods)]
pub mod clipboard;
pub mod runtime;
pub mod terminal;
