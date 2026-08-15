//! Read a stored session back out.
//!
//!     cargo run -p smithy-agent --example transcript -- list [PROJECT]
//!     cargo run -p smithy-agent --example transcript -- show <SESSION-FILE> [--reasoning]
//!     cargo run -p smithy-agent --example transcript -- md   <SESSION-FILE> > session.md
//!
//! Sessions have always been written to disk in full; nothing could read them
//! back. This is that. It exists as an example rather than a subcommand because
//! the editor is a GUI and reaching for `jq` on a 300 KB JSON file to find out
//! what an agent did is not a workflow.
//!
//! `--reasoning` is the reason this was written. The model's thinking used to be
//! shown live and then discarded; it is now stored beside the transcript (never
//! inside it — see [`smithy_agent::persist`]) and this is how you get it back.

use std::path::{Path, PathBuf};

use smithy_agent::message::Role;
use smithy_agent::persist::StoredSession;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("list");

    match command {
        "list" => list(args.get(1).map(String::as_str)),
        "show" => match args.get(1) {
            Some(path) => show(Path::new(path), args.iter().any(|a| a == "--reasoning")),
            None => usage(),
        },
        "md" => match args.get(1) {
            Some(path) => markdown(Path::new(path)),
            None => usage(),
        },
        _ => usage(),
    }
}

fn usage() {
    eprintln!(
        "usage:\n  \
         transcript list [PROJECT-SUBSTRING]\n  \
         transcript show <SESSION-FILE> [--reasoning]\n  \
         transcript md   <SESSION-FILE>"
    );
    std::process::exit(2);
}

/// `~/.local/share/smithy/projects`.
fn projects_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/smithy/projects")
}

fn list(filter: Option<&str>) {
    let root = projects_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("no session store at {}", root.display());
        return;
    };

    let mut rows: Vec<(u64, String, PathBuf, usize, usize)> = Vec::new();
    for project in entries.flatten() {
        let name = project.file_name().to_string_lossy().to_string();
        if filter.is_some_and(|f| !name.to_lowercase().contains(&f.to_lowercase())) {
            continue;
        }
        let sessions = project.path().join("sessions");
        let Ok(files) = std::fs::read_dir(&sessions) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(session) = load(&path) else { continue };
            rows.push((
                session.updated_at,
                name.clone(),
                path,
                session.messages.len(),
                session.reasoning.len(),
            ));
        }
    }

    if rows.is_empty() {
        eprintln!("no sessions found under {}", root.display());
        return;
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));

    println!("{:<38} {:>5} {:>10}  FILE", "PROJECT", "MSGS", "REASONING");
    for (_, project, path, messages, reasoning) in rows {
        let project = if project.chars().count() > 36 {
            format!("{}…", project.chars().take(35).collect::<String>())
        } else {
            project
        };
        println!(
            "{project:<38} {messages:>5} {reasoning:>10}  {}",
            path.display()
        );
    }
}

fn load(path: &Path) -> Option<StoredSession> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn show(path: &Path, with_reasoning: bool) {
    let Some(session) = load(path) else {
        eprintln!("could not read a session from {}", path.display());
        std::process::exit(1);
    };

    eprintln!("── {} ──", session.title);
    eprintln!(
        "model {} · {} messages · {} reasoning blocks · steps {} · context_hard {}",
        session.model,
        session.messages.len(),
        session.reasoning.len(),
        session.limits.max_steps,
        session.limits.context_hard
    );
    if with_reasoning && session.reasoning.is_empty() {
        eprintln!(
            "(no reasoning stored — this session predates reasoning capture, or the model \
             emitted none)"
        );
    }
    eprintln!();

    for (index, message) in session.messages.iter().enumerate() {
        // Reasoning is keyed by how many messages existed when it was emitted,
        // so it slots in ahead of the assistant message it produced.
        if with_reasoning {
            for entry in session
                .reasoning
                .iter()
                .filter(|r| r.after_message == index)
            {
                println!("  ╭─ reasoning (step {})", entry.step);
                for line in entry.text.lines() {
                    println!("  │ {line}");
                }
                println!("  ╰─");
            }
        }

        match message.role {
            Role::System => println!("[system] {} chars", message.content.len()),
            Role::User => println!("\n[user] {}", message.content),
            Role::Assistant => {
                if !message.content.trim().is_empty() {
                    println!("\n[assistant] {}", message.content);
                }
                for call in &message.tool_calls {
                    println!("  → {}({})", call.name, one_line(&call.arguments, 120));
                }
            }
            Role::Tool => {
                let name = message.tool_name.as_deref().unwrap_or("tool");
                println!("    {name}: {}", one_line(&message.content, 160));
            }
        }
    }
}

/// The whole session as Markdown, for keeping or sharing.
fn markdown(path: &Path) {
    let Some(session) = load(path) else {
        eprintln!("could not read a session from {}", path.display());
        std::process::exit(1);
    };

    println!("# {}\n", session.title);
    println!("- **Model:** `{}`", session.model);
    println!("- **Workspace:** `{}`", session.workspace.display());
    println!("- **Messages:** {}", session.messages.len());
    println!("- **Reasoning blocks:** {}", session.reasoning.len());
    println!(
        "- **Budget:** {} steps, {} context\n",
        session.limits.max_steps, session.limits.context_hard
    );

    for (index, message) in session.messages.iter().enumerate() {
        for entry in session
            .reasoning
            .iter()
            .filter(|r| r.after_message == index)
        {
            println!(
                "<details><summary>Reasoning — step {}</summary>\n",
                entry.step
            );
            println!("```\n{}\n```\n", entry.text.trim());
            println!("</details>\n");
        }
        match message.role {
            Role::System => {}
            Role::User => println!("## User\n\n{}\n", message.content),
            Role::Assistant => {
                if !message.content.trim().is_empty() {
                    println!("### Assistant\n\n{}\n", message.content);
                }
                for call in &message.tool_calls {
                    println!("- `{}` — `{}`", call.name, one_line(&call.arguments, 200));
                }
                if !message.tool_calls.is_empty() {
                    println!();
                }
            }
            Role::Tool => {
                let name = message.tool_name.as_deref().unwrap_or("tool");
                println!(
                    "<details><summary>{name} result</summary>\n\n```\n{}\n```\n\n</details>\n",
                    truncate(&message.content, 4000)
                );
            }
        }
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&flat, max)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
