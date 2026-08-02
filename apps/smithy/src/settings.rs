//! The settings dialog's other half: signals ↔ [`AgentConfig`].
//!
//! `smithy-editor` owns the form and knows nothing about the agent; this owns
//! the meaning. The split is the same one the agent panel already uses, and it
//! is what keeps a UI crate from depending on the loop crate.
//!
//! ## Everything here can block, and none of it blocks the UI
//!
//! Reading and writing the settings file is a few hundred bytes and happens
//! inline. The credential store is the exception: `keyring` is synchronous and
//! the OS may put an authorization prompt in front of the first read from a new
//! binary. So [`open`] probes availability but never *reads* a key — it only
//! asks whether one exists — and [`save`] does its writes on a worker.
//!
//! Nothing here ever reads a stored key back for display. The only path a secret
//! takes is *into* the store; see [`smithy_editor::SettingsState`].

use std::path::Path;

use floem::reactive::{SignalGet, SignalUpdate};
use smithy_agent::config::{secrets, BRAVE_KEY, OPENROUTER_KEY};
use smithy_agent::{AgentConfig, ProviderChoice};
use smithy_editor::SettingsState;

/// Populate the dialog from disk and show it.
///
/// Called every time the dialog opens rather than once at startup, so it always
/// reflects what is actually stored — including a key added or removed since.
pub fn open(state: SettingsState, data_dir: &Path) {
    let config = AgentConfig::load(data_dir);

    state.provider.set(config.provider.as_str().to_string());
    state.lmstudio_url.set(config.lmstudio.base_url.clone());
    state.lmstudio_model.set(config.lmstudio.model.clone());
    state.openrouter_url.set(config.openrouter.base_url.clone());
    state.openrouter_model.set(config.openrouter.model.clone());

    // Presence, not value. `secrets::get` does read the secret, and that is
    // unavoidable to answer the question at all — but it is dropped here and
    // never reaches a signal, so it cannot reach the screen.
    state.keychain_available.set(secrets::available());
    state
        .openrouter_key_stored
        .set(secrets::get(OPENROUTER_KEY).is_some());
    state.brave_key_stored.set(secrets::get(BRAVE_KEY).is_some());

    state.forget_typed_secrets();
    state.status.set(String::new());
    state.open.set(true);
}

/// Validate and persist the dialog's contents.
///
/// Returns `Err` only when nothing was written. A partial success — settings
/// saved, key rejected by the credential store — comes back as `Ok` with the
/// warnings, because silently discarding a correct endpoint change because the
/// keychain was locked would be the wrong trade.
pub fn save(state: SettingsState, data_dir: &Path) -> Result<Vec<String>, String> {
    let provider = ProviderChoice::parse(&state.provider.get_untracked())
        .ok_or_else(|| "no backend is selected".to_string())?;

    let config = AgentConfig {
        provider,
        lmstudio: smithy_agent::Endpoint {
            base_url: trimmed(state.lmstudio_url.get_untracked()),
            model: trimmed(state.lmstudio_model.get_untracked()),
        },
        openrouter: smithy_agent::Endpoint {
            base_url: trimmed(state.openrouter_url.get_untracked()),
            model: trimmed(state.openrouter_model.get_untracked()),
        },
    };

    validate(&config)?;

    let mut warnings = Vec::new();

    // Keys first. If the store rejects them we still want to know before
    // reporting success, and an endpoint written without its key would
    // otherwise reconnect straight into an authentication failure.
    let typed_openrouter = state.openrouter_key.get_untracked();
    let typed_brave = state.brave_key.get_untracked();
    for (account, typed) in [
        (OPENROUTER_KEY, typed_openrouter),
        (BRAVE_KEY, typed_brave),
    ] {
        if typed.trim().is_empty() {
            continue; // an untouched field means "leave the stored key alone"
        }
        if let Err(e) = secrets::set(account, &typed) {
            warnings.push(e);
        }
    }

    // The one case worth refusing outright: OpenRouter selected, no key typed
    // and none stored. Reconnecting would fail with a message about a missing
    // key, which is a worse place to learn it than the form you are looking at.
    if provider.needs_api_key()
        && !state.openrouter_key_stored.get_untracked()
        && state.openrouter_key.get_untracked().trim().is_empty()
        && smithy_agent::config::api_key(OPENROUTER_KEY, "OPENROUTER_API_KEY").is_none()
    {
        return Err("OpenRouter needs an API key before it can connect.".to_string());
    }

    config.save(data_dir)?;
    state.forget_typed_secrets();
    Ok(warnings)
}

/// Forget a stored key.
pub fn clear_key(state: SettingsState, account: &str) {
    match secrets::clear(account) {
        Ok(()) => {
            match account {
                OPENROUTER_KEY => state.openrouter_key_stored.set(false),
                BRAVE_KEY => state.brave_key_stored.set(false),
                _ => {}
            }
            state.report("Key removed from your keychain.", false);
        }
        Err(e) => state.report(e, true),
    }
}

/// Reject the configurations that cannot possibly work, and only those.
///
/// Deliberately shallow. A base URL that parses but points nowhere is the
/// endpoint's problem to report on connect, with a far better message than
/// anything guessable from the string — the preflight already says "cannot reach
/// X: is the server running?". What is worth catching here is the empty field
/// and the pasted-without-thinking scheme, because those produce errors that do
/// not name the thing you got wrong.
fn validate(config: &AgentConfig) -> Result<(), String> {
    let endpoint = config.active();
    let what = config.provider.label();

    if endpoint.base_url.is_empty() {
        return Err(format!("{what} needs a server URL."));
    }
    if !endpoint.base_url.starts_with("http://") && !endpoint.base_url.starts_with("https://") {
        return Err(format!(
            "The {what} URL must start with http:// or https://."
        ));
    }
    if endpoint.model.is_empty() {
        return Err(format!("{what} needs a model name."));
    }
    Ok(())
}

fn trimmed(s: String) -> String {
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: ProviderChoice, url: &str, model: &str) -> AgentConfig {
        let mut c = AgentConfig {
            provider,
            lmstudio: smithy_agent::Endpoint {
                base_url: String::new(),
                model: String::new(),
            },
            openrouter: smithy_agent::Endpoint {
                base_url: String::new(),
                model: String::new(),
            },
        };
        *c.active_mut() = smithy_agent::Endpoint {
            base_url: url.to_string(),
            model: model.to_string(),
        };
        c
    }

    #[test]
    fn a_complete_configuration_validates() {
        let c = config(
            ProviderChoice::LmStudio,
            "http://localhost:1234/v1",
            "qwen3.6-27b",
        );
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn an_empty_url_is_rejected_by_name() {
        let c = config(ProviderChoice::LmStudio, "", "qwen3.6-27b");
        let err = validate(&c).unwrap_err();
        assert!(err.contains("LM Studio"), "{err}");
    }

    #[test]
    fn an_empty_model_is_rejected_by_name() {
        let c = config(ProviderChoice::OpenRouter, "https://openrouter.ai/api/v1", "");
        let err = validate(&c).unwrap_err();
        assert!(err.contains("OpenRouter"), "{err}");
    }

    /// A host pasted without its scheme is the mistake this catches — the
    /// resulting request fails with a URL parse error that names nothing.
    #[test]
    fn a_url_without_a_scheme_is_rejected() {
        let c = config(ProviderChoice::LmStudio, "localhost:1234/v1", "m");
        assert!(validate(&c).unwrap_err().contains("http://"));
    }

    /// Validation looks at the *selected* endpoint only. A blank OpenRouter
    /// section must not block saving a local configuration, or switching
    /// backends becomes a trap.
    #[test]
    fn the_unselected_endpoint_is_not_validated() {
        let mut c = config(
            ProviderChoice::LmStudio,
            "http://localhost:1234/v1",
            "qwen3.6-27b",
        );
        c.openrouter.base_url = String::new();
        c.openrouter.model = String::new();
        assert!(validate(&c).is_ok());
    }
}
