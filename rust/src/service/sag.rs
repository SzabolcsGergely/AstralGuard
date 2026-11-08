//! GPU sag from the card's accelerometer. Warn-only, never acts on the GPU.
//!
//! Watches the angle between gravity now and gravity at calibration. A gradual change held
//! past the sustained window is sag; a jump between two samples is the PC being moved or
//! knocked, reported with wording that says so. The daily median goes to
//! logs/sag_history.csv, which is what catches slow creep. Every failure stays inside
//! this module: a flaky tilt reading must never disturb the pin monitoring.

use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, NaiveDate, TimeZone};

use super::Shared;
use crate::config::{self, SagBaseline};
use crate::logger;
use crate::platform::EventKind;
use crate::protocol::{SagStatus, Settings, APP_NAME};
use crate::sensor::PinSensor;

/// Sag moves over hours to months; a knock shows up as a jump even at this rate.
const SAMPLE_EVERY_SEC: f64 = 5.0;
/// Rolling median over one minute: ~0.05° noise becomes nothing.
const WINDOW_SAMPLES: usize = 12;
const CLEAR_FRACTION: f64 = 0.7;
/// Between two consecutive samples: the PC moved or was knocked, not sag.
const JUMP_DEGREES: f64 = 1.0;
/// First-run auto-calibration only trusts a card this close to level (or plumb).
const AUTO_BASELINE_MAX_OFFAXIS: f64 = 1.5;
/// Taken at the pin poll rate, so about 10 s.
const CALIBRATION_SAMPLES: usize = 20;
const MAX_CONSECUTIVE_FAILURES: u32 = 12;
const RETRY_AFTER_SEC: f64 = 300.0;
const DRIFT_DEGREES: f64 = 0.3;
const DRIFT_WINDOW_DAYS: f64 = 30.0;
const DRIFT_MIN_DAYS: f64 = 7.0;
const HISTORY_KEEP_ROWS: usize = 400;
pub const FAST_INTERVAL_SEC: f64 = 0.1;

type V3 = [f64; 3];

pub fn unit(v: V3) -> V3 {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / n, v[1] / n, v[2] / n]
}

/// Angle between two unit vectors.
pub fn angle_deg(a: V3, b: V3) -> f64 {
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0).acos().to_degrees()
}

/// How far gravity is from the nearest sensor axis. A level horizontal card and a plumb
/// vertical mount both read near 0, with no calibration.
pub fn off_axis_deg(v: V3) -> f64 {
    v.iter().map(|c| c.abs()).fold(0.0, f64::max).min(1.0).acos().to_degrees()
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

fn median_vector<'a>(samples: impl Iterator<Item = &'a V3> + Clone) -> V3 {
    let axis = |i: usize| median(samples.clone().map(|s| s[i]).collect());
    unit([axis(0), axis(1), axis(2)])
}

fn history_path() -> std::path::PathBuf {
    config::log_dir().join("sag_history.csv")
}

pub struct SagMonitor {
    shared: Arc<Shared>,
    baseline: Option<V3>,
    baseline_subsystem: Option<u32>,
    subsystem: Option<u32>,
    window: VecDeque<V3>,
    last_sample: Option<V3>,
    next_sample: f64,
    failures: u32,
    unavailable_until: f64,
    active: bool,
    level: u8,
    raise_since: Option<f64>,
    moved_pending: bool,
    auto_refused: bool,
    drift_checked: bool,
    calibrating: Option<Vec<V3>>,
    day: Option<String>,
    day_samples: Vec<(f64, f64)>,
    samples_since_minute: usize,
}

impl SagMonitor {
    pub fn new(shared: Arc<Shared>, baseline: Option<SagBaseline>) -> Self {
        SagMonitor {
            shared,
            baseline: baseline.map(|b| unit(b.vector)),
            baseline_subsystem: baseline.map(|b| b.subsystem),
            subsystem: None,
            window: VecDeque::with_capacity(WINDOW_SAMPLES),
            last_sample: None,
            next_sample: 0.0,
            failures: 0,
            unavailable_until: 0.0,
            active: false,
            level: 0,
            raise_since: None,
            moved_pending: false,
            auto_refused: false,
            drift_checked: false,
            calibrating: None,
            day: None,
            day_samples: Vec::new(),
            samples_since_minute: 0,
        }
    }

    fn tilt(&self, deg: f64) -> String {
        self.shared.tilt(deg)
    }

    /// Called once per pin tick, after the pin decision. Reads the sensor only when due.
    pub fn tick(&mut self, sensor: Option<&mut PinSensor>, now: f64, skip: bool, s: &Settings) {
        let Some(sensor) = sensor.filter(|_| s.sag_enabled) else {
            if self.shared.calibrate_request.swap(false, Ordering::Relaxed) {
                self.shared.notify(APP_NAME, "Sag calibration needs sag detection enabled and a connected card.");
            }
            self.publish(Some(if s.sag_enabled { SagStatus::Unavailable } else { SagStatus::Disabled }), None, None);
            return;
        };
        if Some(sensor.info.subsystem) != self.subsystem {
            self.bind_card(sensor.info.subsystem);
        }
        // While the GPU is limited, nothing but the pin reads touches the bus.
        if skip || now < self.unavailable_until {
            return;
        }
        if self.shared.calibrate_request.swap(false, Ordering::Relaxed) {
            self.calibrating = Some(Vec::new());
            logger::info("Sag calibration started.");
        }
        if self.calibrating.is_some() {
            if let Some(v) = self.read(sensor, now) {
                let samples = self.calibrating.as_mut().unwrap();
                samples.push(v);
                if samples.len() >= CALIBRATION_SAMPLES {
                    self.finish_calibration();
                }
            }
            return;
        }
        if now < self.next_sample {
            return;
        }
        self.next_sample = now + SAMPLE_EVERY_SEC;
        if let Some(v) = self.read(sensor, now) {
            self.process(v, now, s);
        }
    }

    fn bind_card(&mut self, subsystem: u32) {
        self.subsystem = Some(subsystem);
        let override_mm = self.shared.settings().sag_card_length_mm > 0.0;
        logger::info(format!(
            "Sag: using a card length of {} mm for the mm conversion ({}).",
            config::g(self.shared.card_length_mm()),
            if override_mm { "config.ini override" } else { "ASUS spec for this card" }
        ));
        self.reset_tracking();
        if self.baseline.is_some() && Some(subsystem) != self.baseline_subsystem {
            logger::warn(format!(
                "Stored sag baseline belongs to another card (0x{:08X}); it will be re-taken.",
                self.baseline_subsystem.unwrap_or(0)
            ));
            self.baseline = None;
        }
    }

    fn reset_tracking(&mut self) {
        self.window.clear();
        self.last_sample = None;
        self.level = 0;
        self.raise_since = None;
        self.moved_pending = false;
    }

    fn read(&mut self, sensor: &mut PinSensor, now: f64) -> Option<V3> {
        match sensor.read_gsensor() {
            Ok(raw) => {
                self.shared.record_imu(now, raw);
                let v = unit(raw);
                self.failures = 0;
                if !self.active {
                    self.active = true;
                    logger::info(format!(
                        "Sag detection active: card {} off level, baseline {}.",
                        self.tilt(off_axis_deg(v)),
                        if self.baseline.is_some() { "loaded" } else { "not set yet" }
                    ));
                }
                Some(v)
            }
            Err(e) => {
                self.failures += 1;
                if self.failures >= MAX_CONSECUTIVE_FAILURES {
                    logger::warn(format!(
                        "G-sensor not responding ({e}); sag detection paused, retrying in {:.0} min.",
                        RETRY_AFTER_SEC / 60.0
                    ));
                    self.failures = 0;
                    self.active = false;
                    self.unavailable_until = now + RETRY_AFTER_SEC;
                    self.calibrating = None;
                    self.reset_tracking();
                    self.publish(Some(SagStatus::Unavailable), None, None);
                }
                None
            }
        }
    }

    fn process(&mut self, v: V3, now: f64, s: &Settings) {
        if let Some(last) = self.last_sample {
            let jump = angle_deg(v, last);
            if jump >= JUMP_DEGREES {
                logger::info(format!(
                    "Card orientation jumped {} between samples (PC moved or knocked?). Re-evaluating after one minute.",
                    self.tilt(jump)
                ));
                self.window.clear();
                self.raise_since = None;
                self.moved_pending = true;
            }
        }
        self.last_sample = Some(v);
        if self.window.len() >= WINDOW_SAMPLES {
            self.window.pop_front();
        }
        self.window.push_back(v);

        let med = median_vector(self.window.iter());
        let off_axis = off_axis_deg(med);
        let full = self.window.len() == WINDOW_SAMPLES;

        let Some(baseline) = self.baseline else {
            if full {
                self.try_auto_baseline(med, off_axis);
            }
            if self.baseline.is_none() {
                self.publish(Some(SagStatus::Uncalibrated), None, Some(off_axis));
            }
            return;
        };

        let dev = angle_deg(med, baseline);
        self.history_add(dev, off_axis, now);
        if !full {
            // Judge only a full minute of samples, never a half-filled window.
            self.publish(None, Some(dev), Some(off_axis));
            return;
        }
        if !self.drift_checked {
            self.drift_checked = true;
            self.check_drift(dev);
        }

        let (warn, serious) = (s.sag_warn_degrees, s.sag_serious_degrees);
        let target = if dev >= serious {
            2
        } else if dev >= warn {
            1
        } else {
            0
        };

        if self.moved_pending {
            // A full minute after the jump: wherever it is now is where it settled.
            self.moved_pending = false;
            if target > self.level {
                self.escalate(target, dev, true);
            }
            self.publish(None, Some(dev), Some(off_axis));
            return;
        }

        if target > self.level {
            match self.raise_since {
                None => self.raise_since = Some(now),
                Some(since) if now - since >= s.sag_sustained_seconds => {
                    self.escalate(target, dev, false);
                    self.raise_since = None;
                }
                _ => {}
            }
        } else {
            self.raise_since = None;
            let mut new_level = 0;
            for (lvl, threshold) in [(2u8, serious), (1, warn)] {
                if lvl <= self.level && dev >= threshold * CLEAR_FRACTION {
                    new_level = lvl;
                    break;
                }
            }
            if new_level < self.level {
                self.level = new_level;
                let name = ["ok", "warning", "serious"][new_level as usize];
                let msg = format!("GPU sag back to {} from baseline ({name}).", self.tilt(dev));
                logger::info(&msg);
                if new_level == 0 {
                    self.shared.event(EventKind::Info, 7006, &msg);
                    self.shared.notify(APP_NAME, "GPU sag warning cleared.");
                }
            }
        }
        self.publish(None, Some(dev), Some(off_axis));
    }

    fn escalate(&mut self, level: u8, dev: f64, sudden: bool) {
        self.level = level;
        let (id, title, msg) = if sudden {
            (
                7003,
                format!("{APP_NAME} - Card orientation changed"),
                format!(
                    "Card orientation changed suddenly by {} at the far end. If you moved or tilted the PC, \
                     recalibrate sag. Otherwise check the bracket screws and the card support.",
                    self.tilt(dev)
                ),
            )
        } else {
            (
                if level == 1 { 7001 } else { 7002 },
                format!("{APP_NAME} - GPU sag{}", if level == 2 { " (serious)" } else { "" }),
                format!(
                    "The card has sagged {} at the far end since calibration. A sagging card strains the \
                     12VHPWR connector - consider a GPU support bracket.",
                    self.tilt(dev)
                ),
            )
        };
        logger::warn(&msg);
        self.shared.event(EventKind::Warning, id, &msg);
        self.shared.notify(&title, &msg);
    }

    fn try_auto_baseline(&mut self, med: V3, off_axis: f64) {
        if off_axis <= AUTO_BASELINE_MAX_OFFAXIS {
            self.set_baseline(med);
            logger::info(format!("Sag baseline taken automatically (card {} off level).", self.tilt(off_axis)));
            self.shared.notify(APP_NAME, "GPU sag detection calibrated.");
        } else if !self.auto_refused {
            self.auto_refused = true;
            let msg = format!(
                "The card sits {} off level, so it may already be sagging. Support the card, then \
                 calibrate the sag baseline from the dashboard.",
                self.tilt(off_axis)
            );
            logger::warn(&msg);
            self.shared.event(EventKind::Warning, 7007, &msg);
            self.shared.notify(&format!("{APP_NAME} - GPU sag"), &msg);
        }
    }

    fn finish_calibration(&mut self) {
        let samples = self.calibrating.take().unwrap_or_default();
        let med = median_vector(samples.iter());
        let off_axis = off_axis_deg(med);
        let saved = self.set_baseline(med);
        let mut msg = format!("Sag baseline calibrated (card {} off level).", self.tilt(off_axis));
        if off_axis > AUTO_BASELINE_MAX_OFFAXIS {
            msg += " The card is noticeably off level; sag is measured from this position.";
        }
        if !saved {
            msg += " (could not be saved to config.ini)";
        }
        logger::info(&msg);
        self.shared.event(EventKind::Info, 7005, &msg);
        self.shared.notify(APP_NAME, &msg);
    }

    fn set_baseline(&mut self, vec: V3) -> bool {
        self.baseline = Some(vec);
        self.baseline_subsystem = self.subsystem;
        self.reset_tracking();
        self.auto_refused = false;
        // History measured against an older baseline is not comparable.
        self.drift_checked = true;
        self.day_samples.clear();
        self.publish(None, Some(0.0), Some(off_axis_deg(vec)));
        let b = SagBaseline { vector: vec, subsystem: self.subsystem.unwrap_or(0) };
        *self.shared.baseline.lock().unwrap_or_else(|e| e.into_inner()) = Some(b);
        if self.shared.quiet {
            return true;
        }
        config::save_baseline(&config::config_path(), &b).is_ok()
    }

    fn baseline_key(&self) -> String {
        self.baseline.map(|b| format!("{:.5},{:.5},{:.5}", b[0], b[1], b[2])).unwrap_or_default()
    }

    fn publish(&self, status: Option<SagStatus>, dev: Option<f64>, off_axis: Option<f64>) {
        let status = status.unwrap_or([SagStatus::Ok, SagStatus::Warning, SagStatus::Serious][self.level as usize]);
        let baseline = self.baseline;
        self.shared.update(|l| {
            l.sag_status = status;
            l.sag_deg = dev;
            l.sag_offaxis = off_axis;
            l.sag_baseline = baseline;
        });
    }

    /// The pause between pin ticks. With fast polling on, it is spent reading the
    /// accelerometer every FAST_INTERVAL_SEC, in this thread, so the reads never overlap a
    /// pin read. The samples go to the dashboard only, never to detection.
    pub fn idle(&mut self, sensor: Option<&mut PinSensor>, seconds: f64, skip: bool, s: &Settings) {
        let now = crate::now();
        let fast_until = self.shared.fast_until();
        let fast = now < fast_until;
        let sensor = match sensor {
            Some(sensor) if fast && !skip && self.active && s.sag_enabled && now >= self.unavailable_until => sensor,
            _ => {
                self.shared.update(|l| l.sag_fast = fast);
                std::thread::sleep(Duration::from_secs_f64(seconds));
                return;
            }
        };
        self.shared.update(|l| l.sag_fast = true);
        let end = now + seconds;
        loop {
            let t = crate::now();
            if t >= end {
                break;
            }
            // Never counted against the sensor: a miss only thins the plot.
            if let Ok(v) = sensor.read_gsensor() {
                self.shared.record_imu(crate::now(), v);
            }
            let wait = FAST_INTERVAL_SEC.min(end - crate::now()).max(0.0);
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
    }

    fn history_add(&mut self, dev: f64, off_axis: f64, now: f64) {
        let day =
            Local.timestamp_opt(now as i64, 0).single().map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
        if self.day.as_ref().is_some_and(|d| *d != day) {
            if let Some(median_dev) = self.write_history_row() {
                self.check_drift(median_dev);
            }
        }
        self.day = Some(day);
        // One point per minute is plenty for a daily median.
        self.samples_since_minute += 1;
        if self.samples_since_minute >= WINDOW_SAMPLES {
            self.samples_since_minute = 0;
            self.day_samples.push((dev, off_axis));
        }
    }

    /// Append the day's medians to sag_history.csv. Returns the median deviation.
    fn write_history_row(&mut self) -> Option<f64> {
        let samples = std::mem::take(&mut self.day_samples);
        if samples.len() < 10 || self.baseline.is_none() || self.shared.quiet {
            return None;
        }
        let median_dev = median(samples.iter().map(|s| s.0).collect());
        let median_off = median(samples.iter().map(|s| s.1).collect());
        let path = history_path();
        let mut rows: Vec<String> = fs::read_to_string(&path)
            .map(|t| t.lines().skip(1).filter(|r| !r.trim().is_empty()).map(String::from).collect())
            .unwrap_or_default();
        rows.push(format!(
            "{},{median_dev:.3},{median_off:.3},{},\"{}\"",
            self.day.clone().unwrap_or_default(),
            samples.len(),
            self.baseline_key()
        ));
        let keep = &rows[rows.len().saturating_sub(HISTORY_KEEP_ROWS)..];
        let text = format!("date,sag_deg,off_level_deg,minutes,baseline\n{}\n", keep.join("\n"));
        if let Err(e) = fs::write(&path, text) {
            logger::warn(format!("Sag history: could not write ({e})"));
        }
        Some(median_dev)
    }

    /// Warn when sag grew by DRIFT_DEGREES against a day 7-30 days back.
    fn check_drift(&self, current_dev: f64) {
        let Ok(text) = fs::read_to_string(history_path()) else { return };
        let key = self.baseline_key();
        let today = Local::now().date_naive();
        let oldest = text.lines().skip(1).find_map(|row| {
            let parts: Vec<&str> = row.splitn(5, ',').collect();
            if parts.len() < 5 || parts[4].trim_matches('"') != key {
                return None;
            }
            let date = NaiveDate::parse_from_str(parts[0], "%Y-%m-%d").ok()?;
            let age = (today - date).num_days() as f64;
            // Rows are chronological, so the first match is the oldest.
            (DRIFT_MIN_DAYS..=DRIFT_WINDOW_DAYS).contains(&age).then(|| parts[1].parse::<f64>().ok()).flatten()
        });
        if let Some(oldest) = oldest {
            if current_dev - oldest >= DRIFT_DEGREES {
                let msg = format!(
                    "GPU sag is creeping: {} -> {} from baseline over the last few weeks. Check the card \
                     support before it loosens the 12VHPWR connector.",
                    self.tilt(oldest),
                    self.tilt(current_dev)
                );
                logger::warn(&msg);
                self.shared.event(EventKind::Warning, 7004, &msg);
                self.shared.notify(&format!("{APP_NAME} - GPU sag"), &msg);
            }
        }
    }

    /// Flush today's partial row so a machine shut down nightly still logs.
    pub fn close(&mut self) {
        self.write_history_row();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angles() {
        let level = unit([-35.0, -33.0, 4090.0]);
        assert!(off_axis_deg(level) < 1.0);
        let tilted = unit([0.0, 2f64.to_radians().sin(), 2f64.to_radians().cos()]);
        assert!((angle_deg(unit([0.0, 0.0, 1.0]), tilted) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn median_vector_ignores_one_outlier() {
        let a = [0.0, 0.0, 1.0];
        let samples = [a, a, a, [0.5, 0.0, 0.8]];
        let m = median_vector(samples.iter());
        assert!(angle_deg(m, a) < 1e-9);
    }

    #[test]
    fn sustained_sag_escalates() {
        let shared = Shared::new(Settings::default(), None, true);
        let base = SagBaseline { vector: [0.0, 0.0, 1.0], subsystem: 0x89E31043 };
        let mut m = SagMonitor::new(shared, Some(base));
        m.subsystem = Some(0x89E31043);
        let s = Settings::default();
        let sagged = unit([0.0, 1.5f64.to_radians().sin(), 1.5f64.to_radians().cos()]);
        let mut t = 0.0;
        for _ in 0..12 {
            m.process(sagged, t, &s);
            t += 5.0;
        }
        assert_eq!(m.level, 0, "needs the sustained window first");
        for _ in 0..13 {
            m.process(sagged, t, &s);
            t += 5.0;
        }
        assert_eq!(m.level, 1);
    }
}
