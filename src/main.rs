//! dit — cross-platform push-to-toggle voice dictation.
//!
//! Press the hotkey (F9 by default) to start a session: speak, and each stable
//! transcript segment is typed into whatever window is focused. Press it again
//! to stop. A tray icon shows the state (idle / recording / error) and offers a
//! menu to toggle and quit.
//!
//! Everything is per-platform to keep dependencies minimal:
//!
//! - **Linux** is fully self-contained (no external libraries/tools): hotkey via
//!   evdev (`/dev/input`), typing via clipboard (arboard) + `/dev/uinput`, tray
//!   via `ksni` (pure-Rust D-Bus). Works on X11 and Wayland; the main thread
//!   idles while the hotkey reads on its own threads. See [`linux_input`].
//! - **macOS/Windows** type with `enigo` and use `tray-icon` + `global-hotkey`
//!   on a `tao` event loop (which those platforms require on the main thread).
//!
//! Audio capture and the WebSocket always run on a background Tokio runtime.

mod audio;
mod config;
mod doctor;
mod engine;
mod inject;
#[cfg(target_os = "linux")]
mod linux_input;
mod models;
mod notify;
mod output;
mod service;
mod settings;
mod transcribe;
mod update;

use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info};

use config::{Cli, Command, Config};
use inject::Injector;
use transcribe::run_session;

/// Control messages from the UI (tray + hotkey) to the async session manager.
///
/// `Toggle` starts/stops a session. `Reconfigure` swaps a runtime knob
/// (device/language/mode/engine) so the *next* session uses it without
/// restarting the process, and persists the choice. `SetPaused` suspends the
/// hotkey (toggles are ignored while paused). `OpenLastTranscript` opens the
/// most recent session log in the platform's default handler.
pub enum Control {
    Toggle,
    Reconfigure(config::Reconfigure),
    SetPaused(bool),
    OpenLastTranscript,
}

/// Language presets offered in the tray "Language" submenu: (label, code).
const LANGUAGE_PRESETS: &[(&str, &str)] = &[
    ("Português", "pt"),
    ("English", "en"),
    ("Español", "es"),
    ("Français", "fr"),
    ("Deutsch", "de"),
    ("Italiano", "it"),
    ("Auto-detect", "auto"),
];

/// Engine/model presets offered in the tray "Engine" submenu: (label, model id).
const ENGINE_PRESETS: &[(&str, &str)] = &[("Scribe v2 Realtime", "scribe_v2_realtime")];

/// Dictation mode presets offered in the tray "Mode" submenu: (label, no_filler).
const MODE_PRESETS: &[(&str, bool)] = &[("Verbatim", false), ("Remove fillers", true)];

/// What the tray icon should show.
#[derive(Clone, Copy, Debug)]
pub enum IconState {
    Idle,
    Recording { level: u8 },
    Error,
}

fn main() -> Result<()> {
    // rustls 0.23 won't pick a crypto provider on its own; choose ring up front
    // so the Scribe WebSocket's TLS handshake doesn't panic.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dit=info".into()),
        )
        .init();

    // Parse via explicit matches so Config::resolve can tell which flags were
    // actually passed on the command line (they win over the config file/env).
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if let Some(Command::Service { action }) = &cli.command {
        return service::run(action);
    }
    if let Some(Command::Doctor) = &cli.command {
        return doctor::run(cli.device.clone());
    }
    if let Some(Command::Update {
        check,
        force,
        version,
    }) = &cli.command
    {
        return update::run(&update::UpdateArgs {
            check: *check,
            force: *force,
            version: version.clone(),
        });
    }
    if let Some(Command::Models { action }) = &cli.command {
        return models::run(action);
    }
    if let Some(Command::Settings) = &cli.command {
        return settings::run();
    }
    if cli.list_devices {
        return audio::list_devices();
    }

    let cfg = Config::resolve(&cli, &matches)?;
    let injector = Injector::spawn(cfg.paste_shift)?;

    // Background Tokio runtime for audio + the WebSocket. Kept alive for the
    // whole program (the UI below never returns).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    run_ui(cfg, injector, rt)
}

/// Owns the lifecycle of the current session and toggles it on each request.
/// `run_session` reports the Idle/Recording/Error state itself over `state_tx`.
async fn manager(
    mut cfg: Config,
    injector: Injector,
    mut rx: UnboundedReceiver<Control>,
    state_tx: UnboundedSender<IconState>,
) {
    let mut current: Option<(Arc<Notify>, JoinHandle<Result<()>>)> = None;
    let mut paused = false;

    while let Some(msg) = rx.recv().await {
        match msg {
            // Paused suspends the hotkey: toggles are ignored until resumed, but
            // a session already running keeps going.
            Control::Toggle if paused => {}
            Control::Toggle => match current.take() {
                Some((stop, handle)) if !handle.is_finished() => {
                    stop.notify_one();
                    tokio::spawn(async move {
                        if let Ok(Err(e)) = handle.await {
                            error!("session error: {e:#}");
                        }
                    });
                }
                _ => {
                    let stop = Arc::new(Notify::new());
                    let handle = tokio::spawn(run_session(
                        cfg.clone(),
                        injector.clone(),
                        stop.clone(),
                        state_tx.clone(),
                    ));
                    current = Some((stop, handle));
                }
            },
            // Apply to the live config so the next session uses it, and persist
            // the choice to ~/.dit/config.toml so it survives a restart.
            Control::Reconfigure(r) => {
                r.apply_to(&mut cfg);
                if let Err(e) = r.persist() {
                    error!("could not persist setting change: {e:#}");
                }
                info!("reconfigured: {r:?} (applies on next session)");
            }
            Control::SetPaused(p) => {
                paused = p;
                info!("hotkey {}", if p { "paused" } else { "resumed" });
            }
            Control::OpenLastTranscript => output::open_last_transcript(),
        }
    }
}

/// Build a 32×32 RGBA disc for the given state.
fn disc_rgba(state: IconState) -> (Vec<u8>, u32) {
    let (r, g, b) = match state {
        IconState::Idle => (0x9e, 0x9e, 0x9e),
        IconState::Recording { .. } => (0xe5, 0x39, 0x35),
        IconState::Error => (0xf5, 0xa6, 0x23),
    };
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];
    let center = (S as f32 - 1.0) / 2.0;
    let radius = center - 1.0;
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let alpha = if dist <= radius {
                255
            } else if dist <= radius + 1.0 {
                (255.0 * (radius + 1.0 - dist)) as u8
            } else {
                0
            };
            let i = (y * S + x) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = alpha;
        }
    }
    if let IconState::Recording { level } = state {
        draw_vu_meter(&mut rgba, S, level);
    }
    (rgba, S as u32)
}

fn vu_bar_count(level: u8) -> usize {
    if level == 0 {
        0
    } else {
        ((level as usize * 5).div_ceil(255)).clamp(1, 5)
    }
}

fn draw_vu_meter(rgba: &mut [u8], size: usize, level: u8) {
    let bars = vu_bar_count(level);
    // System trays shrink icons aggressively, so use the whole 32×32 icon as a
    // fat high-contrast meter instead of tiny bars inside the red disc.
    let heights = [8usize, 12, 16, 21, 26];
    for (bar, height) in heights.iter().enumerate() {
        let x0 = 3 + bar * 6;
        let y0 = 29usize.saturating_sub(*height);
        let active = bar < bars;
        let (r, g, b) = if !active {
            (0x46, 0x10, 0x10)
        } else if bar < 3 {
            (0x38, 0xff, 0x38)
        } else if bar < 4 {
            (0xff, 0xe0, 0x40)
        } else {
            (0xff, 0x38, 0x38)
        };
        for y in y0..29 {
            for x in x0..x0 + 4 {
                let i = (y * size + x) * 4;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
}

#[cfg(test)]
fn meter_pixel_count(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4)
        .filter(|px| {
            px[3] == 255
                && ((px[1] > px[0] && px[1] > px[2])
                    || (px[0] > 0xf0 && px[1] > 0xa0)
                    || (px[0] > 0xf0 && px[2] > 0x40))
        })
        .count()
}

fn tooltip(state: IconState) -> &'static str {
    match state {
        IconState::Idle => "dit — idle",
        IconState::Recording { .. } => "dit — recording",
        IconState::Error => "dit — error",
    }
}

// ── Linux: ksni (D-Bus) tray + hotkey poll loop, no GTK ──────────────────────

#[cfg(target_os = "linux")]
struct DitTray {
    state: IconState,
    tx: tokio::sync::mpsc::UnboundedSender<Control>,
    /// Live mirror of the runtime knobs, so the submenus can show which option
    /// is currently selected (and the Pause item its checkmark). Updated in the
    /// activate callbacks alongside sending the `Reconfigure`/`SetPaused`.
    device: Option<String>,
    language: String,
    no_filler: bool,
    model: String,
    paused: bool,
    /// Input devices to offer, captured once at startup.
    devices: Vec<String>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for DitTray {
    fn id(&self) -> String {
        "io.reddb.dit".into()
    }
    fn title(&self) -> String {
        tooltip(self.state).into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (mut data, size) = disc_rgba(self.state);
        // ksni wants ARGB32; disc_rgba gives RGBA → rotate each pixel right.
        for px in data.chunks_exact_mut(4) {
            px.rotate_right(1);
        }
        vec![ksni::Icon {
            width: size as i32,
            height: size as i32,
            data,
        }]
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Control::Toggle);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        // ── Device submenu: "System default" + each enumerated input device ──
        let mut device_opts: Vec<RadioItem> = vec![RadioItem {
            label: "System default".into(),
            ..Default::default()
        }];
        device_opts.extend(self.devices.iter().map(|n| RadioItem {
            label: n.clone(),
            ..Default::default()
        }));
        // selected: 0 == default, else 1-based index into `devices`.
        let device_selected = match &self.device {
            None => 0,
            Some(cur) => self
                .devices
                .iter()
                .position(|n| n == cur)
                .map(|i| i + 1)
                .unwrap_or(usize::MAX),
        };
        let devices = self.devices.clone();
        let device_menu = SubMenu {
            label: "Device".into(),
            submenu: vec![RadioGroup {
                selected: device_selected,
                select: Box::new(move |t: &mut DitTray, idx: usize| {
                    let dev = if idx == 0 {
                        None
                    } else {
                        devices.get(idx - 1).cloned()
                    };
                    t.device = dev.clone();
                    let _ = t.tx.send(Control::Reconfigure(config::Reconfigure::Device(dev)));
                }),
                options: device_opts,
            }
            .into()],
            ..Default::default()
        };

        // ── Language submenu ──
        let lang_selected = LANGUAGE_PRESETS
            .iter()
            .position(|(_, code)| *code == self.language)
            .unwrap_or(usize::MAX);
        let language_menu = SubMenu {
            label: "Language".into(),
            submenu: vec![RadioGroup {
                selected: lang_selected,
                select: Box::new(|t: &mut DitTray, idx: usize| {
                    if let Some((_, code)) = LANGUAGE_PRESETS.get(idx) {
                        t.language = (*code).into();
                        let _ = t.tx.send(Control::Reconfigure(
                            config::Reconfigure::Language((*code).into()),
                        ));
                    }
                }),
                options: LANGUAGE_PRESETS
                    .iter()
                    .map(|(label, _)| RadioItem {
                        label: (*label).into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        // ── Mode submenu (verbatim vs. filler removal) ──
        let mode_selected = MODE_PRESETS
            .iter()
            .position(|(_, nf)| *nf == self.no_filler)
            .unwrap_or(usize::MAX);
        let mode_menu = SubMenu {
            label: "Mode".into(),
            submenu: vec![RadioGroup {
                selected: mode_selected,
                select: Box::new(|t: &mut DitTray, idx: usize| {
                    if let Some((_, nf)) = MODE_PRESETS.get(idx) {
                        t.no_filler = *nf;
                        let _ = t
                            .tx
                            .send(Control::Reconfigure(config::Reconfigure::NoFiller(*nf)));
                    }
                }),
                options: MODE_PRESETS
                    .iter()
                    .map(|(label, _)| RadioItem {
                        label: (*label).into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        // ── Engine submenu (speech-to-text model) ──
        let engine_selected = ENGINE_PRESETS
            .iter()
            .position(|(_, id)| *id == self.model)
            .unwrap_or(usize::MAX);
        let engine_menu = SubMenu {
            label: "Engine".into(),
            submenu: vec![RadioGroup {
                selected: engine_selected,
                select: Box::new(|t: &mut DitTray, idx: usize| {
                    if let Some((_, id)) = ENGINE_PRESETS.get(idx) {
                        t.model = (*id).into();
                        let _ = t
                            .tx
                            .send(Control::Reconfigure(config::Reconfigure::Model((*id).into())));
                    }
                }),
                options: ENGINE_PRESETS
                    .iter()
                    .map(|(label, _)| RadioItem {
                        label: (*label).into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        let mut items: Vec<ksni::MenuItem<Self>> = vec![
            StandardItem {
                label: "Start / stop dictation".into(),
                activate: Box::new(|t: &mut DitTray| {
                    let _ = t.tx.send(Control::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Pause hotkey".into(),
                checked: self.paused,
                activate: Box::new(|t: &mut DitTray| {
                    t.paused = !t.paused;
                    let _ = t.tx.send(Control::SetPaused(t.paused));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open last transcript".into(),
                activate: Box::new(|t: &mut DitTray| {
                    let _ = t.tx.send(Control::OpenLastTranscript);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            device_menu.into(),
            language_menu.into(),
            mode_menu.into(),
            engine_menu.into(),
            MenuItem::Separator,
        ];
        #[cfg(feature = "gui")]
        {
            items.push(
                StandardItem {
                    label: "Settings\u{2026}".into(),
                    activate: Box::new(|_: &mut DitTray| {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg("settings").spawn();
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }
        items.push(
            StandardItem {
                label: "Quit dit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

#[cfg(target_os = "linux")]
fn run_ui(cfg: Config, injector: Injector, rt: tokio::runtime::Runtime) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<IconState>();
    rt.spawn(manager(cfg.clone(), injector, rx, state_tx));

    // ksni (D-Bus) tray on the Tokio runtime, driven by session state.
    let tray_tx = tx.clone();
    let tray_cfg = cfg.clone();
    let devices = audio::device_names();
    rt.spawn(async move {
        use ksni::TrayMethods;
        let handle = match (DitTray {
            state: IconState::Idle,
            tx: tray_tx,
            device: tray_cfg.device.clone(),
            language: tray_cfg.language.clone(),
            no_filler: tray_cfg.no_filler,
            model: tray_cfg.model.clone(),
            paused: false,
            devices,
        })
        .spawn()
        .await
        {
            Ok(h) => h,
            Err(e) => {
                error!("could not start the tray: {e}");
                return;
            }
        };
        while let Some(state) = state_rx.recv().await {
            let _ = handle.update(move |t: &mut DitTray| t.state = state).await;
        }
    });

    info!("ready — press the hotkey (or use the tray) to start/stop dictation");

    // Hotkey via evdev (/dev/input) — works on X11 and Wayland alike.
    match linux_input::evdev_binding(&cfg.hotkey) {
        Ok(binding) => {
            if let Err(e) = linux_input::spawn_hotkey(binding, tx) {
                error!("{e:#}");
                notify::notify("dit — hotkey unavailable", &format!("{e}"));
            }
        }
        Err(e) => {
            error!("{e:#}");
            notify::notify("dit — hotkey unavailable", &format!("{e}"));
        }
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

// ── macOS/Windows: tray-icon + tao event loop ────────────────────────────────

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
enum UserEvent {
    SetState(IconState),
}

/// Map our neutral hotkey to the `global-hotkey` `(modifiers, code)` pair. Returns
/// a clear error for keys the OS hotkey API can't capture (e.g. the laptop `Fn`).
#[cfg(not(target_os = "linux"))]
fn global_hotkey_binding(
    hotkey: &config::Hotkey,
) -> Result<(
    Option<global_hotkey::hotkey::Modifiers>,
    global_hotkey::hotkey::Code,
)> {
    use anyhow::bail;
    use config::{Key, Modifier};
    use global_hotkey::hotkey::{Code, Modifiers};

    let mut mods = Modifiers::empty();
    for m in &hotkey.modifiers {
        mods |= match m {
            Modifier::Ctrl => Modifiers::CONTROL,
            Modifier::Alt => Modifiers::ALT,
            Modifier::Shift => Modifiers::SHIFT,
            // global-hotkey only honours ALT/SHIFT/CONTROL/SUPER; SUPER is the
            // Cmd/Win/Meta key on every platform.
            Modifier::Meta => Modifiers::SUPER,
        };
    }

    let code = match hotkey.key {
        Key::F1 => Code::F1,
        Key::F2 => Code::F2,
        Key::F3 => Code::F3,
        Key::F4 => Code::F4,
        Key::F5 => Code::F5,
        Key::F6 => Code::F6,
        Key::F7 => Code::F7,
        Key::F8 => Code::F8,
        Key::F9 => Code::F9,
        Key::F10 => Code::F10,
        Key::F11 => Code::F11,
        Key::F12 => Code::F12,
        Key::Space => Code::Space,
        Key::LeftCtrl => Code::ControlLeft,
        Key::RightCtrl => Code::ControlRight,
        Key::LeftAlt => Code::AltLeft,
        Key::RightAlt => Code::AltRight,
        Key::LeftShift => Code::ShiftLeft,
        Key::RightShift => Code::ShiftRight,
        Key::LeftMeta => Code::MetaLeft,
        Key::RightMeta => Code::MetaRight,
        Key::Letter(c) => match c {
            'A' => Code::KeyA,
            'B' => Code::KeyB,
            'C' => Code::KeyC,
            'D' => Code::KeyD,
            'E' => Code::KeyE,
            'F' => Code::KeyF,
            'G' => Code::KeyG,
            'H' => Code::KeyH,
            'I' => Code::KeyI,
            'J' => Code::KeyJ,
            'K' => Code::KeyK,
            'L' => Code::KeyL,
            'M' => Code::KeyM,
            'N' => Code::KeyN,
            'O' => Code::KeyO,
            'P' => Code::KeyP,
            'Q' => Code::KeyQ,
            'R' => Code::KeyR,
            'S' => Code::KeyS,
            'T' => Code::KeyT,
            'U' => Code::KeyU,
            'V' => Code::KeyV,
            'W' => Code::KeyW,
            'X' => Code::KeyX,
            'Y' => Code::KeyY,
            'Z' => Code::KeyZ,
            other => bail!("letter {other:?} has no global-hotkey code"),
        },
        Key::Fn => bail!(
            "the Fn key is handled in keyboard firmware and cannot be captured by \
             the OS global-hotkey API; pick another hotkey (e.g. RightAlt or Ctrl+Shift+F9)"
        ),
    };

    let mods = if mods.is_empty() { None } else { Some(mods) };
    Ok((mods, code))
}

#[cfg(not(target_os = "linux"))]
fn run_ui(cfg: Config, injector: Injector, rt: tokio::runtime::Runtime) -> Result<()> {
    use global_hotkey::hotkey::HotKey;
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use std::time::{Duration, Instant};
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{Icon, TrayIconBuilder};

    /// One selectable runtime-knob option in a tray submenu, paired with the
    /// `Reconfigure` it sends and the group it belongs to (so selecting one
    /// option clears its siblings' checkmarks — radio behaviour).
    struct OptionItem {
        id: MenuId,
        group: u8,
        reconfig: config::Reconfigure,
        item: CheckMenuItem,
    }

    let (modifiers, code) = global_hotkey_binding(&cfg.hotkey)?;

    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<IconState>();
    rt.spawn(manager(cfg.clone(), injector, rx, state_tx));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Forward session state into the event loop.
    rt.spawn(async move {
        while let Some(state) = state_rx.recv().await {
            let _ = proxy.send_event(UserEvent::SetState(state));
        }
    });

    let hotkey_manager = GlobalHotKeyManager::new()?;
    let hotkey = HotKey::new(modifiers, code);
    if let Err(e) = hotkey_manager.register(hotkey) {
        tracing::warn!("could not register the hotkey: {e}");
    }
    let hotkey_id = hotkey.id();

    let make_icon = |state: IconState| -> Icon {
        let (rgba, size) = disc_rgba(state);
        Icon::from_rgba(rgba, size, size).expect("valid RGBA icon")
    };

    let toggle_item = MenuItem::new("Start / stop dictation", true, None);
    let pause_item = CheckMenuItem::new("Pause hotkey", true, false, None);
    let open_item = MenuItem::new("Open last transcript", true, None);
    let quit_item = MenuItem::new("Quit dit", true, None);
    #[cfg(feature = "gui")]
    let settings_item = MenuItem::new("Settings\u{2026}", true, None);

    // Build the four runtime-knob submenus (device/language/mode/engine). Each
    // option is a checkmark; `opts` records the id→Reconfigure mapping plus the
    // group so the event loop can enforce radio behaviour and toggle the right
    // checkmarks.
    let mut opts: Vec<OptionItem> = Vec::new();
    const G_DEVICE: u8 = 0;
    const G_LANGUAGE: u8 = 1;
    const G_MODE: u8 = 2;
    const G_ENGINE: u8 = 3;

    let device_menu = Submenu::new("Device", true);
    {
        let default_item = CheckMenuItem::new("System default", true, cfg.device.is_none(), None);
        device_menu.append(&default_item).ok();
        opts.push(OptionItem {
            id: default_item.id().clone(),
            group: G_DEVICE,
            reconfig: config::Reconfigure::Device(None),
            item: default_item,
        });
        for name in audio::device_names() {
            let checked = cfg.device.as_deref() == Some(name.as_str());
            let item = CheckMenuItem::new(&name, true, checked, None);
            device_menu.append(&item).ok();
            opts.push(OptionItem {
                id: item.id().clone(),
                group: G_DEVICE,
                reconfig: config::Reconfigure::Device(Some(name)),
                item,
            });
        }
    }

    let language_menu = Submenu::new("Language", true);
    for (label, code) in LANGUAGE_PRESETS {
        let item = CheckMenuItem::new(*label, true, cfg.language == *code, None);
        language_menu.append(&item).ok();
        opts.push(OptionItem {
            id: item.id().clone(),
            group: G_LANGUAGE,
            reconfig: config::Reconfigure::Language((*code).into()),
            item,
        });
    }

    let mode_menu = Submenu::new("Mode", true);
    for (label, nf) in MODE_PRESETS {
        let item = CheckMenuItem::new(*label, true, cfg.no_filler == *nf, None);
        mode_menu.append(&item).ok();
        opts.push(OptionItem {
            id: item.id().clone(),
            group: G_MODE,
            reconfig: config::Reconfigure::NoFiller(*nf),
            item,
        });
    }

    let engine_menu = Submenu::new("Engine", true);
    for (label, id) in ENGINE_PRESETS {
        let item = CheckMenuItem::new(*label, true, cfg.model == *id, None);
        engine_menu.append(&item).ok();
        opts.push(OptionItem {
            id: item.id().clone(),
            group: G_ENGINE,
            reconfig: config::Reconfigure::Model((*id).into()),
            item,
        });
    }

    let menu = Menu::new();
    menu.append(&toggle_item).ok();
    menu.append(&pause_item).ok();
    menu.append(&open_item).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&device_menu).ok();
    menu.append(&language_menu).ok();
    menu.append(&mode_menu).ok();
    menu.append(&engine_menu).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    #[cfg(feature = "gui")]
    {
        menu.append(&settings_item).ok();
        menu.append(&PredefinedMenuItem::separator()).ok();
    }
    menu.append(&quit_item).ok();

    let toggle_id = toggle_item.id().clone();
    let quit_id = quit_item.id().clone();
    let pause_id = pause_item.id().clone();
    let open_id = open_item.id().clone();
    #[cfg(feature = "gui")]
    let settings_id = settings_item.id().clone();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip(IconState::Idle))
        .with_icon(make_icon(IconState::Idle))
        .build()?;

    info!("ready — press the hotkey (or use the tray) to start/stop dictation");

    let hotkey_rx = GlobalHotKeyEvent::receiver();
    let menu_rx = MenuEvent::receiver();
    let mut paused = false;

    event_loop.run(move |event, _, control_flow| {
        let _ = &hotkey_manager;
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));

        if let Ok(ev) = hotkey_rx.try_recv() {
            if ev.id == hotkey_id && ev.state == HotKeyState::Pressed {
                let _ = tx.send(Control::Toggle);
            }
        }
        if let Ok(ev) = menu_rx.try_recv() {
            if ev.id == toggle_id {
                let _ = tx.send(Control::Toggle);
            } else if ev.id == quit_id {
                *control_flow = ControlFlow::Exit;
            } else if ev.id == pause_id {
                paused = !paused;
                pause_item.set_checked(paused);
                let _ = tx.send(Control::SetPaused(paused));
            } else if ev.id == open_id {
                let _ = tx.send(Control::OpenLastTranscript);
            } else if let Some(chosen) = opts.iter().find(|o| o.id == ev.id) {
                // Radio behaviour: check the chosen option, clear its siblings.
                for o in &opts {
                    if o.group == chosen.group {
                        o.item.set_checked(o.id == ev.id);
                    }
                }
                let _ = tx.send(Control::Reconfigure(chosen.reconfig.clone()));
            }
            #[cfg(feature = "gui")]
            if ev.id == settings_id {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("settings").spawn();
                }
            }
        }
        if let Event::UserEvent(UserEvent::SetState(state)) = event {
            let _ = tray.set_icon(Some(make_icon(state)));
            let _ = tray.set_tooltip(Some(tooltip(state)));
        }
    });
}

#[cfg(test)]
mod tray_audio_meter_tests {
    use super::*;

    #[test]
    fn vu_meter_maps_quiet_and_loud_levels_to_visible_bar_counts() {
        assert_eq!(vu_bar_count(0), 0);
        assert_eq!(vu_bar_count(1), 1);
        assert_eq!(vu_bar_count(128), 3);
        assert_eq!(vu_bar_count(255), 5);
    }

    #[test]
    fn recording_icon_changes_when_audio_level_changes() {
        let (quiet, quiet_size) = disc_rgba(IconState::Recording { level: 0 });
        let (loud, loud_size) = disc_rgba(IconState::Recording { level: 255 });
        assert_eq!(quiet_size, loud_size);
        assert_ne!(quiet, loud);
        assert!(meter_pixel_count(&loud) > meter_pixel_count(&quiet));
    }
}
