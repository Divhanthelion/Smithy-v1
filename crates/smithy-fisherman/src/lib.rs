//! The fisherman on the forged rail.
//!
//! Routine, poses, and drawing — no floem, no editor. The UI crate keeps only
//! the `PaintCx` glue and the aesthetic gate.

pub mod fisherman;
pub mod ink;
pub mod routine;

/// Headless checks and contact sheets. Behind the `harness` feature so the
/// default build stays free of tiny-skia / serde_json.
#[cfg(feature = "harness")]
pub mod harness;

pub use fisherman::*;
pub use ink::{Ink, Part};
pub use routine::{Block, Doing, Place};
