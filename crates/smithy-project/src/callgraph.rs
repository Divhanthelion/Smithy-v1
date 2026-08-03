//! The call graph: who calls whom, resolved by the compiler.
//!
//! ## Where each half comes from
//!
//! Neither source can produce this alone.
//!
//! - **[`crate::scip`]** — rust-analyzer's batch index. It knows that an
//!   occurrence of `foo` at `desktop.rs:112` refers to *this* `foo` and not one
//!   of the four others with the same name. That resolution is the whole reason
//!   this is worth building: matching calls by name alone was measured at 55%
//!   unambiguous on this workspace, failing hardest on `new`, `default` and
//!   `run` — the most-called names there are.
//! - **[`crate::symbols`]** — tree-sitter spans. SCIP does *not* say which
//!   function a reference sits inside; its `enclosing_range` field exists for
//!   exactly that and rust-analyzer does not emit it. Without a caller there is
//!   no edge, only a scatter of references.
//!
//! The two also cover each other's gaps. rust-analyzer never sets the `Import`
//! role, so `use` statements cannot be filtered out by role — but a `use` line
//! is outside every function, so enclosure discards it anyway.
//!
//! ## What is counted rather than hidden
//!
//! [`BuildStats`] reports every reference that did *not* become an edge, and
//! why. A graph that silently drops what it could not resolve is a graph that
//! cannot be used to check anything, which would defeat the point.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::scip::ScipIndex;
use crate::symbols::SymbolIndex;

/// A function in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    /// The `impl` type or trait, when this is a method.
    pub container: Option<String>,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
}

impl Node {
    pub fn qualified(&self) -> String {
        match &self.container {
            Some(container) => format!("{container}::{}", self.name),
            None => self.name.clone(),
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

/// One caller → callee relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// Index into [`CallGraph::nodes`] — not a name. SCIP monikers run to ~60
    /// bytes and would dominate the persisted file.
    pub from: u32,
    pub to: u32,
    /// How many call sites. Drawn as line weight.
    pub sites: u32,
}

/// What did and did not become an edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildStats {
    pub occurrences: usize,
    pub definitions: usize,
    pub references: usize,
    /// References whose target is defined in this workspace *and* which sit
    /// inside a known function.
    pub edges_kept: usize,
    /// Inside no function — a `use` line, a `const` initialiser, a struct field
    /// type. These have no caller and are correctly not edges.
    pub unattributed: usize,
    /// Target defined outside the workspace: `std`, `yew`, any dependency.
    /// Keeping them would swamp the graph with nodes you cannot navigate to.
    pub external: usize,
    /// Recursion. Kept, but worth knowing.
    pub self_edges: usize,
    /// *References* to SCIP `local N` symbols — local variables, bindings and
    /// closures. Excluded, for two reasons.
    ///
    /// Counts references only, so `edges_kept + external + locals +
    /// unattributed` accounts for every reference exactly once.
    ///
    /// They are not functions, so they do not belong in a graph of calls. And
    /// they are **document-scoped**: `local 0` in one file and `local 0` in
    /// another are different things. A real index had 4,842 such occurrences
    /// sharing only 226 distinct strings across 24 files, so keying them
    /// globally merged unrelated symbols — which produced an edge claiming
    /// `restore_session` called `FileSystem::rename` thirty-eight times, and
    /// 449 spurious self-edges from functions "calling" their own locals.
    pub locals: usize,
}

/// A resolved call graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub stats: BuildStats,
}

impl CallGraph {
    /// Everything `node` calls.
    pub fn callees(&self, node: u32) -> Vec<(&Node, u32)> {
        self.edges
            .iter()
            .filter(|e| e.from == node)
            .filter_map(|e| self.nodes.get(e.to as usize).map(|n| (n, e.sites)))
            .collect()
    }

    /// Everything that calls `node`.
    pub fn callers(&self, node: u32) -> Vec<(&Node, u32)> {
        self.edges
            .iter()
            .filter(|e| e.to == node)
            .filter_map(|e| self.nodes.get(e.from as usize).map(|n| (n, e.sites)))
            .collect()
    }

    /// Nodes whose bare name matches, for looking one up by hand.
    pub fn find(&self, name: &str) -> Vec<u32> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.name == name)
            .map(|(i, _)| i as u32)
            .collect()
    }

    /// Join a parsed SCIP index to tree-sitter spans.
    ///
    /// Pure — no filesystem, no subprocess — so the joining logic is testable
    /// without running rust-analyzer.
    pub fn assemble(scip: &ScipIndex, symbols: &SymbolIndex) -> CallGraph {
        let mut graph = CallGraph::default();
        // SCIP symbol → node index. Built from *definitions*, which is what
        // makes the node set compiler-resolved rather than name-matched.
        let mut by_symbol: HashMap<&str, u32> = HashMap::new();
        // (file, span start) → node index, so a caller resolves to the same node
        // its definition created.
        let mut by_span: HashMap<(&str, usize), u32> = HashMap::new();

        // Pass one: definitions become nodes, but only those that *are* a
        // function. A definition's own name token sits inside its own span, so
        // the innermost span containing it is the function itself.
        for document in &scip.documents {
            let file = document.relative_path.as_str();
            for occurrence in &document.occurrences {
                graph.stats.occurrences += 1;
                if !occurrence.is_definition() {
                    continue;
                }
                graph.stats.definitions += 1;

                // Counted under `definitions` already; `locals` is reserved for
                // the reference accounting below, which must sum to 100%.
                if is_local(&occurrence.symbol) {
                    continue;
                }

                let Some(span) = symbols.enclosing(file, occurrence.line) else {
                    // A struct, an enum, a top-level const: defined outside any
                    // function, so not a node in a *call* graph.
                    continue;
                };
                let key = (file, span.start_line);
                let id = *by_span.entry(key).or_insert_with(|| {
                    graph.nodes.push(Node {
                        name: span.name.clone(),
                        container: span.container.clone(),
                        file: file.to_string(),
                        line: span.start_line,
                        end_line: span.end_line,
                    });
                    (graph.nodes.len() - 1) as u32
                });
                by_symbol.insert(occurrence.symbol.as_str(), id);
            }
        }

        // Pass two: every non-definition reference is a candidate edge.
        let mut tally: HashMap<(u32, u32), u32> = HashMap::new();
        for document in &scip.documents {
            let file = document.relative_path.as_str();
            for occurrence in &document.occurrences {
                if occurrence.is_definition() {
                    continue;
                }
                graph.stats.references += 1;

                if is_local(&occurrence.symbol) {
                    graph.stats.locals += 1;
                    continue;
                }

                let Some(&to) = by_symbol.get(occurrence.symbol.as_str()) else {
                    // Defined elsewhere — `std`, a dependency, or a non-function.
                    graph.stats.external += 1;
                    continue;
                };
                let Some(span) = symbols.enclosing(file, occurrence.line) else {
                    graph.stats.unattributed += 1;
                    continue;
                };
                let Some(&from) = by_span.get(&(file, span.start_line)) else {
                    // The enclosing function has no definition occurrence of its
                    // own — it exists in the source but SCIP never named it.
                    graph.stats.unattributed += 1;
                    continue;
                };

                if from == to {
                    graph.stats.self_edges += 1;
                }
                *tally.entry((from, to)).or_default() += 1;
                graph.stats.edges_kept += 1;
            }
        }

        graph.edges = tally
            .into_iter()
            .map(|((from, to), sites)| Edge { from, to, sites })
            .collect();
        // Deterministic order, so a persisted graph round-trips byte-identically
        // and two runs over an unchanged tree produce the same file.
        graph.edges.sort_by_key(|e| (e.from, e.to));
        graph
    }

    /// Run `rust-analyzer scip`, then assemble.
    ///
    /// **Blocking and expensive**: measured at ~10 s and 2.3 GB peak on a
    /// 109-crate project. The process exits when it is done, which is the whole
    /// reason this is worth persisting — see [`CallGraph::assemble`].
    pub fn build(root: &Path) -> Result<CallGraph, String> {
        let output_dir = std::env::temp_dir().join("smithy-scip");
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| format!("cannot create {}: {e}", output_dir.display()))?;
        let scip_path = output_dir.join(format!(
            "{}.scip",
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "index".to_string())
        ));

        let output = std::process::Command::new("rust-analyzer")
            .arg("scip")
            .arg(".")
            .arg("--output")
            .arg(&scip_path)
            .current_dir(root)
            .output()
            .map_err(|e| {
                format!("could not run `rust-analyzer scip`: {e}. Is rust-analyzer on PATH?")
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "`rust-analyzer scip` failed: {}",
                stderr.lines().last().unwrap_or("no output")
            ));
        }

        let scip = ScipIndex::from_file(&scip_path)?;
        let symbols = SymbolIndex::build(root);
        Ok(CallGraph::assemble(&scip, &symbols))
    }
}

/// Whether a SCIP symbol is a document-scoped local.
///
/// The spec reserves the `local <id>` form for symbols with no global identity —
/// variables, bindings, closures. They are not callable functions, and the same
/// string means different things in different files, so they are excluded rather
/// than merged.
fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scip::{Document, Occurrence};

    fn occ(symbol: &str, line: usize, definition: bool) -> Occurrence {
        Occurrence {
            symbol: symbol.to_string(),
            line,
            roles: if definition { 0x1 } else { 0 },
        }
    }

    /// Build a symbol index over one synthetic file.
    fn symbols_for(source: &str) -> SymbolIndex {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("m.rs"), source).unwrap();
        SymbolIndex::build(tmp.path())
    }

    const SOURCE: &str = "\
fn caller() {
    callee();
}
fn callee() {
}
";

    #[test]
    fn a_reference_inside_a_function_becomes_an_edge() {
        let symbols = symbols_for(SOURCE);
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 1, true),
                    occ("c/callee().", 4, true),
                    occ("c/callee().", 2, false), // the call site
                ],
            }],
        };

        let graph = CallGraph::assemble(&scip, &symbols);
        assert_eq!(graph.nodes.len(), 2, "{:#?}", graph.nodes);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.stats.edges_kept, 1);

        let caller = graph.find("caller")[0];
        let callees = graph.callees(caller);
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].0.name, "callee");
        assert_eq!(callees[0].1, 1);

        let callee = graph.find("callee")[0];
        assert_eq!(graph.callers(callee)[0].0.name, "caller");
    }

    /// A `use` line is outside every function. rust-analyzer never sets the
    /// `Import` role, so enclosure is the only thing keeping these out — and it
    /// must.
    #[test]
    fn a_reference_outside_any_function_is_counted_not_edged() {
        let symbols = symbols_for("use crate::callee;\nfn caller() {\n}\nfn callee() {\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 2, true),
                    occ("c/callee().", 4, true),
                    occ("c/callee().", 1, false), // the `use` line
                ],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.stats.unattributed, 1);
    }

    /// Calls into `std` or a dependency must not become nodes — they would
    /// swamp the graph with things you cannot navigate to.
    #[test]
    fn a_call_to_something_defined_elsewhere_is_counted_as_external() {
        let symbols = symbols_for(SOURCE);
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 1, true),
                    occ("rust-analyzer cargo std 1.0 vec/Vec#push().", 2, false),
                ],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.stats.external, 1);
        assert_eq!(graph.stats.unattributed, 0, "external is not unattributed");
    }

    /// Two calls to the same function from one caller are one edge of weight
    /// two, not two edges — the graph draws a thicker line, not a double arrow.
    #[test]
    fn repeated_calls_thicken_one_edge() {
        let symbols = symbols_for("fn caller() {\n    callee();\n    callee();\n}\nfn callee() {\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 1, true),
                    occ("c/callee().", 5, true),
                    occ("c/callee().", 2, false),
                    occ("c/callee().", 3, false),
                ],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].sites, 2);
        assert_eq!(graph.stats.edges_kept, 2, "both sites are counted");
    }

    #[test]
    fn recursion_is_kept_and_counted() {
        let symbols = symbols_for("fn f() {\n    f();\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![occ("c/f().", 1, true), occ("c/f().", 2, false)],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, graph.edges[0].to);
        assert_eq!(graph.stats.self_edges, 1);
    }

    /// A method's node carries its type, so `create` on two types are two nodes
    /// rather than one ambiguous blob.
    #[test]
    fn methods_carry_their_container() {
        let symbols =
            symbols_for("impl Desktop {\n    fn create() {\n        helper();\n    }\n}\nfn helper() {\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/Desktop#create().", 2, true),
                    occ("c/helper().", 6, true),
                    occ("c/helper().", 3, false),
                ],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        let create = &graph.nodes[graph.find("create")[0] as usize];
        assert_eq!(create.container.as_deref(), Some("Desktop"));
        assert_eq!(create.qualified(), "Desktop::create");
    }

    /// A definition that is not a function — a struct, an enum — must not
    /// become a node in a graph of calls.
    #[test]
    fn non_function_definitions_are_not_nodes() {
        let symbols = symbols_for("pub struct S;\npub enum E { A }\nfn f() {\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/S#", 1, true),
                    occ("c/E#", 2, true),
                    occ("c/f().", 3, true),
                ],
            }],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["f"]);
        assert_eq!(graph.stats.definitions, 3, "all three are still counted");
    }

    /// Two runs over unchanged input must produce the same bytes, or a
    /// persisted graph churns on every rebuild.
    #[test]
    fn assembly_is_deterministic() {
        let symbols = symbols_for(SOURCE);
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 1, true),
                    occ("c/callee().", 4, true),
                    occ("c/callee().", 2, false),
                ],
            }],
        };
        let a = CallGraph::assemble(&scip, &symbols);
        let b = CallGraph::assemble(&scip, &symbols);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    /// Every reference is accounted for exactly once. A graph that quietly lost
    /// references would be one you could not check anything against — which is
    /// the entire purpose — so the four buckets must sum to the total.
    #[test]
    fn every_reference_lands_in_exactly_one_bucket() {
        let symbols = symbols_for("use crate::callee;\nfn caller() {\n    callee();\n}\nfn callee() {\n}\n");
        let scip = ScipIndex {
            documents: vec![Document {
                relative_path: "src/m.rs".into(),
                occurrences: vec![
                    occ("c/caller().", 2, true),
                    occ("c/callee().", 5, true),
                    occ("c/callee().", 3, false),   // an edge
                    occ("c/callee().", 1, false),   // unattributed (a `use` line)
                    occ("std/println!", 3, false),  // external
                    occ("local 4", 3, false),       // a local
                ],
            }],
        };
        let s = CallGraph::assemble(&scip, &symbols).stats;
        assert_eq!(
            s.edges_kept + s.external + s.locals + s.unattributed,
            s.references,
            "buckets {:?} must sum to {} references",
            (s.edges_kept, s.external, s.locals, s.unattributed),
            s.references
        );
        assert_eq!(s.edges_kept, 1);
        assert_eq!(s.external, 1);
        assert_eq!(s.locals, 1);
        assert_eq!(s.unattributed, 1);
    }

    /// SCIP `local N` symbols are **document-scoped**: `local 0` in two files
    /// are different things. Keying them globally merged unrelated symbols and
    /// produced an edge claiming one function called another thirty-eight times.
    #[test]
    fn identically_named_locals_in_different_files_do_not_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.rs"), "fn a() {\n    let x = 1;\n    let _ = x;\n}\n").unwrap();
        std::fs::write(src.join("b.rs"), "fn b() {\n    let y = 2;\n    let _ = y;\n}\n").unwrap();
        let symbols = SymbolIndex::build(tmp.path());

        let scip = ScipIndex {
            documents: vec![
                Document {
                    relative_path: "src/a.rs".into(),
                    occurrences: vec![
                        occ("c/a().", 1, true),
                        occ("local 0", 2, true),
                        occ("local 0", 3, false),
                    ],
                },
                Document {
                    relative_path: "src/b.rs".into(),
                    occurrences: vec![
                        occ("c/b().", 1, true),
                        occ("local 0", 2, true),
                        occ("local 0", 3, false),
                    ],
                },
            ],
        };
        let graph = CallGraph::assemble(&scip, &symbols);
        assert!(
            graph.edges.is_empty(),
            "locals must produce no edges at all, got {:?}",
            graph.edges
        );
        assert_eq!(graph.stats.self_edges, 0);
        assert_eq!(graph.stats.locals, 2);
    }

    #[test]
    fn an_empty_index_yields_an_empty_graph_rather_than_an_error() {
        let graph = CallGraph::assemble(&ScipIndex::default(), &SymbolIndex::default());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert_eq!(graph.stats.occurrences, 0);
    }
}
