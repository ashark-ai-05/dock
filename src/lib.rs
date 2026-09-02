pub mod cli;
pub mod client;
// The extracted crates keep their old paths so every `crate::detect::…` and
// `crate::terminal::…` call site in this crate resolves unchanged.
pub use dock_detect as detect;
pub use dock_pty::{clipboard, runtime, terminal};
pub mod discovery;
pub mod dispatch;
pub use dock_git as git;
pub use dock_git::files;
pub mod hook;
// Re-exported individually rather than as one `dock_model` module, so that every existing
// `crate::protocol::…` and `crate::board::…` path in this crate resolves unchanged.
//
// `adapter` and `layout` also live in `dock-model` now, not because they belong to either of
// its two internal cycles, but because `protocol` uses their types directly and `dock-model`
// cannot depend on this root crate (this crate already depends on `dock-model`) — see the
// doc comment on `dock_model::lib` for the full reasoning.
pub use dock_model::{
    adapter, board, board_config, board_watch, layout, model, paths, protocol, queue, storage,
};
pub use dock_ui::{attention, copy, dashboard, keymap, picker, theme, verdict};
pub mod server;
