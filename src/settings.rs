//! Settings GUI: `dit settings` — eframe/egui window with General, Account,
//! Audio, and Models tabs.
//!
//! The window is a subprocess launched by the tray's "Settings…" item so it
//! does not share an event loop with the long-running dictation agent.

use anyhow::Result;

/// Extract `ELEVENLABS_API_KEY` from dotenv-style file contents.
#[cfg(any(feature = "gui", test))]
fn parse_api_key(contents: &str) -> String {
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "ELEVENLABS_API_KEY" {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Rewrite `key=value` in dotenv contents, appending when absent; preserves all other lines.
#[cfg(any(feature = "gui", test))]
fn patch_env_key(contents: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') {
            if let Some((k, _)) = trimmed.split_once('=') {
                if k.trim() == key {
                    out.push_str(&format!("{key}={value}\n"));
                    found = true;
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

// ── Headless stub ────────────────────────────────────────────────────────────

#[cfg(not(feature = "gui"))]
pub fn run() -> Result<()> {
    anyhow::bail!("`dit settings` requires a gui build; recompile with `--features gui`")
}

// ── GUI implementation ───────────────────────────────────────────────────────

#[cfg(feature = "gui")]
pub fn run() -> Result<()> {
    use eframe::egui;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("dit Settings")
            .with_inner_size([560.0, 440.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "dit Settings",
        options,
        Box::new(|_cc| Ok(Box::new(SettingsApp::load()))),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}

#[cfg(feature = "gui")]
fn env_file_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".dit.env"))
}

#[cfg(feature = "gui")]
fn load_api_key() -> String {
    env_file_path()
        .and_then(|p| std::fs::read_to_string(&p).ok())
        .map(|s| parse_api_key(&s))
        .unwrap_or_default()
}

#[cfg(feature = "gui")]
fn persist_api_key(key: &str) -> Result<()> {
    let path = env_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, patch_env_key(&existing, "ELEVENLABS_API_KEY", key))?;
    Ok(())
}

#[cfg(feature = "gui")]
fn persist_config(layer: &crate::config::SettingsLayer) -> Result<()> {
    let path = crate::config::config_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string(layer)?)?;
    Ok(())
}

// ── Audio helpers ─────────────────────────────────────────────────────────────

/// Return names of all available input devices.
#[cfg(feature = "gui")]
fn enumerate_audio_devices() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Spawn a background thread that captures audio and stores the current RMS
/// level (0.0–1.0) as f32 bits in `level`. Exits when `stop` is set.
#[cfg(feature = "gui")]
fn spawn_vu_thread(
    device_name: Option<String>,
    level: std::sync::Arc<std::sync::atomic::AtomicU32>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use std::sync::atomic::Ordering;

        let host = cpal::default_host();
        let device = match &device_name {
            Some(name) => {
                let lower = name.to_lowercase();
                host.input_devices()
                    .ok()
                    .and_then(|mut it| {
                        it.find(|d| d.name().map_or(false, |n| n.to_lowercase() == lower))
                    })
                    .or_else(|| host.default_input_device())
            }
            None => host.default_input_device(),
        };
        let device = match device {
            Some(d) => d,
            None => return,
        };
        let config = match device.default_input_config() {
            Ok(c) => c,
            Err(_) => return,
        };

        let channels = config.channels() as usize;
        let sample_format = config.sample_format();

        let stream: Option<cpal::Stream> = match sample_format {
            cpal::SampleFormat::F32 => {
                let lv = level.clone();
                let ch = channels;
                device
                    .build_input_stream(
                        &config.into(),
                        move |data: &[f32], _| {
                            lv.store(vu_rms_f32(data, ch).to_bits(), Ordering::Relaxed);
                        },
                        |_| {},
                        None,
                    )
                    .ok()
            }
            cpal::SampleFormat::I16 => {
                let lv = level.clone();
                let ch = channels;
                device
                    .build_input_stream(
                        &config.into(),
                        move |data: &[i16], _| {
                            let ch = ch.max(1);
                            let n = data.len() / ch;
                            if n == 0 {
                                return;
                            }
                            let sum_sq: f32 = data
                                .chunks(ch)
                                .map(|frame| {
                                    let m = frame
                                        .iter()
                                        .map(|&s| s as f32 / i16::MAX as f32)
                                        .sum::<f32>()
                                        / ch as f32;
                                    m * m
                                })
                                .sum();
                            let rms = (sum_sq / n as f32).sqrt().clamp(0.0, 1.0);
                            lv.store(rms.to_bits(), Ordering::Relaxed);
                        },
                        |_| {},
                        None,
                    )
                    .ok()
            }
            cpal::SampleFormat::U16 => {
                let lv = level.clone();
                let ch = channels;
                device
                    .build_input_stream(
                        &config.into(),
                        move |data: &[u16], _| {
                            let ch = ch.max(1);
                            let n = data.len() / ch;
                            if n == 0 {
                                return;
                            }
                            let sum_sq: f32 = data
                                .chunks(ch)
                                .map(|frame| {
                                    let m = frame
                                        .iter()
                                        .map(|&s| {
                                            (s as f32 - u16::MAX as f32 / 2.0)
                                                / (u16::MAX as f32 / 2.0)
                                        })
                                        .sum::<f32>()
                                        / ch as f32;
                                    m * m
                                })
                                .sum();
                            let rms = (sum_sq / n as f32).sqrt().clamp(0.0, 1.0);
                            lv.store(rms.to_bits(), Ordering::Relaxed);
                        },
                        |_| {},
                        None,
                    )
                    .ok()
            }
            _ => return,
        };

        let stream = match stream {
            Some(s) => s,
            None => return,
        };
        let _ = stream.play();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

/// RMS of interleaved f32 samples downmixed to mono, clamped to 0–1.
#[cfg(feature = "gui")]
fn vu_rms_f32(data: &[f32], channels: usize) -> f32 {
    let ch = channels.max(1);
    let n = data.len() / ch;
    if n == 0 {
        return 0.0;
    }
    let sum_sq: f32 = data
        .chunks(ch)
        .map(|frame| {
            let m = frame.iter().copied().sum::<f32>() / ch as f32;
            m * m
        })
        .sum();
    (sum_sq / n as f32).sqrt().clamp(0.0, 1.0)
}

// ── Tab / App types ───────────────────────────────────────────────────────────

#[cfg(feature = "gui")]
#[derive(PartialEq, Eq)]
enum Tab {
    General,
    Account,
    Audio,
    Models,
}

#[cfg(feature = "gui")]
struct SettingsApp {
    tab: Tab,
    // General
    language: String,
    hotkey: String,
    model: String,
    region: String,
    // Account
    api_key: String,
    show_key: bool,
    // Audio
    audio_devices: Vec<String>,
    selected_device: String,
    vu_level: std::sync::Arc<std::sync::atomic::AtomicU32>,
    vu_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Models: id -> op status message ("" = idle, "Downloading…", "Error: …")
    models_op: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    // Shared status line
    status: String,
}

#[cfg(feature = "gui")]
impl Drop for SettingsApp {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.vu_stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(feature = "gui")]
impl SettingsApp {
    fn load() -> Self {
        use crate::config::{
            config_path, SettingsLayer, DEFAULT_HOTKEY, DEFAULT_LANGUAGE, DEFAULT_MODEL,
            DEFAULT_REGION,
        };
        use std::sync::{
            atomic::{AtomicBool, AtomicU32},
            Arc,
        };

        let layer: SettingsLayer = config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();

        let selected_device = layer.device.clone().unwrap_or_default();
        let audio_devices = enumerate_audio_devices();
        let vu_level = Arc::new(AtomicU32::new(0));
        let vu_stop = Arc::new(AtomicBool::new(false));

        let prefer = if selected_device.is_empty() {
            None
        } else {
            Some(selected_device.clone())
        };
        spawn_vu_thread(prefer, vu_level.clone(), vu_stop.clone());

        Self {
            tab: Tab::General,
            language: layer.language.unwrap_or_else(|| DEFAULT_LANGUAGE.into()),
            hotkey: layer.hotkey.unwrap_or_else(|| DEFAULT_HOTKEY.into()),
            model: layer.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
            region: layer.region.unwrap_or_else(|| DEFAULT_REGION.into()),
            api_key: load_api_key(),
            show_key: false,
            audio_devices,
            selected_device,
            vu_level,
            vu_stop,
            models_op: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            status: String::new(),
        }
    }

    /// Stop the current VU capture thread and start a new one for the selected device.
    fn restart_vu(&mut self) {
        use std::sync::{
            atomic::{AtomicBool, AtomicU32, Ordering},
            Arc,
        };
        self.vu_stop.store(true, Ordering::Relaxed);
        self.vu_stop = Arc::new(AtomicBool::new(false));
        self.vu_level.store(0, Ordering::Relaxed);

        let prefer = if self.selected_device.is_empty() {
            None
        } else {
            Some(self.selected_device.clone())
        };
        spawn_vu_thread(prefer, self.vu_level.clone(), self.vu_stop.clone());
    }

    fn on_save(&mut self) {
        let result = match self.tab {
            Tab::General => {
                use crate::config::{config_path, SettingsLayer};
                let mut layer: SettingsLayer = config_path()
                    .and_then(|p| std::fs::read_to_string(&p).ok())
                    .and_then(|s| toml::from_str(&s).ok())
                    .unwrap_or_default();
                layer.language = Some(self.language.clone());
                layer.hotkey = Some(self.hotkey.clone());
                layer.model = Some(self.model.clone());
                layer.region = Some(self.region.clone());
                persist_config(&layer)
            }
            Tab::Account => persist_api_key(&self.api_key),
            Tab::Audio => {
                use crate::config::{config_path, SettingsLayer};
                let mut layer: SettingsLayer = config_path()
                    .and_then(|p| std::fs::read_to_string(&p).ok())
                    .and_then(|s| toml::from_str(&s).ok())
                    .unwrap_or_default();
                layer.device = if self.selected_device.is_empty() {
                    None
                } else {
                    Some(self.selected_device.clone())
                };
                persist_config(&layer)
            }
            Tab::Models => return,
        };
        self.status = match result {
            Ok(()) => "Saved.".into(),
            Err(e) => format!("Error: {e}"),
        };
    }
}

#[cfg(feature = "gui")]
impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        use eframe::egui;

        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::General, "General");
            ui.selectable_value(&mut self.tab, Tab::Account, "Account");
            ui.selectable_value(&mut self.tab, Tab::Audio, "Audio");
            ui.selectable_value(&mut self.tab, Tab::Models, "Models");
        });
        ui.separator();

        match self.tab {
            Tab::General => {
                egui::Grid::new("general")
                    .num_columns(2)
                    .spacing([40.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Language:");
                        ui.text_edit_singleline(&mut self.language);
                        ui.end_row();

                        ui.label("Hotkey:");
                        egui::ComboBox::from_id_salt("hotkey")
                            .selected_text(&self.hotkey)
                            .show_ui(ui, |ui| {
                                for key in &[
                                    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10",
                                    "F11", "F12",
                                ] {
                                    ui.selectable_value(&mut self.hotkey, key.to_string(), *key);
                                }
                            });
                        ui.end_row();

                        ui.label("Model:");
                        ui.text_edit_singleline(&mut self.model);
                        ui.end_row();

                        ui.label("Region:");
                        egui::ComboBox::from_id_salt("region")
                            .selected_text(&self.region)
                            .show_ui(ui, |ui| {
                                for r in &["global", "us", "eu", "in"] {
                                    ui.selectable_value(&mut self.region, r.to_string(), *r);
                                }
                            });
                        ui.end_row();
                    });
            }

            Tab::Account => {
                egui::Grid::new("account")
                    .num_columns(2)
                    .spacing([40.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("ElevenLabs API key:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.api_key)
                                .password(!self.show_key)
                                .desired_width(280.0),
                        );
                        ui.end_row();

                        ui.label("");
                        ui.checkbox(&mut self.show_key, "Show key");
                        ui.end_row();
                    });
            }

            Tab::Audio => {
                // Repaint continuously so the VU meter animates at ~20 fps.
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));

                let device_names = self.audio_devices.clone();
                let old_device = self.selected_device.clone();

                egui::Grid::new("audio")
                    .num_columns(2)
                    .spacing([40.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Input device:");
                        let display = if self.selected_device.is_empty() {
                            "System default".to_owned()
                        } else {
                            self.selected_device.clone()
                        };
                        egui::ComboBox::from_id_salt("audio_device")
                            .selected_text(display)
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_device,
                                    String::new(),
                                    "System default",
                                );
                                for name in &device_names {
                                    ui.selectable_value(
                                        &mut self.selected_device,
                                        name.clone(),
                                        name.as_str(),
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Level:");
                        use std::sync::atomic::Ordering;
                        let raw = f32::from_bits(self.vu_level.load(Ordering::Relaxed));
                        // Amplify for visual feedback (speech RMS is typically 0.01–0.1).
                        let visual = (raw * 12.0).clamp(0.0, 1.0);
                        let color = if visual > 0.85 {
                            egui::Color32::RED
                        } else if visual > 0.60 {
                            egui::Color32::from_rgb(0xff, 0xd0, 0x00)
                        } else {
                            egui::Color32::from_rgb(0x38, 0xe0, 0x38)
                        };
                        ui.add(
                            egui::ProgressBar::new(visual)
                                .fill(color)
                                .desired_width(300.0),
                        );
                        ui.end_row();

                        if device_names.is_empty() {
                            ui.label("");
                            ui.colored_label(egui::Color32::YELLOW, "No input devices found.");
                            ui.end_row();
                        }
                    });

                if self.selected_device != old_device {
                    self.restart_vu();
                }
            }

            Tab::Models => {
                let ops = self.models_op.clone();

                // Repaint while any operation is in progress.
                {
                    let has_active = ops.lock().unwrap().values().any(|s| s.ends_with('…'));
                    if has_active {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(500));
                    }
                }

                let catalog = crate::models::list_catalog();

                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        egui::Grid::new("models")
                            .num_columns(4)
                            .spacing([12.0, 6.0])
                            .striped(true)
                            .show(ui, |ui| {
                                ui.strong("ID");
                                ui.strong("Description");
                                ui.strong("Status");
                                ui.strong("Action");
                                ui.end_row();

                                for entry in &catalog {
                                    ui.label(entry.id);
                                    ui.label(entry.description);

                                    let op_msg = ops
                                        .lock()
                                        .unwrap()
                                        .get(entry.id)
                                        .cloned()
                                        .unwrap_or_default();

                                    let busy = op_msg.ends_with('…');

                                    if !op_msg.is_empty() {
                                        if op_msg.starts_with("Error") {
                                            ui.colored_label(egui::Color32::RED, &op_msg);
                                        } else {
                                            ui.label(&op_msg);
                                        }
                                    } else if entry.installed {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(0x38, 0xe0, 0x38),
                                            "installed",
                                        );
                                    } else {
                                        ui.label("not installed");
                                    }

                                    if entry.installed {
                                        if ui
                                            .add_enabled(!busy, egui::Button::new("Remove"))
                                            .clicked()
                                        {
                                            let id = entry.id.to_string();
                                            let ops2 = ops.clone();
                                            ops2.lock()
                                                .unwrap()
                                                .insert(id.clone(), "Removing…".into());
                                            std::thread::spawn(move || {
                                                let action = crate::config::ModelsAction::Rm {
                                                    id: id.clone(),
                                                };
                                                let msg = match crate::models::run(&action) {
                                                    Ok(()) => String::new(),
                                                    Err(e) => format!("Error: {e}"),
                                                };
                                                ops2.lock().unwrap().insert(id, msg);
                                            });
                                        }
                                    } else if busy {
                                        ui.add_enabled(false, egui::Button::new("Downloading…"));
                                    } else if ui.button("Download").clicked() {
                                        let id = entry.id.to_string();
                                        let ops2 = ops.clone();
                                        ops2.lock()
                                            .unwrap()
                                            .insert(id.clone(), "Downloading…".into());
                                        std::thread::spawn(move || {
                                            let action = crate::config::ModelsAction::Download {
                                                id: id.clone(),
                                            };
                                            let msg = match crate::models::run(&action) {
                                                Ok(()) => String::new(),
                                                Err(e) => format!("Error: {e}"),
                                            };
                                            ops2.lock().unwrap().insert(id, msg);
                                        });
                                    }

                                    ui.end_row();
                                }
                            });
                    });

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!(
                        "Models directory: {}",
                        crate::models::models_dir().display()
                    ))
                    .small()
                    .color(egui::Color32::GRAY),
                );
            }
        }

        // Save / status footer — not shown on the Models tab (operations apply immediately).
        if self.tab != Tab::Models {
            ui.add_space(12.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.on_save();
                }
                if !self.status.is_empty() {
                    ui.label(&self.status);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_extracted_from_env_file() {
        let env = "# comment\nELEVENLABS_API_KEY=sk_abc123\nOTHER=val\n";
        assert_eq!(parse_api_key(env), "sk_abc123");
    }

    #[test]
    fn api_key_empty_when_absent() {
        assert_eq!(parse_api_key("OTHER=x\n"), "");
    }

    #[test]
    fn api_key_trims_whitespace() {
        assert_eq!(parse_api_key("ELEVENLABS_API_KEY= sk_abc \n"), "sk_abc");
    }

    #[test]
    fn patch_env_key_replaces_existing() {
        let env = "ELEVENLABS_API_KEY=old\nOTHER=x\n";
        let out = patch_env_key(env, "ELEVENLABS_API_KEY", "new");
        assert!(out.contains("ELEVENLABS_API_KEY=new\n"), "got: {out}");
        assert!(!out.contains("old"), "got: {out}");
        assert!(out.contains("OTHER=x"), "got: {out}");
    }

    #[test]
    fn patch_env_key_appends_when_absent() {
        let env = "OTHER=x\n";
        let out = patch_env_key(env, "ELEVENLABS_API_KEY", "sk_new");
        assert!(out.contains("ELEVENLABS_API_KEY=sk_new\n"), "got: {out}");
        assert!(out.contains("OTHER=x"), "got: {out}");
    }

    #[test]
    fn patch_env_key_preserves_comments() {
        let env = "# My keys\nELEVENLABS_API_KEY=old\n";
        let out = patch_env_key(env, "ELEVENLABS_API_KEY", "new");
        assert!(out.starts_with("# My keys\n"), "got: {out}");
        assert!(out.contains("ELEVENLABS_API_KEY=new\n"), "got: {out}");
    }
}
