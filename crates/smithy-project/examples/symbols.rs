//! Query the symbol index from a terminal.
//!
//!     cargo run -p smithy-project --example symbols -- <PROJECT>
//!     cargo run -p smithy-project --example symbols -- <PROJECT> DesktopMsg
//!
//! The same index the agent's `symbol` tool queries. Useful for checking that a
//! project indexes at all, and for seeing what the model would be told.

use smithy_project::symbols::{SymbolIndex, SymbolKind};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let query = args.next();

    let started = std::time::Instant::now();
    let index = SymbolIndex::build(std::path::Path::new(&root));
    let elapsed = started.elapsed();

    eprintln!(
        "indexed {} symbols across {} files in {:.0}ms",
        index.len(),
        index.files(),
        elapsed.as_secs_f64() * 1000.0
    );

    let Some(query) = query else {
        eprintln!("pass a symbol name to look one up");
        return;
    };

    let hits = index.lookup(&query);
    if hits.is_empty() {
        eprintln!("\nno exact match for `{query}` — nearest by substring:");
        for symbol in index.search(&query, 10) {
            println!("  {}", symbol.render());
        }
        return;
    }

    println!("\n{query}:");
    for symbol in hits {
        println!("  {}", symbol.render());
        if symbol.kind == SymbolKind::Enum {
            let variants = index.variants_of(&symbol.name);
            println!("    {} variants:", variants.len());
            for v in variants {
                println!("      {} — {}", v.name, v.signature);
            }
        }
        if symbol.kind == SymbolKind::Struct {
            let methods = index.methods_of(&symbol.name);
            println!("    {} methods", methods.len());
            for m in methods.iter().take(8) {
                println!("      {}", m.signature);
            }
        }
    }
}
