//! Terminal-aware delivery (Linux): route dictation into zellij when the
//! focused window is a terminal showing a zellij session.
//!
//! The paste chord relies on the terminal turning Ctrl+Shift+V into a
//! bracketed paste and on zellij forwarding it; some TUIs inside zellij don't
//! receive that reliably. When the focused app is a known terminal and a zellij
//! session is reachable from it, dit instead writes the transcript straight
//! into the session's focused pane as a bracketed paste, so editors, shells and
//! agent TUIs treat it as pasted text and a newline never submits.
//!
//! Anything uncertain — detection unavailable, not a terminal, no zellij in it,
//! several candidate sessions, zellij failing — falls back to the configured
//! clipboard/typing delivery.

use std::path::Path;

use tracing::{debug, info, warn};

use crate::config::DeliverySetting;
use crate::focus::{terminal_name, FocusDetector, FocusedApp};
use crate::zellij::{self, ProcTable, Target, WriteError};

/// Outcome of one routed delivery attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Routed {
    /// The text was written into zellij; nothing else to do.
    Delivered,
    /// Nothing was written; deliver through the fallback path.
    Fallback,
    /// A partial write happened; do not fall back (it would duplicate text).
    Failed,
}

/// Where a delivery should go, decided before any text is sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Zellij {
        terminal: &'static str,
        target: Target,
    },
    Fallback(String),
}

/// Decide the route for one delivery. Pure apart from `select`, which resolves
/// the zellij session for a focused terminal.
pub fn plan(
    delivery: DeliverySetting,
    focused: Option<&FocusedApp>,
    select: impl FnOnce(&FocusedApp) -> anyhow::Result<Target>,
) -> Plan {
    if delivery != DeliverySetting::Auto {
        return Plan::Fallback(format!("delivery = {}", delivery.as_str()));
    }
    let Some(app) = focused else {
        return Plan::Fallback("focused app unknown".into());
    };
    let Some(terminal) = terminal_name(app) else {
        return Plan::Fallback(format!(
            "focused app {} is not a known terminal",
            describe(app)
        ));
    };
    match select(app) {
        Ok(target) => Plan::Zellij { terminal, target },
        Err(e) => Plan::Fallback(format!("{terminal} focused but no zellij target: {e:#}")),
    }
}

fn describe(app: &FocusedApp) -> String {
    match (app.app_id.is_empty(), app.wm_class.is_empty()) {
        (false, _) => format!("{:?}", app.app_id),
        (true, false) => format!("{:?}", app.wm_class),
        (true, true) => "<unnamed>".into(),
    }
}

/// Owns the focus detector for the injector thread.
pub struct TerminalRouter {
    focus: FocusDetector,
}

impl TerminalRouter {
    pub fn new() -> Self {
        Self {
            focus: FocusDetector::new(),
        }
    }

    /// Try to deliver `text` through zellij.
    pub fn try_deliver(&self, text: &str) -> Routed {
        let (focused, route) = self.route();
        if let Some(app) = &focused {
            debug!(
                "focus ({}): app_id={:?} wm_class={:?} pid={:?}",
                app.source, app.app_id, app.wm_class, app.pid
            );
        }
        match route {
            Plan::Fallback(reason) => {
                info!("delivery route: fallback ({reason})");
                Routed::Fallback
            }
            Plan::Zellij { terminal, target } => {
                info!(
                    "delivery route: zellij session {:?} in {terminal} (bracketed paste via {})",
                    target.session,
                    target.launcher.binary.display()
                );
                match zellij::write_paste(&target, text) {
                    Ok(()) => Routed::Delivered,
                    Err(WriteError::NotDelivered(e)) => {
                        info!("delivery route: zellij write failed, falling back: {e:#}");
                        Routed::Fallback
                    }
                    Err(WriteError::Partial(e)) => {
                        warn!("delivery route: zellij write failed mid-paste: {e:#}");
                        Routed::Failed
                    }
                }
            }
        }
    }

    /// Detect the focused app and plan the route for it (also used by
    /// `dit doctor`).
    pub fn route(&self) -> (Option<FocusedApp>, Plan) {
        let focused = self.focus.detect();
        let route = plan(DeliverySetting::Auto, focused.as_ref(), |app| {
            let procs = ProcTable::read(Path::new("/proc"))?;
            zellij::select_target(
                app.pid,
                &app.title,
                &procs,
                &mut zellij::list_live_sessions,
                &zellij::find_zellij_binary,
            )
        });
        (focused, route)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zellij::Launcher;
    use std::path::PathBuf;

    fn terminal() -> FocusedApp {
        FocusedApp {
            app_id: "Alacritty.desktop".into(),
            wm_class: "Alacritty".into(),
            pid: Some(4242),
            title: "laptop".into(),
            source: "gnome-shell",
        }
    }

    fn target() -> Target {
        Target {
            session: "laptop".into(),
            launcher: Launcher {
                binary: PathBuf::from("/usr/bin/zellij"),
                socket_dir: None,
            },
        }
    }

    #[test]
    fn auto_routes_a_terminal_with_a_zellij_session_to_zellij() {
        let route = plan(DeliverySetting::Auto, Some(&terminal()), |app| {
            assert_eq!(app.pid, Some(4242));
            Ok(target())
        });
        assert_eq!(
            route,
            Plan::Zellij {
                terminal: "Alacritty",
                target: target()
            }
        );
    }

    #[test]
    fn explicit_paste_or_type_never_routes() {
        for delivery in [DeliverySetting::Paste, DeliverySetting::Type] {
            let route = plan(delivery, Some(&terminal()), |_| {
                panic!("session selection must not run for {delivery:?}")
            });
            assert!(matches!(route, Plan::Fallback(_)), "{delivery:?}");
        }
    }

    #[test]
    fn unknown_focus_or_non_terminal_falls_back_without_looking_for_zellij() {
        let route = plan(DeliverySetting::Auto, None, |_| panic!("not a terminal"));
        assert!(matches!(route, Plan::Fallback(_)));

        let browser = FocusedApp {
            app_id: "firefox.desktop".into(),
            wm_class: "firefox".into(),
            ..terminal()
        };
        let route = plan(DeliverySetting::Auto, Some(&browser), |_| {
            panic!("not a terminal")
        });
        assert!(matches!(route, Plan::Fallback(reason) if reason.contains("firefox")));
    }

    #[test]
    fn terminal_without_a_reachable_session_falls_back() {
        let route = plan(DeliverySetting::Auto, Some(&terminal()), |_| {
            anyhow::bail!("no zellij client runs inside the focused terminal")
        });
        assert!(matches!(route, Plan::Fallback(reason) if reason.contains("no zellij client")));
    }
}
