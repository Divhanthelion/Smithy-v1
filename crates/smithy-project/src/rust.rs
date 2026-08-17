//! Rust project ingestion.
//!
//! Extracts a structured description of a Cargo project: its crates, their
//! module trees, and their public API surfaces. This is the standard operating
//! procedure for grounding the agent in Rust work.
//!
//! ## What it extracts and why
//!
//! The goal is to answer, without the model spending turns on `ls` and `read`,
//! the three questions it otherwise always asks first:
//!
//! 1. **What crates exist and where?** From `cargo metadata`. Also gives the
//!    edition, which changes what code is even valid.
//! 2. **What modules does each crate have?** From the file tree, converted to
//!    module paths, so the model can name `crate::tools::edit` instead of
//!    guessing at `src/tools/edit.rs`.
//! 3. **What is each crate's public API?** Signatures only, via tree-sitter.
//!    This is what lets it call across crates correctly on the first try.
//!
//! Dependencies are listed with their version requirements, because the single
//! most common failure on local models is inventing an API from a different
//! major version of a crate.
//!
//! ## What it deliberately does not extract
//!
//! Function bodies, private items, tests, and transitive dependencies.
//! `cargo metadata --no-deps` is used precisely to avoid pulling the entire
//! dependency graph, which on a real project is thousands of packages and would
//! swamp the context with things the model cannot act on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use streaming_iterator::StreamingIterator;

/// One crate in the project.
#[derive(Debug, Clone, PartialEq)]
pub struct Crate {
    pub name: String,
    /// Path relative to the project root.
    pub path: PathBuf,
    pub version: String,
    pub edition: String,
    /// `lib`, `bin`, `proc-macro`, …
    pub targets: Vec<String>,
    /// Direct dependencies as `(name, requirement)`.
    pub dependencies: Vec<(String, String)>,
    /// Module paths, e.g. `tools::edit`. Sorted.
    pub modules: Vec<String>,
    /// Public item signatures, grouped by the file they came from.
    pub api: Vec<ApiItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiItem {
    /// Module path the item lives in, e.g. `tools::edit`.
    pub module: String,
    pub kind: ApiKind,
    pub signature: String,
    /// First `///` doc line, when present. Used for the top-ranked API rows
    /// in the context block — a one-line purpose beats a parameter list.
    pub doc_line: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApiKind {
    Struct,
    Enum,
    Trait,
    Function,
    Type,
    Const,
}

impl ApiKind {
    pub fn label(self) -> &'static str {
        match self {
            ApiKind::Struct => "struct",
            ApiKind::Enum => "enum",
            ApiKind::Trait => "trait",
            ApiKind::Function => "fn",
            ApiKind::Type => "type",
            ApiKind::Const => "const",
        }
    }
}

/// Read the crate graph via `cargo metadata --no-deps`.
///
/// Returns `Err` when cargo is missing or the manifest does not parse — both of
/// which the caller should surface rather than silently degrade, because a Rust
/// project whose metadata cannot be read is a project the agent will struggle
/// in and the user should know why.
pub fn crates(root: &Path) -> Result<Vec<Crate>, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo metadata failed: {}",
            stderr.lines().next().unwrap_or("unknown error").trim()
        ));
    }

    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("could not parse cargo metadata: {e}"))?;

    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata had no packages array")?;

    let mut out = Vec::new();
    for package in packages {
        let manifest = PathBuf::from(package["manifest_path"].as_str().unwrap_or_default());
        let crate_dir = manifest.parent().unwrap_or(root).to_path_buf();
        let relative = crate_dir
            .strip_prefix(root)
            .unwrap_or(&crate_dir)
            .to_path_buf();

        let mut targets: Vec<String> = package["targets"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .filter_map(|t| t["kind"][0].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        targets.sort();
        targets.dedup();

        let mut dependencies: Vec<(String, String)> = package["dependencies"]
            .as_array()
            .map(|ds| {
                ds.iter()
                    .filter(|d| d["kind"].is_null()) // normal deps only, not dev/build
                    .filter_map(|d| {
                        Some((
                            d["name"].as_str()?.to_string(),
                            d["req"].as_str().unwrap_or("*").to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        dependencies.sort();

        let src = crate_dir.join("src");
        let modules = module_paths(&src);
        let api = public_api(&src);

        out.push(Crate {
            name: package["name"].as_str().unwrap_or("?").to_string(),
            path: relative,
            version: package["version"].as_str().unwrap_or("?").to_string(),
            edition: package["edition"].as_str().unwrap_or("?").to_string(),
            targets,
            dependencies,
            modules,
            api,
        });
    }

    // Deterministic order: same tree in, same bytes out. `cargo metadata` does
    // not promise a stable package order, and an unstable order here would
    // change the system prompt between runs and cost a cold prefill.
    out.sort_by_key(|c| c.name.clone());
    Ok(out)
}

/// The module path for one file, relative to a crate's `src/`.
///
/// `src/tools/edit.rs` → `tools::edit`; `src/tools/mod.rs` → `tools`;
/// `src/lib.rs` and `src/main.rs` are the crate root and yield the empty string.
///
/// Extracted from [`module_paths`] so the symbol index applies exactly the same
/// rule. Two implementations of "what module is this file" that disagreed would
/// mean the map and the index naming the same item differently, which is worse
/// than either being wrong on its own.
pub fn module_path_of(file: &Path, src: &Path) -> String {
    let Ok(relative) = file.strip_prefix(src) else {
        return String::new();
    };
    let mut parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();

    let Some(name) = parts.pop() else {
        return String::new();
    };
    let stem = name.trim_end_matches(".rs");

    match stem {
        "lib" | "main" if parts.is_empty() => return String::new(),
        "mod" => {}
        _ => parts.push(stem.to_string()),
    }
    parts.join("::")
}

/// Convert a crate's `src/` tree into Rust module paths.
///
/// `src/tools/edit.rs` → `tools::edit`; `src/tools/mod.rs` → `tools`;
/// `src/lib.rs` and `src/main.rs` are the crate root and produce nothing.
pub fn module_paths(src: &Path) -> Vec<String> {
    if !src.is_dir() {
        return Vec::new();
    }
    let mut modules = Vec::new();

    for entry in ignore::WalkBuilder::new(src)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .build()
        .flatten()
    {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(src) else {
            continue;
        };

        let mut parts: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();

        let Some(file) = parts.pop() else { continue };
        let stem = file.trim_end_matches(".rs");

        match stem {
            "lib" | "main" if parts.is_empty() => continue, // crate root
            "mod" => {}                                     // directory module
            _ => parts.push(stem.to_string()),
        }

        if !parts.is_empty() {
            modules.push(parts.join("::"));
        }
    }

    modules.sort();
    modules.dedup();
    modules
}

/// Extract public item signatures from every `.rs` file under `src`.
pub fn public_api(src: &Path) -> Vec<ApiItem> {
    if !src.is_dir() {
        return Vec::new();
    }
    let mut items = Vec::new();

    for entry in ignore::WalkBuilder::new(src)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .build()
        .flatten()
    {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let module = module_of(src, path);
        items.extend(public_items_in(&source, &module));
    }

    // Stable order for a stable prompt.
    items.sort_by(|a, b| {
        a.module
            .cmp(&b.module)
            .then(a.kind.cmp(&b.kind))
            .then(a.signature.cmp(&b.signature))
    });
    items.dedup();
    items
}

fn module_of(src: &Path, file: &Path) -> String {
    let Ok(relative) = file.strip_prefix(src) else {
        return String::new();
    };
    let mut parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let Some(name) = parts.pop() else {
        return String::new();
    };
    let stem = name.trim_end_matches(".rs");
    match stem {
        "lib" | "main" | "mod" => {}
        _ => parts.push(stem.to_string()),
    }
    parts.join("::")
}

/// Public items in one file, via tree-sitter.
///
/// Signatures only — the body of a function tells the model nothing it needs in
/// order to *call* it, and bodies are where all the tokens are.
pub fn public_items_in(source: &str, module: &str) -> Vec<ApiItem> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    const QUERY: &str = r#"
        (function_item (visibility_modifier) @vis) @fn
        (struct_item (visibility_modifier) @vis) @struct
        (enum_item (visibility_modifier) @vis) @enum
        (trait_item (visibility_modifier) @vis) @trait
        (type_item (visibility_modifier) @vis) @type
        (const_item (visibility_modifier) @vis) @const
    "#;

    let Ok(query) = tree_sitter::Query::new(&tree_sitter_rust::LANGUAGE.into(), QUERY) else {
        return Vec::new();
    };
    let names: Vec<&str> = query.capture_names().to_vec();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches: tree_sitter::QueryMatches<'_, '_, &[u8], &[u8]> =
        cursor.matches(&query, tree.root_node(), source.as_bytes());

    let mut items = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let Some(name) = names.get(capture.index as usize) else {
                continue;
            };
            let kind = match *name {
                "fn" => ApiKind::Function,
                "struct" => ApiKind::Struct,
                "enum" => ApiKind::Enum,
                "trait" => ApiKind::Trait,
                "type" => ApiKind::Type,
                "const" => ApiKind::Const,
                _ => continue, // the @vis capture
            };

            // Only top-level items: an item nested inside a `mod` block or an
            // `impl` is reached by a different path and would be misattributed
            // to this module.
            if !is_top_level(capture.node) {
                continue;
            }

            let Ok(text) = capture.node.utf8_text(source.as_bytes()) else {
                continue;
            };
            if let Some(signature) = signature_of(text) {
                items.push(ApiItem {
                    module: module.to_string(),
                    kind,
                    signature,
                    doc_line: doc_first_line(source, capture.node),
                });
            }
        }
    }
    items
}

/// Whether a node sits directly in the file's top level.
fn is_top_level(node: tree_sitter::Node) -> bool {
    node.parent()
        .map(|p| p.kind() == "source_file")
        .unwrap_or(false)
}

/// Reduce an item's source text to a one-line signature.
///
/// Cuts at the body (`{`) or the terminating `;`, collapses whitespace, and
/// drops doc comments and attributes.
fn signature_of(text: &str) -> Option<String> {
    let body = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with("#["))
        .collect::<Vec<_>>()
        .join(" ");

    let cut = body
        .find(" {")
        .or_else(|| body.find('{'))
        .or_else(|| body.find(';'))
        .unwrap_or(body.len());

    let signature = body[..cut].split_whitespace().collect::<Vec<_>>().join(" ");
    if signature.is_empty() {
        return None;
    }
    Some(signature)
}

/// First non-empty `///` line immediately above an item.
///
/// Walks previous siblings, skipping attributes, collecting doc comments in
/// source order. Ordinary `//` comments stop the walk — they are not docs.
fn doc_first_line(source: &str, node: tree_sitter::Node<'_>) -> Option<String> {
    let mut docs: Vec<String> = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "line_comment" => {
                let Ok(text) = p.utf8_text(source.as_bytes()) else {
                    break;
                };
                let trimmed = text.trim();
                if let Some(rest) = trimmed.strip_prefix("///") {
                    docs.push(rest.trim().to_string());
                    prev = p.prev_named_sibling();
                } else {
                    // A plain `//` between docs and the item is not documentation.
                    break;
                }
            }
            "attribute_item" => {
                prev = p.prev_named_sibling();
            }
            _ => break,
        }
    }
    docs.reverse();
    docs.into_iter().find(|l| !l.is_empty())
}

/// Bare name inside a signature: `pub fn run_turn(...)` → `run_turn`.
pub fn api_item_name(signature: &str) -> Option<&str> {
    const KINDS: &[&str] = &["fn", "struct", "enum", "trait", "type", "const"];
    let tokens: Vec<&str> = signature.split_whitespace().collect();
    for (i, token) in tokens.iter().enumerate() {
        if KINDS.contains(token) {
            let raw = tokens.get(i + 1)?;
            let name = raw.split('<').next().unwrap_or(raw);
            let name = name.split('(').next().unwrap_or(name);
            if name.is_empty() {
                return None;
            }
            return Some(name);
        }
    }
    None
}

/// Group API items by module, preserving the sorted order.
pub fn group_by_module(items: &[ApiItem]) -> BTreeMap<&str, Vec<&ApiItem>> {
    let mut grouped: BTreeMap<&str, Vec<&ApiItem>> = BTreeMap::new();
    for item in items {
        grouped.entry(item.module.as_str()).or_default().push(item);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths_map_files_to_module_names() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("tools")).unwrap();
        std::fs::write(src.join("lib.rs"), "").unwrap();
        std::fs::write(src.join("config.rs"), "").unwrap();
        std::fs::write(src.join("tools/mod.rs"), "").unwrap();
        std::fs::write(src.join("tools/edit.rs"), "").unwrap();

        assert_eq!(
            module_paths(&src),
            vec![
                "config".to_string(),
                "tools".to_string(),
                "tools::edit".to_string()
            ]
        );
    }

    #[test]
    fn the_crate_root_is_not_a_module() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "").unwrap();
        assert!(module_paths(&src).is_empty());
    }

    #[test]
    fn a_missing_src_directory_yields_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(module_paths(&tmp.path().join("src")).is_empty());
        assert!(public_api(&tmp.path().join("src")).is_empty());
    }

    #[test]
    fn extracts_public_signatures_without_bodies() {
        let source = r#"
pub fn parse(input: &str) -> Result<Ast, Error> {
    let noise = 1;
    todo!()
}
pub struct Config {
    pub verbose: bool,
}
pub enum Mode { Fast, Slow }
pub trait Tool { fn run(&self); }
"#;
        let items = public_items_in(source, "core");
        let sigs: Vec<&str> = items.iter().map(|i| i.signature.as_str()).collect();

        assert!(sigs
            .iter()
            .any(|s| s.starts_with("pub fn parse(input: &str) -> Result<Ast, Error>")));
        assert!(sigs.iter().any(|s| s.contains("pub struct Config")));
        assert!(sigs.iter().any(|s| s.contains("pub enum Mode")));
        assert!(sigs.iter().any(|s| s.contains("pub trait Tool")));
        assert!(
            !sigs
                .iter()
                .any(|s| s.contains("todo!") || s.contains("noise")),
            "bodies must not leak into signatures: {sigs:?}"
        );
    }

    /// Private items are not callable from elsewhere, so they are noise.
    #[test]
    fn private_items_are_excluded() {
        let items = public_items_in("fn hidden() {}\nstruct Secret;\npub fn shown() {}", "m");
        assert_eq!(items.len(), 1);
        assert!(items[0].signature.contains("shown"));
    }

    /// An item inside an `impl` or a nested `mod` belongs to a different path
    /// and would be misattributed if reported against this module.
    #[test]
    fn nested_items_are_excluded() {
        let source = r#"
pub struct Thing;
impl Thing {
    pub fn method(&self) {}
}
pub mod inner {
    pub fn buried() {}
}
"#;
        let sigs: Vec<String> = public_items_in(source, "m")
            .into_iter()
            .map(|i| i.signature)
            .collect();
        assert!(sigs.iter().any(|s| s.contains("pub struct Thing")));
        assert!(!sigs.iter().any(|s| s.contains("method")), "got {sigs:?}");
        assert!(!sigs.iter().any(|s| s.contains("buried")), "got {sigs:?}");
    }

    #[test]
    fn attributes_and_doc_comments_are_stripped() {
        let source = "/// Docs here\n#[derive(Debug)]\npub struct Tagged { pub a: u8 }";
        let items = public_items_in(source, "m");
        assert_eq!(items.len(), 1);
        assert!(!items[0].signature.contains("Docs"));
        assert!(!items[0].signature.contains("derive"));
        assert!(items[0].signature.contains("pub struct Tagged"));
    }

    #[test]
    fn a_multiline_signature_collapses_to_one_line() {
        let source = "pub fn wide(\n    a: u32,\n    b: u32,\n) -> u32 { a + b }";
        let items = public_items_in(source, "m");
        assert_eq!(items.len(), 1);
        assert!(!items[0].signature.contains('\n'));
        assert!(items[0].signature.contains("a: u32, b: u32"));
    }

    #[test]
    fn unparseable_source_yields_no_items_rather_than_panicking() {
        assert!(public_items_in("fn ((( {{{ !!!", "m").is_empty());
    }

    /// The whole design depends on the same tree producing the same bytes.
    #[test]
    fn extraction_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn one() {}").unwrap();
        std::fs::write(src.join("a/mod.rs"), "pub struct Two;").unwrap();
        std::fs::write(src.join("a/deep.rs"), "pub trait Three {}").unwrap();

        let first = public_api(&src);
        for _ in 0..5 {
            assert_eq!(
                public_api(&src),
                first,
                "extraction must be stable across runs"
            );
        }
        assert_eq!(module_paths(&src), module_paths(&src));
    }

    #[test]
    fn api_items_group_by_module_in_order() {
        let items = vec![
            ApiItem {
                module: "b".into(),
                kind: ApiKind::Function,
                signature: "pub fn y()".into(),
                doc_line: None,
            },
            ApiItem {
                module: "a".into(),
                kind: ApiKind::Function,
                signature: "pub fn x()".into(),
                doc_line: None,
            },
        ];
        let grouped = group_by_module(&items);
        assert_eq!(grouped.keys().copied().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn the_first_doc_line_is_captured_for_an_item() {
        let source = r#"
/// Build the context block for a project.
///
/// More detail the map does not need.
pub fn extract() {}
"#;
        let items = public_items_in(source, "");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].doc_line.as_deref(),
            Some("Build the context block for a project.")
        );
    }

    #[test]
    fn api_item_name_strips_generics_and_args() {
        assert_eq!(
            api_item_name(
                "pub fn extract(project: &Project, budget: ContextBudget) -> ProjectContext"
            ),
            Some("extract")
        );
        assert_eq!(api_item_name("pub struct Config<T>"), Some("Config"));
    }

    #[test]
    fn reads_the_crate_graph_of_a_real_workspace() {
        // This repository is itself a Cargo workspace, which makes it a
        // convenient fixture and also checks the real-world path.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let crates = crates(root).expect("cargo metadata should succeed on our own workspace");

        assert!(
            crates.len() >= 5,
            "expected several crates, got {}",
            crates.len()
        );
        assert!(crates.iter().any(|c| c.name == "smithy-project"));

        let names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "crate order must be deterministic");

        let me = crates.iter().find(|c| c.name == "smithy-project").unwrap();
        assert_eq!(me.edition, "2021");
        assert!(me.dependencies.iter().any(|(n, _)| n == "tree-sitter"));
    }

    #[test]
    fn a_directory_without_cargo_reports_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(crates(tmp.path()).is_err());
    }
}
