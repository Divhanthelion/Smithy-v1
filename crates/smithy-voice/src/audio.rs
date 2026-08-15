use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use std::sync::{Arc, Mutex};

/// A device sample to `i16`, which is what the recording buffer holds.
///
/// These are free functions, and that is the point. They lived inside the
/// stream callbacks, where nothing could reach them — so their two tests
/// reimplemented the arithmetic in the test body and asserted on *that*.
/// Deleting the real conversion, or inverting it, left both passing.
fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

/// Unsigned device samples are centred on 32768 rather than on zero.
fn u16_to_i16(sample: u16) -> i16 {
    (sample as i32 - 32768) as i16
}

/// Audio recorder that captures microphone input
pub struct AudioRecorder {
    device: Device,
    config: StreamConfig,
    sample_format: SampleFormat,
}

/// Recorded audio data
pub struct RecordedAudio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioRecorder {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("No input device available")?;

        crate::voice_debug!("Using input device: {}", device.name().unwrap_or_default());

        let supported_config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        crate::voice_debug!(
            "Default input config: {} Hz, {} channels, {:?}",
            supported_config.sample_rate().0,
            supported_config.channels(),
            supported_config.sample_format()
        );

        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();

        Ok(Self {
            device,
            config,
            sample_format,
        })
    }

    /// Start recording and return a handle to control the recording
    pub fn start_recording(&self) -> Result<RecordingHandle> {
        let samples: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
        let samples_clone = Arc::clone(&samples);

        let err_fn = |err| {
            crate::voice_debug!("Audio stream error: {}", err);
        };

        let stream = match self.sample_format {
            SampleFormat::I16 => {
                let samples = samples_clone;
                self.device.build_input_stream(
                    &self.config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut buffer) = samples.lock() {
                            buffer.extend_from_slice(data);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::F32 => {
                let samples = samples_clone;
                self.device.build_input_stream(
                    &self.config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut buffer) = samples.lock() {
                            for &sample in data {
                                buffer.push(f32_to_i16(sample));
                            }
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let samples = samples_clone;
                self.device.build_input_stream(
                    &self.config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        if let Ok(mut buffer) = samples.lock() {
                            for &sample in data {
                                buffer.push(u16_to_i16(sample));
                            }
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            _ => {
                anyhow::bail!("Unsupported sample format: {:?}", self.sample_format);
            }
        };

        stream.play().context("Failed to start audio stream")?;
        crate::voice_debug!("Recording started");

        Ok(RecordingHandle {
            stream,
            samples,
            sample_rate: self.config.sample_rate.0,
            channels: self.config.channels,
        })
    }
}

/// Handle to an active recording
pub struct RecordingHandle {
    stream: Stream,
    samples: Arc<Mutex<Vec<i16>>>,
    sample_rate: u32,
    channels: u16,
}

impl RecordingHandle {
    /// Stop recording and return the recorded audio
    pub fn stop(self) -> Result<RecordedAudio> {
        let RecordingHandle {
            stream,
            samples,
            sample_rate,
            channels,
        } = self;
        drop(stream);
        crate::voice_debug!("Recording stopped");

        let samples = samples
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to lock samples buffer"))?
            .clone();

        crate::voice_debug!(
            "Recorded {} samples ({:.2} seconds)",
            samples.len(),
            samples.len() as f32 / sample_rate as f32 / channels as f32
        );

        Ok(RecordedAudio {
            samples,
            sample_rate,
            channels,
        })
    }
}

impl RecordedAudio {
    // WAV encoding is deliberately absent. Ottex needed it to POST audio to a
    // transcription API; this crate decodes in-process and never serialises a
    // recording at all — which is also the privacy claim, so it is worth the
    // dependency staying out of the manifest rather than merely unused.
    /// The recording as **16 kHz mono f32** — the only shape Whisper accepts.
    ///
    /// This is the whole reason [`crate::resample`] exists, and nothing called
    /// it: the microphone's raw samples went straight to the model. A default
    /// input device here runs at 48 kHz, so the model was being handed audio at
    /// three times the rate it interprets — and interleaved, if the device is
    /// stereo, which reads as noise rather than as speech. The recording was
    /// captured perfectly; it was unintelligible by the time it arrived.
    ///
    /// Already-correct audio is passed straight through, which is not just an
    /// optimisation: the resampler works in fixed-size chunks and zero-pads the
    /// tail, so running it needlessly appends silence to every recording.
    pub fn to_whisper_pcm(&self) -> Result<Vec<f32>> {
        if self.sample_rate == 16_000 && self.channels == 1 {
            return Ok(self.samples.iter().map(|&s| s as f32 / 32768.0).collect());
        }
        crate::resample::AudioResampler::new(self.sample_rate, self.channels)?
            .resample_i16_to_f32_16khz(&self.samples)
    }

    /// Check if the recording has meaningful audio content
    pub fn has_audio(&self) -> bool {
        // Check if there are enough samples and they're not all silent
        if self.samples.len() < 1000 {
            return false;
        }

        // Calculate RMS to check for silence
        let sum_squares: f64 = self.samples.iter().map(|&s| (s as f64).powi(2)).sum();
        let rms = (sum_squares / self.samples.len() as f64).sqrt();

        // Threshold for "not silent" - adjust as needed
        rms > 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_audio(samples: Vec<i16>) -> RecordedAudio {
        RecordedAudio {
            samples,
            sample_rate: 48000,
            channels: 1,
        }
    }

    /// **These call the conversion, which is the whole change.** The previous
    /// pair reimplemented the arithmetic inside the test and asserted on their
    /// own copy — they could not fail, whatever the recorder did.
    #[test]
    fn a_float_sample_keeps_its_sign_and_scale() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert!(f32_to_i16(0.5) > 0 && f32_to_i16(-0.5) < 0);
        // Half amplitude is about half of full scale.
        assert!((f32_to_i16(0.5) as i32 - i16::MAX as i32 / 2).abs() < 2);
    }

    /// A device that hands back more than full scale must not wrap to the
    /// opposite sign, which is what an unclamped cast does and what it sounds
    /// like is a loud click.
    #[test]
    fn a_float_sample_beyond_full_scale_clamps_rather_than_wrapping() {
        assert_eq!(f32_to_i16(1.5), i16::MAX);
        assert_eq!(f32_to_i16(-1.5), -i16::MAX);
        assert!(
            f32_to_i16(2.0) > 0,
            "an overloaded sample must stay positive"
        );
    }

    /// Unsigned formats are centred on 32768, so silence is the midpoint and
    /// not zero. Reading them as signed turns quiet into full-scale noise.
    #[test]
    fn an_unsigned_sample_is_recentred_on_zero() {
        assert_eq!(u16_to_i16(32_768), 0);
        assert_eq!(u16_to_i16(0), i16::MIN);
        assert_eq!(u16_to_i16(65_535), i16::MAX);
    }

    /// Silence must not be sent to the model: it costs seconds of decode and
    /// comes back as invented words.
    #[test]
    fn silence_is_not_treated_as_speech() {
        assert!(!make_audio(vec![0; 2000]).has_audio());
    }

    /// Too short to be speech is also not speech — a mis-press should cost
    /// nothing but the press.
    #[test]
    fn a_recording_too_brief_to_be_speech_is_rejected() {
        assert!(!make_audio(vec![1000; 500]).has_audio());
    }

    /// And the negative control, without which the two above would pass on a
    /// function that always said no.
    #[test]
    fn a_recording_with_real_amplitude_is_accepted() {
        let samples: Vec<i16> = (0..2000)
            .map(|i| ((i % 100) as i16 * 200) - 10000)
            .collect();
        assert!(make_audio(samples).has_audio());
    }
}
