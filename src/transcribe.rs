//! A single dictation session: capture -> Scribe v2 Realtime WebSocket -> type.
//!
//! Faithful to the original `whisperflow.py` semantics:
//!
//!   * `partial_transcript`   → ignored (unstable preview).
//!   * `committed_transcript` → stable per-segment text; typed immediately.
//!   * content dedup          → the same segment is never typed twice.
//!
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
use tracing::{debug, info, warn};

use crate::audio::{self, CaptureEvent, Resampler};
use crate::config::{Config, CHUNK_MS, FINAL_WAIT_SECS, SAMPLE_RATE};
use crate::inject::Injector;
use crate::notify::notify;
use crate::output::{Preview, SessionLog};
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

/// Run one dictation session until `stop` is notified, then drain and close.
async fn session_inner(
    cfg: Config,
    injector: Injector,
    stop: Arc<Notify>,
    state: mpsc::UnboundedSender<IconState>,
) -> Result<()> {
    // --- microphone capture on its own thread ---
    let audio_stop = Arc::new(AtomicBool::new(false));
    let (samples_tx, mut samples_rx) =
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
    let mut resampler = Resampler::new(native_rate, SAMPLE_RATE);
    let chunk_bytes = (SAMPLE_RATE * 2 * CHUNK_MS / 1000) as usize;

    // --- connect to Scribe ---
    let request = ClientRequestBuilder::new(cfg.ws_url().parse()?)
        .with_header("xi-api-key", cfg.api_key.clone());
    let (ws, _) = tokio_tungstenite::connect_async(request).await?;
    let (mut write, mut read) = ws.split();

    notify("🎙️ Dictating…", "Speak — press the hotkey again to stop");
    info!("session started (mic {native_rate} Hz)");

    // --- receiver: paste committed segments ---
    let preview_enabled = !cfg.no_preview;
    let recv = tokio::spawn(async move {
        let mut preview = Preview::new(preview_enabled);
        let mut log = SessionLog::open();
        let mut last_committed = String::new();
        // The latest previewed segment that has NOT yet been committed. A
        // non-empty value when the loop exits is a tail we never got a commit
        // for — recovered to the log (not pasted, to avoid a late clobber).
        let mut pending = String::new();
        while let Some(frame) = read.next().await {
            let raw = match frame {
                Ok(Message::Text(raw)) => raw,
                Ok(Message::Close(_)) => break,
                // tungstenite auto-queues Pong for Ping; flushed by the sender's
                // next audio frame. Binary/Pong/raw frames are unused here.
                Ok(_) => continue,
                Err(e) => {
                    warn!("websocket read error: {e}");
                    break;
                }
            };
            let Ok(evt) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let mtype = evt
                .get("message_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Every error payload carries its detail under `error`.
            let detail = || {
                evt.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or(mtype)
                    .to_string()
            };
            match mtype {
                // Stable text — both the plain and the timestamped variant carry
                // the committed segment under `text`.
                "committed_transcript" | "committed_transcript_with_timestamps" => {
                    let text = evt.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    // This segment is now committed, so the previewed tail is
                    // accounted for — drop it from the recovery buffer.
                    pending.clear();
                    if text.is_empty() {
                        warn!("committed empty transcript (audio received, but no recognizable speech)");
                        continue;
                    }
                    if text == last_committed {
                        continue;
                    }
                    last_committed = text.to_string();
                    injector.type_text(format!("{text} "));
                    log.committed(text);
                    if preview.enabled {
                        preview.commit(text);
                    } else {
                        info!("committed: {text}");
                    }
                }
                // Unstable preview — never pasted into the app (typing it
                // char-by-char would scramble output). Shown live in the
                // terminal and held as the recovery tail until it commits.
                "partial_transcript" => {
                    let t = evt.get("text").and_then(Value::as_str).unwrap_or("").trim();
                    if !t.is_empty() {
                        pending = t.to_string();
                        preview.partial(t);
                    }
                }
                "session_started" => {
                    let sid = evt.get("session_id").and_then(Value::as_str).unwrap_or("");
                    info!("scribe session_started: {sid}");
                }
                // Transient notices — log and keep transcribing.
                "commit_throttled" | "insufficient_audio_activity" | "queue_overflow" => {
                    warn!("scribe notice [{mtype}]: {}", detail());
                }
                // Fatal — surface to the user and end the session.
                "error"
                | "auth_error"
                | "quota_exceeded"
                | "unaccepted_terms"
                | "rate_limited"
                | "resource_exhausted"
                | "session_time_limit_exceeded"
                | "input_error"
                | "chunk_size_exceeded"
                | "transcriber_error" => {
                    let msg = detail();
                    warn!("scribe error [{mtype}]: {msg}");
                    notify("dit — STT error", &format!("{mtype}: {msg}"));
                    break;
                }
                other => debug!("unhandled message: {other} {evt}"),
            }
        }
        preview.clear();
        // A previewed tail that never committed: save it so nothing said is
        // lost, but don't type it (the app may no longer be focused).
        if !pending.is_empty() {
            warn!("recovering uncommitted tail to log: {pending}");
            log.uncommitted(&pending);
        }
    });

    // --- sender: stream audio until stopped, then flush a final commit ---
    let mut buf: Vec<u8> = Vec::with_capacity(chunk_bytes * 2);
    let mut sent_bytes: usize = 0;
    let mut sample_events: usize = 0;
    let mut warned_no_audio = false;
    let mut level = AudioLevel::default();
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)), if sent_bytes == 0 && !warned_no_audio => {
                warned_no_audio = true;
                warn!("audio capture opened, but no sample frames have reached the WebSocket yet");
                notify(
                    "dit — no audio frames yet",
                    "Microphone opened, but dit has not received samples from CPAL/PipeWire",
                );
            }
            maybe = samples_rx.recv() => match maybe {
                Some(CaptureEvent::Format { sample_rate }) => {
                    if sample_rate != native_rate {
                        info!("audio input switched to {sample_rate} Hz");
                    }
                    resampler = Resampler::new(sample_rate, SAMPLE_RATE);
                }
                Some(CaptureEvent::Samples(frame)) => {
                    sample_events += 1;
                    level.add(&frame);
                    resampler.push(&frame, &mut buf);
                    sent_bytes += flush_chunks(&mut write, &mut buf, chunk_bytes).await?;
                    if let Some(level) = level.maybe_emit() {
                        let _ = state.send(IconState::Recording { level });
                    }
                }
                None => break,
            }
        }
    }
    audio_stop.store(true, std::sync::atomic::Ordering::Relaxed);

    // Drain whatever audio is still buffered in the channel.
    while let Ok(Some(event)) = timeout(Duration::from_millis(50), samples_rx.recv()).await {
        match event {
            CaptureEvent::Format { sample_rate } => {
                resampler = Resampler::new(sample_rate, SAMPLE_RATE);
            }
            CaptureEvent::Samples(frame) => {
                sample_events += 1;
                level.add(&frame);
                resampler.push(&frame, &mut buf);
            }
        }
    }
    sent_bytes += flush_chunks(&mut write, &mut buf, 1).await?; // flush remainder
    info!(
        "audio sent: {:.1}s ({sent_bytes} bytes pcm16 @ {SAMPLE_RATE} Hz, {sample_events} sample callbacks)",
        sent_bytes as f64 / (SAMPLE_RATE as f64 * 2.0)
    );
    if sent_bytes == 0 {
        notify(
            "dit — no audio sent",
            "Recording stopped before any audio reached Scribe; check PipeWire/CPAL callback logs",
        );
    }

    // Force a commit of the last open segment (VAD may not have closed it).
    let _ = write
        .send(Message::text(
            json!({
                "message_type": "input_audio_chunk",
                "audio_base_64": "",
                "sample_rate": SAMPLE_RATE,
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
async fn flush_chunks<S>(write: &mut S, buf: &mut Vec<u8>, chunk_bytes: usize) -> Result<usize>
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    let threshold = chunk_bytes.max(1);
    let mut sent = 0;
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
            "sample_rate": SAMPLE_RATE,
            "commit": false,
        })
        .to_string();
        write.send(Message::text(payload)).await?;
        sent += chunk.len();
        if chunk_bytes <= 1 {
            break;
        }
    }
    Ok(sent)
}

fn vu_level_from_rms(rms: f64) -> u8 {
    if rms <= 0.0005 {
        return 0;
    }
    // Speech RMS values are usually small; square-root compression makes the
    // tray meter readable without needing exact dB calibration.
    ((rms.min(0.20) / 0.20).sqrt() * 255.0).round() as u8
}

#[derive(Default)]
struct AudioLevel {
    samples: usize,
    sumsq: f64,
    peak: f32,
    last_emit: Option<std::time::Instant>,
    last_log: Option<std::time::Instant>,
}

impl AudioLevel {
    fn add(&mut self, samples: &[f32]) {
        for &sample in samples {
            self.samples += 1;
            self.sumsq += (sample as f64) * (sample as f64);
            self.peak = self.peak.max(sample.abs());
        }
    }

    fn maybe_emit(&mut self) -> Option<u8> {
        let now = std::time::Instant::now();
        if self
            .last_emit
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(200))
        {
            return None;
        }
        if self.samples == 0 {
            return None;
        }
        let rms = (self.sumsq / self.samples as f64).sqrt();
        if self
            .last_log
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            info!("audio level: rms={rms:.4} peak={:.4}", self.peak);
            self.last_log = Some(now);
        }
        let level = vu_level_from_rms(rms);
        self.samples = 0;
        self.sumsq = 0.0;
        self.peak = 0.0;
        self.last_emit = Some(now);
        Some(level)
    }
}
