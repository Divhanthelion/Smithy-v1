//! `symbol` — ask where something is defined, instead of searching for it.
//!
//! ## Why a tool and not more prompt
//!
//! The project context block in [`smithy_project::context`] is prefilled on
//! *every* request of the session, so everything in it is paid for repeatedly.
//! That is the right place for a map — crate layout, dependencies, module paths
//! — and the wrong place for detail. A full index of a mid-sized workspace runs
//! to hundreds of symbols; putting it in the preamble would cost more than it
//! saves and crowd out the conversation.
//!
//! So the index lives in memory and is *queried*. A lookup is one hash, not a
//! walk of the tree, and only the handful of lines that answer the question
//! enter the history.
//!
//! ## The failure it exists to prevent
//!
//! A measured session was asked to implement a plan against a Yew codebase. The
//! map told it `DesktopMsg` existed but not what was in it, so it wrote
//! `DesktopMsg::PluginsChanged` — a variant that did not exist. It called
//! `restore_session` with two arguments; the method took one, and being neither
//! `pub` nor top-level it appeared nowhere in the map. Four of the seven
//! resulting build errors were that one shape: **a name it could see existed,
//! whose shape it could not.**
//!
//! `grep` could have answered all of them. The point is not that the information
//! was unreachable — it is that finding it cost several calls and a guess was
//! cheaper, so the model guessed. One call that answers exactly is what changes
//! that trade.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smithy_project::symbols::{SymbolIndex, SymbolKind};
use smithy_tools::registry::{Tool, ToolCtx};
use smithy_tools::schema::{arg_str, ToolDefinition, ToolOutput, ToolParameter};

/// How many substring matches a fallback search returns.
const SEARCH_LIMIT: usize = 15;

/// How many definitions of one name are shown before the list is cut.
///
/// A name like `new` has dozens. Showing all of them is worse than showing the
/// first few and saying how many there are.
const MAX_DEFINITIONS: usize = 12;

pub struct SymbolLookup {
    index: Arc<SymbolIndex>,
}

impl SymbolLookup {
    pub fn new(index: Arc<SymbolIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for SymbolLookup {
    fn name(&self) -> &'static str {
        "symbol"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "symbol",
            "Look up where a Rust symbol is defined and what its shape is: file, line, and \
             signature. Covers structs, enums, traits, functions, methods inside `impl` blocks, \
             type aliases, consts and modules — public and private alike.\n\n\
             Use it *before* referring to something you have not read. Asking for an enum returns \
             all of its variants; asking for a struct returns its methods with their full \
             signatures. That is the fastest way to avoid naming a variant that does not exist or \
             calling a method with the wrong number of arguments.\n\n\
             Prefer this over `grep` for \"where is X defined\" and \"what are X's variants/methods\" \
             — it is one exact lookup rather than a search of every file. Keep using `grep` for \
             what it is good at: finding *call sites*, matching text, and searching non-Rust files.\n\n\
             An unknown name returns near matches, so a guessed spelling still gets you somewhere.",
            vec![
                ToolParameter::string(
                    "name",
                    "The bare symbol name — `DesktopMsg`, `restore_session`. Not a path: pass \
                     `restore_session`, not `Desktop::restore_session`.",
                    true,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        let name = match arg_str(args, "name") {
            Ok(n) => n.trim(),
            Err(e) => return ToolOutput::err(e),
        };
        if name.is_empty() {
            return ToolOutput::err("the name is empty");
        }
        // Tolerate a path even though the description asks for a bare name: a
        // model that sends `Desktop::restore_session` has told us exactly what
        // it wants, and refusing on a formality costs a round trip.
        let name = name.rsplit("::").next().unwrap_or(name);

        if self.index.is_empty() {
            return ToolOutput::err(
                "the symbol index is empty — this project may have no Rust sources. Use `grep`."
                    .to_string(),
            );
        }

        let hits = self.index.lookup(name);
        if hits.is_empty() {
            return ToolOutput::ok(self.render_near_misses(name));
        }
        ToolOutput::ok(self.render_definitions(name, hits))
    }
}

impl SymbolLookup {
    fn render_definitions(
        &self,
        name: &str,
        hits: &[smithy_project::symbols::Symbol],
    ) -> String {
        let mut out = String::new();
        let shown = hits.len().min(MAX_DEFINITIONS);

        if hits.len() == 1 {
            out.push_str(&format!("`{name}` is defined once:\n\n"));
        } else {
            out.push_str(&format!(
                "`{name}` has {} definitions{}:\n\n",
                hits.len(),
                if hits.len() > shown {
                    format!(", showing {shown}")
                } else {
                    String::new()
                }
            ));
        }

        for symbol in hits.iter().take(shown) {
            out.push_str(&format!("- {}\n", symbol.render()));
            if let Some(container) = &symbol.container {
                out.push_str(&format!("  in `{container}` ({})\n", symbol.module));
            } else if !symbol.module.is_empty() {
                out.push_str(&format!("  module `{}`\n", symbol.module));
            }

            // The expansions that answer the follow-up question before it is
            // asked — and that would have prevented the errors in the module
            // docs.
            match symbol.kind {
                SymbolKind::Enum => {
                    let variants = self.index.variants_of(&symbol.name);
                    out.push_str(&format!("  {} variants:\n", variants.len()));
                    for v in &variants {
                        out.push_str(&format!("    {}\n", v.signature));
                    }
                    if variants.is_empty() {
                        out.push_str("    (none)\n");
                    }
                }
                SymbolKind::Struct | SymbolKind::Trait => {
                    let methods = self.index.methods_of(&symbol.name);
                    if methods.is_empty() {
                        out.push_str("  no methods found\n");
                    } else {
                        out.push_str(&format!("  {} methods:\n", methods.len()));
                        for m in methods.iter().take(30) {
                            out.push_str(&format!("    {}:{} {}\n", m.file, m.line, m.signature));
                        }
                        if methods.len() > 30 {
                            out.push_str(&format!("    … and {} more\n", methods.len() - 30));
                        }
                    }
                }
                _ => {}
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    /// What to say when the exact name is not there.
    ///
    /// Never an error: "no such symbol" *is* the answer to "does this exist",
    /// and it is the answer that would have stopped a model inventing a variant.
    /// Near matches are offered because the commonest reason for a miss is a
    /// remembered-but-wrong spelling.
    fn render_near_misses(&self, name: &str) -> String {
        let near = self.index.nearest(name, SEARCH_LIMIT);
        if near.is_empty() {
            return format!(
                "No symbol named `{name}` exists in this project, and nothing similar was found. \
                 It is not defined here — do not refer to it as though it were. ({} symbols \
                 indexed across {} files.)",
                self.index.len(),
                self.index.files()
            );
        }

        let mut out = format!(
            "No symbol is named exactly `{name}`. Similar names that do exist:\n\n"
        );
        for symbol in near {
            out.push_str(&format!("- {} — {}\n", symbol.name, symbol.render()));
        }
        out.push_str(
            "\nIf none of these is what you meant, `{name}` does not exist in this project.",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::Workspace;

    fn index_from(source: &str) -> Arc<SymbolIndex> {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("desktop.rs"), source).unwrap();
        Arc::new(SymbolIndex::build(tmp.path()))
    }

    async fn ask(index: Arc<SymbolIndex>, name: &str) -> ToolOutput {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Workspace::open(tmp.path()).unwrap());
        SymbolLookup::new(index)
            .run(&serde_json::json!({ "name": name }), &ctx)
            .await
    }

    const SOURCE: &str = "pub enum DesktopMsg {\n    CloseWindow(String),\n    DesktopClick,\n}\n\
         pub struct Desktop;\n\
         impl Desktop {\n    fn restore_session(&mut self, session: Session) {}\n}\n";

    /// The question that would have prevented four build errors.
    #[tokio::test]
    async fn asking_for_an_enum_returns_every_variant() {
        let out = ask(index_from(SOURCE), "DesktopMsg").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("CloseWindow(String)"), "{}", out.content);
        assert!(out.content.contains("DesktopClick"), "{}", out.content);
        assert!(out.content.contains("2 variants"), "{}", out.content);
    }

    /// The other two errors: a private method's arity.
    #[tokio::test]
    async fn asking_for_a_private_method_returns_its_signature_and_location() {
        let out = ask(index_from(SOURCE), "restore_session").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("session: Session"),
            "the arity must be visible: {}",
            out.content
        );
        assert!(out.content.contains("desktop.rs:"), "{}", out.content);
    }

    #[tokio::test]
    async fn asking_for_a_struct_lists_its_methods() {
        let out = ask(index_from(SOURCE), "Desktop").await;
        assert!(out.content.contains("restore_session"), "{}", out.content);
    }

    /// "It does not exist" is a useful answer, and must not read as a failure —
    /// an error invites a retry, and the model would go looking with `grep`.
    #[tokio::test]
    async fn a_name_that_does_not_exist_says_so_without_erroring() {
        let out = ask(index_from(SOURCE), "PluginsChanged").await;
        assert!(!out.is_error, "absence is an answer, not a failure");
        assert!(
            out.content.contains("does not exist") || out.content.contains("No symbol"),
            "{}",
            out.content
        );
    }

    /// A wrong-but-close spelling should still get somewhere.
    #[tokio::test]
    async fn a_near_miss_offers_the_real_names() {
        let out = ask(index_from(SOURCE), "DesktopMessage").await;
        assert!(out.content.contains("DesktopMsg"), "{}", out.content);
    }

    /// The description asks for a bare name; a path is what a model will
    /// sometimes send anyway, and refusing it costs a round trip.
    #[tokio::test]
    async fn a_qualified_path_is_accepted_rather_than_refused() {
        let out = ask(index_from(SOURCE), "Desktop::restore_session").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("session: Session"), "{}", out.content);
    }

    #[tokio::test]
    async fn an_empty_name_is_refused() {
        let out = ask(index_from(SOURCE), "   ").await;
        assert!(out.is_error);
    }

    /// A project with no Rust in it should say so and point elsewhere, rather
    /// than reporting every lookup as "not found".
    #[tokio::test]
    async fn an_empty_index_says_to_use_grep() {
        let out = ask(Arc::new(SymbolIndex::default()), "anything").await;
        assert!(out.is_error);
        assert!(out.content.contains("grep"), "{}", out.content);
    }

    /// The tool description carries the "prefer this over grep, for this" rule,
    /// which is the part that changes behaviour.
    #[test]
    fn the_description_says_when_to_use_it_and_when_not_to() {
        let tool = SymbolLookup::new(Arc::new(SymbolIndex::default()));
        let description = tool.definition().description;
        assert!(description.contains("grep"), "{description}");
        assert!(description.contains("variants"), "{description}");
        assert!(description.contains("call sites"), "{description}");
    }
}
