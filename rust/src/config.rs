//! config.ini: the protection settings and the sag baseline, in one [Settings] section, so
//! an existing install keeps its values.
//!
//! Every bad value falls back to its own default rather than failing the file, except the
//! two core values (threshold, sustained time): if either is unusable, everything is
//! default, as before.

use std::fs;
use std::ops::RangeInclusive;
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

// Allowed overclock values, shared by loading and by `check`.
const OC_MV: RangeInclusive<f64> = 400.0..=1300.0;
const OC_TARGET_MHZ: RangeInclusive<f64> = 200.0..=4000.0;
const OC_POWER_W: RangeInclusive<f64> = 1.0..=2000.0;
const OC_MEM_MHZ: RangeInclusive<f64> = -10000.0..=10000.0;
const OC_DELAY_S: RangeInclusive<f64> = 0.0..=600.0;
/// The XBAR / SYS offset; the driver's own range is checked again before a write.
const OC_CLK_MHZ: RangeInclusive<f64> = -1000.0..=1000.0;
const OC_LOAD_W: RangeInclusive<f64> = 10.0..=2000.0;
const OC_LOAD_S: RangeInclusive<f64> = 1.0..=3600.0;

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

    // Overclock: a value that is missing takes its default; one that is present but out of
    // range falls back too, and also unticks the part it belongs to, so a damaged file never
    // applies a half-understood setting to the GPU.
    let oc = |key: &str, fallback: f64, range: &RangeInclusive<f64>| -> (f64, bool) {
        match ini.float(key) {
            None => (fallback, true),
            Some(Ok(v)) if range.contains(&v) => (v, true),
            _ => (fallback, false),
        }
    };
    let (from, from_ok) = oc("oc_uv_from_mv", d.oc_uv_from_mv, &OC_MV);
    let (to, to_ok) = oc("oc_uv_to_mv", d.oc_uv_to_mv, &OC_MV);
    let (target, target_ok) = oc("oc_uv_target_mhz", d.oc_uv_target_mhz, &OC_TARGET_MHZ);
    let uv_ok = from_ok && to_ok && target_ok && from < to;
    if uv_ok {
        (s.oc_uv_from_mv, s.oc_uv_to_mv, s.oc_uv_target_mhz) = (from, to, target);
    }
    s.oc_uv_enabled = uv_ok && ini.boolean("oc_uv_enabled").unwrap_or(d.oc_uv_enabled);
    let (power, power_ok) = oc("oc_power_w", d.oc_power_w, &OC_POWER_W);
    s.oc_power_w = power;
    s.oc_power_enabled = power_ok && ini.boolean("oc_power_enabled").unwrap_or(d.oc_power_enabled);
    let (mem, mem_ok) = oc("oc_mem_offset_mhz", d.oc_mem_offset_mhz, &OC_MEM_MHZ);
    s.oc_mem_offset_mhz = mem;
    s.oc_mem_enabled = mem_ok && ini.boolean("oc_mem_enabled").unwrap_or(d.oc_mem_enabled);
    s.oc_apply_delay_seconds = oc("oc_apply_delay_seconds", d.oc_apply_delay_seconds, &OC_DELAY_S).0;
    let (xbar, xbar_ok) = oc("oc_xbar_offset_mhz", d.oc_xbar_offset_mhz, &OC_CLK_MHZ);
    s.oc_xbar_offset_mhz = xbar;
    s.oc_xbar_enabled = xbar_ok && ini.boolean("oc_xbar_enabled").unwrap_or(d.oc_xbar_enabled);
    let (sys, sys_ok) = oc("oc_sys_offset_mhz", d.oc_sys_offset_mhz, &OC_CLK_MHZ);
    s.oc_sys_offset_mhz = sys;
    s.oc_sys_enabled = sys_ok && ini.boolean("oc_sys_enabled").unwrap_or(d.oc_sys_enabled);
    // A damaged load gate falls back to its defaults rather than unticking: the gate only ever
    // makes the offsets apply less often than "always".
    s.oc_clk_load_only = ini.boolean("oc_clk_load_only").unwrap_or(d.oc_clk_load_only);
    s.oc_clk_load_watts = oc("oc_clk_load_watts", d.oc_clk_load_watts, &OC_LOAD_W).0;
    s.oc_clk_load_seconds = oc("oc_clk_load_seconds", d.oc_clk_load_seconds, &OC_LOAD_S).0;
    s.oc_clk_idle_seconds = oc("oc_clk_idle_seconds", d.oc_clk_idle_seconds, &OC_LOAD_S).0;

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
    update(path, |ini| {
        ini.set(S, "i2c_adapter", c.adapter.clone());
        ini.set(S, "i2c_adapter_subsystem", format!("0x{:08X}", c.subsystem));
    })
}

/// Serializes every config.ini save in the process. Each one reads the whole file, changes its
/// keys and writes it back through the same temporary file, so two at once (the IPC thread
/// saving a setting while the monitor saves a driver check) would lose one's keys or fail the
/// rename.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read config.ini, change it, write it back, under `WRITE_LOCK`.
fn update(path: &Path, f: impl FnOnce(&mut Ini)) -> std::io::Result<()> {
    // A panic in another save leaves nothing half-done in memory: the file is the state.
    let _held = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ini = Ini::read(path);
    f(&mut ini);
    ini.write(path)
}

/// The driver version the overclock's curve write check last passed on
/// (`oc_vf_verified_driver`): the check runs again only after a driver update.
pub fn load_oc_verified(path: &Path) -> Option<String> {
    let v = Ini::read(path).get(S, "oc_vf_verified_driver")?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub fn save_oc_verified(path: &Path, driver: &str) -> std::io::Result<()> {
    update(path, |ini| {
        ini.set(S, "oc_vf_verified_driver", driver.to_string());
    })
}

/// The driver version the curve write check failed on with a wrong landing
/// (`oc_vf_failed_driver`): no further test writes on it, and no undervolt, until the driver
/// changes (or the key is removed by hand).
pub fn load_oc_vf_failed(path: &Path) -> Option<String> {
    let v = Ini::read(path).get(S, "oc_vf_failed_driver")?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub fn save_oc_vf_failed(path: &Path, driver: &str) -> std::io::Result<()> {
    update(path, |ini| {
        ini.set(S, "oc_vf_failed_driver", driver.to_string());
    })
}

/// The driver version the XBAR / SYS write check passed on (`oc_clk_verified_driver`), and
/// the one it failed on with a wrong landing (`oc_clk_failed_driver`: no XBAR / SYS write on
/// it until the driver changes or the key is removed by hand).
pub fn load_oc_clk_verified(path: &Path) -> Option<String> {
    let v = Ini::read(path).get(S, "oc_clk_verified_driver")?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub fn save_oc_clk_verified(path: &Path, driver: &str) -> std::io::Result<()> {
    update(path, |ini| {
        ini.set(S, "oc_clk_verified_driver", driver.to_string());
    })
}

pub fn load_oc_clk_failed(path: &Path) -> Option<String> {
    let v = Ini::read(path).get(S, "oc_clk_failed_driver")?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub fn save_oc_clk_failed(path: &Path, driver: &str) -> std::io::Result<()> {
    update(path, |ini| {
        ini.set(S, "oc_clk_failed_driver", driver.to_string());
    })
}

/// A float as config.ini stores it: "9.5", "15.0".
pub fn fmt_float(v: f64) -> String {
    format!("{v:?}")
}

fn fmt_bool(v: bool) -> String {
    if v { "True" } else { "False" }.to_string()
}

/// Write the given settings keys into config.ini, leaving every other key untouched.
pub fn save_settings(path: &Path, s: &Settings, keys: &[&str]) -> std::io::Result<()> {
    update(path, |ini| {
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
                "oc_uv_enabled" => fmt_bool(s.oc_uv_enabled),
                "oc_uv_from_mv" => fmt_float(s.oc_uv_from_mv),
                "oc_uv_to_mv" => fmt_float(s.oc_uv_to_mv),
                "oc_uv_target_mhz" => fmt_float(s.oc_uv_target_mhz),
                "oc_power_enabled" => fmt_bool(s.oc_power_enabled),
                "oc_power_w" => fmt_float(s.oc_power_w),
                "oc_mem_enabled" => fmt_bool(s.oc_mem_enabled),
                "oc_mem_offset_mhz" => fmt_float(s.oc_mem_offset_mhz),
                "oc_apply_delay_seconds" => fmt_float(s.oc_apply_delay_seconds),
                "oc_xbar_enabled" => fmt_bool(s.oc_xbar_enabled),
                "oc_xbar_offset_mhz" => fmt_float(s.oc_xbar_offset_mhz),
                "oc_sys_enabled" => fmt_bool(s.oc_sys_enabled),
                "oc_sys_offset_mhz" => fmt_float(s.oc_sys_offset_mhz),
                "oc_clk_load_only" => fmt_bool(s.oc_clk_load_only),
                "oc_clk_load_watts" => fmt_float(s.oc_clk_load_watts),
                "oc_clk_load_seconds" => fmt_float(s.oc_clk_load_seconds),
                "oc_clk_idle_seconds" => fmt_float(s.oc_clk_idle_seconds),
                _ => continue,
            };
            ini.set(S, key, value);
        }
    })
}

pub fn save_baseline(path: &Path, b: &SagBaseline) -> std::io::Result<()> {
    update(path, |ini| {
        ini.set(S, "sag_baseline", format!("{:.5},{:.5},{:.5}", b.vector[0], b.vector[1], b.vector[2]));
        ini.set(S, "sag_baseline_subsystem", format!("0x{:08X}", b.subsystem));
    })
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
        sensor_auto_scan,
        oc_uv_enabled,
        oc_uv_from_mv,
        oc_uv_to_mv,
        oc_uv_target_mhz,
        oc_power_enabled,
        oc_power_w,
        oc_mem_enabled,
        oc_mem_offset_mhz,
        oc_apply_delay_seconds,
        oc_xbar_enabled,
        oc_xbar_offset_mhz,
        oc_sys_enabled,
        oc_sys_offset_mhz,
        oc_clk_load_only,
        oc_clk_load_watts,
        oc_clk_load_seconds,
        oc_clk_idle_seconds
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
        sensor_auto_scan,
        oc_uv_enabled,
        oc_uv_from_mv,
        oc_uv_to_mv,
        oc_uv_target_mhz,
        oc_power_enabled,
        oc_power_w,
        oc_mem_enabled,
        oc_mem_offset_mhz,
        oc_apply_delay_seconds,
        oc_xbar_enabled,
        oc_xbar_offset_mhz,
        oc_sys_enabled,
        oc_sys_offset_mhz,
        oc_clk_load_only,
        oc_clk_load_watts,
        oc_clk_load_seconds,
        oc_clk_idle_seconds
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
    let ocs: [(&str, Option<f64>, &RangeInclusive<f64>, &str); 11] = [
        ("Undervolt start", p.oc_uv_from_mv, &OC_MV, "mV"),
        ("Undervolt end", p.oc_uv_to_mv, &OC_MV, "mV"),
        ("Undervolt target clock", p.oc_uv_target_mhz, &OC_TARGET_MHZ, "MHz"),
        ("Power limit", p.oc_power_w, &OC_POWER_W, "W"),
        ("Memory offset", p.oc_mem_offset_mhz, &OC_MEM_MHZ, "MHz"),
        ("Apply delay", p.oc_apply_delay_seconds, &OC_DELAY_S, "s"),
        ("XBAR offset", p.oc_xbar_offset_mhz, &OC_CLK_MHZ, "MHz"),
        ("SYS offset", p.oc_sys_offset_mhz, &OC_CLK_MHZ, "MHz"),
        ("Load threshold", p.oc_clk_load_watts, &OC_LOAD_W, "W"),
        ("Load time", p.oc_clk_load_seconds, &OC_LOAD_S, "s"),
        ("Idle time", p.oc_clk_idle_seconds, &OC_LOAD_S, "s"),
    ];
    for (name, value, range, unit) in ocs {
        if let Some(v) = value {
            if !range.contains(&v) {
                errors.push(format!("{name} must be between {} and {} {unit}", g(*range.start()), g(*range.end())));
            }
        }
    }
    if m.oc_uv_from_mv >= m.oc_uv_to_mv {
        errors.push("The undervolt range must start below where it ends".into());
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

    // Safety rule 6: a change that makes protection weaker than it is now asks first. Weaker
    // than the current settings, not than the defaults: loosening a value that was tightened
    // is still loosening. For every one of these, a higher number trips later.
    let c = current;
    let looser: [(&str, &str, Option<f64>, f64, f64); 9] = [
        ("threshold", "A", p.threshold_amps, c.threshold_amps, d.threshold_amps),
        (
            "sustained time",
            "s",
            p.sustained_seconds_required,
            c.sustained_seconds_required,
            d.sustained_seconds_required,
        ),
        ("tier 2 (critical) current", "A", p.critical_amps, c.critical_amps, d.critical_amps),
        ("tier 2 (critical) time", "s", p.critical_seconds, c.critical_seconds, d.critical_seconds),
        ("tier 3 (catastrophic) current", "A", p.catastrophic_amps, c.catastrophic_amps, d.catastrophic_amps),
        ("imbalance threshold", "%", p.imbalance_percent, c.imbalance_percent, d.imbalance_percent),
        ("imbalance max delta", "A", p.imbalance_delta_amps, c.imbalance_delta_amps, d.imbalance_delta_amps),
        ("imbalance minimum current", "A", p.imbalance_min_amps, c.imbalance_min_amps, d.imbalance_min_amps),
        (
            "imbalance sustained time",
            "s",
            p.imbalance_sustained_seconds,
            c.imbalance_sustained_seconds,
            d.imbalance_sustained_seconds,
        ),
    ];
    for (kind, unit, value, now, default) in looser {
        if let Some(v) = value {
            if v > now {
                warnings.push(format!(
                    "Raising the {kind} from {} {unit} to {} {unit} reduces protection and increases \
                     the risk of connector overheating, melting, or fire (the default is {} {unit}).",
                    g(now),
                    g(v),
                    g(default)
                ));
            }
        }
    }
    if c.imbalance_enabled && p.imbalance_enabled == Some(false) {
        warnings.push(
            "Turning the pin imbalance check off removes the early warning of a bad contact, which \
             often comes long before any pin runs over its limit."
                .into(),
        );
    }
    if c.imbalance_response == ImbalanceResponse::Act && p.imbalance_response == Some(ImbalanceResponse::Warn) {
        warnings.push("Switching pin imbalance to Warn only means an imbalance no longer limits the GPU.".into());
    }
    if c.sag_enabled && p.sag_enabled == Some(false) {
        warnings
            .push("Turning sag detection off removes the warning when the card sags and strains the connector.".into());
    }
    (errors, warnings)
}

/// A number with no trailing zeros ("{:g}").
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
    fn reads_existing_config() {
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
    fn oc_verified_driver_round_trips_and_keeps_other_keys() {
        let path = std::env::temp_dir().join(format!("ag-cfg-ocv-{}.ini", std::process::id()));
        fs::write(&path, "[Settings]\nfuture_key = x\n").unwrap();
        assert_eq!(load_oc_verified(&path), None);
        save_oc_verified(&path, "620.70.06").unwrap();
        assert_eq!(load_oc_verified(&path).as_deref(), Some("620.70.06"));
        assert!(fs::read_to_string(&path).unwrap().contains("future_key = x"));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn oc_vf_failed_driver_round_trips_next_to_the_verified_one() {
        let path = std::env::temp_dir().join(format!("ag-cfg-ocf-{}.ini", std::process::id()));
        fs::write(&path, "[Settings]\nfuture_key = x\n").unwrap();
        assert_eq!(load_oc_vf_failed(&path), None);
        save_oc_verified(&path, "620.70.06").unwrap();
        save_oc_vf_failed(&path, "621.01").unwrap();
        assert_eq!(load_oc_vf_failed(&path).as_deref(), Some("621.01"));
        assert_eq!(load_oc_verified(&path).as_deref(), Some("620.70.06"), "both kept");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("oc_vf_failed_driver = 621.01") && text.contains("future_key = x"), "{text}");
        fs::remove_file(&path).unwrap();
    }

    /// Every save is a read-modify-write of the whole file through one temporary file: two at
    /// once (the IPC thread saving a setting, the monitor saving a verified driver) must not lose
    /// either one's key.
    #[test]
    fn concurrent_saves_keep_every_key() {
        let path = std::env::temp_dir().join(format!("ag-cfg-race-{}.ini", std::process::id()));
        fs::write(&path, "[Settings]\n").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let p = path.clone();
                std::thread::spawn(move || {
                    for i in 0..20 {
                        match n % 4 {
                            0 => save_oc_verified(&p, &format!("v{i}")).unwrap(),
                            1 => save_oc_vf_failed(&p, &format!("f{i}")).unwrap(),
                            2 => {
                                let s = Settings { threshold_amps: 9.0, ..Settings::default() };
                                save_settings(&p, &s, &["threshold_amps"]).unwrap()
                            }
                            _ => save_baseline(&p, &SagBaseline { vector: [0.0, 0.0, 1.0], subsystem: 1 }).unwrap(),
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let ini = Ini::read(&path);
        for key in ["oc_vf_verified_driver", "oc_vf_failed_driver", "threshold_amps", "sag_baseline"] {
            assert!(ini.get(S, key).is_some(), "{key} lost: {}", ini.to_text());
        }
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn overclock_is_off_by_default_and_parses() {
        let d = from_ini(&Ini::parse("[Settings]\n")).0;
        assert!(!d.oc_uv_enabled && !d.oc_power_enabled && !d.oc_mem_enabled);
        assert_eq!(d.oc_apply_delay_seconds, 30.0);
        let s = from_ini(&Ini::parse(
            "[Settings]\noc_uv_enabled = True\noc_uv_from_mv = 820\noc_uv_to_mv = 895\noc_uv_target_mhz = 2827\n",
        ))
        .0;
        assert!(s.oc_uv_enabled);
        assert_eq!((s.oc_uv_from_mv, s.oc_uv_to_mv, s.oc_uv_target_mhz), (820.0, 895.0, 2827.0));
    }

    #[test]
    fn a_bad_overclock_value_leaves_that_setting_unmanaged() {
        let s = from_ini(&Ini::parse("[Settings]\noc_power_enabled = True\noc_power_w = lots\n")).0;
        assert!(!s.oc_power_enabled);
        let s = from_ini(&Ini::parse("[Settings]\noc_uv_enabled = True\noc_uv_from_mv = 900\noc_uv_to_mv = 850\n")).0;
        assert!(!s.oc_uv_enabled);
        let s = from_ini(&Ini::parse("[Settings]\noc_mem_enabled = True\noc_mem_offset_mhz = 99999\n")).0;
        assert!(!s.oc_mem_enabled);
        assert_eq!(s.oc_mem_offset_mhz, Settings::default().oc_mem_offset_mhz);
    }

    #[test]
    fn overclock_keys_round_trip_through_the_patch_and_the_file() {
        let p = SettingsPatch { oc_power_enabled: Some(true), oc_power_w: Some(480.0), ..Default::default() };
        assert_eq!(patch_keys(&p), vec!["oc_power_enabled", "oc_power_w"]);
        let m = merged(&Settings::default(), &p);
        assert!(m.oc_power_enabled && m.oc_power_w == 480.0);
        let path = std::env::temp_dir().join(format!("ag-oc-{}.ini", std::process::id()));
        save_settings(&path, &m, &patch_keys(&p)).unwrap();
        let back = load(&path).0;
        let _ = std::fs::remove_file(&path);
        assert!(back.oc_power_enabled && back.oc_power_w == 480.0);
    }

    #[test]
    fn check_rejects_an_empty_undervolt_range_and_a_negative_delay() {
        let base = Settings::default();
        let p = SettingsPatch { oc_uv_from_mv: Some(900.0), oc_uv_to_mv: Some(850.0), ..Default::default() };
        assert!(!check(&base, &p).0.is_empty());
        let p = SettingsPatch { oc_apply_delay_seconds: Some(-1.0), ..Default::default() };
        assert!(!check(&base, &p).0.is_empty());
        let p = SettingsPatch { oc_power_w: Some(5000.0), ..Default::default() };
        assert!(!check(&base, &p).0.is_empty());
        assert!(check(&base, &SettingsPatch::default()).0.is_empty());
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

    // Safety rule 6: the dashboard asks before applying a change that weakens protection.
    // These pin down which changes ask (a warning) and which are refused (an error).

    #[test]
    fn raising_any_tier_above_its_default_asks_first() {
        let d = Settings::default();
        let cases = [
            SettingsPatch { threshold_amps: Some(d.threshold_amps + 0.5), ..Default::default() },
            SettingsPatch {
                sustained_seconds_required: Some(d.sustained_seconds_required + 5.0),
                ..Default::default()
            },
            SettingsPatch { critical_amps: Some(d.critical_amps + 1.0), ..Default::default() },
            SettingsPatch { critical_seconds: Some(d.critical_seconds + 1.0), ..Default::default() },
            SettingsPatch { catastrophic_amps: Some(d.catastrophic_amps + 1.0), ..Default::default() },
        ];
        for p in cases {
            let (errors, warnings) = check(&d, &p);
            assert!(errors.is_empty(), "{p:?}: {errors:?}");
            assert_eq!(warnings.len(), 1, "{p:?}");
            assert!(warnings[0].contains("reduces protection") && warnings[0].contains(" from "), "{}", warnings[0]);
        }
        let all = SettingsPatch {
            threshold_amps: Some(10.0),
            sustained_seconds_required: Some(20.0),
            critical_amps: Some(14.0),
            critical_seconds: Some(5.0),
            catastrophic_amps: Some(18.0),
            ..Default::default()
        };
        assert_eq!(check(&d, &all).1.len(), 5, "one warning per weakened value");
    }

    #[test]
    fn tightening_or_keeping_the_defaults_does_not_ask() {
        let d = Settings::default();
        let tighter = SettingsPatch {
            threshold_amps: Some(8.5),
            sustained_seconds_required: Some(10.0),
            critical_amps: Some(12.0),
            critical_seconds: Some(2.0),
            catastrophic_amps: Some(15.0),
            ..Default::default()
        };
        assert_eq!(check(&d, &tighter), (vec![], vec![]));
        let same = SettingsPatch {
            threshold_amps: Some(d.threshold_amps),
            critical_amps: Some(d.critical_amps),
            ..Default::default()
        };
        assert_eq!(check(&d, &same), (vec![], vec![]), "exactly the default is not a weakening");
        assert_eq!(check(&d, &SettingsPatch::default()), (vec![], vec![]));
    }

    #[test]
    fn loosening_a_tightened_value_asks_even_below_the_default() {
        // From a tightened 8.5 A, going back up to 9.0 A is still a weakening, although 9.0 A
        // is under the 9.5 A default.
        let current = Settings { threshold_amps: 8.5, ..Settings::default() };
        let p = SettingsPatch { threshold_amps: Some(9.0), ..Default::default() };
        let warnings = check(&current, &p).1;
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("from 8.5 A to 9 A") && warnings[0].contains("default is 9.5 A"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn tightening_a_loosened_value_does_not_ask_even_above_the_default() {
        let current = Settings { threshold_amps: 11.0, critical_amps: 14.0, ..Settings::default() };
        let p = SettingsPatch { threshold_amps: Some(10.0), ..Default::default() };
        assert_eq!(check(&current, &p), (vec![], vec![]), "10 A is above the default but tighter than 11 A");
    }

    #[test]
    fn loosening_the_imbalance_check_asks_first() {
        let d = Settings::default();
        for p in [
            SettingsPatch { imbalance_percent: Some(d.imbalance_percent + 10.0), ..Default::default() },
            SettingsPatch { imbalance_delta_amps: Some(d.imbalance_delta_amps + 1.0), ..Default::default() },
            SettingsPatch { imbalance_min_amps: Some(d.imbalance_min_amps + 1.0), ..Default::default() },
            SettingsPatch {
                imbalance_sustained_seconds: Some(d.imbalance_sustained_seconds + 5.0),
                ..Default::default()
            },
            SettingsPatch { imbalance_enabled: Some(false), ..Default::default() },
        ] {
            let (errors, warnings) = check(&d, &p);
            assert!(errors.is_empty(), "{p:?}: {errors:?}");
            assert_eq!(warnings.len(), 1, "{p:?}");
        }
        let acting = Settings { imbalance_response: ImbalanceResponse::Act, ..d.clone() };
        let to_warn = SettingsPatch { imbalance_response: Some(ImbalanceResponse::Warn), ..Default::default() };
        assert!(check(&acting, &to_warn).1[0].contains("no longer limits the GPU"));
    }

    #[test]
    fn turning_sag_detection_off_asks_first() {
        let p = SettingsPatch { sag_enabled: Some(false), ..Default::default() };
        assert!(check(&Settings::default(), &p).1[0].contains("sag detection off"));
    }

    #[test]
    fn switching_checks_on_or_to_act_does_not_ask() {
        let off = Settings {
            imbalance_enabled: false,
            sag_enabled: false,
            imbalance_response: ImbalanceResponse::Warn,
            ..Settings::default()
        };
        let p = SettingsPatch {
            imbalance_enabled: Some(true),
            sag_enabled: Some(true),
            imbalance_response: Some(ImbalanceResponse::Act),
            imbalance_percent: Some(30.0),
            ..Default::default()
        };
        assert_eq!(check(&off, &p), (vec![], vec![]), "all of it is stricter");
        // Already off: setting off again is not a change.
        let again = SettingsPatch { imbalance_enabled: Some(false), sag_enabled: Some(false), ..Default::default() };
        assert!(check(&off, &again).1.is_empty());
    }

    #[test]
    fn tiers_out_of_order_are_refused() {
        let d = Settings::default();
        for p in [
            SettingsPatch { threshold_amps: Some(13.0), ..Default::default() }, // = tier 2
            SettingsPatch { critical_amps: Some(17.0), ..Default::default() },  // above tier 3
            SettingsPatch { catastrophic_amps: Some(9.0), ..Default::default() }, // below tier 1
        ] {
            let (errors, _) = check(&d, &p);
            assert!(errors.iter().any(|e| e.starts_with("Thresholds must rise")), "{p:?}: {errors:?}");
        }
    }

    #[test]
    fn numbers_that_make_no_sense_are_refused() {
        let d = Settings::default();
        for p in [
            SettingsPatch { threshold_amps: Some(f64::NAN), ..Default::default() },
            SettingsPatch { sustained_seconds_required: Some(0.0), ..Default::default() },
            SettingsPatch { critical_seconds: Some(-3.0), ..Default::default() },
            SettingsPatch { imbalance_percent: Some(100.0), ..Default::default() },
            SettingsPatch { sag_card_length_mm: Some(-1.0), ..Default::default() },
            SettingsPatch { sag_serious_degrees: Some(d.sag_warn_degrees), ..Default::default() },
            SettingsPatch { oc_xbar_offset_mhz: Some(5000.0), ..Default::default() },
        ] {
            assert!(!check(&d, &p).0.is_empty(), "{p:?} must be refused");
        }
        let spec_length = SettingsPatch { sag_card_length_mm: Some(0.0), ..Default::default() };
        assert!(check(&d, &spec_length).0.is_empty(), "0 mm means the card's spec length");
    }
}
