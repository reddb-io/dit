//! Linux input backend that works under Wayland (and X11): read the hotkey from
//! `/dev/input` (evdev) and "type" by setting the clipboard (`wl-copy`) and
//! emitting the paste chord through a `/dev/uinput` virtual keyboard. It all
//! goes through the kernel, below the compositor — the route the original
//! `whisperflow.py` took with evdev + ydotool, and the only one that works on
//! GNOME Wayland (which blocks X11 global hotkeys and X11 synthetic input).
//!
//! Requires the user to be in the `input` group (to read keyboards) and write
//! access to `/dev/uinput` (a udev rule), plus `wl-clipboard` for `wl-copy`.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, KeyCode, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::Control;

/// Running under a Wayland compositor?
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Map our configured hotkey to an evdev key code.
pub fn evdev_keycode(code: global_hotkey::hotkey::Code) -> KeyCode {
    use global_hotkey::hotkey::Code;
    match code {
        Code::F1 => KeyCode::KEY_F1,
        Code::F2 => KeyCode::KEY_F2,
        Code::F3 => KeyCode::KEY_F3,
        Code::F4 => KeyCode::KEY_F4,
        Code::F5 => KeyCode::KEY_F5,
        Code::F6 => KeyCode::KEY_F6,
        Code::F7 => KeyCode::KEY_F7,
        Code::F8 => KeyCode::KEY_F8,
        Code::F10 => KeyCode::KEY_F10,
        Code::F11 => KeyCode::KEY_F11,
        Code::F12 => KeyCode::KEY_F12,
        _ => KeyCode::KEY_F9,
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

/// A `/dev/uinput` virtual keyboard that pastes by emitting Ctrl+V (optionally
/// Ctrl+Shift+V for terminals). The text itself rides the clipboard, so any
/// Unicode is preserved — uinput only sends the chord.
pub struct Paster {
    device: VirtualDevice,
    shift: bool,
}

impl Paster {
    pub fn new(shift: bool) -> Result<Self> {
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
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(Self { device, shift })
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        set_clipboard(text)?;
        std::thread::sleep(std::time::Duration::from_millis(40));
        self.emit_paste()
    }

    fn emit_paste(&mut self) -> Result<()> {
        let mut down = vec![*KeyEvent::new(KeyCode::KEY_LEFTCTRL, 1)];
        if self.shift {
            down.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 1));
        }
        down.push(*KeyEvent::new(KeyCode::KEY_V, 1));
        self.device.emit(&down)?;

        std::thread::sleep(std::time::Duration::from_millis(8));

        let mut up = vec![*KeyEvent::new(KeyCode::KEY_V, 0)];
        if self.shift {
            up.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 0));
        }
        up.push(*KeyEvent::new(KeyCode::KEY_LEFTCTRL, 0));
        self.device.emit(&up)?;
        Ok(())
    }
}

/// Put `text` on the Wayland clipboard via `wl-copy` (a short-lived subprocess
/// that forks a server — safer than forking from our multi-threaded process).
fn set_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("wl-copy not found — install it:  sudo apt-get install -y wl-clipboard")?;
    {
        let mut stdin = child.stdin.take().context("wl-copy stdin unavailable")?;
        stdin.write_all(text.as_bytes())?;
        // drop closes stdin → wl-copy reads EOF, forks its server, parent exits.
    }
    let _ = child.wait();
    Ok(())
}
