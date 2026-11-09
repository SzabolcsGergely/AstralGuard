//! The protection core: `astralguard service`.
//!
//! Headless on purpose. The monitor thread reads the pins, runs the response ladder and,
//! when it must, limits the GPU or shuts the machine down. The UI is another process
//! talking over a local socket, so nothing it does (or how it crashes) can reach the
//! thread that makes the shutdown decision.

mod crosscheck;
mod flight;
mod imbalance;
mod monitor;
mod response;
mod sag;

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::config::{self, SagBaseline};
use crate::logger;
use crate::platform::{self, EventKind};
use crate::protocol::{
    fmt_tilt, ImbalanceResponse, Live, Notice, Request, Response, ResponseMode, Settings, SettingsPatch, APP_NAME,
    APP_VERSION,
};

/// 5 minutes of pin samples at the 0.5 s poll, for the dashboard's chart.
const PIN_HISTORY_SAMPLES: usize = 600;
/// 60 s at the fast IMU rate, or ~50 min at the normal one.
const IMU_HISTORY_SAMPLES: usize = 600;
const NOTICES_KEPT: usize = 50;
/// How long a starting service waits for a previous guard to release the lock.
const SINGLE_INSTANCE_WAIT_SEC: u64 = 15;
pub const SAG_FAST_MAX_SECONDS: f64 = 300.0;

pub struct State {
    /// The live snapshot without the histories and settings, which are filled in on read.
    pub live: Live,
    pub history: VecDeque<(f64, Vec<f64>)>,
    pub imu: VecDeque<(f64, f64, f64, f64)>,
    notices: VecDeque<Notice>,
    notice_seq: u64,
}

/// Everything the monitor thread and the socket handlers share.
pub struct Shared {
    state: Mutex<State>,
    settings: Mutex<Settings>,
    pub paused: AtomicBool,
    pub stop: AtomicBool,
    pub calibrate_request: AtomicBool,
    /// A sensor scan saved a new adapter: stop waiting and connect now.
    pub reconnect_now: AtomicBool,
    /// A scan is running; a second request is refused rather than queued.
    scanning: AtomicBool,
    /// Held by a sensor scan for as long as it talks to the adapters, and by the monitor
    /// (try_lock only) while it connects: the two never touch the bus at the same time.
    pub bus: Mutex<()>,
    fast_until: Mutex<f64>,
    pub baseline: Mutex<Option<SagBaseline>>,
    /// Tests: no toasts, no Event Log, no config writes.
    pub quiet: bool,
    pub watchdog: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not take the guard down with a poisoned lock.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(settings: Settings, baseline: Option<SagBaseline>, quiet: bool) -> Arc<Shared> {
        let live = Live {
            version: APP_VERSION.into(),
            last_message: "Starting...".into(),
            card_length_mm: crate::sensor::DEFAULT_CARD_LENGTH_MM,
            ..Default::default()
        };
        Arc::new(Shared {
            state: Mutex::new(State {
                live,
                history: VecDeque::with_capacity(PIN_HISTORY_SAMPLES),
                imu: VecDeque::with_capacity(IMU_HISTORY_SAMPLES),
                notices: VecDeque::new(),
                notice_seq: 0,
            }),
            settings: Mutex::new(settings),
            paused: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            calibrate_request: AtomicBool::new(false),
            reconnect_now: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            bus: Mutex::new(()),
            fast_until: Mutex::new(0.0),
            baseline: Mutex::new(baseline),
            quiet,
            watchdog: std::env::var_os("GUARD_WATCHDOG").is_some(),
        })
    }

    pub fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut Live)) {
        f(&mut lock(&self.state).live);
    }

    pub fn live_field<T>(&self, f: impl FnOnce(&Live) -> T) -> T {
        f(&lock(&self.state).live)
    }

    pub fn record_pins(&self, now: f64, amps: Vec<f64>) {
        let mut st = lock(&self.state);
        if st.history.len() >= PIN_HISTORY_SAMPLES {
            st.history.pop_front();
        }
        st.history.push_back((now, amps));
    }

    pub fn record_imu(&self, now: f64, v: [f64; 3]) {
        let mut st = lock(&self.state);
        if st.imu.len() >= IMU_HISTORY_SAMPLES {
            st.imu.pop_front();
        }
        st.imu.push_back((now, v[0], v[1], v[2]));
        st.live.sag_xyz = Some(v);
    }

    pub fn fast_until(&self) -> f64 {
        *lock(&self.fast_until)
    }

    pub fn request_fast(&self, seconds: f64) {
        let seconds = seconds.clamp(0.0, SAG_FAST_MAX_SECONDS);
        *lock(&self.fast_until) = if seconds > 0.0 { crate::now() + seconds } else { 0.0 };
        logger::info(if seconds > 0.0 {
            format!("IMU fast polling on for {seconds:.0} s (dashboard).")
        } else {
            "IMU fast polling off (dashboard).".into()
        });
    }

    /// A desktop notification: a toast on Windows; on Linux the UI picks it up.
    pub fn notify(&self, title: &str, msg: &str) {
        {
            let mut st = lock(&self.state);
            st.notice_seq += 1;
            let seq = st.notice_seq;
            if st.notices.len() >= NOTICES_KEPT {
                st.notices.pop_front();
            }
            st.notices.push_back(Notice { seq, time: crate::now(), title: title.into(), message: msg.into() });
        }
        if !self.quiet {
            platform::toast(title, msg);
        }
    }

    pub fn event(&self, kind: EventKind, id: u32, msg: &str) {
        if !self.quiet {
            platform::event_log(kind, id, msg);
        }
    }

    /// "" or "ready" means the GPU can be limited; anything else is why not.
    pub fn mitigation_unavailable(&self) -> String {
        let m = self.live_field(|l| l.mitigation.clone());
        if m.is_empty() || m == "ready" {
            String::new()
        } else {
            m
        }
    }

    /// True when a tier crossing goes straight to shutdown, whatever the setting says.
    pub fn effective_shutdown_only(&self) -> bool {
        self.settings().response_mode == ResponseMode::ShutdownOnly || !self.mitigation_unavailable().is_empty()
    }

    pub fn card_length_mm(&self) -> f64 {
        let override_mm = self.settings().sag_card_length_mm;
        if override_mm > 0.0 {
            override_mm
        } else {
            self.live_field(|l| crate::sensor::card_length_mm(l.card_subsystem))
        }
    }

    pub fn tilt(&self, deg: f64) -> String {
        fmt_tilt(deg, self.card_length_mm())
    }

    fn snapshot(&self) -> Live {
        let settings = self.settings();
        let shutdown_only = self.effective_shutdown_only();
        let card_length = self.card_length_mm();
        let fast_until = self.fast_until();
        let st = lock(&self.state);
        let mut live = st.live.clone();
        live.paused = self.paused.load(Ordering::Relaxed);
        live.history = st.history.iter().cloned().collect();
        live.imu_history = st.imu.iter().cloned().collect();
        live.settings = settings;
        live.effective_shutdown_only = shutdown_only;
        live.card_length_mm = card_length;
        live.sag_fast_until = fast_until;
        live.watchdog = self.watchdog;
        live.log_path = logger::path().map(|p| p.display().to_string()).unwrap_or_default();
        live
    }

    /// Apply a validated change: memory first (the ladder reads it next tick), then disk.
    fn apply(&self, patch: &SettingsPatch) -> Vec<String> {
        let mut failures = Vec::new();
        let (errors, _warnings) = config::check(&self.settings(), patch);
        if !errors.is_empty() {
            return errors;
        }
        let before = self.settings();
        let after = config::merged(&before, patch);
        *lock(&self.settings) = after.clone();
        let keys = config::patch_keys(patch);
        if !self.quiet {
            if let Err(e) = config::save_settings(&config::config_path(), &after, &keys) {
                failures.push(format!("not saved to config.ini ({e})"));
            }
        }
        if !keys.is_empty() {
            logger::info(format!(
                "Settings changed from the dashboard: {}",
                keys.iter().map(|k| format!("{k}={}", setting_value(&after, k))).collect::<Vec<_>>().join(", ")
            ));
        }
        if before.response_mode != after.response_mode {
            let msg = match after.response_mode {
                ResponseMode::Tiered => {
                    let reason = self.mitigation_unavailable();
                    if reason.is_empty() {
                        "Overcurrent will slow the GPU down first.".to_string()
                    } else {
                        format!("Saved, but the GPU cannot be limited right now ({reason}), so overcurrent still means shutdown.")
                    }
                }
                ResponseMode::ShutdownOnly => {
                    "Overcurrent will shut the machine down without limiting the GPU first.".to_string()
                }
            };
            self.notify(APP_NAME, &msg);
        }
        if before.imbalance_response != after.imbalance_response {
            let msg = match after.imbalance_response {
                ImbalanceResponse::Act => format!(
                    "A sustained pin imbalance now gets the same response as overcurrent {}",
                    if self.effective_shutdown_only() { "(shutdown-only)." } else { "(limit the GPU first)." }
                ),
                ImbalanceResponse::Warn => "A pin imbalance now only warns.".to_string(),
            };
            self.notify(APP_NAME, &msg);
        }
        failures
    }

    /// The sensor scan, from the dashboard or automatic. Only while no sensor is connected: a working sensor is never
    /// disturbed. The bus lock keeps the monitor from connecting meanwhile, so the scan is alone on it.
    pub(crate) fn scan_sensor(&self, origin: &str) -> crate::protocol::ScanReport {
        use crate::protocol::ScanReport;
        let refuse = |message: &str| ScanReport { found: false, message: message.into(), lines: Vec::new() };
        if self.live_field(|l| !l.card.is_empty()) {
            return refuse("The pin sensor is reading; no scan needed.");
        }
        if !platform::is_elevated() {
            return refuse("The guard is not running as root, so it cannot scan or save the result.");
        }
        if self.scanning.swap(true, Ordering::SeqCst) {
            return refuse("A scan is already running.");
        }
        let report = {
            let _bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
            // Checked again under the lock: the monitor may have connected since the check above.
            if self.live_field(|l| !l.card.is_empty()) {
                refuse("The pin sensor is reading; no scan needed.")
            } else {
                logger::info(format!("Sensor scan started {origin}."));
                crate::sensor::scan()
            }
        };
        self.scanning.store(false, Ordering::SeqCst);
        for line in &report.lines {
            logger::info(format!("Sensor scan: {line}"));
        }
        logger::info(format!("Sensor scan: {}", report.message));
        if report.found {
            self.reconnect_now.store(true, Ordering::Relaxed);
        }
        report
    }

    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::Live => Response::Live { live: Box::new(self.snapshot()) },
            Request::Defaults => Response::Settings { settings: Settings::default() },
            Request::Check { changes } => {
                let (errors, warnings) = config::check(&self.settings(), &changes);
                Response::Check { errors, warnings }
            }
            Request::Apply { changes } => Response::Applied { failures: self.apply(&changes) },
            Request::ResetDefaults => {
                let d = Settings::default();
                let patch = SettingsPatch {
                    threshold_amps: Some(d.threshold_amps),
                    sustained_seconds_required: Some(d.sustained_seconds_required),
                    response_mode: Some(d.response_mode),
                    critical_amps: Some(d.critical_amps),
                    critical_seconds: Some(d.critical_seconds),
                    catastrophic_amps: Some(d.catastrophic_amps),
                    sag_enabled: Some(d.sag_enabled),
                    sag_warn_degrees: Some(d.sag_warn_degrees),
                    sag_serious_degrees: Some(d.sag_serious_degrees),
                    sag_sustained_seconds: Some(d.sag_sustained_seconds),
                    sag_card_length_mm: Some(d.sag_card_length_mm),
                    imbalance_enabled: Some(d.imbalance_enabled),
                    imbalance_min_amps: Some(d.imbalance_min_amps),
                    imbalance_percent: Some(d.imbalance_percent),
                    imbalance_delta_amps: Some(d.imbalance_delta_amps),
                    imbalance_sustained_seconds: Some(d.imbalance_sustained_seconds),
                    imbalance_response: Some(d.imbalance_response),
                    sensor_auto_scan: Some(d.sensor_auto_scan),
                    // The oc_* keys stay out: a reset must not change what the GPU runs at.
                    ..Default::default()
                };
                let failures = self.apply(&patch);
                self.notify(
                    APP_NAME,
                    if failures.is_empty() { "Settings reset to defaults" } else { "Failed to reset settings" },
                );
                Response::Applied { failures }
            }
            Request::SetPaused { paused } => {
                self.paused.store(paused, Ordering::Relaxed);
                Response::Ok
            }
            Request::ScanSensor => Response::Scan { report: self.scan_sensor("from the dashboard") },
            Request::CalibrateSag => {
                self.calibrate_request.store(true, Ordering::Relaxed);
                Response::Ok
            }
            Request::ImuFast { seconds } => {
                self.request_fast(seconds);
                Response::Ok
            }
            Request::Exit => {
                logger::info("Exit requested from the dashboard.");
                self.stop.store(true, Ordering::Relaxed);
                Response::Ok
            }
            Request::Autostart => Response::Autostart { state: platform::autostart() },
            Request::SetAutostart { enable } => {
                let ok = platform::set_autostart(enable);
                if ok {
                    self.notify(
                        APP_NAME,
                        &format!("Start with the system {}.", if enable { "enabled" } else { "disabled" }),
                    );
                }
                Response::Bool { value: ok }
            }
            Request::FlightDumps => Response::Names { names: flight::dumps() },
            Request::LogTail { max_bytes } => Response::Text { text: log_tail(max_bytes.min(1 << 20)) },
            Request::ReadFlight { name } => match flight::read(&name) {
                Some(text) => Response::Text { text },
                None => Response::Error { message: "no such flight dump".into() },
            },
            Request::Events { since } => {
                let st = lock(&self.state);
                Response::Events { notices: st.notices.iter().filter(|n| n.seq > since).cloned().collect() }
            }
        }
    }
}

fn setting_value(s: &Settings, key: &str) -> String {
    let v = serde_json::to_value(s).ok();
    v.and_then(|v| v.get(key).map(|x| x.to_string().trim_matches('"').to_string())).unwrap_or_default()
}

fn log_tail(max_bytes: u64) -> String {
    let Some(path) = logger::path() else { return String::new() };
    let Ok(mut f) = fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Drop the partial first line.
    if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default()
    } else {
        text
    }
}

/// `astralguard service`. Returns the exit code: 0 for a deliberate stop (exit, shutdown,
/// already running), which tells the watchdog not to restart it.
pub fn run(_args: &[String]) -> i32 {
    let log_dir = config::log_dir();
    logger::init(&log_dir.join("astralguard.log"), logger::Level::Info);
    crate::crash::install(&log_dir);

    // Two guards would mean two shutdown commands. A guard that was just stopped can take a
    // few seconds to let go of the lock (a restart of the logon task does exactly that), so
    // wait before deciding that another guard really is running: giving up too early would
    // leave no guard at all.
    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(SINGLE_INSTANCE_WAIT_SEC);
    let mut waited = false;
    while !platform::acquire_single_instance() {
        if std::time::Instant::now() >= give_up {
            logger::info("Another AstralGuard is already running; this copy exits.");
            return 0;
        }
        if !waited {
            logger::info("Another AstralGuard holds the lock; waiting for it to exit.");
            waited = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    let (settings, baseline) = config::load(&config::config_path());
    let shared = Shared::new(settings, baseline, false);

    let handler_shared = Arc::clone(&shared);
    if let Err(e) = crate::ipc::serve(move |req| handler_shared.handle(req)) {
        logger::error(format!("Dashboard socket unavailable ({e}); protection runs without it."));
    }

    let shutting_down = monitor::run(&shared);
    if shutting_down {
        // Keep the process alive while the OS powers off; exiting early would make the
        // watchdog restart a guard into a dying session.
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_never_runs_while_the_sensor_is_reading() {
        let shared = Shared::new(Settings::default(), None, true);
        shared.update(|l| l.card = "ROG Astral RTX 5090 OC".into());
        let Response::Scan { report } = shared.handle(Request::ScanSensor) else { panic!("not a scan answer") };
        assert!(!report.found);
        assert!(report.message.contains("no scan needed"), "{}", report.message);
        assert!(report.lines.is_empty());
        assert!(!shared.reconnect_now.load(Ordering::Relaxed));
    }

    #[test]
    fn reset_defaults_leaves_the_overclock_alone() {
        let shared =
            Shared::new(Settings { oc_power_enabled: true, oc_power_w: 480.0, ..Default::default() }, None, true);
        let Response::Applied { failures } = shared.handle(Request::ResetDefaults) else {
            panic!("not an apply answer")
        };
        assert!(failures.is_empty(), "{failures:?}");
        let s = shared.settings();
        assert!(s.oc_power_enabled);
        assert_eq!(s.oc_power_w, 480.0);
    }
}
