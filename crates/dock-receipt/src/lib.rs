//! Declared checks: reading them, running them, and deciding what they add up to.
//!
//! Dock may execute a command **declared by the repository or the user** — never one an agent
//! composed — in the run's bound worktree, at a pinned SHA, under a cleared and allowlisted
//! environment. An agent names a check; the name is looked up in `.dock/checks.toml` and an
//! unknown one is recorded `unwitnessed` rather than run. Dock still never stages, commits,
//! rebases, merges, pushes, or removes a worktree.
// The exec surface. This is the one crate whose argv comes from a file Dock did not write, which
// is exactly why the rest of the workspace denies `Command::new` and this crate is the only
// place a reviewer has to look. See spec section 7.
#![allow(clippy::disallowed_methods)]

pub mod declaration;
pub mod rules;
pub mod runner;
