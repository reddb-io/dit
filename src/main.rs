//! dit — cross-platform push-to-toggle voice dictation.
//!
//! Press the hotkey (F9 by default) to start a session: speak, and each stable
//! transcript segment is pasted into whatever window is focused. Press it again
//! to stop. A Rust, multi-platform reimplementation of the original
//! `whisperflow.py` (Linux/Wayland-only) Python script.

mod audio;
mod config;
mod inject;
mod notify;
mod output;
mod transcribe;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rdev::{Event, EventType};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info};

use config::{Cli, Config};
use inject::Injector;
use transcribe::run_session;

/// Control messages from the keyboard thread to the async session manager.
enum Control {
    Toggle,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dit=info".into()),
        )
        .init();

    let cli = Cli::parse();
    if cli.list_devices {
        return audio::list_devices();
    }

    let cfg = Config::resolve(&cli)?;
    let hotkey = cfg.hotkey;
    let injector = Injector::spawn()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (tx, rx) = mpsc::unbounded_channel::<Control>();
    rt.spawn(manager(cfg, injector, rx));

    info!("ready — press the hotkey to start/stop dictation, Ctrl+C to quit");

    // Keyboard listening must run on the main thread (required on macOS) and
    // blocks for the lifetime of the program.
    let mut key_down = false;
    let result = rdev::listen(move |event: Event| match event.event_type {
        // Guard on `!key_down` so key-repeat while held doesn't re-toggle.
        EventType::KeyPress(k) if k == hotkey && !key_down => {
            key_down = true;
            let _ = tx.send(Control::Toggle);
        }
        EventType::KeyRelease(k) if k == hotkey => {
            key_down = false;
        }
        _ => {}
    });

    if let Err(e) = result {
        error!("keyboard listener failed: {e:?}");
        notify::notify(
            "dit error",
            "Could not capture the keyboard. On Linux ensure X11 access or run with the right permissions; on macOS grant Accessibility.",
        );
        std::process::exit(1);
    }
    Ok(())
}

/// Owns the lifecycle of the current session and toggles it on each hotkey.
async fn manager(cfg: Config, injector: Injector, mut rx: UnboundedReceiver<Control>) {
    let mut current: Option<(Arc<Notify>, JoinHandle<Result<()>>)> = None;

    while let Some(Control::Toggle) = rx.recv().await {
        match current.take() {
            // An active, still-running session → stop it.
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
                let handle = tokio::spawn(run_session(cfg.clone(), injector.clone(), stop.clone()));
                current = Some((stop, handle));
            }
        }
    }
}
