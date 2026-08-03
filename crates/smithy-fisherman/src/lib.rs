//! The fisherman on the forged rail.
//!
//! Routine, poses, and drawing — no floem, no editor. The UI crate keeps only
//! the `PaintCx` glue and the aesthetic gate.

pub mod fisherman;
pub mod ink;
pub mod routine;

pub use fisherman::*;
pub use ink::Ink;
pub use routine::{Block, Doing, Place};
