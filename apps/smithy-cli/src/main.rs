//! Terminal Session. Same agent loop as the editor; the Harness is files.

mod args;
mod boot;
mod hooks;
mod repl;

use smithy_agent::{init_project_harness, install_bundled_user_skills, load_harness};
use smithy_project::Project;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", args::usage());
        return Ok(());
    }

    let args = args::parse(raw)?;
    let project = Project::discover(&args.project)
        .or_else(|_| Project::open(&args.project))
        .map_err(|e| e.to_string())?;

    if args.which_harness {
        let h = load_harness(&project.root);
        println!("{}", h.source.label());
        if let Some(path) = &h.manifest {
            println!("manifest {}", path.display());
        }
        if h.includes.is_empty() {
            println!("includes (none)");
        }
        for inc in &h.includes {
            println!("include {} ({} chars)", inc.name, inc.body.len());
        }
        for n in &h.notices {
            eprintln!("{n}");
        }
        return Ok(());
    }
    if args.init_harness {
        let path = init_project_harness(&project.root)?;
        println!("wrote {}", path.display());
        println!("edit it, then /new (or restart) so the next Session loads it.");
        return Ok(());
    }

    install_bundled_user_skills();

    let mut repl = repl::Repl::start(project, args.yolo).await?;
    if let Some(task) = args.message {
        repl.one_shot(&task).await
    } else if !repl::stdin_is_a_terminal() {
        let task = repl::read_piped_task()?;
        if task.trim().is_empty() {
            return Err("stdin is empty — pass -m TEXT or type in the REPL".into());
        }
        repl.one_shot(task.trim()).await
    } else {
        repl.run().await
    }
}
