//! A symbol index: name → where it is defined, and what it looks like.
//!
//! ## Why this exists alongside the context block
//!
//! [`crate::context`] renders a *map* that goes in the system prompt, and its
//! whole design is a token budget — it stops when it runs out. That makes it the
//! wrong place for detail. A real session demonstrated the gap precisely: the map
//! said
//!
//! ```text
//! components::desktop::pub enum DesktopMsg
//! ```
//!
//! and the model, unable to see the variants, emitted `DesktopMsg::PluginsChanged`
//! — which did not exist. It also called `restore_session` with the wrong arity,
//! because that method is neither `pub` nor top-level and so never appears in the
//! map at all. Four of the seven build errors in that session were this one
//! shape: *a name the model could see existed, whose shape it could not.*
//!
//! Putting all of that in the prompt is not the answer — it is several times the
//! size of the map and would be prefilled on every request. So the index lives in
//! memory and is *asked*, not read: an exact-name lookup is a `HashMap` hit
//! rather than a `grep` of the tree.
//!
//! ## What it indexes that the map does not
//!
//! - **Enum variants**, with their container.
//! - **Methods inside `impl` blocks** — which is most of the code in a UI crate,
//!   and none of which the map can reach, since [`crate::rust`] deliberately
//!   keeps only top-level items.
//! - **Private items.** The map describes the public surface, which is right for
//!   a map. But the agent is editing the crate from the inside, where `pub` is
//!   not the interesting boundary.
//! - **`file:line` for everything**, so an answer can be verified rather than
//!   trusted.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Struct,
    Enum,
    /// A variant of an enum. Carries its enum in [`Symbol::container`].
    Variant,
    Trait,
    /// A free function.
    Function,
    /// A function inside an `impl`. Carries its type in [`Symbol::container`].
    Method,
    Type,
    Const,
    Static,
    Module,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Variant => "variant",
            SymbolKind::Trait => "trait",
            SymbolKind::Function => "fn",
            SymbolKind::Method => "method",
            SymbolKind::Type => "type",
            SymbolKind::Const => "const",
            SymbolKind::Static => "static",
            SymbolKind::Module => "mod",
        }
    }
}

/// One definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// The bare name, which is what a lookup is keyed on.
    pub name: String,
    pub kind: SymbolKind,
    /// The enclosing enum or `impl` type, for variants and methods.
    pub container: Option<String>,
    /// The module path, e.g. `components::desktop`.
    pub module: String,
    /// One line, body removed.
    pub signature: String,
    /// Workspace-relative.
    pub file: String,
    /// 1-based, so it can be pasted into an editor or a `read` offset.
    pub line: usize,
    pub is_public: bool,
}

impl Symbol {
    /// How the symbol is written when it is referred to in full.
    pub fn qualified(&self) -> String {
        match &self.container {
            Some(container) => format!("{}::{container}::{}", self.module, self.name),
            None => format!("{}::{}", self.module, self.name),
        }
    }

    /// One rendered line for a tool result.
    ///
    /// The signature already carries the visibility and the keyword for every
    /// kind except a variant, whose signature is the bare `Name(Ty)`. Prefixing
    /// them unconditionally produced `pub enum pub enum DesktopMsg`.
    pub fn render(&self) -> String {
        let prefix = match self.kind {
            SymbolKind::Variant => "variant ",
            _ => "",
        };
        format!("{}:{} — {prefix}{}", self.file, self.line, self.signature)
    }
}

/// Every symbol in a project, indexed by bare name.
#[derive(Debug, Default, Clone)]
pub struct SymbolIndex {
    by_name: HashMap<String, Vec<Symbol>>,
    count: usize,
    files: usize,
}

impl SymbolIndex {
    /// Exact-name lookup. This is the point of the whole module: one hash, no
    /// walk of the tree, no regex over every file.
    pub fn lookup(&self, name: &str) -> &[Symbol] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Case-insensitive substring search, for when the exact name is not known.
    ///
    /// Linear, unlike [`SymbolIndex::lookup`] — this is the fallback, and it is
    /// bounded by `limit` because a two-letter query would otherwise return the
    /// whole index.
    pub fn search(&self, needle: &str, limit: usize) -> Vec<&Symbol> {
        let needle = needle.to_lowercase();
        let mut hits: Vec<&Symbol> = self
            .by_name
            .iter()
            .filter(|(name, _)| name.to_lowercase().contains(&needle))
            .flat_map(|(_, symbols)| symbols.iter())
            .collect();
        // Shortest name first: a search for `Msg` should surface `Msg` itself
        // ahead of `ContextMenuMsgHandler`.
        hits.sort_by(|a, b| {
            a.name
                .len()
                .cmp(&b.name.len())
                .then(a.name.cmp(&b.name))
                .then(a.file.cmp(&b.file))
        });
        hits.truncate(limit);
        hits
    }

    /// The closest names to one that was not found.
    ///
    /// Substring search alone is not enough for the case that matters: a model
    /// writing `DesktopMessage` for `DesktopMsg` shares no substring in either
    /// direction, so a plain `contains` returns nothing and the answer becomes
    /// "no idea" when the right answer is one line away.
    ///
    /// So it falls back to the longest shared *prefix*, shortening the query
    /// until something matches. `DesktopMessage` → `DesktopMes` → … →
    /// `Desktop`, which hits. Stops at three characters, below which every
    /// result would be noise.
    pub fn nearest(&self, needle: &str, limit: usize) -> Vec<&Symbol> {
        let direct = self.search(needle, limit);
        if !direct.is_empty() {
            return direct;
        }
        let chars: Vec<char> = needle.chars().collect();
        for take in (3..chars.len()).rev() {
            let prefix: String = chars[..take].iter().collect();
            let hits = self.search(&prefix, limit);
            if !hits.is_empty() {
                return hits;
            }
        }
        Vec::new()
    }

    /// Every variant of an enum, in declaration order.
    ///
    /// The question that would have prevented four build errors in one session.
    pub fn variants_of(&self, enum_name: &str) -> Vec<&Symbol> {
        let mut variants: Vec<&Symbol> = self
            .by_name
            .values()
            .flatten()
            .filter(|s| {
                s.kind == SymbolKind::Variant && s.container.as_deref() == Some(enum_name)
            })
            .collect();
        variants.sort_by_key(|s| s.line);
        variants
    }

    /// Every method on a type.
    pub fn methods_of(&self, type_name: &str) -> Vec<&Symbol> {
        let mut methods: Vec<&Symbol> = self
            .by_name
            .values()
            .flatten()
            .filter(|s| s.kind == SymbolKind::Method && s.container.as_deref() == Some(type_name))
            .collect();
        methods.sort_by_key(|s| s.line);
        methods
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many files were parsed.
    pub fn files(&self) -> usize {
        self.files
    }

    fn insert(&mut self, symbol: Symbol) {
        self.count += 1;
        self.by_name.entry(symbol.name.clone()).or_default().push(symbol);
    }

    /// Walk a project and index every Rust file in it.
    ///
    /// Gitignore-aware, matching the search tools — an indexed `target/` would be
    /// both enormous and useless. Unlike `glob`, hidden files are *not* skipped:
    /// this is a structural index, and a module under a dotted directory is
    /// still code the agent may have to edit.
    pub fn build(root: &Path) -> SymbolIndex {
        let mut index = SymbolIndex::default();

        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .require_git(false)
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            // Guard against a generated monster: a megabyte of bindings costs
            // seconds to parse and answers nothing anyone asks.
            if source.len() > 2 * 1024 * 1024 {
                continue;
            }
            let display = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let module = module_for(path);

            index.files += 1;
            for symbol in symbols_in(&source, &module, &display) {
                index.insert(symbol);
            }
        }
        index
    }
}

/// The module path for a file, found by locating its nearest `src/` ancestor.
///
/// A workspace has many crates and therefore many `src/` roots, so the path has
/// to be resolved per file rather than against one project-wide prefix. Files
/// outside any `src/` — build scripts, examples, tests — keep their stem, which
/// is what someone searching for them would type.
fn module_for(file: &Path) -> String {
    let mut ancestor = file.parent();
    while let Some(dir) = ancestor {
        if dir.file_name().and_then(|n| n.to_str()) == Some("src") {
            return crate::rust::module_path_of(file, dir);
        }
        ancestor = dir.parent();
    }
    file.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Extract every symbol in one file.
pub fn symbols_in(source: &str, module: &str, file: &str) -> Vec<Symbol> {
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

    let mut out = Vec::new();
    walk(tree.root_node(), source, module, file, None, &mut out);
    out
}

/// Recursive descent, carrying the current module path and container.
///
/// Written as a walk rather than a tree-sitter query because the interesting
/// facts are *relational* — a variant belongs to its enum, a method to its
/// `impl` type — and expressing "the name of my grandparent" in a query is far
/// less legible than tracking it on the way down.
fn walk(
    node: tree_sitter::Node,
    source: &str,
    module: &str,
    file: &str,
    container: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "mod_item" => {
                // An inline `mod foo { … }` extends the path; a `mod foo;`
                // declaration has no body and its contents live in another file
                // that the walker reaches on its own.
                let Some(name) = name_of(child, source) else {
                    continue;
                };
                out.push(make(child, source, &name, SymbolKind::Module, None, module, file));
                if let Some(body) = child.child_by_field_name("body") {
                    let nested = if module.is_empty() {
                        name.clone()
                    } else {
                        format!("{module}::{name}")
                    };
                    walk(body, source, &nested, file, None, out);
                }
            }
            "enum_item" => {
                let Some(name) = name_of(child, source) else {
                    continue;
                };
                out.push(make(child, source, &name, SymbolKind::Enum, None, module, file));
                // The variants — the thing the map cannot show and a model
                // cannot guess.
                if let Some(body) = child.child_by_field_name("body") {
                    let mut vc = body.walk();
                    for variant in body.children(&mut vc) {
                        if variant.kind() != "enum_variant" {
                            continue;
                        }
                        let Some(vname) = name_of(variant, source) else {
                            continue;
                        };
                        out.push(make(
                            variant,
                            source,
                            &vname,
                            SymbolKind::Variant,
                            Some(&name),
                            module,
                            file,
                        ));
                    }
                }
            }
            "impl_item" => {
                // `impl Foo` and `impl Trait for Foo` both hang their methods
                // off the concrete type, which is what someone looking up a
                // method has in hand.
                let type_name = child
                    .child_by_field_name("type")
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .map(|s| s.trim().to_string());
                if let Some(body) = child.child_by_field_name("body") {
                    walk(body, source, module, file, type_name.as_deref(), out);
                }
            }
            "trait_item" => {
                let Some(name) = name_of(child, source) else {
                    continue;
                };
                out.push(make(child, source, &name, SymbolKind::Trait, None, module, file));
                if let Some(body) = child.child_by_field_name("body") {
                    walk(body, source, module, file, Some(&name), out);
                }
            }
            "function_item" | "function_signature_item" => {
                let Some(name) = name_of(child, source) else {
                    continue;
                };
                // Inside an `impl` or `trait` it is a method; at file or module
                // level it is a free function.
                let kind = if container.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                out.push(make(child, source, &name, kind, container, module, file));
            }
            "struct_item" => push_named(child, source, SymbolKind::Struct, module, file, out),
            "type_item" => push_named(child, source, SymbolKind::Type, module, file, out),
            "const_item" => push_named(child, source, SymbolKind::Const, module, file, out),
            "static_item" => push_named(child, source, SymbolKind::Static, module, file, out),
            // Bodies and blocks that hold more items.
            "declaration_list" | "source_file" => {
                walk(child, source, module, file, container, out)
            }
            _ => {}
        }
    }
}

fn push_named(
    node: tree_sitter::Node,
    source: &str,
    kind: SymbolKind,
    module: &str,
    file: &str,
    out: &mut Vec<Symbol>,
) {
    if let Some(name) = name_of(node, source) {
        out.push(make(node, source, &name, kind, None, module, file));
    }
}

fn name_of(node: tree_sitter::Node, source: &str) -> Option<String> {
    node.child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()
        .map(str::to_string)
}

fn make(
    node: tree_sitter::Node,
    source: &str,
    name: &str,
    kind: SymbolKind,
    container: Option<&str>,
    module: &str,
    file: &str,
) -> Symbol {
    let text = node.utf8_text(source.as_bytes()).unwrap_or("");
    Symbol {
        name: name.to_string(),
        kind,
        container: container.map(str::to_string),
        module: module.to_string(),
        signature: one_line_signature(text),
        file: file.to_string(),
        // tree-sitter rows are 0-based; editors and `read` offsets are not.
        line: node.start_position().row + 1,
        is_public: has_visibility(node, source),
    }
}

fn has_visibility(node: tree_sitter::Node, source: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|c| {
        c.kind() == "visibility_modifier"
            && c.utf8_text(source.as_bytes())
                .map(|t| t.starts_with("pub"))
                .unwrap_or(false)
    });
    found
}

/// Reduce a definition to one readable line.
///
/// Cuts at the body, drops doc comments and attributes, collapses whitespace.
/// A signature is what you need in order to *call* something; the body is where
/// all the bytes are.
fn one_line_signature(text: &str) -> String {
    let cleaned = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with("#["))
        .collect::<Vec<_>>()
        .join(" ");

    let cut = cleaned
        .find(" {")
        .or_else(|| cleaned.find('{'))
        .or_else(|| cleaned.find(';'))
        .unwrap_or(cleaned.len());
    let head = cleaned[..cut].trim();

    let flat = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 200 {
        format!("{}…", flat.chars().take(200).collect::<String>())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(source: &str) -> Vec<Symbol> {
        symbols_in(source, "components::desktop", "src/components/desktop.rs")
    }

    /// The headline gap. The context block says an enum exists; nothing said
    /// what was in it, and a model emitted a variant that did not exist.
    #[test]
    fn enum_variants_are_indexed_with_their_enum() {
        let symbols = index_of(
            "pub enum DesktopMsg {\n    CloseWindow(String),\n    DesktopClick,\n}\n",
        );
        let variants: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Variant)
            .collect();
        assert_eq!(variants.len(), 2, "{symbols:#?}");
        assert_eq!(variants[0].name, "CloseWindow");
        assert_eq!(variants[0].container.as_deref(), Some("DesktopMsg"));
        assert_eq!(variants[1].name, "DesktopClick");
    }

    /// The other half: `restore_session` is a private method inside an `impl`,
    /// so the public-API map could never show it, and its arity was guessed.
    #[test]
    fn impl_methods_are_indexed_with_their_type() {
        let symbols = index_of(
            "impl Desktop {\n    fn restore_session(&mut self, session: Session) {}\n}\n",
        );
        let method = symbols
            .iter()
            .find(|s| s.name == "restore_session")
            .expect("method found");
        assert_eq!(method.kind, SymbolKind::Method);
        assert_eq!(method.container.as_deref(), Some("Desktop"));
        assert!(!method.is_public, "it is private, and still indexed");
        assert!(
            method.signature.contains("session: Session"),
            "the signature must carry the arity: {}",
            method.signature
        );
    }

    /// A trait impl hangs its methods off the concrete type, which is what
    /// someone looking the method up actually has.
    #[test]
    fn trait_impl_methods_attach_to_the_concrete_type() {
        let symbols = index_of("impl Component for Desktop {\n    fn create() {}\n}\n");
        let method = symbols.iter().find(|s| s.name == "create").unwrap();
        assert_eq!(method.container.as_deref(), Some("Desktop"));
    }

    #[test]
    fn line_numbers_are_one_based_so_they_can_be_pasted_into_an_editor() {
        let symbols = index_of("\n\npub struct Desktop;\n");
        let s = symbols.iter().find(|s| s.name == "Desktop").unwrap();
        assert_eq!(s.line, 3);
    }

    #[test]
    fn visibility_is_recorded_but_never_a_filter() {
        let symbols = index_of("pub struct Shown;\nstruct Hidden;\n");
        assert!(symbols.iter().find(|s| s.name == "Shown").unwrap().is_public);
        assert!(!symbols.iter().find(|s| s.name == "Hidden").unwrap().is_public);
    }

    #[test]
    fn inline_modules_extend_the_path_and_are_themselves_symbols() {
        let symbols = symbols_in("pub mod inner { pub struct Deep; }", "outer", "src/lib.rs");
        let deep = symbols.iter().find(|s| s.name == "Deep").unwrap();
        assert_eq!(deep.module, "outer::inner");
        assert!(symbols.iter().any(|s| s.name == "inner" && s.kind == SymbolKind::Module));
    }

    /// `mod foo;` has no body — its contents are in another file the walker
    /// reaches separately, and must not be attributed here.
    #[test]
    fn a_bodiless_module_declaration_adds_nothing_but_itself() {
        let symbols = symbols_in("pub mod other;", "crate_root", "src/lib.rs");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].kind, SymbolKind::Module);
    }

    // --- the index itself ---

    fn built() -> SymbolIndex {
        let mut index = SymbolIndex::default();
        for s in index_of(
            "pub enum DesktopMsg { CloseWindow(String), DesktopClick }\n\
             impl Desktop { fn restore_session(&mut self, s: Session) {} }\n\
             pub struct Desktop;\n",
        ) {
            index.insert(s);
        }
        index
    }

    #[test]
    fn an_exact_lookup_finds_every_definition_of_a_name() {
        let index = built();
        let hits = index.lookup("restore_session");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].container.as_deref(), Some("Desktop"));
        assert!(index.lookup("nothing_named_this").is_empty());
    }

    #[test]
    fn variants_of_answers_the_question_that_broke_the_build() {
        let index = built();
        let names: Vec<&str> = index
            .variants_of("DesktopMsg")
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["CloseWindow", "DesktopClick"]);
        assert!(
            !names.contains(&"PluginsChanged"),
            "and says plainly when one is absent"
        );
    }

    #[test]
    fn methods_of_lists_a_types_methods() {
        let index = built();
        let names: Vec<&str> = index
            .methods_of("Desktop")
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["restore_session"]);
    }

    /// Search is the fallback and must stay bounded — a two-letter query would
    /// otherwise return the whole index.
    #[test]
    fn search_is_case_insensitive_shortest_first_and_capped() {
        let index = built();
        let hits = index.search("desktop", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "Desktop", "shortest match first");
        assert!(index.search("e", 2).len() <= 2, "the cap holds");
    }

    /// The signature already says `pub enum`; saying it again read as a stutter.
    #[test]
    fn a_rendered_line_does_not_repeat_the_keyword() {
        let symbols = index_of("pub enum DesktopMsg { CloseWindow(String) }\n");
        let e = symbols.iter().find(|s| s.kind == SymbolKind::Enum).unwrap();
        assert_eq!(e.render(), "src/components/desktop.rs:1 — pub enum DesktopMsg");

        // A variant's signature is bare, so it does get a tag.
        let v = symbols.iter().find(|s| s.kind == SymbolKind::Variant).unwrap();
        assert!(v.render().contains("variant CloseWindow(String)"), "{}", v.render());
    }

    /// The case substring search cannot reach: a misremembered name that shares
    /// only a prefix with the real one.
    #[test]
    fn nearest_falls_back_to_a_shared_prefix() {
        let index = built();
        let names: Vec<&str> = index
            .nearest("DesktopMessage", 5)
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            names.contains(&"DesktopMsg"),
            "a shared prefix must still find it: {names:?}"
        );
    }

    #[test]
    fn nearest_prefers_a_direct_match_when_there_is_one() {
        let index = built();
        let hits = index.nearest("Desktop", 5);
        assert_eq!(hits[0].name, "Desktop");
    }

    /// Below three characters every symbol matches and the answer is noise.
    #[test]
    fn nearest_gives_up_rather_than_returning_everything() {
        let index = built();
        assert!(index.nearest("Zq", 5).is_empty());
    }

    #[test]
    fn a_signature_keeps_the_head_and_drops_the_body() {
        assert_eq!(
            one_line_signature("pub fn f(a: u8) -> u8 {\n    a + 1\n}"),
            "pub fn f(a: u8) -> u8"
        );
        assert_eq!(one_line_signature("pub struct S;"), "pub struct S");
        assert_eq!(
            one_line_signature("#[derive(Debug)]\n/// doc\npub struct S { a: u8 }"),
            "pub struct S"
        );
    }
}
