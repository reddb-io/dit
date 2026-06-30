//! Configuration: CLI flags, env-file loading and the derived runtime settings.
//!
//! Mirrors `whisperflow.py`'s config block: it reads `ELEVENLABS_API_KEY` from a
//! dotenv-style file (default `~/.dit.env`) or the process environment, and
//! exposes the model/language/hotkey knobs.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::parser::ValueSource;
use clap::{ArgMatches, Parser, Subcommand};
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

    /// On Wayland, paste with Ctrl+Shift+V instead of Ctrl+V (for terminals).
    #[arg(long)]
    pub paste_shift: bool,

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
    /// Diagnose keyboard, microphone, display/session, and API prerequisites.
    Doctor,
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

// Defaults for the layered settings. These are the same values clap advertises
// in `--help`; they are the bottom layer (defaults < config.toml < env < flags).
const DEFAULT_LANGUAGE: &str = "pt";
const DEFAULT_MODEL: &str = "scribe_v2_realtime";
const DEFAULT_HOTKEY: &str = "F9";
const DEFAULT_REGION: &str = "global";
const DEFAULT_VAD_SILENCE: f64 = 1.5;

/// Target sample rate sent to the API (Scribe expects 16 kHz mono s16le).
pub const SAMPLE_RATE: u32 = 16_000;
/// Roughly how much audio to batch per WebSocket frame (~100 ms).
pub const CHUNK_MS: u32 = 100;
/// How long to keep listening for final commits after the user toggles off.
pub const FINAL_WAIT_SECS: f64 = 3.0;

/// One layer of overridable settings. Every field is optional so a layer can
/// speak only to the knobs it cares about; layers are stacked lowest-to-highest
/// with [`Settings::merge_under`]. Serialised form is `~/.dit/config.toml`.
///
/// Secrets (the API key) and purely operational flags (`--env-file`,
/// `--list-devices`) are deliberately *not* here — they are not part of the
/// persisted, layerable configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Option<String>,
    pub model: Option<String>,
    pub hotkey: Option<String>,
    pub device: Option<String>,
    pub no_filler: Option<bool>,
    pub keyterms: Option<Vec<String>>,
    pub vad_silence: Option<f64>,
    pub region: Option<String>,
    pub no_preview: Option<bool>,
    pub paste_shift: Option<bool>,
}

impl Settings {
    /// Overlay `higher` on top of `self`: any value set in `higher` wins, while
    /// `self`'s values show through wherever `higher` is silent. Used to stack
    /// the layers `defaults < file < env < flags`.
    fn merge_under(self, higher: Settings) -> Settings {
        Settings {
            language: higher.language.or(self.language),
            model: higher.model.or(self.model),
            hotkey: higher.hotkey.or(self.hotkey),
            device: higher.device.or(self.device),
            no_filler: higher.no_filler.or(self.no_filler),
            keyterms: higher.keyterms.or(self.keyterms),
            vad_silence: higher.vad_silence.or(self.vad_silence),
            region: higher.region.or(self.region),
            no_preview: higher.no_preview.or(self.no_preview),
            paste_shift: higher.paste_shift.or(self.paste_shift),
        }
    }

    /// Settings that were *explicitly* given on the command line. A flag left at
    /// its clap default contributes nothing, so the lower layers (env, file)
    /// remain visible; an explicitly-passed flag always wins.
    fn from_cli(cli: &Cli, m: &ArgMatches) -> Settings {
        let on_cli = |id: &str| m.value_source(id) == Some(ValueSource::CommandLine);
        Settings {
            language: on_cli("language").then(|| cli.language.clone()),
            model: on_cli("model").then(|| cli.model.clone()),
            hotkey: on_cli("hotkey").then(|| cli.hotkey.clone()),
            device: cli.device.clone(),
            no_filler: on_cli("no_filler").then_some(true),
            keyterms: (!cli.keyterms.is_empty()).then(|| cli.keyterms.clone()),
            vad_silence: on_cli("vad_silence").then_some(cli.vad_silence),
            region: on_cli("region").then(|| cli.region.clone()),
            no_preview: on_cli("no_preview").then_some(true),
            paste_shift: on_cli("paste_shift").then_some(true),
        }
    }

    /// Settings sourced from the environment (`DIT_*`). Unparseable values are
    /// dropped (treated as unset) rather than failing the whole run.
    fn from_env() -> Settings {
        let s = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Settings {
            language: s("DIT_LANGUAGE"),
            model: s("DIT_MODEL"),
            hotkey: s("DIT_HOTKEY"),
            device: s("DIT_DEVICE"),
            no_filler: s("DIT_NO_FILLER").as_deref().and_then(parse_bool),
            keyterms: s("DIT_KEYTERMS").map(|v| {
                v.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            }),
            vad_silence: s("DIT_VAD_SILENCE").and_then(|v| v.parse().ok()),
            region: s("DIT_REGION"),
            no_preview: s("DIT_NO_PREVIEW").as_deref().and_then(parse_bool),
            paste_shift: s("DIT_PASTE_SHIFT").as_deref().and_then(parse_bool),
        }
    }
}

/// Default location of the persistent config store.
fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".dit").join("config.toml"))
}

/// Read and parse `~/.dit/config.toml`. A missing file yields empty settings; a
/// malformed file logs a warning and likewise degrades to empty settings so the
/// run continues on defaults rather than crashing.
fn load_file_config(path: Option<&Path>) -> Settings {
    let Some(path) = path else {
        return Settings::default();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    parse_file_config(&contents).unwrap_or_else(|e| {
        tracing::warn!("ignoring malformed {}: {e}", path.display());
        Settings::default()
    })
}

/// Parse the TOML body of a config file into [`Settings`]. Partial files are
/// fine — every field is optional.
fn parse_file_config(contents: &str) -> Result<Settings, toml::de::Error> {
    toml::from_str(contents)
}

/// Lenient boolean parser for env values: `1/true/yes/on` → true.
fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    pub fn resolve(cli: &Cli, matches: &ArgMatches) -> Result<Self> {
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

        // Stack the layers: defaults (applied below via `unwrap_or`) < file < env
        // < flags. Each higher layer only overrides the knobs it actually sets.
        let file = load_file_config(config_path().as_deref());
        let s = file
            .merge_under(Settings::from_env())
            .merge_under(Settings::from_cli(cli, matches));

        let hotkey_str = s.hotkey.unwrap_or_else(|| DEFAULT_HOTKEY.to_string());
        let hotkey = parse_hotkey(&hotkey_str)
            .with_context(|| format!("unsupported hotkey: {hotkey_str}"))?;

        Ok(Self {
            api_key,
            language: s.language.unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
            model: s.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            hotkey,
            device: s.device,
            no_filler: s.no_filler.unwrap_or(false),
            keyterms: s.keyterms.unwrap_or_default(),
            vad_silence: s.vad_silence.unwrap_or(DEFAULT_VAD_SILENCE),
            region: s.region.unwrap_or_else(|| DEFAULT_REGION.to_string()),
            no_preview: s.no_preview.unwrap_or(false),
            paste_shift: s.paste_shift.unwrap_or(false),
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

    fn layer_language(v: &str) -> Settings {
        Settings {
            language: Some(v.to_string()),
            ..Settings::default()
        }
    }

    #[test]
    fn higher_layer_wins_lower_layer_shows_through() {
        let file = Settings {
            language: Some("es".into()),
            model: Some("file-model".into()),
            ..Settings::default()
        };
        let env = Settings {
            model: Some("env-model".into()),
            region: Some("eu".into()),
            ..Settings::default()
        };
        let cli = Settings {
            region: Some("us".into()),
            ..Settings::default()
        };

        let merged = file.merge_under(env).merge_under(cli);

        // Only the file set it → file value survives.
        assert_eq!(merged.language.as_deref(), Some("es"));
        // env overrides the file.
        assert_eq!(merged.model.as_deref(), Some("env-model"));
        // cli overrides env.
        assert_eq!(merged.region.as_deref(), Some("us"));
        // Nobody set it → stays unset (defaults applied later).
        assert_eq!(merged.hotkey, None);
    }

    #[test]
    fn cli_always_beats_env_which_beats_file() {
        let resolved = layer_language("file")
            .merge_under(layer_language("env"))
            .merge_under(layer_language("cli"));
        assert_eq!(resolved.language.as_deref(), Some("cli"));

        let resolved = layer_language("file").merge_under(layer_language("env"));
        assert_eq!(resolved.language.as_deref(), Some("env"));
    }

    #[test]
    fn unset_layers_fall_back_to_compiled_defaults() {
        // An empty stack must leave every knob unset so `resolve` applies the
        // documented defaults.
        let merged = Settings::default()
            .merge_under(Settings::default())
            .merge_under(Settings::default());
        assert_eq!(merged, Settings::default());
        assert_eq!(
            merged
                .language
                .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
            "pt"
        );
        assert_eq!(merged.vad_silence.unwrap_or(DEFAULT_VAD_SILENCE), 1.5);
    }

    #[test]
    fn settings_survive_a_toml_round_trip() {
        let original = Settings {
            language: Some("en".into()),
            model: Some("scribe_v2_realtime".into()),
            hotkey: Some("F8".into()),
            device: Some("USB Mic".into()),
            no_filler: Some(true),
            keyterms: Some(vec!["RedDB".into(), "Scribe".into()]),
            vad_silence: Some(2.5),
            region: Some("eu".into()),
            no_preview: Some(true),
            paste_shift: Some(false),
        };

        let toml_text = toml::to_string(&original).expect("serialise");
        let parsed = parse_file_config(&toml_text).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn partial_config_only_sets_named_fields() {
        let parsed = parse_file_config("language = \"fr\"\nno_filler = true\n").expect("parse");
        assert_eq!(parsed.language.as_deref(), Some("fr"));
        assert_eq!(parsed.no_filler, Some(true));
        // Untouched knobs stay unset.
        assert_eq!(parsed.model, None);
        assert_eq!(parsed.region, None);
    }

    #[test]
    fn malformed_config_degrades_to_defaults() {
        // Not valid TOML → parse error, surfaced as Err so `load_file_config`
        // can swallow it and return empty settings.
        assert!(parse_file_config("this is not = = toml [[[").is_err());
        // Missing file path → empty settings, no panic.
        assert_eq!(
            load_file_config(Some(Path::new("/nonexistent/dit/config.toml"))),
            Settings::default()
        );
        assert_eq!(load_file_config(None), Settings::default());
    }

    #[test]
    fn env_layer_parses_values_and_drops_junk() {
        let env = Settings {
            no_filler: parse_bool("yes"),
            keyterms: Some(
                "RedDB, Scribe ,"
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
            ),
            ..Settings::default()
        };
        assert_eq!(env.no_filler, Some(true));
        assert_eq!(
            env.keyterms,
            Some(vec!["RedDB".to_string(), "Scribe".to_string()])
        );
        // Unrecognised boolean strings are treated as unset.
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool("off"), Some(false));
    }
}
