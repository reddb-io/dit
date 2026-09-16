//! `dit gnome-extension` — install, remove and inspect the GNOME Shell focus
//! bridge (`extras/gnome-shell/dit-focus@reddb.io`).
//!
//! The extension files are compiled into the binary, so the installed bridge
//! always matches the dit that queries it and `dit update` users can install
//! it without the repository. Files go to the per-user extension directory;
//! enabling goes through `gnome-extensions enable`, or — when GNOME Shell has
//! not scanned the new directory yet, which on Wayland only happens at the
//! next login — by adding the uuid to `org.gnome.shell enabled-extensions`.

use anyhow::{Context, Result};

use crate::config::GnomeExtensionAction;

pub const UUID: &str = "dit-focus@reddb.io";
const METADATA: &str = include_str!("../extras/gnome-shell/dit-focus@reddb.io/metadata.json");
const EXTENSION_JS: &str = include_str!("../extras/gnome-shell/dit-focus@reddb.io/extension.js");

pub fn run(action: &GnomeExtensionAction) -> Result<()> {
    match action {
        GnomeExtensionAction::Install => install(),
        GnomeExtensionAction::Uninstall => uninstall(),
        GnomeExtensionAction::Status => status(),
    }
}

fn extension_dir() -> Result<std::path::PathBuf> {
    let data = dirs::data_dir().context("no XDG data dir")?;
    Ok(data.join("gnome-shell/extensions").join(UUID))
}

/// Parse a `gsettings get` string array (`['a', 'b']` or `@as []`).
fn parse_strv(raw: &str) -> Vec<String> {
    let raw = raw.trim().trim_start_matches("@as").trim();
    let inner = raw.trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Render a string array for `gsettings set`.
fn format_strv(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

fn gsettings_list(key: &str) -> Option<Vec<String>> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.shell", key])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_strv(&String::from_utf8_lossy(&out.stdout)))
}

fn gsettings_set_list(key: &str, items: &[String]) -> bool {
    std::process::Command::new("gsettings")
        .args(["set", "org.gnome.shell", key, &format_strv(items)])
        .status()
        .is_ok_and(|s| s.success())
}

fn gnome_extensions(args: &[&str]) -> bool {
    std::process::Command::new("gnome-extensions")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn install() -> Result<()> {
    let dir = extension_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("metadata.json"), METADATA)?;
    std::fs::write(dir.join("extension.js"), EXTENSION_JS)?;
    println!("✓ installed the dit focus bridge → {}", dir.display());

    let enabled = if gnome_extensions(&["enable", UUID]) {
        true
    } else if let Some(mut list) = gsettings_list("enabled-extensions") {
        // GNOME Shell hasn't seen the new directory yet; enable it for the
        // next login directly in the settings the shell reads at startup.
        if !list.iter().any(|u| u == UUID) {
            list.push(UUID.to_string());
        }
        gsettings_set_list("enabled-extensions", &list)
    } else {
        false
    };
    if enabled {
        println!("✓ enabled {UUID}");
    } else {
        println!("! could not enable {UUID}; run: gnome-extensions enable {UUID}");
    }
    if let Some(disabled) = gsettings_list("disabled-extensions") {
        if disabled.iter().any(|u| u == UUID) {
            let rest: Vec<String> = disabled.into_iter().filter(|u| u != UUID).collect();
            gsettings_set_list("disabled-extensions", &rest);
        }
    }
    let user_extensions_off = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.shell", "disable-user-extensions"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true");
    if user_extensions_off {
        println!(
            "! user extensions are switched off (org.gnome.shell disable-user-extensions = true);\n  \
             turn them on in the Extensions app for the bridge to load"
        );
    }
    println!(
        "  On Wayland, GNOME Shell loads new extensions only at login: log out and back in,\n  \
         then check with `dit gnome-extension status`."
    );
    Ok(())
}

fn uninstall() -> Result<()> {
    let _ = gnome_extensions(&["disable", UUID]);
    if let Some(list) = gsettings_list("enabled-extensions") {
        if list.iter().any(|u| u == UUID) {
            let rest: Vec<String> = list.into_iter().filter(|u| u != UUID).collect();
            gsettings_set_list("enabled-extensions", &rest);
        }
    }
    let dir = extension_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        println!("✓ removed {}", dir.display());
    } else {
        println!("✓ {UUID} was not installed");
    }
    println!(
        "  dit falls back to its clipboard/typing delivery; log out and back in to unload it."
    );
    Ok(())
}

fn status() -> Result<()> {
    let dir = extension_dir()?;
    let installed = dir.join("metadata.json").exists();
    let enabled = gsettings_list("enabled-extensions").is_some_and(|l| l.iter().any(|u| u == UUID));
    println!(
        "{} files: {}",
        if installed { "✓" } else { "!" },
        if installed {
            dir.display().to_string()
        } else {
            "not installed — run `dit gnome-extension install`".into()
        }
    );
    println!(
        "{} enabled: {}",
        if enabled { "✓" } else { "!" },
        if enabled { "yes" } else { "no" }
    );
    match crate::focus::FocusDetector::new().detect() {
        Some(app) if app.source == "gnome-shell" => println!(
            "✓ running: focused app {:?} (wm_class {:?}, pid {})",
            app.app_id,
            app.wm_class,
            app.pid.map_or("unknown".into(), |p| p.to_string())
        ),
        _ => println!(
            "! running: {} is not answering on the session bus (log out and back in after installing)",
            crate::focus::GNOME_BUS_NAME
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gsettings_arrays_round_trip() {
        assert_eq!(
            parse_strv("['a@b', 'dit-focus@reddb.io']\n"),
            vec!["a@b", "dit-focus@reddb.io"]
        );
        assert!(parse_strv("@as []").is_empty());
        assert_eq!(
            format_strv(&["x@y".to_string(), UUID.to_string()]),
            "['x@y', 'dit-focus@reddb.io']"
        );
    }

    #[test]
    fn bundled_metadata_matches_the_uuid() {
        let meta: serde_json::Value = serde_json::from_str(METADATA).expect("valid JSON");
        assert_eq!(meta["uuid"], UUID);
        let versions = meta["shell-version"].as_array().expect("shell-version");
        assert!(versions.iter().any(|v| v == "46"));
        assert!(EXTENSION_JS.contains("io.reddb.dit.Focus"));
    }
}
