//! Backend abstraction for speech-to-text engines.
//!
//! [`Transcriber`] covers both live streaming (cloud) and batch (local)
//! transcription. The current implementation is [`ScribeEngine`] (ElevenLabs
//! Scribe v2 Realtime). Future slices will add local-engine implementations
//! behind the same trait without reshaping callers.

use std::sync::{atomic::AtomicBool, Arc};

use anyhow::Result;
use tokio::sync::{mpsc, Notify};

use crate::audio::CaptureEvent;
use crate::config::Config;
use crate::inject::Injector;
use crate::IconState;

pub mod scribe;
pub use scribe::ScribeEngine;

/// Speech-to-text backend.
///
/// `run_stream` drives live dictation (audio in → text typed in real time).
/// `transcribe_batch` covers future file/local-recording transcription (audio
/// buffer in → full transcript out). Both shapes share the same engine
/// abstraction so callers can switch implementations without structural changes.
pub trait Transcriber: Send + Sync {
    /// Connect to the backend, stream audio from `audio` until `stop` fires,
    /// then drain buffered frames, flush a final commit, and close. Committed
    /// segments are typed via `injector`. When the stop signal fires the engine
    /// sets `audio_stop` to signal the capture thread to halt.
    async fn run_stream(
        &self,
        cfg: &Config,
        injector: Injector,
        audio: mpsc::Receiver<CaptureEvent>,
        audio_stop: Arc<AtomicBool>,
        native_rate: u32,
        stop: Arc<Notify>,
        state: mpsc::UnboundedSender<IconState>,
    ) -> Result<()>;

    /// Transcribe a complete PCM-16 mono 16 kHz buffer and return the full
    /// transcript. Intended for batch/local-file transcription paths.
    async fn transcribe_batch(&self, pcm: Vec<i16>, language: &str) -> Result<String>;
}
