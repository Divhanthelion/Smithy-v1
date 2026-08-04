use anyhow::{Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self as m, model::Whisper, Config};
use std::sync::mpsc;
use tokenizers::Tokenizer;

use crate::model::ModelConfig;
use crate::voice_debug;

/// What the weights are held as in memory.
///
/// **Not `m::DTYPE`, which is f32.** `whisper-large-v3-turbo` ships f16 on
/// disk — 1.51 GiB for 809M parameters — and loading it as f32 widened every
/// weight on the way in for nothing: 809M × 4 bytes ≈ 3.2 GB resident, which is
/// what the memory meter was reporting and roughly sixteen times the rest of
/// the editor. Held as it arrives, it is half that.
///
/// Speech recognition is not a precision-sensitive workload — the model was
/// trained in mixed precision and is judged on word error rate, not on the
/// third decimal place of a logit. The decode loop still argmaxes in f32; see
/// `transcribe`, which converts the logits back at the one point it matters.
const DTYPE: DType = DType::F16;

/// What the supervisor is told to do.
pub enum Command {
    Load(ModelConfig),
    /// PCM, f32, 16 kHz, mono — the only shape Whisper accepts.
    Transcribe(Vec<f32>),
    Shutdown,
}

/// What comes back.
///
/// Events on a channel rather than ottex's `oneshot` replies, because the
/// caller here is a UI thread that cannot await anything. This is the shape the
/// rest of Smithy already uses to get work off a thread and onto the screen —
/// `app_state::bridge` ticks a signal and the effect drains the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Loaded,
    Transcribed(String),
    /// Loading or transcription failed, with a sentence saying which and why.
    Failed(String),
}

/// A handle to the thread Whisper runs on.
///
/// **A dedicated OS thread, not a task.** Decoding is seconds of solid compute
/// with no await points in it; on any shared executor it would block whatever
/// else was scheduled there, and on the UI thread it would freeze the window.
pub struct Transcriber {
    tx: mpsc::Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Transcriber {
    /// Spawn the supervisor. Nothing is loaded until [`Self::load`] is called.
    pub fn new(events: crossbeam_channel::Sender<Event>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();

        let thread = std::thread::Builder::new()
            .name("smithy-voice".into())
            .spawn(move || Self::run_loop(rx, events))
            .expect("failed to spawn the voice thread");

        Self {
            tx,
            thread: Some(thread),
        }
    }

    /// Fetch and load the model. Answers with [`Event::Loaded`] or
    /// [`Event::Failed`].
    pub fn load(&self, config: ModelConfig) {
        let _ = self.tx.send(Command::Load(config));
    }

    /// Transcribe captured audio. Answers with [`Event::Transcribed`].
    pub fn transcribe(&self, audio_f32_16khz: Vec<f32>) {
        let _ = self.tx.send(Command::Transcribe(audio_f32_16khz));
    }

    fn run_loop(rx: mpsc::Receiver<Command>, events: crossbeam_channel::Sender<Event>) {
        let mut engine: Option<WhisperEngine> = None;

        while let Ok(command) = rx.recv() {
            match command {
                Command::Load(config) => {
                    voice_debug!("loading {} …", config.repo_id);
                    match WhisperEngine::load(&config) {
                        Ok(loaded) => {
                            voice_debug!("model ready");
                            engine = Some(loaded);
                            let _ = events.send(Event::Loaded);
                        }
                        Err(e) => {
                            voice_debug!("load failed: {e:#}");
                            let _ = events.send(Event::Failed(describe(&e)));
                        }
                    }
                }
                Command::Transcribe(audio) => {
                    let seconds = audio.len() as f64 / 16_000.0;
                    voice_debug!("transcribing {seconds:.1}s");
                    let outcome = match &mut engine {
                        Some(engine) => engine.transcribe(&audio),
                        // Unreachable through the state machine, which will not
                        // record before the model is ready — but a channel is a
                        // channel and this is cheaper than a panic.
                        None => Err(anyhow::anyhow!("the model is not loaded yet")),
                    };
                    let _ = match outcome {
                        Ok(text) => {
                            voice_debug!("heard: {text:?}");
                            events.send(Event::Transcribed(text))
                        }
                        Err(e) => {
                            voice_debug!("transcription failed: {e:#}");
                            events.send(Event::Failed(describe(&e)))
                        }
                    };
                }
                Command::Shutdown => break,
            }
        }
        voice_debug!("voice thread finished");
    }
}

impl Drop for Transcriber {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whisper inference engine using candle-transformers
struct WhisperEngine {
    model: Whisper,
    tokenizer: Tokenizer,
    mel_filters: Vec<f32>,
    config: Config,
    device: Device,
    // Special token IDs
    sot_token: u32,
    eot_token: u32,
    transcribe_token: u32,
    no_timestamps_token: u32,
    suppress_tokens: Tensor,
}

impl WhisperEngine {
    fn load(local_config: &ModelConfig) -> Result<Self> {
        let device = Device::Cpu;

        // Use hf-hub to download/cache model files
        // **Into Smithy's own data directory**, not `~/.cache/huggingface`.
        //
        // `Api::new()` was used here, which ignores `cache_dir` entirely — so
        // the field was computed, stored, documented, asserted by a test, and
        // never handed to anything. A gigabyte of weights went somewhere the
        // documentation did not say, and deleting Smithy's data directory left
        // them behind.
        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_cache_dir(std::path::PathBuf::from(&local_config.cache_dir))
            .build()
            .context("Failed to create HF Hub API")?;
        let repo = api.repo(hf_hub::Repo::with_revision(
            local_config.repo_id.clone(),
            hf_hub::RepoType::Model,
            local_config.revision.clone(),
        ));

        voice_debug!(
            "Downloading/caching model files from {}...",
            local_config.repo_id
        );

        let config_path = repo
            .get("config.json")
            .context("Failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("Failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("Failed to download model.safetensors")?;

        voice_debug!("Model files cached, loading...");

        // Load config
        let config_str =
            std::fs::read_to_string(&config_path).context("Failed to read config.json")?;
        let config: Config =
            serde_json::from_str(&config_str).context("Failed to parse config.json")?;

        // Load mel filters from embedded bytes based on num_mel_bins
        let mel_filters = Self::load_mel_filters(config.num_mel_bins)?;

        // Load tokenizer
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)
                .context("Failed to load model weights")?
        };
        let model = Whisper::load(&vb, config.clone()).context("Failed to build Whisper model")?;

        // Resolve special tokens
        let sot_token = Self::token_id(&tokenizer, m::SOT_TOKEN)?;
        let eot_token = Self::token_id(&tokenizer, m::EOT_TOKEN)?;
        let transcribe_token = Self::token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let no_timestamps_token = Self::token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;

        // Build suppress tokens tensor
        let suppress_tokens = Self::build_suppress_tokens(&config, &device)?;

        voice_debug!(
            "Whisper model ready (encoder: {} layers, decoder: {} layers, mel bins: {})",
            config.encoder_layers,
            config.decoder_layers,
            config.num_mel_bins
        );

        Ok(Self {
            model,
            tokenizer,
            mel_filters,
            config,
            device,
            sot_token,
            eot_token,
            transcribe_token,
            no_timestamps_token,
            suppress_tokens,
        })
    }

    fn load_mel_filters(num_mel_bins: usize) -> Result<Vec<f32>> {
        // The mel filter bytes are standard Whisper assets.
        // We embed the 80-bin and 128-bin versions.
        let mel_bytes: &[u8] = match num_mel_bins {
            80 => include_bytes!("mel_filters_80.bytes"),
            128 => include_bytes!("mel_filters_128.bytes"),
            n => anyhow::bail!("Unsupported num_mel_bins: {}", n),
        };

        let mut filters = vec![0f32; mel_bytes.len() / 4];
        use byteorder::{ByteOrder, LittleEndian};
        LittleEndian::read_f32_into(mel_bytes, &mut filters);
        Ok(filters)
    }

    fn token_id(tokenizer: &Tokenizer, token: &str) -> Result<u32> {
        tokenizer
            .token_to_id(token)
            .ok_or_else(|| anyhow::anyhow!("Token not found in tokenizer: {}", token))
    }

    fn build_suppress_tokens(config: &Config, device: &Device) -> Result<Tensor> {
        let mut suppress = vec![0f32; config.vocab_size];
        for &token_id in &config.suppress_tokens {
            if (token_id as usize) < config.vocab_size {
                suppress[token_id as usize] = f32::NEG_INFINITY;
            }
        }
        let tensor =
            Tensor::from_vec(suppress, config.vocab_size, device).context("suppress tensor")?;
        Ok(tensor)
    }

    /// Transcribe f32 PCM audio at 16kHz mono.
    fn transcribe(&mut self, audio_f32: &[f32]) -> Result<String> {
        let start = std::time::Instant::now();

        // Pad or truncate to 30-second chunks and process each
        let mut all_text = String::new();
        let chunk_samples = m::N_SAMPLES; // 480000 = 30s at 16kHz

        let mut offset = 0;
        while offset < audio_f32.len() {
            let end = (offset + chunk_samples).min(audio_f32.len());
            let chunk = &audio_f32[offset..end];

            // Pad to exactly 30 seconds if shorter
            let padded: Vec<f32> = if chunk.len() < chunk_samples {
                let mut p = chunk.to_vec();
                p.resize(chunk_samples, 0.0);
                p
            } else {
                chunk.to_vec()
            };

            let text = self.transcribe_chunk(&padded)?;
            all_text.push_str(&text);

            offset += chunk_samples;
        }

        let elapsed = start.elapsed();
        voice_debug!(
            "Transcription complete in {:.2}s for {:.2}s of audio",
            elapsed.as_secs_f32(),
            audio_f32.len() as f32 / 16000.0
        );

        Ok(all_text.trim().to_string())
    }

    fn transcribe_chunk(&mut self, audio_30s: &[f32]) -> Result<String> {
        // Reset KV cache for fresh decoding
        self.model.reset_kv_cache();
        // Compute mel spectrogram using candle's built-in function
        let mel = m::audio::pcm_to_mel(&self.config, audio_30s, &self.mel_filters);
        let n_mels = self.config.num_mel_bins;
        let produced = mel.len() / n_mels;

        let mel = Tensor::from_vec(mel, (1, n_mels, produced), &self.device)
            .context("Failed to create mel tensor")?;
        // **Trimmed to the window the encoder was built for.** See
        // `encoder_frames` — feeding what `pcm_to_mel` returns is what produced
        // "narrow invalid args start + len > dim_len: [1500, 1280] … len: 2250".
        let mel = mel
            .narrow(2, 0, encoder_frames(produced))
            .context("Failed to trim the mel spectrogram to the encoder window")?;
        // `pcm_to_mel` returns f32 and the weights are [`DTYPE`]. candle will
        // not mix them — the encoder's first matmul fails on the mismatch
        // rather than converting for you.
        let mel = mel
            .to_dtype(DTYPE)
            .context("Failed to convert the mel spectrogram to the model's dtype")?;

        // Encode
        let audio_features = self
            .model
            .encoder
            .forward(&mel, true)
            .context("Encoder forward failed")?;

        // Build initial token sequence
        let mut tokens: Vec<u32> = vec![
            self.sot_token,
            // Language token for English (token ID for "<|en|>" is typically sot_token + 1 for en)
            self.sot_token + 1,
            self.transcribe_token,
            self.no_timestamps_token,
        ];

        let sample_len = 224; // Max tokens to generate

        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;

            let ys = self
                .model
                .decoder
                .forward(&tokens_t, &audio_features, i == 0)
                .context("Decoder forward failed")?;

            let seq_len = tokens.len();
            let logits = self
                .model
                .decoder
                .final_linear(&ys.i((..1, seq_len - 1..))?)?
                .squeeze(0)?
                .squeeze(0)?
                // Back to f32 for the one step that cares. The suppress mask is
                // f32 and carries `-inf`, which f16 cannot hold as anything but
                // an overflow, and the argmax below reads `Vec<f32>`. Converting
                // here keeps both correct and costs one vocab-sized vector.
                .to_dtype(DType::F32)
                .context("Failed to convert logits for sampling")?;

            // Apply suppress tokens
            let logits = logits.broadcast_add(&self.suppress_tokens)?;

            // Greedy argmax
            let logits_v: Vec<f32> = logits.to_vec1()?;
            let next_token = logits_v
                .iter()
                .enumerate()
                .max_by(|(_, a): &(usize, &f32), (_, b): &(usize, &f32)| a.total_cmp(b))
                .map(|(idx, _)| idx as u32)
                .unwrap_or(self.eot_token);

            if next_token == self.eot_token {
                break;
            }

            tokens.push(next_token);
        }

        // Decode tokens to text, skipping special tokens
        let text_tokens: Vec<u32> = tokens
            .into_iter()
            .skip(4) // Skip SOT, language, transcribe, no_timestamps
            .collect();

        let text = self
            .tokenizer
            .decode(&text_tokens, true)
            .map_err(|e| anyhow::anyhow!("Token decoding failed: {}", e))?;

        Ok(text)
    }
}

/// How many mel frames the encoder may be given, of however many were produced.
///
/// **`pcm_to_mel` returns more frames than the encoder can accept, on purpose,
/// and the caller has to trim.** candle pads "with at least one extra chunk of
/// zeros": it rounds the frame count up to a multiple of `100 * CHUNK_LENGTH / 2`
/// — 1500 — and then adds 1500 more unconditionally. So a full 30-second chunk
/// of 480,000 samples gives 3000 frames, which is already a multiple, and comes
/// back as **4500**.
///
/// Handing all 4500 to the encoder is what this exists to stop. The conv stack
/// halves them to 2250, and the encoder then narrows its positional embedding —
/// shape `[1500, 1280]`, because Whisper's audio context is exactly
/// [`m::N_FRAMES`] / 2 — by 2250:
///
/// ```text
/// Encoder forward failed: narrow invalid args start + len > dim_len:
///   [1500, 1280], dim: 0, start: 0, len: 2250
/// ```
///
/// Which is not an intermittent failure or a bad recording. Transcription could
/// never have worked, for any audio, since the model runs immediately after this.
///
/// `min` rather than an assertion: a chunk shorter than 30 seconds is padded to
/// length before it gets here, so a smaller mel is not expected — but refusing to
/// transcribe would be a worse answer than transcribing what there is.
fn encoder_frames(produced: usize) -> usize {
    m::N_FRAMES.min(produced)
}

/// A failure, as a sentence somebody can act on.
///
/// The chain matters here more than usual: "load failed" is useless, whereas
/// "no such host: huggingface.co" says the network is down and
/// "Permission denied" says the cache directory is not writable. Those have
/// entirely different fixes and the outer message distinguishes none of them.
fn describe(error: &anyhow::Error) -> String {
    let mut parts = vec![error.to_string()];
    parts.extend(error.chain().skip(1).map(|cause| cause.to_string()));
    parts.join(": ")
}
