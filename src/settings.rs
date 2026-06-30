//! Settings GUI: `dit settings` — eframe/egui window with General and Account tabs.
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
            .with_inner_size([480.0, 360.0])
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
    let path = crate::config::config_path()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string(layer)?)?;
    Ok(())
}

#[cfg(feature = "gui")]
#[derive(PartialEq, Eq)]
enum Tab {
    General,
    Account,
}

#[cfg(feature = "gui")]
struct SettingsApp {
    tab: Tab,
    language: String,
    hotkey: String,
    model: String,
    region: String,
    api_key: String,
    show_key: bool,
    status: String,
}

#[cfg(feature = "gui")]
impl SettingsApp {
    fn load() -> Self {
        use crate::config::{
            config_path, SettingsLayer, DEFAULT_HOTKEY, DEFAULT_LANGUAGE, DEFAULT_MODEL,
            DEFAULT_REGION,
        };
        let layer: SettingsLayer = config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            tab: Tab::General,
            language: layer.language.unwrap_or_else(|| DEFAULT_LANGUAGE.into()),
            hotkey: layer.hotkey.unwrap_or_else(|| DEFAULT_HOTKEY.into()),
            model: layer.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
            region: layer.region.unwrap_or_else(|| DEFAULT_REGION.into()),
            api_key: load_api_key(),
            show_key: false,
            status: String::new(),
        }
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
                                    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9",
                                    "F10", "F11", "F12",
                                ] {
                                    ui.selectable_value(
                                        &mut self.hotkey,
                                        key.to_string(),
                                        *key,
                                    );
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
                                    ui.selectable_value(
                                        &mut self.region,
                                        r.to_string(),
                                        *r,
                                    );
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
        }

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
