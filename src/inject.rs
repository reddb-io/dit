//! Text injection. The default backend types directly with `enigo` (works in
//! any app and never touches the clipboard) — but on Linux/Wayland, where X11
//! synthetic input doesn't reach native Wayland apps, we fall back to the
//! `/dev/uinput` clipboard-paste backend in [`crate::wayland`].
//!
//! The backend (which isn't `Send`) is owned on a dedicated thread and driven
//! through a channel.

use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::Result;
use tracing::{debug, error};

pub enum InjectMsg {
    /// Type `text` into whatever app is focused.
    Type(String),
}

#[derive(Clone)]
pub struct Injector {
    tx: Sender<InjectMsg>,
}

impl Injector {
    /// Spawn the injector thread. `paste_shift` only affects the Wayland
    /// backend (Ctrl+Shift+V instead of Ctrl+V, for terminals).
    pub fn spawn(paste_shift: bool) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<InjectMsg>();

        #[cfg(target_os = "linux")]
        if crate::wayland::is_wayland() {
            std::thread::spawn(move || run_wayland(rx, paste_shift));
            return Ok(Self { tx });
        }

        let _ = paste_shift;
        std::thread::spawn(move || run_enigo(rx));
        Ok(Self { tx })
    }

    pub fn type_text(&self, text: String) {
        let _ = self.tx.send(InjectMsg::Type(text));
    }
}

/// Default backend: synthesize the characters as keystrokes (X11, macOS, Windows).
fn run_enigo(rx: Receiver<InjectMsg>) {
    use enigo::{Enigo, Keyboard, Settings};
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            error!("input simulation unavailable: {e}");
            return;
        }
    };
    while let Ok(InjectMsg::Type(text)) = rx.recv() {
        if let Err(e) = enigo.text(&text) {
            error!("type failed: {e}");
        } else {
            debug!("typed: {text}");
        }
    }
}

/// Linux/Wayland backend: clipboard + a `/dev/uinput` paste chord.
#[cfg(target_os = "linux")]
fn run_wayland(rx: Receiver<InjectMsg>, shift: bool) {
    let mut paster = match crate::wayland::Paster::new(shift) {
        Ok(p) => p,
        Err(e) => {
            error!("{e:#}");
            return;
        }
    };
    while let Ok(InjectMsg::Type(text)) = rx.recv() {
        if let Err(e) = paster.paste(&text) {
            error!("paste failed: {e}");
        } else {
            debug!("pasted: {text}");
        }
    }
}
