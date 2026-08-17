//! smithy-tools — the single tool layer for Smithy.
//!
//! This crate consolidates four previously separate implementations:
//!
//! - **coda** contributed the [`Tool`]/[`Registry`] shape and the rule that
//!   [`Registry::execute`] is the *only* dispatch path, so guardrails, approval
//!   prompts and logging have exactly one place to live. Its always-on tool set
//!   (`read`/`write`/`edit`/`ls`/`glob`/`grep`/`bash`/`todo`) is carried over
//!   intact, including the byte-stable schema serialization that keeps the
//!   model's KV prefix cache warm.
//! - **forge** contributed the richer [`ToolDefinition`]/[`ToolParameter`]
//!   schema modeling and the OpenAI `tools` array serialization.
//! - **divcli** contributed the `cap-std` capability sandbox, which replaces
//!   coda's lexical path guard with a real `Dir`-rooted confinement that also
//!   holds against symlink escapes.
//! - **rustcoder** contributed the fuzzy edit cascade in [`fuzzy`], which turns
//!   the single most likely real-world failure — the model failing to reproduce
//!   `old_string` byte-exactly — from a hard error into a recoverable one.
//!
//! The [`ToolHook`] trait is the seam coda designed `execute` around and that
//! divcli sketched (but left empty): it is where write review, shell approval,
//! and security scanning attach without any tool knowing about them.

pub mod fuzzy;
pub mod registry;
pub mod sandbox;
pub mod schema;
pub mod tools;

pub use registry::{AllowBash, GatePause, HookDecision, Registry, Todo, Tool, ToolCtx, ToolHook};
pub use sandbox::{check_bash, command_leaves_project, Workspace};
pub use schema::{ToolCall, ToolDefinition, ToolParameter, ToolResult};
