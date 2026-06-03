//! A single dictation session: capture -> Scribe v2 Realtime WebSocket -> paste.
//!
//! Faithful to the original `whisperflow.py` semantics:
//!   * `partial_transcript`   → ignored (unstable preview).
//!   * `committed_transcript` → stable per-segment text; pasted immediately.
//!   * content dedup          → the same segment is never pasted twice.
//! On stop, an empty `commit: true` frame flushes the final segment, then we
//! keep reading for `FINAL_WAIT_SECS` to catch trailing commits.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Message};
use tracing::{info, warn};

use crate::audio::{self, Resampler};
use crate::config::{Config, CHUNK_MS, FINAL_WAIT_SECS, SAMPLE_RATE};
use crate::inject::Injector;
use crate::notify::notify;

/// Run one dictation session until `stop` is notified, then drain and close.
pub async fn run_session(cfg: Config, injector: Injector, stop: Arc<Notify>) -> Result<()> {
    injector.save();

    // --- microphone capture on its own thread ---
    let audio_stop = Arc::new(AtomicBool::new(false));
    let (samples_tx, mut samples_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (rate_tx, rate_rx) = oneshot::channel::<u32>();
    audio::spawn_capture(cfg.device.clone(), audio_stop.clone(), samples_tx, rate_tx);
    let native_rate = rate_rx.await.unwrap_or(SAMPLE_RATE);
    let resampler = Resampler::new(native_rate, SAMPLE_RATE);
    let chunk_bytes = (SAMPLE_RATE * 2 * CHUNK_MS / 1000) as usize;

    // --- connect to Scribe ---
    let request = ClientRequestBuilder::new(cfg.ws_url().parse()?)
        .with_header("xi-api-key", cfg.api_key.clone());
    let (ws, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = ws.split();

    notify("🎙️ Dictating…", "Speak — press the hotkey again to stop");
    info!("session started (mic {native_rate} Hz)");

    // --- receiver: paste committed segments ---
    let recv = tokio::spawn(async move {
        let mut last_committed = String::new();
        while let Some(frame) = read.next().await {
            let Ok(Message::Text(raw)) = frame else {
                if matches!(frame, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
                continue;
            };
            let Ok(evt) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            match evt
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or("")
            {
                "committed_transcript" => {
                    let text = evt.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    if text.is_empty() || text == last_committed {
                        continue;
                    }
                    last_committed = text.to_string();
                    injector.paste(format!("{text} "));
                    info!("committed: {text}");
                }
                "session_started" => info!("scribe session_started"),
                "partial_transcript" => {} // ignored on purpose
                other if other.contains("error") => {
                    warn!("server error: {evt}");
                    notify("STT error", &evt.to_string());
                    break;
                }
                _ => {}
            }
        }
        injector.restore();
    });

    // --- sender: stream audio until stopped, then flush a final commit ---
    let mut buf: Vec<u8> = Vec::with_capacity(chunk_bytes * 2);
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            maybe = samples_rx.recv() => match maybe {
                Some(frame) => {
                    resampler.push(&frame, &mut buf);
                    flush_chunks(&mut write, &mut buf, chunk_bytes).await?;
                }
                None => break,
            }
        }
    }
    audio_stop.store(true, std::sync::atomic::Ordering::Relaxed);

    // Drain whatever audio is still buffered in the channel.
    while let Ok(Some(frame)) = timeout(Duration::from_millis(50), samples_rx.recv()).await {
        resampler.push(&frame, &mut buf);
    }
    flush_chunks(&mut write, &mut buf, 1).await?; // flush remainder

    // Force a commit of the last open segment (VAD may not have closed it).
    let _ = write
        .send(Message::text(
            json!({
                "message_type": "input_audio_chunk",
                "audio_base_64": "",
                "commit": true
            })
            .to_string(),
        ))
        .await;

    // Wait briefly for trailing commits, then close.
    match timeout(Duration::from_secs_f64(FINAL_WAIT_SECS), recv).await {
        Ok(_) => {}
        Err(_) => { /* receiver still running; it owns the read half and will drop */ }
    }
    let _ = write.send(Message::Close(None)).await;
    info!("session ended");
    Ok(())
}

/// Send full `chunk_bytes`-sized audio frames out of `buf`. When `chunk_bytes`
/// is 1 (flush remainder), send whatever is left.
async fn flush_chunks<S>(write: &mut S, buf: &mut Vec<u8>, chunk_bytes: usize) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let threshold = chunk_bytes.max(1);
    while buf.len() >= threshold && !buf.is_empty() {
        let take = if chunk_bytes <= 1 {
            buf.len()
        } else {
            chunk_bytes
        };
        let chunk: Vec<u8> = buf.drain(..take).collect();
        let payload = json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": STANDARD.encode(&chunk),
        })
        .to_string();
        write.send(Message::text(payload)).await?;
        if chunk_bytes <= 1 {
            break;
        }
    }
    Ok(())
}
