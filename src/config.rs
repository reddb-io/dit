//! Configuration: CLI flags, env-file loading and the derived runtime settings.
//!
//! Mirrors `whisperflow.py`'s config block: it reads `ELEVENLABS_API_KEY` from a
//! dotenv-style file (default `~/.dictator.env`) or the process environment, and
//! exposes the model/language/hotkey knobs.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rdev::Key;

/// Cross-platform voice dictation via ElevenLabs Scribe v2 Realtime.
#[derive(Parser, Debug)]
#[command(name = "dictator", version, about)]
pub struct Cli {
    /// Language code passed to Scribe (e.g. `pt`, `en`, `es`).
    #[arg(long, default_value = "pt")]
    pub language: String,

    /// Scribe realtime model id.
    #[arg(long, default_value = "scribe_v2_realtime")]
    pub model: String,

    /// Toggle hotkey. Supports F1..F12 (e.g. `F9`).
    #[arg(long, default_value = "F9")]
    pub hotkey: String,

    /// Input device name substring to prefer (otherwise the system default).
    #[arg(long)]
    pub device: Option<String>,

    /// Path to a dotenv-style file holding `ELEVENLABS_API_KEY`.
    /// Defaults to `~/.dictator.env`.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// List input audio devices and exit.
    #[arg(long)]
    pub list_devices: bool,
}

/// Fully-resolved runtime settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    pub language: String,
    pub model: String,
    pub hotkey: Key,
    pub device: Option<String>,
}

/// Target sample rate sent to the API (Scribe expects 16 kHz mono s16le).
pub const SAMPLE_RATE: u32 = 16_000;
/// Roughly how much audio to batch per WebSocket frame (~100 ms).
pub const CHUNK_MS: u32 = 100;
/// How long to keep listening for final commits after the user toggles off.
pub const FINAL_WAIT_SECS: f64 = 3.0;

impl Config {
    pub fn resolve(cli: &Cli) -> Result<Self> {
        let env_path = cli
            .env_file
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".dictator.env")));
        if let Some(path) = &env_path {
            load_env_file(path);
        }

        let api_key = std::env::var("ELEVENLABS_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            bail!(
                "ELEVENLABS_API_KEY is not set. Put it in {} or export it in the environment.",
                env_path
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.dictator.env".into())
            );
        }

        let hotkey = parse_hotkey(&cli.hotkey)
            .with_context(|| format!("unsupported hotkey: {}", cli.hotkey))?;

        Ok(Self {
            api_key,
            language: cli.language.clone(),
            model: cli.model.clone(),
            hotkey,
            device: cli.device.clone(),
        })
    }

    /// Build the Scribe realtime WebSocket URL (VAD commit strategy).
    pub fn ws_url(&self) -> String {
        format!(
            "wss://api.elevenlabs.io/v1/speech-to-text/realtime?model_id={}&language_code={}&commit_strategy=vad",
            self.model, self.language
        )
    }
}

/// Minimal dotenv loader: `KEY=VALUE` lines, `#` comments, no overrides of
/// values already present in the environment.
fn load_env_file(path: &PathBuf) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
}

/// Parse function-key names into `rdev::Key`.
fn parse_hotkey(name: &str) -> Result<Key> {
    let key = match name.to_ascii_uppercase().as_str() {
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        other => bail!("only F1..F12 are supported, got {other}"),
    };
    Ok(key)
}
