//! dit — cross-platform push-to-toggle voice dictation.
//!
//! Press the hotkey (F9 by default) to start a session: speak, and each stable
//! transcript segment is typed into whatever window is focused. Press it again
//! to stop. A tray icon shows the state (idle / recording / error) and offers a
//! menu to toggle and quit.
//!
//! The main thread runs a `tao` event loop hosting the global hotkey and the
//! tray icon (both need the platform event loop); audio capture and the
//! WebSocket run on a background Tokio runtime. State changes flow back to the
//! loop through an `EventLoopProxy` so the tray icon can be updated on the main
//! thread.

mod audio;
mod config;
mod inject;
mod notify;
mod output;
mod service;
mod transcribe;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use config::{Cli, Command, Config};
use inject::Injector;
use transcribe::run_session;

/// Control messages from the UI loop to the async session manager.
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

/// Events delivered to the main `tao` loop from background tasks.
#[derive(Debug)]
pub enum UserEvent {
    SetState(IconState),
}

fn main() -> Result<()> {
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
    let injector = Injector::spawn()?;

    // Background Tokio runtime for audio + the WebSocket. Kept alive for the
    // whole program (the event loop below never returns).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Main-thread event loop hosting the hotkey and the tray icon.
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    rt.spawn(manager(cfg.clone(), injector, rx, proxy));

    // Global hotkey.
    let hotkey_manager = GlobalHotKeyManager::new()?;
    let hotkey = HotKey::new(None, cfg.hotkey);
    if let Err(e) = hotkey_manager.register(hotkey) {
        warn!("could not register the hotkey: {e}");
        notify::notify(
            "dit — hotkey unavailable",
            "Another app may have grabbed the key. On Linux/Wayland use an X11 session.",
        );
    }
    let hotkey_id = hotkey.id();

    // Tray icon + menu.
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
        .with_tooltip("dit — idle")
        .with_icon(make_icon(IconState::Idle))
        .build()?;

    info!("ready — press the hotkey (or use the tray) to start/stop dictation");

    let hotkey_rx = GlobalHotKeyEvent::receiver();
    let menu_rx = MenuEvent::receiver();

    // `tao`'s run never returns; `rt`, `tray` and `hotkey_manager` are moved/kept
    // alive for the program's lifetime.
    event_loop.run(move |event, _, control_flow| {
        // Keep `hotkey_manager` alive inside the loop.
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
            let tip = match state {
                IconState::Idle => "dit — idle",
                IconState::Recording => "dit — recording",
                IconState::Error => "dit — error",
            };
            let _ = tray.set_icon(Some(make_icon(state)));
            let _ = tray.set_tooltip(Some(tip));
        }
    });
}

/// Owns the lifecycle of the current session and toggles it on each request.
async fn manager(
    cfg: Config,
    injector: Injector,
    mut rx: UnboundedReceiver<Control>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut current: Option<(Arc<Notify>, JoinHandle<Result<()>>)> = None;

    while let Some(Control::Toggle) = rx.recv().await {
        match current.take() {
            // An active, still-running session → stop it. run_session reports the
            // resulting Idle/Error state itself.
            Some((stop, handle)) if !handle.is_finished() => {
                stop.notify_one();
                tokio::spawn(async move {
                    if let Ok(Err(e)) = handle.await {
                        error!("session error: {e:#}");
                    }
                });
            }
            // No session (or it already ended) → start a fresh one.
            _ => {
                let stop = Arc::new(Notify::new());
                let handle = tokio::spawn(run_session(
                    cfg.clone(),
                    injector.clone(),
                    stop.clone(),
                    proxy.clone(),
                ));
                current = Some((stop, handle));
            }
        }
    }
}

/// Build a 32×32 RGBA disc icon for the given state.
fn make_icon(state: IconState) -> Icon {
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
    Icon::from_rgba(rgba, S as u32, S as u32).expect("valid RGBA icon")
}
