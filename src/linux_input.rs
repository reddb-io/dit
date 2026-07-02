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
use crate::layout::{char_to_key, typing_keycodes, KeyboardLayout};
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

/// Spawn one reader thread per keyboard; each forwards raw hotkey presses and
/// releases to `tx` as [`Control::KeyDown`] / [`Control::KeyUp`].
///
/// The readers don't interpret toggle-vs-hold — the session manager does,
/// against the *live* recording mode, so the tray can switch modes at runtime.
/// Presses are debounced against OS-level duplicates; releases never are, so a
/// hold-mode stop is always delivered. Autorepeat (`value == 2`) is ignored.
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
                    if ev.code() == binding.trigger && modifiers_satisfied(&binding, &held) {
                        if ev.value() == 1 {
                            if debouncer.should_fire(Instant::now()) {
                                let _ = tx.send(Control::KeyDown);
                            } else {
                                debug!("ignored duplicate hotkey press within debounce window");
                            }
                        } else if ev.value() == 0 {
                            let _ = tx.send(Control::KeyUp);
                        }
                        // autorepeat (value == 2) always ignored
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
///
/// `shift` selects Ctrl+Shift+V over Ctrl+V for the clipboard paste chord.
/// `type_hybrid` opts into the typing-first delivery path: characters `layout`
/// can produce are injected as keystrokes via `/dev/uinput`, and only the
/// characters it can't type (dead-key accents/symbols/emoji) fall back to the
/// clipboard — so most deliveries never touch the clipboard at all.
pub fn run_injector(
    rx: Receiver<InjectMsg>,
    shift: bool,
    type_hybrid: bool,
    layout: KeyboardLayout,
) {
    let mut paster = match Paster::new(shift, type_hybrid, layout) {
        Ok(p) => p,
        Err(e) => {
            error!("{e:#}");
            return;
        }
    };
    while let Ok(InjectMsg::Type(text)) = rx.recv() {
        let chars = text.chars().count();
        let mode = if type_hybrid {
            "typing (uinput, clipboard fallback)"
        } else if shift {
            "clipboard + Ctrl+Shift+V paste chord"
        } else {
            "clipboard + Ctrl+V paste chord"
        };
        info!("delivery started: {mode} ({chars} chars)");
        if let Err(e) = paster.deliver(&text) {
            error!("delivery failed: {mode} failed ({chars} chars): {e:#}");
        } else {
            debug!("delivered: {text}");
            info!("delivery emitted: {mode} ({chars} chars)");
        }
    }
}

/// How long to let the compositor's X11↔Wayland clipboard bridge settle after
/// a `set_text` before reading it back. Mutter's bridge on GNOME/Wayland needs
/// this: when a native image was the last clipboard owner, the `image/*`
/// target can still be live when the receiving app reads, attaching the
/// transcript as an image. The settle plus a verified re-set closes that race.
const CLIPBOARD_SETTLE: Duration = Duration::from_millis(120);
/// How many times to set + verify the clipboard before giving up and emitting
/// the paste chord anyway.
const CLIPBOARD_SET_ATTEMPTS: usize = 3;

/// Clipboard (arboard) + a `/dev/uinput` virtual keyboard. Depending on the
/// delivery mode it either pastes through the clipboard (default) or types the
/// text directly, falling back to the clipboard only for characters the layout
/// can't produce. Held for the program's lifetime so the clipboard keeps serving.
struct Paster {
    device: VirtualDevice,
    clipboard: arboard::Clipboard,
    shift: bool,
    /// When true, deliver by typing (uinput) with a clipboard fallback for
    /// characters the active layout can't type; otherwise paste via clipboard.
    type_hybrid: bool,
    /// The keyboard layout whose char → keycode map the typing path uses.
    layout: KeyboardLayout,
}

impl Paster {
    fn new(shift: bool, type_hybrid: bool, layout: KeyboardLayout) -> Result<Self> {
        let clipboard = arboard::Clipboard::new().context("clipboard unavailable")?;

        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::KEY_LEFTCTRL);
        keys.insert(KeyCode::KEY_LEFTSHIFT);
        keys.insert(KeyCode::KEY_V);
        // The typing path can emit any key the layout map produces, so the
        // virtual device must advertise all of them up front.
        if type_hybrid {
            for key in typing_keycodes(layout) {
                keys.insert(key);
            }
        }
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
            type_hybrid,
            layout,
        })
    }

    /// Deliver `text` using the configured mode.
    fn deliver(&mut self, text: &str) -> Result<()> {
        if self.type_hybrid {
            self.deliver_typed(text)
        } else {
            self.paste(text)
        }
    }

    /// Default path: set the clipboard (verified, with a settle to let the
    /// X11↔Wayland bridge sync) and emit the paste chord.
    fn paste(&mut self, text: &str) -> Result<()> {
        self.set_clipboard_verified(text)?;
        self.emit_paste()
    }

    /// Hybrid typing path. Walk the text in runs: type the runs the layout can
    /// produce as keystrokes, and only for runs of characters it can't type
    /// (dead-key accents/symbols/emoji) fall back to the clipboard. When the
    /// whole text is typeable the clipboard is never touched — so it is not
    /// clobbered and the Wayland image-paste bug cannot occur.
    fn deliver_typed(&mut self, text: &str) -> Result<()> {
        let segments = plan_segments(text, self.layout);
        let needs_clipboard = segments.iter().any(|s| matches!(s, Segment::Paste(_)));
        // Best-effort: only when a fallback is unavoidable do we save the user's
        // clipboard so we can restore it afterwards.
        let saved = if needs_clipboard {
            self.clipboard.get_text().ok()
        } else {
            None
        };

        for segment in segments {
            match segment {
                Segment::Type(run) => {
                    for c in run.chars() {
                        let (key, shift) = char_to_key(self.layout, c)
                            .expect("planner only emits typeable chars in Type runs");
                        self.emit_char(key, shift)?;
                    }
                }
                Segment::Paste(run) => {
                    info!(
                        "delivery checkpoint: clipboard fallback for {} untypeable char(s)",
                        run.chars().count()
                    );
                    self.set_clipboard_verified(&run)?;
                    self.emit_paste()?;
                    // Let the target consume the paste before we touch the
                    // clipboard again (next fallback or restore).
                    std::thread::sleep(CLIPBOARD_SETTLE);
                }
            }
        }

        // Restore whatever the user had, so the typing path never permanently
        // clobbers the clipboard.
        if let Some(prev) = saved {
            if let Err(e) = self.clipboard.set_text(prev) {
                warn!("delivery checkpoint: could not restore clipboard after fallback: {e}");
            }
        }
        Ok(())
    }

    /// Set the clipboard to `text` and verify the readback, retrying a few times
    /// with a settle in between to give the compositor's X11↔Wayland bridge time
    /// to converge before we emit the chord (mitigation B).
    fn set_clipboard_verified(&mut self, text: &str) -> Result<()> {
        for attempt in 1..=CLIPBOARD_SET_ATTEMPTS {
            self.clipboard
                .set_text(text.to_string())
                .context("could not set the clipboard")?;
            // Let Mutter's X11→Wayland bridge sync before reading back.
            std::thread::sleep(CLIPBOARD_SETTLE);
            match self.clipboard.get_text() {
                Ok(current) if current == text => {
                    info!("delivery checkpoint: clipboard readback matched (attempt {attempt})");
                    return Ok(());
                }
                Ok(current) => warn!(
                    "delivery checkpoint: clipboard readback mismatch on attempt {attempt} \
                     (wanted {} bytes, got {} bytes); re-setting",
                    text.len(),
                    current.len()
                ),
                Err(e) => warn!(
                    "delivery checkpoint: clipboard readback failed on attempt {attempt}: {e}; \
                     re-setting"
                ),
            }
        }
        warn!(
            "delivery checkpoint: clipboard still unverified after {CLIPBOARD_SET_ATTEMPTS} \
             attempts; emitting paste anyway"
        );
        Ok(())
    }

    /// Type a single character: hold Shift if the layout needs it, tap the key,
    /// release. Small inter-event sleeps keep fast TUIs from dropping events.
    fn emit_char(&mut self, key: KeyCode, shift: bool) -> Result<()> {
        let mut down = Vec::with_capacity(2);
        if shift {
            down.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 1));
        }
        down.push(*KeyEvent::new(key, 1));
        self.device.emit(&down)?;
        std::thread::sleep(Duration::from_millis(2));

        let mut up = Vec::with_capacity(2);
        up.push(*KeyEvent::new(key, 0));
        if shift {
            up.push(*KeyEvent::new(KeyCode::KEY_LEFTSHIFT, 0));
        }
        self.device.emit(&up)?;
        std::thread::sleep(Duration::from_millis(1));
        Ok(())
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

// ── Hybrid typing: delivery planning ──────────────────────────────────────────
//
// The clipboard path is Unicode/layout-proof but rides the clipboard, which is
// exactly what triggers the Wayland image-paste bug. The typing path injects
// characters directly through `/dev/uinput` using the char → keycode map for
// the active layout (see [`crate::layout`]). Anything the map can't produce
// returns `None` and rides the clipboard fallback, so dictation never loses a
// character even though the bulk of the text avoids the clipboard entirely.

/// One contiguous run of the transcript, tagged by how it will be delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    /// Characters the layout can type directly as keystrokes.
    Type(String),
    /// Characters the layout can't type; delivered via the clipboard fallback.
    Paste(String),
}

/// Split `text` into alternating typeable / untypeable runs, preserving order.
/// Consecutive characters of the same kind are coalesced so the typing path
/// emits as few clipboard fallbacks as possible.
fn plan_segments(text: &str, layout: KeyboardLayout) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for c in text.chars() {
        let typeable = char_to_key(layout, c).is_some();
        match segments.last_mut() {
            Some(Segment::Type(run)) if typeable => run.push(c),
            Some(Segment::Paste(run)) if !typeable => run.push(c),
            _ => segments.push(if typeable {
                Segment::Type(c.to_string())
            } else {
                Segment::Paste(c.to_string())
            }),
        }
    }
    segments
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
                [
                    KeyCode::KEY_LEFTSHIFT.code(),
                    KeyCode::KEY_RIGHTSHIFT.code()
                ],
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
    fn plan_segments_splits_typeable_from_fallback_runs() {
        // Pure ASCII never needs the clipboard.
        assert_eq!(
            plan_segments("ok let's go", KeyboardLayout::Us),
            vec![Segment::Type("ok let's go".to_string())]
        );
        // On US, accents coalesce into one Paste run; the ASCII around them is
        // typed: "informa" typed, "çã" pasted, "o" typed.
        assert_eq!(
            plan_segments("informação", KeyboardLayout::Us),
            vec![
                Segment::Type("informa".to_string()),
                Segment::Paste("çã".to_string()),
                Segment::Type("o".to_string()),
            ]
        );
        // On ABNT2 the ç is a real key — only the ã needs the clipboard.
        assert_eq!(
            plan_segments("informação", KeyboardLayout::Abnt2),
            vec![
                Segment::Type("informaç".to_string()),
                Segment::Paste("ã".to_string()),
                Segment::Type("o".to_string()),
            ]
        );
        // Empty text yields no work.
        assert!(plan_segments("", KeyboardLayout::Us).is_empty());
    }

    #[test]
    fn fully_typeable_text_needs_no_clipboard() {
        // The headline property: typeable transcripts never touch the clipboard,
        // so they can't trigger the Wayland image-paste bug nor clobber it.
        for layout in [KeyboardLayout::Us, KeyboardLayout::Abnt2] {
            let segments = plan_segments("the quick brown fox: jumps! (123)", layout);
            assert!(segments.iter().all(|s| matches!(s, Segment::Type(_))));
        }
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
