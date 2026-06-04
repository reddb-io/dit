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
mod inject;
#[cfg(target_os = "linux")]
mod linux_input;
mod notify;
mod output;
mod service;
mod transcribe;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info};

use config::{Cli, Command, Config};
use inject::Injector;
use transcribe::run_session;

/// Control messages from the UI to the async session manager.
pub enum Control {
    Toggle,
}

/// What the tray icon should show.
#[derive(Clone, Copy, Debug)]
pub enum IconState {
    Idle,
    Recording,
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

    let cli = Cli::parse();
    if let Some(Command::Service { action }) = &cli.command {
        return service::run(action);
    }
    if cli.list_devices {
        return audio::list_devices();
    }

    let cfg = Config::resolve(&cli)?;
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
    cfg: Config,
    injector: Injector,
    mut rx: UnboundedReceiver<Control>,
    state_tx: UnboundedSender<IconState>,
) {
    let mut current: Option<(Arc<Notify>, JoinHandle<Result<()>>)> = None;

    while let Some(Control::Toggle) = rx.recv().await {
        match current.take() {
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
        }
    }
}

/// Build a 32×32 RGBA disc for the given state.
fn disc_rgba(state: IconState) -> (Vec<u8>, u32) {
    let (r, g, b) = match state {
        IconState::Idle => (0x9e, 0x9e, 0x9e),
        IconState::Recording => (0xe5, 0x39, 0x35),
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
    (rgba, S as u32)
}

fn tooltip(state: IconState) -> &'static str {
    match state {
        IconState::Idle => "dit — idle",
        IconState::Recording => "dit — recording",
        IconState::Error => "dit — error",
    }
}

// ── Linux: ksni (D-Bus) tray + hotkey poll loop, no GTK ──────────────────────

#[cfg(target_os = "linux")]
struct DitTray {
    state: IconState,
    tx: tokio::sync::mpsc::UnboundedSender<Control>,
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
        vec![
            StandardItem {
                label: "Start / stop dictation".into(),
                activate: Box::new(|t: &mut DitTray| {
                    let _ = t.tx.send(Control::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit dit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(target_os = "linux")]
fn run_ui(cfg: Config, injector: Injector, rt: tokio::runtime::Runtime) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<IconState>();
    rt.spawn(manager(cfg.clone(), injector, rx, state_tx));

    // ksni (D-Bus) tray on the Tokio runtime, driven by session state.
    let tray_tx = tx.clone();
    rt.spawn(async move {
        use ksni::TrayMethods;
        let handle = match (DitTray {
            state: IconState::Idle,
            tx: tray_tx,
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
    if let Err(e) = linux_input::spawn_hotkey(linux_input::evdev_keycode(cfg.hotkey), tx) {
        error!("{e:#}");
        notify::notify("dit — hotkey unavailable", &format!("{e}"));
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

#[cfg(not(target_os = "linux"))]
fn run_ui(cfg: Config, injector: Injector, rt: tokio::runtime::Runtime) -> Result<()> {
    use global_hotkey::hotkey::{Code, HotKey};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    use std::time::{Duration, Instant};
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let code = match cfg.hotkey {
        config::FunctionKey::F1 => Code::F1,
        config::FunctionKey::F2 => Code::F2,
        config::FunctionKey::F3 => Code::F3,
        config::FunctionKey::F4 => Code::F4,
        config::FunctionKey::F5 => Code::F5,
        config::FunctionKey::F6 => Code::F6,
        config::FunctionKey::F7 => Code::F7,
        config::FunctionKey::F8 => Code::F8,
        config::FunctionKey::F9 => Code::F9,
        config::FunctionKey::F10 => Code::F10,
        config::FunctionKey::F11 => Code::F11,
        config::FunctionKey::F12 => Code::F12,
    };

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
    let hotkey = HotKey::new(None, code);
    if let Err(e) = hotkey_manager.register(hotkey) {
        tracing::warn!("could not register the hotkey: {e}");
    }
    let hotkey_id = hotkey.id();

    let make_icon = |state: IconState| -> Icon {
        let (rgba, size) = disc_rgba(state);
        Icon::from_rgba(rgba, size, size).expect("valid RGBA icon")
    };

    let menu = Menu::new();
    let toggle_item = MenuItem::new("Start / stop dictation", true, None);
    let quit_item = MenuItem::new("Quit dit", true, None);
    menu.append(&toggle_item).ok();
    menu.append(&PredefinedMenuItem::separator()).ok();
    menu.append(&quit_item).ok();
    let toggle_id = toggle_item.id().clone();
    let quit_id = quit_item.id().clone();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip(IconState::Idle))
        .with_icon(make_icon(IconState::Idle))
        .build()?;

    info!("ready — press the hotkey (or use the tray) to start/stop dictation");

    let hotkey_rx = GlobalHotKeyEvent::receiver();
    let menu_rx = MenuEvent::receiver();

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
            }
        }
        if let Event::UserEvent(UserEvent::SetState(state)) = event {
            let _ = tray.set_icon(Some(make_icon(state)));
            let _ = tray.set_tooltip(Some(tooltip(state)));
        }
    });
}
