//! Two output surfaces that keep the focused app's text safe:
//!
//!   * [`Preview`] renders the unstable `partial_transcript` on a single,
//!     self-rewriting terminal line — the work "materializes" live without ever
//!     touching the target app (which only receives committed text).
//!   * [`SessionLog`] appends every committed segment to an on-disk file so
//!     nothing said is ever lost, even if a paste fails or the session dies.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::debug;

/// Live, in-place terminal preview of the current (uncommitted) segment.
pub struct Preview {
    pub enabled: bool,
    active: bool,
}

impl Preview {
    /// Enabled only when previews are wanted *and* stdout is a real terminal
    /// (so piping `dit > file` doesn't get ANSI control codes).
    pub fn new(want: bool) -> Self {
        Self {
            enabled: want && std::io::stdout().is_terminal(),
            active: false,
        }
    }

    /// Redraw the live preview line with the latest partial text (dimmed).
    pub fn partial(&mut self, text: &str) {
        if !self.enabled {
            return;
        }
        // \r → column 0, \x1b[2K → clear line, \x1b[2m..\x1b[0m → dim.
        print!("\r\x1b[2K\x1b[2m… {text}\x1b[0m");
        let _ = std::io::stdout().flush();
        self.active = true;
    }

    /// Lock the segment in: clear the live line and print it as a record row.
    pub fn commit(&mut self, text: &str) {
        if !self.enabled {
            return;
        }
        println!("\r\x1b[2K{text}");
        let _ = std::io::stdout().flush();
        self.active = false;
    }

    /// Drop any half-drawn preview line (e.g. on session end).
    pub fn clear(&mut self) {
        if self.enabled && self.active {
            print!("\r\x1b[2K");
            let _ = std::io::stdout().flush();
            self.active = false;
        }
    }
}

/// Append-only transcript file for the session, under `~/.dit/sessions/`.
pub struct SessionLog {
    file: Option<File>,
}

impl SessionLog {
    /// Open a fresh per-session file. Best-effort: a failure degrades to a
    /// no-op logger rather than taking the session down.
    pub fn open() -> Self {
        let file = Self::try_open();
        if let Some((path, f)) = file {
            debug!("logging transcript to {}", path);
            return Self { file: Some(f) };
        }
        Self { file: None }
    }

    fn try_open() -> Option<(String, File)> {
        let dir = dirs::home_dir()?.join(".dit").join("sessions");
        fs::create_dir_all(&dir).ok()?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("session-{stamp}.txt"));
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let _ = writeln!(f, "# dit session — unix_ms={stamp}");
        Some((path.display().to_string(), f))
    }

    /// Record a committed segment (one per line).
    pub fn committed(&mut self, text: &str) {
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{text}");
        }
    }

    /// Record a tail that was previewed but never committed (recovery only —
    /// deliberately NOT pasted, to avoid clobbering the focused app late).
    pub fn uncommitted(&mut self, text: &str) {
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "# [uncommitted] {text}");
        }
    }
}
