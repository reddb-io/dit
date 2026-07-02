//! Offline Whisper engine — record then transcribe, entirely on-device.
//!
//! Compiled only with `--features local`. Accumulates PCM while the user
//! records, then runs quantised Whisper inference via candle and injects the
//! transcript. No API key or network required.
//!
//! Audio of any length is handled by splitting it into Whisper's native 30 s
//! windows and transcribing them in sequence with the same loaded model, so
//! nothing said is dropped. A window boundary can split a word; for dictation
//! the trade-off (rare clipped word vs. silently losing everything past 30 s)
//! is the right one.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, Notify};
use tracing::{info, warn};

use candle_core::{Device, Tensor};
use candle_transformers::models::whisper::{audio, quantized_model, Config as WhisperConfig};
use candle_transformers::quantized_var_builder;

use crate::audio::{drain_capture, AudioLevel, CaptureEvent, Resampler};
use crate::config::{Config, SAMPLE_RATE};
use crate::engine::Transcriber;
use crate::inject::Injector;
use crate::models::{LocalModel, LocalModelKind};
use crate::notify::notify;
use crate::output::SessionLog;
use crate::IconState;

// Whisper STFT parameters (fixed in candle's audio module).
const HOP_LENGTH: usize = 160;
const N_MELS: usize = 80;
// 30 s at 10 ms hop = 3000 frames (Whisper's per-window limit).
const WHISPER_MEL_FRAMES: usize = 3000;
const WHISPER_WINDOW_SAMPLES: usize = WHISPER_MEL_FRAMES * HOP_LENGTH; // 480 000

// Whisper multilingual special tokens.
const SOT: u32 = 50258;
const EOT: u32 = 50256;
const TRANSCRIBE: u32 = 50359;
const NO_TIMESTAMPS: u32 = 50363;
// Language tokens occupy 50259..=50357, in the order of `WHISPER_LANGUAGES`.
const LANG_TOK_START: u32 = 50259;
const LANG_TOK_END: u32 = 50357;
// Everything from 50257 up is a special/added token, never plain text.
const FIRST_SPECIAL_TOKEN: u32 = 50257;

/// The 99 languages of multilingual Whisper, in language-token order:
/// `WHISPER_LANGUAGES[i]` has token id `LANG_TOK_START + i`.
const WHISPER_LANGUAGES: &[&str] = &[
    "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it",
    "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur",
    "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn",
    "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si",
    "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo",
    "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln",
    "ha", "ba", "jw", "su",
];

/// The precomputed slaney-normalised mel filterbank Whisper was trained with
/// (80 mels × 201 FFT bins of little-endian f32), taken verbatim from candle's
/// whisper example. Embedding the known-good table avoids drifting from the
/// reference implementation.
const MEL_FILTERS_BYTES: &[u8] = include_bytes!("melfilters.bytes");

/// Token id for a language code, or the `pt` token (dit's default language)
/// with a warning when the code isn't one Whisper knows.
fn language_token(lang: &str) -> u32 {
    match WHISPER_LANGUAGES.iter().position(|&l| l == lang) {
        Some(i) => LANG_TOK_START + i as u32,
        None => {
            warn!("local: Whisper has no language {lang:?}; falling back to pt");
            language_token("pt")
        }
    }
}

/// Offline Whisper engine. Accumulates PCM during recording then runs batch
/// inference via `spawn_blocking`.
pub struct LocalEngine {
    model: LocalModel,
}

impl LocalEngine {
    pub fn new(model: LocalModel) -> Self {
        Self { model }
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
        let mut raw_pcm: Vec<u8> = Vec::with_capacity(WHISPER_WINDOW_SAMPLES * 2);

        notify(
            "🎙️ Dictating (offline)…",
            "Speak — press the hotkey again to stop",
        );
        info!("local session started (mic {native_rate} Hz → {SAMPLE_RATE} Hz)");

        let mut level = AudioLevel::default();
        loop {
            tokio::select! {
                _ = stop.notified() => break,
                maybe = audio.recv() => match maybe {
                    Some(CaptureEvent::Format { sample_rate }) => {
                        resampler = Resampler::new(sample_rate, SAMPLE_RATE);
                    }
                    Some(CaptureEvent::Samples(frame)) => {
                        level.add(&frame);
                        resampler.push(&frame, &mut raw_pcm);
                        if let Some(lv) = level.maybe_emit() {
                            let _ = state.send(IconState::Recording { level: lv });
                        }
                    }
                    None => break,
                }
            }
        }
        audio_stop.store(true, Ordering::Relaxed);
        drain_capture(&mut audio, &mut resampler, &mut raw_pcm).await;

        if raw_pcm.is_empty() {
            info!("local: no audio captured — skipping inference");
            return Ok(());
        }

        let dur = raw_pcm.len() as f64 / (SAMPLE_RATE as f64 * 2.0);
        info!("local: captured {dur:.1}s — running Whisper inference …");

        let model = self.model.clone();
        let language = cfg.language.clone();
        let session_max_age_days = cfg.session_max_age_days;
        let session_max_count = cfg.session_max_count;

        let transcript =
            tokio::task::spawn_blocking(move || transcribe_blocking(&model, raw_pcm, &language))
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

    async fn transcribe_batch(&self, cfg: &Config, pcm: Vec<i16>) -> Result<String> {
        let raw: Vec<u8> = pcm.iter().flat_map(|&s| s.to_le_bytes()).collect();
        let model = self.model.clone();
        let language = cfg.language.clone();
        tokio::task::spawn_blocking(move || transcribe_blocking(&model, raw, &language))
            .await
            .context("inference task panicked")?
    }
}

// ── Blocking inference ────────────────────────────────────────────────────────

/// Transcribe raw s16le PCM (16 kHz mono) of any length: load the model once,
/// then run each 30 s window through encode → greedy decode and join the texts.
fn transcribe_blocking(model: &LocalModel, raw_pcm: Vec<u8>, language: &str) -> Result<String> {
    let device = Device::Cpu;
    let config = whisper_config(model.kind);
    let vocab = load_tokenizer_vocab(model)?;
    let mel_filters = load_mel_filters();

    let vb = quantized_var_builder::VarBuilder::from_gguf(&model.path, &device)
        .context("loading GGUF model weights")?;
    let mut whisper =
        quantized_model::Whisper::load(&vb, config.clone()).context("building Whisper model")?;

    // Convert i16 LE bytes → normalised f32.
    let samples: Vec<f32> = raw_pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();

    let windows = samples.len().div_ceil(WHISPER_WINDOW_SAMPLES).max(1);
    // "auto" resolves once, on the first window, and sticks for the rest.
    let mut lang_tok: Option<u32> = (language != "auto").then(|| language_token(language));
    let mut parts: Vec<String> = Vec::new();

    for (i, chunk) in samples.chunks(WHISPER_WINDOW_SAMPLES).enumerate() {
        // Pad to exactly 30 s so the encoder sees a full receptive field.
        let mut window = chunk.to_vec();
        window.resize(WHISPER_WINDOW_SAMPLES, 0.0);

        let mel_flat = audio::pcm_to_mel(&config, &window, &mel_filters);
        let mel_frames = mel_flat.len() / N_MELS;
        let mel =
            Tensor::from_vec(mel_flat, (1, N_MELS, mel_frames), &device).context("mel tensor")?;
        let features = whisper.encoder.forward(&mel, true).context("encoder")?;

        let tok = match lang_tok {
            Some(t) => t,
            None => {
                let detected = detect_language(&mut whisper, &features, &device)?;
                let code = WHISPER_LANGUAGES
                    .get((detected - LANG_TOK_START) as usize)
                    .copied()
                    .unwrap_or("?");
                info!("local: auto-detected language {code:?} (token {detected})");
                lang_tok = Some(detected);
                detected
            }
        };

        let text = decode_window(&mut whisper, &features, tok, &config, &vocab, &device)?;
        if windows > 1 {
            info!("local: window {}/{windows} done", i + 1);
        }
        if !text.is_empty() {
            parts.push(text);
        }
    }

    Ok(parts.join(" "))
}

/// One decoder forward pass with `[SOT]`, argmax over the language-token range.
fn detect_language(
    whisper: &mut quantized_model::Whisper,
    features: &Tensor,
    device: &Device,
) -> Result<u32> {
    let sot_in = Tensor::from_slice(&[SOT], (1, 1), device).context("sot tensor")?;
    let logits = whisper
        .decoder
        .forward(&sot_in, features, true)
        .context("lang detect")?;
    let last = logits.get(0)?.get(0)?;
    let lang_range = (LANG_TOK_END - LANG_TOK_START + 1) as usize;
    let lang_logits = last
        .narrow(0, LANG_TOK_START as usize, lang_range)
        .context("lang slice")?;
    let detected = lang_logits
        .argmax(0)?
        .to_scalar::<u32>()
        .context("lang argmax")?
        + LANG_TOK_START;
    Ok(detected)
}

/// Greedy decode of one encoded 30 s window (full-sequence, no incremental KV
/// cache — simple and correct, at the cost of extra compute on long outputs).
fn decode_window(
    whisper: &mut quantized_model::Whisper,
    features: &Tensor,
    lang_tok: u32,
    config: &WhisperConfig,
    vocab: &[String],
    device: &Device,
) -> Result<String> {
    let mut tokens: Vec<u32> = vec![SOT, lang_tok, TRANSCRIBE, NO_TIMESTAMPS];
    let mut output: Vec<u32> = vec![];

    for _ in 0..config.max_target_positions {
        let t_in = Tensor::from_slice(tokens.as_slice(), (1, tokens.len()), device)
            .context("decoder input")?;
        let logits = whisper
            .decoder
            .forward(&t_in, features, true)
            .context("decoder")?;
        // logits: (1, seq_len, vocab_size) → pick last position.
        let last = logits.get(0)?.get(tokens.len() - 1)?;
        let next = last.argmax(0)?.to_scalar::<u32>().context("argmax")?;
        if next == EOT {
            break;
        }
        // Special tokens (timestamps etc.) are decoder state, not text.
        if next < FIRST_SPECIAL_TOKEN {
            output.push(next);
        }
        tokens.push(next);
        if tokens.len() >= config.max_target_positions {
            warn!("local: decoder hit max length without EOT");
            break;
        }
    }

    Ok(decode_tokens(&output, vocab).trim().to_string())
}

// ── Model config ──────────────────────────────────────────────────────────────

/// Whisper dimensions for each supported local model. Kept in code (rather
/// than downloading a config.json) so the engine and the model catalog can't
/// disagree about what a model id means.
fn whisper_config(kind: LocalModelKind) -> WhisperConfig {
    match kind {
        LocalModelKind::Tiny => serde_json::from_str(
            r#"{
            "num_mel_bins": 80, "max_source_positions": 1500, "d_model": 384,
            "encoder_attention_heads": 6, "encoder_layers": 4,
            "decoder_attention_heads": 6, "decoder_layers": 4,
            "vocab_size": 51865, "max_target_positions": 448
        }"#,
        )
        .expect("tiny config"),
    }
}

// ── Vocabulary ────────────────────────────────────────────────────────────────

/// Load the id → token table from the HuggingFace `tokenizer.json` that ships
/// alongside the GGUF weights. Only plain-text tokens (< 50257) matter here;
/// special/added tokens are filtered out during decoding anyway.
fn load_tokenizer_vocab(model: &LocalModel) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(&model.tokenizer)
        .with_context(|| format!("reading {}", model.tokenizer.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&contents).context("parsing tokenizer.json")?;
    let Some(map) = json
        .get("model")
        .and_then(|m| m.get("vocab"))
        .and_then(|v| v.as_object())
    else {
        bail!(
            "no model.vocab table in {} — re-download with `dit models download`",
            model.tokenizer.display()
        );
    };
    let mut vocab = vec![String::new(); FIRST_SPECIAL_TOKEN as usize];
    for (token, id) in map {
        if let Some(id) = id.as_u64() {
            if (id as usize) < vocab.len() {
                vocab[id as usize] = token.clone();
            }
        }
    }
    Ok(vocab)
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

/// Decode the embedded filterbank into the `Vec<f32>` candle expects.
fn load_mel_filters() -> Vec<f32> {
    MEL_FILTERS_BYTES
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tokens_cover_all_99_whisper_languages() {
        assert_eq!(WHISPER_LANGUAGES.len(), 99);
        assert_eq!(
            LANG_TOK_START + (WHISPER_LANGUAGES.len() as u32 - 1),
            LANG_TOK_END
        );
        // Anchor a few well-known assignments.
        assert_eq!(language_token("en"), 50259);
        assert_eq!(language_token("pt"), 50267);
        assert_eq!(language_token("su"), LANG_TOK_END);
        // Unknown codes fall back to pt (with a warning).
        assert_eq!(language_token("xx"), language_token("pt"));
    }

    #[test]
    fn embedded_mel_filterbank_has_whisper_dimensions() {
        let filters = load_mel_filters();
        // 80 mel bands × 201 FFT bins.
        assert_eq!(filters.len(), 80 * 201);
        // A real filterbank is sparse but not empty.
        assert!(filters.iter().any(|&f| f > 0.0));
        assert!(filters.iter().all(|&f| f.is_finite()));
    }

    #[test]
    fn byte_level_bpe_roundtrips_ascii_and_utf8() {
        let map = build_unicode_to_byte_map();
        assert_eq!(map.len(), 256);
        // 'a' maps to itself; the space marker 'Ġ' (U+0120) maps to 0x20.
        assert_eq!(map.get(&'a'), Some(&b'a'));
        assert_eq!(map.get(&'\u{120}'), Some(&0x20));
    }
}
