//! Which model, and where it lives.

use serde::{Deserialize, Serialize};

/// The locally-embedded Whisper model.
///
/// Downloaded from Hugging Face on first use and cached on disk, so the second
/// launch is instant and every launch after that works offline — the same
/// local-first rule the agent follows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Where the downloaded weights are cached.
    ///
    /// Handed to `hf-hub` when the model loads. It was set and never read for
    /// as long as this existed, so the weights went to `~/.cache/huggingface`
    /// while every note about this field claimed they went where it pointed —
    /// a setting that configured nothing and documentation that agreed with it.
    pub cache_dir: String,
    /// Hugging Face repository.
    pub repo_id: String,
    pub revision: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            // `turbo` rather than plain large-v3: about eight times faster to
            // decode for a barely measurable accuracy cost, and dictation is
            // judged on the wait, not on the third decimal place of its word
            // error rate.
            repo_id: "openai/whisper-large-v3-turbo".to_string(),
            revision: "main".to_string(),
        }
    }
}

/// Alongside everything else Smithy keeps, rather than in a directory of its
/// own — one place to delete when someone wants their disk back.
fn default_cache_dir() -> String {
    std::env::var("HOME")
        .map(|home| format!("{home}/.local/share/smithy/models"))
        .unwrap_or_else(|_| "models".to_string())
}

/// Report what the voice layer is doing, when `SMITHY_VOICE_DEBUG=1`.
///
/// The same pattern as `SMITHY_KEY_DEBUG` and `SMITHY_SQUIGGLE_DEBUG`: model
/// loading takes tens of seconds the first time and involves a network, a
/// cache, a device and a tokenizer, and every one of those failing looks
/// identical from the outside — a mic button that does nothing.
#[macro_export]
macro_rules! voice_debug {
    ($($arg:tt)*) => {
        if std::env::var("SMITHY_VOICE_DEBUG").is_ok_and(|v| v != "0") {
            eprintln!("[voice] {}", format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache belongs with the rest of Smithy's data. A model is hundreds of
    /// megabytes and somebody will want to find it.
    #[test]
    fn the_model_is_cached_under_smithys_own_data_directory() {
        let dir = ModelConfig::default().cache_dir;
        assert!(
            dir.contains("smithy"),
            "cached at {dir}, which is nobody's guess"
        );
    }
}
