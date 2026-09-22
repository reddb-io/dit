//! Native Redcode composer delivery for Linux.
//!
//! A running Redcode TUI publishes a capability-protected Unix socket under
//! `$XDG_RUNTIME_DIR`. We only connect when that TUI can be tied to the focused
//! terminal directly or to the focused zellij session. Ambiguous matches fall
//! back to dit's ordinary delivery path.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info};

use crate::focus::{terminal_name, FocusedApp};
use crate::terminal_route::Plan;
use crate::zellij::ProcTable;

const IO_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct Registration {
    version: u8,
    pid: u32,
    socket: PathBuf,
    token: String,
    zellij_session: Option<String>,
}

pub struct VoiceSink {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    token: String,
    dictation_id: String,
}

impl VoiceSink {
    pub fn connect(focused: Option<&FocusedApp>, plan: &Plan, dictation_id: &str) -> Option<Self> {
        let registration = select_registration(focused, plan)?;
        let stream = UnixStream::connect(&registration.socket).ok()?;
        stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
        stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
        let reader = BufReader::new(stream.try_clone().ok()?);
        let mut sink = Self {
            stream,
            reader,
            token: registration.token,
            dictation_id: dictation_id.to_string(),
        };
        sink.send("start", None).then(|| {
            info!(
                "delivery route: Redcode voice sink pid {} ({})",
                registration.pid,
                registration.socket.display()
            );
            sink
        })
    }

    pub fn partial(&mut self, text: &str) -> bool {
        self.send("partial", Some(text))
    }

    pub fn commit(&mut self, text: &str) -> bool {
        self.send("commit", Some(text))
    }

    pub fn finish(&mut self) -> bool {
        self.send("finish", None)
    }

    fn send(&mut self, event: &str, text: Option<&str>) -> bool {
        let mut value = json!({
            "version": 1,
            "token": self.token,
            "dictation_id": self.dictation_id,
            "event": event,
        });
        if let Some(text) = text {
            value["text"] = json!(text);
        }
        if writeln!(self.stream, "{value}").is_err() {
            return false;
        }
        let mut response = String::new();
        if self.reader.read_line(&mut response).is_err() {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| value.get("ok").and_then(|ok| ok.as_bool()))
            .unwrap_or(false)
    }
}

fn select_registration(focused: Option<&FocusedApp>, plan: &Plan) -> Option<Registration> {
    let focused = focused?;
    terminal_name(focused)?;
    let registrations = registrations();
    let matches = match plan {
        Plan::Zellij { target, .. } => registrations
            .into_iter()
            .filter(|registration| {
                registration.zellij_session.as_deref() == Some(target.session.as_str())
            })
            .collect::<Vec<_>>(),
        Plan::Fallback(_) => {
            let focused_pid = focused.pid?;
            let procs = ProcTable::read(Path::new("/proc")).ok()?;
            let descendants = procs.descendants(focused_pid);
            registrations
                .into_iter()
                .filter(|registration| descendants.contains(&registration.pid))
                .collect::<Vec<_>>()
        }
    };
    if matches.len() != 1 {
        debug!("Redcode voice sink candidates: {}", matches.len());
        return None;
    }
    matches.into_iter().next()
}

fn registrations() -> Vec<Registration> {
    let Some(uid) = current_uid() else {
        return Vec::new();
    };
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("redcode-{uid}"))
        .join("voice-input");
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|contents| serde_json::from_str::<Registration>(&contents).ok())
        .filter(|registration| {
            registration.version == 1
                && !registration.token.is_empty()
                && registration.socket.is_absolute()
                && Path::new("/proc")
                    .join(registration.pid.to_string())
                    .exists()
        })
        .collect()
}

fn current_uid() -> Option<u32> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_effective_process_uid() {
        assert!(current_uid().is_some());
    }
}
