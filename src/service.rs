//! `dit service` — install/remove a per-user autostart service.
//!
//! dit must run inside your **graphical login session** (it needs the display,
//! audio, and synthetic input), so this is a *user agent*, never a system
//! daemon: a root/system service runs isolated from your session and could not
//! read the keyboard or type into your apps.
//!
//! - Linux: systemd `--user` service, falling back to an XDG autostart
//!   `.desktop` entry when there's no user systemd manager.
//! - macOS: a LaunchAgent under `~/Library/LaunchAgents`.
//! - Windows: a logon task via Task Scheduler (not a Windows Service, which
//!   runs in session 0 with no desktop access).

use anyhow::{Context, Result};

use crate::config::ServiceAction;

pub fn run(action: &ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install { args } => install(args),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Status => status(),
    }
}

/// Absolute path to the running dit executable, baked into the autostart entry.
fn exe_path() -> Result<String> {
    let p = std::env::current_exe().context("cannot locate the dit executable")?;
    Ok(p.to_string_lossy().into_owned())
}

/// Run a command and fail loudly on a non-zero exit.
#[allow(dead_code)]
fn run_ok(cmd: &mut std::process::Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    anyhow::ensure!(status.success(), "command {:?} failed: {status}", cmd);
    Ok(())
}

// ── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|c| c.join("systemd/user/dit.service"))
}

#[cfg(target_os = "linux")]
fn autostart_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|c| c.join("autostart/dit.desktop"))
}

/// A usable `systemctl --user` manager is present in this session.
#[cfg(target_os = "linux")]
fn has_systemd_user() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn install(args: &[String]) -> Result<()> {
    use std::process::Command;

    let exec = exec_line(args)?;

    if has_systemd_user() {
        let path = systemd_unit_path().context("no XDG config dir")?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Unit]\n\
                 Description=dit — voice dictation\n\
                 After=graphical-session.target\n\
                 PartOf=graphical-session.target\n\n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart={exec}\n\
                 Restart=on-failure\n\
                 RestartSec=2\n\n\
                 [Install]\n\
                 WantedBy=graphical-session.target\n"
            ),
        )?;
        run_ok(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        // Best-effort: make the session's display/audio env visible to the unit.
        let _ = Command::new("systemctl")
            .args([
                "--user",
                "import-environment",
                "DISPLAY",
                "XAUTHORITY",
                "WAYLAND_DISPLAY",
                "XDG_RUNTIME_DIR",
            ])
            .status();
        run_ok(Command::new("systemctl").args(["--user", "enable", "--now", "dit.service"]))?;
        println!("✓ installed systemd user service (dit.service)");
        println!("  logs:  journalctl --user -u dit -f");
        println!("  stop:  systemctl --user stop dit");
    } else {
        let path = autostart_path().context("no XDG config dir")?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\n\
                 Type=Application\n\
                 Name=dit\n\
                 Comment=Voice dictation\n\
                 Exec={exec}\n\
                 Terminal=false\n\
                 NoDisplay=true\n\
                 X-GNOME-Autostart-enabled=true\n"
            ),
        )?;
        println!("✓ installed XDG autostart entry — starts at next login");
        println!("  ({})", path.display());
        println!("  no systemd --user here; run `dit` directly for this session");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall() -> Result<()> {
    use std::process::Command;
    let mut removed = false;
    if has_systemd_user() {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "dit.service"])
            .status();
    }
    if let Some(p) = systemd_unit_path() {
        if p.exists() {
            std::fs::remove_file(&p)?;
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            removed = true;
        }
    }
    if let Some(p) = autostart_path() {
        if p.exists() {
            std::fs::remove_file(&p)?;
            removed = true;
        }
    }
    println!(
        "{}",
        if removed {
            "✓ removed dit autostart"
        } else {
            "nothing to remove"
        }
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn status() -> Result<()> {
    let mut any = false;
    if let Some(p) = systemd_unit_path() {
        if p.exists() {
            let active = std::process::Command::new("systemctl")
                .args(["--user", "is-active", "dit.service"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            println!("systemd user service: installed ({active})");
            any = true;
        }
    }
    if let Some(p) = autostart_path() {
        if p.exists() {
            println!("XDG autostart: installed ({})", p.display());
            any = true;
        }
    }
    if !any {
        println!("not installed");
    }
    Ok(())
}

// ── macOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents/io.reddb.dit.plist"))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install(args: &[String]) -> Result<()> {
    use std::process::Command;

    let exe = exe_path()?;
    let mut program_args = format!("    <string>{}</string>\n", xml_escape(&exe));
    for a in args {
        program_args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }

    let path = plist_path().context("no home dir")?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        &path,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key><string>io.reddb.dit</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n{program_args}\x20 </array>\n\
             \x20 <key>RunAtLoad</key><true/>\n\
             \x20 <key>KeepAlive</key><true/>\n\
             </dict>\n\
             </plist>\n"
        ),
    )?;

    let _ = Command::new("launchctl").arg("unload").arg(&path).status();
    run_ok(Command::new("launchctl").arg("load").arg("-w").arg(&path))?;
    println!("✓ installed LaunchAgent (io.reddb.dit)");
    println!("  grant Accessibility to dit: System Settings → Privacy & Security → Accessibility");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<()> {
    use std::process::Command;
    let path = plist_path().context("no home dir")?;
    if path.exists() {
        let _ = Command::new("launchctl").arg("unload").arg(&path).status();
        std::fs::remove_file(&path)?;
        println!("✓ removed LaunchAgent");
    } else {
        println!("nothing to remove");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn status() -> Result<()> {
    let path = plist_path().context("no home dir")?;
    if path.exists() {
        let loaded = std::process::Command::new("launchctl")
            .args(["list", "io.reddb.dit"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!(
            "LaunchAgent: installed ({})",
            if loaded { "loaded" } else { "not loaded" }
        );
    } else {
        println!("not installed");
    }
    Ok(())
}

// ── Windows ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
const TASK_NAME: &str = "dit";

#[cfg(target_os = "windows")]
fn install(args: &[String]) -> Result<()> {
    let exe = exe_path()?;
    // schtasks /TR takes one string: "C:\path\dit.exe" --flag value …
    let mut tr = format!("\"{exe}\"");
    for a in args {
        tr.push(' ');
        tr.push_str(a);
    }
    run_ok(std::process::Command::new("schtasks").args([
        "/Create", "/TN", TASK_NAME, "/TR", &tr, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
    ]))?;
    println!("✓ installed logon task ({TASK_NAME}) — dit starts at sign-in");
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall() -> Result<()> {
    let ok = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "{}",
        if ok {
            "✓ removed logon task"
        } else {
            "nothing to remove"
        }
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn status() -> Result<()> {
    let ok = std::process::Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "logon task: {}",
        if ok { "installed" } else { "not installed" }
    );
    Ok(())
}

// ── Linux ExecStart/Exec line ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn exec_line(args: &[String]) -> Result<String> {
    let exe = exe_path()?;
    let mut line = exe;
    for a in args {
        line.push(' ');
        line.push_str(a);
    }
    Ok(line)
}

// ── Fallback for unsupported targets ─────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn install(_args: &[String]) -> Result<()> {
    anyhow::bail!("`dit service` is not supported on this platform")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn uninstall() -> Result<()> {
    anyhow::bail!("`dit service` is not supported on this platform")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn status() -> Result<()> {
    anyhow::bail!("`dit service` is not supported on this platform")
}
