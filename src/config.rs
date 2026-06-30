//! Configuration: CLI flags, env-file loading and the derived runtime settings.
//!
//! Mirrors `whisperflow.py`'s config block: it reads `ELEVENLABS_API_KEY` from a
//! dotenv-style file (default `~/.dit.env`) or the process environment, and
//! exposes the model/language/hotkey knobs.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// A platform-neutral toggle key (F1..F12). Converted to the right per-OS
/// representation where it's used (global-hotkey on macOS/Windows, evdev on Linux).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionKey {
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

/// Cross-platform voice dictation via ElevenLabs Scribe v2 Realtime.
#[derive(Parser, Debug)]
#[command(name = "dit", version, about)]
pub struct Cli {
    /// Subcommand (omit to run dictation directly).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Language code passed to Scribe (e.g. `pt`, `en`, `es`). [default: pt]
    #[arg(long)]
    pub language: Option<String>,

    /// Scribe realtime model id. [default: scribe_v2_realtime]
    #[arg(long)]
    pub model: Option<String>,

    /// Toggle hotkey. Supports F1..F12 (e.g. `F9`). [default: F9]
    #[arg(long)]
    pub hotkey: Option<String>,

    /// Input device name substring to prefer (otherwise the system default).
    #[arg(long)]
    pub device: Option<String>,

    /// Remove filler words ("uh", "um", …) from the transcript (`no_verbatim`).
    #[arg(long)]
    pub no_filler: bool,

    /// Bias the model toward a term (name, jargon, product). Repeatable.
    #[arg(long = "keyterm", value_name = "TERM")]
    pub keyterms: Vec<String>,

    /// Seconds of silence before VAD commits a segment (lower = snappier, more fragmented). [default: 1.5]
    #[arg(long)]
    pub vad_silence: Option<f64>,

    /// API region: `global`, `us`, `eu`, `in` (data residency). [default: global]
    #[arg(long)]
    pub region: Option<String>,

    /// Disable the live terminal preview of partial transcripts.
    #[arg(long)]
    pub no_preview: bool,

    /// On Wayland, paste with Ctrl+Shift+V instead of Ctrl+V (for terminals).
    #[arg(long)]
    pub paste_shift: bool,

    /// Path to a dotenv-style file holding `ELEVENLABS_API_KEY`.
    /// Defaults to `~/.dit.env`.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// Path to the persistent config file. Defaults to `~/.dit/config.toml`.
    #[arg(long)]
    pub config: Option<PathBuf>,

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
    /// Diagnose keyboard, microphone, display/session, and API prerequisites.
    Doctor,
    /// Update dit to the latest release (idempotent: a no-op when current).
    Update {
        /// Only report whether a newer release exists; install nothing.
        #[arg(long)]
        check: bool,
        /// Reinstall even when the target version is already present.
        #[arg(long)]
        force: bool,
        /// Install a specific release tag (e.g. v0.2.4) instead of the latest.
        #[arg(long)]
        version: Option<String>,
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
    pub hotkey: FunctionKey,
    pub device: Option<String>,
    pub no_filler: bool,
    pub keyterms: Vec<String>,
    pub vad_silence: f64,
    pub region: String,
    pub no_preview: bool,
    pub paste_shift: bool,
}

/// Target sample rate sent to the API (Scribe expects 16 kHz mono s16le).
pub const SAMPLE_RATE: u32 = 16_000;
/// Roughly how much audio to batch per WebSocket frame (~100 ms).
pub const CHUNK_MS: u32 = 100;
/// How long to keep listening for final commits after the user toggles off.
pub const FINAL_WAIT_SECS: f64 = 3.0;

/// One layer of optional settings. Used for the persistent config file, the
/// process environment, and the explicit CLI flags. Every field is optional so
/// that "unset" can be distinguished from "set to the default value", which is
/// what makes layered resolution (defaults < file < env < flags) possible.
///
/// Deserialized from `~/.dit/config.toml`; unknown keys are ignored and missing
/// keys stay `None`, so partial files degrade gracefully.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PartialConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_filler: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyterms: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad_silence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_preview: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste_shift: Option<bool>,
}

impl PartialConfig {
    /// The explicit CLI flags as a layer. A flag is only `Some` when the user
    /// actually passed it, so unset flags fall through to lower layers.
    fn from_cli(cli: &Cli) -> Self {
        Self {
            language: cli.language.clone(),
            model: cli.model.clone(),
            hotkey: cli.hotkey.clone(),
            device: cli.device.clone(),
            // store_true flags can only turn a setting *on*; an absent flag is
            // left `None` so the file/env layers still decide.
            no_filler: if cli.no_filler { Some(true) } else { None },
            keyterms: if cli.keyterms.is_empty() {
                None
            } else {
                Some(cli.keyterms.clone())
            },
            vad_silence: cli.vad_silence,
            region: cli.region.clone(),
            no_preview: if cli.no_preview { Some(true) } else { None },
            paste_shift: if cli.paste_shift { Some(true) } else { None },
        }
    }

    /// The process environment as a layer (`DIT_*` variables). Unparseable
    /// values are dropped (left `None`) rather than aborting startup.
    fn from_env() -> Self {
        Self {
            language: env_string("DIT_LANGUAGE"),
            model: env_string("DIT_MODEL"),
            hotkey: env_string("DIT_HOTKEY"),
            device: env_string("DIT_DEVICE"),
            no_filler: env_bool("DIT_NO_FILLER"),
            keyterms: env_string("DIT_KEYTERMS").map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            }),
            vad_silence: env_string("DIT_VAD_SILENCE").and_then(|s| s.parse().ok()),
            region: env_string("DIT_REGION"),
            no_preview: env_bool("DIT_NO_PREVIEW"),
            paste_shift: env_bool("DIT_PASTE_SHIFT"),
        }
    }
}

/// Read a `DIT_*` string variable, treating blank values as unset.
fn env_string(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Read a `DIT_*` boolean variable (`1/true/yes/on` → true, `0/false/no/off`
/// → false; anything else is ignored).
fn env_bool(key: &str) -> Option<bool> {
    match std::env::var(key).ok()?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The default `~/.dit/config.toml` path.
fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".dit").join("config.toml"))
}

/// Load and parse the persistent config file. Missing files yield an empty
/// layer; malformed files are logged and also yield an empty layer, so startup
/// always falls back to env/CLI/defaults.
fn load_config_file(path: &Path) -> PartialConfig {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return PartialConfig::default();
    };
    match toml::from_str(&contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                "ignoring malformed config file {}: {err}",
                path.display()
            );
            PartialConfig::default()
        }
    }
}

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

        let config_path = cli.config.clone().or_else(default_config_path);
        let file = config_path
            .as_deref()
            .map(load_config_file)
            .unwrap_or_default();

        Self::from_layers(file, PartialConfig::from_env(), PartialConfig::from_cli(cli), api_key)
    }

    /// Merge the three layers over the built-in defaults. Precedence, lowest to
    /// highest: defaults < file < env < flags.
    ///
    /// Pure (no I/O) so layered resolution can be unit-tested directly.
    fn from_layers(
        file: PartialConfig,
        env: PartialConfig,
        cli: PartialConfig,
        api_key: String,
    ) -> Result<Self> {
        // For each field: CLI wins, then env, then file, then the default.
        macro_rules! pick {
            ($field:ident, $default:expr) => {
                cli.$field
                    .or(env.$field)
                    .or(file.$field)
                    .unwrap_or_else(|| $default)
            };
        }

        let hotkey_str = pick!(hotkey, "F9".to_string());
        let hotkey = parse_hotkey(&hotkey_str)
            .with_context(|| format!("unsupported hotkey: {hotkey_str}"))?;

        Ok(Self {
            api_key,
            language: pick!(language, "pt".to_string()),
            model: pick!(model, "scribe_v2_realtime".to_string()),
            hotkey,
            device: cli.device.or(env.device).or(file.device),
            no_filler: cli.no_filler.or(env.no_filler).or(file.no_filler).unwrap_or(false),
            keyterms: cli
                .keyterms
                .or(env.keyterms)
                .or(file.keyterms)
                .unwrap_or_default(),
            vad_silence: cli
                .vad_silence
                .or(env.vad_silence)
                .or(file.vad_silence)
                .unwrap_or(1.5),
            region: pick!(region, "global".to_string()),
            no_preview: cli
                .no_preview
                .or(env.no_preview)
                .or(file.no_preview)
                .unwrap_or(false),
            paste_shift: cli
                .paste_shift
                .or(env.paste_shift)
                .or(file.paste_shift)
                .unwrap_or(false),
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
    /// We always send 16 kHz mono PCM (`encoding=pcm_16000`) because the
    /// capture pipeline resamples to that rate, and use VAD-based commits so the
    /// server closes segments on natural pauses.
    pub fn ws_url(&self) -> String {
        let mut url = format!(
            "wss://{}/v1/speech-to-text/realtime\
             ?model_id={}&encoding=pcm_{}&sample_rate={}\
             &commit_strategy=vad&vad_silence_threshold_secs={}&language_code={}",
            self.host(),
            self.model,
            SAMPLE_RATE,
            SAMPLE_RATE,
            self.vad_silence,
            self.language,
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

/// Parse a function-key name into a [`FunctionKey`].
fn parse_hotkey(name: &str) -> Result<FunctionKey> {
    use FunctionKey::*;
    let key = match name.to_ascii_uppercase().as_str() {
        "F1" => F1,
        "F2" => F2,
        "F3" => F3,
        "F4" => F4,
        "F5" => F5,
        "F6" => F6,
        "F7" => F7,
        "F8" => F8,
        "F9" => F9,
        "F10" => F10,
        "F11" => F11,
        "F12" => F12,
        other => bail!("only F1..F12 are supported, got {other}"),
    };
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// defaults < file < env < flags, demonstrated on a single string field.
    #[test]
    fn layered_precedence() {
        let key = "k".to_string();

        // Nothing set anywhere → built-in defaults.
        let c = Config::from_layers(
            PartialConfig::default(),
            PartialConfig::default(),
            PartialConfig::default(),
            key.clone(),
        )
        .unwrap();
        assert_eq!(c.language, "pt");
        assert_eq!(c.model, "scribe_v2_realtime");
        assert_eq!(c.region, "global");
        assert_eq!(c.vad_silence, 1.5);
        assert_eq!(c.hotkey, FunctionKey::F9);
        assert!(!c.no_filler);

        let file = PartialConfig {
            language: Some("en".into()),
            ..Default::default()
        };
        // File overrides the default.
        let c = Config::from_layers(
            file.clone(),
            PartialConfig::default(),
            PartialConfig::default(),
            key.clone(),
        )
        .unwrap();
        assert_eq!(c.language, "en");

        let env = PartialConfig {
            language: Some("es".into()),
            ..Default::default()
        };
        // Env overrides the file.
        let c = Config::from_layers(
            file.clone(),
            env.clone(),
            PartialConfig::default(),
            key.clone(),
        )
        .unwrap();
        assert_eq!(c.language, "es");

        let cli = PartialConfig {
            language: Some("fr".into()),
            ..Default::default()
        };
        // A CLI flag overrides everything.
        let c = Config::from_layers(file, env, cli, key).unwrap();
        assert_eq!(c.language, "fr");
    }

    /// Booleans and keyterms layer the same way, including env-set-to-false
    /// overriding a file-set-to-true.
    #[test]
    fn bool_and_keyterm_layering() {
        let file = PartialConfig {
            no_filler: Some(true),
            keyterms: Some(vec!["RedDB".into()]),
            hotkey: Some("F8".into()),
            ..Default::default()
        };

        let c = Config::from_layers(
            file.clone(),
            PartialConfig::default(),
            PartialConfig::default(),
            "k".into(),
        )
        .unwrap();
        assert!(c.no_filler);
        assert_eq!(c.keyterms, vec!["RedDB".to_string()]);
        assert_eq!(c.hotkey, FunctionKey::F8);

        // Env explicitly disables a file-enabled flag.
        let env = PartialConfig {
            no_filler: Some(false),
            ..Default::default()
        };
        let c = Config::from_layers(file, env, PartialConfig::default(), "k".into()).unwrap();
        assert!(!c.no_filler);
    }

    /// A config file written out and read back round-trips field-for-field.
    #[test]
    fn file_round_trip() {
        let original = PartialConfig {
            language: Some("en".into()),
            hotkey: Some("F8".into()),
            vad_silence: Some(2.0),
            keyterms: Some(vec!["foo".into(), "bar".into()]),
            no_filler: Some(true),
            region: Some("eu".into()),
            ..Default::default()
        };

        let serialized = toml::to_string(&original).unwrap();
        let parsed: PartialConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);

        // And it resolves to the values written.
        let c = Config::from_layers(
            parsed,
            PartialConfig::default(),
            PartialConfig::default(),
            "k".into(),
        )
        .unwrap();
        assert_eq!(c.language, "en");
        assert_eq!(c.hotkey, FunctionKey::F8);
        assert_eq!(c.vad_silence, 2.0);
        assert_eq!(c.keyterms, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(c.region, "eu");
    }

    /// Missing files yield an empty layer; partial files keep their keys and
    /// leave the rest unset; malformed files degrade to an empty layer.
    #[test]
    fn config_file_degrades_gracefully() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();

        // Missing file.
        let missing = dir.join(format!("dit-missing-{pid}.toml"));
        let _ = std::fs::remove_file(&missing);
        assert_eq!(load_config_file(&missing), PartialConfig::default());

        // Partial file: only `language` set.
        let partial = dir.join(format!("dit-partial-{pid}.toml"));
        std::fs::write(&partial, "language = \"de\"\n").unwrap();
        let p = load_config_file(&partial);
        assert_eq!(p.language.as_deref(), Some("de"));
        assert_eq!(p.model, None);
        let _ = std::fs::remove_file(&partial);

        // Malformed file: not valid TOML → empty layer, no panic.
        let bad = dir.join(format!("dit-bad-{pid}.toml"));
        std::fs::write(&bad, "this is = not valid = toml ][\n").unwrap();
        assert_eq!(load_config_file(&bad), PartialConfig::default());
        let _ = std::fs::remove_file(&bad);
    }
}
