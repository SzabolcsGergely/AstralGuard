//! `astralguard ui --demo`: a simulated service, no hardware and no IPC. Loops through idle,
//! gaming load, Pin3 dropping out (imbalance) and the response ladder limiting the GPU.
//! `--no-sensor` starts with the card found but its pin monitor unreadable, until Scan.

use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::backend::Backend;
use crate::config;
use crate::protocol::*;

const CYCLE: f64 = 90.0;
const CARD_MM: f64 = 357.6;

pub struct Demo {
    t0: f64,
    paused: bool,
    offline: bool,
    /// The card is found but its pin monitor gives no valid data, until a scan.
    no_sensor: bool,
    settings: Settings,
    history: VecDeque<(f64, Vec<f64>)>,
    imu: VecDeque<(f64, f64, f64, f64)>,
    imu_last: f64,
    fast_until: f64,
    baseline: [f64; 3],
    autostart: bool,
    rng: u64,
    oc: DemoOc,
}

/// The simulated GPU's overclock state: what AstralGuard has written.
struct DemoOc {
    managed: Vec<String>,
    deltas_khz: Vec<i32>,
    power_w: u32,
    mem_ab: i32,
}

/// The owner's stock V/F curve, (mV, MHz).
const OC_STOCK: [(f32, f32); 13] = [
    (700.0, 225.0),
    (770.0, 247.0),
    (800.0, 675.0),
    (810.0, 757.0),
    (815.0, 847.0),
    (820.0, 930.0),
    (850.0, 1365.0),
    (895.0, 1965.0),
    (900.0, 2047.0),
    (950.0, 2655.0),
    (1000.0, 2820.0),
    (1100.0, 3022.0),
    (1240.0, 3232.0),
];
const OC_STOCK_W: u32 = 600;
const OC_POWER_RANGE: (u32, u32) = (400, 600);
const OC_MEM_RANGE: (i32, i32) = (-1000, 3000);
const OC_LIVE_MV: f32 = 940.0;

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl Demo {
    /// `phase`: seconds into the cycle to start at (52 = imbalance, 67 = GPU limited).
    pub fn new(phase: f64, offline: bool, no_sensor: bool) -> Demo {
        let t = now();
        let mut d = Demo {
            t0: t - phase,
            paused: false,
            offline,
            no_sensor,
            settings: Settings::default(),
            history: VecDeque::new(),
            imu: VecDeque::new(),
            imu_last: t - 60.0,
            fast_until: 0.0,
            baseline: [-0.00929, -0.00806, 0.99992],
            autostart: true,
            rng: 0x9E37_79B9_7F4A_7C15,
            oc: DemoOc { managed: Vec::new(), deltas_khz: Vec::new(), power_w: OC_STOCK_W, mem_ab: 0 },
        };
        // the owner's profile, ticked and already applied, so the Overclock tab has something to show
        d.settings.oc_uv_enabled = true;
        d.settings.oc_power_enabled = true;
        d.settings.oc_mem_enabled = true;
        let _ = d.oc_apply();
        // five minutes of history, so the chart is not empty
        for i in 0..600 {
            let ts = t - 300.0 + i as f64 * 0.5;
            let pins = d.pins(ts);
            d.history.push_back((ts, pins));
        }
        // a dense minute of accelerometer samples, as with fast polling on
        while d.imu_last + 0.1 <= t {
            d.imu_last += 0.1;
            let xyz = d.xyz(d.imu_last);
            d.imu.push_back((d.imu_last, xyz[0], xyz[1], xyz[2]));
        }
        d
    }

    fn oc_points() -> Vec<crate::overclock::curve::RawPoint> {
        OC_STOCK
            .iter()
            .map(|&(mv, mhz)| crate::overclock::curve::RawPoint {
                volt_uv: (mv * 1000.0).round() as u32,
                freq_khz: (mhz * 1000.0).round() as u32,
                delta_khz: 0,
                memory: false,
            })
            .collect()
    }

    /// Write the ticked settings to the simulated GPU and put the unticked ones back.
    fn oc_apply(&mut self) -> OcReport {
        let s = self.settings.clone();
        let mut results = Vec::new();
        let mut res = |setting: &str, ok: bool, message: String| {
            results.push(OcResult { setting: setting.into(), ok, message });
        };
        let managed = |oc: &DemoOc, k: &str| oc.managed.iter().any(|m| m == k);
        if s.oc_uv_enabled {
            match crate::overclock::curve::plan_undervolt(
                &Self::oc_points(),
                s.oc_uv_from_mv,
                s.oc_uv_to_mv,
                s.oc_uv_target_mhz,
                1000,
            ) {
                Ok(plan) => {
                    self.oc.deltas_khz = plan.deltas_khz.iter().map(|d| d.1).collect();
                    if !managed(&self.oc, "undervolt") {
                        self.oc.managed.push("undervolt".into());
                    }
                    res(
                        "undervolt",
                        true,
                        format!(
                            "Undervolt written: {:+} MHz on {}-{} mV",
                            plan.offset_mhz,
                            config::g(s.oc_uv_from_mv),
                            config::g(s.oc_uv_to_mv)
                        ),
                    );
                }
                Err(e) => res("undervolt", false, e),
            }
        } else if managed(&self.oc, "undervolt") {
            self.oc.deltas_khz.clear();
            self.oc.managed.retain(|m| m != "undervolt");
            res("undervolt", true, "Stock curve restored".into());
        }
        if s.oc_power_enabled {
            let w = s.oc_power_w.round();
            if w >= OC_POWER_RANGE.0 as f64 && w <= OC_POWER_RANGE.1 as f64 {
                self.oc.power_w = w as u32;
                if !managed(&self.oc, "power") {
                    self.oc.managed.push("power".into());
                }
                res("power", true, format!("Power limit set to {w} W"));
            } else {
                res(
                    "power",
                    false,
                    format!("{w} W is outside the card's range {}-{} W", OC_POWER_RANGE.0, OC_POWER_RANGE.1),
                );
            }
        } else if managed(&self.oc, "power") {
            self.oc.power_w = OC_STOCK_W;
            self.oc.managed.retain(|m| m != "power");
            res("power", true, format!("Power limit back to {OC_STOCK_W} W"));
        }
        if s.oc_mem_enabled {
            let m = s.oc_mem_offset_mhz.round();
            if m >= OC_MEM_RANGE.0 as f64 && m <= OC_MEM_RANGE.1 as f64 {
                self.oc.mem_ab = m as i32;
                if !managed(&self.oc, "memory") {
                    self.oc.managed.push("memory".into());
                }
                res("memory", true, format!("Memory offset set to {m:+} MHz"));
            } else {
                res(
                    "memory",
                    false,
                    format!("{m:+} MHz is outside the card's range {}..{} MHz", OC_MEM_RANGE.0, OC_MEM_RANGE.1),
                );
            }
        } else if managed(&self.oc, "memory") {
            self.oc.mem_ab = 0;
            self.oc.managed.retain(|m| m != "memory");
            res("memory", true, "Memory offset back to +0 MHz".into());
        }
        OcReport { results, deferred: false }
    }

    fn oc_status(&self) -> OcStatus {
        let current: Vec<(f32, f32)> = OC_STOCK
            .iter()
            .enumerate()
            .map(|(i, &(mv, mhz))| (mv, mhz + self.oc.deltas_khz.get(i).copied().unwrap_or(0) as f32 / 1000.0))
            .collect();
        let live_mhz = current.windows(2).find(|w| w[0].0 <= OC_LIVE_MV && OC_LIVE_MV <= w[1].0).map(|w| {
            let f = (OC_LIVE_MV - w[0].0) / (w[1].0 - w[0].0);
            w[0].1 + (w[1].1 - w[0].1) * f
        });
        let s = &self.settings;
        let uv_offset_mhz = crate::overclock::curve::plan_undervolt(
            &Self::oc_points(),
            s.oc_uv_from_mv,
            s.oc_uv_to_mv,
            s.oc_uv_target_mhz,
            1000,
        )
        .ok()
        .map(|p| p.offset_mhz);
        OcStatus {
            available: true,
            reason: String::new(),
            power_w: Some(self.oc.power_w),
            power_range_w: Some(OC_POWER_RANGE),
            mem_ab: Some(self.oc.mem_ab),
            mem_range_ab: Some(OC_MEM_RANGE),
            mem_clock_mhz: Some((14001 + self.oc.mem_ab) as u32),
            uv_offset_mhz,
            foreign_core_offset_mhz: 0,
            curve_compatible: Some(true),
            stock_curve: OC_STOCK.to_vec(),
            current_curve: current,
            live_mv: Some(OC_LIVE_MV),
            live_mhz,
            managed: self.oc.managed.clone(),
        }
    }

    fn rand(&mut self) -> f64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.rand()
    }

    fn phase(&self, t: f64) -> f64 {
        (t - self.t0).rem_euclid(CYCLE)
    }

    fn pins(&mut self, t: f64) -> Vec<f64> {
        let ph = self.phase(t);
        let load = if ph < 15.0 { 1.1 } else { 8.2 + 0.6 * (t / 3.0).sin() };
        let mut amps: Vec<f64> = (0..6).map(|_| load * (1.0 + self.uniform(-0.04, 0.04))).collect();
        if (45.0..75.0).contains(&ph) {
            for (i, a) in amps.iter_mut().enumerate() {
                *a = if i == 2 { 0.5 } else { *a * 1.18 };
            }
        }
        if (65.0..75.0).contains(&ph) {
            for a in amps.iter_mut() {
                *a *= 0.15;
            }
        }
        amps
    }

    fn xyz(&mut self, t: f64) -> [f64; 3] {
        // ~0.7° off level with a little wobble, like the real card
        let x = -0.0085 + 0.0015 * (t / 4.0).sin() + self.uniform(-0.0006, 0.0006);
        let y = -0.0080 + 0.0010 * (t / 6.0).cos() + self.uniform(-0.0006, 0.0006);
        let z = 0.9985 + self.uniform(-0.0008, 0.0008);
        [x, y, z]
    }

    /// What the service reports while the card is found but its sensor is unreadable.
    fn live_no_sensor(&self) -> Live {
        Live {
            version: APP_VERSION.into(),
            status: Status::Error,
            last_message: "Waiting for pin sensors...".into(),
            mitigation: "ready".into(),
            card_length_mm: CARD_MM,
            settings: self.settings.clone(),
            watchdog: true,
            sensor_scan_suggested: true,
            sensor_problem:
                "sensor chip did not answer: implausible sample: no pin shows the 12 V rail (highest 0.056 V)".into(),
            loop_at: now(),
            ..Default::default()
        }
    }

    fn scan_report(&mut self) -> ScanReport {
        thread::sleep(Duration::from_millis(1500));
        self.no_sensor = false;
        let lines = (1..=7)
            .map(|n| {
                let verdict = match n {
                    1 => "pin monitor: rail 12.10-12.10 V, 1.00 g, pins 67 W of 67 W board power",
                    6 => "not the pin monitor: sample 1: read failed (Input/output error)",
                    _ => "nothing at 0x2B",
                };
                format!("/dev/i2c-{} NVIDIA i2c adapter {n} at 1:00.0: {verdict}", n + 8)
            })
            .collect();
        ScanReport {
            found: true,
            message: "Pin monitor on /dev/i2c-9 (\"NVIDIA i2c adapter 1 at 1:00.0\"), saved (demo).".into(),
            lines,
        }
    }

    fn live(&mut self) -> Live {
        if self.no_sensor {
            return self.live_no_sensor();
        }
        let t = now();
        let amps = self.pins(t);
        self.history.push_back((t, amps.clone()));
        while self.history.front().is_some_and(|h| t - h.0 > 300.0) {
            self.history.pop_front();
        }
        let step = if t < self.fast_until { 0.1 } else { 5.0 };
        while self.imu_last + step <= t {
            self.imu_last += step;
            let xyz = self.xyz(self.imu_last);
            self.imu.push_back((self.imu_last, xyz[0], xyz[1], xyz[2]));
        }
        while self.imu.len() > 3000 {
            self.imu.pop_front();
        }

        let ph = self.phase(t);
        let engaged = !self.paused && (65.0..75.0).contains(&ph);
        let imbalance = !self.paused && (50.0..65.0).contains(&ph);
        let pins: Vec<(String, f64)> = amps.iter().enumerate().map(|(i, a)| (format!("Pin{}", i + 1), *a)).collect();
        let (hi_i, hi) = pins.iter().enumerate().fold((0, f64::MIN), |m, (i, p)| if p.1 > m.1 { (i, p.1) } else { m });
        let (lo_i, lo) = pins.iter().enumerate().fold((0, f64::MAX), |m, (i, p)| if p.1 < m.1 { (i, p.1) } else { m });
        let volts: Vec<f64> = (0..6).map(|i| if i == 4 { 12.02 } else { 12.10 } + self.uniform(-0.01, 0.01)).collect();
        let s = &self.settings;
        let status = if self.paused {
            Status::Paused
        } else if engaged {
            Status::Mitigated
        } else if hi > s.threshold_amps {
            Status::Over
        } else {
            Status::Ok
        };
        let xyz = self.imu.back().map(|v| [v.1, v.2, v.3]);
        let offaxis = xyz.map(|v| {
            let g = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
            (v[2].abs() / g).clamp(-1.0, 1.0).acos().to_degrees()
        });
        let sag = xyz.map(|v| {
            let g = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
            let b = self.baseline;
            let dot = (v[0] * b[0] + v[1] * b[1] + v[2] * b[2]) / g;
            dot.clamp(-1.0, 1.0).acos().to_degrees()
        });
        let sag_status = if !s.sag_enabled {
            SagStatus::Disabled
        } else {
            match sag {
                Some(d) if d >= s.sag_serious_degrees => SagStatus::Serious,
                Some(d) if d >= s.sag_warn_degrees => SagStatus::Warning,
                Some(_) => SagStatus::Ok,
                None => SagStatus::Starting,
            }
        };
        Live {
            version: APP_VERSION.into(),
            status,
            paused: self.paused,
            last_message: String::new(),
            card: "ROG Astral RTX 5090 OC".into(),
            card_subsystem: Some(0x89E3_1043),
            max_label: pins[hi_i].0.clone(),
            max_val: hi,
            min_label: pins[lo_i].0.clone(),
            min_val: Some(lo),
            pins,
            volts: Some(volts),
            mitigation: "ready".into(),
            effective_shutdown_only: s.response_mode == ResponseMode::ShutdownOnly,
            engaged,
            engaged_tier: engaged.then_some(1),
            engaged_at: engaged.then_some(t - (ph - 65.0)),
            imbalance_active: imbalance,
            imbalance_delta: Some(hi - lo),
            imbalance_pct: Some(if hi > 0.0 { (hi - lo) / hi * 100.0 } else { 0.0 }),
            sag_status,
            sag_deg: sag,
            sag_offaxis: offaxis,
            sag_xyz: xyz,
            sag_baseline: Some(self.baseline),
            sag_fast: t < self.fast_until,
            sag_fast_until: self.fast_until,
            card_length_mm: if s.sag_card_length_mm > 0.0 { s.sag_card_length_mm } else { CARD_MM },
            history: self.history.iter().cloned().collect(),
            imu_history: self.imu.iter().filter(|v| t - v.0 <= 300.0).cloned().collect(),
            settings: self.settings.clone(),
            watchdog: true,
            log_path: String::new(),
            sensor_scan_suggested: false,
            sensor_problem: String::new(),
            loop_at: t,
        }
    }

    fn log_text(&self) -> String {
        let mut out = String::new();
        let lines = [
            ("INFO", "Starting AstralGuard (demo)"),
            ("INFO", "Connected: NVIDIA GeForce RTX 5090 [ROG Astral RTX 5090 OC] [direct]"),
            ("INFO", "Sag detection active: card 0.7° / 4.3 mm off level, baseline loaded."),
            ("INFO", "GPU limiting ready (NVAPI power limit)."),
            ("WARNING", "Pin current imbalance: Pin1 9.71 A vs Pin3 0.50 A (9.21 A, 95%) for 5 s."),
            ("WARNING", "Tier 1: Pin1 at 9.84 A for 15 s - limiting the GPU."),
            ("INFO", "Flight recorder: 120 samples written to flight_20261109_101500_engaged.csv"),
            ("INFO", "GPU limit released after 60 s below the threshold."),
            ("ERROR", "Example error line: sensor read timed out (retrying)."),
            ("INFO", "Settings changed: imbalance_percent 40 -> 35"),
        ];
        for i in 0..36 {
            let (lvl, msg) = lines[i % lines.len()];
            out.push_str(&format!(
                "2026-11-09 10:{:02}:{:02},{:03} | {lvl} | {msg}\n",
                10 + i / 6,
                (i * 7) % 60,
                (i * 137) % 1000
            ));
        }
        out
    }

    fn flight_csv(&mut self) -> String {
        let mut out = String::from("time,pin1,pin2,pin3,pin4,pin5,pin6,sag_deg\n");
        for i in 0..120 {
            let ramp = if i < 70 { 8.3 } else { 8.3 + (i - 70) as f64 * 0.04 };
            let mut row = format!("2026-11-09 10:14:{:02}.{:03}", i / 2, (i % 2) * 500);
            for p in 0..6 {
                let a = if p == 2 { 0.5 } else { ramp * 1.15 * (1.0 + self.uniform(-0.03, 0.03)) };
                row.push_str(&format!(",{a:.3}"));
            }
            row.push_str(",0.05\n");
            out.push_str(&row);
        }
        out
    }
}

impl Backend for Demo {
    fn request(&mut self, req: &Request) -> Result<Response, String> {
        if self.offline {
            thread::sleep(Duration::from_millis(5));
            return Err("the guard service is not running (demo: offline)".into());
        }
        Ok(match req {
            Request::Live => Response::Live { live: Box::new(self.live()) },
            Request::Defaults => Response::Settings { settings: Settings::default() },
            Request::Check { changes } => {
                let (errors, warnings) = config::check(&self.settings, changes);
                Response::Check { errors, warnings }
            }
            Request::Apply { changes } => {
                let (errors, _) = config::check(&self.settings, changes);
                if errors.is_empty() {
                    self.settings = config::merged(&self.settings, changes);
                }
                Response::Applied { failures: errors }
            }
            Request::ResetDefaults => {
                self.settings = Settings::default();
                Response::Ok
            }
            Request::SetPaused { paused } => {
                self.paused = *paused;
                Response::Ok
            }
            Request::CalibrateSag => {
                if let Some(v) = self.imu.back() {
                    let g = (v.1 * v.1 + v.2 * v.2 + v.3 * v.3).sqrt().max(1e-9);
                    self.baseline = [v.1 / g, v.2 / g, v.3 / g];
                }
                Response::Ok
            }
            Request::ImuFast { seconds } => {
                self.fast_until = if *seconds > 0.0 { now() + seconds.min(300.0) } else { 0.0 };
                Response::Ok
            }
            Request::Exit => Response::Ok,
            Request::Autostart => {
                thread::sleep(Duration::from_millis(400));
                Response::Autostart { state: Some(self.autostart) }
            }
            Request::SetAutostart { enable } => {
                thread::sleep(Duration::from_millis(600));
                self.autostart = *enable;
                Response::Bool { value: true }
            }
            Request::FlightDumps => Response::Names {
                names: vec!["flight_20261109_101500_engaged.csv".into(), "flight_20261031_221204_engaged.csv".into()],
            },
            Request::LogTail { .. } => Response::Text { text: self.log_text() },
            Request::ReadFlight { .. } => Response::Text { text: self.flight_csv() },
            Request::Events { .. } => Response::Events { notices: Vec::new() },
            Request::ScanSensor => Response::Scan { report: self.scan_report() },
            // Saves like the real service, then writes to the simulated GPU. Nothing is written
            // while the guard is limiting the GPU.
            Request::ApplyOverclock { changes } => {
                let (errors, _) = config::check(&self.settings, changes);
                if !errors.is_empty() {
                    return Ok(Response::Error { message: errors.join("; ") });
                }
                self.settings = config::merged(&self.settings, changes);
                thread::sleep(Duration::from_millis(600));
                if (65.0..75.0).contains(&self.phase(now())) {
                    Response::Overclock { report: OcReport { results: Vec::new(), deferred: true } }
                } else {
                    Response::Overclock { report: self.oc_apply() }
                }
            }
            Request::OverclockStatus => Response::OverclockStatus { status: self.oc_status() },
        })
    }
}
