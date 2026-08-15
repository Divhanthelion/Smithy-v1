//! Live end-to-end smoke test against a running LM Studio.
//!
//! Every source project validated its agent by anecdote — coda's post-mortem is
//! explicit that it "has never touched a real repository, a large file, a
//! genuinely hard task, or adversarial input". This is the smallest honest
//! check that the whole stack works together: provider → parse → loop → tool
//! dispatch → sandbox → answer.
//!
//! It builds a throwaway workspace, asks a question only a tool call can
//! answer, and verifies the answer contains what the file actually said.
//!
//!     cargo run -p smithy-agent --example smoke

use std::sync::Arc;

use smithy_agent::{
    create_provider_from_env, session::default_system_prompt, Outcome, Session, SessionConfig,
    TurnEvent,
};
use smithy_tools::{Registry, ToolCtx, Workspace};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("notes.txt"), "the secret word is FJORD\n")?;
    std::fs::write(dir.path().join("decoy.txt"), "nothing interesting here\n")?;

    let provider = create_provider_from_env()?;
    println!(
        "provider: {} · model: {}",
        provider.name(),
        provider.model()
    );

    if let Some(info) = provider.probe_model().await? {
        println!(
            "  loaded={} ctx={:?} max_ctx={:?} tools={} {} {}",
            info.loaded,
            info.context_length,
            info.max_context_length,
            info.trained_for_tool_use,
            info.format,
            info.quantization
        );
        let limits = info.suggested_limits();
        println!(
            "  derived budget: warn at {} tokens, hard stop at {}",
            limits.context_warn, limits.context_hard
        );
    }

    provider.preflight().await?;
    println!("preflight ok\n");

    let registry = Arc::new(Registry::core());
    let ws = Workspace::open(dir.path())?;
    let prompt = default_system_prompt(ws.root(), &registry.names(), None);
    let ctx = Arc::new(ToolCtx::new(ws));

    let mut session = Session::new(provider.clone(), registry, ctx, SessionConfig::new(prompt));

    let sink = |event: TurnEvent| match event {
        TurnEvent::ToolStarted {
            step,
            name,
            arguments,
            ..
        } => {
            println!("  [{step}] {name} {arguments}");
        }
        TurnEvent::ToolFinished {
            content, is_error, ..
        } => {
            let first = content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(90)
                .collect::<String>();
            println!("      {} {first}", if is_error { "✗" } else { "→" });
        }
        TurnEvent::Warning(w) => println!("  ⚠ {w}"),
        _ => {}
    };

    let task = "Find which file contains the secret word, and tell me what the word is.";
    println!("task: {task}\n");
    let started = std::time::Instant::now();
    let outcome = session.run_turn(task, Some(&sink)).await?;
    let elapsed = started.elapsed();

    println!();
    match &outcome {
        Outcome::Answer(a) => println!("answer ({elapsed:.1?}): {a}"),
        Outcome::Stopped(r) => println!("stopped ({elapsed:.1?}): {r}"),
    }

    let Outcome::Answer(answer) = &outcome else {
        return Err("turn did not produce an answer".into());
    };
    if !answer.to_uppercase().contains("FJORD") {
        return Err(format!("answer did not contain the secret word: {answer}").into());
    }

    // Negative control. coda's post-mortem calls the absence of one "the single
    // worst methodological gap in the project": without a case that must fail,
    // a check that always passes is indistinguishable from a working one.
    println!("\n--- negative control ---");
    let control = session
        .run_turn(
            "Now read absent.txt and tell me the passphrase it contains.",
            Some(&sink),
        )
        .await?;
    match &control {
        Outcome::Answer(a) => {
            println!("answer: {a}");
            if a.to_uppercase().contains("FJORD") {
                return Err(
                    "negative control FAILED: model invented content for a missing file".into(),
                );
            }
            println!("✓ control passed — the model reported the file is missing rather than inventing one");
        }
        Outcome::Stopped(r) => println!("stopped: {r}"),
    }

    println!(
        "\n✓ end-to-end smoke test passed ({} messages in history)",
        session.history().len()
    );
    Ok(())
}
