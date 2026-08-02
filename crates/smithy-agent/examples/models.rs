//! List what each backend offers.
//!
//!     cargo run -p smithy-agent --example models -- lmstudio
//!     cargo run -p smithy-agent --example models -- openrouter
//!     cargo run -p smithy-agent --example models -- deepseek
//!     cargo run -p smithy-agent --example models -- load <model-key>
//!
//! The same calls the settings dialog's model picker makes, without the dialog —
//! useful for checking that an endpoint is reachable, that the tool-capable
//! filter is keeping the right things out, and that a local model loads.

use smithy_agent::catalogue::{self, ModelTier};
use smithy_agent::{AgentConfig, ProviderChoice};

#[tokio::main]
async fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "lmstudio".into());

    if which == "load" {
        let Some(model) = std::env::args().nth(2) else {
            eprintln!("usage: … --example models -- load <model-key>");
            std::process::exit(2);
        };
        let config = AgentConfig::from_env();
        eprintln!("loading {model} — this blocks until it is resident…");
        match catalogue::load_lmstudio_model(&config.lmstudio.base_url, &model, None).await {
            Ok(loaded) => println!("loaded {} in {:.1}s", loaded.model, loaded.seconds),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Some(provider) = ProviderChoice::parse(&which) else {
        eprintln!("unknown provider `{which}` — use `lmstudio`, `openrouter`, or `load <model>`");
        std::process::exit(2);
    };

    let config = AgentConfig::from_env();
    let endpoint = config.endpoint(provider);
    // Whichever key this backend uses, or none for a local server.
    let key = provider.api_key();

    eprintln!("── {} at {} ──", provider.label(), endpoint.base_url);

    let entries = match catalogue::list(provider, endpoint, key.as_deref()).await {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    };

    let usable = entries.iter().filter(|e| e.tool_capable).count();
    let free = entries.iter().filter(|e| e.tier.is_free()).count();
    eprintln!(
        "{} models · {usable} tool-capable · {free} free\n",
        entries.len()
    );

    for entry in &entries {
        let mark = if entry.tool_capable { " " } else { "✗" };
        let loaded = matches!(entry.tier, ModelTier::Local { loaded: true, .. });
        println!(
            "{mark} {:<52} {:>10}  {}{}",
            entry.id,
            entry.context_label(),
            entry.badge(),
            if loaded { "  ← resident" } else { "" }
        );
    }
    eprintln!("\n✗ = cannot call tools; the agent cannot use it.");
}
