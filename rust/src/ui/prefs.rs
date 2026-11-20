//! Window preferences. They belong to the UI, not the service: stored as JSON in
//! %APPDATA%\AstralGuard\ui.json on Windows, ~/.config/astralguard/ui.json elsewhere.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Ask GitHub once a day whether a newer release exists.
    pub check_updates: bool,
    /// A release the user chose to skip: no banner for it.
    pub skipped_update: String,
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
            check_updates: true,
            skipped_update: String::new(),
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
        path().map(|p| Prefs::load_from(&p)).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            self.save_to(&p);
        }
    }

    /// Missing or unreadable: the defaults. A field it doesn't have keeps its default.
    fn load_from(p: &Path) -> Prefs {
        std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_to(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            // write-then-rename: a crash mid-write must not lose the old file
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ag-prefs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn saved_prefs_load_back() {
        let dir = temp("roundtrip");
        let p = dir.join("AstralGuard").join("ui.json");
        let prefs = Prefs {
            theme: "oled".into(),
            close_action: "exit".into(),
            always_on_top: true,
            window: Some([10.0, 20.0, 800.0, 820.0]),
            last_tab: "overclock".into(),
            check_updates: false,
            skipped_update: "1.2.0".into(),
            ..Prefs::default()
        };
        prefs.save_to(&p);
        assert!(!p.with_extension("json.tmp").exists(), "written whole, then renamed");
        assert_eq!(Prefs::load_from(&p), prefs);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_broken_prefs_are_the_defaults() {
        let dir = temp("broken");
        let p = dir.join("ui.json");
        assert_eq!(Prefs::load_from(&p), Prefs::default());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, "{\"theme\": ").unwrap();
        assert_eq!(Prefs::load_from(&p), Prefs::default());
        let d = Prefs::default();
        assert_eq!((d.theme.as_str(), d.close_action.as_str(), d.last_tab.as_str()), ("dark", "tray", "live"));
        assert!(d.check_updates, "update checks are on unless turned off");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_older_ui_json_keeps_its_values_and_gets_defaults_for_the_rest() {
        let dir = temp("older");
        let p = dir.join("ui.json");
        std::fs::create_dir_all(&dir).unwrap();
        // Written before check_updates / skipped_update existed, with a key since dropped.
        std::fs::write(&p, r#"{"theme": "light", "last_tab": "imu", "old_key": 3}"#).unwrap();
        let loaded = Prefs::load_from(&p);
        assert_eq!((loaded.theme.as_str(), loaded.last_tab.as_str()), ("light", "imu"));
        assert!(loaded.check_updates);
        assert_eq!(loaded.close_action, "tray");
        let _ = std::fs::remove_dir_all(dir);
    }
}
