//! Two output surfaces that keep the focused app's text safe:
//!
//!   * [`Preview`] renders the unstable `partial_transcript` on a single,
//!     self-rewriting terminal line — the work "materializes" live without ever
//!     touching the target app (which only receives committed text).
//!   * [`SessionLog`] appends every committed segment to an on-disk file so
//!     nothing said is ever lost, even if a paste fails or the session dies.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
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
    /// Open a fresh per-session file, pruning old logs first.
    /// Best-effort: a failure degrades to a no-op logger rather than taking
    /// the session down.
    pub fn open(max_age_days: u64, max_count: usize) -> Self {
        let file = Self::try_open(max_age_days, max_count);
        if let Some((path, f)) = file {
            debug!("logging transcript to {}", path);
            return Self { file: Some(f) };
        }
        Self { file: None }
    }

    fn try_open(max_age_days: u64, max_count: usize) -> Option<(String, File)> {
        let dir = dirs::home_dir()?.join(".dit").join("sessions");
        fs::create_dir_all(&dir).ok()?;
        prune_sessions(&dir, max_age_days, max_count);
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

/// Prune old session log files from `dir`. Removes files older than
/// `max_age_days` days first, then removes the oldest remaining files if more
/// than `max_count` still exist. Best-effort: errors are silently ignored.
fn prune_sessions(dir: &Path, max_age_days: u64, max_count: usize) {
    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis(),
        Err(_) => return,
    };
    let max_age_ms = (max_age_days as u128) * 24 * 3600 * 1000;

    let mut entries: Vec<(u128, std::path::PathBuf)> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                let ms = s.strip_prefix("session-")?.strip_suffix(".txt")?.parse::<u128>().ok()?;
                Some((ms, e.path()))
            })
            .collect(),
        Err(_) => return,
    };

    // Sort oldest-first so excess trimming removes the oldest.
    entries.sort_unstable_by_key(|(ms, _)| *ms);

    // Remove files older than max_age_days.
    entries.retain(|(ms, path)| {
        if now_ms.saturating_sub(*ms) > max_age_ms {
            let _ = fs::remove_file(path);
            false
        } else {
            true
        }
    });

    // Remove oldest files if more than max_count remain.
    if entries.len() > max_count {
        let excess = entries.len() - max_count;
        for (_, path) in entries.iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dit-prune-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_session(dir: &Path, age_days: u64) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ms = now_ms.saturating_sub((age_days as u128) * 24 * 3600 * 1000);
        let path = dir.join(format!("session-{ms}.txt"));
        fs::write(path, "# test\n").unwrap();
    }

    fn count_sessions(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("session-"))
            .count()
    }

    #[test]
    fn prune_by_age_removes_old_sessions() {
        let dir = session_dir("age");
        make_session(&dir, 1);
        make_session(&dir, 10);
        make_session(&dir, 31);
        make_session(&dir, 60);

        prune_sessions(&dir, 30, 1000);
        assert_eq!(count_sessions(&dir), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_by_count_keeps_newest_after_age_prune() {
        let dir = session_dir("count");
        for i in 1..=5u64 {
            make_session(&dir, i);
        }

        prune_sessions(&dir, 30, 3);
        assert_eq!(count_sessions(&dir), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_all_recent_within_limit() {
        let dir = session_dir("keep");
        for i in 1..=5u64 {
            make_session(&dir, i);
        }

        prune_sessions(&dir, 30, 1000);
        assert_eq!(count_sessions(&dir), 5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_empty_dir_is_a_no_op() {
        let dir = session_dir("empty");
        prune_sessions(&dir, 30, 100);
        assert_eq!(count_sessions(&dir), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_ignores_non_session_files() {
        let dir = session_dir("nonmatch");
        fs::write(dir.join("other.txt"), "").unwrap();
        fs::write(dir.join("session-notanumber.txt"), "").unwrap();
        make_session(&dir, 60);

        prune_sessions(&dir, 30, 1000);
        // Only the parseable old session file is removed; unrelated files stay.
        assert!(dir.join("other.txt").exists());
        assert!(dir.join("session-notanumber.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
