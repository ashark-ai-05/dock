//! The non-interactive face of `dock`: one module per verb.
//!
//! Each verb is a pure `parse_arguments` and a `render`, joined by a four-line `run`. Splitting
//! them is what makes a verb testable at all — parsing needs no daemon, and neither does
//! deciding what to print.

pub mod wire;
