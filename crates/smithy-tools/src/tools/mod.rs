//! The core tool set.
//!
//! Carried over from coda, which arrived at these eight through measurement
//! rather than guesswork, with two changes:
//!
//! - `edit` now runs the fuzzy cascade from [`crate::fuzzy`] instead of
//!   requiring a byte-exact `old_string`.
//! - `grep` no longer shells out to ripgrep. coda listed ripgrep as its one
//!   runtime dependency and preflighted for it; walking with the `ignore` crate
//!   (which *is* ripgrep's walker) and matching with `regex` gives the same
//!   gitignore-aware behaviour with nothing to install.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod read;
pub mod todo;
pub mod write;
