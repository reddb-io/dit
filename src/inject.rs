//! Text injection. macOS/Windows type directly with `enigo`. Linux uses the
//! self-contained [`crate::linux_input`] backend (clipboard + `/dev/uinput`),
//! which works on both X11 and Wayland and needs no external libraries.
//!
//! The backend (which isn't `Send`) is owned on a dedicated thread and driven
//! through a channel.

use std::sync::mpsc::{self, Sender};

use anyhow::Result;
use tracing::{error, info};

pub enum InjectMsg {
    /// Type `text` into whatever app is focused.
    Type(String),
}

#[derive(Clone)]
pub struct Injector {
    tx: Sender<InjectMsg>,
}

impl Injector {
    /// Spawn the injector thread. On Linux this honours `cfg.paste_shift`
    /// (Ctrl+Shift+V instead of Ctrl+V, for terminals), `cfg.type_hybrid`
    /// (type via uinput with a clipboard fallback instead of pasting) and
    /// `cfg.layout` (which char → keycode map the typing path uses; `auto`
    /// detects the active layout once, here). macOS/Windows always type via
    /// enigo, which follows the OS input method, so none of this applies.
    pub fn spawn(cfg: &crate::config::Config) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<InjectMsg>();

        #[cfg(target_os = "linux")]
        {
            let paste_shift = cfg.paste_shift;
            let type_hybrid = cfg.type_hybrid;
            let layout = crate::layout::resolve(cfg.layout);
            std::thread::spawn(move || {
                crate::linux_input::run_injector(rx, paste_shift, type_hybrid, layout)
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            std::thread::spawn(move || run_enigo(rx));
        }

        Ok(Self { tx })
    }

    pub fn type_text(&self, text: String) {
        let chars = text.chars().count();
        let bytes = text.len();
        match self.tx.send(InjectMsg::Type(text)) {
            Ok(()) => info!("delivery queued: {chars} chars, {bytes} bytes"),
            Err(e) => error!("delivery queue failed: injector thread is gone: {e}"),
        }
    }
}

/// macOS/Windows backend: synthesize the characters as keystrokes.
#[cfg(not(target_os = "linux"))]
fn run_enigo(rx: std::sync::mpsc::Receiver<InjectMsg>) {
    use enigo::{Enigo, Keyboard, Settings};
    use tracing::{debug, error};

    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            error!("input simulation unavailable: {e}");
            return;
        }
    };
    while let Ok(InjectMsg::Type(text)) = rx.recv() {
        let chars = text.chars().count();
        if let Err(e) = enigo.text(&text) {
            error!("delivery failed: enigo text input failed ({chars} chars): {e}");
        } else {
            debug!("typed: {text}");
            info!("delivery emitted: enigo text input ({chars} chars)");
        }
    }
}
