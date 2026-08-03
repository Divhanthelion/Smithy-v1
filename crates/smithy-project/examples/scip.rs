//! Read a `.scip` index and report what is in it.
//!
//!     rust-analyzer scip . --output /tmp/x.scip
//!     cargo run -p smithy-project --example scip -- /tmp/x.scip
//!
//! The acceptance check for the SCIP reader: the counts it prints must match
//! what an independent parse of the same file produces. They were fixed by a
//! Python probe before this reader existed, so agreement is evidence rather than
//! coincidence.

use std::collections::BTreeMap;

use smithy_project::scip::ScipIndex;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: … --example scip -- <FILE.scip> [SYMBOL-SUBSTRING]");
        std::process::exit(2);
    };
    let needle = std::env::args().nth(2);

    let started = std::time::Instant::now();
    let index = match ScipIndex::from_file(std::path::Path::new(&path)) {
        Ok(index) => index,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed();

    let definitions = index
        .documents
        .iter()
        .flat_map(|d| d.occurrences.iter())
        .filter(|o| o.is_definition())
        .count();
    let imports = index
        .documents
        .iter()
        .flat_map(|d| d.occurrences.iter())
        .filter(|o| o.is_import())
        .count();

    println!("documents        {}", index.documents.len());
    println!("occurrences      {}", index.occurrence_count());
    println!("with any role    {}", index.roled_count());
    println!("  definitions    {definitions}");
    println!("  imports        {imports}");
    println!(
        "references       {}",
        index.occurrence_count() - index.roled_count()
    );
    println!("parsed in        {:.0} ms", elapsed.as_secs_f64() * 1000.0);

    if let Some(needle) = needle {
        println!("\noccurrences matching `{needle}`:");
        let mut by_file: BTreeMap<&str, Vec<&smithy_project::scip::Occurrence>> = BTreeMap::new();
        for document in &index.documents {
            for occurrence in &document.occurrences {
                if occurrence.symbol.contains(&needle) {
                    by_file
                        .entry(document.relative_path.as_str())
                        .or_default()
                        .push(occurrence);
                }
            }
        }
        for (file, occurrences) in by_file.iter().take(12) {
            for occurrence in occurrences.iter().take(6) {
                let role = if occurrence.is_definition() {
                    "def"
                } else if occurrence.is_import() {
                    "use"
                } else {
                    "ref"
                };
                println!("  {role}  {file}:{}", occurrence.line);
            }
        }
        let total: usize = by_file.values().map(Vec::len).sum();
        println!("  ({total} total across {} files)", by_file.len());
    }
}
