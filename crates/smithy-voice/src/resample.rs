use anyhow::{Context, Result};
use rubato::{FftFixedIn, Resampler};

/// Resamples audio from the microphone's native rate to 16kHz mono,
/// which is what Whisper expects.
pub struct AudioResampler {
    resampler: FftFixedIn<f32>,
    input_channels: usize,
}

impl AudioResampler {
    /// Create a new resampler from the given input sample rate and channel count
    /// to 16kHz mono output.
    pub fn new(input_sample_rate: u32, input_channels: u16) -> Result<Self> {
        let resampler = FftFixedIn::<f32>::new(
            input_sample_rate as usize,
            16000,
            1024, // chunk size
            2,    // sub-chunks
            input_channels as usize,
        )
        .context("Failed to create audio resampler")?;

        Ok(Self {
            resampler,
            input_channels: input_channels as usize,
        })
    }

    /// Resample i16 audio to f32 mono at 16kHz.
    /// Input is interleaved multi-channel i16 samples at the original rate.
    pub fn resample_i16_to_f32_16khz(&mut self, samples: &[i16]) -> Result<Vec<f32>> {
        // Convert i16 to f32 and de-interleave into per-channel buffers
        let total_frames = samples.len() / self.input_channels;
        let mut channels: Vec<Vec<f32>> = (0..self.input_channels)
            .map(|_| Vec::with_capacity(total_frames))
            .collect();

        for frame in samples.chunks_exact(self.input_channels) {
            for (ch, &sample) in frame.iter().enumerate() {
                channels[ch].push(sample as f32 / 32768.0);
            }
        }

        // Process through resampler in chunks
        let chunk_size = self.resampler.input_frames_next();
        let mut output_all: Vec<f32> = Vec::new();

        let mut pos = 0;
        while pos + chunk_size <= total_frames {
            let chunk: Vec<Vec<f32>> = channels
                .iter()
                .map(|ch| ch[pos..pos + chunk_size].to_vec())
                .collect();

            let resampled = self
                .resampler
                .process(&chunk, None)
                .context("Resampler processing failed")?;

            // Mix down to mono by averaging channels
            if let Some(first_ch) = resampled.first() {
                if self.input_channels == 1 {
                    output_all.extend_from_slice(first_ch);
                } else {
                    for i in 0..first_ch.len() {
                        let sum: f32 = resampled.iter().map(|ch| ch[i]).sum();
                        output_all.push(sum / self.input_channels as f32);
                    }
                }
            }

            pos += chunk_size;
        }

        // Handle remaining samples by zero-padding
        let remaining = total_frames - pos;
        if remaining > 0 {
            // Zero-padded to a full chunk, which is why the output runs a
            // little past the audio. Harmless downstream: Whisper pads to 30
            // seconds regardless, and `RecordedAudio::to_whisper_pcm` skips this
            // path entirely when the device already gives 16 kHz mono.
            let chunk: Vec<Vec<f32>> = channels
                .iter()
                .map(|ch| {
                    let mut v = ch[pos..].to_vec();
                    v.resize(chunk_size, 0.0);
                    v
                })
                .collect();

            let resampled = self
                .resampler
                .process(&chunk, None)
                .context("Resampler processing failed (tail)")?;

            if let Some(first_ch) = resampled.first() {
                if self.input_channels == 1 {
                    output_all.extend_from_slice(first_ch);
                } else {
                    for i in 0..first_ch.len() {
                        let sum: f32 = resampled.iter().map(|ch| ch[i]).sum();
                        output_all.push(sum / self.input_channels as f32);
                    }
                }
            }
        }

        Ok(output_all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone at `hz`, as interleaved i16 at `rate` across `channels`.
    fn tone(hz: f64, rate: u32, channels: u16, seconds: f64) -> Vec<i16> {
        let frames = (rate as f64 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * channels as usize);
        for frame in 0..frames {
            let t = frame as f64 / rate as f64;
            let value = ((t * hz * std::f64::consts::TAU).sin() * 12_000.0) as i16;
            for _ in 0..channels {
                out.push(value);
            }
        }
        out
    }

    /// Dominant frequency of a tone, in Hz.
    ///
    /// Two details, both learned by getting them wrong and measuring:
    ///
    /// **A Schmitt trigger, not a bare zero crossing.** FFT resampling leaves a
    /// little ringing around each crossing, and counting every sign change reads
    /// that ripple as extra cycles — it reported a clean 440 Hz tone as 473 Hz.
    /// Requiring the signal to swing decisively negative before the next rising
    /// edge counts ignores ripple without ignoring signal.
    ///
    /// **Trailing silence is trimmed.** The resampler zero-pads its final chunk,
    /// so the output runs slightly past the audio; dividing cycles by the full
    /// length flattens the answer by a few percent.
    fn pitch_hz(samples: &[f32], rate: u32) -> f64 {
        let end = samples
            .iter()
            .rposition(|s| s.abs() > 0.01)
            .map(|i| i + 1)
            .unwrap_or(samples.len());
        let samples = &samples[..end];
        if samples.is_empty() {
            return 0.0;
        }

        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        let (high, low) = (peak * 0.25, -peak * 0.25);
        let (mut armed, mut cycles) = (false, 0usize);
        for &s in samples {
            if s < low {
                armed = true;
            } else if s > high && armed {
                cycles += 1;
                armed = false;
            }
        }
        cycles as f64 * rate as f64 / samples.len() as f64
    }

    /// **The property the previous tests did not check.** All four fed silence
    /// and asserted only that the output length was roughly right — which a
    /// resampler that dropped every sample and returned zeros would also
    /// satisfy. Whether the audio survives is the entire job.
    #[test]
    fn a_tone_keeps_its_pitch_through_a_48k_mono_resample() {
        let mut resampler = AudioResampler::new(48_000, 1).unwrap();
        let out = resampler
            .resample_i16_to_f32_16khz(&tone(440.0, 48_000, 1, 1.0))
            .unwrap();

        let hz = pitch_hz(&out, 16_000);
        assert!(
            (hz - 440.0).abs() < 25.0,
            "440 Hz came back as {hz:.1} Hz — the audio did not survive the rate change"
        );
        assert!(
            out.iter().any(|s| s.abs() > 0.1),
            "the output is silent; a length-correct silence passes a length-only test"
        );
    }

    /// Stereo has to be **mixed down**, not read as one stream. Reading
    /// interleaved samples in order doubles the apparent pitch and is what the
    /// model would have received as noise.
    #[test]
    fn a_stereo_tone_is_mixed_to_mono_rather_than_read_interleaved() {
        let mut resampler = AudioResampler::new(44_100, 2).unwrap();
        let out = resampler
            .resample_i16_to_f32_16khz(&tone(440.0, 44_100, 2, 1.0))
            .unwrap();

        let hz = pitch_hz(&out, 16_000);
        assert!(
            (hz - 440.0).abs() < 25.0,
            "440 Hz stereo came back as {hz:.1} Hz; reading the interleaved stream as mono \
             would report about double"
        );
    }

    /// One second in is about one second out, at the new rate. Kept from the
    /// original tests — it is a real property, it was simply the only one.
    #[test]
    fn a_second_of_audio_becomes_a_second_at_sixteen_kilohertz() {
        for (rate, channels) in [(48_000u32, 1u16), (44_100, 2), (16_000, 1)] {
            let mut resampler = AudioResampler::new(rate, channels).unwrap();
            let out = resampler
                .resample_i16_to_f32_16khz(&tone(440.0, rate, channels, 1.0))
                .unwrap();
            assert!(
                out.len() > 15_000 && out.len() < 17_500,
                "{rate} Hz / {channels}ch produced {} samples for one second",
                out.len()
            );
        }
    }
}
