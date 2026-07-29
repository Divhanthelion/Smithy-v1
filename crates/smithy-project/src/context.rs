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
    "\n[Public API list truncated to fit. Items not shown still exist — use `grep` to find them \
     rather than assuming absence.]\n";

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
    /// mechanism the doc described did not exist. The plumbing is gone; see
    /// HANDOFF §10. What remains is a genuinely useful equality check, and
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
pub fn extract(project: &Project, budget: ContextBudget) -> ProjectContext {
    match project.kind {
        ProjectKind::Rust { .. } => extract_rust(project, budget),
        ProjectKind::Generic => extract_generic(project, budget),
    }
}

fn extract_rust(project: &Project, budget: ContextBudget) -> ProjectContext {
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
        let (api_section, truncated) = render_api(&crates, api_budget);
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

/// Render public API signatures, stopping at `max_chars`.
///
/// Round-robins across crates rather than filling one crate at a time, so a
/// tight budget yields a shallow view of everything instead of a deep view of
/// whichever crate happened to sort first.
fn render_api(crates: &[rust::Crate], max_chars: usize) -> (String, bool) {
    let mut out = String::from("\n## Public API\n");
    let mut truncated = false;

    let mut queues: Vec<(&str, std::vec::IntoIter<&rust::ApiItem>)> = crates
        .iter()
        .filter(|c| !c.api.is_empty())
        .map(|c| {
            (
                c.name.as_str(),
                c.api.iter().collect::<Vec<_>>().into_iter(),
            )
        })
        .collect();

    if queues.is_empty() {
        return (String::new(), false);
    }

    let mut emitted_headers: Vec<&str> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new(); // (crate, line)

    'outer: loop {
        let mut progressed = false;
        for (name, queue) in queues.iter_mut() {
            let Some(item) = queue.next() else { continue };
            progressed = true;

            let prefix = if item.module.is_empty() {
                String::new()
            } else {
                format!("{}::", item.module)
            };
            let line = format!("  {prefix}{}\n", item.signature);

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

    // Group the collected lines under their crate headings.
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
}
