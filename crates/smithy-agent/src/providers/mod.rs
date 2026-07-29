//! Backend implementations of [`crate::Provider`].
//!
//! LM Studio is the only one, on purpose. The four source projects between them
//! carried five provider implementations (Anthropic, Gemini, OpenAI, OpenRouter,
//! LM Studio) of which one was ever used. Carrying the other four means
//! maintaining wire formats nobody exercises. The [`crate::Provider`] trait is
//! the seam if that changes.

pub mod lmstudio;

pub use lmstudio::{LmStudio, ModelInfo};
