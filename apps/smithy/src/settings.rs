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
//! binary. So [`open`] answers "is a key stored?" from a non-secret sidecar
//! ([`secrets::is_stored`]) and never unlocks the keychain just to populate the
//! dialog. The key itself is read only when a backend actually needs it
//! (catalogue refresh, connect), and then cached for the process.
//!
//! Nothing here ever reads a stored key back for display. The only path a secret
//! takes is *into* the store; see [`smithy_editor::SettingsState`].

use std::path::Path;

use floem::reactive::{SignalGet, SignalUpdate};
use smithy_agent::catalogue;
use smithy_agent::config::{secrets, BRAVE_KEY, DEEPSEEK_KEY, OPENROUTER_KEY};
use smithy_agent::{AgentConfig, ProviderChoice};
use smithy_editor::{ModelRow, SettingsState};

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
    state.deepseek_url.set(config.deepseek.base_url.clone());
    state.deepseek_model.set(config.deepseek.model.clone());

    // Presence from the sidecar, never from the keychain. Reading a stored key
    // just to learn that it exists is what used to cost three password prompts
    // every time this dialog opened.
    state.keychain_available.set(secrets::available());
    state
        .openrouter_key_stored
        .set(secrets::is_stored(OPENROUTER_KEY));
    state
        .deepseek_key_stored
        .set(secrets::is_stored(DEEPSEEK_KEY));
    state.brave_key_stored.set(secrets::is_stored(BRAVE_KEY));

    state.forget_typed_secrets();
    state.status.set(String::new());
    // Free-only is meaningless on a local backend, where nothing has a price.
    state.free_only.set(config.provider == ProviderChoice::OpenRouter);
    state.open.set(true);

    // Populate the picker without being asked. Both catalogues are one cheap
    // request and the list is the reason most people open this dialog; making
    // them press Refresh first would be a step that exists only because it was
    // easier to write.
    refresh_models(state, data_dir);
}

/// Fetch the selected backend's model list into the dialog.
///
/// Everything happens off the UI thread and lands back through the signals,
/// which are `Copy` and only touched from floem's thread — the fetch itself
/// carries no signal into the async block, only plain data out.
pub fn refresh_models(state: SettingsState, data_dir: &Path) {
    if state.loading_models.get_untracked() {
        return; // a second press while one is in flight would race the first
    }
    let Some(provider) = ProviderChoice::parse(&state.provider.get_untracked()) else {
        return;
    };

    let typed_url = trimmed(state.active_url().get_untracked());
    let endpoint = if typed_url.is_empty() {
        // Fall back to what is stored *for this backend* — not to `active()`,
        // which follows the saved selection and would hand LM Studio's URL to a
        // dialog on which you had just clicked DeepSeek.
        AgentConfig::load(data_dir).endpoint(provider).clone()
    } else {
        smithy_agent::Endpoint {
            base_url: typed_url,
            model: String::new(),
        }
    };

    state.loading_models.set(true);
    state.models_error.set(String::new());

    let (tx, rx) = crossbeam_channel::bounded::<Result<Vec<ModelRow>, String>>(1);
    crate::runtime::tokio_runtime().spawn(async move {
        // Whichever key this backend uses, read off the UI thread because the
        // credential store can block. `None` is fine and expected: LM Studio
        // needs none, and OpenRouter's catalogue is public — you can browse the
        // free tier before deciding to sign up for a key to use it with.
        // DeepSeek is the one that will report back that it needs one.
        let key = tokio::task::spawn_blocking(move || provider.api_key())
            .await
            .ok()
            .flatten();

        let result = catalogue::list(provider, &endpoint, key.as_deref())
            .await
            .map(|entries| entries.iter().map(to_row).collect());
        let _ = tx.send(result);
    });

    // Drain on the UI thread. `Effect` + a tick is how the agent panel bridges
    // its channel; here the exchange is a single value, so a short poll is
    // simpler than standing up another tick source for one message.
    poll_once(rx, move |result| match result {
        Ok(rows) => {
            state.loading_models.set(false);
            state.models.set(rows);
        }
        Err(e) => {
            state.loading_models.set(false);
            state.models.set(Vec::new());
            state.models_error.set(e);
        }
    });
}

/// Load a local model into memory.
///
/// Optional in the sense that LM Studio's JIT loader would do it on the first
/// request anyway — and worth having anyway, because that first request then
/// appears to hang for a minute with nothing on screen explaining why.
pub fn load_model(state: SettingsState, data_dir: &Path, model: &str) {
    if !state.loading_into_memory.get_untracked().is_empty() {
        return; // one at a time; two concurrent loads would fight for memory
    }
    let base_url = {
        let typed = trimmed(state.lmstudio_url.get_untracked());
        if typed.is_empty() {
            AgentConfig::load(data_dir).lmstudio.base_url
        } else {
            typed
        }
    };

    let model = model.to_string();
    state.loading_into_memory.set(model.clone());
    state.report(format!("Loading {model} into LM Studio…"), false);

    let (tx, rx) = crossbeam_channel::bounded(1);
    let for_task = model.clone();
    crate::runtime::tokio_runtime().spawn(async move {
        let result = catalogue::load_lmstudio_model(&base_url, &for_task, None).await;
        let _ = tx.send(result);
    });

    let data_dir = data_dir.to_path_buf();
    poll_once(rx, move |result| {
        state.loading_into_memory.set(String::new());
        match result {
            Ok(loaded) => {
                state.report(
                    format!("Loaded {} in {:.0}s.", loaded.model, loaded.seconds),
                    false,
                );
                // Selecting it is what you meant by loading it.
                state.choose_model(&loaded.model);
                // Refetch so the row now says "loaded" rather than still
                // offering the button that just worked.
                refresh_models(state, &data_dir);
            }
            Err(e) => state.report(e, true),
        }
    });
}

/// Wait for one value on `rx` and hand it to `deliver` on the UI thread.
///
/// floem's reactive graph is single-threaded and its signals are not `Send`, so
/// the worker cannot write them directly. This watches the channel from a floem
/// timer and fires once.
fn poll_once<T: 'static>(
    rx: crossbeam_channel::Receiver<T>,
    deliver: impl Fn(T) + 'static,
) {
    fn tick<T: 'static>(
        rx: crossbeam_channel::Receiver<T>,
        deliver: std::rc::Rc<dyn Fn(T)>,
    ) {
        floem::action::exec_after(std::time::Duration::from_millis(60), move |_| {
            match rx.try_recv() {
                Ok(value) => deliver(value),
                // Disconnected means the worker died without sending; stopping
                // here rather than rescheduling avoids a timer that never ends.
                Err(crossbeam_channel::TryRecvError::Disconnected) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => tick(rx, deliver),
            }
        });
    }
    tick(rx, std::rc::Rc::new(deliver));
}

/// Translate a catalogue entry into the row the dialog renders.
///
/// The rendering happens here rather than in `smithy-editor` so that crate never
/// learns what a pricing tier is — the same split the agent panel uses.
fn to_row(entry: &smithy_agent::ModelEntry) -> ModelRow {
    ModelRow {
        id: entry.id.clone(),
        label: entry.label.clone(),
        context: entry.context_label(),
        badge: entry.badge(),
        tool_capable: entry.tool_capable,
        free: entry.tier.is_free(),
        local: matches!(entry.tier, smithy_agent::ModelTier::Local { .. }),
        loaded: matches!(
            entry.tier,
            smithy_agent::ModelTier::Local { loaded: true, .. }
        ),
    }
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
        deepseek: smithy_agent::Endpoint {
            base_url: trimmed(state.deepseek_url.get_untracked()),
            model: trimmed(state.deepseek_model.get_untracked()),
        },
    };

    validate(&config)?;

    let mut warnings = Vec::new();

    // Keys first. If the store rejects them we still want to know before
    // reporting success, and an endpoint written without its key would
    // otherwise reconnect straight into an authentication failure.
    for (account, typed) in [
        (OPENROUTER_KEY, state.openrouter_key.get_untracked()),
        (DEEPSEEK_KEY, state.deepseek_key.get_untracked()),
        (BRAVE_KEY, state.brave_key.get_untracked()),
    ] {
        if typed.trim().is_empty() {
            continue; // an untouched field means "leave the stored key alone"
        }
        if let Err(e) = secrets::set(account, &typed) {
            warnings.push(e);
        }
    }

    // The one case worth refusing outright: a hosted backend selected with no
    // key typed and none stored. Reconnecting would fail with a message about a
    // missing key, which is a worse place to learn it than the form in front of
    // you. Checked *after* the writes above, so a key typed in this very visit
    // counts.
    if provider.needs_api_key() && provider.api_key().is_none() {
        return Err(format!(
            "{} needs an API key before it can connect.",
            provider.label()
        ));
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
                DEEPSEEK_KEY => state.deepseek_key_stored.set(false),
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

    /// A config where only the selected backend is filled in, so validation is
    /// exercised against exactly one populated endpoint.
    fn config(provider: ProviderChoice, url: &str, model: &str) -> AgentConfig {
        let blank = || smithy_agent::Endpoint {
            base_url: String::new(),
            model: String::new(),
        };
        let mut c = AgentConfig {
            provider,
            lmstudio: blank(),
            openrouter: blank(),
            deepseek: blank(),
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

    /// Validation looks at the *selected* endpoint only. A blank OpenRouter or
    /// DeepSeek section must not block saving a local configuration, or
    /// switching backends becomes a trap.
    #[test]
    fn the_unselected_endpoints_are_not_validated() {
        let c = config(
            ProviderChoice::LmStudio,
            "http://localhost:1234/v1",
            "qwen3.6-27b",
        );
        assert!(c.openrouter.base_url.is_empty() && c.deepseek.base_url.is_empty());
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn a_deepseek_configuration_validates_and_is_named_in_its_errors() {
        let good = config(
            ProviderChoice::DeepSeek,
            "https://api.deepseek.com",
            "deepseek-v4-flash",
        );
        assert!(validate(&good).is_ok());

        let bad = config(ProviderChoice::DeepSeek, "https://api.deepseek.com", "");
        assert!(validate(&bad).unwrap_err().contains("DeepSeek"));
    }

    /// Every backend the dialog offers must round-trip through `parse`, or
    /// selecting it silently falls back to the default.
    #[test]
    fn every_offered_backend_parses_back() {
        for (tag, _, _) in smithy_editor::PROVIDERS {
            let parsed = ProviderChoice::parse(tag)
                .unwrap_or_else(|| panic!("dialog offers `{tag}` which ProviderChoice cannot parse"));
            assert_eq!(parsed.as_str(), tag);
        }
        assert_eq!(
            smithy_editor::PROVIDERS.len(),
            ProviderChoice::ALL.len(),
            "the dialog and the enum disagree about how many backends exist"
        );
    }

    /// A hosted backend needs a key; the local one must not be nagged for one.
    #[test]
    fn only_the_hosted_backends_require_a_key() {
        assert!(!ProviderChoice::LmStudio.needs_api_key());
        assert!(ProviderChoice::OpenRouter.needs_api_key());
        assert!(ProviderChoice::DeepSeek.needs_api_key());
    }

    /// Two hosted backends must not share a credential-store slot, or saving one
    /// key would overwrite the other.
    #[test]
    fn each_backend_has_its_own_key_slot() {
        let openrouter = ProviderChoice::OpenRouter.key_names().unwrap();
        let deepseek = ProviderChoice::DeepSeek.key_names().unwrap();
        assert_ne!(openrouter.0, deepseek.0, "credential-store accounts collide");
        assert_ne!(openrouter.1, deepseek.1, "environment variables collide");
        assert!(ProviderChoice::LmStudio.key_names().is_none());
    }
}
