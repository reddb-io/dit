//! Focused-app detection for terminal-aware delivery (Linux).
//!
//! dit injects text below the compositor, so it normally has no idea *where*
//! the text lands. Terminal-aware delivery (see [`crate::terminal_route`]) needs
//! one fact: which app is focused, and its pid. Providers, tried in order:
//!
//!   1. **GNOME Shell** — the `dit-focus@reddb.io` extension shipped in
//!      `extras/gnome-shell/` exposes `io.reddb.dit.Focus.Get()` on the session
//!      bus. This is the only way to learn the focused window on GNOME Wayland.
//!   2. **X11** — `_NET_ACTIVE_WINDOW` + `WM_CLASS` + `_NET_WM_PID`, via the
//!      pure-Rust `x11rb` client. Only on X11 sessions: under Wayland, XWayland
//!      only knows about X11 windows, so its answer would be wrong.
//!
//! Anything else yields `None` ("unknown"), and delivery keeps its existing
//! clipboard/typing behaviour. Every provider is cheap (one round trip) so it
//! runs per delivery rather than tracking focus continuously.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::debug;

/// What the focused window told us about itself. Fields are empty/`None` when a
/// provider could not tell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FocusedApp {
    /// Desktop app id (`Alacritty.desktop`, `org.gnome.Ptyxis.desktop`) on
    /// GNOME; the `WM_CLASS` instance name on X11.
    pub app_id: String,
    /// Wayland app id / X11 `WM_CLASS` class (`Alacritty`, `kitty`).
    pub wm_class: String,
    /// Process id of the window's client, when known.
    pub pid: Option<u32>,
    /// Window title (used only to disambiguate zellij sessions).
    pub title: String,
    /// Which provider answered (`gnome-shell`, `x11`), for logs.
    pub source: &'static str,
}

/// The well-known terminals terminal-aware delivery applies to. Each entry
/// lists the normalised app ids / `WM_CLASS` values that identify it.
const TERMINALS: &[(&str, &[&str])] = &[
    ("Alacritty", &["alacritty"]),
    ("kitty", &["kitty"]),
    (
        "WezTerm",
        &["org.wezfurlong.wezterm", "wezterm", "wezterm-gui"],
    ),
    ("foot", &["foot", "footclient", "org.codeberg.dnkl.foot"]),
    ("Ghostty", &["com.mitchellh.ghostty", "ghostty"]),
    (
        "GNOME Terminal",
        &[
            "org.gnome.terminal",
            "gnome-terminal",
            "gnome-terminal-server",
        ],
    ),
    (
        "Ptyxis",
        &["org.gnome.ptyxis", "org.gnome.ptyxis.devel", "ptyxis"],
    ),
    ("GNOME Console", &["org.gnome.console", "kgx"]),
    ("Konsole", &["org.kde.konsole", "konsole"]),
];

/// Lowercase and drop a trailing `.desktop`, so `Alacritty.desktop`,
/// `Alacritty` and `alacritty` compare equal.
fn normalize_id(id: &str) -> String {
    let lower = id.trim().to_lowercase();
    lower
        .strip_suffix(".desktop")
        .map(str::to_string)
        .unwrap_or(lower)
}

/// The display name of the known terminal `app` is, if any. Matches the app id
/// or the `WM_CLASS`, whichever the provider filled in.
pub fn terminal_name(app: &FocusedApp) -> Option<&'static str> {
    let ids = [normalize_id(&app.app_id), normalize_id(&app.wm_class)];
    TERMINALS.iter().find_map(|(name, known)| {
        ids.iter()
            .any(|id| !id.is_empty() && known.contains(&id.as_str()))
            .then_some(*name)
    })
}

/// Whether clipboard delivery should use the terminal paste chord for the
/// focused app. The configured value remains the fallback when focus is
/// unavailable or the focused window is not a known terminal.
pub fn paste_with_shift(app: Option<&FocusedApp>, configured: bool) -> bool {
    configured || app.and_then(terminal_name).is_some()
}

/// Whether the X11 provider should be consulted. `XDG_SESSION_TYPE` decides
/// when set; otherwise an X `DISPLAY` without a `WAYLAND_DISPLAY` means X11.
fn session_is_x11(get: impl Fn(&str) -> Option<String>) -> bool {
    match get("XDG_SESSION_TYPE").as_deref() {
        Some("x11") => true,
        Some("wayland") => false,
        _ => {
            get("WAYLAND_DISPLAY").filter(|v| !v.is_empty()).is_none()
                && get("DISPLAY").filter(|v| !v.is_empty()).is_some()
        }
    }
}

/// D-Bus coordinates of the GNOME Shell extension.
pub const GNOME_BUS_NAME: &str = "io.reddb.dit.Focus";
const GNOME_OBJECT_PATH: &str = "/io/reddb/dit/Focus";
const GNOME_INTERFACE: &str = "io.reddb.dit.Focus";
/// Upper bound for one focus query; delivery must never stall on it.
const QUERY_TIMEOUT: Duration = Duration::from_millis(400);

/// Runs the focus providers. Owns a small current-thread Tokio runtime for the
/// async D-Bus client, so it can be driven from the (synchronous) injector
/// thread.
pub struct FocusDetector {
    rt: Option<tokio::runtime::Runtime>,
}

impl FocusDetector {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| debug!("focus: no runtime for the D-Bus provider: {e}"))
            .ok();
        Self { rt }
    }

    /// Ask each provider in turn; `None` when none of them could tell.
    pub fn detect(&self) -> Option<FocusedApp> {
        if let Some(rt) = &self.rt {
            match rt.block_on(query_gnome_shell()) {
                Ok(Some(app)) => return Some(app),
                Ok(None) => debug!("focus: GNOME Shell extension not available"),
                Err(e) => debug!("focus: GNOME Shell query failed: {e:#}"),
            }
        }
        if session_is_x11(|k| std::env::var(k).ok()) {
            match query_x11() {
                Ok(app) => return app,
                Err(e) => debug!("focus: X11 query failed: {e:#}"),
            }
        }
        None
    }
}

/// Query the `dit-focus@reddb.io` extension. `Ok(None)` when it isn't running
/// (name not owned) — the common "not installed / not GNOME" case.
async fn query_gnome_shell() -> Result<Option<FocusedApp>> {
    let call = async {
        let conn = zbus::Connection::session()
            .await
            .context("session bus unavailable")?;
        let reply = conn
            .call_method(
                Some(GNOME_BUS_NAME),
                GNOME_OBJECT_PATH,
                Some(GNOME_INTERFACE),
                "Get",
                &(),
            )
            .await;
        let reply = match reply {
            Ok(r) => r,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                    || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e).context("io.reddb.dit.Focus.Get failed"),
        };
        let (app_id, wm_class, pid, title): (String, String, u32, String) = reply
            .body()
            .deserialize()
            .context("unexpected io.reddb.dit.Focus.Get reply")?;
        Ok(Some(FocusedApp {
            app_id,
            wm_class,
            pid: (pid > 0).then_some(pid),
            title,
            source: "gnome-shell",
        }))
    };
    tokio::time::timeout(QUERY_TIMEOUT, call)
        .await
        .context("io.reddb.dit.Focus.Get timed out")?
}

/// Query the X server for the active window. `Ok(None)` when no window is
/// active.
fn query_x11() -> Result<Option<FocusedApp>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Window};

    let (conn, screen) = x11rb::connect(None).context("cannot connect to the X server")?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &[u8]| -> Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    let net_active_window = atom(b"_NET_ACTIVE_WINDOW")?;
    let net_wm_pid = atom(b"_NET_WM_PID")?;
    let net_wm_name = atom(b"_NET_WM_NAME")?;
    let utf8_string = atom(b"UTF8_STRING")?;

    let active: Option<Window> = conn
        .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)?
        .reply()?
        .value32()
        .and_then(|mut v| v.next())
        .filter(|w| *w != 0);
    let Some(win) = active else {
        return Ok(None);
    };

    let class = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?
        .reply()?
        .value;
    let (instance, class) = split_wm_class(&class);
    let pid = conn
        .get_property(false, win, net_wm_pid, AtomEnum::CARDINAL, 0, 1)?
        .reply()?
        .value32()
        .and_then(|mut v| v.next())
        .filter(|p| *p > 0);
    let title = conn
        .get_property(false, win, net_wm_name, utf8_string, 0, 4096)?
        .reply()?
        .value;

    Ok(Some(FocusedApp {
        app_id: instance,
        wm_class: class,
        pid,
        title: String::from_utf8_lossy(&title).into_owned(),
        source: "x11",
    }))
}

/// Split a raw `WM_CLASS` property (`instance\0class\0`) into its two parts.
fn split_wm_class(raw: &[u8]) -> (String, String) {
    let mut parts = raw
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    (instance, class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(app_id: &str, wm_class: &str) -> FocusedApp {
        FocusedApp {
            app_id: app_id.into(),
            wm_class: wm_class.into(),
            ..Default::default()
        }
    }

    #[test]
    fn known_terminals_are_recognised_by_app_id_or_wm_class() {
        let table = [
            (app("Alacritty.desktop", "Alacritty"), "Alacritty"),
            (app("", "Alacritty"), "Alacritty"),
            (app("kitty.desktop", "kitty"), "kitty"),
            (app("org.wezfurlong.wezterm.desktop", ""), "WezTerm"),
            (app("", "org.wezfurlong.wezterm"), "WezTerm"),
            (app("foot.desktop", "foot"), "foot"),
            (app("", "footclient"), "foot"),
            (app("com.mitchellh.ghostty.desktop", ""), "Ghostty"),
            (app("org.gnome.Terminal.desktop", ""), "GNOME Terminal"),
            (
                app("gnome-terminal-server", "Gnome-terminal"),
                "GNOME Terminal",
            ),
            (
                app("org.gnome.Ptyxis.desktop", "org.gnome.Ptyxis"),
                "Ptyxis",
            ),
            (app("org.gnome.Console.desktop", ""), "GNOME Console"),
            (app("org.kde.konsole.desktop", ""), "Konsole"),
            (app("", "konsole"), "Konsole"),
        ];
        for (focused, expected) in table {
            assert_eq!(
                terminal_name(&focused),
                Some(expected),
                "focused app {focused:?}"
            );
        }
    }

    #[test]
    fn non_terminals_and_unknown_windows_are_not_terminals() {
        for focused in [
            app("firefox.desktop", "firefox"),
            app("code.desktop", "Code"),
            app("org.gnome.Nautilus.desktop", "org.gnome.Nautilus"),
            // Substrings must not match: "kittyhawk" is not kitty.
            app("", "kittyhawk"),
            app("", ""),
        ] {
            assert_eq!(terminal_name(&focused), None, "focused app {focused:?}");
        }
    }

    #[test]
    fn terminals_select_ctrl_shift_v_without_manual_configuration() {
        assert!(paste_with_shift(
            Some(&app("Alacritty.desktop", "Alacritty")),
            false
        ));
        assert!(!paste_with_shift(
            Some(&app("firefox.desktop", "firefox")),
            false
        ));
        assert!(!paste_with_shift(None, false));
        assert!(paste_with_shift(None, true));
    }

    #[test]
    fn x11_provider_only_runs_on_x11_sessions() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(session_is_x11(env(&[("XDG_SESSION_TYPE", "x11")])));
        // GNOME Wayland exports DISPLAY for XWayland too; it must not count.
        assert!(!session_is_x11(env(&[
            ("XDG_SESSION_TYPE", "wayland"),
            ("DISPLAY", ":0"),
        ])));
        assert!(session_is_x11(env(&[("DISPLAY", ":0")])));
        assert!(!session_is_x11(env(&[
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ])));
        assert!(!session_is_x11(env(&[])));
    }

    #[test]
    fn wm_class_property_splits_into_instance_and_class() {
        assert_eq!(
            split_wm_class(b"Alacritty\0Alacritty\0"),
            ("Alacritty".to_string(), "Alacritty".to_string())
        );
        assert_eq!(
            split_wm_class(b"gnome-terminal-server\0Gnome-terminal\0"),
            (
                "gnome-terminal-server".to_string(),
                "Gnome-terminal".to_string()
            )
        );
        assert_eq!(split_wm_class(b""), (String::new(), String::new()));
    }
}
