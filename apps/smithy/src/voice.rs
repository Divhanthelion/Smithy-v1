//! The microphone, wired to the agent panel.
//!
//! `smithy-voice` turns audio into a string and knows nothing about buttons;
//! this is the other half — one press handler, and the bridge that carries
//! results from the Whisper thread onto the screen.
//!
//! What a press *means* is [`smithy_voice::press`], a pure function tested
//! without a microphone. What it *does* is here, because that is where the
//! channels and the signals live.

use std::cell::RefCell;
use std::rc::Rc;

use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use smithy_voice::audio::{AudioRecorder, RecordingHandle};
use smithy_voice::inference::{Event, Transcriber};
use smithy_voice::{press, Press, Voice};

/// Everything the microphone needs to stay alive between presses.
///
/// **No recorder here, deliberately.** It used to hold one, resolved once by
/// `AudioRecorder::new()` at launch and kept for the life of the process, which
/// meant the input device was whatever existed at startup. Connect AirPods
/// afterwards and they were never found — the only cure was relaunching the
/// editor. Worse, a machine with no device at launch got `None` back from
/// `new`, so the whole control never existed and the button was dead for the
/// session even after something was plugged in. `smithy_voice::press` already
/// maps `Failed` back to `LoadModel`; there was simply nothing left to press.
pub struct VoiceControl {
    transcriber: Transcriber,
    /// Live only while recording. Held here because stopping consumes it and a
    /// press is a different call from the one that started it.
    recording: RefCell<Option<RecordingHandle>>,
    state: RwSignal<Voice>,
}

impl VoiceControl {
    /// Start the Whisper thread and bridge its events onto the UI.
    ///
    /// The thread is spawned now and the *model* is not — nothing is fetched
    /// until the first press, so a launch costs a thread and nothing else.
    ///
    /// Infallible: it no longer touches audio hardware, so there is nothing
    /// here to fail. The device is found at the press that needs it.
    pub fn new(state: RwSignal<Voice>, input: RwSignal<String>) -> Rc<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<Event>();
        let (tick, inbox) = crate::app_state::bridge(rx);

        floem::reactive::Effect::new(move |_| {
            tick.get();
            for event in crate::app_state::drain(&inbox) {
                match event {
                    Event::Loaded => state.set(Voice::Ready),
                    Event::Transcribed(text) => {
                        // Appended rather than replacing: dictation is
                        // additive, and there is no undo on a text box somebody
                        // is mid-thought in.
                        input.update(|existing| {
                            *existing = smithy_voice::append(existing, &text);
                        });
                        state.set(Voice::Ready);
                    }
                    Event::Failed(why) => state.set(Voice::Failed(why)),
                }
            }
        });

        Rc::new(Self {
            transcriber: Transcriber::new(tx),
            recording: RefCell::new(None),
            state,
        })
    }

    /// One press of the microphone, or of its hotkey.
    pub fn press(&self) {
        match press(&self.state.get_untracked()) {
            Press::LoadModel => {
                self.state.set(Voice::Loading);
                self.transcriber.load(smithy_voice::ModelConfig::default());
            }
            // The device is resolved *here*, on the press that needs it, and
            // dropped again immediately: `RecordingHandle` owns its stream, so
            // the recorder is only the thing that built it. Enumeration costs
            // milliseconds, and paying it per press is what lets a headset
            // connected after launch actually be found.
            //
            // **Not "no microphone".** This error covers every reason the input
            // device could not be opened — none selected, permission never
            // granted, another process holding it exclusively — and naming the
            // one cause it usually is *not* sends you looking at hardware. The
            // panel puts the detail under a hover.
            Press::StartRecording => {
                match AudioRecorder::new().and_then(|recorder| recorder.start_recording()) {
                    Ok(handle) => {
                        *self.recording.borrow_mut() = Some(handle);
                        self.state.set(Voice::Listening);
                    }
                    Err(e) => self
                        .state
                        .set(Voice::Failed(format!("microphone unavailable: {e}"))),
                }
            }
            Press::StopAndTranscribe => {
                let Some(handle) = self.recording.borrow_mut().take() else {
                    // Nothing was open. Fall back rather than wedging in a
                    // state whose only exit was the recording that is missing.
                    self.state.set(Voice::Ready);
                    return;
                };
                match handle.stop() {
                    Ok(audio) if audio.has_audio() => {
                        // Resampled to 16 kHz mono here, not sent raw. The
                        // microphone runs at whatever rate it likes — 48 kHz on
                        // this machine — and Whisper accepts exactly one shape.
                        match audio.to_whisper_pcm() {
                            Ok(pcm) => {
                                self.state.set(Voice::Transcribing);
                                self.transcriber.transcribe(pcm);
                            }
                            Err(e) => self
                                .state
                                .set(Voice::Failed(format!("could not prepare audio: {e}"))),
                        }
                    }
                    // Silence is not an error and must not look like one — a
                    // mis-press should cost nothing but the press.
                    Ok(_) => self.state.set(Voice::Ready),
                    Err(e) => self
                        .state
                        .set(Voice::Failed(format!("could not stop recording: {e}"))),
                }
            }
            Press::Ignore => {}
        }
    }
}
