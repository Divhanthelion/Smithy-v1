//! Talking to Smithy.
//!
//! A microphone in, a string out. **No floem** — the same separation
//! `smithy-agent`, `smithy-tools` and `smithy-sky` have, so the thing that
//! knows about Whisper does not also know about buttons.
//!
//! Ported from `app-ottex`, which had already solved the hard parts: Whisper
//! through candle on a dedicated OS thread behind a command channel, so a
//! transcription that takes two seconds cannot stall a UI frame. What did not
//! come across is everything ottex needed as a standalone tray app — global
//! hotkeys, synthetic keystrokes, the cloud API fallback, desktop notifications.
//! Smithy has its own answers to all four, and porting them would have meant
//! two.
//!
//! ## The model is loaded, not called
//!
//! Whisper runs *in this process*. Nothing is uploaded and nothing needs a key,
//! which is the same rule the agent follows — and the reason the first press of
//! the microphone is slow: several hundred megabytes are fetched from Hugging
//! Face and cached. Every press after that, and every launch after that, is
//! immediate and works with the network off.
//!
//! That cost is why loading is a state a caller can see rather than something
//! hidden inside the first recording. A button that appears dead for forty
//! seconds is indistinguishable from a broken one.

pub mod audio;
pub mod inference;
pub mod model;
pub mod resample;

pub use model::ModelConfig;

/// What the voice layer is doing.
///
/// A caller drives this with [`press`] and shows it. Deliberately a
/// plain enum with a pure transition function: what the microphone button does
/// on its third press is a question that should be answerable without a
/// microphone, a model, or a running application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Voice {
    /// Nothing loaded yet. The first press fetches the model.
    Cold,
    /// Fetching and loading. Slow the first time, and the caller has to say so.
    Loading,
    /// Loaded and waiting.
    Ready,
    /// Recording. The next press stops it.
    Listening,
    /// Audio captured, words being decoded.
    Transcribing,
    /// Something went wrong, with a sentence saying what.
    Failed(String),
}

impl Voice {
    /// Whether a press does anything at all right now.
    ///
    /// Loading and transcribing are both busy, and a second press during either
    /// has no sensible meaning — the model cannot be loaded twice and audio
    /// already captured cannot be un-captured.
    pub fn accepts_press(&self) -> bool {
        matches!(
            self,
            Voice::Cold | Voice::Ready | Voice::Listening | Voice::Failed(_)
        )
    }

    /// Whether the caller should be showing a spinner.
    pub fn is_busy(&self) -> bool {
        matches!(self, Voice::Loading | Voice::Transcribing)
    }

    /// Whether the microphone is live. The one state that needs to be obvious
    /// from across the room, because it is the one with a privacy cost.
    pub fn is_recording(&self) -> bool {
        matches!(self, Voice::Listening)
    }
}

/// What a press should cause.
///
/// Returned rather than performed, so the decision is testable and the effects
/// stay in the caller where the channels and the UI live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// Fetch and load the model, then go to [`Voice::Ready`].
    LoadModel,
    /// Open the microphone.
    StartRecording,
    /// Close it and transcribe what was captured.
    StopAndTranscribe,
    /// Busy — do nothing.
    Ignore,
}

/// What pressing the microphone means, given what it is currently doing.
///
/// The whole button, as one function:
///
/// | state | press |
/// |---|---|
/// | `Cold` | load the model |
/// | `Ready` | start recording |
/// | `Listening` | stop, and transcribe |
/// | `Loading`, `Transcribing` | nothing; it is busy |
/// | `Failed` | try loading again |
///
/// **A cold press loads and stops.** It deliberately does *not* go on to record.
/// The first load takes tens of seconds, and a microphone that silently opened
/// at the end of it would be recording a room whose owner had long since looked
/// away — the press that opens a microphone should be the press immediately
/// before speaking, always.
pub fn press(state: &Voice) -> Press {
    match state {
        Voice::Cold | Voice::Failed(_) => Press::LoadModel,
        Voice::Ready => Press::StartRecording,
        Voice::Listening => Press::StopAndTranscribe,
        Voice::Loading | Voice::Transcribing => Press::Ignore,
    }
}

/// Tidy a raw transcript for insertion into a prompt.
///
/// Whisper pads with spaces and is fond of a trailing full stop on a fragment.
/// Neither belongs in the middle of a sentence somebody is still dictating.
pub fn tidy(raw: &str) -> String {
    raw.trim().to_string()
}

/// Add dictated text to whatever is already in the box.
///
/// Appended with a space rather than replacing, because dictation is additive:
/// you say a sentence, look at it, and say another. Replacing would throw away
/// the first one, and there is no undo on a text field somebody is mid-thought
/// in.
pub fn append(existing: &str, dictated: &str) -> String {
    let dictated = tidy(dictated);
    if dictated.is_empty() {
        return existing.to_string();
    }
    if existing.trim().is_empty() {
        return dictated;
    }
    let separator = if existing.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    format!("{existing}{separator}{dictated}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The button, as the table in `press`'s own documentation.
    #[test]
    fn the_microphone_button_does_what_its_state_says() {
        assert_eq!(press(&Voice::Cold), Press::LoadModel);
        assert_eq!(press(&Voice::Ready), Press::StartRecording);
        assert_eq!(press(&Voice::Listening), Press::StopAndTranscribe);
        assert_eq!(press(&Voice::Loading), Press::Ignore);
        assert_eq!(press(&Voice::Transcribing), Press::Ignore);
        assert_eq!(press(&Voice::Failed("no device".into())), Press::LoadModel);
    }

    /// **Loading must not roll straight into recording.** The first load takes
    /// tens of seconds; opening a microphone at the end of it would start
    /// recording a room nobody is paying attention to any more. The press that
    /// opens a microphone is always the press immediately before speaking.
    #[test]
    fn loading_the_model_never_opens_the_microphone_by_itself() {
        assert_eq!(press(&Voice::Cold), Press::LoadModel);
        // And having loaded, the next press is the one that records.
        assert_eq!(press(&Voice::Ready), Press::StartRecording);
        assert!(!Voice::Loading.is_recording());
        assert!(!Voice::Ready.is_recording());
        assert!(Voice::Listening.is_recording());
    }

    /// A failure has to be recoverable by doing the obvious thing. A dead
    /// button after one bad load is a feature nobody uses twice.
    #[test]
    fn a_failure_can_be_retried_by_pressing_again() {
        let failed = Voice::Failed("network unreachable".into());
        assert!(failed.accepts_press());
        assert_eq!(press(&failed), Press::LoadModel);
    }

    /// Busy states swallow presses rather than queueing them. Two loads is not
    /// a thing, and audio already captured cannot be un-captured.
    #[test]
    fn a_press_while_busy_does_nothing() {
        assert!(!Voice::Loading.accepts_press());
        assert!(!Voice::Transcribing.accepts_press());
        assert!(Voice::Loading.is_busy() && Voice::Transcribing.is_busy());
        assert!(!Voice::Ready.is_busy());
    }

    /// Dictation is additive. You say a sentence, read it, and say another —
    /// so the second must not eat the first.
    #[test]
    fn dictating_twice_keeps_both_sentences() {
        let first = append("", " What is in this repo? ");
        assert_eq!(first, "What is in this repo?");

        let second = append(&first, "  And who wrote it?");
        assert_eq!(second, "What is in this repo? And who wrote it?");
    }

    /// Whatever is already typed is kept exactly, including a trailing space
    /// somebody left on purpose.
    #[test]
    fn dictation_joins_typed_text_without_mangling_it() {
        assert_eq!(append("explain ", "this function"), "explain this function");
        assert_eq!(append("explain", "this function"), "explain this function");
        // Silence changes nothing at all.
        assert_eq!(append("explain", "   "), "explain");
        assert_eq!(append("", ""), "");
    }
}
