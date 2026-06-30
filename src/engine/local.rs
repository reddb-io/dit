//! Offline Whisper engine — record then transcribe, entirely on-device.
//!
//! Compiled only with `--features local`. Accumulates PCM while the user
//! records, then runs quantised Whisper inference via candle and injects the
//! transcript. No API key or network required.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;
use tracing::{info, warn};

use candle_core::{Device, Tensor};
use candle_core::quantized::gguf_file;
use candle_transformers::quantized_var_builder;
use candle_transformers::models::whisper::{audio, quantized_model, Config as WhisperConfig};

use crate::audio::{CaptureEvent, Resampler};
use crate::config::{Config, SAMPLE_RATE};
use crate::engine::Transcriber;
use crate::inject::Injector;
use crate::notify::notify;
use crate::output::SessionLog;
use crate::IconState;

// Whisper STFT parameters (fixed in candle's audio module).
const N_FFT: usize = 400;
const HOP_LENGTH: usize = 160;
const N_MELS: usize = 80;
// 30 s at 10 ms hop = 3000 frames (Whisper's hard limit).
const WHISPER_MEL_FRAMES: usize = 3000;
const WHISPER_MAX_SAMPLES: usize = WHISPER_MEL_FRAMES * HOP_LENGTH; // 480 000

// Whisper multilingual special tokens.
const SOT: u32 = 50258;
const EOT: u32 = 50256;
const TRANSCRIBE: u32 = 50359;
const NO_TIMESTAMPS: u32 = 50363;

fn language_token(lang: &str) -> u32 {
    match lang {
        "en" => 50259,
        "zh" => 50260,
        "de" => 50261,
        "es" => 50262,
        "ru" => 50263,
        "ko" => 50264,
        "fr" => 50265,
        "ja" => 50266,
        "pt" => 50267,
        "tr" => 50268,
        "pl" => 50269,
        "ca" => 50270,
        "nl" => 50271,
        "ar" => 50272,
        "sv" => 50273,
        "it" => 50274,
        _ => 50267, // default: pt
    }
}

/// Offline Whisper engine. Accumulates PCM during recording then runs batch
/// inference via `spawn_blocking`.
pub struct LocalEngine {
    pub model_path: PathBuf,
}

impl LocalEngine {
    pub fn new(model_path: PathBuf) -> Self {
        Self { model_path }
    }
}

impl Transcriber for LocalEngine {
    async fn run_stream(
        &self,
        cfg: &Config,
        injector: Injector,
        mut audio: mpsc::Receiver<CaptureEvent>,
        audio_stop: Arc<AtomicBool>,
        native_rate: u32,
        stop: Arc<Notify>,
        state: mpsc::UnboundedSender<IconState>,
    ) -> Result<()> {
        let mut resampler = Resampler::new(native_rate, SAMPLE_RATE);
        // Accumulate resampled i16 LE bytes.
        let mut raw_pcm: Vec<u8> = Vec::with_capacity(WHISPER_MAX_SAMPLES * 2);

        notify("🎙️ Dictating (offline)…", "Speak — press the hotkey again to stop");
        info!("local session started (mic {native_rate} Hz → {SAMPLE_RATE} Hz)");

        let mut sumsq = 0.0f64;
        let mut n_level = 0usize;
        let mut last_emit = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = stop.notified() => break,
                maybe = audio.recv() => match maybe {
                    Some(CaptureEvent::Format { sample_rate }) => {
                        resampler = Resampler::new(sample_rate, SAMPLE_RATE);
                    }
                    Some(CaptureEvent::Samples(frame)) => {
                        for &s in &frame {
                            sumsq += (s as f64) * (s as f64);
                            n_level += 1;
                        }
                        resampler.push(&frame, &mut raw_pcm);
                        if last_emit.elapsed() >= Duration::from_millis(200) && n_level > 0 {
                            let rms = (sumsq / n_level as f64).sqrt();
                            let _ = state.send(IconState::Recording { level: vu_level(rms) });
                            sumsq = 0.0;
                            n_level = 0;
                            last_emit = std::time::Instant::now();
                        }
                    }
                    None => break,
                }
            }
        }
        audio_stop.store(true, Ordering::Relaxed);

        // Drain buffered frames.
        while let Ok(Some(ev)) = timeout(Duration::from_millis(50), audio.recv()).await {
            match ev {
                CaptureEvent::Format { sample_rate } => {
                    resampler = Resampler::new(sample_rate, SAMPLE_RATE);
                }
                CaptureEvent::Samples(frame) => {
                    resampler.push(&frame, &mut raw_pcm);
                }
            }
        }

        if raw_pcm.is_empty() {
            info!("local: no audio captured — skipping inference");
            return Ok(());
        }

        let dur = raw_pcm.len() as f64 / (SAMPLE_RATE as f64 * 2.0);
        info!("local: captured {dur:.1}s — running Whisper inference …");

        let model_path = self.model_path.clone();
        let language = cfg.language.clone();
        let session_max_age_days = cfg.session_max_age_days;
        let session_max_count = cfg.session_max_count;

        let transcript = tokio::task::spawn_blocking(move || {
            transcribe_blocking(&model_path, raw_pcm, &language)
        })
        .await
        .context("inference task panicked")??;

        if transcript.is_empty() {
            info!("local: inference produced no text");
        } else {
            info!("local: transcript: {transcript}");
            injector.type_text(format!("{transcript} "));
            let mut log = SessionLog::open(session_max_age_days, session_max_count);
            log.committed(&transcript);
        }

        Ok(())
    }

    async fn transcribe_batch(&self, pcm: Vec<i16>, language: &str) -> Result<String> {
        let raw: Vec<u8> = pcm.iter().flat_map(|&s| s.to_le_bytes()).collect();
        let model_path = self.model_path.clone();
        let lang = language.to_string();
        tokio::task::spawn_blocking(move || transcribe_blocking(&model_path, raw, &lang))
            .await
            .context("inference task panicked")?
    }
}

// ── Blocking inference ────────────────────────────────────────────────────────

fn transcribe_blocking(model_path: &PathBuf, raw_pcm: Vec<u8>, language: &str) -> Result<String> {
    let device = Device::Cpu;
    let config = whisper_config(model_path);
    let vocab = load_gguf_vocab(model_path).unwrap_or_default();

    let vb = quantized_var_builder::VarBuilder::from_gguf(model_path, &device)
        .context("loading GGUF model weights")?;
    let mut model = quantized_model::Whisper::load(&vb, config.clone())
        .context("building Whisper model")?;

    // Convert i16 LE bytes → normalised f32.
    let mut samples: Vec<f32> = raw_pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    // Pad to exactly 30 s so the encoder sees a full receptive field.
    if samples.len() < WHISPER_MAX_SAMPLES {
        samples.resize(WHISPER_MAX_SAMPLES, 0.0);
    } else {
        samples.truncate(WHISPER_MAX_SAMPLES);
    }

    // Mel spectrogram.
    let mel_filters = compute_mel_filters(SAMPLE_RATE as usize, N_FFT, N_MELS);
    let mel_flat = audio::pcm_to_mel(&config, &samples, &mel_filters);
    let mel_frames = mel_flat.len() / N_MELS;
    let mel = Tensor::from_vec(mel_flat, (1, N_MELS, mel_frames), &device)
        .context("mel tensor")?;

    // Encode.
    let features = model.encoder.forward(&mel, true).context("encoder")?;

    // Greedy decode (full-sequence, no incremental KV cache).
    let lang_tok = language_token(language);
    let mut tokens: Vec<u32> = vec![SOT, lang_tok, TRANSCRIBE, NO_TIMESTAMPS];
    let mut output: Vec<u32> = vec![];

    for _ in 0..config.max_target_positions {
        let t_in = Tensor::from_slice(tokens.as_slice(), (1, tokens.len()), &device)
            .context("decoder input")?;
        let logits = model.decoder.forward(&t_in, &features, true).context("decoder")?;
        // logits: (1, seq_len, vocab_size) → pick last position.
        let last = logits.get(0)?.get(tokens.len() - 1)?;
        let next = last.argmax(0)?.to_scalar::<u32>().context("argmax")?;
        if next == EOT {
            break;
        }
        // Skip timestamp tokens (50364+) — we requested NO_TIMESTAMPS but guard anyway.
        if next < 50257 {
            output.push(next);
        }
        tokens.push(next);
        if output.len() >= config.max_target_positions {
            warn!("local: decoder hit max length without EOT");
            break;
        }
    }

    Ok(decode_tokens(&output, &vocab).trim().to_string())
}

// ── Model config ──────────────────────────────────────────────────────────────

fn whisper_config(model_path: &PathBuf) -> WhisperConfig {
    let is_base = model_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .contains("base");

    if is_base {
        serde_json::from_str(r#"{
            "num_mel_bins": 80, "max_source_positions": 1500, "d_model": 512,
            "encoder_attention_heads": 8, "encoder_layers": 6,
            "decoder_attention_heads": 8, "decoder_layers": 6,
            "vocab_size": 51865, "max_target_positions": 448
        }"#)
        .expect("base config")
    } else {
        serde_json::from_str(r#"{
            "num_mel_bins": 80, "max_source_positions": 1500, "d_model": 384,
            "encoder_attention_heads": 6, "encoder_layers": 4,
            "decoder_attention_heads": 6, "decoder_layers": 4,
            "vocab_size": 51865, "max_target_positions": 448
        }"#)
        .expect("tiny config")
    }
}

// ── Vocabulary ────────────────────────────────────────────────────────────────

fn load_gguf_vocab(model_path: &PathBuf) -> Result<Vec<String>> {
    let mut f = std::fs::File::open(model_path)
        .with_context(|| format!("opening {}", model_path.display()))?;
    let content = gguf_file::Content::read(&mut f).context("reading GGUF metadata")?;
    match content.metadata.get("tokenizer.ggml.tokens") {
        Some(gguf_file::Value::Array(items)) => {
            let vocab: Vec<String> = items
                .iter()
                .filter_map(|v| {
                    if let gguf_file::Value::String(s) = v {
                        Some(s.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            Ok(vocab)
        }
        _ => bail!("tokenizer.ggml.tokens not found in GGUF metadata"),
    }
}

fn decode_tokens(tokens: &[u32], vocab: &[String]) -> String {
    let map = build_unicode_to_byte_map();
    let mut bytes: Vec<u8> = Vec::new();
    for &tid in tokens {
        if let Some(s) = vocab.get(tid as usize) {
            for c in s.chars() {
                if let Some(&b) = map.get(&c) {
                    bytes.push(b);
                }
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn build_unicode_to_byte_map() -> std::collections::HashMap<char, u8> {
    // GPT-2 byte-level BPE inverse map: unicode char → raw byte value.
    // Printable latin chars (!, ¡–¬, ®–ÿ) map to themselves; the remaining
    // 33 bytes (0–32, 127, 128–160, 173) map to codepoints starting at 256.
    let mut m: std::collections::HashMap<char, u8> = std::collections::HashMap::new();
    for b in 33u8..=126 {
        m.insert(b as char, b);
    }
    for b in 161u8..=172 {
        m.insert(b as char, b);
    }
    for b in 174u8..=255 {
        m.insert(b as char, b);
    }
    let mut cp = 256u32;
    for b in 0u8..=255 {
        let c = b as char;
        if !m.contains_key(&c) {
            if let Some(ch) = char::from_u32(cp) {
                m.insert(ch, b);
            }
            cp += 1;
        }
    }
    m
}

// ── Mel filterbank ────────────────────────────────────────────────────────────

fn compute_mel_filters(sample_rate: usize, n_fft: usize, n_mels: usize) -> Vec<f32> {
    let fmax = sample_rate as f64 / 2.0;
    let n_freqs = n_fft / 2 + 1;

    let hz_to_mel = |hz: f64| 2595.0 * (1.0 + hz / 700.0).log10();
    let mel_to_hz = |mel: f64| 700.0 * (10.0f64.powf(mel / 2595.0) - 1.0);

    let mel_min = hz_to_mel(0.0);
    let mel_max = hz_to_mel(fmax);

    let mel_pts: Vec<f64> = (0..=n_mels + 1)
        .map(|i| mel_min + (mel_max - mel_min) * i as f64 / (n_mels + 1) as f64)
        .collect();
    let bin_pts: Vec<f64> = mel_pts
        .iter()
        .map(|&m| mel_to_hz(m) * (n_fft as f64 + 1.0) / sample_rate as f64)
        .collect();

    let mut filters = vec![0.0f32; n_mels * n_freqs];
    for m in 0..n_mels {
        let (f0, f1, f2) = (bin_pts[m], bin_pts[m + 1], bin_pts[m + 2]);
        for ki in 0..n_freqs {
            let k = ki as f64;
            let w = if k < f0 || k > f2 {
                0.0
            } else if k <= f1 {
                (k - f0) / (f1 - f0)
            } else {
                (f2 - k) / (f2 - f1)
            };
            filters[m * n_freqs + ki] = w as f32;
        }
    }
    filters
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn vu_level(rms: f64) -> u8 {
    if rms <= 0.0005 {
        return 0;
    }
    ((rms.min(0.20) / 0.20).sqrt() * 255.0).round() as u8
}
