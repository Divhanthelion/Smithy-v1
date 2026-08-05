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

/// Credential-store account name for the DeepSeek key.
pub const DEEPSEEK_KEY: &str = "deepseek-api-key";

/// Which backend the agent talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderChoice {
    /// A local OpenAI-compatible server.
    #[default]
    LmStudio,
    /// The hosted aggregator.
    OpenRouter,
    /// DeepSeek's own API.
    DeepSeek,
}

impl ProviderChoice {
    /// Every backend, in the order the settings dialog offers them.
    ///
    /// A single list so that adding one cannot half-land: the dialog, the
    /// round-trip test and the key lookup all read from here.
    pub const ALL: &'static [ProviderChoice] = &[
        ProviderChoice::LmStudio,
        ProviderChoice::OpenRouter,
        ProviderChoice::DeepSeek,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderChoice::LmStudio => "lmstudio",
            ProviderChoice::OpenRouter => "openrouter",
            ProviderChoice::DeepSeek => "deepseek",
        }
    }

    /// What to call it on screen.
    pub fn label(self) -> &'static str {
        match self {
            ProviderChoice::LmStudio => "LM Studio",
            ProviderChoice::OpenRouter => "OpenRouter",
            ProviderChoice::DeepSeek => "DeepSeek",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "lmstudio" | "lm-studio" | "lm_studio" | "local" => Some(ProviderChoice::LmStudio),
            "openrouter" | "open-router" | "open_router" => Some(ProviderChoice::OpenRouter),
            "deepseek" | "deep-seek" | "deep_seek" => Some(ProviderChoice::DeepSeek),
            _ => None,
        }
    }

    /// Whether this backend needs a key before it can be reached at all.
    ///
    /// Drives whether the settings dialog treats an empty key field as an error
    /// or as normal, which is the difference between a helpful form and a form
    /// that nags you about a field a local server has no use for.
    pub fn needs_api_key(self) -> bool {
        !matches!(self, ProviderChoice::LmStudio)
    }

    /// Where this backend's key lives: credential-store account, environment
    /// variable. `None` for a backend that needs no key.
    pub fn key_names(self) -> Option<(&'static str, &'static str)> {
        match self {
            ProviderChoice::LmStudio => None,
            ProviderChoice::OpenRouter => Some((OPENROUTER_KEY, "OPENROUTER_API_KEY")),
            ProviderChoice::DeepSeek => Some((DEEPSEEK_KEY, "DEEPSEEK_API_KEY")),
        }
    }

    /// This backend's key, if one is stored or in the environment.
    pub fn api_key(self) -> Option<String> {
        let (account, env_var) = self.key_names()?;
        api_key(account, env_var)
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

    fn deepseek_default() -> Self {
        Endpoint {
            base_url: crate::providers::deepseek::DEFAULT_URL.to_string(),
            model: crate::providers::deepseek::DEFAULT_MODEL.to_string(),
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
    /// Defaulted rather than required, so a settings file written before
    /// DeepSeek existed still parses instead of resetting every other setting.
    #[serde(default = "Endpoint::deepseek_default")]
    pub deepseek: Endpoint,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            provider: ProviderChoice::default(),
            lmstudio: Endpoint::lmstudio_default(),
            openrouter: Endpoint::openrouter_default(),
            deepseek: Endpoint::deepseek_default(),
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

        let has_key = |name: &str| {
            std::env::var(name)
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
        };
        let provider = explicit.unwrap_or_else(|| {
            // Order matters only in the unusual case of both being set, and
            // OpenRouter stays first because it was the behaviour before
            // DeepSeek existed — a checkout that worked must keep working.
            if has_key("OPENROUTER_API_KEY") {
                ProviderChoice::OpenRouter
            } else if has_key("DEEPSEEK_API_KEY") {
                ProviderChoice::DeepSeek
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
            deepseek: Endpoint {
                base_url: std::env::var("DEEPSEEK_URL")
                    .unwrap_or_else(|_| Endpoint::deepseek_default().base_url),
                model: std::env::var("DEEPSEEK_MODEL")
                    .unwrap_or_else(|_| Endpoint::deepseek_default().model),
            },
        }
    }

    /// The endpoint for a given backend.
    pub fn endpoint(&self, provider: ProviderChoice) -> &Endpoint {
        match provider {
            ProviderChoice::LmStudio => &self.lmstudio,
            ProviderChoice::OpenRouter => &self.openrouter,
            ProviderChoice::DeepSeek => &self.deepseek,
        }
    }

    /// The endpoint the current selection points at.
    pub fn active(&self) -> &Endpoint {
        self.endpoint(self.provider)
    }

    /// The endpoint the current selection points at, mutably.
    pub fn active_mut(&mut self) -> &mut Endpoint {
        match self.provider {
            ProviderChoice::LmStudio => &mut self.lmstudio,
            ProviderChoice::OpenRouter => &mut self.openrouter,
            ProviderChoice::DeepSeek => &mut self.deepseek,
        }
    }

    /// Build the provider these settings describe.
    ///
    /// Blocking, because reading the key can block on the OS credential store.
    /// Callers are already on a worker; see the module docs.
    pub fn build_provider(&self) -> Result<Arc<dyn Provider>, ProviderError> {
        self.build_provider_with_account_fingerprint()
            .map(|(provider, _)| provider)
    }

    /// Build the provider and retain only a one-way account identity.
    ///
    /// Fingerprinting happens before the key is moved into the provider, so no
    /// second raw `String` crosses into app state beside the provider that needs
    /// it. Reading Keychain again would prompt twice and can observe a different
    /// value if the account changes between reads.
    pub fn build_provider_with_account_fingerprint(
        &self,
    ) -> Result<
        (
            Arc<dyn Provider>,
            Option<crate::persist::CredentialAccountFingerprint>,
        ),
        ProviderError,
    > {
        match self.provider {
            ProviderChoice::LmStudio => Ok((
                Arc::new(LmStudio::new(
                    self.lmstudio.base_url.clone(),
                    self.lmstudio.model.clone(),
                )?),
                None,
            )),
            ProviderChoice::OpenRouter => {
                let key = self.require_key(ProviderChoice::OpenRouter, "OPENROUTER_API_KEY")?;
                let identity = crate::persist::CredentialAccountFingerprint::from_secret(
                    self.provider.as_str(),
                    &key,
                );
                Ok((
                    Arc::new(OpenRouter::new(
                        self.openrouter.base_url.clone(),
                        self.openrouter.model.clone(),
                        key,
                    )?),
                    Some(identity),
                ))
            }
            ProviderChoice::DeepSeek => {
                let key = self.require_key(ProviderChoice::DeepSeek, "DEEPSEEK_API_KEY")?;
                let identity = crate::persist::CredentialAccountFingerprint::from_secret(
                    self.provider.as_str(),
                    &key,
                );
                Ok((
                    Arc::new(crate::providers::DeepSeek::new(
                        self.deepseek.base_url.clone(),
                        self.deepseek.model.clone(),
                        key,
                    )?),
                    Some(identity),
                ))
            }
        }
    }

    fn require_key(
        &self,
        provider: ProviderChoice,
        env_var: &str,
    ) -> Result<String, ProviderError> {
        provider.api_key().ok_or_else(|| {
            ProviderError::Other(format!(
                "{} needs an API key. Add one under Settings → Agent, or set {env_var}.",
                provider.label()
            ))
        })
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
///
/// ## Why a cache and a presence file
///
/// macOS prompts once **per keychain item** the first time a binary path reads
/// it. Opening Settings used to call [`get`] three times (OpenRouter, DeepSeek,
/// Brave) just to learn whether a key existed, and refreshing the model list
/// called it again — so a single visit could ask for the login password three
/// or four times. [`is_stored`] answers presence from a non-secret sidecar;
/// [`get`] caches the value for the life of the process after the first
/// successful read. `cargo install --force` still re-prompts once per item
/// (new binary, new ACL), but never more than once per process after that.
pub mod secrets {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use super::SERVICE;

    /// Process-lifetime cache of secrets we have already unlocked.
    fn cache() -> &'static Mutex<HashMap<String, String>> {
        static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn presence_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(home.join(".local/share/smithy/key_presence.json"))
    }

    fn read_presence() -> HashMap<String, bool> {
        let Some(path) = presence_path() else {
            return HashMap::new();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_presence(map: &HashMap<String, bool>) {
        let Some(path) = presence_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string_pretty(map) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn mark_present(account: &str, present: bool) {
        let mut map = read_presence();
        if present {
            map.insert(account.to_string(), true);
        } else {
            map.remove(account);
        }
        write_presence(&map);
    }

    /// Whether a key is stored — without unlocking the keychain.
    ///
    /// Backed by a sidecar written whenever a key is saved, cleared, or
    /// successfully read. The settings dialog uses this so opening it does not
    /// cost a password prompt.
    pub fn is_stored(account: &str) -> bool {
        if cache()
            .lock()
            .ok()
            .is_some_and(|c| c.contains_key(account))
        {
            return true;
        }
        read_presence().get(account).copied().unwrap_or(false)
    }

    /// Fetch a secret. `None` when it is unset, empty, or unreachable.
    pub fn get(account: &str) -> Option<String> {
        if let Ok(cache) = cache().lock() {
            if let Some(secret) = cache.get(account) {
                return Some(secret.clone());
            }
        }

        let entry = keyring::Entry::new(SERVICE, account).ok()?;
        let secret = entry.get_password().ok()?;
        let secret = secret.trim().to_string();
        if secret.is_empty() {
            mark_present(account, false);
            None
        } else {
            if let Ok(mut cache) = cache().lock() {
                cache.insert(account.to_string(), secret.clone());
            }
            mark_present(account, true);
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
            .map_err(|e| format!("cannot save the key: {e}"))?;
        if let Ok(mut cache) = cache().lock() {
            cache.insert(account.to_string(), secret.trim().to_string());
        }
        mark_present(account, true);
        Ok(())
    }

    /// Remove a secret. Succeeds when there was nothing there.
    pub fn clear(account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account)
            .map_err(|e| format!("cannot reach the credential store: {e}"))?;
        match entry.delete_credential() {
            Ok(()) => {}
            Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(format!("cannot remove the key: {e}")),
        }
        if let Ok(mut cache) = cache().lock() {
            cache.remove(account);
        }
        mark_present(account, false);
        Ok(())
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
        let mut config = AgentConfig {
            provider: ProviderChoice::OpenRouter,
            ..AgentConfig::default()
        };
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
        let mut config = AgentConfig {
            provider: ProviderChoice::LmStudio,
            ..AgentConfig::default()
        };
        assert_eq!(config.active().base_url, "http://localhost:1234/v1");
        config.provider = ProviderChoice::OpenRouter;
        assert_eq!(config.active().base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn provider_names_round_trip_through_their_serialized_form() {
        for &choice in ProviderChoice::ALL {
            assert_eq!(ProviderChoice::parse(choice.as_str()), Some(choice));
        }
        assert_eq!(ProviderChoice::parse("nonsense"), None);
    }

    /// Only the hosted backends should make the dialog insist on a key; a local
    /// server has no use for one.
    #[test]
    fn only_the_hosted_backends_require_a_key() {
        assert!(ProviderChoice::OpenRouter.needs_api_key());
        assert!(ProviderChoice::DeepSeek.needs_api_key());
        assert!(!ProviderChoice::LmStudio.needs_api_key());
    }

    /// The whole point of `ALL`: a backend added to the enum but not the list
    /// would be invisible to the dialog, which iterates it.
    #[test]
    fn every_backend_is_listed_in_all() {
        assert_eq!(ProviderChoice::ALL.len(), 3);
        for &choice in ProviderChoice::ALL {
            assert!(!choice.label().is_empty());
            assert!(!choice.as_str().is_empty());
        }
    }

    /// A settings file written before DeepSeek existed must still load. Without
    /// `#[serde(default)]` the parse fails and `load` silently falls back to the
    /// environment — quietly discarding every setting the user had saved.
    #[test]
    fn a_settings_file_from_before_deepseek_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let old = r#"{
            "provider": "openrouter",
            "lmstudio": { "base_url": "http://localhost:1234/v1", "model": "qwen3.6-27b" },
            "openrouter": { "base_url": "https://openrouter.ai/api/v1", "model": "gpt-oss-20b:free" }
        }"#;
        std::fs::write(AgentConfig::file_in(tmp.path()), old).unwrap();

        let config = AgentConfig::load(tmp.path());
        assert_eq!(config.provider, ProviderChoice::OpenRouter);
        assert_eq!(config.openrouter.model, "gpt-oss-20b:free", "kept, not reset");
        assert_eq!(config.lmstudio.model, "qwen3.6-27b", "kept, not reset");
        assert_eq!(
            config.deepseek.base_url,
            crate::providers::deepseek::DEFAULT_URL,
            "the new section is defaulted in"
        );
    }

    #[test]
    fn all_three_endpoints_survive_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = AgentConfig {
            provider: ProviderChoice::DeepSeek,
            ..AgentConfig::default()
        };
        config.deepseek.model = "deepseek-v4-pro".to_string();
        config.openrouter.model = "cloud".to_string();
        config.lmstudio.model = "local".to_string();

        config.save(tmp.path()).unwrap();
        let reloaded = AgentConfig::load(tmp.path());
        assert_eq!(reloaded, config);
        assert_eq!(reloaded.active().model, "deepseek-v4-pro");
    }

    /// Each hosted backend must have its own credential slot, or saving one key
    /// overwrites the other.
    #[test]
    fn hosted_backends_do_not_share_a_key_slot() {
        let a = ProviderChoice::OpenRouter.key_names().unwrap();
        let b = ProviderChoice::DeepSeek.key_names().unwrap();
        assert_ne!(a, b);
        assert!(ProviderChoice::LmStudio.key_names().is_none());
    }
}
