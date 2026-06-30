//! `dit doctor` — lightweight diagnostics for the things that commonly make
//! desktop dictation look stuck: session env, Linux input permissions, uinput,
//! microphones, and API-key presence.

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};

pub fn run(prefer_device: Option<String>) -> Result<()> {
    println!("dit doctor\n==========");
    check_api_key();
    check_local_engine();
    check_session_env();
    check_linux_input();
    check_audio(prefer_device)?;
    Ok(())
}

fn check_local_engine() {
    // Report local engine readiness: whether the default offline model is on disk.
    use crate::config::DEFAULT_LOCAL_MODEL;
    use crate::models::resolve_local_model;
    let present = resolve_local_model(DEFAULT_LOCAL_MODEL).is_some();
    status(
        present,
        "local engine model",
        if present {
            "whisper-tiny-local found in ~/.dit/models/"
        } else {
            "not installed — run `dit models download whisper-tiny-local` to enable --engine local"
        },
    );
}

fn check_api_key() {
    let present = std::env::var_os("ELEVENLABS_API_KEY").is_some()
        || dirs::home_dir()
            .map(|h| h.join(".dit.env").exists())
            .unwrap_or(false);
    status(
        present,
        "ElevenLabs API key",
        "ELEVENLABS_API_KEY env or ~/.dit.env found",
    );
}

fn check_session_env() {
    let graphical =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    status(
        graphical,
        "graphical session",
        "WAYLAND_DISPLAY or DISPLAY is set",
    );
    status(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some(),
        "D-Bus session",
        "DBUS_SESSION_BUS_ADDRESS is set",
    );
    status(
        std::env::var_os("XDG_RUNTIME_DIR").is_some(),
        "runtime dir",
        "XDG_RUNTIME_DIR is set",
    );
}

#[cfg(target_os = "linux")]
fn check_linux_input() {
    use std::os::unix::fs::PermissionsExt;

    status(
        std::path::Path::new("/dev/input").exists(),
        "/dev/input",
        "input devices directory exists",
    );
    let in_input_group = std::process::Command::new("id")
        .arg("-nG")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|g| g == "input")
        })
        .unwrap_or(false);
    status(
        in_input_group,
        "input group",
        "current user is in group 'input'",
    );

    let readable_events = std::fs::read_dir("/dev/input")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .filter(|e| std::fs::File::open(e.path()).is_ok())
        .count();
    status(
        readable_events > 0,
        "keyboard event access",
        &format!("{readable_events} readable /dev/input/event* node(s)"),
    );

    let uinput_ok = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .is_ok();
    let uinput_mode = std::fs::metadata("/dev/uinput")
        .map(|m| format!("mode {:o}", m.permissions().mode() & 0o777))
        .unwrap_or_else(|_| "missing".to_string());
    status(uinput_ok, "/dev/uinput write", &uinput_mode);
}

#[cfg(not(target_os = "linux"))]
fn check_linux_input() {}

fn check_audio(prefer_device: Option<String>) -> Result<()> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());
    println!("\nMicrophones:");
    let mut count = 0usize;
    for device in host.input_devices()? {
        count += 1;
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let marker = if Some(name.as_str()) == default.as_deref() {
            " default"
        } else {
            ""
        };
        let preferred = prefer_device
            .as_ref()
            .is_some_and(|needle| name.to_lowercase().contains(&needle.to_lowercase()));
        let preferred = if preferred { " preferred" } else { "" };
        match device.default_input_config() {
            Ok(cfg) => println!(
                "  ✓ {name}{marker}{preferred} — {} Hz, {} ch, {:?}",
                cfg.sample_rate().0,
                cfg.channels(),
                cfg.sample_format()
            ),
            Err(e) => println!("  ! {name}{marker}{preferred} — no default input config: {e}"),
        }
    }
    status(
        count > 0,
        "input devices",
        &format!("{count} input device(s) enumerated"),
    );
    Ok(())
}

fn status(ok: bool, label: &str, detail: &str) {
    let icon = if ok { "✓" } else { "!" };
    println!("{icon} {label}: {detail}");
}
