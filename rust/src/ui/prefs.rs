//! Window preferences. They belong to the UI, not the service: stored as JSON in
//! %APPDATA%\AstralGuard\ui.json on Windows, ~/.config/astralguard/ui.json elsewhere.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// A theme::PRESETS key.
    pub theme: String,
    /// "tray" hides the window, "exit" stops the guard (after asking).
    pub close_action: String,
    pub minimize_to_tray: bool,
    pub show_on_start: bool,
    pub always_on_top: bool,
    pub connector_mirror: bool,
    /// Outer position and inner size in points: [x, y, w, h].
    pub window: Option<[f32; 4]>,
    pub last_tab: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            theme: "dark".into(),
            close_action: "tray".into(),
            minimize_to_tray: false,
            show_on_start: false,
            always_on_top: false,
            connector_mirror: false,
            window: None,
            last_tab: "live".into(),
        }
    }
}

pub fn path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("AstralGuard").join("ui.json"))
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("astralguard").join("ui.json"))
    }
}

impl Prefs {
    pub fn load() -> Prefs {
        let p = path();
        // Saved under the app's earlier name (PinGuard, until 2026-11-09)?
        let legacy = p.as_ref().and_then(|p| {
            let dir = p.parent()?;
            let old = dir.with_file_name(if cfg!(windows) { "PinGuard" } else { "pinguard" }).join("ui.json");
            (!p.exists() && old.exists()).then_some(old)
        });
        legacy
            .or(p)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // write-then-rename: a crash mid-write must not lose the old file
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}
