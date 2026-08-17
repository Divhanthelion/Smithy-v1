//! Rendering a project into the block the model sees.
//!
//! ## The budget is the whole design
//!
//! A real workspace has more structure than fits usefully in a prompt. Ours has
//! seven crates and several hundred public items; dumping all of it would cost
//! tens of thousands of tokens *before the user has said anything*, and every
//! one of those tokens is prefilled on the first request of the session.
//!
//! So the context is built in **layers, most useful first**, and stops when the
//! budget runs out:
//!
//! 1. **Layout** — crate names, paths, kinds, editions. Never dropped. This is
//!    the map, and without it the model cannot even name a file correctly.
//! 2. **Dependencies** — name and version requirement. Cheap, and the single
//!    highest-value item per token: inventing an API from the wrong major
//!    version is the most common local-model failure there is.
//! 3. **Modules** — the module path of every file. Lets the model write
//!    `crate::tools::edit` instead of discovering it with three tool calls.
//! 4. **Public API** — signatures. The most expensive layer and the first to be
//!    truncated. Truncation is *reported* in the text, so the model knows the
//!    list is partial and can `grep` for the rest rather than concluding an
//!    item does not exist.
//!
//! Truncating from the bottom means a small budget still produces a *correct*
//! context, just a less complete one. Truncating proportionally across layers
//! would produce a context that is wrong everywhere.

use std::fmt::Write as _;

use crate::rust;
use crate::{Project, ProjectKind};

/// Appended when the API list did not fit.
///
/// Silence here would teach the model that an item it cannot see does not
/// exist, which is worse than telling it the list is partial.
const TRUNCATION_NOTICE: &str =
    "\n[Public API list truncated to fit. Items not shown still exist — look them up with \
     `symbol`, or `grep`, rather than assuming absence.]\n";

/// How much room the context block may take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// Ceiling in characters. Characters rather than tokens because we have no
    /// tokenizer and are not adding one — the endpoint reports real token
    /// counts after the fact, and roughly four characters per token is close
    /// enough to size a budget.
    pub max_chars: usize,
}

impl ContextBudget {
    /// Roughly `tokens * 4`.
    pub fn from_tokens(tokens: usize) -> Self {
        Self {
            max_chars: tokens * 4,
        }
    }

    /// ~6k tokens. Sized against a 131k window: large enough to describe a
    /// mid-sized workspace fully, small enough that it is a rounding error
    /// against the context ceiling.
    pub fn standard() -> Self {
        Self::from_tokens(6_000)
    }

    /// ~1.5k tokens, for when the window is small.
    pub fn compact() -> Self {
        Self::from_tokens(1_500)
    }

    /// Size the block against the model's actual context window.
    ///
    /// The app used to pass [`ContextBudget::standard`] unconditionally, so the
    /// project description was ~6k tokens whether the model had 32k of room or a
    /// million. On a large window that is a rounding error being treated as a
    /// constraint: a measured session against a 1M-token model went in with a
    /// 1,550-token system prompt and then spent tool calls rediscovering the
    /// layout it could have been handed.
    ///
    /// Five per cent of the window, clamped. The floor keeps a small model's
    /// context honest — the layers below [`compact`] stop being a usable map at
    /// all. The ceiling exists because this block is prefilled on *every*
    /// request of the session, so past a point more of it buys less than the
    /// tokens would buy elsewhere, and because the extraction itself grows with
    /// the workspace rather than with the window.
    pub fn for_window(context_length: Option<i64>) -> Self {
        const SHARE: f64 = 0.05;
        const FLOOR_TOKENS: usize = 6_000;
        const CEILING_TOKENS: usize = 40_000;

        let Some(window) = context_length.filter(|w| *w > 0) else {
            return Self::standard();
        };
        let tokens = ((window as f64) * SHARE) as usize;
        Self::from_tokens(tokens.clamp(FLOOR_TOKENS, CEILING_TOKENS))
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::standard()
    }
}

/// The rendered description of a project.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectContext {
    /// The block injected into the system prompt.
    pub rendered: String,
    /// Which layers made it in.
    pub layers: Vec<Layer>,
    /// A cheap identity for the extracted structure.
    ///
    /// **Used to compare two extractions, not to detect staleness.** It was
    /// introduced so the UI could notice that a project had moved on under a
    /// running session — whose system prompt is frozen — and offer a new
    /// session. That was designed and never built: the app plumbed the value
    /// through three layers and never compared it against anything, so the
    /// mechanism the design described did not exist. The plumbing is gone.
    /// What remains is a genuinely useful equality check, and
    /// `rendering_is_deterministic` is what reads it.
    pub fingerprint: u64,
    /// Populated when extraction partly failed; the context is still usable.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Layout,
    Dependencies,
    Modules,
    Api,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Layout => "layout",
            Layer::Dependencies => "dependencies",
            Layer::Modules => "modules",
            Layer::Api => "public API",
        }
    }
}

impl ProjectContext {
    pub fn char_len(&self) -> usize {
        self.rendered.chars().count()
    }

    pub fn approx_tokens(&self) -> usize {
        self.char_len() / 4
    }

    pub fn includes(&self, layer: Layer) -> bool {
        self.layers.contains(&layer)
    }
}

/// Build the context block for a project.
///
/// `graph` is optional and never built here. The call graph is an explicit,
/// user-triggered index that costs ~10 s and gigabytes of rust-analyzer;
/// opening a session must not pay for one, and this is the call site most
/// likely to make it happen by accident.
/// When present — even a stale one — public-API rows are ordered by fan-in so
/// truncation cuts the least-central symbols. A stale graph yields a stale
/// *ranking*, not a stale *fact*; wrong order is cheap, wrong signatures are
/// not. Without a graph, behaviour is today's source-order round-robin.
pub fn extract(
    project: &Project,
    budget: ContextBudget,
    graph: Option<&crate::callgraph::CallGraph>,
) -> ProjectContext {
    match project.kind {
        ProjectKind::Rust { .. } => extract_rust(project, budget, graph),
        ProjectKind::Generic => extract_generic(project, budget),
    }
}

/// Crates and modules only — the empty-editor backdrop, not the agent prompt.
///
/// The agent context includes the public API and is sized for a model. Putting
/// that wall of `pub struct` / `pub enum` behind the shortcuts made the pane
/// look like a dump and nothing like the navigable call map this project is
/// aiming at. Keep this short and structural.
pub fn outline(project: &Project) -> String {
    match project.kind {
        ProjectKind::Rust { .. } => outline_rust(project),
        ProjectKind::Generic => {
            let mut out = extract(project, ContextBudget { max_chars: 1_200 }, None).rendered;
            if let Some(idx) = out.find("\n## Public API") {
                out.truncate(idx);
            }
            out.push_str(
                "\n\n— project outline —\nThe navigable call map (who calls whom) is not in this pane yet.",
            );
            out
        }
    }
}

fn outline_rust(project: &Project) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let crates = match rust::crates(&project.root) {
        Ok(c) => c,
        Err(_) => {
            return format!(
                "# {}\n\n(could not read cargo metadata)\n\n— project outline —\nThe navigable call map is not in this pane yet.",
                project.name
            );
        }
    };

    let _ = writeln!(out, "# {} ({})", project.name, project.kind.label());
    let _ = writeln!(out, "\n## Crates");
    for c in &crates {
        let path = if c.path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            c.path.display().to_string()
        };
        let _ = writeln!(
            out,
            "- {} v{} ({}) — {}",
            c.name,
            c.version,
            c.targets.join("+"),
            path
        );
    }
    out.push_str(&render_modules(&crates));
    out.push_str("\n— project outline —\nCall map: Agent → Build Call Graph (~10 s, ~2 GB).");
    out
}

fn extract_rust(
    project: &Project,
    budget: ContextBudget,
    graph: Option<&crate::callgraph::CallGraph>,
) -> ProjectContext {
    let mut warnings = Vec::new();
    let crates = match rust::crates(&project.root) {
        Ok(c) => c,
        Err(e) => {
            // A Rust project whose metadata will not read is still workable —
            // fall back rather than leaving the agent with nothing.
            warnings.push(format!(
                "could not read cargo metadata ({e}); falling back to a file listing"
            ));
            let mut context = extract_generic(project, budget);
            context.warnings = warnings;
            return context;
        }
    };

    let mut out = String::new();
    let mut layers = Vec::new();

    // --- Layer 1: layout. Never dropped. ---
    let _ = writeln!(
        out,
        "# Project: {} ({})",
        project.name,
        project.kind.label()
    );
    let _ = writeln!(out, "Root: {}", project.root.display());
    let _ = writeln!(out, "\n## Crates");
    for c in &crates {
        let path = if c.path.as_os_str().is_empty() {
            ".".to_string()
        } else {
            c.path.display().to_string()
        };
        let _ = writeln!(
            out,
            "- {} v{} ({}) — {} [edition {}]",
            c.name,
            c.version,
            c.targets.join("+"),
            path,
            c.edition
        );
    }
    layers.push(Layer::Layout);

    // --- Layer 2: dependencies. ---
    let deps_section = render_dependencies(&crates);
    if out.chars().count() + deps_section.chars().count() <= budget.max_chars {
        out.push_str(&deps_section);
        layers.push(Layer::Dependencies);
    }

    // --- Layer 3: modules. ---
    let modules_section = render_modules(&crates);
    if out.chars().count() + modules_section.chars().count() <= budget.max_chars {
        out.push_str(&modules_section);
        layers.push(Layer::Modules);
    }

    // --- Layer 4: public API, truncated to whatever room is left. ---
    //
    // The truncation notice is reserved *up front* rather than appended after.
    // Appending it unbudgeted is how this overran its ceiling the first time:
    // the section fitted exactly, and then ~140 characters of explanation
    // pushed the whole context over.
    let remaining = budget.max_chars.saturating_sub(out.chars().count());
    if remaining > 200 + TRUNCATION_NOTICE.len() {
        let api_budget = remaining - TRUNCATION_NOTICE.len();
        let (api_section, truncated) = render_api(&crates, api_budget, graph);
        if !api_section.is_empty() {
            out.push_str(&api_section);
            layers.push(Layer::Api);
            if truncated {
                out.push_str(TRUNCATION_NOTICE);
            }
        }
    }

    let fingerprint = fingerprint_of(&crates);
    ProjectContext {
        rendered: out,
        layers,
        fingerprint,
        warnings,
    }
}

fn render_dependencies(crates: &[rust::Crate]) -> String {
    let mut out = String::from("\n## Dependencies (direct)\n");
    let mut any = false;
    for c in crates {
        if c.dependencies.is_empty() {
            continue;
        }
        any = true;
        let deps = c
            .dependencies
            .iter()
            .map(|(name, req)| format!("{name} {req}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "- {}: {}", c.name, deps);
    }
    if any {
        out
    } else {
        String::new()
    }
}

fn render_modules(crates: &[rust::Crate]) -> String {
    let mut out = String::from("\n## Modules\n");
    let mut any = false;
    for c in crates {
        if c.modules.is_empty() {
            continue;
        }
        any = true;
        let _ = writeln!(out, "- {}: {}", c.name, c.modules.join(", "));
    }
    if any {
        out
    } else {
        String::new()
    }
}

/// How many of the highest-degree items get a doc first-line instead of a
/// full signature. Past this, signatures stay — the map needs *names* more
/// than prose once you leave the centre.
const TOP_DOC_BY_DEGREE: usize = 50;

/// Argument-parsing helpers that used to sit in the API block at equal weight
/// with `run_turn`. They are not a map of the project.
fn is_api_noise(item: &rust::ApiItem) -> bool {
    matches!(
        rust::api_item_name(&item.signature),
        Some("arg_str" | "arg_str_opt" | "arg_i64" | "arg_bool")
    )
}

/// Fan-in of the best-matching call-graph node for this API item, or 0.
///
/// Matching is by bare name, preferring a node whose container equals the
/// item's module. Unmatched items (structs, rarely-called free fns) sort to
/// the bottom and are what truncation cuts first.
fn fan_in(graph: &crate::callgraph::CallGraph, item: &rust::ApiItem) -> usize {
    let Some(name) = rust::api_item_name(&item.signature) else {
        return 0;
    };
    let candidates = graph.find(name);
    if candidates.is_empty() {
        return 0;
    }
    candidates
        .into_iter()
        .map(|id| {
            let node = &graph.nodes[id as usize];
            let container = node.container.as_deref().unwrap_or("");
            let module_hit = item.module.is_empty() && container.is_empty()
                || !item.module.is_empty()
                    && (container == item.module
                        || container.ends_with(&item.module)
                        || item.module.ends_with(container));
            (module_hit as usize, graph.callers(id).len())
        })
        .max()
        .map(|(_, degree)| degree)
        .unwrap_or(0)
}

fn format_api_line(item: &rust::ApiItem, use_doc: bool) -> String {
    let prefix = if item.module.is_empty() {
        String::new()
    } else {
        format!("{}::", item.module)
    };
    if use_doc {
        if let (Some(name), Some(doc)) = (
            rust::api_item_name(&item.signature),
            item.doc_line.as_deref(),
        ) {
            return format!("  {prefix}{name} — {doc}\n");
        }
    }
    format!("  {prefix}{}\n", item.signature)
}

/// Render public API signatures, stopping at `max_chars`.
///
/// With a call graph: order by fan-in (highest first), truncate from the
/// bottom of that order, and give the top [`TOP_DOC_BY_DEGREE`] a doc line
/// when one exists. Without a graph: round-robin across crates in source
/// order — today's behaviour — so opening a session never requires an index.
fn render_api(
    crates: &[rust::Crate],
    max_chars: usize,
    graph: Option<&crate::callgraph::CallGraph>,
) -> (String, bool) {
    match graph {
        Some(graph) => render_api_ranked(crates, max_chars, graph),
        None => render_api_round_robin(crates, max_chars),
    }
}

fn render_api_ranked(
    crates: &[rust::Crate],
    max_chars: usize,
    graph: &crate::callgraph::CallGraph,
) -> (String, bool) {
    let mut ranked: Vec<(&str, &rust::ApiItem, usize)> = Vec::new();
    for c in crates {
        for item in &c.api {
            if is_api_noise(item) {
                continue;
            }
            let degree = fan_in(graph, item);
            ranked.push((c.name.as_str(), item, degree));
        }
    }
    // Highest fan-in first; stable ties on crate/module/signature so the
    // prompt bytes do not flicker between identical graphs.
    ranked.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(a.0.cmp(b.0))
            .then(a.1.module.cmp(&b.1.module))
            .then(a.1.signature.cmp(&b.1.signature))
    });

    // Select in global rank order so truncation cuts the least-central
    // symbols. Display later regroups under one ### per crate — putting the
    // crate on every line burned ~750 tokens of the budget on prefixes.
    let mut selected: Vec<(&str, &rust::ApiItem, bool)> = Vec::new();
    let mut truncated = false;
    let mut used = "\n## Public API\n".chars().count();
    // Reserve a cheap upper bound for headers we will emit once per crate.
    let mut header_budget: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (i, (crate_name, item, _degree)) in ranked.iter().enumerate() {
        let use_doc = i < TOP_DOC_BY_DEGREE;
        let line = format_api_line(item, use_doc);
        let header_cost = if header_budget.contains(crate_name) {
            0
        } else {
            crate_name.len() + 6 // "### name\n"
        };
        if used + header_cost + line.chars().count() > max_chars {
            truncated = true;
            break;
        }
        used += header_cost + line.chars().count();
        header_budget.insert(crate_name);
        selected.push((crate_name, item, use_doc));
    }

    if selected.is_empty() {
        return (String::new(), false);
    }

    // Crate order = first appearance in rank order (highest-degree item wins).
    let mut crate_order: Vec<&str> = Vec::new();
    for (crate_name, _, _) in &selected {
        if !crate_order.contains(crate_name) {
            crate_order.push(crate_name);
        }
    }

    let mut out = String::from("\n## Public API\n");
    for name in crate_order {
        let _ = writeln!(out, "### {name}");
        for (crate_name, item, use_doc) in &selected {
            if crate_name == &name {
                out.push_str(&format_api_line(item, *use_doc));
            }
        }
    }

    (out, truncated)
}

fn render_api_round_robin(crates: &[rust::Crate], max_chars: usize) -> (String, bool) {
    let mut out = String::from("\n## Public API\n");
    let mut truncated = false;

    // Round-robin across crates rather than filling one crate at a time, so a
    // tight budget yields a shallow view of everything instead of a deep view
    // of whichever crate happened to sort first.
    let mut queues: Vec<(&str, std::vec::IntoIter<&rust::ApiItem>)> = crates
        .iter()
        .filter_map(|c| {
            let items: Vec<&rust::ApiItem> = c.api.iter().filter(|i| !is_api_noise(i)).collect();
            if items.is_empty() {
                None
            } else {
                Some((c.name.as_str(), items.into_iter()))
            }
        })
        .collect();

    if queues.is_empty() {
        return (String::new(), false);
    }

    let mut emitted_headers: Vec<&str> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();

    'outer: loop {
        let mut progressed = false;
        for (name, queue) in queues.iter_mut() {
            let Some(item) = queue.next() else { continue };
            progressed = true;

            let line = format_api_line(item, false);
            let header_cost = if emitted_headers.contains(name) {
                0
            } else {
                name.len() + 6
            };
            let projected =
                out.chars().count() + pending_len(&pending) + line.chars().count() + header_cost;
            if projected > max_chars {
                truncated = true;
                break 'outer;
            }
            if !emitted_headers.contains(name) {
                emitted_headers.push(name);
            }
            pending.push((name.to_string(), line));
        }
        if !progressed {
            break;
        }
    }

    for name in &emitted_headers {
        let _ = writeln!(out, "### {name}");
        for (owner, line) in &pending {
            if owner == name {
                out.push_str(line);
            }
        }
    }

    (out, truncated)
}

fn pending_len(pending: &[(String, String)]) -> usize {
    pending.iter().map(|(_, l)| l.chars().count()).sum()
}

/// Fallback for non-Cargo projects: a bounded top-level file listing.
fn extract_generic(project: &Project, budget: ContextBudget) -> ProjectContext {
    let mut out = String::new();
    let _ = writeln!(out, "# Project: {}", project.name);
    let _ = writeln!(out, "Root: {}", project.root.display());
    let _ = writeln!(out, "\n## Top-level contents");

    let mut entries: Vec<String> = std::fs::read_dir(&project.root)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();

    for entry in entries {
        if out.chars().count() + entry.len() + 3 > budget.max_chars {
            out.push_str("… (listing truncated)\n");
            break;
        }
        let _ = writeln!(out, "- {entry}");
    }

    ProjectContext {
        rendered: out,
        layers: vec![Layer::Layout],
        fingerprint: fingerprint_str(&project.root.display().to_string()),
        warnings: Vec::new(),
    }
}

/// A stable hash of the extracted structure.
///
/// Hand-rolled FNV-1a rather than `DefaultHasher`: `RandomState` is seeded per
/// process, so the same project would fingerprint differently on every launch
/// and stale-context detection would fire constantly.
fn fingerprint_of(crates: &[rust::Crate]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };
    for c in crates {
        feed(c.name.as_bytes());
        feed(c.version.as_bytes());
        feed(c.edition.as_bytes());
        for m in &c.modules {
            feed(m.as_bytes());
        }
        for (name, req) in &c.dependencies {
            feed(name.as_bytes());
            feed(req.as_bytes());
        }
        for item in &c.api {
            feed(item.signature.as_bytes());
        }
    }
    hash
}

fn fingerprint_str(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod window_budget_tests {
    use super::*;

    /// The regression: a flat 6k block regardless of window, so a million-token
    /// model was handed the same map as a 32k one.
    #[test]
    fn a_larger_window_earns_a_larger_block() {
        let small = ContextBudget::for_window(Some(32_768)).max_chars;
        let large = ContextBudget::for_window(Some(1_000_000)).max_chars;
        assert!(large > small, "1M got {large}, 32k got {small}");
    }

    /// The floor: below it the API layer starts being dropped and the block
    /// stops being a usable map, so a small window keeps the old standard.
    #[test]
    fn a_small_window_never_drops_below_the_standard_block() {
        for window in [4_096, 8_192, 32_768, 100_000] {
            assert_eq!(
                ContextBudget::for_window(Some(window)).max_chars,
                ContextBudget::standard().max_chars,
                "{window} should keep the standard floor"
            );
        }
    }

    /// The ceiling: this block is prefilled on every request of the session, so
    /// it must not scale without bound.
    #[test]
    fn a_huge_window_is_capped() {
        let capped = ContextBudget::for_window(Some(100_000_000)).max_chars;
        assert_eq!(capped, ContextBudget::from_tokens(40_000).max_chars);
    }

    #[test]
    fn five_percent_of_the_window_is_what_lands_between_the_bounds() {
        // 400k window → 20k tokens, comfortably inside both bounds.
        assert_eq!(
            ContextBudget::for_window(Some(400_000)).max_chars,
            ContextBudget::from_tokens(20_000).max_chars
        );
    }

    /// An endpoint that reports no window must not produce a zero-size budget,
    /// which would drop every layer including the layout.
    #[test]
    fn an_unknown_or_nonsense_window_falls_back_to_the_standard() {
        for window in [None, Some(0), Some(-1)] {
            assert_eq!(
                ContextBudget::for_window(window).max_chars,
                ContextBudget::standard().max_chars,
                "{window:?} must fall back rather than produce an empty context"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Project;

    fn our_workspace() -> Project {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        Project::open(root).unwrap()
    }

    #[test]
    fn a_rust_workspace_renders_its_layout() {
        let context = our_workspace().context(ContextBudget::standard());
        assert!(context.includes(Layer::Layout));
        assert!(context.rendered.contains("## Crates"));
        assert!(context.rendered.contains("smithy-project"));
        assert!(context.rendered.contains("edition 2021"));
    }

    #[test]
    fn a_standard_budget_fits_the_useful_layers() {
        let context = our_workspace().context(ContextBudget::standard());
        assert!(context.includes(Layer::Dependencies));
        assert!(context.includes(Layer::Modules));
        assert!(
            context.approx_tokens() <= 6_100,
            "context overran its budget: {} tokens",
            context.approx_tokens()
        );
    }

    /// Layout must survive any budget, because without the map nothing else is
    /// actionable.
    #[test]
    fn layout_survives_a_tiny_budget() {
        let context = our_workspace().context(ContextBudget { max_chars: 400 });
        assert!(context.includes(Layer::Layout));
        assert!(!context.includes(Layer::Api));
        assert!(context.rendered.contains("## Crates"));
    }

    /// The empty-editor outline must not dump the public API — that is what
    /// made the pane look like a paste rather than a map.
    #[test]
    fn the_outline_skips_the_public_api() {
        let text = our_workspace().outline();
        assert!(text.contains("## Crates"), "{text}");
        assert!(text.contains("## Modules"), "{text}");
        assert!(
            !text.contains("## Public API"),
            "outline leaked the API wall:\n{text}"
        );
        assert!(text.contains("Call map"), "{text}");
        assert!(text.contains("Build Call Graph"), "{text}");
    }

    /// Layers drop from the bottom, so a smaller budget is a subset.
    #[test]
    fn layers_drop_from_the_least_important_end() {
        let project = our_workspace();
        let big = project.context(ContextBudget::standard());
        let small = project.context(ContextBudget::compact());
        for layer in &small.layers {
            assert!(
                big.layers.contains(layer),
                "a smaller budget produced a layer the larger one lacks: {layer:?}"
            );
        }
        assert!(small.layers.len() <= big.layers.len());
    }

    /// Silent truncation would teach the model that missing means absent.
    #[test]
    fn truncation_is_announced_in_the_text() {
        let project = our_workspace();
        let context = project.context(ContextBudget { max_chars: 3_000 });
        if context.includes(Layer::Api) {
            assert!(
                context.rendered.contains("truncated"),
                "a truncated API list must say so"
            );
        }
    }

    /// The real contract: the budget is honoured for every *optional* layer.
    /// Layout is unconditional — a project with more crates than the budget can
    /// describe still gets its map, because a context without one is not merely
    /// smaller, it is unusable. So the invariant is "within budget, unless only
    /// layout was emitted".
    #[test]
    fn optional_layers_always_respect_the_budget() {
        let project = our_workspace();
        for max_chars in [500, 1_000, 4_000, 24_000] {
            let context = project.context(ContextBudget { max_chars });
            let layout_only = context.layers == vec![Layer::Layout];
            assert!(
                context.char_len() <= max_chars + 200 || layout_only,
                "budget {max_chars} produced {} chars across layers {:?}",
                context.char_len(),
                context.layers
            );
        }
    }

    /// ...and when layout alone overruns, nothing optional is added on top.
    #[test]
    fn a_budget_smaller_than_the_layout_adds_nothing_else() {
        let context = our_workspace().context(ContextBudget { max_chars: 200 });
        assert_eq!(context.layers, vec![Layer::Layout]);
        assert!(!context.rendered.contains("## Public API"));
        assert!(!context.rendered.contains("## Modules"));
    }

    /// The system prompt must be byte-stable, so extraction must be too.
    #[test]
    fn rendering_is_deterministic() {
        let project = our_workspace();
        let first = project.context(ContextBudget::standard());
        for _ in 0..3 {
            let again = project.context(ContextBudget::standard());
            assert_eq!(
                first.rendered, again.rendered,
                "rendered context must be stable"
            );
            assert_eq!(first.fingerprint, again.fingerprint);
        }
    }

    /// `DefaultHasher` would change every launch and make stale-detection
    /// useless; this asserts we are not using it.
    #[test]
    fn the_fingerprint_is_stable_across_processes() {
        assert_eq!(fingerprint_str("smithy"), fingerprint_str("smithy"));
        assert_ne!(fingerprint_str("smithy"), fingerprint_str("smithy2"));
        // Known FNV-1a value, so a change of algorithm is caught rather than
        // silently invalidating every stored fingerprint.
        assert_eq!(fingerprint_str(""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn a_generic_project_gets_a_file_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.md"), "").unwrap();
        std::fs::create_dir(tmp.path().join("data")).unwrap();

        let context = Project::open(tmp.path())
            .unwrap()
            .context(ContextBudget::standard());
        assert!(context.rendered.contains("notes.md"));
        assert!(context.rendered.contains("data/"));
        assert_eq!(context.layers, vec![Layer::Layout]);
    }

    /// A broken manifest should degrade to a listing, with the reason recorded,
    /// rather than leaving the agent with no context at all.
    #[test]
    fn a_broken_manifest_falls_back_with_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "this is not valid toml {{{").unwrap();
        std::fs::write(tmp.path().join("stray.rs"), "").unwrap();

        let context = Project::open(tmp.path())
            .unwrap()
            .context(ContextBudget::standard());
        assert!(
            !context.warnings.is_empty(),
            "the failure should be reported"
        );
        assert!(
            context.rendered.contains("stray.rs"),
            "should still list files"
        );
    }

    #[test]
    fn budget_conversion_is_four_chars_per_token() {
        assert_eq!(ContextBudget::from_tokens(1_000).max_chars, 4_000);
        assert!(ContextBudget::compact().max_chars < ContextBudget::standard().max_chars);
    }

    /// With a graph, higher fan-in wins the budget; without one, source order
    /// is unchanged. Noise helpers never appear either way.
    #[test]
    fn api_layer_ranks_by_fan_in_when_a_graph_is_present() {
        use crate::callgraph::{CallGraph, Edge, Node};

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            r#"
/// Build the context block for a project.
pub fn extract() {}
pub fn arg_bool() {}
pub fn peripheral() {}
"#,
        )
        .unwrap();

        let mut graph = CallGraph::default();
        graph.nodes.push(Node {
            name: "extract".into(),
            container: None,
            file: "src/lib.rs".into(),
            line: 1,
            end_line: 1,
        });
        graph.nodes.push(Node {
            name: "peripheral".into(),
            container: None,
            file: "src/lib.rs".into(),
            line: 2,
            end_line: 2,
        });
        // Callers at indices 2, 3, 4 — three edges into extract, none into peripheral.
        for i in 2..5 {
            graph.nodes.push(Node {
                name: format!("caller{i}"),
                container: None,
                file: "src/lib.rs".into(),
                line: 10 + i,
                end_line: 10 + i,
            });
            graph.edges.push(Edge {
                from: i as u32,
                to: 0,
                sites: 1,
            });
        }

        let project = crate::Project::discover(tmp.path()).unwrap();
        let ranked = project.context_with_graph(ContextBudget { max_chars: 2_000 }, Some(&graph));
        let api = ranked.rendered.split("## Public API").nth(1).unwrap_or("");
        assert!(
            api.find("extract").unwrap() < api.find("peripheral").unwrap(),
            "higher fan-in must sort first:\n{api}"
        );
        assert!(
            api.contains("extract — Build the context block"),
            "top-ranked items use the doc line:\n{api}"
        );
        assert!(
            !api.contains("arg_bool"),
            "argument helpers are noise:\n{api}"
        );
    }
}
