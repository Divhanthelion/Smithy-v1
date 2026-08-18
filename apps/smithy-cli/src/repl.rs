//! Read a line, run a Turn, print what happened.

use std::io::{self, IsTerminal, Read, Write};

use smithy_agent::{
    handoff_injection, list_skills, load_skill, parse_command, unused_harness_files, Outcome,
    Session, TurnEvent,
};
use smithy_project::Project;

use crate::boot::{self, Booted};

pub struct Repl {
    pub project: Project,
    pub booted: Booted,
    pub session_id: String,
}

impl Repl {
    pub async fn start(project: Project, yolo: bool) -> Result<Self, String> {
        let booted = boot::boot(&project, yolo, None).await?;
        Ok(Self {
            project,
            booted,
            session_id: boot::new_session_id(),
        })
    }

    fn banner(&self) {
        let yolo = self
            .booted
            .auto_approve
            .load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "smithy-agent  {}  {}  {}",
            self.project.root.display(),
            self.booted.model_label,
            if yolo { "YOLO" } else { "reviewed" }
        );
        eprintln!("  {}", self.booted.context_summary);
        for n in &self.booted.notices {
            eprintln!("  {n}");
        }
        eprintln!("  /help for commands. Ctrl-C stops a Turn.\n");
    }

    pub async fn run(mut self) -> Result<(), String> {
        self.banner();
        loop {
            eprint!("{}> ", self.project.name);
            let _ = io::stderr().flush();
            let mut line = String::new();
            let n = io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if self.dispatch(line).await? {
                break;
            }
        }
        Ok(())
    }

    pub async fn one_shot(&mut self, task: &str) -> Result<(), String> {
        self.turn(task).await
    }

    /// `true` means the process should exit.
    async fn dispatch(&mut self, line: &str) -> Result<bool, String> {
        if let Some(cmd) = parse_command(line) {
            match cmd.name.as_str() {
                "quit" | "exit" => return Ok(true),
                "help" => {
                    eprint!("{}", crate::args::usage());
                    return Ok(false);
                }
                "inspect" => {
                    self.inspect();
                    return Ok(false);
                }
                "prompt" => {
                    self.dump_prompt();
                    return Ok(false);
                }
                "request" => {
                    self.dump_request();
                    return Ok(false);
                }
                "skills" => {
                    self.list_skills();
                    return Ok(false);
                }
                "new" => {
                    let yolo = self
                        .booted
                        .auto_approve
                        .load(std::sync::atomic::Ordering::Relaxed);
                    self.booted = boot::boot(&self.project, yolo, None).await?;
                    self.session_id = boot::new_session_id();
                    self.banner();
                    return Ok(false);
                }
                "yolo" => {
                    self.booted
                        .auto_approve
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "YOLO — in-Project writes skip Review; bash that stays down in the tree skips the prompt."
                    );
                    return Ok(false);
                }
                "reviewed" => {
                    self.booted
                        .auto_approve
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    eprintln!("reviewed — writes and bash wait.");
                    return Ok(false);
                }
                "compact" => {
                    self.compact(&cmd.rest).await?;
                    return Ok(false);
                }
                "handoff" => {
                    let body = match load_skill(&self.project.root, "handoff") {
                        Some(skill) => skill.injection(),
                        None => handoff_injection(&cmd.rest),
                    };
                    let task = if cmd.rest.is_empty() {
                        body
                    } else {
                        format!("{body}\n\n{}", cmd.rest)
                    };
                    self.turn(&task).await?;
                    return Ok(false);
                }
                "which" | "which-harness" => {
                    let (source, _) = smithy_agent::resolve_system_template(&self.project.root);
                    eprintln!("{}", source.label());
                    return Ok(false);
                }
                name => {
                    let Some(skill) = load_skill(&self.project.root, name) else {
                        eprintln!("No skill named `{name}`.");
                        return Ok(false);
                    };
                    self.booted.session.set_skill(Some(skill.meta.name.clone()));
                    let task = if cmd.rest.is_empty() {
                        skill.injection()
                    } else {
                        format!("{}\n\n{}", skill.injection(), cmd.rest)
                    };
                    self.turn(&task).await?;
                    return Ok(false);
                }
            }
        }
        self.turn(line).await?;
        Ok(false)
    }

    async fn compact(&mut self, focus: &str) -> Result<(), String> {
        match self.booted.session.compact(focus, Some(&sink)).await {
            Ok(summary) => {
                eprintln!("compacted.\n{summary}");
                self.save();
            }
            Err(e) => eprintln!("{e}"),
        }
        Ok(())
    }

    async fn turn(&mut self, task: &str) -> Result<(), String> {
        let result = run_turn(&mut self.booted.session, task).await;
        self.save();
        match result {
            Ok(Outcome::Answer(a)) => {
                if !a.trim().is_empty() {
                    println!("{a}");
                }
            }
            Ok(Outcome::Stopped(r)) => eprintln!("stopped: {r}"),
            Err(e) => eprintln!("{e}"),
        }
        Ok(())
    }

    fn save(&self) {
        if let Err(e) = boot::save_session(
            &self.project,
            &self.booted.session,
            &self.booted.model_label,
            &self.session_id,
        ) {
            eprintln!("could not save session: {e}");
        }
    }

    fn inspect(&self) {
        let h = &self.booted.harness;
        eprintln!("Harness: {}", h.source.label());
        if let Some(path) = &h.manifest {
            eprintln!("  manifest {}", path.display());
        }
        if h.includes.is_empty() {
            eprintln!(
                "  includes (none) — files in the harness directory are not sent unless listed"
            );
        } else {
            for inc in &h.includes {
                eprintln!(
                    "  include  {:>6} chars  {} ({})",
                    inc.body.len(),
                    inc.name,
                    inc.path.display()
                );
            }
        }
        for path in unused_harness_files(&self.project.root, h) {
            let chars = std::fs::read_to_string(&path).map(|t| t.len()).unwrap_or(0);
            eprintln!(
                "  unused   {:>6} chars  {} — not in harness.toml, not sent",
                chars,
                path.display()
            );
        }
        let ledger = self.booted.session.ledger();
        eprintln!("This Session will send:");
        for seg in &ledger.segments {
            let frozen = if seg.frozen { "frozen" } else { "grows" };
            eprintln!(
                "  {:<18} {:>7} chars  ~{:>6} tok  {frozen}",
                seg.name, seg.chars, seg.tokens
            );
        }
        eprintln!(
            "  {:<18} {:>7} chars  ~{:>6} tok  estimate",
            "total",
            ledger.segments.iter().map(|s| s.chars).sum::<usize>(),
            ledger.estimate_tokens
        );
        if ledger.prompt_tokens > 0 {
            eprintln!(
                "  last billed prompt {} tok ({} cached)",
                ledger.prompt_tokens, ledger.cached_tokens
            );
        } else {
            eprintln!("  no completion yet — token column is chars/4 until the provider reports");
        }
        for n in &h.notices {
            eprintln!("  notice: {n}");
        }
    }

    fn dump_prompt(&self) {
        for (name, text) in self.booted.session.inspect_segments() {
            eprintln!("----- {name} ({} chars) -----", text.len());
            println!("{text}");
        }
    }

    fn dump_request(&self) {
        match self.booted.session.last_request_json() {
            Some(json) => println!("{json}"),
            None => eprintln!("no completion yet — nothing has been POSTed"),
        }
    }

    fn list_skills(&self) {
        eprintln!("Skills in this Project (type /name to inject this Turn):");
        for skill in list_skills(&self.project.root) {
            let hint = if skill.argument_hint.is_empty() {
                String::new()
            } else {
                format!("  {}", skill.argument_hint)
            };
            eprintln!("  /{}{hint}", skill.name);
            if !skill.description.is_empty() {
                eprintln!("      {}", skill.description);
            }
        }
        for cmd in smithy_agent::harness_commands() {
            eprintln!("  /{}  {}", cmd.name, cmd.description);
        }
    }
}

fn sink(event: TurnEvent) {
    match event {
        TurnEvent::Content(t) => {
            print!("{t}");
            let _ = io::stdout().flush();
        }
        TurnEvent::Reasoning(t) => {
            eprint!("{t}");
            let _ = io::stderr().flush();
        }
        TurnEvent::ToolStarted {
            step,
            name,
            arguments,
            ..
        } => {
            let args = trim_args(&arguments);
            eprintln!("  [{step}] {name} {args}");
        }
        TurnEvent::ToolFinished {
            content, is_error, ..
        } => {
            let first = content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect::<String>();
            eprintln!("      {} {first}", if is_error { "✗" } else { "→" });
        }
        TurnEvent::Warning(w) => eprintln!("  ⚠ {w}"),
    }
}

fn trim_args(arguments: &str) -> String {
    let one: String = arguments.chars().take(120).collect();
    if arguments.chars().count() > 120 {
        format!("{one}…")
    } else {
        one
    }
}

async fn run_turn(
    session: &mut Session,
    task: &str,
) -> Result<Outcome, smithy_agent::ProviderError> {
    let stopper = session.stopper();
    let run = session.run_turn(task, Some(&sink));
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        _ = tokio::signal::ctrl_c() => {
            stopper.stop();
            eprintln!("\nstopping…");
            run.await
        }
    }
}

pub fn stdin_is_a_terminal() -> bool {
    io::stdin().is_terminal()
}

pub fn read_piped_task() -> Result<String, String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}
