//! Text injection. macOS/Windows type directly with `enigo`. Linux uses the
//! self-contained [`crate::linux_input`] backend (clipboard + `/dev/uinput`),
//! which works on both X11 and Wayland and needs no external libraries.
//!
//! The backend (which isn't `Send`) is owned on a dedicated thread and driven
//! through a channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::{error, info};

pub enum InjectMsg {
    /// Type `text` into whatever app is focused.
    Type(String),
    Start(String),
    Partial {
        dictation_id: String,
        text: String,
    },
    Commit {
        dictation_id: String,
        text: String,
    },
    Finish(String),
}

#[derive(Clone)]
pub struct Injector {
    tx: Sender<InjectMsg>,
    dictation_id: Arc<Mutex<Option<String>>>,
}

impl Injector {
    /// Spawn the injector thread. On Linux this honours `cfg.delivery`
    /// (terminal-aware zellij routing, or a fixed paste/type path),
    /// `cfg.paste_shift` (the fallback when auto focus detection is unavailable;
    /// known terminals select Ctrl+Shift+V automatically),
    /// `cfg.type_hybrid` (type via uinput with a clipboard fallback instead of
    /// pasting) and
    /// `cfg.layout` (which char → keycode map the typing path uses; `auto`
    /// detects the active layout once, here). macOS/Windows always type via
    /// enigo, which follows the OS input method, so none of this applies.
    pub fn spawn(cfg: &crate::config::Config) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<InjectMsg>();

        #[cfg(target_os = "linux")]
        {
            let paste_shift = cfg.paste_shift;
            let delivery = cfg.delivery;
            let type_hybrid = delivery.types(cfg.type_hybrid);
            let layout = crate::layout::resolve(cfg.layout);
            std::thread::spawn(move || {
                crate::linux_input::run_injector(rx, delivery, paste_shift, type_hybrid, layout)
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = cfg;
            std::thread::spawn(move || run_enigo(rx));
        }

        Ok(Self {
            tx,
            dictation_id: Arc::new(Mutex::new(None)),
        })
    }

    pub fn start(&self) {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        *self.dictation_id.lock().expect("dictation lock poisoned") = Some(id.clone());
        let _ = self.tx.send(InjectMsg::Start(id));
    }

    pub fn partial(&self, text: String) {
        let Some(dictation_id) = self
            .dictation_id
            .lock()
            .expect("dictation lock poisoned")
            .clone()
        else {
            return;
        };
        let _ = self.tx.send(InjectMsg::Partial { dictation_id, text });
    }

    pub fn commit(&self, text: String) {
        let Some(dictation_id) = self
            .dictation_id
            .lock()
            .expect("dictation lock poisoned")
            .clone()
        else {
            self.type_text(text);
            return;
        };
        let _ = self.tx.send(InjectMsg::Commit { dictation_id, text });
    }

    pub fn finish(&self) {
        let Some(dictation_id) = self
            .dictation_id
            .lock()
            .expect("dictation lock poisoned")
            .take()
        else {
            return;
        };
        let _ = self.tx.send(InjectMsg::Finish(dictation_id));
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
    while let Ok(message) = rx.recv() {
        let text = match message {
            InjectMsg::Type(text) => text,
            InjectMsg::Commit { text, .. } => format!("{text} "),
            _ => continue,
        };
        let chars = text.chars().count();
        if let Err(e) = enigo.text(&text) {
            error!("delivery failed: enigo text input failed ({chars} chars): {e}");
        } else {
            debug!("typed: {text}");
            info!("delivery emitted: enigo text input ({chars} chars)");
        }
    }
}
