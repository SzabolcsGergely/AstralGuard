//! config.ini: the protection settings and the sag baseline, in the same [Settings] format
//! the Python guard wrote, so an existing install keeps its values.
//!
//! Every bad value falls back to its own default rather than failing the file, except the
//! two core values (threshold, sustained time): if either is unusable, everything is
//! default, as before.

use std::fs;
use std::path::{Path, PathBuf};

use crate::protocol::{ImbalanceResponse, ResponseMode, Settings, SettingsPatch};

/// Where config.ini, logs/ and the state files live. ASTRALGUARD_HOME overrides it.
/// Windows: %ProgramData%\AstralGuard, or next to the executable when a config.ini is
/// already there (a portable copy, and installs from before the installer existed).
/// Linux: /var/lib/astralguard (the unit's StateDirectory).
pub fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("ASTRALGUARD_HOME") {
        return PathBuf::from(home);
    }
    if cfg!(windows) {
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(dir) = exe_dir.filter(|d| d.join("config.ini").is_file()) {
            return dir;
        }
        let root = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(root).join(crate::protocol::APP_NAME)
    } else {
        PathBuf::from("/var/lib/astralguard")
    }
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.ini")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// A config.ini kept as text lines, so keys this version does not know (and comments)
/// survive a save.
#[derive(Default, Clone, Debug)]
pub struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    pub fn read(path: &Path) -> Ini {
        fs::read_to_string(path).map(|t| Ini::parse(&t)).unwrap_or_default()
    }

    pub fn parse(text: &str) -> Ini {
        let mut ini = Ini::default();
        let text = text.trim_start_matches('\u{feff}');
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                ini.sections.push((line[1..line.len() - 1].trim().to_string(), Vec::new()));
                continue;
            }
            let Some(pos) = line.find(['=', ':']) else { continue };
            let key = line[..pos].trim().to_ascii_lowercase();
            let value = line[pos + 1..].trim().to_string();
            if ini.sections.is_empty() {
                continue; // configparser rejects keys before a section; so do we
            }
            let entries = &mut ini.sections.last_mut().unwrap().1;
            match entries.iter_mut().find(|(k, _)| *k == key) {
                Some(e) => e.1 = value,
                None => entries.push((key, value)),
            }
        }
        ini
    }

    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(s, _)| s == section)
            .and_then(|(_, e)| e.iter().find(|(k, _)| k == key))
            .map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, section: &str, key: &str, value: String) {
        let idx = match self.sections.iter().position(|(s, _)| s == section) {
            Some(i) => i,
            None => {
                self.sections.push((section.to_string(), Vec::new()));
                self.sections.len() - 1
            }
        };
        let entries = &mut self.sections[idx].1;
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value,
            None => entries.push((key.to_string(), value)),
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (name, entries) in &self.sections {
            out.push_str(&format!("[{name}]\n"));
            for (k, v) in entries {
                out.push_str(&format!("{k} = {v}\n"));
            }
            out.push('\n');
        }
        out
    }

    /// Written to a temporary file and renamed into place: a crash mid-write must not
    /// leave a half-written config behind.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("ini.tmp");
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&tmp, self.to_text())?;
        fs::rename(&tmp, path)
    }

    fn float(&self, key: &str) -> Option<Result<f64, ()>> {
        self.get(S, key).map(|v| v.parse::<f64>().map_err(|_| ()))
    }

    fn boolean(&self, key: &str) -> Option<bool> {
        match self.get(S, key)?.to_ascii_lowercase().as_str() {
            "1" | "yes" | "true" | "on" => Some(true),
            "0" | "no" | "false" | "off" => Some(false),
            _ => None,
        }
    }
}

const S: &str = "Settings";

/// The sag reference: gravity's direction when calibrated, and the card it was taken on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SagBaseline {
    pub vector: [f64; 3],
    pub subsystem: u32,
}

pub fn load(path: &Path) -> (Settings, Option<SagBaseline>) {
    let ini = Ini::read(path);
    from_ini(&ini)
}

pub fn from_ini(ini: &Ini) -> (Settings, Option<SagBaseline>) {
    let d = Settings::default();
    let mut s = d.clone();

    let positive = |key: &str, fallback: f64| -> f64 {
        match ini.float(key) {
            Some(Ok(v)) if v > 0.0 && v.is_finite() => v,
            _ => fallback,
        }
    };

    // The two core values: unusable means the whole file is not trusted.
    let threshold = ini.float("threshold_amps").unwrap_or(Ok(d.threshold_amps));
    let sustained = ini.float("sustained_seconds_required").unwrap_or(Ok(d.sustained_seconds_required));
    match (threshold, sustained) {
        (Ok(t), Ok(su)) if t > 0.0 && su > 0.0 => {
            s.threshold_amps = t;
            s.sustained_seconds_required = su;
        }
        _ => return (d, None),
    }
    s.response_mode = ini.get(S, "response_mode").and_then(ResponseMode::parse).unwrap_or(d.response_mode);

    s.critical_amps = positive("critical_amps", d.critical_amps);
    s.critical_seconds = positive("critical_seconds", d.critical_seconds);
    s.catastrophic_amps = positive("catastrophic_amps", d.catastrophic_amps);
    s.mitigation_grace_seconds = positive("mitigation_grace_seconds", d.mitigation_grace_seconds);
    s.mitigation_clear_seconds = positive("mitigation_clear_seconds", d.mitigation_clear_seconds);
    s.mitigation_min_hold_seconds = positive("mitigation_min_hold_seconds", d.mitigation_min_hold_seconds);
    s.retrigger_window_seconds = positive("retrigger_window_seconds", d.retrigger_window_seconds);

    s.sag_warn_degrees = positive("sag_warn_degrees", d.sag_warn_degrees);
    s.sag_serious_degrees = positive("sag_serious_degrees", d.sag_serious_degrees);
    s.sag_sustained_seconds = positive("sag_sustained_seconds", d.sag_sustained_seconds);
    s.sag_card_length_mm = positive("sag_card_length_mm", d.sag_card_length_mm);
    if s.sag_serious_degrees <= s.sag_warn_degrees {
        s.sag_serious_degrees = s.sag_warn_degrees * 2.0;
    }
    s.sag_enabled = ini.boolean("sag_enabled").unwrap_or(d.sag_enabled);

    s.imbalance_min_amps = positive("imbalance_min_amps", d.imbalance_min_amps);
    s.imbalance_percent = positive("imbalance_percent", d.imbalance_percent);
    if s.imbalance_percent >= 100.0 {
        s.imbalance_percent = d.imbalance_percent;
    }
    s.imbalance_delta_amps = positive("imbalance_delta_amps", d.imbalance_delta_amps);
    s.imbalance_sustained_seconds = positive("imbalance_sustained_seconds", d.imbalance_sustained_seconds);
    s.imbalance_enabled = ini.boolean("imbalance_enabled").unwrap_or(d.imbalance_enabled);
    s.sensor_auto_scan = ini.boolean("sensor_auto_scan").unwrap_or(d.sensor_auto_scan);
    s.imbalance_response =
        ini.get(S, "imbalance_response").and_then(ImbalanceResponse::parse).unwrap_or(d.imbalance_response);

    (s, baseline_from_ini(ini))
}

fn baseline_from_ini(ini: &Ini) -> Option<SagBaseline> {
    let raw = ini.get(S, "sag_baseline")?.trim();
    let sub = ini.get(S, "sag_baseline_subsystem")?.trim();
    let parts: Vec<f64> = raw.split(',').map(|p| p.trim().parse::<f64>()).collect::<Result<_, _>>().ok()?;
    if parts.len() != 3 {
        return None;
    }
    let sub = u32::from_str_radix(sub.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()?;
    Some(SagBaseline { vector: [parts[0], parts[1], parts[2]], subsystem: sub })
}

/// The Linux I2C adapter `astralguard probe --detect` proved carries the IT8915, and the
/// card it was proved on. Written only by detection, never by the dashboard.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct I2cChoice {
    /// The driver's adapter name, e.g. "NVIDIA i2c adapter 3 at 1:00.0"
    pub adapter: String,
    pub subsystem: u32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn load_i2c_choice(path: &Path) -> Option<I2cChoice> {
    let ini = Ini::read(path);
    let adapter = ini.get(S, "i2c_adapter")?.trim().to_string();
    let sub = ini.get(S, "i2c_adapter_subsystem")?.trim();
    let subsystem = u32::from_str_radix(sub.trim_start_matches("0x").trim_start_matches("0X"), 16).ok()?;
    (!adapter.is_empty()).then_some(I2cChoice { adapter, subsystem })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn save_i2c_choice(path: &Path, c: &I2cChoice) -> std::io::Result<()> {
    let mut ini = Ini::read(path);
    ini.set(S, "i2c_adapter", c.adapter.clone());
    ini.set(S, "i2c_adapter_subsystem", format!("0x{:08X}", c.subsystem));
    ini.write(path)
}

/// Python's str(float): "9.5", "15.0".
pub fn fmt_float(v: f64) -> String {
    format!("{v:?}")
}

fn fmt_bool(v: bool) -> String {
    if v { "True" } else { "False" }.to_string()
}

/// Write the given settings keys into config.ini, leaving every other key untouched.
pub fn save_settings(path: &Path, s: &Settings, keys: &[&str]) -> std::io::Result<()> {
    let mut ini = Ini::read(path);
    for &key in keys {
        let value = match key {
            "threshold_amps" => fmt_float(s.threshold_amps),
            "sustained_seconds_required" => fmt_float(s.sustained_seconds_required),
            "response_mode" => s.response_mode.key().to_string(),
            "critical_amps" => fmt_float(s.critical_amps),
            "critical_seconds" => fmt_float(s.critical_seconds),
            "catastrophic_amps" => fmt_float(s.catastrophic_amps),
            "sag_enabled" => fmt_bool(s.sag_enabled),
            "sag_warn_degrees" => fmt_float(s.sag_warn_degrees),
            "sag_serious_degrees" => fmt_float(s.sag_serious_degrees),
            "sag_sustained_seconds" => fmt_float(s.sag_sustained_seconds),
            "sag_card_length_mm" => fmt_float(s.sag_card_length_mm),
            "imbalance_enabled" => fmt_bool(s.imbalance_enabled),
            "sensor_auto_scan" => fmt_bool(s.sensor_auto_scan),
            "imbalance_min_amps" => fmt_float(s.imbalance_min_amps),
            "imbalance_percent" => fmt_float(s.imbalance_percent),
            "imbalance_delta_amps" => fmt_float(s.imbalance_delta_amps),
            "imbalance_sustained_seconds" => fmt_float(s.imbalance_sustained_seconds),
            "imbalance_response" => s.imbalance_response.key().to_string(),
            _ => continue,
        };
        ini.set(S, key, value);
    }
    ini.write(path)
}

pub fn save_baseline(path: &Path, b: &SagBaseline) -> std::io::Result<()> {
    let mut ini = Ini::read(path);
    ini.set(S, "sag_baseline", format!("{:.5},{:.5},{:.5}", b.vector[0], b.vector[1], b.vector[2]));
    ini.set(S, "sag_baseline_subsystem", format!("0x{:08X}", b.subsystem));
    ini.write(path)
}

/// The keys a patch touches, in config.ini spelling.
pub fn patch_keys(p: &SettingsPatch) -> Vec<&'static str> {
    let mut keys = Vec::new();
    macro_rules! key {
        ($($f:ident),*) => { $( if p.$f.is_some() { keys.push(stringify!($f)); } )* };
    }
    key!(
        threshold_amps,
        sustained_seconds_required,
        response_mode,
        critical_amps,
        critical_seconds,
        catastrophic_amps,
        sag_enabled,
        sag_warn_degrees,
        sag_serious_degrees,
        sag_sustained_seconds,
        sag_card_length_mm,
        imbalance_enabled,
        imbalance_min_amps,
        imbalance_percent,
        imbalance_delta_amps,
        imbalance_sustained_seconds,
        imbalance_response,
        sensor_auto_scan
    );
    keys
}

/// The settings with a patch applied (no validation).
pub fn merged(s: &Settings, p: &SettingsPatch) -> Settings {
    let mut m = s.clone();
    macro_rules! take {
        ($($f:ident),*) => { $( if let Some(v) = p.$f { m.$f = v; } )* };
    }
    take!(
        threshold_amps,
        sustained_seconds_required,
        response_mode,
        critical_amps,
        critical_seconds,
        catastrophic_amps,
        sag_enabled,
        sag_warn_degrees,
        sag_serious_degrees,
        sag_sustained_seconds,
        sag_card_length_mm,
        imbalance_enabled,
        imbalance_min_amps,
        imbalance_percent,
        imbalance_delta_amps,
        imbalance_sustained_seconds,
        imbalance_response,
        sensor_auto_scan
    );
    m
}

/// (errors, warnings). Errors block the change; warnings are the reduced-protection
/// confirmations: raising a current or a time above its default.
pub fn check(current: &Settings, p: &SettingsPatch) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let m = merged(current, p);
    let d = Settings::default();

    let floats: [(&str, Option<f64>); 13] = [
        ("threshold amps", p.threshold_amps),
        ("sustained seconds required", p.sustained_seconds_required),
        ("critical amps", p.critical_amps),
        ("critical seconds", p.critical_seconds),
        ("catastrophic amps", p.catastrophic_amps),
        ("imbalance percent", p.imbalance_percent),
        ("imbalance delta amps", p.imbalance_delta_amps),
        ("imbalance min amps", p.imbalance_min_amps),
        ("imbalance sustained seconds", p.imbalance_sustained_seconds),
        ("sag warn degrees", p.sag_warn_degrees),
        ("sag serious degrees", p.sag_serious_degrees),
        ("sag sustained seconds", p.sag_sustained_seconds),
        ("sag card length mm", p.sag_card_length_mm),
    ];
    for (name, value) in floats {
        let Some(v) = value else { continue };
        if !v.is_finite() {
            errors.push(format!("{name} must be a number"));
        } else if name == "sag card length mm" {
            if v < 0.0 {
                errors.push("Card length cannot be negative (0 = from the card's spec)".into());
            }
        } else if v <= 0.0 {
            errors.push(format!("{name} must be greater than 0"));
        }
    }
    if p.imbalance_percent.is_some_and(|v| v >= 100.0) {
        errors.push("Imbalance threshold must be below 100%".into());
    }
    if m.sag_serious_degrees <= m.sag_warn_degrees {
        errors.push("The serious sag angle must be larger than the warning angle".into());
    }
    // Out of order, a lower tier would be skipped or a higher one would fire first.
    if !(m.threshold_amps < m.critical_amps && m.critical_amps < m.catastrophic_amps) {
        errors.push(format!(
            "Thresholds must rise: threshold ({} A) < tier 2 ({} A) < tier 3 ({} A)",
            g(m.threshold_amps),
            g(m.critical_amps),
            g(m.catastrophic_amps)
        ));
    }

    let raised: [(&str, &str, Option<f64>, f64); 5] = [
        ("threshold", "A", p.threshold_amps, d.threshold_amps),
        ("sustained time", "s", p.sustained_seconds_required, d.sustained_seconds_required),
        ("tier 2 (critical) current", "A", p.critical_amps, d.critical_amps),
        ("tier 2 (critical) time", "s", p.critical_seconds, d.critical_seconds),
        ("tier 3 (catastrophic) current", "A", p.catastrophic_amps, d.catastrophic_amps),
    ];
    for (kind, unit, value, default) in raised {
        if let Some(v) = value {
            if v > default {
                warnings.push(format!(
                    "Raising the {kind} above the default ({} {unit}) reduces protection and \
                     increases the risk of connector overheating, melting, or fire. \
                     Requested: {} {unit}.",
                    g(default),
                    g(v)
                ));
            }
        }
    }
    (errors, warnings)
}

/// Python's "{:g}": no trailing zeros.
pub fn g(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_python_config() {
        let ini = Ini::parse(
            "[Settings]\nthreshold_amps = 9.0\nsustained_seconds_required = 15.0\n\
             response_mode = shutdown_only\nsag_enabled = True\nsag_warn_degrees = 3\n\
             sag_serious_degrees = 2\nimbalance_percent = 120\nfuture_key = x\n\
             sag_baseline = -0.00855,-0.00806,0.99993\nsag_baseline_subsystem = 0x89E31043\n",
        );
        let (s, b) = from_ini(&ini);
        assert_eq!(s.threshold_amps, 9.0);
        assert_eq!(s.response_mode, ResponseMode::ShutdownOnly);
        assert_eq!(s.sag_serious_degrees, 6.0); // serious <= warn -> warn * 2
        assert_eq!(s.imbalance_percent, 40.0); // >= 100 -> default
        let b = b.unwrap();
        assert_eq!(b.subsystem, 0x89E31043);
        assert!((b.vector[2] - 0.99993).abs() < 1e-9);
    }

    #[test]
    fn bad_core_value_means_defaults() {
        let (s, b) = from_ini(&Ini::parse("[Settings]\nthreshold_amps = -1\n"));
        assert_eq!(s, Settings::default());
        assert!(b.is_none());
    }

    #[test]
    fn i2c_choice_round_trips_and_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("ag-cfg-i2c-{}", std::process::id()));
        let path = dir.join("config.ini");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "[Settings]\nfuture_key = x\nthreshold_amps = 9.0\n").unwrap();
        assert_eq!(load_i2c_choice(&path), None);
        let c = I2cChoice { adapter: "NVIDIA i2c adapter 3 at 1:00.0".into(), subsystem: 0x89E31043 };
        save_i2c_choice(&path, &c).unwrap();
        assert_eq!(load_i2c_choice(&path), Some(c));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("future_key = x") && text.contains("threshold_amps = 9.0"));
        assert_eq!(load(&path).0.threshold_amps, 9.0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sensor_auto_scan_is_off_unless_set() {
        assert!(!from_ini(&Ini::parse("[Settings]\n")).0.sensor_auto_scan);
        assert!(from_ini(&Ini::parse("[Settings]\nsensor_auto_scan = True\n")).0.sensor_auto_scan);
        assert!(!from_ini(&Ini::parse("[Settings]\nsensor_auto_scan = maybe\n")).0.sensor_auto_scan);
        let on = Settings { sensor_auto_scan: true, ..Default::default() };
        let p = SettingsPatch { sensor_auto_scan: Some(true), ..Default::default() };
        assert_eq!(patch_keys(&p), vec!["sensor_auto_scan"]);
        assert_eq!(merged(&Settings::default(), &p), on);
    }

    #[test]
    fn save_keeps_unknown_keys() {
        let mut ini = Ini::parse("[Settings]\nfuture_key = x\nthreshold_amps = 9.5\n");
        ini.set(S, "threshold_amps", fmt_float(8.0));
        let text = ini.to_text();
        assert!(text.contains("future_key = x"));
        assert!(text.contains("threshold_amps = 8.0"));
    }

    #[test]
    fn check_orders_tiers_and_warns_on_raise() {
        let s = Settings::default();
        let p = SettingsPatch { threshold_amps: Some(14.0), ..Default::default() };
        let (errors, warnings) = check(&s, &p);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(warnings.len(), 1);
        let p = SettingsPatch { threshold_amps: Some(9.0), ..Default::default() };
        let (errors, warnings) = check(&s, &p);
        assert!(errors.is_empty() && warnings.is_empty());
    }

    #[test]
    fn g_format() {
        assert_eq!(g(9.5), "9.5");
        assert_eq!(g(15.0), "15");
        assert_eq!(g(0.25), "0.25");
    }
}
