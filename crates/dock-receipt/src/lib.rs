//! Declared checks: reading them, running them, and deciding what they add up to.
//!
//! Dock may execute a command **declared by the repository or the user** — never one an agent
//! composed — in the run's bound worktree, at a pinned SHA, under a cleared and allowlisted
//! environment. An agent names a check; the name is looked up in `.dock/checks.toml` and an
//! unknown one is recorded `unwitnessed` rather than run. Dock still never stages, commits,
//! rebases, merges, pushes, or removes a worktree.
// The exec surface. Four crates opt out of the workspace's default
// `deny(clippy::disallowed_methods)`, each with a comment naming who wrote its argv:
// `dock-git`, `dock-daemon`, `dock-pty`, and the root binary all spawn commands Dock itself
// composed. This crate is the only one of the four whose argv comes from a file Dock did not
// write — a repository's or a user's `checks.toml` — which is exactly why a reviewer auditing
// untrusted argv has only this one place to look. See spec section 7.
#![allow(clippy::disallowed_methods)]

pub mod declaration;
pub mod rules;
pub mod runner;
