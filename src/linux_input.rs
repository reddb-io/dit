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

use crate::config::{FunctionKey, Hotkey, Modifier, SidedModifier, TriggerKey};
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

/// A [`Hotkey`] resolved to raw evdev key codes, ready for the reader threads.
#[derive(Debug)]
pub struct ResolvedHotkey {
    /// The key whose press fires the toggle.
    trigger: u16,
    /// Each required modifier as the set of evdev codes (left/right) that satisfy
    /// it. A modifier counts as held when any of its codes is down.
    modifiers: Vec<Vec<u16>>,
}

/// Map a function key to its evdev code.
fn function_keycode(key: FunctionKey) -> KeyCode {
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

/// Map a single sided modifier key to its evdev code.
fn sided_keycode(key: SidedModifier) -> KeyCode {
    use SidedModifier::*;
    match key {
        LeftCtrl => KeyCode::KEY_LEFTCTRL,
        RightCtrl => KeyCode::KEY_RIGHTCTRL,
        LeftAlt => KeyCode::KEY_LEFTALT,
        RightAlt => KeyCode::KEY_RIGHTALT,
        LeftShift => KeyCode::KEY_LEFTSHIFT,
        RightShift => KeyCode::KEY_RIGHTSHIFT,
        LeftSuper => KeyCode::KEY_LEFTMETA,
        RightSuper => KeyCode::KEY_RIGHTMETA,
    }
}

/// The evdev codes (left and right) that satisfy a side-agnostic modifier.
fn modifier_codes(m: Modifier) -> Vec<u16> {
    use Modifier::*;
    let pair = match m {
        Ctrl => [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL],
        Alt => [KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT],
        Shift => [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT],
        Super => [KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA],
    };
    pair.iter().map(|k| k.code()).collect()
}

/// Resolve a neutral [`Hotkey`] into raw evdev codes, or fail with a clear
/// message for keys evdev cannot capture on Linux.
pub fn resolve_hotkey(hotkey: &Hotkey) -> Result<ResolvedHotkey> {
    let trigger = match hotkey.trigger {
        TriggerKey::Function(f) => function_keycode(f).code(),
        TriggerKey::Modifier(m) => sided_keycode(m).code(),
        TriggerKey::Fn => bail!(
            "the Fn key cannot be captured via evdev on Linux (it is handled in \
             keyboard firmware, below /dev/input); bind a different key such as \
             RightAlt or F9"
        ),
    };
    let modifiers = hotkey.modifiers.iter().copied().map(modifier_codes).collect();
    Ok(ResolvedHotkey { trigger, modifiers })
}

/// Whether every required modifier is currently held (any of its codes down).
fn modifiers_held(held: &HashSet<u16>, modifiers: &[Vec<u16>]) -> bool {
    modifiers
        .iter()
        .all(|codes| codes.iter().any(|c| held.contains(c)))
}

/// Spawn one reader thread per keyboard; each forwards a press of `target` as a
/// toggle. Autorepeat (value 2) and release (value 0) are ignored.
///
/// The device set is re-scanned forever so USB/Bluetooth keyboards that appear
/// after `dit` starts are picked up automatically. Dead readers unregister their
/// path so the monitor can attach again if the kernel reuses the event node.
pub fn spawn_hotkey(hotkey: ResolvedHotkey, tx: UnboundedSender<Control>) -> Result<()> {
    let hotkey = Arc::new(hotkey);
    let active = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
    let debouncer = Arc::new(HotkeyDebouncer::default());
    let found = scan_keyboards(&hotkey, &tx, &active, &debouncer);

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
        let newly_found = scan_keyboards(&hotkey, &tx, &active, &debouncer);
        if newly_found > 0 {
            info!("attached hotkey listener to {newly_found} new keyboard(s)");
        }
    });
    Ok(())
}

fn scan_keyboards(
    hotkey: &Arc<ResolvedHotkey>,
    tx: &UnboundedSender<Control>,
    active: &Arc<Mutex<HashSet<PathBuf>>>,
    debouncer: &Arc<HotkeyDebouncer>,
) -> usize {
    let mut found = 0;
    for (path, dev) in evdev::enumerate() {
        if !is_hotkey_keyboard(&dev, hotkey.trigger) {
            continue;
        }
        {
            let mut active = active.lock().expect("keyboard active-set lock poisoned");
            if !active.insert(path.clone()) {
                continue;
            }
        }
        found += 1;
        let hotkey = hotkey.clone();
        let tx = tx.clone();
        let active = active.clone();
        let debouncer = debouncer.clone();
        std::thread::spawn(move || run_keyboard_reader(path, dev, hotkey, tx, active, debouncer));
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

fn run_keyboard_reader(
    path: PathBuf,
    mut dev: evdev::Device,
    hotkey: Arc<ResolvedHotkey>,
    tx: UnboundedSender<Control>,
    active: Arc<Mutex<HashSet<PathBuf>>>,
    debouncer: Arc<HotkeyDebouncer>,
) {
    let name = dev.name().unwrap_or("<unknown>").to_string();
    info!("hotkey listener attached to {} ({name})", path.display());
    // Track which keys are currently held so combos (Ctrl+Shift+F9) only fire
    // when every required modifier is down at the moment the trigger is pressed.
    let mut held = HashSet::<u16>::new();
    loop {
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() != EventType::KEY {
                        continue;
                    }
                    match ev.value() {
                        1 => {
                            held.insert(ev.code());
                        }
                        0 => {
                            held.remove(&ev.code());
                        }
                        _ => {}
                    }
                    if ev.code() == hotkey.trigger
                        && ev.value() == 1
                        && modifiers_held(&held, &hotkey.modifiers)
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
    fn resolves_function_keys_and_modifier_keys() {
        // A plain function key: trigger code, no modifiers.
        let f9 = resolve_hotkey(&Hotkey {
            modifiers: vec![],
            trigger: TriggerKey::Function(FunctionKey::F9),
        })
        .unwrap();
        assert_eq!(f9.trigger, KeyCode::KEY_F9.code());
        assert!(f9.modifiers.is_empty());

        // A lone modifier key (Right Alt) becomes the trigger.
        let ralt = resolve_hotkey(&Hotkey {
            modifiers: vec![],
            trigger: TriggerKey::Modifier(SidedModifier::RightAlt),
        })
        .unwrap();
        assert_eq!(ralt.trigger, KeyCode::KEY_RIGHTALT.code());
    }

    #[test]
    fn fn_key_is_rejected_with_a_clear_error() {
        let err = resolve_hotkey(&Hotkey {
            modifiers: vec![],
            trigger: TriggerKey::Fn,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("Fn"), "error should mention the Fn key: {err}");
    }

    #[test]
    fn combo_only_fires_when_all_modifiers_are_held() {
        let combo = resolve_hotkey(&Hotkey {
            modifiers: vec![Modifier::Ctrl, Modifier::Shift],
            trigger: TriggerKey::Function(FunctionKey::F9),
        })
        .unwrap();

        let mut held = HashSet::new();
        assert!(!modifiers_held(&held, &combo.modifiers));
        held.insert(KeyCode::KEY_LEFTCTRL.code());
        assert!(!modifiers_held(&held, &combo.modifiers));
        // Either side satisfies a modifier: right shift counts for Shift.
        held.insert(KeyCode::KEY_RIGHTSHIFT.code());
        assert!(modifiers_held(&held, &combo.modifiers));
    }

    #[test]
    fn hotkey_debouncer_suppresses_duplicate_events_in_window() {
        let debouncer = HotkeyDebouncer::default();
        let start = Instant::now();
        assert!(debouncer.should_fire(start));
        assert!(!debouncer.should_fire(start + Duration::from_millis(100)));
        assert!(debouncer.should_fire(start + Duration::from_millis(400)));
    }
}
