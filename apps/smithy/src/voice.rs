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
pub struct VoiceControl {
    transcriber: Transcriber,
    recorder: AudioRecorder,
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
    pub fn new(state: RwSignal<Voice>, input: RwSignal<String>) -> Option<Rc<Self>> {
        let recorder = match AudioRecorder::new() {
            Ok(recorder) => recorder,
            Err(e) => {
                // Not a crash — the button simply goes dim.
                //
                // **Not "no microphone".** It said that for a while, on a
                // machine with a working microphone, because this error covers
                // every reason the input device could not be opened: no default
                // device selected, the OS never granted the permission, another
                // process holding it exclusively. Naming the one cause it is
                // usually *not* sends you looking at hardware. The detail is in
                // the message and the panel puts it under a hover.
                state.set(Voice::Failed(format!("microphone unavailable: {e}")));
                return None;
            }
        };

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

        Some(Rc::new(Self {
            transcriber: Transcriber::new(tx),
            recorder,
            recording: RefCell::new(None),
            state,
        }))
    }

    /// One press of the microphone, or of its hotkey.
    pub fn press(&self) {
        match press(&self.state.get_untracked()) {
            Press::LoadModel => {
                self.state.set(Voice::Loading);
                self.transcriber.load(smithy_voice::ModelConfig::default());
            }
            Press::StartRecording => match self.recorder.start_recording() {
                Ok(handle) => {
                    *self.recording.borrow_mut() = Some(handle);
                    self.state.set(Voice::Listening);
                }
                Err(e) => self
                    .state
                    .set(Voice::Failed(format!("could not record: {e}"))),
            },
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
