//! `dit update` — built-in, self-contained self-upgrade.
//!
//! No `curl | bash`, no external installer: this resolves the latest release,
//! downloads the asset that matches *this* host (the right arch, and the right
//! glibc vs. fully-static musl variant — inferred from how this very binary was
//! built), verifies its published **SHA-256**, and atomically swaps the running
//! executable in place. On Linux it then restarts an active `dit.service` so the
//! desktop agent picks up the new binary.
//!
//! It is idempotent: when you are already on the target version it changes
//! nothing and says so. `--check` reports availability without downloading.

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

const REPO: &str = "reddb-io/dit";
const UA: &str = concat!("dit-update/", env!("CARGO_PKG_VERSION"));
/// Hard ceiling on a downloaded asset, so a bad URL can't exhaust memory.
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

/// Parsed flags for `dit update`.
pub struct UpdateArgs {
    /// Only report whether a newer release exists; install nothing.
    pub check: bool,
    /// Reinstall even when the target version is already present.
    pub force: bool,
    /// Pin a specific release tag (e.g. v0.2.4) instead of the latest.
    pub version: Option<String>,
}

pub fn run(args: &UpdateArgs) -> Result<()> {
    let current = normalize(env!("CARGO_PKG_VERSION"));

    let target_tag = match &args.version {
        Some(v) => v.clone(),
        None => latest_tag().context("could not reach GitHub to find the latest release")?,
    };
    let target = normalize(&target_tag);

    let up_to_date = current == target;

    if args.check {
        if up_to_date {
            println!("✓ dit {current} is already the latest release — nothing to update");
        } else {
            println!("› update available: {current} → {target}");
            println!("  run `dit update` to install it");
        }
        return Ok(());
    }

    if up_to_date && !args.force {
        println!("✓ dit {current} is already the latest release — nothing to update");
        return Ok(());
    }

    let asset = asset_name()?;
    if up_to_date {
        println!("› reinstalling dit {current} ({asset}, --force)");
    } else {
        println!("› updating dit {current} → {target} ({asset})");
    }

    let base = format!("https://github.com/{REPO}/releases/download/{target_tag}");

    let bytes = http_get_bytes(&format!("{base}/{asset}")).with_context(|| {
        format!("could not download {asset} for {target_tag} — this platform may not have a prebuilt binary")
    })?;
    verify_checksum(&bytes, &format!("{base}/{asset}.sha256"))?;

    let exe = std::env::current_exe().context("cannot locate the running dit executable")?;
    install_over_self(&exe, &bytes)?;
    println!("✓ installed dit {target} → {}", exe.display());

    restart_service();
    Ok(())
}

/// The release asset matching this host. The glibc-vs-static choice is taken
/// from this binary's own build target (`target_env`): a musl binary upgrades
/// to the `-static` asset, a gnu binary to the glibc-portable one.
fn asset_name() -> Result<String> {
    let ext = if cfg!(windows) { ".exe" } else { "" };

    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        bail!("unsupported operating system for self-update");
    };

    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" => "armv7",
        other => bail!("unsupported architecture for self-update: {other}"),
    };

    let variant = if cfg!(all(target_os = "linux", target_env = "musl")) {
        "-static"
    } else {
        ""
    };

    Ok(format!("dit-{os}-{arch}{variant}{ext}"))
}

/// Write the new bytes next to the current executable and atomically replace it.
/// `self_replace` handles the platform quirks (you cannot unlink a running exe on
/// Windows; on Unix the rename is atomic and the running process keeps its inode).
fn install_over_self(exe: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = exe
        .parent()
        .context("dit executable has no parent directory")?;
    // Keep the temp file on the *same* filesystem as the target so the swap is a
    // rename, never a cross-device copy.
    let tmp = dir.join(format!(".dit-update-{}.tmp", std::process::id()));

    let res = (|| -> Result<()> {
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                .context("setting executable permissions")?;
        }
        self_replace::self_replace(&tmp).context("replacing the running executable")?;
        Ok(())
    })();

    let _ = std::fs::remove_file(&tmp);
    res
}

/// Resolve the `tag_name` of the latest published release.
fn latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = agent()
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub API request failed")?
        .into_string()
        .context("reading GitHub API response")?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parsing GitHub API response")?;
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .context("no published release found")
}

/// Download the `.sha256` sidecar and compare it to the asset's digest. A
/// missing sidecar is a warning, not a hard failure (mirrors the install script).
fn verify_checksum(bytes: &[u8], sha_url: &str) -> Result<()> {
    let sums = match http_get_string(sha_url) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("! no checksum published; skipping verification");
            return Ok(());
        }
    };
    let expected = sums
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if expected.is_empty() {
        eprintln!("! no checksum published; skipping verification");
        return Ok(());
    }

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex(hasher.finalize().as_slice());

    if expected != actual {
        bail!("checksum mismatch — refusing to install (expected {expected}, got {actual})");
    }
    println!("✓ checksum verified");
    Ok(())
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = agent().get(url).call().context("download request failed")?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_ASSET_BYTES)
        .read_to_end(&mut buf)
        .context("reading the downloaded asset")?;
    Ok(buf)
}

fn http_get_string(url: &str) -> Result<String> {
    Ok(agent().get(url).call()?.into_string()?)
}

/// A shared ureq agent: a User-Agent (GitHub requires one) and sane timeouts.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(UA)
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
}

/// Restart a running, dit-owned user service so it picks up the new binary.
#[cfg(target_os = "linux")]
fn restart_service() {
    use std::process::Command;
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "dit.service"])
        .output();
    let is_active = matches!(active, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).trim() == "active");
    if !is_active {
        return;
    }
    match Command::new("systemctl")
        .args(["--user", "restart", "dit.service"])
        .status()
    {
        Ok(s) if s.success() => println!("✓ restarted dit.service"),
        _ => eprintln!("! could not restart dit.service — restart dit manually"),
    }
}

#[cfg(not(target_os = "linux"))]
fn restart_service() {}

/// Strip a leading `dit ` / `v` so `dit 0.2.4`, `v0.2.4` and `0.2.4` compare equal.
fn normalize(v: &str) -> String {
    v.trim()
        .trim_start_matches("dit ")
        .trim_start_matches('v')
        .trim()
        .to_string()
}

/// Lowercase hex encoding of a digest, without pulling in a `hex` crate.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
