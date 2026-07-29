//! Print the context block a project would inject.
//!
//!     cargo run -p smithy-project --example dump [PATH]
use smithy_project::{ContextBudget, Project};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let project = Project::discover(&path).expect("open project");
    let context = project.context(ContextBudget::standard());

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
