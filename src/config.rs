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

/// A platform-neutral trigger key. This is the key the user actually presses to
/// toggle dictation. It can be a function key (F1..F12), a letter, Space, or a
/// single modifier key used on its own (e.g. Right Alt). `Fn` is recognised so we
/// can emit a clear "not capturable" error rather than silently ignoring it.
///
/// Converted to the right per-OS representation where it's used (global-hotkey on
/// macOS/Windows, evdev on Linux).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
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
    /// A letter key. Always stored uppercase (`'A'`..=`'Z'`).
    Letter(char),
    Space,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftShift,
    RightShift,
    LeftMeta,
    RightMeta,
    /// The laptop `Fn` key — recognised, but not capturable through the global
    /// hotkey/evdev APIs we use, so platform mapping rejects it with a clear error.
    Fn,
}

/// A modifier that can prefix a combo (e.g. the `Ctrl` and `Shift` in
/// `Ctrl+Shift+F9`). Left/right are unified — a combo modifier matches either side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Meta,
}

/// A fully-parsed hotkey: zero or more held modifiers plus the trigger key that
/// fires the toggle. A bare modifier key (Right Alt on its own) is represented as
/// an empty `modifiers` list with `key` set to that modifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: Vec<Modifier>,
    pub key: Key,
}

/// Cross-platform voice dictation via ElevenLabs Scribe v2 Realtime.
#[derive(Parser, Debug)]
#[command(name = "dit", version, about)]
pub struct Cli {
    /// Subcommand (omit to run dictation directly).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Language code passed to Scribe (e.g. `pt`, `en`, `es`), or `auto` to
    /// let the engine detect the spoken language automatically.
    #[arg(long, default_value = "pt")]
    pub language: String,

    /// Scribe realtime model id.
    #[arg(long, default_value = "scribe_v2_realtime")]
    pub model: String,

    /// Toggle hotkey. Accepts a function key (`F9`), a single modifier key
    /// (`RightAlt`), a letter (`D`), or a combo (`Ctrl+Shift+F9`).
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
    /// Manage local speech-to-text models stored in ~/.dit/models/.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Open the settings GUI (requires a build with the `gui` cargo feature).
    Settings,
}

#[derive(Subcommand, Debug)]
pub enum ModelsAction {
    /// List known models and whether each is installed.
    List,
    /// Download a model from HuggingFace and verify its SHA-256 checksum.
    Download {
        /// Model ID (e.g. `whisper-tiny`). Run `dit models list` to see all IDs.
        id: String,
    },
    /// Print the directory where models are stored (~/.dit/models/).
    Path,
    /// Delete a downloaded model to reclaim disk space.
    Rm {
        /// Model ID to remove.
        id: String,
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

// ── Layered configuration ────────────────────────────────────────────────────
//
// Settings resolve in the order *defaults < config.toml < environment < CLI
// flags*. A value written to `~/.dit/config.toml` is honoured on the next run,
// while any CLI flag still overrides it. The merge logic lives in pure functions
// ([`merge`], [`env_layer`]) so it can be unit-tested without touching the real
// filesystem or process environment.

pub(crate) const DEFAULT_LANGUAGE: &str = "pt";
pub(crate) const DEFAULT_MODEL: &str = "scribe_v2_realtime";
pub(crate) const DEFAULT_HOTKEY: &str = "F9";
pub(crate) const DEFAULT_REGION: &str = "global";
const DEFAULT_VAD_SILENCE: f64 = 1.5;
pub const DEFAULT_SESSION_MAX_AGE_DAYS: u64 = 30;
pub const DEFAULT_SESSION_MAX_COUNT: usize = 100;

/// One layer of overrides. Every field is optional: a `None` means "this layer
/// does not touch the setting". The same shape is used for the on-disk
/// `config.toml` (so missing/partial files degrade gracefully) and for the
/// derived environment and CLI layers.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SettingsLayer {
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
    pub session_max_age_days: Option<u64>,
    pub session_max_count: Option<usize>,
}

/// Settings after merging every layer over the built-in defaults. Distinct from
/// [`Config`] because it holds no secrets and no parsed hotkey — just the merged
/// scalar knobs, which keeps [`merge`] pure and easy to assert on.
#[derive(Clone, Debug, PartialEq)]
struct ResolvedSettings {
    language: String,
    model: String,
    hotkey: String,
    device: Option<String>,
    no_filler: bool,
    keyterms: Vec<String>,
    vad_silence: f64,
    region: String,
    no_preview: bool,
    paste_shift: bool,
    session_max_age_days: u64,
    session_max_count: usize,
}

/// Merge the three override layers over the defaults. Later arguments win:
/// `cli` over `env` over `file` over the built-in default.
fn merge(file: SettingsLayer, env: SettingsLayer, cli: SettingsLayer) -> ResolvedSettings {
    fn pick<T>(default: T, file: Option<T>, env: Option<T>, cli: Option<T>) -> T {
        cli.or(env).or(file).unwrap_or(default)
    }
    ResolvedSettings {
        language: pick(
            DEFAULT_LANGUAGE.into(),
            file.language,
            env.language,
            cli.language,
        ),
        model: pick(DEFAULT_MODEL.into(), file.model, env.model, cli.model),
        hotkey: pick(DEFAULT_HOTKEY.into(), file.hotkey, env.hotkey, cli.hotkey),
        // `device` is itself optional, so its resolved default is simply "unset".
        device: cli.device.or(env.device).or(file.device),
        no_filler: pick(false, file.no_filler, env.no_filler, cli.no_filler),
        keyterms: pick(Vec::new(), file.keyterms, env.keyterms, cli.keyterms),
        vad_silence: pick(
            DEFAULT_VAD_SILENCE,
            file.vad_silence,
            env.vad_silence,
            cli.vad_silence,
        ),
        region: pick(DEFAULT_REGION.into(), file.region, env.region, cli.region),
        no_preview: pick(false, file.no_preview, env.no_preview, cli.no_preview),
        paste_shift: pick(false, file.paste_shift, env.paste_shift, cli.paste_shift),
        session_max_age_days: pick(
            DEFAULT_SESSION_MAX_AGE_DAYS,
            file.session_max_age_days,
            env.session_max_age_days,
            cli.session_max_age_days,
        ),
        session_max_count: pick(
            DEFAULT_SESSION_MAX_COUNT,
            file.session_max_count,
            env.session_max_count,
            cli.session_max_count,
        ),
    }
}

/// Path to the persistent config store, `~/.dit/config.toml`.
pub(crate) fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".dit").join("config.toml"))
}

/// Read and parse `config.toml`. A missing, unreadable or malformed file
/// degrades gracefully to an empty layer (the defaults are used) rather than
/// crashing.
fn load_file_config(path: &Path) -> SettingsLayer {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return SettingsLayer::default();
    };
    match toml::from_str(&contents) {
        Ok(layer) => layer,
        Err(e) => {
            tracing::warn!("ignoring malformed config {}: {e}", path.display());
            SettingsLayer::default()
        }
    }
}

/// Derive the environment layer from `DIT_*` variables. Pure: the variable
/// lookup is injected so it can be tested without touching the process env.
fn env_layer(get: impl Fn(&str) -> Option<String>) -> SettingsLayer {
    let s = |k: &str| get(k).filter(|v| !v.is_empty());
    let flag = |k: &str| {
        s(k).map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
    };
    SettingsLayer {
        language: s("DIT_LANGUAGE"),
        model: s("DIT_MODEL"),
        hotkey: s("DIT_HOTKEY"),
        device: s("DIT_DEVICE"),
        no_filler: flag("DIT_NO_FILLER"),
        keyterms: s("DIT_KEYTERMS").map(|v| {
            v.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        }),
        vad_silence: s("DIT_VAD_SILENCE").and_then(|v| v.parse().ok()),
        region: s("DIT_REGION"),
        no_preview: flag("DIT_NO_PREVIEW"),
        paste_shift: flag("DIT_PASTE_SHIFT"),
        session_max_age_days: s("DIT_SESSION_MAX_AGE_DAYS").and_then(|v| v.parse().ok()),
        session_max_count: s("DIT_SESSION_MAX_COUNT").and_then(|v| v.parse().ok()),
    }
}

/// Derive the CLI layer: only flags the user actually passed contribute, so
/// clap's own defaults never shadow the file/env layers. Detection uses clap's
/// value source rather than comparing against defaults.
fn cli_layer(cli: &Cli, matches: &ArgMatches) -> SettingsLayer {
    let on_cli = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
    SettingsLayer {
        language: on_cli("language").then(|| cli.language.clone()),
        model: on_cli("model").then(|| cli.model.clone()),
        hotkey: on_cli("hotkey").then(|| cli.hotkey.clone()),
        device: on_cli("device").then(|| cli.device.clone()).flatten(),
        no_filler: on_cli("no_filler").then_some(cli.no_filler),
        keyterms: on_cli("keyterms").then(|| cli.keyterms.clone()),
        vad_silence: on_cli("vad_silence").then_some(cli.vad_silence),
        region: on_cli("region").then(|| cli.region.clone()),
        no_preview: on_cli("no_preview").then_some(cli.no_preview),
        paste_shift: on_cli("paste_shift").then_some(cli.paste_shift),
        session_max_age_days: None,
        session_max_count: None,
    }
}

/// Fully-resolved runtime settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    pub language: String,
    pub model: String,
    pub hotkey: Hotkey,
    pub device: Option<String>,
    pub no_filler: bool,
    pub keyterms: Vec<String>,
    pub vad_silence: f64,
    pub region: String,
    pub no_preview: bool,
    pub paste_shift: bool,
    pub session_max_age_days: u64,
    pub session_max_count: usize,
}

/// Target sample rate sent to the API (Scribe expects 16 kHz mono s16le).
pub const SAMPLE_RATE: u32 = 16_000;
/// Roughly how much audio to batch per WebSocket frame (~100 ms).
pub const CHUNK_MS: u32 = 100;
/// How long to keep listening for final commits after the user toggles off.
pub const FINAL_WAIT_SECS: f64 = 3.0;

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

        // defaults < config.toml < environment < CLI flags
        let file = config_path()
            .map(|p| load_file_config(&p))
            .unwrap_or_default();
        let env = env_layer(|k| std::env::var(k).ok());
        let cli_overrides = cli_layer(cli, matches);
        let settings = merge(file, env, cli_overrides);

        let hotkey = parse_hotkey(&settings.hotkey)
            .with_context(|| format!("unsupported hotkey: {}", settings.hotkey))?;

        Ok(Self {
            api_key,
            language: settings.language,
            model: settings.model,
            hotkey,
            device: settings.device,
            no_filler: settings.no_filler,
            keyterms: settings.keyterms,
            vad_silence: settings.vad_silence,
            region: settings.region,
            no_preview: settings.no_preview,
            paste_shift: settings.paste_shift,
            session_max_age_days: settings.session_max_age_days,
            session_max_count: settings.session_max_count,
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
             &commit_strategy=vad&vad_silence_threshold_secs={}",
            self.host(),
            self.model,
            SAMPLE_RATE,
            SAMPLE_RATE,
            self.vad_silence,
        );
        // "auto" means omit the parameter so Scribe detects the language itself.
        if self.language != "auto" {
            url.push_str("&language_code=");
            url.push_str(&self.language);
        }
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

/// A single runtime setting change requested from the tray control surface.
///
/// Each variant names one switchable knob. Applying it mutates the live
/// [`Config`] (so the *next* session picks it up without restarting the
/// process) and writes the same value back to `~/.dit/config.toml` so the
/// choice survives a restart. The two halves are kept separate — [`apply_to`]
/// for the in-memory swap, [`persist`] for the on-disk store — so each can be
/// unit-tested in isolation.
///
/// [`apply_to`]: Reconfigure::apply_to
/// [`persist`]: Reconfigure::persist
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfigure {
    /// Input device substring to prefer; `None` restores the system default.
    Device(Option<String>),
    /// Scribe language code (e.g. `pt`, `en`, `auto`).
    Language(String),
    /// Dictation mode: `true` strips filler words ("uh", "um"), `false` keeps
    /// them verbatim. Mirrors the `--no-filler` flag.
    NoFiller(bool),
    /// Speech-to-text engine / model id (e.g. `scribe_v2_realtime`).
    Model(String),
}

impl Reconfigure {
    /// Apply this change to the live config so the next session uses it.
    pub fn apply_to(&self, cfg: &mut Config) {
        match self {
            Reconfigure::Device(d) => cfg.device = d.clone(),
            Reconfigure::Language(l) => cfg.language = l.clone(),
            Reconfigure::NoFiller(b) => cfg.no_filler = *b,
            Reconfigure::Model(m) => cfg.model = m.clone(),
        }
    }

    /// Overlay this change onto an existing settings layer (used for both the
    /// on-disk store and unit tests). Only the touched key is changed; every
    /// other key in `layer` is left intact so unrelated settings survive.
    fn overlay(&self, layer: &mut SettingsLayer) {
        match self {
            Reconfigure::Device(d) => layer.device = d.clone(),
            Reconfigure::Language(l) => layer.language = Some(l.clone()),
            Reconfigure::NoFiller(b) => layer.no_filler = Some(*b),
            Reconfigure::Model(m) => layer.model = Some(m.clone()),
        }
    }

    /// Persist this change to `~/.dit/config.toml`, preserving every other key
    /// already present in the file. A missing file is created; a missing parent
    /// directory is created too.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        self.persist_to(&path)
    }

    /// Persistence against an explicit path (so tests need not touch `$HOME`).
    fn persist_to(&self, path: &Path) -> Result<()> {
        let mut layer = if path.exists() {
            load_file_config(path)
        } else {
            SettingsLayer::default()
        };
        self.overlay(&mut layer);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let toml = toml::to_string_pretty(&layer).context("serializing config.toml")?;
        std::fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
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

/// Parse a hotkey spec into a [`Hotkey`].
///
/// Accepts a single key (`F9`, `RightAlt`, `D`, `Space`) or a `+`-separated combo
/// (`Ctrl+Shift+F9`, `Alt+Space`). Every segment but the last must be a modifier;
/// the last segment is the trigger key (which may itself be a modifier key, giving
/// either a bare modifier-key binding or a modifier+modifier combo).
fn parse_hotkey(spec: &str) -> Result<Hotkey> {
    let segments: Vec<&str> = spec.split('+').map(str::trim).collect();
    if segments.iter().any(|s| s.is_empty()) {
        bail!("malformed hotkey {spec:?}: empty key segment");
    }
    let (trigger, mods) = segments
        .split_last()
        .expect("split on '+' always yields at least one segment");

    let modifiers = mods
        .iter()
        .map(|m| {
            parse_modifier(m)
                .with_context(|| format!("{m:?} is not a modifier (in hotkey {spec:?})"))
        })
        .collect::<Result<Vec<_>>>()?;

    let key = parse_key(trigger)
        .with_context(|| format!("unrecognized key {trigger:?} in hotkey {spec:?}"))?;

    Ok(Hotkey { modifiers, key })
}

/// Normalise a segment for matching: uppercase with spaces/underscores removed, so
/// `Right Alt`, `right_alt` and `RIGHTALT` all collapse to the same token.
fn normalize_segment(seg: &str) -> String {
    seg.chars()
        .filter(|c| !c.is_whitespace() && *c != '_')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Parse a combo modifier prefix (`Ctrl`, `Alt`, `Shift`, `Meta`/`Super`/`Cmd`/`Win`).
fn parse_modifier(seg: &str) -> Result<Modifier> {
    Ok(match normalize_segment(seg).as_str() {
        "CTRL" | "CONTROL" => Modifier::Ctrl,
        "ALT" | "OPTION" => Modifier::Alt,
        "SHIFT" => Modifier::Shift,
        "META" | "SUPER" | "WIN" | "WINDOWS" | "CMD" | "COMMAND" => Modifier::Meta,
        other => bail!("unknown modifier {other:?}"),
    })
}

/// Parse the trigger key segment.
fn parse_key(seg: &str) -> Result<Key> {
    use Key::*;
    let token = normalize_segment(seg);
    Ok(match token.as_str() {
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
        "SPACE" => Space,
        "LEFTCTRL" | "CTRLLEFT" | "LCTRL" => LeftCtrl,
        "RIGHTCTRL" | "CTRLRIGHT" | "RCTRL" => RightCtrl,
        "LEFTALT" | "ALTLEFT" | "LALT" => LeftAlt,
        "RIGHTALT" | "ALTRIGHT" | "RALT" | "ALTGR" => RightAlt,
        "LEFTSHIFT" | "SHIFTLEFT" | "LSHIFT" => LeftShift,
        "RIGHTSHIFT" | "SHIFTRIGHT" | "RSHIFT" => RightShift,
        "LEFTMETA" | "METALEFT" | "LMETA" | "LSUPER" | "LWIN" => LeftMeta,
        "RIGHTMETA" | "METARIGHT" | "RMETA" | "RSUPER" | "RWIN" => RightMeta,
        "FN" => Fn,
        single if single.len() == 1 && single.as_bytes()[0].is_ascii_alphabetic() => {
            Letter(single.as_bytes()[0] as char)
        }
        other => bail!("unknown key {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};
    use std::collections::HashMap;

    /// `env_layer` over an empty environment touches nothing.
    fn empty_env() -> SettingsLayer {
        env_layer(|_| None)
    }

    #[test]
    fn clap_defaults_match_the_resolution_defaults() {
        // The defaults declared on `Cli` (shown in --help) must stay in sync
        // with the constants the layered merge falls back to.
        let cli = Cli::parse_from(["dit"]);
        assert_eq!(cli.language, DEFAULT_LANGUAGE);
        assert_eq!(cli.model, DEFAULT_MODEL);
        assert_eq!(cli.hotkey, DEFAULT_HOTKEY);
        assert_eq!(cli.region, DEFAULT_REGION);
        assert_eq!(cli.vad_silence, DEFAULT_VAD_SILENCE);
    }

    #[test]
    fn empty_layers_yield_the_built_in_defaults() {
        let resolved = merge(
            SettingsLayer::default(),
            SettingsLayer::default(),
            SettingsLayer::default(),
        );
        assert_eq!(resolved.language, DEFAULT_LANGUAGE);
        assert_eq!(resolved.model, DEFAULT_MODEL);
        assert_eq!(resolved.hotkey, DEFAULT_HOTKEY);
        assert_eq!(resolved.region, DEFAULT_REGION);
        assert_eq!(resolved.vad_silence, DEFAULT_VAD_SILENCE);
        assert_eq!(resolved.device, None);
        assert!(!resolved.no_filler);
        assert!(resolved.keyterms.is_empty());
        assert!(!resolved.no_preview);
        assert!(!resolved.paste_shift);
    }

    #[test]
    fn each_layer_overrides_the_one_below_it() {
        // defaults < file < env < cli, asserted on a single field at a time so
        // every rung of the ladder is exercised.
        let file = SettingsLayer {
            language: Some("es".into()),
            model: Some("from_file".into()),
            region: Some("eu".into()),
            no_filler: Some(true),
            ..Default::default()
        };
        let env = SettingsLayer {
            language: Some("en".into()),
            region: Some("us".into()),
            ..Default::default()
        };
        let cli = SettingsLayer {
            language: Some("fr".into()),
            ..Default::default()
        };

        let resolved = merge(file, env, cli);
        // language is set in all three layers → CLI wins.
        assert_eq!(resolved.language, "fr");
        // region is set in file and env → env wins over file.
        assert_eq!(resolved.region, "us");
        // model only in file → file wins over default.
        assert_eq!(resolved.model, "from_file");
        // no_filler only in file → file wins over default.
        assert!(resolved.no_filler);
        // hotkey set nowhere → default.
        assert_eq!(resolved.hotkey, DEFAULT_HOTKEY);
    }

    #[test]
    fn env_layer_parses_typed_values() {
        let vars: HashMap<&str, &str> = [
            ("DIT_LANGUAGE", "de"),
            ("DIT_NO_FILLER", "true"),
            ("DIT_PASTE_SHIFT", "0"),
            ("DIT_KEYTERMS", "Rust, ElevenLabs ,"),
            ("DIT_VAD_SILENCE", "2.5"),
        ]
        .into_iter()
        .collect();
        let layer = env_layer(|k| vars.get(k).map(|v| v.to_string()));

        assert_eq!(layer.language.as_deref(), Some("de"));
        assert_eq!(layer.no_filler, Some(true));
        assert_eq!(layer.paste_shift, Some(false));
        assert_eq!(
            layer.keyterms,
            Some(vec!["Rust".to_string(), "ElevenLabs".to_string()])
        );
        assert_eq!(layer.vad_silence, Some(2.5));
        // Unset variables stay None.
        assert_eq!(layer.region, None);
    }

    #[test]
    fn config_file_round_trips_through_toml() {
        let original = SettingsLayer {
            language: Some("pt".into()),
            hotkey: Some("F8".into()),
            keyterms: Some(vec!["foo".into(), "bar".into()]),
            vad_silence: Some(0.8),
            no_filler: Some(true),
            ..Default::default()
        };
        let serialized = toml::to_string(&original).expect("serializes");
        let parsed: SettingsLayer = toml::from_str(&serialized).expect("parses");
        assert_eq!(original, parsed);

        // And a full round-trip through the on-disk loader used at startup.
        let path = std::env::temp_dir().join(format!("dit-roundtrip-{}.toml", std::process::id()));
        std::fs::write(&path, &serialized).expect("writes temp config");
        let loaded = load_file_config(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(original, loaded);
    }

    #[test]
    fn partial_config_file_only_sets_named_keys() {
        let path = std::env::temp_dir().join(format!("dit-partial-{}.toml", std::process::id()));
        std::fs::write(&path, "language = \"ja\"\n").expect("writes temp config");
        let file = load_file_config(&path);
        let _ = std::fs::remove_file(&path);

        let resolved = merge(file, empty_env(), SettingsLayer::default());
        assert_eq!(resolved.language, "ja");
        // Everything not named in the file keeps its default.
        assert_eq!(resolved.model, DEFAULT_MODEL);
        assert_eq!(resolved.hotkey, DEFAULT_HOTKEY);
    }

    #[test]
    fn missing_or_malformed_config_falls_back_to_defaults() {
        // Missing file → empty layer.
        let missing = std::env::temp_dir().join("dit-does-not-exist-xyzzy.toml");
        assert_eq!(load_file_config(&missing), SettingsLayer::default());

        // Malformed file → empty layer, no panic.
        let path = std::env::temp_dir().join(format!("dit-malformed-{}.toml", std::process::id()));
        std::fs::write(&path, "this is = not [ valid toml").expect("writes temp config");
        let layer = load_file_config(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(layer, SettingsLayer::default());
    }

    #[test]
    fn cli_layer_captures_only_flags_actually_passed() {
        // A flag present on the command line contributes; an absent one (even
        // though clap fills a default) stays None so lower layers show through.
        let matches = Cli::command().get_matches_from(["dit", "--language", "it", "--no-filler"]);
        let cli = Cli::from_arg_matches(&matches).expect("parses");
        let layer = cli_layer(&cli, &matches);

        assert_eq!(layer.language.as_deref(), Some("it"));
        assert_eq!(layer.no_filler, Some(true));
        // `--model` was not passed, so the CLI layer leaves it untouched.
        assert_eq!(layer.model, None);
        assert_eq!(layer.region, None);
    }

    #[test]
    fn passed_cli_flag_overrides_the_config_file() {
        // Regression guard: a value in config.toml is honoured, but a CLI flag
        // still wins — the contract for existing flags.
        let file = SettingsLayer {
            language: Some("es".into()),
            ..Default::default()
        };
        let matches = Cli::command().get_matches_from(["dit", "--language", "en"]);
        let cli = Cli::from_arg_matches(&matches).expect("parses");
        let cli_overrides = cli_layer(&cli, &matches);

        let resolved = merge(file.clone(), empty_env(), cli_overrides);
        assert_eq!(resolved.language, "en");

        // Without the flag, the file value is honoured on the next run.
        let matches = Cli::command().get_matches_from(["dit"]);
        let cli = Cli::from_arg_matches(&matches).expect("parses");
        let resolved = merge(file, empty_env(), cli_layer(&cli, &matches));
        assert_eq!(resolved.language, "es");
    }

    #[test]
    fn parse_hotkey_handles_function_keys() {
        // F1..F12 keep their plain, modifier-free meaning (case-insensitive).
        assert_eq!(
            parse_hotkey("F9").unwrap(),
            Hotkey {
                modifiers: vec![],
                key: Key::F9
            }
        );
        assert_eq!(
            parse_hotkey("f12").unwrap(),
            Hotkey {
                modifiers: vec![],
                key: Key::F12
            }
        );
    }

    #[test]
    fn parse_hotkey_handles_single_modifier_keys() {
        // A lone modifier key is a valid trigger with no held modifiers, and the
        // spelling is forgiving about side/case/spacing.
        for spelling in ["RightAlt", "right alt", "ALT_RIGHT", "AltGr"] {
            assert_eq!(
                parse_hotkey(spelling).unwrap(),
                Hotkey {
                    modifiers: vec![],
                    key: Key::RightAlt
                },
                "spelling {spelling:?} should parse to Right Alt"
            );
        }
        assert_eq!(
            parse_hotkey("RightCtrl").unwrap(),
            Hotkey {
                modifiers: vec![],
                key: Key::RightCtrl
            }
        );
    }

    #[test]
    fn parse_hotkey_handles_combos() {
        // Held modifiers + a trigger key, in order.
        assert_eq!(
            parse_hotkey("Ctrl+Shift+F9").unwrap(),
            Hotkey {
                modifiers: vec![Modifier::Ctrl, Modifier::Shift],
                key: Key::F9
            }
        );
        // Combos can end in a letter or Space, and aliases resolve.
        assert_eq!(
            parse_hotkey("Super+Space").unwrap(),
            Hotkey {
                modifiers: vec![Modifier::Meta],
                key: Key::Space
            }
        );
        assert_eq!(
            parse_hotkey("alt + d").unwrap(),
            Hotkey {
                modifiers: vec![Modifier::Alt],
                key: Key::Letter('D')
            }
        );
    }

    #[test]
    fn parse_hotkey_rejects_unknown_and_malformed_keys() {
        // An unrecognised key name is an error, not a silent no-op.
        let err = parse_hotkey("Banana").unwrap_err().to_string();
        assert!(
            err.contains("Banana"),
            "error should name the bad key: {err}"
        );

        // A modifier in trigger position that isn't a real key is rejected.
        assert!(parse_hotkey("Ctrl+Nope").is_err());

        // A trailing '+' leaves an empty trigger segment.
        assert!(parse_hotkey("Ctrl+").is_err());

        // A non-modifier in a modifier (prefix) position is rejected.
        assert!(parse_hotkey("F9+F10").is_err());
    }

    fn dummy_config(language: &str) -> Config {
        Config {
            api_key: "key".into(),
            language: language.into(),
            model: "scribe_v2_realtime".into(),
            hotkey: Hotkey {
                modifiers: vec![],
                key: Key::F9,
            },
            device: None,
            no_filler: false,
            keyterms: vec![],
            vad_silence: 1.5,
            region: "global".into(),
            no_preview: false,
            paste_shift: false,
            session_max_age_days: DEFAULT_SESSION_MAX_AGE_DAYS,
            session_max_count: DEFAULT_SESSION_MAX_COUNT,
        }
    }

    #[test]
    fn ws_url_explicit_language_includes_language_code_param() {
        let url = dummy_config("pt").ws_url();
        assert!(url.contains("language_code=pt"), "url: {url}");

        let url = dummy_config("en").ws_url();
        assert!(url.contains("language_code=en"), "url: {url}");
    }

    #[test]
    fn ws_url_auto_language_omits_language_code_param() {
        let url = dummy_config("auto").ws_url();
        assert!(
            !url.contains("language_code"),
            "url should have no language_code: {url}"
        );
    }

    #[test]
    fn reconfigure_apply_to_swaps_only_the_named_knob() {
        let mut cfg = dummy_config("pt");
        Reconfigure::Language("en".into()).apply_to(&mut cfg);
        assert_eq!(cfg.language, "en");
        // Other knobs untouched.
        assert_eq!(cfg.model, "scribe_v2_realtime");
        assert_eq!(cfg.device, None);

        Reconfigure::Device(Some("USB Mic".into())).apply_to(&mut cfg);
        assert_eq!(cfg.device.as_deref(), Some("USB Mic"));
        Reconfigure::Device(None).apply_to(&mut cfg);
        assert_eq!(cfg.device, None);

        Reconfigure::NoFiller(true).apply_to(&mut cfg);
        assert!(cfg.no_filler);

        Reconfigure::Model("scribe_v2_realtime".into()).apply_to(&mut cfg);
        assert_eq!(cfg.model, "scribe_v2_realtime");
    }

    #[test]
    fn reconfigure_persist_writes_and_preserves_other_keys() {
        let dir = std::env::temp_dir().join(format!("dit-reconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        // First write into a fresh (non-existent) file: parent dir is created.
        Reconfigure::Language("es".into())
            .persist_to(&path)
            .expect("persists language");
        let loaded = load_file_config(&path);
        assert_eq!(loaded.language.as_deref(), Some("es"));

        // A second, different change preserves the first.
        Reconfigure::NoFiller(true)
            .persist_to(&path)
            .expect("persists no_filler");
        let loaded = load_file_config(&path);
        assert_eq!(loaded.language.as_deref(), Some("es"));
        assert_eq!(loaded.no_filler, Some(true));

        // Re-resolving from the merged layers reflects the persisted choice.
        let resolved = merge(loaded, empty_env(), SettingsLayer::default());
        assert_eq!(resolved.language, "es");
        assert!(resolved.no_filler);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_propagates_through_all_config_layers() {
        // via config file layer
        let file = SettingsLayer {
            language: Some("auto".into()),
            ..Default::default()
        };
        let resolved = merge(file, empty_env(), SettingsLayer::default());
        assert_eq!(resolved.language, "auto");

        // via env layer
        let env = env_layer(|k| (k == "DIT_LANGUAGE").then(|| "auto".into()));
        let resolved = merge(SettingsLayer::default(), env, SettingsLayer::default());
        assert_eq!(resolved.language, "auto");

        // via CLI flag
        let matches = Cli::command().get_matches_from(["dit", "--language", "auto"]);
        let cli = Cli::from_arg_matches(&matches).expect("parses");
        let cli_overrides = cli_layer(&cli, &matches);
        let resolved = merge(SettingsLayer::default(), empty_env(), cli_overrides);
        assert_eq!(resolved.language, "auto");
    }
}
