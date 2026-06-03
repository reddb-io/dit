//! Configuration: CLI flags, env-file loading and the derived runtime settings.
//!
//! Mirrors `whisperflow.py`'s config block: it reads `ELEVENLABS_API_KEY` from a
//! dotenv-style file (default `~/.dit.env`) or the process environment, and
//! exposes the model/language/hotkey knobs.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use global_hotkey::hotkey::Code;

/// Cross-platform voice dictation via ElevenLabs Scribe v2 Realtime.
#[derive(Parser, Debug)]
#[command(name = "dit", version, about)]
pub struct Cli {
    /// Subcommand (omit to run dictation directly).
    #[command(subcommand)]
    pub command: Option<Command>,

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

    /// Remove filler words ("uh", "um", …) from the transcript (`no_verbatim`).
    #[arg(long)]
    pub no_filler: bool,

    /// Bias the model toward a term (name, jargon, product). Repeatable.
    #[arg(long = "keyterm", value_name = "TERM")]
    pub keyterms: Vec<String>,

    /// Seconds of silence before VAD commits a segment (lower = snappier, more fragmented).
    #[arg(long, default_value_t = 1.5)]
    pub vad_silence: f64,

    /// API region: `global`, `us`, `eu`, `in` (data residency).
    #[arg(long, default_value = "global")]
    pub region: String,

    /// Disable the live terminal preview of partial transcripts.
    #[arg(long)]
    pub no_preview: bool,

    /// Path to a dotenv-style file holding `ELEVENLABS_API_KEY`.
    /// Defaults to `~/.dit.env`.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// List input audio devices and exit.
    #[arg(long)]
    pub list_devices: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage the autostart user service (runs dit in your login session).
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// Install and enable the autostart user service.
    Install {
        /// Flags to pass to dit when the service runs it (e.g. --language en).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stop and remove the autostart user service.
    Uninstall,
    /// Show whether the service is installed and running.
    Status,
}

/// Fully-resolved runtime settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    pub language: String,
    pub model: String,
    pub hotkey: Code,
    pub device: Option<String>,
    pub no_filler: bool,
    pub keyterms: Vec<String>,
    pub vad_silence: f64,
    pub region: String,
    pub no_preview: bool,
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
            .or_else(|| dirs::home_dir().map(|h| h.join(".dit.env")));
        if let Some(path) = &env_path {
            load_env_file(path);
        }

        let api_key = std::env::var("ELEVENLABS_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            bail!(
                "ELEVENLABS_API_KEY is not set. Put it in {} or export it in the environment.",
                env_path
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.dit.env".into())
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
            no_filler: cli.no_filler,
            keyterms: cli.keyterms.clone(),
            vad_silence: cli.vad_silence,
            region: cli.region.clone(),
            no_preview: cli.no_preview,
        })
    }

    /// Resolve the API host for the configured region (data residency).
    fn host(&self) -> &'static str {
        match self.region.as_str() {
            "us" => "api.us.elevenlabs.io",
            "eu" => "api.eu.residency.elevenlabs.io",
            "in" => "api.in.residency.elevenlabs.io",
            _ => "api.elevenlabs.io",
        }
    }

    /// Build the Scribe realtime WebSocket URL.
    ///
    /// We always send 16 kHz mono PCM (`audio_format=pcm_16000`) because the
    /// capture pipeline resamples to that rate, and use VAD-based commits so the
    /// server closes segments on natural pauses.
    pub fn ws_url(&self) -> String {
        let mut url = format!(
            "wss://{}/v1/speech-to-text/realtime\
             ?model_id={}&language_code={}&audio_format=pcm_{}\
             &commit_strategy=vad&vad_silence_threshold_secs={}",
            self.host(),
            self.model,
            self.language,
            SAMPLE_RATE,
            self.vad_silence,
        );
        if self.no_filler {
            url.push_str("&no_verbatim=true");
        }
        for term in &self.keyterms {
            url.push_str("&keyterms=");
            url.push_str(&percent_encode(term));
        }
        url
    }
}

/// Minimal percent-encoding for query values (keyterms may contain spaces/UTF-8).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

/// Parse function-key names into a `global_hotkey` key code.
fn parse_hotkey(name: &str) -> Result<Code> {
    let key = match name.to_ascii_uppercase().as_str() {
        "F1" => Code::F1,
        "F2" => Code::F2,
        "F3" => Code::F3,
        "F4" => Code::F4,
        "F5" => Code::F5,
        "F6" => Code::F6,
        "F7" => Code::F7,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        "F11" => Code::F11,
        "F12" => Code::F12,
        other => bail!("only F1..F12 are supported, got {other}"),
    };
    Ok(key)
}
