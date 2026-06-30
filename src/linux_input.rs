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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, KeyCode, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use crate::config::{FunctionKey, RecordMode};
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

/// Translates raw evdev key values for the hotkey into [`Control`] messages
/// according to the active [`RecordMode`]. Shared across every keyboard reader
/// thread so duplicate events from multiple device nodes are handled coherently.
struct HotkeyState {
    mode: RecordMode,
    /// Toggle mode: time-based debounce that swallows duplicate key-down bursts.
    debouncer: HotkeyDebouncer,
    /// Hold mode: whether the hotkey is currently held down.
    held: AtomicBool,
}

impl HotkeyState {
    fn new(mode: RecordMode) -> Self {
        Self {
            mode,
            debouncer: HotkeyDebouncer::default(),
            held: AtomicBool::new(false),
        }
    }

    /// Map one evdev key `value` (1 = press, 0 = release, 2 = autorepeat) to a
    /// control message, or `None` when the event should be ignored.
    fn on_event(&self, value: i32, now: Instant) -> Option<Control> {
        match self.mode {
            // Press-to-toggle: act on key-down only, debounced. Release and
            // autorepeat are ignored.
            RecordMode::Toggle => {
                (value == 1 && self.debouncer.should_fire(now)).then_some(Control::Toggle)
            }
            // Hold-to-record: start on the first key-down, stop on key-up. The
            // held flag makes both edges idempotent, so duplicate press/release
            // events (multiple device nodes) and autorepeat can't double-fire.
            RecordMode::Hold => match value {
                1 => (!self.held.swap(true, Ordering::SeqCst)).then_some(Control::Start),
                0 => self
                    .held
                    .swap(false, Ordering::SeqCst)
                    .then_some(Control::Stop),
                _ => None,
            },
        }
    }
}

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

/// Spawn one reader thread per keyboard; each forwards hotkey presses (and, in
/// hold mode, releases) of `target` as [`Control`] messages. The active
/// [`RecordMode`] decides whether key-down toggles or key-down/up start/stop.
///
/// The device set is re-scanned forever so USB/Bluetooth keyboards that appear
/// after `dit` starts are picked up automatically. Dead readers unregister their
/// path so the monitor can attach again if the kernel reuses the event node.
pub fn spawn_hotkey(target: KeyCode, mode: RecordMode, tx: UnboundedSender<Control>) -> Result<()> {
    let target_code = target.code();
    let active = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
    let state = Arc::new(HotkeyState::new(mode));
    let found = scan_keyboards(target_code, &tx, &active, &state);

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
        let newly_found = scan_keyboards(target_code, &tx, &active, &state);
        if newly_found > 0 {
            info!("attached hotkey listener to {newly_found} new keyboard(s)");
        }
    });
    Ok(())
}

fn scan_keyboards(
    target_code: u16,
    tx: &UnboundedSender<Control>,
    active: &Arc<Mutex<HashSet<PathBuf>>>,
    state: &Arc<HotkeyState>,
) -> usize {
    let mut found = 0;
    for (path, dev) in evdev::enumerate() {
        if !is_hotkey_keyboard(&dev, target_code) {
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
        let state = state.clone();
        std::thread::spawn(move || run_keyboard_reader(path, dev, target_code, tx, active, state));
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
    target_code: u16,
    tx: UnboundedSender<Control>,
    active: Arc<Mutex<HashSet<PathBuf>>>,
    state: Arc<HotkeyState>,
) {
    let name = dev.name().unwrap_or("<unknown>").to_string();
    info!("hotkey listener attached to {} ({name})", path.display());
    loop {
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if ev.event_type() == EventType::KEY && ev.code() == target_code {
                        match state.on_event(ev.value(), Instant::now()) {
                            Some(control) => {
                                let _ = tx.send(control);
                            }
                            None => debug!("ignored hotkey event (value={})", ev.value()),
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
    fn toggle_mode_fires_on_press_and_debounces() {
        let state = HotkeyState::new(RecordMode::Toggle);
        let t0 = Instant::now();
        // Key-down toggles.
        assert_eq!(state.on_event(1, t0), Some(Control::Toggle));
        // Release and autorepeat are ignored.
        assert_eq!(state.on_event(0, t0), None);
        assert_eq!(state.on_event(2, t0), None);
        // A duplicate press within the debounce window is suppressed.
        assert_eq!(state.on_event(1, t0 + Duration::from_millis(100)), None);
        // A genuine later press fires again.
        assert_eq!(
            state.on_event(1, t0 + Duration::from_millis(400)),
            Some(Control::Toggle)
        );
    }

    #[test]
    fn hold_mode_starts_on_press_and_stops_on_release() {
        let state = HotkeyState::new(RecordMode::Hold);
        let t0 = Instant::now();
        assert_eq!(state.on_event(1, t0), Some(Control::Start));
        // Autorepeat while held does nothing.
        assert_eq!(state.on_event(2, t0), None);
        assert_eq!(state.on_event(0, t0), Some(Control::Stop));
    }

    #[test]
    fn hold_mode_deduplicates_repeated_edges() {
        let state = HotkeyState::new(RecordMode::Hold);
        let t0 = Instant::now();
        // Duplicate key-down events (e.g. two device nodes) start only once.
        assert_eq!(state.on_event(1, t0), Some(Control::Start));
        assert_eq!(state.on_event(1, t0), None);
        // Duplicate key-up events stop only once.
        assert_eq!(state.on_event(0, t0), Some(Control::Stop));
        assert_eq!(state.on_event(0, t0), None);
        // A release with nothing held is a no-op.
        assert_eq!(state.on_event(0, t0), None);
    }

    #[test]
    fn hold_mode_allows_rapid_taps_without_debounce_loss() {
        let state = HotkeyState::new(RecordMode::Hold);
        let t0 = Instant::now();
        // A quick press/release pair must not lose the Stop to a debounce window.
        assert_eq!(state.on_event(1, t0), Some(Control::Start));
        assert_eq!(
            state.on_event(0, t0 + Duration::from_millis(20)),
            Some(Control::Stop)
        );
        // And an immediate re-press starts a fresh session.
        assert_eq!(
            state.on_event(1, t0 + Duration::from_millis(30)),
            Some(Control::Start)
        );
    }
}
