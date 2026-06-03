//! Text injection via clipboard + paste keystroke (replaces `wl-copy` +
//! `ydotool key ctrl+v` from the original script).
//!
//! `arboard` and `enigo` are not `Send`, so the injector owns them on a
//! dedicated thread and is driven through a channel. The clipboard instance is
//! kept alive for the whole program so it can keep serving its contents to other
//! apps (a Linux clipboard is served by the owning process).

use std::sync::mpsc::{self, Sender};

use anyhow::Result;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use tracing::{debug, error};

pub enum InjectMsg {
    /// Snapshot the current clipboard so it can be restored later.
    Save,
    /// Put `text` on the clipboard and paste it into the focused app.
    Paste(String),
    /// Restore the clipboard captured by `Save`.
    Restore,
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
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    error!("clipboard unavailable: {e}");
                    return;
                }
            };
            let mut enigo = match Enigo::new(&Settings::default()) {
                Ok(e) => e,
                Err(e) => {
                    error!("input simulation unavailable: {e}");
                    return;
                }
            };

            let mut saved: Option<String> = None;
            while let Ok(msg) = rx.recv() {
                match msg {
                    InjectMsg::Save => {
                        saved = clipboard.get_text().ok();
                    }
                    InjectMsg::Paste(text) => {
                        if let Err(e) = paste(&mut clipboard, &mut enigo, &text) {
                            error!("paste failed: {e}");
                        } else {
                            debug!("pasted: {text}");
                        }
                    }
                    InjectMsg::Restore => {
                        if let Some(prev) = saved.take() {
                            let _ = clipboard.set_text(prev);
                        }
                    }
                }
            }
        });

        Ok(Self { tx })
    }

    pub fn save(&self) {
        let _ = self.tx.send(InjectMsg::Save);
    }

    pub fn paste(&self, text: String) {
        let _ = self.tx.send(InjectMsg::Paste(text));
    }

    pub fn restore(&self) {
        let _ = self.tx.send(InjectMsg::Restore);
    }
}

/// Cmd on macOS, Ctrl elsewhere — the platform paste modifier.
#[cfg(target_os = "macos")]
const PASTE_MODIFIER: Key = Key::Meta;
#[cfg(not(target_os = "macos"))]
const PASTE_MODIFIER: Key = Key::Control;

fn paste(clipboard: &mut arboard::Clipboard, enigo: &mut Enigo, text: &str) -> Result<()> {
    clipboard.set_text(text.to_string())?;
    // Give the clipboard a beat to settle before the paste keystroke.
    std::thread::sleep(std::time::Duration::from_millis(50));
    enigo.key(PASTE_MODIFIER, Direction::Press)?;
    enigo.key(Key::Unicode('v'), Direction::Click)?;
    enigo.key(PASTE_MODIFIER, Direction::Release)?;
    Ok(())
}
