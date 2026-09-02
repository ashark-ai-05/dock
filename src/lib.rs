//! `dock-tui` is now a thin crate: the two binaries and their command surface.
//!
//! Every module below lives in a workspace crate and is re-exported here under the name it
//! had when it was a module, so `crate::protocol::…` and the rest keep resolving in
//! `src/main.rs` and `src/cli/`.
// Re-exports and the CLI verb surface: no process is started here. `src/main.rs` is a
// separate crate root and carries its own exemption.
#![deny(clippy::disallowed_methods)]
pub mod cli;

pub use dock_daemon::{client, dispatch, hook, server};
pub use dock_detect as detect;
pub use dock_git as git;
pub use dock_model::{adapter, board, board_watch, layout, model, paths, protocol, queue, storage};
pub use dock_pty::terminal;
pub use dock_ui::dashboard;
