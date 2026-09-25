//! A single dictation session: capture → engine → type.
//!
//! This module owns the audio-capture lifecycle and tray-state reporting.
//! Speech-to-text work is delegated to the [`engine::Transcriber`] trait;
//! the active implementation is [`engine::ScribeEngine`].

use std::sync::atomic::{AtomicBool, Ordering};
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

/// Stops the audio-capture thread when the session ends for *any* reason.
///
/// The engine also sets this flag on its happy path, but it can return early
/// with `?` (e.g. a failed WebSocket connect or write) before doing so, which
/// left a detached capture thread re-opening the microphone forever. Owning the
/// flag in a guard covers every exit path, including the error ones.
struct StopCaptureOnDrop(Arc<AtomicBool>);

impl Drop for StopCaptureOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run one dictation session and report the tray state around it: Recording
/// while live, then Idle on a clean close or Error if it failed.
pub async fn run_session(
    cfg: Config,
    injector: Injector,
    stop: Arc<Notify>,
    state: mpsc::UnboundedSender<IconState>,
) -> Result<()> {
    let _ = state.send(IconState::Recording { level: 0 });
    injector.start();
    let result = session_inner(cfg, injector.clone(), stop, state.clone()).await;
    injector.finish();
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
    // Guarantee the capture thread is torn down even if the engine bails early.
    let _capture_guard = StopCaptureOnDrop(audio_stop.clone());
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
        Engine::ElevenLabs => {
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
