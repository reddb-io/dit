//! Linux input backend (X11 **and** Wayland), fully self-contained.
//!
//! Read the hotkey from `/dev/input` (evdev) and "type" by setting the clipboard
//! (arboard — pure Rust, no C libs) and emitting the paste chord through a
//! `/dev/uinput` virtual keyboard. It all goes through the kernel, below the
//! compositor — the only route that works on GNOME Wayland (which blocks X11
//! global hotkeys and X11 synthetic input), and it needs no external libraries
//! or tools (no libxdo, no wl-clipboard).
//!
//! Requires the `input` group (read keyboards) and write access to
//! `/dev/uinput` (a udev rule). Unicode rides the clipboard; uinput only sends
//! the Ctrl+V (or Ctrl+Shift+V) chord.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, KeyCode, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use crate::config::FunctionKey;
use crate::inject::InjectMsg;
use crate::Control;

/// Map our neutral hotkey to an evdev key code.
pub fn evdev_keycode(key: FunctionKey) -> KeyCode {
    use FunctionKey::*;
    match key {
        F1 => KeyCode::KEY_F1,
        F2 => KeyCode::KEY_F2,
        F3 => KeyCode::KEY_F3,
        F4 => KeyCode::KEY_F4,
        F5 => KeyCode::KEY_F5,
        F6 => KeyCode::KEY_F6,
        F7 => KeyCode::KEY_F7,
        F8 => KeyCode::KEY_F8,
        F9 => KeyCode::KEY_F9,
        F10 => KeyCode::KEY_F10,
        F11 => KeyCode::KEY_F11,
        F12 => KeyCode::KEY_F12,
    }
}

/// Spawn one reader thread per keyboard; each forwards a press of `target` as a
/// toggle. Autorepeat (value 2) and release (value 0) are ignored.
pub fn spawn_hotkey(target: KeyCode, tx: UnboundedSender<Control>) -> Result<()> {
    let target_code = target.code();
    let mut found = 0;

    for (_path, dev) in evdev::enumerate() {
        let is_keyboard = dev
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::KEY_SPACE) && k.contains(KeyCode::KEY_A));
        if !is_keyboard {
            continue;
        }
        found += 1;
        let tx = tx.clone();
        let mut dev = dev;
        std::thread::spawn(move || loop {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if ev.event_type() == EventType::KEY
                            && ev.code() == target_code
                            && ev.value() == 1
                        {
                            let _ = tx.send(Control::Toggle);
                        }
                    }
                }
                Err(e) => {
                    warn!("keyboard reader stopped: {e}");
                    break;
                }
            }
        });
    }

    if found == 0 {
        bail!(
            "no readable keyboards under /dev/input — add yourself to the 'input' group:\n  \
             sudo usermod -aG input $USER   (then log out and back in)"
        );
    }
    info!("listening for the hotkey on {found} keyboard(s) via evdev");
    Ok(())
}

/// Drive the injector backend on a dedicated thread (called from `inject.rs`).
pub fn run_injector(rx: Receiver<InjectMsg>, shift: bool) {
    let mut paster = match Paster::new(shift) {
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

/// Clipboard (arboard) + a `/dev/uinput` virtual keyboard that emits the paste
/// chord. Held for the program's lifetime so the clipboard keeps serving.
struct Paster {
    device: VirtualDevice,
    clipboard: arboard::Clipboard,
    shift: bool,
}

impl Paster {
    fn new(shift: bool) -> Result<Self> {
        let clipboard = arboard::Clipboard::new().context("clipboard unavailable")?;

        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::KEY_LEFTCTRL);
        keys.insert(KeyCode::KEY_LEFTSHIFT);
        keys.insert(KeyCode::KEY_V);
        let device = VirtualDevice::builder()
            .context(
                "cannot open /dev/uinput — grant write access:\n  \
                 echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"' | \
                 sudo tee /etc/udev/rules.d/99-uinput.rules\n  \
                 sudo udevadm control --reload && sudo udevadm trigger\n  \
                 (and be in the 'input' group, then re-login)",
            )?
            .name("dit virtual keyboard")
            .with_keys(&keys)?
            .build()?;

        // Let the compositor register the new keyboard before first use.
        std::thread::sleep(Duration::from_millis(300));
        Ok(Self {
            device,
            clipboard,
            shift,
        })
    }

    fn paste(&mut self, text: &str) -> Result<()> {
        self.clipboard
            .set_text(text.to_string())
            .context("could not set the clipboard")?;
        std::thread::sleep(Duration::from_millis(40));
        self.emit_paste()
    }

    fn emit_paste(&mut self) -> Result<()> {
        let mut down = vec![*KeyEvent::new(KeyCode::KEY_LEFTCTRL, 1)];
        if self.shift {
            down.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 1));
        }
        down.push(*KeyEvent::new(KeyCode::KEY_V, 1));
        self.device.emit(&down)?;

        std::thread::sleep(Duration::from_millis(8));

        let mut up = vec![*KeyEvent::new(KeyCode::KEY_V, 0)];
        if self.shift {
            up.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 0));
        }
        up.push(*KeyEvent::new(KeyCode::KEY_LEFTCTRL, 0));
        self.device.emit(&up)?;
        Ok(())
    }
}
