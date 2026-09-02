//! Everything Dock draws: the palette, the widgets, and the dashboard.
//! Nothing here spawns a process; Task 7 makes that a build failure rather than a promise.
// A crate that only holds shapes, or only draws, has no business starting a process. The
// workspace already warns; this makes it an error even when clippy runs without -D warnings.
#![deny(clippy::disallowed_methods)]
pub mod attention;
pub mod copy;
pub mod dashboard;
pub mod keymap;
pub mod picker;
pub mod theme;
pub mod verdict;
