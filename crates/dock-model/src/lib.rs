//! Dock's durable shapes: the wire protocol, the board, the queue, and what is stored on disk.
//!
//! `board` and `board_config` reference each other, and `protocol` references `queue`. Those
//! cycles are why these eight modules are one crate rather than several: legal within a
//! crate, impossible across one.
//!
//! `adapter` and `layout` also live here, though they belong to no cycle of their own:
//! `protocol` uses `AdapterId`/`AdapterCapabilities`/`LayoutSnapshot` and the rest directly in
//! its wire types, and neither `adapter` nor `layout` depends on anything else in this crate or
//! in the root crate. Leaving them in the root crate would make the root crate a dependency of
//! `dock-model` (for these two modules) while `dock-model` is also a dependency of the root
//! crate (for `protocol` and friends) — an illegal cycle. Moving them here is the same
//! resolution the plan already applied to `dock-ui`'s `clipboard`/`files` split: place a module
//! where its real dependency need puts it, not where its original file grouping suggested.
// A crate that only holds shapes, or only draws, has no business starting a process. The
// workspace already warns; this makes it an error even when clippy runs without -D warnings.
#![deny(clippy::disallowed_methods)]
pub mod adapter;
pub mod board;
pub mod board_config;
pub mod board_watch;
pub mod env;
pub mod layout;
pub mod model;
pub mod paths;
pub mod protocol;
pub mod queue;
pub mod receipt;
pub mod storage;
