//! Build and query a call graph from the terminal.
//!
//!     cargo run -p smithy-project --example callgraph -- <PROJECT> [SYMBOL]
//!     cargo run -p smithy-project --example callgraph -- <PROJECT> --scip <FILE> [SYMBOL]
//!
//! Runs `rust-analyzer scip` unless handed an existing index, joins it to
//! tree-sitter spans, and prints what resolved and what did not.
//!
//! This exists so the edges are checkable by hand before anything is drawn: an
//! edge you can read here and confirm against the source is an edge worth
//! putting on a screen.

use smithy_project::callgraph::CallGraph;
use smithy_project::scip::ScipIndex;
use smithy_project::symbols::SymbolIndex;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(root) = args.first() else {
        eprintln!("usage: … --example callgraph -- <PROJECT> [--scip FILE] [SYMBOL]");
        std::process::exit(2);
    };
    let root = std::path::Path::new(root);

    // `--scip FILE` reuses an index instead of spending ten seconds and two
    // gigabytes rebuilding one.
    let prebuilt = args
        .iter()
        .position(|a| a == "--scip")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let flag_values: Vec<&String> = ["--scip", "--save", "--load"]
        .iter()
        .filter_map(|f| {
            args.iter()
                .position(|a| a == f)
                .and_then(|i| args.get(i + 1))
        })
        .collect();
    let symbol = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--") && !flag_values.contains(a));

    // `--save FILE` persists; `--load FILE` reads back instead of building.
    let save_to = args
        .iter()
        .position(|a| a == "--save")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let load_from = args
        .iter()
        .position(|a| a == "--load")
        .and_then(|i| args.get(i + 1))
        .cloned();

    if let Some(path) = &load_from {
        let graph = match CallGraph::load(std::path::Path::new(path)) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!(
            "loaded           {} nodes, {} edges",
            graph.nodes.len(),
            graph.edges.len()
        );
        println!("file size        {:.0} KB", bytes as f64 / 1024.0);
        println!("indexed files    {}", graph.sources.len());
        let staleness = graph.staleness(root);
        println!(
            "freshness        {}",
            if staleness.is_stale() {
                staleness.describe()
            } else {
                "current".into()
            }
        );
        for f in staleness.changed.iter().take(5) {
            println!("  changed  {f}");
        }
        for f in staleness.added.iter().take(5) {
            println!("  added    {f}");
        }
        return;
    }

    let started = std::time::Instant::now();
    let graph = match &prebuilt {
        Some(path) => {
            let scip = match ScipIndex::from_file(std::path::Path::new(path)) {
                Ok(index) => index,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            };
            let symbols = SymbolIndex::build(root);
            let mut graph = CallGraph::assemble(&scip, &symbols);
            // Sources come from the *documents the indexer saw*, never from the
            // nodes. A file of `pub mod` declarations or constants produces no
            // nodes but is still analysed, and recording only node files makes
            // every such file look newly added on the next staleness check.
            let analysed: Vec<String> = scip
                .documents
                .iter()
                .map(|d| d.relative_path.clone())
                .collect();
            graph.record_sources(root, &analysed);
            graph
        }
        None => {
            eprintln!("running `rust-analyzer scip` — expect ~10 s and ~2 GB…");
            match CallGraph::build(root) {
                Ok(graph) => graph,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
    };
    let elapsed = started.elapsed();

    if let Some(path) = &save_to {
        match graph.save(std::path::Path::new(path)) {
            Ok(()) => {
                let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                println!("saved            {path} ({:.0} KB)", bytes as f64 / 1024.0);
            }
            Err(e) => eprintln!("{e}"),
        }
    }

    let s = graph.stats;
    println!("nodes            {}", graph.nodes.len());
    println!("edges            {}", graph.edges.len());
    println!("built in         {:.1} s", elapsed.as_secs_f64());
    println!();
    println!("occurrences      {}", s.occurrences);
    println!("  definitions    {}", s.definitions);
    println!("  references     {}", s.references);
    println!();
    println!("of the references:");
    let pct = |n: usize| {
        if s.references == 0 {
            0.0
        } else {
            100.0 * n as f64 / s.references as f64
        }
    };
    println!(
        "  became edges   {:<7} {:.0}%",
        s.edges_kept,
        pct(s.edges_kept)
    );
    println!(
        "  external       {:<7} {:.0}%   (std, deps — deliberately dropped)",
        s.external,
        pct(s.external)
    );
    println!(
        "  locals         {:<7} {:.0}%   (variables/closures, not functions)",
        s.locals,
        pct(s.locals)
    );
    println!(
        "  unattributed   {:<7} {:.0}%   (outside any function)",
        s.unattributed,
        pct(s.unattributed)
    );
    println!("  self-edges     {}", s.self_edges);

    let Some(symbol) = symbol else {
        println!("\npass a symbol name to see its callers and callees");
        return;
    };

    let hits = graph.find(symbol);
    if hits.is_empty() {
        println!("\nno function named `{symbol}` in the graph");
        return;
    }
    for id in hits {
        let node = &graph.nodes[id as usize];
        println!("\n{} — {}", node.qualified(), node.location());

        let callers = graph.callers(id);
        println!("  called by ({}):", callers.len());
        for (n, sites) in callers.iter().take(20) {
            let times = if *sites > 1 {
                format!(" ×{sites}")
            } else {
                String::new()
            };
            println!("    {} — {}{}", n.qualified(), n.location(), times);
        }
        if callers.is_empty() {
            println!("    (nothing — an entry point, or dead)");
        }

        let callees = graph.callees(id);
        println!("  calls ({}):", callees.len());
        for (n, sites) in callees.iter().take(20) {
            let times = if *sites > 1 {
                format!(" ×{sites}")
            } else {
                String::new()
            };
            println!("    {} — {}{}", n.qualified(), n.location(), times);
        }
    }
}
