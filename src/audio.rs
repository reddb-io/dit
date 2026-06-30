//! Cross-platform microphone capture (replaces `parec` from the original script).
//!
//! `cpal` opens the best available input device: an explicit `--device` match,
//! then PipeWire, then real ALSA hardware/plughw devices, and only then the
//! system default alias. This avoids getting stuck on “default” devices that
//! open successfully but do not feed CPAL audio on some PipeWire/ALSA setups.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

/// Bounded realtime audio queue. If the WebSocket/network is behind, callbacks
/// drop stale frames instead of letting memory grow or sending old speech late.
const STALE_AUDIO_CAPACITY: usize = 64;

/// Messages produced by the capture thread.
pub enum CaptureEvent {
    /// The active microphone changed format. Consumers must rebuild resamplers.
    Format { sample_rate: u32 },
    /// Mono `f32` frames at the most recently announced native sample rate.
    Samples(Vec<f32>),
}

pub fn recommended_audio_channel_capacity() -> usize {
    STALE_AUDIO_CAPACITY
}

/// Rank capture devices. Lower is better. This avoids getting stuck on ALSA
/// pseudo-devices that CPAL may enumerate as inputs but that commonly fail or
/// are playback-oriented aliases (`front`, `surround*`, `dmix`, HDMI, etc.).
fn device_rank(
    name: &str,
    prefer: Option<&str>,
    default_name: Option<&str>,
    index: usize,
) -> (u8, usize) {
    let lower = name.to_lowercase();
    let preferred = prefer.is_some_and(|needle| lower.contains(needle));
    let default = default_name.is_some_and(|default| name == default);
    let bad_alsa_pseudo = lower.starts_with("surround")
        || lower.starts_with("front:")
        || lower.starts_with("dmix")
        || lower.starts_with("iec958")
        || lower.starts_with("hdmi");
    let pipewire = lower == "pipewire";
    let default_alias = lower == "default";
    let plughw = lower.starts_with("plughw:");
    let hw = lower.starts_with("hw:");
    let sysdefault = lower.starts_with("sysdefault:");
    // Continuity Camera / iPhone mics on macOS are real capture hardware and
    // must not fall into the generic catch-all below real ALSA hardware.
    let continuity_mic = lower.contains("iphone") || lower.contains("continuity");

    let rank = if preferred {
        0
    } else if pipewire {
        1
    } else if plughw || continuity_mic {
        2
    } else if hw {
        3
    } else if sysdefault {
        4
    } else if default || default_alias {
        6
    } else if bad_alsa_pseudo {
        9
    } else {
        5
    };
    (rank, index)
}

/// List input devices to stdout (for `--list-devices`).
pub fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    let default = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    println!("Input devices:");
    for device in host.input_devices()? {
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let marker = if name == default { " (default)" } else { "" };
        println!("  - {name}{marker}");
    }
    Ok(())
}

/// Input device names for the tray "Device" submenu, ranked best-first so the
/// most likely real microphone shows up at the top. Best-effort: returns an
/// empty list when enumeration fails rather than erroring.
pub fn device_names() -> Vec<String> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut ranked: Vec<((u8, usize), String)> = devices
        .enumerate()
        .map(|(idx, device)| {
            let name = device.name().unwrap_or_else(|_| "<unknown>".into());
            let rank = device_rank(&name, None, default_name.as_deref(), idx);
            (rank, name)
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    let mut names: Vec<String> = Vec::with_capacity(ranked.len());
    for (_, name) in ranked {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn candidate_devices(prefer: &Option<String>) -> Result<Vec<(String, cpal::Device)>> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let prefer = prefer.as_ref().map(|s| s.to_lowercase());
    let mut ranked = Vec::new();

    for (idx, device) in host.input_devices()?.enumerate() {
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let rank = device_rank(&name, prefer.as_deref(), default_name.as_deref(), idx);
        ranked.push((rank, name, device));
    }

    if ranked.is_empty() {
        return Err(anyhow!("no input devices available"));
    }
    if let Some(needle) = prefer {
        if !ranked
            .iter()
            .any(|(_, name, _)| name.to_lowercase().contains(&needle))
        {
            warn!("device matching '{needle}' not found, trying all input devices");
        }
    }

    ranked.sort_by_key(|(rank, _, _)| *rank);
    Ok(ranked
        .into_iter()
        .map(|(_, name, device)| (name, device))
        .collect())
}

/// Spawn the capture thread. Returns once the stream is live (or errors).
///
/// * `events_tx` receives format-change notices and mono `f32` chunks.
/// * `rate_tx` is fired once with the first live native sample rate.
/// * Setting `stop` tears the stream down.
pub fn spawn_capture(
    prefer: Option<String>,
    stop: Arc<AtomicBool>,
    events_tx: Sender<CaptureEvent>,
    rate_tx: oneshot::Sender<u32>,
) {
    std::thread::spawn(move || {
        run_capture(prefer, stop, events_tx, rate_tx);
    });
}

fn run_capture(
    prefer: Option<String>,
    stop: Arc<AtomicBool>,
    events_tx: Sender<CaptureEvent>,
    rate_tx: oneshot::Sender<u32>,
) {
    let mut first_rate = Some(rate_tx);
    while !stop.load(Ordering::Relaxed) {
        match run_capture_once(&prefer, stop.clone(), events_tx.clone(), &mut first_rate) {
            Ok(()) => break,
            Err(e) if stop.load(Ordering::Relaxed) => {
                debug!("audio capture stopped: {e:#}");
                break;
            }
            Err(e) => {
                warn!("audio capture unavailable, retrying: {e:#}");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

fn run_capture_once(
    prefer: &Option<String>,
    stop: Arc<AtomicBool>,
    events_tx: Sender<CaptureEvent>,
    first_rate: &mut Option<oneshot::Sender<u32>>,
) -> Result<()> {
    let candidates = candidate_devices(prefer)?;
    let mut errors = Vec::new();
    for (name, device) in candidates {
        match open_stream(&name, &device, stop.clone(), events_tx.clone(), first_rate) {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    Err(anyhow!(
        "no usable input device found ({})",
        errors.join("; ")
    ))
}

fn open_stream(
    name: &str,
    device: &cpal::Device,
    stop: Arc<AtomicBool>,
    events_tx: Sender<CaptureEvent>,
    first_rate: &mut Option<oneshot::Sender<u32>>,
) -> Result<()> {
    let config = device.default_input_config()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("capturing from '{name}' @ {sample_rate} Hz, {channels} ch");
    if let Some(tx) = first_rate.take() {
        let _ = tx.send(sample_rate);
    }
    let _ = events_tx.try_send(CaptureEvent::Format { sample_rate });

    let stream_failed = Arc::new(AtomicBool::new(false));
    let callbacks = Arc::new(AtomicU64::new(0));
    let delivered_samples = Arc::new(AtomicU64::new(0));
    let dropped_samples = Arc::new(AtomicU64::new(0));
    let failed = stream_failed.clone();
    let device_name = name.to_string();
    let err_fn = move |e| {
        error!("audio stream error on '{device_name}': {e}");
        failed.store(true, Ordering::Relaxed);
    };

    // Downmix interleaved frames to mono and forward. try_send intentionally
    // drops frames when the bounded realtime queue is full.
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            let tx = events_tx.clone();
            let callbacks = callbacks.clone();
            let delivered_samples = delivered_samples.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    callbacks.fetch_add(1, Ordering::Relaxed);
                    let frame = downmix(data, channels, |s| s);
                    let len = frame.len() as u64;
                    if len == 0 {
                        return;
                    }
                    if tx.try_send(CaptureEvent::Samples(frame)).is_ok() {
                        delivered_samples.fetch_add(len, Ordering::Relaxed);
                    } else {
                        dropped_samples.fetch_add(len, Ordering::Relaxed);
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            let tx = events_tx.clone();
            let callbacks = callbacks.clone();
            let delivered_samples = delivered_samples.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    callbacks.fetch_add(1, Ordering::Relaxed);
                    let frame = downmix(data, channels, |s| s as f32 / i16::MAX as f32);
                    let len = frame.len() as u64;
                    if len == 0 {
                        return;
                    }
                    if tx.try_send(CaptureEvent::Samples(frame)).is_ok() {
                        delivered_samples.fetch_add(len, Ordering::Relaxed);
                    } else {
                        dropped_samples.fetch_add(len, Ordering::Relaxed);
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::U16 => {
            let tx = events_tx.clone();
            let callbacks = callbacks.clone();
            let delivered_samples = delivered_samples.clone();
            let dropped_samples = dropped_samples.clone();
            device.build_input_stream(
                &config.into(),
                move |data: &[u16], _| {
                    callbacks.fetch_add(1, Ordering::Relaxed);
                    let frame = downmix(data, channels, |s| {
                        (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0)
                    });
                    let len = frame.len() as u64;
                    if len == 0 {
                        return;
                    }
                    if tx.try_send(CaptureEvent::Samples(frame)).is_ok() {
                        delivered_samples.fetch_add(len, Ordering::Relaxed);
                    } else {
                        dropped_samples.fetch_add(len, Ordering::Relaxed);
                    }
                },
                err_fn,
                None,
            )?
        }
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };

    stream
        .play()
        .with_context(|| format!("could not start input stream for '{name}'"))?;
    let started = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) && !stream_failed.load(Ordering::Relaxed) {
        if delivered_samples.load(Ordering::Relaxed) == 0
            && started.elapsed() > std::time::Duration::from_secs(1)
        {
            warn!(
                "audio stream for '{name}' started but delivered no samples after {} callbacks ({} dropped samples); trying next input device",
                callbacks.load(Ordering::Relaxed),
                dropped_samples.load(Ordering::Relaxed)
            );
            stream_failed.store(true, Ordering::Relaxed);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(stream);
    if stream_failed.load(Ordering::Relaxed) {
        Err(anyhow!("input stream for '{name}' stopped"))
    } else {
        Ok(())
    }
}

/// Average interleaved channels down to a single mono track.
fn downmix<T: Copy>(data: &[T], channels: usize, to_f32: impl Fn(T) -> f32) -> Vec<f32> {
    if channels <= 1 {
        return data.iter().map(|&s| to_f32(s)).collect();
    }
    data.chunks(channels)
        .map(|frame| frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32)
        .collect()
}

/// Block-based linear resampler: native rate -> 16 kHz, mono `f32` -> `i16` LE bytes.
pub struct Resampler {
    ratio: f64, // out_rate / in_rate
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            ratio: out_rate as f64 / in_rate as f64,
        }
    }

    /// Resample one block and append the resulting s16le bytes to `out`.
    pub fn push(&self, input: &[f32], out: &mut Vec<u8>) {
        if input.is_empty() {
            return;
        }
        if (self.ratio - 1.0).abs() < f64::EPSILON {
            for &s in input {
                out.extend_from_slice(&to_i16(s).to_le_bytes());
            }
            return;
        }
        let out_len = (input.len() as f64 * self.ratio) as usize;
        for i in 0..out_len {
            let src = i as f64 / self.ratio;
            let idx = src.floor() as usize;
            let frac = (src - idx as f64) as f32;
            let a = input[idx.min(input.len() - 1)];
            let b = input[(idx + 1).min(input.len() - 1)];
            out.extend_from_slice(&to_i16(a + (b - a) * frac).to_le_bytes());
        }
    }
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_real_hardware_before_default_alias_and_noisy_alsa_pseudo_devices() {
        let mut names = [
            "surround51:CARD=PCH,DEV=0",
            "hw:CARD=USB,DEV=0",
            "default",
            "pipewire",
            "front:CARD=PCH,DEV=0",
            "plughw:CARD=USB,DEV=0",
        ];
        names.sort_by_key(|name| device_rank(name, None, Some("default"), 0));
        assert_eq!(names[0], "pipewire");
        assert_eq!(names[1], "plughw:CARD=USB,DEV=0");
        assert_eq!(names[2], "hw:CARD=USB,DEV=0");
        assert_eq!(names[3], "default");
        assert!(names[4].starts_with("surround") || names[4].starts_with("front"));
    }

    #[test]
    fn explicit_device_preference_wins_over_pipewire_and_default() {
        let preferred = device_rank("USB Audio Device", Some("usb audio"), Some("default"), 5);
        let pipewire = device_rank("pipewire", Some("usb audio"), Some("default"), 0);
        assert!(preferred < pipewire);
    }

    #[test]
    fn continuity_iphone_mic_ranks_as_real_capture_device() {
        let default_name = Some("MacBook Pro Microphone");
        // Both iPhone and Continuity-branded mics should rank above the generic
        // built-in catch-all (rank 5) and above the "default" alias (rank 6).
        let iphone = device_rank("Filip's iPhone Microphone", None, default_name, 3);
        let continuity = device_rank("Continuity Camera Microphone", None, default_name, 4);
        let builtin = device_rank("MacBook Pro Microphone", None, default_name, 0);
        let default_alias = device_rank("default", None, default_name, 1);
        assert!(
            iphone < builtin,
            "iPhone mic (rank {}) should beat generic built-in (rank {})",
            iphone.0,
            builtin.0
        );
        assert!(
            continuity < builtin,
            "Continuity mic (rank {}) should beat generic built-in (rank {})",
            continuity.0,
            builtin.0
        );
        assert!(
            iphone < default_alias,
            "iPhone mic (rank {}) should beat default alias (rank {})",
            iphone.0,
            default_alias.0
        );
    }

    #[test]
    fn audio_channel_is_bounded_to_drop_stale_realtime_audio() {
        assert!(recommended_audio_channel_capacity() > 0);
        assert!(recommended_audio_channel_capacity() <= 128);
    }
}
