//! Text injection by typing directly into the focused app (enigo).
//!
//! We synthesize the characters as keystrokes rather than going through the
//! clipboard + a paste shortcut, because paste bindings are not universal:
//! terminals reserve Ctrl+V and use Ctrl+Shift+V, while most apps use Ctrl+V —
//! so no single chord works everywhere. Typing works in anything that accepts
//! keyboard input and never touches the user's clipboard.
//!
//! enigo is not `Send`, so the injector owns it on a dedicated thread and is
//! driven through a channel.

use std::sync::mpsc::{self, Sender};

use anyhow::Result;
use enigo::{Enigo, Keyboard, Settings};
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
    /// Spawn the injector thread. Lives for the whole program.
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<InjectMsg>();
        std::thread::spawn(move || {
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
        });

        Ok(Self { tx })
    }

    pub fn type_text(&self, text: String) {
        let _ = self.tx.send(InjectMsg::Type(text));
    }
}
