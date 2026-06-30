//! Transcription engine abstraction.
//!
//! [`Transcriber`] is the trait every engine must implement.  The two method
//! signatures — [`Transcriber::stream`] for real-time capture and
//! [`Transcriber::transcribe_buf`] for offline/batch use — let callers switch
//! engines without changing their code.  The cloud path is behind
//! [`ScribeTranscriber`] (ElevenLabs Scribe v2 Realtime).

pub mod scribe;

pub use scribe::ScribeTranscriber;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, Notify};

use crate::config::Config;
use crate::inject::Injector;
use crate::IconState;

/// A transcription backend that can be streaming, batch, or both.
///
/// Callers pick the mode they need and the engine takes care of the rest:
///
/// * **Streaming** (`stream`) — the engine captures from the system
///   microphone, forwards audio to the backend in real-time, and commits
///   each stable segment via `injector`.  The session runs until `stop` is
///   notified.
///
/// * **Batch** (`transcribe_buf`) — the engine transcribes a pre-captured
///   audio buffer and returns the full text.  Streaming-only engines return
///   an error from the default implementation.
pub trait Transcriber: Send {
    /// Run a real-time dictation session.
    async fn stream(
        &self,
        cfg: Config,
        injector: Injector,
        stop: Arc<Notify>,
        state_tx: mpsc::UnboundedSender<IconState>,
    ) -> Result<()>;

    /// Transcribe a complete audio buffer (offline / file mode).
    ///
    /// `samples` must be mono f32 PCM at `sample_rate` Hz.  Returns an error
    /// by default for streaming-only engines; local engines override this.
    #[allow(dead_code)]
    async fn transcribe_buf(
        &self,
        _cfg: Config,
        _samples: Vec<f32>,
        _sample_rate: u32,
    ) -> Result<String> {
        Err(anyhow::anyhow!(
            "batch transcription is not supported by this engine"
        ))
    }
}
