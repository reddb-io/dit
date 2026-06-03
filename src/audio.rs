//! Cross-platform microphone capture (replaces `parec` from the original script).
//!
//! `cpal` opens the default (or named) input device at whatever native rate it
//! prefers. The capture runs on its own thread because a `cpal::Stream` is not
//! `Send`; it pushes mono `f32` frames into a Tokio channel, and reports the
//! device's sample rate back so the sender side can resample to 16 kHz.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tracing::{error, info, warn};

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

fn pick_device(prefer: &Option<String>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(needle) = prefer {
        let needle = needle.to_lowercase();
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if let Ok(name) = device.name() {
                    if name.to_lowercase().contains(&needle) {
                        return Ok(device);
                    }
                }
            }
        }
        warn!("device matching '{needle}' not found, falling back to default");
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("no input device available"))
}

/// Spawn the capture thread. Returns once the stream is live (or errors).
///
/// * `samples_tx` receives mono `f32` chunks at the device's native rate.
/// * `rate_tx` is fired once with that native sample rate.
/// * Setting `stop` tears the stream down.
pub fn spawn_capture(
    prefer: Option<String>,
    stop: Arc<AtomicBool>,
    samples_tx: UnboundedSender<Vec<f32>>,
    rate_tx: oneshot::Sender<u32>,
) {
    std::thread::spawn(move || {
        if let Err(e) = run_capture(prefer, stop, samples_tx, rate_tx) {
            error!("audio capture failed: {e:#}");
        }
    });
}

fn run_capture(
    prefer: Option<String>,
    stop: Arc<AtomicBool>,
    samples_tx: UnboundedSender<Vec<f32>>,
    rate_tx: oneshot::Sender<u32>,
) -> Result<()> {
    let device = pick_device(&prefer)?;
    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let config = device.default_input_config()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("capturing from '{name}' @ {sample_rate} Hz, {channels} ch");
    let _ = rate_tx.send(sample_rate);

    let err_fn = |e| error!("audio stream error: {e}");
    let tx = samples_tx.clone();

    // Downmix interleaved frames to mono and forward.
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                tx.send(downmix(data, channels, |s| s)).ok();
            },
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _| {
                tx.send(downmix(data, channels, |s| s as f32 / i16::MAX as f32))
                    .ok();
            },
            err_fn,
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            &config.into(),
            move |data: &[u16], _| {
                tx.send(downmix(data, channels, |s| {
                    (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0)
                }))
                .ok();
            },
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };

    stream.play()?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(stream);
    Ok(())
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
