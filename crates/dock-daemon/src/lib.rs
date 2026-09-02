//! The daemon: the socket server, dispatch, and the hooks agents report through.
//!
//! `adapter` and `layout` live in `dock-model`, not here — `protocol` imports from both, and
//! `protocol` is `dock-model`.
// Spawns `git` and `ps` with argv fixed at compile time, plus `sleep` and `git` as fixture
// processes in tests.
#![allow(clippy::disallowed_methods)]
pub mod client;
pub mod discovery;
pub mod dispatch;
pub mod hook;
pub mod server;
