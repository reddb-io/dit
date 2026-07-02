//! `dit transcribe <file...>` — batch file transcription.
//!
//! Decodes common audio formats (wav/mp3/flac/m4a) via Symphonia, resamples
//! to 16 kHz mono using the existing [`Resampler`], and sends the PCM through
//! the [`Transcriber`] trait — cloud Scribe by default, or offline Whisper
//! with `--engine local` (no API key required). Output can be routed to
//! stdout, a `.txt` file, or the clipboard.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::audio::Resampler;
use crate::config::{
    self, Config, Engine, Hotkey, Key, LayoutSetting, RecordingMode, DEFAULT_MODEL, DEFAULT_REGION,
    DEFAULT_SESSION_MAX_AGE_DAYS, DEFAULT_SESSION_MAX_COUNT, DEFAULT_VAD_SILENCE, SAMPLE_RATE,
};
use crate::engine::{ScribeEngine, Transcriber};

pub enum OutputDest {
    Stdout,
    File(PathBuf),
    Clipboard,
}

pub struct TranscribeArgs {
    pub files: Vec<PathBuf>,
    pub output: OutputDest,
    /// Language code (`pt`, `en`, …) or `auto`.
    pub language: String,
    /// Path to the dotenv file holding `ELEVENLABS_API_KEY`.
    pub env_file: Option<PathBuf>,
    /// Engine name: `cloud` or `local`.
    pub engine: String,
    /// Model override (Scribe id for cloud, `dit models` id for local).
    pub model: Option<String>,
}

pub async fn run(args: TranscribeArgs) -> Result<()> {
    let engine = config::parse_engine(&args.engine)?;
    let cfg = build_config(&args, engine)?;
    let multi = args.files.len() > 1;
    let mut parts: Vec<String> = Vec::new();

    for path in &args.files {
        let (pcm_f32, native_rate) =
            decode_audio(path).with_context(|| format!("cannot decode {}", path.display()))?;

        let resampler = Resampler::new(native_rate, SAMPLE_RATE);
        let mut pcm_bytes: Vec<u8> = Vec::new();
        resampler.push(&pcm_f32, &mut pcm_bytes);
        let pcm_i16: Vec<i16> = pcm_bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();

        let transcript = transcribe_pcm(engine, &cfg, pcm_i16)
            .await
            .with_context(|| format!("transcription failed for {}", path.display()))?;

        let entry = if multi {
            format!("=== {} ===\n{}", path.display(), transcript)
        } else {
            transcript
        };
        parts.push(entry);
    }

    let result = parts.join("\n\n");

    match &args.output {
        OutputDest::Stdout => {
            println!("{result}");
        }
        OutputDest::File(path) => {
            let mut f = std::fs::File::create(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
            writeln!(f, "{result}")?;
            eprintln!("transcript written to {}", path.display());
        }
        OutputDest::Clipboard => {
            let mut ctx = arboard::Clipboard::new().context("cannot open clipboard")?;
            ctx.set_text(&result).context("cannot write to clipboard")?;
            eprintln!(
                "transcript copied to clipboard ({} chars)",
                result.chars().count()
            );
        }
    }

    Ok(())
}

/// Decode an audio file to mono f32 PCM at its native sample rate.
///
/// Supports wav, mp3, flac, and m4a via Symphonia. Returns the flat mono
/// samples and the file's native sample rate so callers can resample.
pub fn decode_audio(path: &Path) -> Result<(Vec<f32>, u32)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("unrecognized audio format")?;

    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .context("no audio track found")?;

    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .context("audio track has no sample rate")?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("unsupported codec")?;

    let mut samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => continue,
            Err(e) => return Err(e.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => {
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        let spec = *decoded.spec();
        let ch_count = spec.channels.count();
        let capacity = decoded.capacity() as u64;

        let mut sb = SampleBuffer::<f32>::new(capacity, spec);
        sb.copy_interleaved_ref(decoded);
        let raw = sb.samples();

        if ch_count <= 1 {
            samples.extend_from_slice(raw);
        } else {
            for frame in raw.chunks(ch_count) {
                samples.push(frame.iter().sum::<f32>() / ch_count as f32);
            }
        }
    }

    Ok((samples, sample_rate))
}

/// Route one decoded file through the selected engine.
async fn transcribe_pcm(engine: Engine, cfg: &Config, pcm: Vec<i16>) -> Result<String> {
    match engine {
        Engine::ElevenLabs => ScribeEngine.transcribe_batch(cfg, pcm).await,
        #[cfg(feature = "local")]
        Engine::Local => {
            let model = crate::models::resolve_local_model(&cfg.model)?;
            crate::engine::LocalEngine::new(model)
                .transcribe_batch(cfg, pcm)
                .await
        }
        #[cfg(not(feature = "local"))]
        Engine::Local => anyhow::bail!(
            "dit was built without --features local; rebuild with `cargo build --features local`"
        ),
    }
}

fn build_config(args: &TranscribeArgs, engine: Engine) -> Result<Config> {
    // Load API key from the env file (if present) then from the process env.
    let env_path = args
        .env_file
        .clone()
        .or_else(|| dirs::home_dir().map(|h| h.join(".dit.env")));
    if let Some(ref path) = env_path {
        config::load_env_file(path);
    }

    // The local engine is fully offline — only the ElevenLabs path needs a key.
    let api_key = std::env::var("ELEVENLABS_API_KEY").unwrap_or_default();
    if api_key.is_empty() && engine == Engine::ElevenLabs {
        anyhow::bail!(
            "ELEVENLABS_API_KEY is not set. Put it in {} or export it in the environment.",
            env_path
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.dit.env".into())
        );
    }

    let file = config::config_path()
        .map(|p| config::load_file_config(&p))
        .unwrap_or_default();

    let model = match (&args.model, engine) {
        (Some(m), _) => m.clone(),
        (None, Engine::ElevenLabs) => file.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        (None, Engine::Local) => config::DEFAULT_LOCAL_MODEL.into(),
    };

    Ok(Config {
        api_key,
        language: args.language.clone(),
        model,
        // Hotkey is unused for transcription; use a harmless default.
        hotkey: Hotkey {
            modifiers: vec![],
            key: Key::F9,
        },
        mode: RecordingMode::Toggle,
        device: None,
        no_filler: file.no_filler.unwrap_or(false),
        keyterms: file.keyterms.unwrap_or_default(),
        vad_silence: file.vad_silence.unwrap_or(DEFAULT_VAD_SILENCE),
        region: file.region.unwrap_or_else(|| DEFAULT_REGION.into()),
        no_preview: true,
        paste_shift: false,
        type_hybrid: false,
        session_max_age_days: file
            .session_max_age_days
            .unwrap_or(DEFAULT_SESSION_MAX_AGE_DAYS),
        session_max_count: file.session_max_count.unwrap_or(DEFAULT_SESSION_MAX_COUNT),
        engine,
        layout: LayoutSetting::Auto,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SAMPLE_RATE;

    /// Build a minimal WAV file (PCM s16le, mono) from raw i16 samples.
    fn make_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        // RIFF header
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        // fmt chunk
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
                                                     // data chunk
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn decode_and_resample_wav_fixture() {
        let native_rate = 44_100u32;
        let duration_frames = native_rate / 10; // 0.1 s
        let value: i16 = 1_000;
        let raw: Vec<i16> = vec![value; duration_frames as usize];
        let wav = make_wav(native_rate, &raw);

        // Write to a temp file and decode
        let tmp =
            std::env::temp_dir().join(format!("dit-transcribe-test-{}.wav", std::process::id()));
        std::fs::write(&tmp, &wav).unwrap();
        let (pcm_f32, rate) = decode_audio(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        assert_eq!(rate, native_rate);
        assert_eq!(pcm_f32.len(), duration_frames as usize);

        // Each sample should round-trip to ≈ value / i16::MAX
        let expected_f32 = value as f32 / i16::MAX as f32;
        for s in &pcm_f32 {
            assert!(
                (*s - expected_f32).abs() < 1e-4,
                "sample {s} not close to {expected_f32}"
            );
        }

        // Resample to 16 kHz and verify output length
        let resampler = Resampler::new(rate, SAMPLE_RATE);
        let mut pcm_bytes: Vec<u8> = Vec::new();
        resampler.push(&pcm_f32, &mut pcm_bytes);

        let expected_frames =
            (duration_frames as f64 * SAMPLE_RATE as f64 / native_rate as f64) as usize;
        let actual_frames = pcm_bytes.len() / 2;
        assert!(
            (actual_frames as i64 - expected_frames as i64).abs() <= 2,
            "resampled frames {actual_frames} not close to expected {expected_frames}"
        );
    }
}
