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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, KeyCode, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use crate::config::{Hotkey, Key, Modifier};
use crate::inject::InjectMsg;
use crate::Control;

const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(350);

#[derive(Default)]
struct HotkeyDebouncer {
    last: Mutex<Option<Instant>>,
}

impl HotkeyDebouncer {
    fn should_fire(&self, now: Instant) -> bool {
        let mut last = self.last.lock().expect("hotkey debounce lock poisoned");
        if last.is_some_and(|last| now.duration_since(last) < HOTKEY_DEBOUNCE) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// A hotkey resolved to evdev key codes: the trigger code that fires the toggle,
/// plus the modifier keys (each accepting its left or right side) that must be held
/// at the same time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvdevBinding {
    /// The trigger key's evdev code.
    pub trigger: u16,
    /// One entry per required modifier; the modifier is satisfied if *either* of
    /// its two codes (left/right) is currently held.
    pub modifiers: Vec<[u16; 2]>,
}

/// Map our neutral hotkey to evdev key codes. Returns a clear error for keys evdev
/// can't capture (the laptop `Fn` key is handled in firmware, never reaching us).
pub fn evdev_binding(hotkey: &Hotkey) -> Result<EvdevBinding> {
    let trigger = key_keycode(hotkey.key)?.code();
    let modifiers = hotkey
        .modifiers
        .iter()
        .map(|m| {
            let (l, r) = modifier_keycodes(*m);
            [l.code(), r.code()]
        })
        .collect();
    Ok(EvdevBinding { trigger, modifiers })
}

/// The left/right evdev codes a combo modifier matches.
fn modifier_keycodes(m: Modifier) -> (KeyCode, KeyCode) {
    match m {
        Modifier::Ctrl => (KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL),
        Modifier::Alt => (KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT),
        Modifier::Shift => (KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT),
        Modifier::Meta => (KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA),
    }
}

/// Map a trigger [`Key`] to its evdev code.
fn key_keycode(key: Key) -> Result<KeyCode> {
    use Key::*;
    Ok(match key {
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
        Space => KeyCode::KEY_SPACE,
        LeftCtrl => KeyCode::KEY_LEFTCTRL,
        RightCtrl => KeyCode::KEY_RIGHTCTRL,
        LeftAlt => KeyCode::KEY_LEFTALT,
        RightAlt => KeyCode::KEY_RIGHTALT,
        LeftShift => KeyCode::KEY_LEFTSHIFT,
        RightShift => KeyCode::KEY_RIGHTSHIFT,
        LeftMeta => KeyCode::KEY_LEFTMETA,
        RightMeta => KeyCode::KEY_RIGHTMETA,
        Letter(c) => letter_keycode(c)?,
        Fn => bail!(
            "the Fn key is handled in keyboard firmware and cannot be captured via \
             evdev on Linux; pick another hotkey (e.g. RightAlt or Ctrl+Shift+F9)"
        ),
    })
}

/// Map an uppercase letter to its evdev code.
fn letter_keycode(c: char) -> Result<KeyCode> {
    Ok(match c {
        'A' => KeyCode::KEY_A,
        'B' => KeyCode::KEY_B,
        'C' => KeyCode::KEY_C,
        'D' => KeyCode::KEY_D,
        'E' => KeyCode::KEY_E,
        'F' => KeyCode::KEY_F,
        'G' => KeyCode::KEY_G,
        'H' => KeyCode::KEY_H,
        'I' => KeyCode::KEY_I,
        'J' => KeyCode::KEY_J,
        'K' => KeyCode::KEY_K,
        'L' => KeyCode::KEY_L,
        'M' => KeyCode::KEY_M,
        'N' => KeyCode::KEY_N,
        'O' => KeyCode::KEY_O,
        'P' => KeyCode::KEY_P,
        'Q' => KeyCode::KEY_Q,
        'R' => KeyCode::KEY_R,
        'S' => KeyCode::KEY_S,
        'T' => KeyCode::KEY_T,
        'U' => KeyCode::KEY_U,
        'V' => KeyCode::KEY_V,
        'W' => KeyCode::KEY_W,
        'X' => KeyCode::KEY_X,
        'Y' => KeyCode::KEY_Y,
        'Z' => KeyCode::KEY_Z,
        other => bail!("letter {other:?} has no evdev key code"),
    })
}

/// Spawn one reader thread per keyboard; each forwards a press of `target` as a
/// toggle. Autorepeat (value 2) and release (value 0) are ignored.
///
/// The device set is re-scanned forever so USB/Bluetooth keyboards that appear
/// after `dit` starts are picked up automatically. Dead readers unregister their
/// path so the monitor can attach again if the kernel reuses the event node.
pub fn spawn_hotkey(binding: EvdevBinding, tx: UnboundedSender<Control>) -> Result<()> {
    let binding = Arc::new(binding);
    let active = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
    let debouncer = Arc::new(HotkeyDebouncer::default());
    let found = scan_keyboards(&binding, &tx, &active, &debouncer);

    if found == 0 && !Path::new("/dev/input").exists() {
        bail!(
            "no readable keyboards under /dev/input — add yourself to the 'input' group:\n  \
             sudo usermod -aG input $USER   (then log out and back in)"
        );
    }
    if found == 0 {
        warn!("no readable hotkey-capable keyboard found yet; monitoring /dev/input");
    } else {
        info!("listening for the hotkey on {found} keyboard(s) via evdev");
    }

    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        let newly_found = scan_keyboards(&binding, &tx, &active, &debouncer);
        if newly_found > 0 {
            info!("attached hotkey listener to {newly_found} new keyboard(s)");
        }
    });
    Ok(())
}

fn scan_keyboards(
    binding: &Arc<EvdevBinding>,
    tx: &UnboundedSender<Control>,
    active: &Arc<Mutex<HashSet<PathBuf>>>,
    debouncer: &Arc<HotkeyDebouncer>,
) -> usize {
    let mut found = 0;
    for (path, dev) in evdev::enumerate() {
        if !is_hotkey_keyboard(&dev, binding.trigger) {
            continue;
        }
        {
            let mut active = active.lock().expect("keyboard active-set lock poisoned");
            if !active.insert(path.clone()) {
                continue;
            }
        }
        found += 1;
        let tx = tx.clone();
        let active = active.clone();
        let debouncer = debouncer.clone();
        let binding = binding.clone();
        std::thread::spawn(move || run_keyboard_reader(path, dev, binding, tx, active, debouncer));
    }
    found
}

fn is_hotkey_keyboard(dev: &evdev::Device, target_code: u16) -> bool {
    let Some(keys) = dev.supported_keys() else {
        return false;
    };
    let has_target = keys.iter().any(|k| k.code() == target_code);
    let looks_like_keyboard = keys.contains(KeyCode::KEY_SPACE) && keys.contains(KeyCode::KEY_A);
    has_target && looks_like_keyboard
}

/// True when every modifier the binding requires is currently held (either the
/// left or right side counts). Vacuously true for a modifier-free hotkey.
fn modifiers_satisfied(binding: &EvdevBinding, held: &HashSet<u16>) -> bool {
    binding
        .modifiers
        .iter()
        .all(|pair| pair.iter().any(|code| held.contains(code)))
}

fn run_keyboard_reader(
    path: PathBuf,
    mut dev: evdev::Device,
    binding: Arc<EvdevBinding>,
    tx: UnboundedSender<Control>,
    active: Arc<Mutex<HashSet<PathBuf>>>,
    debouncer: Arc<HotkeyDebouncer>,
) {
    let name = dev.name().unwrap_or("<unknown>").to_string();
    info!("hotkey listener attached to {} ({name})", path.display());
    // Codes that matter for the combo modifier state, tracked per device.
    let modifier_codes: HashSet<u16> = binding.modifiers.iter().flatten().copied().collect();
    let mut held: HashSet<u16> = HashSet::new();
    loop {
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() != EventType::KEY {
                        continue;
                    }
                    // Track modifier press/release so a combo can check what's held.
                    if modifier_codes.contains(&ev.code()) {
                        match ev.value() {
                            0 => {
                                held.remove(&ev.code());
                            }
                            1 => {
                                held.insert(ev.code());
                            }
                            _ => {}
                        }
                    }
                    if ev.code() == binding.trigger
                        && ev.value() == 1
                        && modifiers_satisfied(&binding, &held)
                    {
                        if debouncer.should_fire(Instant::now()) {
                            let _ = tx.send(Control::Toggle);
                        } else {
                            debug!("ignored duplicate hotkey event within debounce window");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    "keyboard reader stopped for {} ({name}): {e}",
                    path.display()
                );
                active
                    .lock()
                    .expect("keyboard active-set lock poisoned")
                    .remove(&path);
                break;
            }
        }
    }
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
        let chars = text.chars().count();
        info!(
            "delivery started: clipboard + {} paste chord ({chars} chars)",
            if shift { "Ctrl+Shift+V" } else { "Ctrl+V" }
        );
        if let Err(e) = paster.paste(&text) {
            error!("delivery failed: clipboard/uinput paste failed ({chars} chars): {e:#}");
        } else {
            debug!("pasted: {text}");
            info!(
                "delivery emitted: clipboard set and {} paste chord sent ({chars} chars)",
                if shift { "Ctrl+Shift+V" } else { "Ctrl+V" }
            );
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
        match self.clipboard.get_text() {
            Ok(current) if current == text => {
                info!("delivery checkpoint: clipboard readback matched")
            }
            Ok(current) => warn!(
                "delivery checkpoint: clipboard readback mismatch (wanted {} bytes, got {} bytes)",
                text.len(),
                current.len()
            ),
            Err(e) => warn!("delivery checkpoint: clipboard readback failed: {e}"),
        }
        std::thread::sleep(Duration::from_millis(40));
        self.emit_paste()
    }

    fn emit_paste(&mut self) -> Result<()> {
        info!(
            "delivery checkpoint: emitting {} via /dev/uinput",
            if self.shift { "Ctrl+Shift+V" } else { "Ctrl+V" }
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_debouncer_suppresses_duplicate_events_in_window() {
        let debouncer = HotkeyDebouncer::default();
        let start = Instant::now();
        assert!(debouncer.should_fire(start));
        assert!(!debouncer.should_fire(start + Duration::from_millis(100)));
        assert!(debouncer.should_fire(start + Duration::from_millis(400)));
    }

    #[test]
    fn evdev_binding_maps_function_keys_and_combos() {
        // A plain function key: no modifiers, just the trigger code.
        let f9 = evdev_binding(&Hotkey {
            modifiers: vec![],
            key: Key::F9,
        })
        .unwrap();
        assert_eq!(f9.trigger, KeyCode::KEY_F9.code());
        assert!(f9.modifiers.is_empty());

        // A single modifier key binds as its own trigger.
        let ralt = evdev_binding(&Hotkey {
            modifiers: vec![],
            key: Key::RightAlt,
        })
        .unwrap();
        assert_eq!(ralt.trigger, KeyCode::KEY_RIGHTALT.code());

        // A combo records both sides of each held modifier.
        let combo = evdev_binding(&Hotkey {
            modifiers: vec![Modifier::Ctrl, Modifier::Shift],
            key: Key::F9,
        })
        .unwrap();
        assert_eq!(
            combo.modifiers,
            vec![
                [KeyCode::KEY_LEFTCTRL.code(), KeyCode::KEY_RIGHTCTRL.code()],
                [KeyCode::KEY_LEFTSHIFT.code(), KeyCode::KEY_RIGHTSHIFT.code()],
            ]
        );
    }

    #[test]
    fn evdev_binding_rejects_uncapturable_fn_key() {
        let err = evdev_binding(&Hotkey {
            modifiers: vec![],
            key: Key::Fn,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("Fn"), "error should name the Fn key: {err}");
    }

    #[test]
    fn modifiers_satisfied_accepts_either_side() {
        let binding = EvdevBinding {
            trigger: KeyCode::KEY_F9.code(),
            modifiers: vec![[KeyCode::KEY_LEFTCTRL.code(), KeyCode::KEY_RIGHTCTRL.code()]],
        };
        let mut held = HashSet::new();
        assert!(!modifiers_satisfied(&binding, &held));
        held.insert(KeyCode::KEY_RIGHTCTRL.code());
        assert!(modifiers_satisfied(&binding, &held));
    }
}
