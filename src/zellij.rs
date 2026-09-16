//! Zellij delivery: find the session the focused terminal is showing and write
//! the transcript into its focused pane as a bracketed paste (Linux).
//!
//! The dit service runs outside any terminal, so it has no `ZELLIJ*` env. The
//! session is recovered from the focused window's process tree instead: the
//! terminal's descendant `zellij` client names its session on its command line
//! (`zellij attach --create NAME`, `zellij --session NAME`) or in its env
//! (`ZELLIJ_SESSION_NAME`). The client's own executable is used to talk to the
//! server, because zellij only lists sessions for a compatible binary — a
//! `zellij` found on `PATH` may be a different version from the running one.
//!
//! The paste is written as three separate `zellij action write` calls: the
//! `ESC[200~` marker, the text, and the `ESC[201~` marker. Zellij forwards a
//! write that is *exactly* a paste marker only when the pane's app enabled
//! bracketed paste (mode 2004) and drops it otherwise, just like a real
//! terminal would — a single combined write would leak literal `^[[200~` into
//! apps that never asked for bracketed paste.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Start-of-paste marker (`ESC [ 2 0 0 ~`).
pub const PASTE_BEGIN: &[u8] = b"\x1b[200~";
/// End-of-paste marker (`ESC [ 2 0 1 ~`).
pub const PASTE_END: &[u8] = b"\x1b[201~";
/// Upper bound for one zellij CLI call.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

// ── Bracketed paste payload ──────────────────────────────────────────────────

/// Make dictated text safe to place between paste markers: drop every control
/// character except tab and newlines. That removes ESC (so no `ESC[201~` or
/// any other escape sequence can end the paste early or drive the terminal),
/// DEL, and the C1 range (including the 8-bit CSI, U+009B).
pub fn sanitize_paste(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// The three writes that make up one bracketed paste, or `None` when nothing
/// printable is left to send.
pub fn paste_writes(text: &str) -> Option<[Vec<u8>; 3]> {
    let body = sanitize_paste(text);
    if body.is_empty() {
        return None;
    }
    Some([PASTE_BEGIN.to_vec(), body.into_bytes(), PASTE_END.to_vec()])
}

/// Arguments for `zellij --session NAME action write <bytes…>`. Bytes are
/// passed as decimal numbers, which sidesteps any quoting or leading-dash
/// ambiguity in the text.
pub fn write_args(session: &str, bytes: &[u8]) -> Vec<String> {
    let mut args = Vec::with_capacity(bytes.len() + 4);
    args.extend(["--session", session, "action", "write"].map(String::from));
    args.extend(bytes.iter().map(u8::to_string));
    args
}

// ── Process table ────────────────────────────────────────────────────────────

/// A snapshot of `/proc` (or a fake tree rooted elsewhere, for tests): every
/// process's parent and command name, with details read on demand.
pub struct ProcTable {
    root: PathBuf,
    parent: HashMap<u32, u32>,
    comm: HashMap<u32, String>,
}

impl ProcTable {
    /// Scan `<root>/<pid>/stat` for every numeric entry. Processes that vanish
    /// mid-scan or are unreadable are skipped.
    pub fn read(root: &Path) -> Result<Self> {
        let mut parent = HashMap::new();
        let mut comm = HashMap::new();
        let entries =
            std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))?;
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if let Some((name, ppid)) = parse_stat(&stat) {
                parent.insert(pid, ppid);
                comm.insert(pid, name);
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            parent,
            comm,
        })
    }

    pub fn contains(&self, pid: u32) -> bool {
        self.parent.contains_key(&pid)
    }

    /// Every descendant of `pid` (not including `pid`), in ascending pid order.
    pub fn descendants(&self, pid: u32) -> Vec<u32> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&child, &ppid) in &self.parent {
            children.entry(ppid).or_default().push(child);
        }
        let mut out = Vec::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            for &c in children.get(&p).into_iter().flatten() {
                // Guard against pid reuse loops in a racy snapshot.
                if c != pid && !out.contains(&c) {
                    out.push(c);
                    stack.push(c);
                }
            }
        }
        out.sort_unstable();
        out
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        std::fs::read(self.root.join(pid.to_string()).join("cmdline"))
            .map(|raw| split_nul(&raw))
            .unwrap_or_default()
    }

    fn environ_var(&self, pid: u32, key: &str) -> Option<String> {
        let raw = std::fs::read(self.root.join(pid.to_string()).join("environ")).ok()?;
        split_nul(&raw)
            .into_iter()
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
    }

    /// The executable behind `pid`, if it still exists on disk (an upgraded
    /// binary shows up as `… (deleted)` and can't be run again).
    fn exe(&self, pid: u32) -> Option<PathBuf> {
        let target = std::fs::read_link(self.root.join(pid.to_string()).join("exe")).ok()?;
        target.is_file().then_some(target)
    }

    /// Describe `pid` as a zellij client, or `None` when it isn't one (not
    /// zellij, the server, or a one-shot CLI call such as `zellij action`).
    fn zellij_client(&self, pid: u32) -> Option<ZellijClient> {
        let comm = self.comm.get(&pid)?;
        let args = self.cmdline(pid);
        let arg0_is_zellij = args
            .first()
            .and_then(|a| Path::new(a).file_name())
            .is_some_and(|n| n == "zellij");
        if comm != "zellij" && !arg0_is_zellij {
            return None;
        }
        let ClientArgs::Client(from_args) = parse_client_args(&args) else {
            return None;
        };
        let session = from_args.or_else(|| {
            self.environ_var(pid, "ZELLIJ_SESSION_NAME")
                .filter(|s| !s.is_empty())
        });
        Some(ZellijClient {
            session,
            exe: self.exe(pid),
            socket_dir: self
                .environ_var(pid, "ZELLIJ_SOCKET_DIR")
                .filter(|s| !s.is_empty())
                .map(OsString::from),
        })
    }
}

/// Parse `pid (comm) state ppid …`. The command name may itself contain spaces
/// or parentheses, so split at the *last* `)`.
fn parse_stat(stat: &str) -> Option<(String, u32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((name, ppid))
}

fn split_nul(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

// ── Client command lines ─────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum ClientArgs {
    /// An interactive client, with the session name when the args name it.
    Client(Option<String>),
    /// The server or a one-shot subcommand (`action`, `run`, `list-sessions`…).
    NotClient,
}

/// Global zellij options that consume the following argument.
const VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "--config-dir",
    "--data-dir",
    "-l",
    "--layout",
    "--layout-string",
    "--max-panes",
    "-n",
    "--new-session-with-layout",
];
/// `attach` options that consume the following argument.
const ATTACH_VALUE_OPTIONS: &[&str] = &["--index", "-t", "--token", "--ca-cert"];

/// Classify a zellij command line and extract the session it attaches to.
fn parse_client_args(args: &[String]) -> ClientArgs {
    let mut session = None;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--server" => return ClientArgs::NotClient,
            "-s" | "--session" => {
                session = args.get(i + 1).cloned();
                i += 2;
            }
            _ if arg.starts_with("--session=") => {
                session = Some(arg["--session=".len()..].to_string());
                i += 1;
            }
            _ if VALUE_OPTIONS.contains(&arg) => i += 2,
            _ if arg.starts_with('-') => i += 1,
            "attach" | "a" => {
                let mut j = i + 1;
                while j < args.len() {
                    let a = args[j].as_str();
                    if ATTACH_VALUE_OPTIONS.contains(&a) {
                        j += 2;
                    } else if a.starts_with('-') {
                        j += 1;
                    } else {
                        if a != "options" && a != "help" {
                            session = Some(a.to_string());
                        }
                        break;
                    }
                }
                break;
            }
            // `zellij [opts] options …` is still an interactive client.
            "options" => break,
            // Any other subcommand is a one-shot CLI call, not a client.
            _ => return ClientArgs::NotClient,
        }
    }
    ClientArgs::Client(session.filter(|s| !s.is_empty()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ZellijClient {
    session: Option<String>,
    exe: Option<PathBuf>,
    socket_dir: Option<OsString>,
}

// ── Session selection ────────────────────────────────────────────────────────

/// How to invoke the zellij CLI so it reaches the right server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launcher {
    pub binary: PathBuf,
    /// `ZELLIJ_SOCKET_DIR` of the client, when it overrides the default.
    pub socket_dir: Option<OsString>,
}

impl Launcher {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.binary);
        // Never let an inherited in-session env redirect the call.
        cmd.env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("ZELLIJ_PANE_ID");
        if let Some(dir) = &self.socket_dir {
            cmd.env("ZELLIJ_SOCKET_DIR", dir);
        }
        cmd
    }
}

/// A zellij session to deliver into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub session: String,
    pub launcher: Launcher,
}

/// Session names from `zellij list-sessions --no-formatting`, skipping exited
/// (resurrectable) sessions, which can't receive input.
pub fn parse_list_sessions(output: &str) -> Vec<String> {
    output
        .lines()
        .map(strip_ansi)
        .filter(|line| !line.contains("EXITED"))
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .filter(|name| !name.is_empty())
        .collect()
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI sequence: ESC [ params… final-byte.
            if chars.next() == Some('[') {
                for f in chars.by_ref() {
                    if ('@'..='~').contains(&f) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Does a terminal window title name this session? Zellij titles the terminal
/// with the session name (`NAME`, `NAME | pane`, `Zellij (NAME) - pane`).
fn title_names_session(title: &str, session: &str) -> bool {
    let title = title.trim();
    if title.is_empty() || session.is_empty() {
        return false;
    }
    if title == session || title.contains(&format!("({session})")) {
        return true;
    }
    title
        .strip_prefix(session)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_whitespace() || matches!(c, '|' | '-' | ':'))
}

/// The single candidate the title points at, if exactly one matches.
fn pick_by_title<'a>(
    title: &str,
    candidates: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    let mut hits = candidates
        .into_iter()
        .filter(|s| title_names_session(title, s));
    let first = hits.next()?;
    hits.next().is_none().then(|| first.clone())
}

/// Choose the zellij session the focused terminal shows.
///
/// * `focused_pid` known and in `procs`: look for zellij clients among its
///   descendants. None → no zellij in this terminal (never guess another
///   terminal's session). One named session → it. Several named sessions →
///   the one the window title names. Unnamed clients → the live session the
///   title names, else the only live session.
/// * `focused_pid` unknown: the live session the title names, else the only
///   live session.
///
/// `list_live` runs `zellij list-sessions` with the given launcher;
/// `default_binary` locates a zellij binary when no client exe is usable.
pub fn select_target(
    focused_pid: Option<u32>,
    title: &str,
    procs: &ProcTable,
    list_live: &mut dyn FnMut(&Launcher) -> Option<Vec<String>>,
    default_binary: &dyn Fn() -> Option<PathBuf>,
) -> Result<Target> {
    let default_launcher = |socket_dir: Option<OsString>| -> Result<Launcher> {
        let binary = default_binary().context("zellij is not installed (not found on PATH)")?;
        Ok(Launcher { binary, socket_dir })
    };

    let Some(pid) = focused_pid.filter(|p| procs.contains(*p)) else {
        let launcher = default_launcher(None)?;
        let live = list_live(&launcher).context("zellij list-sessions failed")?;
        let session = pick_by_title(title, &live)
            .or_else(|| (live.len() == 1).then(|| live[0].clone()))
            .with_context(|| {
                format!(
                    "focused window pid unknown and {} live zellij sessions; cannot tell which one is focused",
                    live.len()
                )
            })?;
        return Ok(Target { session, launcher });
    };

    let clients: Vec<ZellijClient> = procs
        .descendants(pid)
        .into_iter()
        .filter_map(|p| procs.zellij_client(p))
        .collect();
    if clients.is_empty() {
        bail!("no zellij client runs inside the focused terminal (pid {pid})");
    }

    let socket_dir = clients.iter().find_map(|c| c.socket_dir.clone());
    let launcher = match clients.iter().find_map(|c| c.exe.clone()) {
        Some(binary) => Launcher { binary, socket_dir },
        None => default_launcher(socket_dir)?,
    };

    let named: BTreeSet<String> = clients.iter().filter_map(|c| c.session.clone()).collect();
    let unnamed = clients.iter().any(|c| c.session.is_none());

    if !unnamed {
        if named.len() == 1 {
            let session = named.into_iter().next().expect("one element");
            return Ok(Target { session, launcher });
        }
        let session = pick_by_title(title, &named).with_context(|| {
            format!(
                "{} zellij sessions run inside the focused terminal; the window title names none of them",
                named.len()
            )
        })?;
        return Ok(Target { session, launcher });
    }

    let live = list_live(&launcher).context("zellij list-sessions failed")?;
    let session = pick_by_title(title, &live)
        .or_else(|| (clients.len() == 1 && live.len() == 1).then(|| live[0].clone()))
        .with_context(|| {
            format!(
                "the focused terminal's zellij client does not name its session and {} live sessions exist",
                live.len()
            )
        })?;
    Ok(Target { session, launcher })
}

/// Locate a `zellij` binary: `PATH` first, then the usual per-user install
/// locations (the service's `PATH` is often minimal).
pub fn find_zellij_binary() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join("zellij"));
    let from_home = dirs::home_dir().into_iter().flat_map(|h| {
        [
            h.join(".local/bin/zellij"),
            h.join(".local/share/mise/shims/zellij"),
            h.join(".cargo/bin/zellij"),
        ]
    });
    from_path.chain(from_home).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ── Running the CLI ──────────────────────────────────────────────────────────

/// Run a zellij CLI call with a timeout; returns stdout on success.
fn run(launcher: &Launcher, args: &[String]) -> Result<String> {
    let mut child = launcher
        .command()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", launcher.binary.display()))?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("zellij did not answer within {COMMAND_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let msg = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!(
            "zellij exited with {}: {}",
            out.status,
            msg.lines().next().unwrap_or("").trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `zellij list-sessions --no-formatting`, parsed to live session names.
pub fn list_live_sessions(launcher: &Launcher) -> Option<Vec<String>> {
    let args = ["list-sessions", "--no-formatting"].map(String::from);
    match run(launcher, &args) {
        Ok(out) => Some(parse_list_sessions(&out)),
        // "No active zellij sessions found." exits non-zero.
        Err(e) if e.to_string().contains("No active zellij sessions") => Some(Vec::new()),
        Err(_) => None,
    }
}

/// Why a zellij delivery did not complete.
#[derive(Debug)]
pub enum WriteError {
    /// Nothing reached the pane; the caller may safely fall back.
    NotDelivered(anyhow::Error),
    /// Part of the paste was written; falling back would duplicate text.
    Partial(anyhow::Error),
}

/// Write `text` into the focused pane of `target` as a bracketed paste.
pub fn write_paste(target: &Target, text: &str) -> std::result::Result<(), WriteError> {
    let Some(writes) = paste_writes(text) else {
        return Ok(());
    };
    for (i, bytes) in writes.iter().enumerate() {
        if let Err(e) = run(&target.launcher, &write_args(&target.session, bytes)) {
            return Err(if i == 0 {
                WriteError::NotDelivered(e)
            } else {
                WriteError::Partial(e)
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fake /proc ──

    struct FakeProc {
        root: PathBuf,
    }

    impl FakeProc {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("dit-fakeproc-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn add(&self, pid: u32, ppid: u32, comm: &str, args: &[&str], env: &[&str]) -> &Self {
            let dir = self.root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("stat"),
                format!("{pid} ({comm}) S {ppid} {pid} 0 0"),
            )
            .unwrap();
            std::fs::write(dir.join("cmdline"), nul_joined(args)).unwrap();
            std::fs::write(dir.join("environ"), nul_joined(env)).unwrap();
            self
        }

        /// Point `<pid>/exe` at a real (fake) executable file.
        fn exe(&self, pid: u32, name: &str) -> PathBuf {
            let bin = self.root.join(name);
            std::fs::write(&bin, "").unwrap();
            let link = self.root.join(pid.to_string()).join("exe");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&bin, &link).unwrap();
            bin
        }

        fn table(&self) -> ProcTable {
            ProcTable::read(&self.root).unwrap()
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn nul_joined(parts: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(p.as_bytes());
            out.push(0);
        }
        out
    }

    fn no_binary() -> Option<PathBuf> {
        Some(PathBuf::from("/usr/bin/zellij"))
    }

    fn select(
        pid: Option<u32>,
        title: &str,
        procs: &ProcTable,
        live: Option<Vec<&str>>,
    ) -> (Result<Target>, usize) {
        let mut calls = 0;
        let live: Option<Vec<String>> = live.map(|v| v.into_iter().map(String::from).collect());
        let mut list = |_: &Launcher| {
            calls += 1;
            live.clone()
        };
        let result = select_target(pid, title, procs, &mut list, &no_binary);
        (result, calls)
    }

    /// End-to-end check against a throwaway zellij session (never the user's):
    ///
    /// ```text
    /// DIT_ZELLIJ_E2E_PID=<pid whose child is the zellij client> \
    /// DIT_ZELLIJ_E2E_SESSION=<session name> \
    ///   cargo test -- --ignored zellij_e2e
    /// ```
    ///
    /// Then `zellij --session <name> action dump-screen` shows the paste.
    #[test]
    #[ignore = "needs a throwaway zellij session; see the doc comment"]
    fn zellij_e2e_bracketed_paste() {
        let pid: u32 = std::env::var("DIT_ZELLIJ_E2E_PID")
            .expect("DIT_ZELLIJ_E2E_PID")
            .parse()
            .expect("numeric pid");
        let session = std::env::var("DIT_ZELLIJ_E2E_SESSION").expect("DIT_ZELLIJ_E2E_SESSION");
        let procs = ProcTable::read(Path::new("/proc")).unwrap();
        let target = select_target(
            Some(pid),
            "",
            &procs,
            &mut list_live_sessions,
            &find_zellij_binary,
        )
        .expect("selects the throwaway session");
        assert_eq!(target.session, session);
        write_paste(&target, "dit e2e \x1b[201~olá\nfim").expect("writes the paste");
    }

    // ── bracketed paste ──

    #[test]
    fn paste_is_three_writes_with_markers_around_the_text() {
        let [begin, body, end] = paste_writes("olá, mundo").unwrap();
        assert_eq!(begin, b"\x1b[200~");
        assert_eq!(body, "olá, mundo".as_bytes());
        assert_eq!(end, b"\x1b[201~");
    }

    #[test]
    fn dictated_text_cannot_break_out_of_the_paste() {
        let hostile = "a\x1b[201~rm -rf ~\r\x1b]0;x\x07b\u{9b}201~c\x7fd";
        let body = sanitize_paste(hostile);
        assert!(!body.contains('\x1b'));
        assert!(!body.contains('\u{9b}'));
        assert!(!body.contains('\x07'));
        assert!(!body.contains('\x7f'));
        assert_eq!(body, "a[201~rm -rf ~\r]0;xb201~cd");
        // Whitespace that belongs to the text survives.
        assert_eq!(sanitize_paste("one\ttwo\nthree"), "one\ttwo\nthree");
    }

    #[test]
    fn nothing_printable_means_no_writes() {
        assert!(paste_writes("").is_none());
        assert!(paste_writes("\x1b\x1b\x07").is_none());
    }

    #[test]
    fn write_args_pass_bytes_as_decimal_numbers() {
        assert_eq!(
            write_args("main", b"\x1b[-a"),
            vec![
                "--session",
                "main",
                "action",
                "write",
                "27",
                "91",
                "45",
                "97"
            ]
        );
    }

    // ── /proc parsing ──

    #[test]
    fn stat_parsing_survives_odd_command_names() {
        assert_eq!(
            parse_stat("42 (zellij) S 7 42 0"),
            Some(("zellij".to_string(), 7))
        );
        assert_eq!(
            parse_stat("43 (my (odd) prog) R 1 43 0"),
            Some(("my (odd) prog".to_string(), 1))
        );
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn client_args_name_the_session() {
        let parse = |args: &[&str]| {
            parse_client_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        let named = |s: &str| ClientArgs::Client(Some(s.to_string()));
        assert_eq!(
            parse(&["zellij", "attach", "--create", "box"]),
            named("box")
        );
        assert_eq!(parse(&["/usr/bin/zellij", "a", "-c", "box"]), named("box"));
        assert_eq!(
            parse(&[
                "zellij", "--config", "/c.kdl", "--layout", "l.kdl", "attach", "--index", "0",
                "box"
            ]),
            named("box")
        );
        assert_eq!(parse(&["zellij", "-s", "work"]), named("work"));
        assert_eq!(parse(&["zellij", "--session=work"]), named("work"));
        assert_eq!(
            parse(&[
                "zellij",
                "--config",
                "c.kdl",
                "--session",
                "repro",
                "--new-session-with-layout",
                "l.kdl",
                "options",
                "--show-startup-tips",
                "false"
            ]),
            named("repro")
        );
        assert_eq!(parse(&["zellij"]), ClientArgs::Client(None));
        assert_eq!(
            parse(&["zellij", "options", "--simplified-ui", "true"]),
            ClientArgs::Client(None)
        );
        assert_eq!(
            parse(&["zellij", "--server", "/run/user/1000/zellij/x/box"]),
            ClientArgs::NotClient
        );
        assert_eq!(
            parse(&["zellij", "action", "write", "65"]),
            ClientArgs::NotClient
        );
        assert_eq!(parse(&["zellij", "list-sessions"]), ClientArgs::NotClient);
    }

    #[test]
    fn list_sessions_output_keeps_live_sessions_only() {
        let out = "cyber-laptop [Created 3h 2m ago] (current)\n\
                   \x1b[32;1mscratch\x1b[m [Created 5m ago]\n\
                   old [Created 2days ago] (EXITED - attach to resurrect)\n\
                   \n";
        assert_eq!(parse_list_sessions(out), vec!["cyber-laptop", "scratch"]);
        assert!(parse_list_sessions("").is_empty());
    }

    #[test]
    fn titles_identify_sessions() {
        assert!(title_names_session("box", "box"));
        assert!(title_names_session("box | nvim", "box"));
        assert!(title_names_session("Zellij (box) - bash", "box"));
        assert!(!title_names_session("boxer", "box"));
        assert!(!title_names_session("", "box"));
    }

    // ── session selection ──

    #[test]
    fn picks_the_session_of_the_client_inside_the_focused_terminal() {
        let fake = FakeProc::new("named");
        fake.add(100, 1, "alacritty", &["alacritty"], &[])
            .add(101, 100, "bash", &["bash"], &[])
            .add(
                102,
                101,
                "zellij",
                &["zellij", "attach", "--create", "laptop"],
                &["ZELLIJ_SOCKET_DIR=/tmp/zs"],
            )
            // A different terminal with its own session must not win.
            .add(200, 1, "kitty", &["kitty"], &[])
            .add(201, 200, "zellij", &["zellij", "attach", "other"], &[])
            // The server is not a client.
            .add(
                300,
                1,
                "zellij",
                &["zellij", "--server", "/run/user/1000/zellij/c/laptop"],
                &[],
            );
        let exe = fake.exe(102, "zellij-0.44");

        let (target, list_calls) = select(Some(100), "", &fake.table(), None);
        let target = target.unwrap();
        assert_eq!(target.session, "laptop");
        assert_eq!(target.launcher.binary, exe, "uses the client's own binary");
        assert_eq!(target.launcher.socket_dir, Some(OsString::from("/tmp/zs")));
        assert_eq!(list_calls, 0, "a named client needs no list-sessions call");
    }

    #[test]
    fn session_name_can_come_from_the_client_environment() {
        let fake = FakeProc::new("env");
        fake.add(100, 1, "foot", &["foot"], &[]).add(
            101,
            100,
            "zellij",
            &["zellij"],
            &["ZELLIJ_SESSION_NAME=from-env"],
        );
        let (target, _) = select(Some(100), "", &fake.table(), None);
        assert_eq!(target.unwrap().session, "from-env");
    }

    #[test]
    fn terminal_without_zellij_is_not_routed_even_if_sessions_exist() {
        let fake = FakeProc::new("nozellij");
        fake.add(100, 1, "alacritty", &["alacritty"], &[])
            .add(101, 100, "bash", &["bash"], &[])
            .add(200, 1, "kitty", &["kitty"], &[])
            .add(201, 200, "zellij", &["zellij", "attach", "elsewhere"], &[]);
        let (target, calls) = select(Some(100), "", &fake.table(), Some(vec!["elsewhere"]));
        assert!(target.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn unnamed_client_uses_title_then_single_live_session() {
        let fake = FakeProc::new("unnamed");
        fake.add(100, 1, "ghostty", &["ghostty"], &[])
            .add(101, 100, "zellij", &["zellij"], &[]);
        let procs = fake.table();

        let (target, calls) = select(Some(100), "", &procs, Some(vec!["only"]));
        assert_eq!(target.unwrap().session, "only");
        assert_eq!(calls, 1);

        let (target, _) = select(Some(100), "beta | vim", &procs, Some(vec!["alpha", "beta"]));
        assert_eq!(target.unwrap().session, "beta");

        let (target, _) = select(Some(100), "vim", &procs, Some(vec!["alpha", "beta"]));
        assert!(target.is_err(), "ambiguous sessions fall back");

        let (target, _) = select(Some(100), "", &procs, None);
        assert!(target.is_err(), "list-sessions failure falls back");
    }

    #[test]
    fn several_sessions_in_one_terminal_need_the_title() {
        let fake = FakeProc::new("several");
        fake.add(100, 1, "alacritty", &["alacritty"], &[])
            .add(101, 100, "zellij", &["zellij", "a", "one"], &[])
            .add(102, 100, "zellij", &["zellij", "a", "two"], &[]);
        let procs = fake.table();
        let (target, _) = select(Some(100), "two", &procs, None);
        assert_eq!(target.unwrap().session, "two");
        let (target, _) = select(Some(100), "shell", &procs, None);
        assert!(target.is_err());
    }

    #[test]
    fn unknown_pid_falls_back_to_the_single_live_session() {
        let fake = FakeProc::new("nopid");
        fake.add(100, 1, "alacritty", &["alacritty"], &[]);
        let procs = fake.table();

        let (target, _) = select(None, "", &procs, Some(vec!["laptop"]));
        let target = target.unwrap();
        assert_eq!(target.session, "laptop");
        assert_eq!(target.launcher.binary, PathBuf::from("/usr/bin/zellij"));

        // A pid that isn't in the table (e.g. another pid namespace) counts as unknown.
        let (target, _) = select(Some(999), "", &procs, Some(vec!["laptop"]));
        assert_eq!(target.unwrap().session, "laptop");

        let (target, _) = select(None, "", &procs, Some(vec!["a", "b"]));
        assert!(target.is_err());
        let (target, _) = select(None, "", &procs, Some(vec![]));
        assert!(target.is_err());
    }

    #[test]
    fn missing_zellij_binary_is_an_error_not_a_panic() {
        let fake = FakeProc::new("nobinary");
        fake.add(100, 1, "alacritty", &["alacritty"], &[]).add(
            101,
            100,
            "zellij",
            &["zellij", "attach", "s"],
            &[],
        );
        let mut list = |_: &Launcher| Some(vec!["s".to_string()]);
        let none = || None;
        // Client exe unreadable and nothing on PATH → error.
        assert!(select_target(Some(100), "", &fake.table(), &mut list, &none).is_err());
        assert!(select_target(None, "", &fake.table(), &mut list, &none).is_err());
    }
}
