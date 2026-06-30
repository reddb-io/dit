//! Settings GUI — opened by `dit settings` or the tray "Settings…" item.
//!
//! Reads `~/.dit/config.toml` and `~/.dit.env` on launch; writes them on Save.
//! Non-GUI fields in config.toml (e.g. `session_max_age_days`) are round-tripped
//! unchanged so nothing is silently lost.

use std::path::PathBuf;

use anyhow::Result;
use eframe::egui;

use crate::config::{config_path, default_env_path, load_file_config, save_config, SettingsLayer};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    General,
    Account,
}

struct SettingsApp {
    active_tab: Tab,
    // General tab
    language: String,
    hotkey: String,
    model: String,
    no_filler: bool,
    // Account tab
    api_key: String,
    api_key_visible: bool,
    // Paths
    config_file_path: Option<PathBuf>,
    env_file_path: Option<PathBuf>,
    // Full layer loaded at startup — lets us round-trip fields not shown in the GUI.
    base_layer: SettingsLayer,
    // Status bar
    status: Option<String>,
}

impl SettingsApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config_file_path = config_path();
        let base_layer = config_file_path
            .as_ref()
            .map(|p| load_file_config(p))
            .unwrap_or_default();

        let env_file_path = default_env_path();
        let api_key = load_api_key(&env_file_path);

        Self {
            active_tab: Tab::General,
            language: base_layer.language.clone().unwrap_or_else(|| "pt".into()),
            hotkey: base_layer.hotkey.clone().unwrap_or_else(|| "F9".into()),
            model: base_layer
                .model
                .clone()
                .unwrap_or_else(|| "scribe_v2_realtime".into()),
            no_filler: base_layer.no_filler.unwrap_or(false),
            api_key,
            api_key_visible: false,
            config_file_path,
            env_file_path,
            base_layer,
            status: None,
        }
    }

    fn save(&self) -> Result<()> {
        // Overlay GUI-controlled fields onto the full base layer to preserve
        // everything else (session_max_age_days, vad_silence, …).
        let mut layer = self.base_layer.clone();
        layer.language = Some(self.language.clone());
        layer.hotkey = Some(self.hotkey.clone());
        layer.model = Some(self.model.clone());
        layer.no_filler = Some(self.no_filler);

        if let Some(path) = &self.config_file_path {
            save_config(path, &layer)?;
        }
        if let Some(path) = &self.env_file_path {
            save_api_key(path, &self.api_key)?;
        }
        Ok(())
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, Tab::General, "General");
                ui.selectable_value(&mut self.active_tab, Tab::Account, "Account");
            });
            ui.separator();

            match self.active_tab {
                Tab::General => {
                    egui::Grid::new("general")
                        .num_columns(2)
                        .spacing([20.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Language:");
                            ui.text_edit_singleline(&mut self.language);
                            ui.end_row();

                            ui.label("Hotkey:");
                            egui::ComboBox::new("hotkey_cb", "")
                                .selected_text(&self.hotkey)
                                .show_ui(ui, |ui| {
                                    for fk in [
                                        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9",
                                        "F10", "F11", "F12",
                                    ] {
                                        ui.selectable_value(
                                            &mut self.hotkey,
                                            fk.to_string(),
                                            fk,
                                        );
                                    }
                                });
                            ui.end_row();

                            ui.label("Engine:");
                            ui.text_edit_singleline(&mut self.model);
                            ui.end_row();

                            ui.label("Mode:");
                            ui.checkbox(
                                &mut self.no_filler,
                                "Remove filler words (uh, um…)",
                            );
                            ui.end_row();
                        });
                }
                Tab::Account => {
                    egui::Grid::new("account")
                        .num_columns(2)
                        .spacing([20.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("ElevenLabs API Key:");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.api_key)
                                        .password(!self.api_key_visible)
                                        .desired_width(260.0),
                                );
                                let lbl = if self.api_key_visible { "Hide" } else { "Show" };
                                if ui.small_button(lbl).clicked() {
                                    self.api_key_visible = !self.api_key_visible;
                                }
                            });
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    let env_display = self
                        .env_file_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "~/.dit.env".into());
                    ui.label(
                        egui::RichText::new(format!("Saved to: {env_display}")).weak(),
                    );
                }
            }

            ui.add_space(8.0);
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    match self.save() {
                        Ok(()) => self.status = Some("Saved.".into()),
                        Err(e) => self.status = Some(format!("Error: {e}")),
                    }
                }
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if let Some(msg) = &self.status {
                    ui.label(msg);
                }
            });
        });
    }
}

fn load_api_key(env_path: &Option<PathBuf>) -> String {
    if let Some(path) = env_path {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if let Some(val) = line.trim().strip_prefix("ELEVENLABS_API_KEY=") {
                    return val.trim().to_string();
                }
            }
        }
    }
    std::env::var("ELEVENLABS_API_KEY").unwrap_or_default()
}

fn save_api_key(path: &PathBuf, api_key: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("ELEVENLABS_API_KEY=") {
                found = true;
                format!("ELEVENLABS_API_KEY={api_key}")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("ELEVENLABS_API_KEY={api_key}"));
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("dit — Settings")
            .with_inner_size([500.0, 280.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "dit Settings",
        options,
        Box::new(|cc| Box::new(SettingsApp::new(cc))),
    )
    .map_err(|e| anyhow::anyhow!("settings window error: {e}"))?;
    Ok(())
}
