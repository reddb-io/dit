//! `dit models` — local model management.
//!
//! Every catalog entry is a set of files (GGUF weights + the HuggingFace
//! tokenizer.json they were trained with) downloaded into `~/.dit/models/`
//! and verified against pinned SHA-256 digests before saving. A re-download
//! of an already-current model is a no-op.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::ModelsAction;

/// Which Whisper architecture a local model uses. The engine derives the
/// model dimensions from this, so the catalog and the engine can never
/// disagree about what an id means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalModelKind {
    Tiny,
}

/// One downloadable file of a model.
#[derive(Debug)]
struct ModelFile {
    url: &'static str,
    /// SHA-256 hex of the downloaded file. Empty string = skip verification.
    sha256: &'static str,
}

#[derive(Debug)]
struct ModelEntry {
    id: &'static str,
    description: &'static str,
    /// Read by the local engine (`--features local`) to pick model dimensions.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    kind: LocalModelKind,
    files: &'static [ModelFile],
}

/// Models usable by `--engine local` (quantised GGUF for candle). The GGUF
/// holds only tensors, so each entry also carries the matching tokenizer.
static CATALOG: &[ModelEntry] = &[ModelEntry {
    id: "whisper-tiny-local",
    description: "Whisper tiny multilingual (GGUF q8_0, ~40 MB) — offline, for --engine local",
    kind: LocalModelKind::Tiny,
    files: &[
        ModelFile {
            url: "https://huggingface.co/lmz/candle-whisper/resolve/main/model-tiny-q80.gguf",
            sha256: "edcc907db61aef092f1244dc1e53c55056b472be343e9bbb4dc12ebd4740392f",
        },
        ModelFile {
            url: "https://huggingface.co/lmz/candle-whisper/resolve/main/tokenizer-tiny.json",
            sha256: "dfc530298b6fbed1a97c6472c575b026453706e2a204c7f7038f2c9d208b0759",
        },
    ],
}];

/// A local model resolved to its on-disk files, ready for the engine.
#[cfg(feature = "local")]
#[derive(Clone, Debug)]
pub struct LocalModel {
    /// GGUF weights.
    pub path: PathBuf,
    /// HuggingFace tokenizer.json (id → token table).
    pub tokenizer: PathBuf,
    pub kind: LocalModelKind,
}

/// Directory where all downloaded models live: `~/.dit/models/`.
pub fn models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dit")
        .join("models")
}

/// Friendly aliases so `--model tiny` works alongside the full catalog id.
fn canonical_id(name: &str) -> &str {
    match name {
        "tiny" | "whisper-tiny" => "whisper-tiny-local",
        other => other,
    }
}

fn file_path(file: &ModelFile) -> PathBuf {
    let filename = file.url.rsplit('/').next().unwrap_or("model");
    models_dir().join(filename)
}

fn entry_installed(entry: &ModelEntry) -> bool {
    entry.files.iter().all(|f| file_path(f).exists())
}

/// Resolve a model id (or alias) to its on-disk files. Errors distinguish an
/// unknown id from a known-but-not-downloaded model so the user always knows
/// the next step.
#[cfg(feature = "local")]
pub fn resolve_local_model(name: &str) -> Result<LocalModel> {
    let id = canonical_id(name);
    let Some(entry) = CATALOG.iter().find(|m| m.id == id) else {
        let ids: Vec<_> = CATALOG.iter().map(|m| m.id).collect();
        bail!(
            "unknown local model {name:?} — available: {} (see `dit models list`)",
            ids.join(", ")
        );
    };
    if !entry_installed(entry) {
        bail!(
            "local model {} is not downloaded — run `dit models download {}` first",
            entry.id,
            entry.id
        );
    }
    let path = entry
        .files
        .iter()
        .map(file_path)
        .find(|p| p.extension().is_some_and(|e| e == "gguf"))
        .context("catalog entry has no GGUF file")?;
    let tokenizer = entry
        .files
        .iter()
        .map(file_path)
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .context("catalog entry has no tokenizer file")?;
    Ok(LocalModel {
        path,
        tokenizer,
        kind: entry.kind,
    })
}

pub fn run(action: &ModelsAction) -> Result<()> {
    match action {
        ModelsAction::Path => {
            println!("{}", models_dir().display());
        }

        ModelsAction::List => {
            println!("{:<20} {:<12} DESCRIPTION", "ID", "INSTALLED");
            for entry in CATALOG {
                let installed = if entry_installed(entry) { "yes" } else { "no" };
                println!("{:<20} {:<12} {}", entry.id, installed, entry.description);
            }
        }

        ModelsAction::Download { id } => {
            let entry = find_model(id)?;
            let dir = models_dir();
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

            for file in entry.files {
                let path = file_path(file);
                if path.exists() {
                    if file.sha256.is_empty() {
                        println!("✓ {} present (no checksum to verify)", path.display());
                        continue;
                    }
                    if sha256_file(&path)? == file.sha256 {
                        println!("✓ {} is already downloaded and current", path.display());
                        continue;
                    }
                    eprintln!("! checksum mismatch on {} — re-downloading", path.display());
                }

                println!("› downloading {} …", file.url);
                let bytes = http_get_bytes(file.url)
                    .with_context(|| format!("downloading {}", file.url))?;

                if !file.sha256.is_empty() {
                    let actual = sha256_bytes(&bytes);
                    if actual != file.sha256 {
                        bail!(
                            "checksum mismatch — refusing to save (expected {}, got {})",
                            file.sha256,
                            actual
                        );
                    }
                }

                std::fs::write(&path, &bytes)
                    .with_context(|| format!("writing {}", path.display()))?;
                println!("✓ saved {}", path.display());
            }
            println!("✓ {} is ready for `dit --engine local`", entry.id);
        }

        ModelsAction::Rm { id } => {
            let entry = find_model(id)?;
            if !entry_installed(entry) {
                bail!("{} is not downloaded", entry.id);
            }
            for file in entry.files {
                let path = file_path(file);
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!("✓ removed {}", path.display());
            }
        }
    }
    Ok(())
}

fn find_model(id: &str) -> Result<&'static ModelEntry> {
    let id = canonical_id(id);
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
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
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

/// Public view of a catalog entry for the settings GUI.
pub struct CatalogEntry {
    pub id: &'static str,
    /// Read by the settings GUI's Models tab (`--features gui`).
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub description: &'static str,
    pub installed: bool,
}

/// List all catalog entries with their current installation status.
/// Reads the filesystem; cheap for the small catalog.
pub fn list_catalog() -> Vec<CatalogEntry> {
    CATALOG
        .iter()
        .map(|e| CatalogEntry {
            id: e.id,
            description: e.description,
            installed: entry_installed(e),
        })
        .collect()
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

    #[cfg(feature = "local")]
    #[test]
    fn resolve_unknown_model_names_the_alternatives() {
        let err = resolve_local_model("nonexistent-model-xyz").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown local model"), "got: {msg}");
        assert!(msg.contains("whisper-tiny-local"), "got: {msg}");
    }

    #[cfg(feature = "local")]
    #[test]
    fn aliases_resolve_to_the_catalog_id() {
        // Whether or not the files are on disk, an alias must reach the entry:
        // the error (if any) is "not downloaded", never "unknown model".
        for alias in ["tiny", "whisper-tiny", "whisper-tiny-local"] {
            match resolve_local_model(alias) {
                Ok(model) => assert_eq!(model.kind, LocalModelKind::Tiny),
                Err(e) => assert!(
                    e.to_string().contains("not downloaded"),
                    "alias {alias}: unexpected error {e}"
                ),
            }
        }
    }

    #[test]
    fn find_model_errors_on_unknown_id() {
        let err = find_model("not-a-real-model").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown model id"),
            "unexpected message: {msg}"
        );
        assert!(
            msg.contains("not-a-real-model"),
            "message missing id: {msg}"
        );
    }

    #[test]
    fn catalog_entries_have_weights_plus_tokenizer_and_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for entry in CATALOG {
            assert!(seen.insert(entry.id), "duplicate catalog id: {}", entry.id);
            let has_gguf = entry.files.iter().any(|f| f.url.ends_with(".gguf"));
            let has_tokenizer = entry.files.iter().any(|f| f.url.ends_with(".json"));
            assert!(has_gguf, "{} lacks GGUF weights", entry.id);
            assert!(has_tokenizer, "{} lacks a tokenizer", entry.id);
        }
    }
}
