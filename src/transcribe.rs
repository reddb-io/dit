//! A single dictation session: capture → engine → type.
//!
//! This module owns the audio-capture lifecycle and tray-state reporting.
//! Speech-to-text work is delegated to the [`engine::Transcriber`] trait;
//! the active implementation is [`engine::ScribeEngine`].

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;
use tracing::warn;

use crate::audio::{self, CaptureEvent};
use crate::config::{Config, Engine, SAMPLE_RATE};
use crate::engine::{ScribeEngine, Transcriber};
use crate::inject::Injector;
use crate::notify::notify;
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
    let result = session_inner(cfg, injector, stop, state.clone()).await;
    let _ = state.send(if result.is_ok() {
        IconState::Idle
    } else {
        IconState::Error
    });
    result
}

/// Start microphone capture, detect the native sample rate, then hand off to
/// the engine for the rest of the session.
async fn session_inner(
    cfg: Config,
    injector: Injector,
    stop: Arc<Notify>,
    state: mpsc::UnboundedSender<IconState>,
) -> Result<()> {
    let audio_stop = Arc::new(AtomicBool::new(false));
    let (samples_tx, samples_rx) =
        mpsc::channel::<CaptureEvent>(audio::recommended_audio_channel_capacity());
    let (rate_tx, rate_rx) = oneshot::channel::<u32>();
    audio::spawn_capture(cfg.device.clone(), audio_stop.clone(), samples_tx, rate_tx);
    let native_rate = match timeout(Duration::from_secs(5), rate_rx).await {
        Ok(Ok(rate)) => rate,
        Ok(Err(_)) => SAMPLE_RATE,
        Err(_) => {
            warn!("still waiting for a usable microphone");
            notify(
                "dit — waiting for microphone",
                "No usable input stream yet; plug/select a mic and dit will keep trying",
            );
            SAMPLE_RATE
        }
    };

    match cfg.engine {
        #[cfg(feature = "local")]
        Engine::Local => {
            use crate::engine::LocalEngine;
            use crate::models::resolve_local_model;
            let model = resolve_local_model(&cfg.model)?;
            LocalEngine::new(model)
                .run_stream(
                    &cfg,
                    injector,
                    samples_rx,
                    audio_stop,
                    native_rate,
                    stop,
                    state,
                )
                .await
        }
        #[cfg(not(feature = "local"))]
        Engine::Local => {
            anyhow::bail!("dit was built without --features local; rebuild with `cargo build --features local`")
        }
        Engine::Cloud => {
            ScribeEngine
                .run_stream(
                    &cfg,
                    injector,
                    samples_rx,
                    audio_stop,
                    native_rate,
                    stop,
                    state,
                )
                .await
        }
    }
}
