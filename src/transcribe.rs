//! Session lifecycle: wraps the active engine with tray-state reporting.
//!
//! [`run_session`] drives one dictation session through the [`Transcriber`]
//! trait, reporting [`IconState`] transitions around the engine call so the
//! tray always reflects the current state.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, Notify};

use crate::config::Config;
use crate::engine::{ScribeTranscriber, Transcriber};
use crate::inject::Injector;
use crate::IconState;

/// Run one dictation session and report the tray state around it: Recording
/// while live, then Idle on a clean close or Error if it failed.
pub async fn run_session(
    cfg: Config,
    injector: Injector,
    stop: Arc<Notify>,
    state: mpsc::UnboundedSender<IconState>,
) -> Result<()> {
    let _ = state.send(IconState::Recording { level: 0 });
    let result = ScribeTranscriber
        .stream(cfg, injector, stop, state.clone())
        .await;
    let _ = state.send(if result.is_ok() {
        IconState::Idle
    } else {
        IconState::Error
    });
    result
}
