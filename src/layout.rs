//! Keyboard-layout awareness for the Linux typing path (`--type`).
//!
//! uinput speaks *keycodes*, not characters, so typing text requires knowing
//! which physical key (and Shift state) produces each character on the user's
//! layout. This module resolves the active layout — explicitly from config, or
//! by asking the desktop (XKB env → GNOME settings → setxkbmap → localectl →
//! locale hint) — and provides the per-layout char → keycode maps.
//!
//! Characters a layout cannot produce with a single (Shift+)key — dead-key
//! accents, other scripts, emoji — return `None` and ride the clipboard
//! fallback, so no layout ever loses characters; it only changes how many can
//! be typed directly.

use std::process::Command;

use evdev::KeyCode;
use tracing::{info, warn};

use crate::config::LayoutSetting;

/// A concrete layout the typing path knows how to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardLayout {
    /// US/ASCII QWERTY.
    Us,
    /// Brazilian ABNT2: `ç` on its own key, `/?` on the dedicated ABNT key,
    /// punctuation rearranged, and the accents (´ ` ~ ^ ¨) as dead keys.
    Abnt2,
}

impl KeyboardLayout {
    pub fn name(&self) -> &'static str {
        match self {
            KeyboardLayout::Us => "us",
            KeyboardLayout::Abnt2 => "abnt2",
        }
    }
}

/// Resolve the layout to use: an explicit config choice wins; `auto` asks the
/// platform. Logged so a wrong guess is visible and overridable (`--layout`).
pub fn resolve(setting: LayoutSetting) -> KeyboardLayout {
    match setting {
        LayoutSetting::Us => KeyboardLayout::Us,
        LayoutSetting::Abnt2 => KeyboardLayout::Abnt2,
        LayoutSetting::Auto => {
            let detected = detect();
            info!(
                "keyboard layout: {} (auto-detected; pin with --layout or `layout` in config.toml)",
                detected.name()
            );
            detected
        }
    }
}

/// Ask the platform which layout is active, most-authoritative source first.
fn detect() -> KeyboardLayout {
    // 1. wlroots compositors (sway, hyprland, …) export the XKB choice.
    if let Ok(xkb) = std::env::var("XKB_DEFAULT_LAYOUT") {
        if let Some(layout) = layout_from_token(first_layout(&xkb)) {
            return layout;
        }
    }
    // 2. GNOME stores the active input sources in dconf (works on Wayland,
    //    where setxkbmap can't see the compositor's state).
    for key in ["mru-sources", "sources"] {
        if let Some(out) = run_capture(
            "gsettings",
            &["get", "org.gnome.desktop.input-sources", key],
        ) {
            if let Some(token) = parse_gsettings_sources(&out) {
                if let Some(layout) = layout_from_token(&token) {
                    return layout;
                }
            }
        }
    }
    // 3. X11 sessions.
    if let Some(out) = run_capture("setxkbmap", &["-query"]) {
        if let Some(token) = parse_kv_output(&out, "layout:") {
            if let Some(layout) = layout_from_token(first_layout(&token)) {
                return layout;
            }
        }
    }
    // 4. systemd-localed knows the configured console/X11 keymap.
    if let Some(out) = run_capture("localectl", &["status"]) {
        for key in ["X11 Layout:", "VC Keymap:"] {
            if let Some(token) = parse_kv_output(&out, key) {
                if let Some(layout) = layout_from_token(first_layout(&token)) {
                    return layout;
                }
            }
        }
    }
    // 5. Weak hint: a pt_BR locale most likely means an ABNT2 keyboard.
    if let Some(layout) = locale_hint() {
        warn!(
            "keyboard layout: guessing {} from the locale; set --layout us if that's wrong",
            layout.name()
        );
        return layout;
    }
    KeyboardLayout::Us
}

/// Run a command and return stdout on success; `None` when the binary is
/// missing or exits non-zero (all detection sources are best-effort).
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first group of a comma-separated XKB layout list (`"br,us"` → `"br"`).
fn first_layout(list: &str) -> &str {
    list.split(',').next().unwrap_or(list).trim()
}

/// Map an XKB layout token to a supported layout. Unknown layouts (de, fr, …)
/// return `None`: detection keeps looking and ultimately falls back to US,
/// which matches the pre-detection behaviour for those users.
fn layout_from_token(token: &str) -> Option<KeyboardLayout> {
    let token = token.trim().trim_matches('\'').to_ascii_lowercase();
    match token.as_str() {
        "us" | "en" => Some(KeyboardLayout::Us),
        t if t == "br" || t.starts_with("br-") => Some(KeyboardLayout::Abnt2),
        _ => None,
    }
}

/// Extract the first xkb source from a gsettings input-sources value, e.g.
/// `[('xkb', 'br'), ('xkb', 'us')]` → `br`. `@a(ss) []` (empty) yields `None`.
fn parse_gsettings_sources(value: &str) -> Option<String> {
    let (_, after) = value.split_once("('xkb', '")?;
    let (token, _) = after.split_once('\'')?;
    // A source can carry a variant ("br+nodeadkeys") — the layout is the part
    // before the '+'.
    Some(token.split('+').next().unwrap_or(token).to_string())
}

/// Find `key` at the start of a line and return the rest of that line, e.g.
/// setxkbmap's `layout:     br` or localectl's `X11 Layout: br`.
fn parse_kv_output(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)
            .map(|rest| rest.trim().to_string())
    })
}

/// `pt_BR` locale → ABNT2, checked across the usual locale variables.
fn locale_hint() -> Option<KeyboardLayout> {
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if value.to_ascii_lowercase().starts_with("pt_br") {
                return Some(KeyboardLayout::Abnt2);
            }
        }
    }
    None
}

// ── Char → keycode maps ───────────────────────────────────────────────────────

/// Map a character to the key (and whether Shift is held) that produces it on
/// `layout`, or `None` if the layout can't type it in one keystroke (those
/// characters ride the clipboard fallback).
pub fn char_to_key(layout: KeyboardLayout, c: char) -> Option<(KeyCode, bool)> {
    match layout {
        KeyboardLayout::Us => char_to_key_us(c),
        KeyboardLayout::Abnt2 => char_to_key_abnt2(c),
    }
}

fn letter_key(c: char) -> Option<KeyCode> {
    use KeyCode as K;
    Some(match c.to_ascii_uppercase() {
        'A' => K::KEY_A,
        'B' => K::KEY_B,
        'C' => K::KEY_C,
        'D' => K::KEY_D,
        'E' => K::KEY_E,
        'F' => K::KEY_F,
        'G' => K::KEY_G,
        'H' => K::KEY_H,
        'I' => K::KEY_I,
        'J' => K::KEY_J,
        'K' => K::KEY_K,
        'L' => K::KEY_L,
        'M' => K::KEY_M,
        'N' => K::KEY_N,
        'O' => K::KEY_O,
        'P' => K::KEY_P,
        'Q' => K::KEY_Q,
        'R' => K::KEY_R,
        'S' => K::KEY_S,
        'T' => K::KEY_T,
        'U' => K::KEY_U,
        'V' => K::KEY_V,
        'W' => K::KEY_W,
        'X' => K::KEY_X,
        'Y' => K::KEY_Y,
        'Z' => K::KEY_Z,
        _ => return None,
    })
}

/// Keys every supported layout shares: letters (Shift = uppercase), digits,
/// whitespace, and the digit-row symbols that sit in the same place on US and
/// ABNT2 (all except `^`, which is a dead key on ABNT2).
fn char_to_key_common(c: char) -> Option<(KeyCode, bool)> {
    use KeyCode as K;
    if c.is_ascii_lowercase() {
        return letter_key(c).map(|k| (k, false));
    }
    if c.is_ascii_uppercase() {
        return letter_key(c).map(|k| (k, true));
    }
    Some(match c {
        ' ' => (K::KEY_SPACE, false),
        '\n' => (K::KEY_ENTER, false),
        '\t' => (K::KEY_TAB, false),
        '1' => (K::KEY_1, false),
        '2' => (K::KEY_2, false),
        '3' => (K::KEY_3, false),
        '4' => (K::KEY_4, false),
        '5' => (K::KEY_5, false),
        '6' => (K::KEY_6, false),
        '7' => (K::KEY_7, false),
        '8' => (K::KEY_8, false),
        '9' => (K::KEY_9, false),
        '0' => (K::KEY_0, false),
        '!' => (K::KEY_1, true),
        '@' => (K::KEY_2, true),
        '#' => (K::KEY_3, true),
        '$' => (K::KEY_4, true),
        '%' => (K::KEY_5, true),
        '&' => (K::KEY_7, true),
        '*' => (K::KEY_8, true),
        '(' => (K::KEY_9, true),
        ')' => (K::KEY_0, true),
        '-' => (K::KEY_MINUS, false),
        '_' => (K::KEY_MINUS, true),
        '=' => (K::KEY_EQUAL, false),
        '+' => (K::KEY_EQUAL, true),
        ',' => (K::KEY_COMMA, false),
        '<' => (K::KEY_COMMA, true),
        '.' => (K::KEY_DOT, false),
        '>' => (K::KEY_DOT, true),
        _ => return None,
    })
}

fn char_to_key_us(c: char) -> Option<(KeyCode, bool)> {
    use KeyCode as K;
    if let Some(hit) = char_to_key_common(c) {
        return Some(hit);
    }
    Some(match c {
        '^' => (K::KEY_6, true),
        '[' => (K::KEY_LEFTBRACE, false),
        '{' => (K::KEY_LEFTBRACE, true),
        ']' => (K::KEY_RIGHTBRACE, false),
        '}' => (K::KEY_RIGHTBRACE, true),
        '\\' => (K::KEY_BACKSLASH, false),
        '|' => (K::KEY_BACKSLASH, true),
        ';' => (K::KEY_SEMICOLON, false),
        ':' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_APOSTROPHE, false),
        '"' => (K::KEY_APOSTROPHE, true),
        '`' => (K::KEY_GRAVE, false),
        '~' => (K::KEY_GRAVE, true),
        '/' => (K::KEY_SLASH, false),
        '?' => (K::KEY_SLASH, true),
        _ => return None,
    })
}

/// ABNT2: `ç` is a real key, `/?` live on the dedicated ABNT key next to the
/// right Shift (`KEY_RO`), the bracket/quote punctuation moves, and the accent
/// characters themselves (´ ` ~ ^ ¨) are dead keys — not typeable in one
/// stroke, so they fall back to the clipboard.
fn char_to_key_abnt2(c: char) -> Option<(KeyCode, bool)> {
    use KeyCode as K;
    if let Some(hit) = char_to_key_common(c) {
        return Some(hit);
    }
    Some(match c {
        'ç' => (K::KEY_SEMICOLON, false),
        'Ç' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_GRAVE, false),
        '"' => (K::KEY_GRAVE, true),
        '[' => (K::KEY_RIGHTBRACE, false),
        '{' => (K::KEY_RIGHTBRACE, true),
        ']' => (K::KEY_BACKSLASH, false),
        '}' => (K::KEY_BACKSLASH, true),
        ';' => (K::KEY_SLASH, false),
        ':' => (K::KEY_SLASH, true),
        '/' => (K::KEY_RO, false),
        '?' => (K::KEY_RO, true),
        '\\' => (K::KEY_102ND, false),
        '|' => (K::KEY_102ND, true),
        _ => return None,
    })
}

/// Every key `layout`'s map can emit, so the virtual uinput device can
/// advertise them up front. Derived by probing the map over printable ASCII,
/// whitespace, and the layout's extra characters (ç/Ç on ABNT2).
pub fn typing_keycodes(layout: KeyboardLayout) -> Vec<KeyCode> {
    let mut keys: Vec<KeyCode> = vec![KeyCode::KEY_LEFTSHIFT];
    let mut push = |k: KeyCode| {
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    let probes = (0x20u8..=0x7e)
        .map(|b| b as char)
        .chain(['\n', '\t', 'ç', 'Ç']);
    for c in probes {
        if let Some((k, _)) = char_to_key(layout, c) {
            push(k);
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abnt2_types_cedilla_directly_and_relocated_punctuation() {
        let l = KeyboardLayout::Abnt2;
        assert_eq!(char_to_key(l, 'ç'), Some((KeyCode::KEY_SEMICOLON, false)));
        assert_eq!(char_to_key(l, 'Ç'), Some((KeyCode::KEY_SEMICOLON, true)));
        assert_eq!(char_to_key(l, '?'), Some((KeyCode::KEY_RO, true)));
        assert_eq!(char_to_key(l, '/'), Some((KeyCode::KEY_RO, false)));
        assert_eq!(char_to_key(l, ':'), Some((KeyCode::KEY_SLASH, true)));
        assert_eq!(char_to_key(l, '"'), Some((KeyCode::KEY_GRAVE, true)));
        assert_eq!(char_to_key(l, '|'), Some((KeyCode::KEY_102ND, true)));
    }

    #[test]
    fn abnt2_dead_keys_fall_back_to_clipboard() {
        let l = KeyboardLayout::Abnt2;
        for dead in ['`', '~', '^', '´', '¨'] {
            assert_eq!(
                char_to_key(l, dead),
                None,
                "{dead:?} should not be typeable"
            );
        }
        // Accented vowels need a dead-key sequence → clipboard.
        assert_eq!(char_to_key(l, 'ã'), None);
        assert_eq!(char_to_key(l, 'é'), None);
    }

    #[test]
    fn us_map_matches_the_historical_behaviour() {
        let l = KeyboardLayout::Us;
        assert_eq!(char_to_key(l, 'a'), Some((KeyCode::KEY_A, false)));
        assert_eq!(char_to_key(l, 'A'), Some((KeyCode::KEY_A, true)));
        assert_eq!(char_to_key(l, '?'), Some((KeyCode::KEY_SLASH, true)));
        assert_eq!(char_to_key(l, '"'), Some((KeyCode::KEY_APOSTROPHE, true)));
        assert_eq!(char_to_key(l, '^'), Some((KeyCode::KEY_6, true)));
        assert_eq!(char_to_key(l, 'ç'), None);
    }

    #[test]
    fn every_ascii_printable_is_typeable_on_us() {
        for b in 0x20u8..=0x7e {
            assert!(
                char_to_key(KeyboardLayout::Us, b as char).is_some(),
                "{:?} should be typeable on US",
                b as char
            );
        }
    }

    #[test]
    fn typing_keycodes_include_layout_specific_keys() {
        let abnt2 = typing_keycodes(KeyboardLayout::Abnt2);
        assert!(abnt2.contains(&KeyCode::KEY_RO));
        assert!(abnt2.contains(&KeyCode::KEY_102ND));
        let us = typing_keycodes(KeyboardLayout::Us);
        assert!(us.contains(&KeyCode::KEY_APOSTROPHE));
        assert!(!us.contains(&KeyCode::KEY_RO));
    }

    #[test]
    fn xkb_tokens_classify_br_and_us_only() {
        assert_eq!(layout_from_token("br"), Some(KeyboardLayout::Abnt2));
        assert_eq!(layout_from_token("br-abnt2"), Some(KeyboardLayout::Abnt2));
        assert_eq!(layout_from_token("us"), Some(KeyboardLayout::Us));
        assert_eq!(layout_from_token("'us'"), Some(KeyboardLayout::Us));
        // Unknown layouts keep detection searching (and default to US).
        assert_eq!(layout_from_token("de"), None);
        assert_eq!(layout_from_token(""), None);
    }

    #[test]
    fn detection_source_outputs_parse() {
        assert_eq!(first_layout("br,us"), "br");
        assert_eq!(
            parse_gsettings_sources("[('xkb', 'br'), ('xkb', 'us')]").as_deref(),
            Some("br")
        );
        assert_eq!(
            parse_gsettings_sources("[('xkb', 'br+nodeadkeys')]").as_deref(),
            Some("br")
        );
        assert_eq!(parse_gsettings_sources("@a(ss) []"), None);
        let setxkbmap = "rules:      evdev\nmodel:      pc105\nlayout:     br,us\n";
        assert_eq!(
            parse_kv_output(setxkbmap, "layout:").as_deref(),
            Some("br,us")
        );
        let localectl = "   System Locale: LANG=pt_BR.UTF-8\n       VC Keymap: br-abnt2\n      X11 Layout: br\n";
        assert_eq!(
            parse_kv_output(localectl, "X11 Layout:").as_deref(),
            Some("br")
        );
        assert_eq!(
            parse_kv_output(localectl, "VC Keymap:").as_deref(),
            Some("br-abnt2")
        );
    }
}
