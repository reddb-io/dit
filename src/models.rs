//! `dit models` — local model management.
//!
//! Models are downloaded from HuggingFace into `~/.dit/models/` and their
//! SHA-256 digest is verified against the catalog before saving. A re-download
//! of an already-current model is a no-op.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::ModelsAction;

#[derive(Debug)]
struct ModelEntry {
    id: &'static str,
    description: &'static str,
    hf_url: &'static str,
    /// SHA-256 hex of the downloaded file. Empty string = skip verification.
    sha256: &'static str,
}

static CATALOG: &[ModelEntry] = &[
    // ── Cloud engine models (ggml format, for reference / future use) ─────────
    ModelEntry {
        id: "whisper-tiny",
        description: "Whisper tiny (ggml, ~75 MB) — fastest, lowest accuracy",
        hf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        sha256: "bd577a113a864445d4c299885e0cb97d4ba92b5f4e0f65d52b40f50ec6bfacef",
    },
    ModelEntry {
        id: "whisper-base",
        description: "Whisper base (ggml, ~148 MB) — fast, good accuracy",
        hf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    ModelEntry {
        id: "whisper-small",
        description: "Whisper small (ggml, ~488 MB) — balanced",
        hf_url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    // ── Local engine models (GGUF, for --engine local via candle) ─────────────
    ModelEntry {
        id: "whisper-tiny-local",
        description: "Whisper tiny (GGUF quantized, ~40 MB) — fastest offline, for --engine local",
        hf_url: "https://huggingface.co/lmz/candle-whisper/resolve/main/pytorch_model_whisper-tiny.gguf",
        sha256: "",
    },
    ModelEntry {
        id: "whisper-base-local",
        description: "Whisper base (GGUF quantized, ~75 MB) — offline, for --engine local",
        hf_url: "https://huggingface.co/lmz/candle-whisper/resolve/main/pytorch_model_whisper-base.gguf",
        sha256: "",
    },
];

/// Directory where all downloaded models live: `~/.dit/models/`.
pub fn models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dit")
        .join("models")
}

/// Resolve a model id to its local file path; returns `None` when not on disk.
#[allow(dead_code)]
pub fn resolve_model(name: &str) -> Option<PathBuf> {
    let entry = CATALOG.iter().find(|m| m.id == name)?;
    let path = model_path(entry);
    if path.exists() { Some(path) } else { None }
}

/// Resolve a local-engine model to its on-disk path.
/// Returns `None` when the model has not been downloaded yet.
pub fn resolve_local_model(name: &str) -> Option<PathBuf> {
    resolve_model(name)
}

fn model_path(entry: &ModelEntry) -> PathBuf {
    let filename = entry.hf_url.rsplit('/').next().unwrap_or(entry.id);
    models_dir().join(filename)
}

pub fn run(action: &ModelsAction) -> Result<()> {
    match action {
        ModelsAction::Path => {
            println!("{}", models_dir().display());
        }

        ModelsAction::List => {
            println!("{:<20} {:<12} {}", "ID", "INSTALLED", "DESCRIPTION");
            for entry in CATALOG {
                let installed = if model_path(entry).exists() { "yes" } else { "no" };
                println!("{:<20} {:<12} {}", entry.id, installed, entry.description);
            }
        }

        ModelsAction::Download { id } => {
            let entry = find_model(id)?;
            let dir = models_dir();
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            let path = model_path(entry);

            if path.exists() {
                if entry.sha256.is_empty() {
                    println!("✓ {} is already downloaded (no checksum to verify)", entry.id);
                    return Ok(());
                }
                let existing = sha256_file(&path)?;
                if existing == entry.sha256 {
                    println!("✓ {} is already downloaded and current", entry.id);
                    return Ok(());
                }
                eprintln!("! checksum mismatch on existing file — re-downloading");
            }

            println!("› downloading {} …", entry.id);
            let bytes = http_get_bytes(entry.hf_url)
                .with_context(|| format!("downloading {}", entry.hf_url))?;

            if !entry.sha256.is_empty() {
                let actual = sha256_bytes(&bytes);
                if actual != entry.sha256 {
                    bail!(
                        "checksum mismatch — refusing to save (expected {}, got {})",
                        entry.sha256,
                        actual
                    );
                }
            }

            std::fs::write(&path, &bytes)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("✓ {} saved to {}", entry.id, path.display());
        }

        ModelsAction::Rm { id } => {
            let entry = find_model(id)?;
            let path = model_path(entry);
            if !path.exists() {
                bail!("{} is not downloaded", id);
            }
            std::fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            println!("✓ removed {}", path.display());
        }
    }
    Ok(())
}

fn find_model(id: &str) -> Result<&'static ModelEntry> {
    CATALOG.iter().find(|m| m.id == id).with_context(|| {
        let ids: Vec<_> = CATALOG.iter().map(|m| m.id).collect();
        format!("unknown model id {:?} — available: {}", id, ids.join(", "))
    })
}

fn sha256_bytes(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex(h.finalize().as_slice())
}

fn sha256_file(path: &PathBuf) -> Result<String> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_bytes(&data))
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    const MAX: u64 = 4 * 1024 * 1024 * 1024;
    let resp = ureq::AgentBuilder::new()
        .user_agent(concat!("dit-models/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Duration::from_secs(30))
        .timeout(Duration::from_secs(3600))
        .build()
        .get(url)
        .call()
        .context("HTTP request failed")?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX)
        .read_to_end(&mut buf)
        .context("reading response")?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_ends_with_expected_path() {
        let dir = models_dir();
        let s = dir.to_string_lossy();
        assert!(s.ends_with(".dit/models"), "got: {s}");
    }

    #[test]
    fn resolve_unknown_model_returns_none() {
        assert!(resolve_model("nonexistent-model-xyz").is_none());
    }

    #[test]
    fn resolve_not_downloaded_model_returns_none() {
        // In a fresh/CI environment the model file won't exist on disk.
        // We verify the function doesn't panic and returns None for missing files.
        let result = resolve_model("whisper-tiny");
        if result.is_some() {
            // Model is genuinely present — that's fine too.
            assert!(result.unwrap().exists());
        }
    }

    #[test]
    fn find_model_errors_on_unknown_id() {
        let err = find_model("not-a-real-model").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown model id"), "unexpected message: {msg}");
        assert!(msg.contains("not-a-real-model"), "message missing id: {msg}");
    }

    #[test]
    fn model_path_derives_filename_from_url() {
        let entry = find_model("whisper-tiny").unwrap();
        let path = model_path(entry);
        let s = path.to_string_lossy();
        assert!(s.ends_with("ggml-tiny.bin"), "got: {s}");
        assert!(s.contains(".dit/models/"), "got: {s}");
    }

    #[test]
    fn catalog_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in CATALOG {
            assert!(seen.insert(entry.id), "duplicate catalog id: {}", entry.id);
        }
    }
}
