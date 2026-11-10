//! Everything the service and the UI say to each other.
//!
//! One JSON object per line over a local socket: the UI sends a [`Request`], the service
//! answers with one [`Response`]. The UI polls; the service never pushes. Notifications
//! the UI should show (Linux, where a root service cannot reach the desktop) come back
//! through [`Request::Events`].

use serde::{Deserialize, Serialize};

pub const APP_NAME: &str = "AstralGuard";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Ok,
    #[default]
    Missing,
    Over,
    Mitigated,
    Paused,
    Error,
    Shutdown,
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
impl Status {
    /// Tray and dashboard colour, RGB. Red only ever means "we shut the machine down".
    pub fn color(self) -> [u8; 3] {
        match self {
            Status::Ok => [0, 200, 0],
            Status::Missing => [160, 160, 160],
            Status::Over => [255, 170, 0],
            Status::Mitigated => [255, 100, 0],
            Status::Paused => [70, 140, 255],
            Status::Error => [150, 70, 220],
            Status::Shutdown => [230, 0, 0],
        }
    }

    /// How safe the connector is, in one word: the label that goes with the colour.
    pub fn text(self) -> &'static str {
        match self {
            Status::Ok => "Safe",
            Status::Over => "Warning",
            Status::Mitigated => "Protecting",
            Status::Shutdown => "Danger",
            Status::Paused => "Unprotected",
            // No readings: whether it is safe cannot be told.
            Status::Error | Status::Missing => "Unknown",
        }
    }

    /// Why, in a few words.
    pub fn detail(self) -> &'static str {
        match self {
            Status::Ok => "all pins within limits",
            Status::Over => "over threshold",
            Status::Mitigated => "GPU limited",
            Status::Shutdown => "shutting down",
            Status::Paused => "monitoring paused",
            Status::Error => "waiting for pin sensors",
            Status::Missing => "no pin readings",
        }
    }
}

/// tiered: slow the GPU down first, shut down only if that does not fix it.
/// shutdown_only: no GPU limiting, the tier timings still apply.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    #[default]
    Tiered,
    ShutdownOnly,
}

impl ResponseMode {
    pub fn key(self) -> &'static str {
        match self {
            ResponseMode::Tiered => "tiered",
            ResponseMode::ShutdownOnly => "shutdown_only",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tiered" => Some(ResponseMode::Tiered),
            "shutdown_only" => Some(ResponseMode::ShutdownOnly),
            _ => None,
        }
    }
}

/// warn: notify and log only. act: a tripped imbalance enters the response ladder as a
/// tier 1 crossing and gets exactly what sustained overcurrent gets.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImbalanceResponse {
    #[default]
    Warn,
    Act,
}

impl ImbalanceResponse {
    pub fn key(self) -> &'static str {
        match self {
            ImbalanceResponse::Warn => "warn",
            ImbalanceResponse::Act => "act",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "warn" => Some(ImbalanceResponse::Warn),
            "act" => Some(ImbalanceResponse::Act),
            _ => None,
        }
    }
}

/// Every protection value. Lives in the service; the UI gets a copy in [`Live`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    // tier 1: elevated current, ridden out for a while before acting
    pub threshold_amps: f64,
    pub sustained_seconds_required: f64,
    pub response_mode: ResponseMode,
    // tiers 2 and 3: currents too high to sit through the tier 1 window
    pub critical_amps: f64,
    pub critical_seconds: f64,
    pub catastrophic_amps: f64,
    pub mitigation_grace_seconds: f64,
    pub mitigation_clear_seconds: f64,
    pub mitigation_min_hold_seconds: f64,
    pub retrigger_window_seconds: f64,
    // GPU sag (warn-only)
    pub sag_enabled: bool,
    pub sag_warn_degrees: f64,
    pub sag_serious_degrees: f64,
    pub sag_sustained_seconds: f64,
    /// 0 = from the detected card's ASUS spec.
    pub sag_card_length_mm: f64,
    // pin imbalance
    pub imbalance_enabled: bool,
    pub imbalance_min_amps: f64,
    pub imbalance_percent: f64,
    pub imbalance_delta_amps: f64,
    pub imbalance_sustained_seconds: f64,
    pub imbalance_response: ImbalanceResponse,
    /// Linux: scan for the pin monitor by itself when the card is there but unreadable,
    /// once per time that happens (see sensor/detect.rs). Off: the dashboard asks.
    pub sensor_auto_scan: bool,
    // Overclock (see the Overclock tab). Each part is applied only when its box is ticked;
    // all off by default so a fresh install never touches the GPU's clocks or voltage.
    pub oc_uv_enabled: bool,
    /// Voltage/frequency curve undervolt: the points from `from` to `to` mV are pinned to
    /// `target` MHz.
    pub oc_uv_from_mv: f64,
    pub oc_uv_to_mv: f64,
    pub oc_uv_target_mhz: f64,
    pub oc_power_enabled: bool,
    pub oc_power_w: f64,
    pub oc_mem_enabled: bool,
    /// Afterburner units (twice the NVML offset's effective-clock figure).
    pub oc_mem_offset_mhz: f64,
    /// Wait after the service starts before applying, so the driver has settled.
    pub oc_apply_delay_seconds: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            threshold_amps: 9.5,
            sustained_seconds_required: 15.0,
            response_mode: ResponseMode::Tiered,
            // Spec load at 600 W is ~8.3 A per pin and ASUS warns at 9.2 A; documented
            // slow-cook failures run 11-15 A, the famous melted plug measured over 22 A.
            critical_amps: 13.0,
            critical_seconds: 3.0,
            catastrophic_amps: 16.0,
            mitigation_grace_seconds: 10.0,
            mitigation_clear_seconds: 60.0,
            mitigation_min_hold_seconds: 120.0,
            retrigger_window_seconds: 600.0,
            sag_enabled: true,
            sag_warn_degrees: 1.0,
            sag_serious_degrees: 2.0,
            sag_sustained_seconds: 60.0,
            sag_card_length_mm: 0.0,
            // Modelled on the Thermal Grizzly WireView Pro II: compare highest and lowest
            // pin once one carries 6 A, alarm above 40%.
            imbalance_enabled: true,
            imbalance_min_amps: 6.0,
            imbalance_percent: 40.0,
            imbalance_delta_amps: 4.0,
            imbalance_sustained_seconds: 5.0,
            imbalance_response: ImbalanceResponse::Warn,
            // Off: a scan touches I2C buses other than the known one, so the first one is
            // the user's decision; the scan popup offers to make it automatic.
            sensor_auto_scan: false,
            // Overclock: off until the user ticks it; values are the owner's tuned profile.
            oc_uv_enabled: false,
            oc_uv_from_mv: 820.0,
            oc_uv_to_mv: 895.0,
            oc_uv_target_mhz: 2827.0,
            oc_power_enabled: false,
            oc_power_w: 520.0,
            oc_mem_enabled: false,
            oc_mem_offset_mhz: 2000.0,
            oc_apply_delay_seconds: 30.0,
        }
    }
}

/// A partial change from the dashboard. Only the fields that are set are checked/applied.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SettingsPatch {
    pub threshold_amps: Option<f64>,
    pub sustained_seconds_required: Option<f64>,
    pub response_mode: Option<ResponseMode>,
    pub critical_amps: Option<f64>,
    pub critical_seconds: Option<f64>,
    pub catastrophic_amps: Option<f64>,
    pub sag_enabled: Option<bool>,
    pub sag_warn_degrees: Option<f64>,
    pub sag_serious_degrees: Option<f64>,
    pub sag_sustained_seconds: Option<f64>,
    pub sag_card_length_mm: Option<f64>,
    pub imbalance_enabled: Option<bool>,
    pub imbalance_min_amps: Option<f64>,
    pub imbalance_percent: Option<f64>,
    pub imbalance_delta_amps: Option<f64>,
    pub imbalance_sustained_seconds: Option<f64>,
    pub imbalance_response: Option<ImbalanceResponse>,
    pub sensor_auto_scan: Option<bool>,
    pub oc_uv_enabled: Option<bool>,
    pub oc_uv_from_mv: Option<f64>,
    pub oc_uv_to_mv: Option<f64>,
    pub oc_uv_target_mhz: Option<f64>,
    pub oc_power_enabled: Option<bool>,
    pub oc_power_w: Option<f64>,
    pub oc_mem_enabled: Option<bool>,
    pub oc_mem_offset_mhz: Option<f64>,
    pub oc_apply_delay_seconds: Option<f64>,
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        *self == SettingsPatch::default()
    }
}

/// Sag detection state as shown to the user.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SagStatus {
    /// Not known yet.
    #[default]
    Starting,
    Ok,
    Warning,
    Serious,
    Uncalibrated,
    Unavailable,
    Disabled,
}

/// The service's state, polled by the UI a few times a second.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Live {
    pub version: String,
    pub status: Status,
    pub paused: bool,
    pub last_message: String,
    /// Model name of the monitored card, "" while none is connected.
    pub card: String,
    pub card_subsystem: Option<u32>,
    /// Every pin as (short name "Pin1", amps), in the order the card reports them.
    pub pins: Vec<(String, f64)>,
    pub volts: Option<Vec<f64>>,
    pub max_label: String,
    pub max_val: f64,
    pub min_label: String,
    pub min_val: Option<f64>,
    /// "" until known, then "ready" or why the GPU cannot be limited.
    pub mitigation: String,
    pub effective_shutdown_only: bool,
    pub engaged: bool,
    pub engaged_tier: Option<u8>,
    /// Unix seconds.
    pub engaged_at: Option<f64>,
    pub imbalance_active: bool,
    pub imbalance_delta: Option<f64>,
    pub imbalance_pct: Option<f64>,
    pub sag_status: SagStatus,
    /// Sag since calibration, degrees.
    pub sag_deg: Option<f64>,
    /// Absolute tilt from level (or plumb), degrees; needs no calibration.
    pub sag_offaxis: Option<f64>,
    /// Latest raw accelerometer sample in g.
    pub sag_xyz: Option<[f64; 3]>,
    /// Calibrated gravity direction, unit vector.
    pub sag_baseline: Option<[f64; 3]>,
    pub sag_fast: bool,
    /// Unix seconds; fast IMU polling was requested until then.
    pub sag_fast_until: f64,
    /// Card length used to turn sag degrees into mm at the far end.
    pub card_length_mm: f64,
    /// (unix seconds, amps per pin), last 5 minutes.
    pub history: Vec<(f64, Vec<f64>)>,
    /// (unix seconds, x, y, z in g).
    pub imu_history: Vec<(f64, f64, f64, f64)>,
    pub settings: Settings,
    /// Running under the watchdog: a crash is restarted.
    pub watchdog: bool,
    pub log_path: String,
    /// A supported card is present but its pin monitor gives no valid readings: the
    /// dashboard offers a sensor scan (Linux; see sensor/detect.rs).
    pub sensor_scan_suggested: bool,
    /// Why the sensor cannot be read, "" while it can.
    pub sensor_problem: String,
    /// Unix seconds: when the monitor loop last started an iteration (paused and waiting for a
    /// sensor included). The dashboard warns when it stops moving: a stalled loop means the
    /// pins are not being watched, whatever the rest of this snapshot says.
    #[serde(default)]
    pub loop_at: f64,
}

/// What a sensor scan found, one line per I2C adapter.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ScanReport {
    /// An adapter passed and was saved; the service reconnects to it.
    pub found: bool,
    /// One sentence for the user.
    pub message: String,
    /// Per adapter: "/dev/i2c-9 NVIDIA i2c adapter 1 at 1:00.0: IT8915: ...".
    pub lines: Vec<String>,
}

/// Something the user should be told about (desktop notification).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Notice {
    pub seq: u64,
    pub time: f64,
    pub title: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Live,
    Defaults,
    /// Validate without applying: errors block, warnings need the user's confirmation.
    Check {
        changes: SettingsPatch,
    },
    Apply {
        changes: SettingsPatch,
    },
    ResetDefaults,
    SetPaused {
        paused: bool,
    },
    CalibrateSag,
    /// Fast accelerometer polling for the IMU view; 0 stops it. Capped at 5 minutes.
    ImuFast {
        seconds: f64,
    },
    /// Stop the guard. The UI asks for confirmation first.
    Exit,
    Autostart,
    SetAutostart {
        enable: bool,
    },
    FlightDumps,
    /// The last `max_bytes` of the log file.
    LogTail {
        max_bytes: u64,
    },
    /// A flight dump's content, by file name (no paths).
    ReadFlight {
        name: String,
    },
    /// Notices with seq > since.
    Events {
        since: u64,
    },
    /// Find the card's pin monitor on its I2C adapters and save the one that passes.
    /// Refused while the sensor is reading fine, so it can never interrupt protection.
    ScanSensor,
    /// Apply now: save the overclock keys, then write them to the GPU. Refused while the
    /// guard is limiting it.
    ApplyOverclock {
        changes: SettingsPatch,
    },
    /// The overclock state read from the driver, for the Overclock tab while it is visible.
    /// Only this, an apply or a release opens the GPU for the overclock.
    OverclockStatus,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Live { live: Box<Live> },
    Settings { settings: Settings },
    Check { errors: Vec<String>, warnings: Vec<String> },
    Applied { failures: Vec<String> },
    Ok,
    Autostart { state: Option<bool> },
    Bool { value: bool },
    Names { names: Vec<String> },
    Text { text: String },
    Events { notices: Vec<Notice> },
    Scan { report: ScanReport },
    Overclock { report: OcReport },
    OverclockStatus { status: OcStatus },
    Error { message: String },
}

/// The outcome of one overclock setting in an apply, a restore or a re-apply after release.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct OcResult {
    /// "undervolt" | "power" | "memory"
    pub setting: String,
    pub ok: bool,
    /// One sentence for the user: what was written, or why not.
    pub message: String,
}

/// What an apply did. Settings that are not managed and have nothing to restore get no entry.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct OcReport {
    pub results: Vec<OcResult>,
    /// Nothing was written because the guard is limiting the GPU; apply again after release.
    pub deferred: bool,
}

/// The overclock state as read from the driver, for the Overclock tab. Every value is
/// `None` when its read failed; the tab shows the rest.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct OcStatus {
    pub available: bool,
    /// Why the GPU cannot be reached, "" while it can.
    pub reason: String,
    /// Power limit in effect, W.
    pub power_w: Option<u32>,
    pub power_range_w: Option<(u32, u32)>,
    /// What the whole card draws right now, W (connector and slot together).
    pub power_draw_w: Option<u32>,
    /// Memory offset in effect, Afterburner units.
    pub mem_ab: Option<i32>,
    pub mem_range_ab: Option<(i32, i32)>,
    /// Memory clock at full load (P0 max) with the offset included, MHz.
    pub mem_clock_mhz: Option<u32>,
    /// The offset the configured From/To/Target gives on this card's curve (None if it is refused).
    pub uv_offset_mhz: Option<i32>,
    /// NVML's graphics clock offset; non-zero means another tool set one and the undervolt is refused.
    pub foreign_core_offset_mhz: i32,
    /// Some(true) once the curve write check passed on this driver, Some(false) if it failed,
    /// None while it has not run on this driver.
    pub curve_compatible: Option<bool>,
    /// Core points as (mV, stock MHz).
    pub stock_curve: Vec<(f32, f32)>,
    /// Core points as (mV, effective MHz).
    pub current_curve: Vec<(f32, f32)>,
    pub live_mv: Option<f32>,
    /// The current curve's clock at the live voltage (where the card sits on the graph).
    pub live_mhz: Option<f32>,
    /// Settings AstralGuard applied and will restore on untick: "undervolt", "power", "memory".
    pub managed: Vec<String>,
}

/// Local socket name: a named pipe on Windows, a socket file on Linux.
pub fn socket_name() -> &'static str {
    if cfg!(windows) {
        "astralguard.sock"
    } else {
        "/run/astralguard/astralguard.sock"
    }
}

/// "1.0° / 6.2 mm": every angle shown to the user, plus what it means at the far end.
pub fn fmt_tilt(deg: f64, card_length_mm: f64) -> String {
    format!("{:.1}° / {:.1} mm", deg, sag_mm(deg, card_length_mm))
}

/// Droop at the far end of the card for a given angle, treating it as rigid.
pub fn sag_mm(deg: f64, card_length_mm: f64) -> f64 {
    card_length_mm * deg.to_radians().sin()
}
