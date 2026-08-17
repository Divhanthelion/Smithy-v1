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

    // `--at path/to/file.rs:42` — which function contains that line?
    //
    // The half of the call graph rust-analyzer does not supply: its SCIP output
    // says an occurrence references definition X, but not which function it sits
    // inside. This is how that is answered, and how it can be checked by hand.
    if query.as_deref() == Some("--at") {
        let Some(spec) = args.next() else {
            eprintln!("usage: … -- <PROJECT> --at <FILE>:<LINE>");
            std::process::exit(2);
        };
        let (file, line) = match spec.rsplit_once(':') {
            Some((f, l)) => (f.to_string(), l.parse::<usize>().unwrap_or(0)),
            None => {
                eprintln!("expected FILE:LINE, got `{spec}`");
                std::process::exit(2);
            }
        };
        match index.enclosing(&file, line) {
            Some(span) => println!(
                "{file}:{line} is inside {} (lines {}–{})",
                span.qualified(),
                span.start_line,
                span.end_line
            ),
            None => println!(
                "{file}:{line} is inside no function — a call here has no caller to \
                 attribute an edge to"
            ),
        }
        eprintln!(
            "({} functions indexed in that file)",
            index.spans_in(&file).len()
        );
        return;
    }

    let Some(query) = query else {
        eprintln!("pass a symbol name to look one up, or --at FILE:LINE");
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
