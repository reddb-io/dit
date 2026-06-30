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
    /// Spawn the injector thread. `paste_shift` and `type_hybrid` only affect
    /// Linux: `paste_shift` uses Ctrl+Shift+V instead of Ctrl+V (for terminals),
    /// and `type_hybrid` opts into typing the text via uinput with a clipboard
    /// fallback instead of pasting (issue #18). macOS/Windows always type via
    /// enigo, so both flags are ignored there.
    pub fn spawn(paste_shift: bool, type_hybrid: bool) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<InjectMsg>();

        #[cfg(target_os = "linux")]
        std::thread::spawn(move || crate::linux_input::run_injector(rx, paste_shift, type_hybrid));

        #[cfg(not(target_os = "linux"))]
        {
            let _ = paste_shift;
            let _ = type_hybrid;
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
