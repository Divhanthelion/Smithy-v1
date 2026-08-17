//! Print the context block a project would inject.
//!
//!     cargo run -p smithy-project --example dump [PATH]
//!
//! Loads a persisted call graph from the project registry when one exists
//! (`~/.local/share/smithy/projects/<key>/callgraph.json`) and ranks the API
//! layer by fan-in. Never builds a graph — that is deliberate and matches the
//! session-open path.
use smithy_project::callgraph::CallGraph;
use smithy_project::{ContextBudget, Project, ProjectRegistry};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let project = Project::discover(&path).expect("open project");

    let graph = ProjectRegistry::default_location().ok().and_then(|reg| {
        let graph_path = reg.callgraph_path(&project.root);
        match CallGraph::load(&graph_path) {
            Ok(g) => {
                eprintln!(
                    "call graph: {} nodes, {} edges ({})",
                    g.nodes.len(),
                    g.edges.len(),
                    graph_path.display()
                );
                Some(g)
            }
            Err(_) => {
                eprintln!(
                    "call graph: none at {} — API layer stays source-ordered",
                    graph_path.display()
                );
                None
            }
        }
    });

    let context = project.context_with_graph(ContextBudget::standard(), graph.as_ref());

    eprintln!("── {} ({}) ──", project.name, project.kind.label());
    eprintln!(
        "layers: {:?}",
        context.layers.iter().map(|l| l.label()).collect::<Vec<_>>()
    );
    eprintln!(
        "size: {} chars ≈ {} tokens",
        context.char_len(),
        context.approx_tokens()
    );
    for w in &context.warnings {
        eprintln!("warning: {w}");
    }
    eprintln!("────────────────");
    println!("{}", context.rendered);
}
