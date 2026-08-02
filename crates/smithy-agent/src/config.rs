//! Where the backend is chosen, and where its key is kept.
//!
//! ## Why this exists
//!
//! Provider selection used to be readable only from the process environment —
//! `SMITHY_PROVIDER`, `OPENROUTER_API_KEY`, and a hand-rolled `.env` parser that
//! ran on the way to building the first session. That is a fine bootstrap and a
//! poor product: changing model meant leaving the editor, editing a dotfile in
//! the repository, and starting over. An IDE should be able to point itself at a
//! different endpoint from inside itself.
//!
//! ## Precedence, and why it is this way round
//!
//! 1. The settings file, when it exists.
//! 2. The environment (including `.env`), when it does not.
//!
//! The file wins because it is the thing the UI writes, and a setting you
//! changed in a dialog that loses to an environment variable you forgot about is
//! the worst of both worlds. But it only *exists* once you have saved something,
//! so an installation that has never opened the settings dialog behaves exactly
//! as it did before this module — the existing `.env` keeps working, untouched.
//!
//! Secrets are the one exception, and they resolve the other way for a reason:
//! see [`api_key`].
//!
//! ## Secrets do not live here
//!
//! [`AgentConfig`] is serialized to disk as plain JSON, so it holds no keys. The
//! API key is read from and written to the OS credential store — Keychain
//! Services on macOS — under [`SERVICE`]. What lands in the settings file is the
//! endpoint and the model name, which are not secrets and are much easier to
//! debug when they are legible.
//!
//! Keychain access is *synchronous and can block*: the OS may put up an
//! authorization prompt the first time a new binary reads an item. Every call
//! here is therefore made from session construction, which already runs on a
//! worker, and never from the UI thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::provider::{Provider, ProviderError};
use crate::providers::{LmStudio, OpenRouter};

/// The credential-store service every Smithy secret is filed under.
pub const SERVICE: &str = "smithy";

/// Credential-store account name for the OpenRouter key.
pub const OPENROUTER_KEY: &str = "openrouter-api-key";

/// Credential-store account name for the Brave Search key.
pub const BRAVE_KEY: &str = "brave-api-key";

/// Which backend the agent talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderChoice {
    /// A local OpenAI-compatible server.
    #[default]
    LmStudio,
    /// The hosted aggregator.
    OpenRouter,
}

impl ProviderChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderChoice::LmStudio => "lmstudio",
            ProviderChoice::OpenRouter => "openrouter",
        }
    }

    /// What to call it on screen.
    pub fn label(self) -> &'static str {
        match self {
            ProviderChoice::LmStudio => "LM Studio",
            ProviderChoice::OpenRouter => "OpenRouter",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "lmstudio" | "lm-studio" | "lm_studio" | "local" => Some(ProviderChoice::LmStudio),
            "openrouter" | "open-router" | "open_router" => Some(ProviderChoice::OpenRouter),
            _ => None,
        }
    }

    /// Whether this backend needs a key before it can be reached at all.
    ///
    /// Drives whether the settings dialog treats an empty key field as an error
    /// or as normal, which is the difference between a helpful form and a form
    /// that nags you about a field a local server has no use for.
    pub fn needs_api_key(self) -> bool {
        matches!(self, ProviderChoice::OpenRouter)
    }
}

/// One backend's address and model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
}

impl Endpoint {
    fn lmstudio_default() -> Self {
        Endpoint {
            base_url: "http://localhost:1234/v1".to_string(),
            model: "qwen3.6-27b".to_string(),
        }
    }

    fn openrouter_default() -> Self {
        Endpoint {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "anthropic/claude-3.5-sonnet".to_string(),
        }
    }
}

/// The persisted backend selection.
///
/// Both endpoints are kept, not just the selected one, so that switching back
/// and forth does not lose the model you had configured on the other side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub provider: ProviderChoice,
    pub lmstudio: Endpoint,
    pub openrouter: Endpoint,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            provider: ProviderChoice::default(),
            lmstudio: Endpoint::lmstudio_default(),
            openrouter: Endpoint::openrouter_default(),
        }
    }
}

impl AgentConfig {
    /// Where the settings live, given the app's data directory.
    pub fn file_in(data_dir: &Path) -> PathBuf {
        data_dir.join("provider.json")
    }

    /// Read the stored settings, falling back to the environment.
    ///
    /// Never fails. A settings file written by a future version, or truncated by
    /// a full disk, means the environment-derived default — on the same grounds
    /// as [`crate::providers`]' old behaviour and [`Aesthetic::load`]: a corrupt
    /// preference should cost you your preference, not your editor.
    ///
    /// [`Aesthetic::load`]: https://docs.rs/smithy-editor
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file_in(data_dir))
            .ok()
            .and_then(|text| serde_json::from_str::<AgentConfig>(&text).ok())
            .unwrap_or_else(Self::from_env)
    }

    /// Persist the settings.
    ///
    /// Write-then-rename, for the reason [`crate::persist`] and the project
    /// registry both do it: an interrupted write must not be able to leave a
    /// file that fails to parse on the next launch. Here that would mean an
    /// editor that silently reverted to a different model.
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        let path = Self::file_in(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }

    /// Derive settings from the environment, the way this used to work.
    ///
    /// Reached only when there is no settings file. The selection rule is the
    /// one [`crate::providers::create_provider_from_env`] documented: an explicit
    /// `SMITHY_PROVIDER`/`PROVIDER` first, otherwise OpenRouter when a key is
    /// present, otherwise the local server.
    pub fn from_env() -> Self {
        load_dotenv_if_present();

        let explicit = std::env::var("SMITHY_PROVIDER")
            .or_else(|_| std::env::var("PROVIDER"))
            .ok()
            .and_then(|s| ProviderChoice::parse(&s));

        let provider = explicit.unwrap_or_else(|| {
            let has_key = std::env::var("OPENROUTER_API_KEY")
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false);
            if has_key {
                ProviderChoice::OpenRouter
            } else {
                ProviderChoice::LmStudio
            }
        });

        AgentConfig {
            provider,
            lmstudio: Endpoint {
                base_url: std::env::var("LMSTUDIO_URL")
                    .unwrap_or_else(|_| Endpoint::lmstudio_default().base_url),
                model: std::env::var("LMSTUDIO_MODEL")
                    .unwrap_or_else(|_| Endpoint::lmstudio_default().model),
            },
            openrouter: Endpoint {
                base_url: std::env::var("OPENROUTER_URL")
                    .unwrap_or_else(|_| Endpoint::openrouter_default().base_url),
                model: std::env::var("OPENROUTER_MODEL")
                    .unwrap_or_else(|_| Endpoint::openrouter_default().model),
            },
        }
    }

    /// The endpoint the current selection points at.
    pub fn active(&self) -> &Endpoint {
        match self.provider {
            ProviderChoice::LmStudio => &self.lmstudio,
            ProviderChoice::OpenRouter => &self.openrouter,
        }
    }

    /// The endpoint the current selection points at, mutably.
    pub fn active_mut(&mut self) -> &mut Endpoint {
        match self.provider {
            ProviderChoice::LmStudio => &mut self.lmstudio,
            ProviderChoice::OpenRouter => &mut self.openrouter,
        }
    }

    /// Build the provider these settings describe.
    ///
    /// Blocking, because reading the key can block on the OS credential store.
    /// Callers are already on a worker; see the module docs.
    pub fn build_provider(&self) -> Result<Arc<dyn Provider>, ProviderError> {
        match self.provider {
            ProviderChoice::LmStudio => Ok(Arc::new(LmStudio::new(
                self.lmstudio.base_url.clone(),
                self.lmstudio.model.clone(),
            )?)),
            ProviderChoice::OpenRouter => {
                let key = api_key(OPENROUTER_KEY, "OPENROUTER_API_KEY").ok_or_else(|| {
                    ProviderError::Other(
                        "OpenRouter needs an API key. Add one under Settings → Agent, or set \
                         OPENROUTER_API_KEY."
                            .to_string(),
                    )
                })?;
                Ok(Arc::new(OpenRouter::new(
                    self.openrouter.base_url.clone(),
                    self.openrouter.model.clone(),
                    key,
                )?))
            }
        }
    }
}

/// Read a secret, preferring the credential store and falling back to the
/// environment.
///
/// **This is the opposite of the precedence [`AgentConfig`] itself uses**, and
/// deliberately so. Settings resolve file-first because the file is what the UI
/// writes and the UI must win. Secrets resolve store-first for the same reason —
/// a key you saved in the dialog must beat a stale one in `.env` — but they keep
/// the environment as a *fallback* rather than dropping it, because that is what
/// lets an existing checkout with a working `.env` keep working without anyone
/// having to migrate anything, and what lets CI supply a key with no keychain at
/// all.
///
/// An empty value counts as absent. A key set to the empty string is a mistake
/// every time, and reporting "no key" beats reporting an authentication failure.
pub fn api_key(account: &str, env_var: &str) -> Option<String> {
    if let Some(secret) = secrets::get(account) {
        return Some(secret);
    }
    load_dotenv_if_present();
    std::env::var(env_var)
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// The OS credential store, reduced to the three operations we need.
///
/// Every function here degrades to `None`/`Err` rather than panicking. A locked
/// keychain, a headless session, or a platform with no credential store at all
/// must leave you with a usable editor and a legible message — the same posture
/// [`crate::providers`] takes toward an unreachable endpoint.
pub mod secrets {
    use super::SERVICE;

    /// Fetch a secret. `None` when it is unset, empty, or unreachable.
    pub fn get(account: &str) -> Option<String> {
        let entry = keyring::Entry::new(SERVICE, account).ok()?;
        let secret = entry.get_password().ok()?;
        let secret = secret.trim().to_string();
        if secret.is_empty() {
            None
        } else {
            Some(secret)
        }
    }

    /// Store a secret. An empty value deletes it instead, so that clearing the
    /// field in the settings dialog does what it looks like it does rather than
    /// filing an empty string that later reads back as a key.
    pub fn set(account: &str, secret: &str) -> Result<(), String> {
        if secret.trim().is_empty() {
            return clear(account);
        }
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| format!("cannot reach the credential store: {e}"))?;
        entry
            .set_password(secret.trim())
            .map_err(|e| format!("cannot save the key: {e}"))
    }

    /// Remove a secret. Succeeds when there was nothing there.
    pub fn clear(account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| format!("cannot reach the credential store: {e}"))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("cannot remove the key: {e}")),
        }
    }

    /// Whether the credential store can be reached at all.
    ///
    /// The settings dialog asks before offering to save a key, so that a machine
    /// where the store is unavailable says so up front instead of accepting the
    /// key and silently forgetting it.
    pub fn available() -> bool {
        keyring::Entry::store_status().is_ok()
    }
}

/// Load `.env` into the process environment, if there is one.
///
/// Moved here from `providers` unchanged. Still deliberately non-overriding: a
/// real environment variable beats the file, because that is the direction that
/// lets a one-off `OPENROUTER_MODEL=… cargo run` work.
pub fn load_dotenv_if_present() {
    let Ok(content) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if std::env::var(key).is_err() {
                std::env::set_var(key, val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::default();
        config.provider = ProviderChoice::OpenRouter;
        config.openrouter.model = "anthropic/claude-opus-4".to_string();
        config.lmstudio.model = "qwen3.6-27b".to_string();

        config.save(tmp.path()).unwrap();
        assert_eq!(AgentConfig::load(tmp.path()), config);
    }

    /// A settings file written by a future version, or corrupted, must not stop
    /// the editor opening.
    #[test]
    fn unreadable_settings_fall_back_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(AgentConfig::file_in(tmp.path()), "{not json").unwrap();
        let _ = AgentConfig::load(tmp.path()); // must not panic
    }

    /// Switching provider must not discard the other side's model, or a round
    /// trip through the dialog silently rewrites configuration you never touched.
    #[test]
    fn both_endpoints_are_kept_across_a_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::default();
        config.lmstudio.model = "local-model".to_string();
        config.openrouter.model = "cloud-model".to_string();

        config.provider = ProviderChoice::OpenRouter;
        config.save(tmp.path()).unwrap();
        let mut reloaded = AgentConfig::load(tmp.path());
        assert_eq!(reloaded.active().model, "cloud-model");

        reloaded.provider = ProviderChoice::LmStudio;
        assert_eq!(reloaded.active().model, "local-model");
    }

    #[test]
    fn the_active_endpoint_follows_the_selection() {
        let mut config = AgentConfig::default();
        config.provider = ProviderChoice::LmStudio;
        assert_eq!(config.active().base_url, "http://localhost:1234/v1");
        config.provider = ProviderChoice::OpenRouter;
        assert_eq!(config.active().base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn provider_names_round_trip_through_their_serialized_form() {
        for choice in [ProviderChoice::LmStudio, ProviderChoice::OpenRouter] {
            assert_eq!(ProviderChoice::parse(choice.as_str()), Some(choice));
        }
        assert_eq!(ProviderChoice::parse("nonsense"), None);
    }

    /// Only OpenRouter should make the dialog insist on a key; a local server
    /// has no use for one.
    #[test]
    fn only_the_hosted_backend_requires_a_key() {
        assert!(ProviderChoice::OpenRouter.needs_api_key());
        assert!(!ProviderChoice::LmStudio.needs_api_key());
    }
}
