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
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crate::config::{self, SagBaseline};
use crate::logger;
use crate::overclock::{self, OcGpu, Overclocker};
use crate::platform::{self, EventKind};
use crate::protocol::{
    fmt_tilt, ImbalanceResponse, Live, Notice, OcReport, OcResult, OcStatus, Request, Response, ResponseMode, Settings,
    SettingsPatch, APP_NAME, APP_VERSION,
};

/// 5 minutes of pin samples at the 0.5 s poll, for the dashboard's chart.
const PIN_HISTORY_SAMPLES: usize = 600;
/// 60 s at the fast IMU rate, or ~50 min at the normal one.
const IMU_HISTORY_SAMPLES: usize = 600;
const NOTICES_KEPT: usize = 50;
/// How long a starting service waits for a previous guard to release the lock.
const SINGLE_INSTANCE_WAIT_SEC: u64 = 15;
pub const SAG_FAST_MAX_SECONDS: f64 = 300.0;

/// What the service needs from the overclock, so `Shared` holds it without a type parameter
/// and the tests can give it a fake GPU.
pub trait OcDriver {
    fn apply(&mut self, s: &Settings, guard_engaged: bool) -> OcReport;
    fn after_release(&mut self, s: &Settings) -> OcReport;
    fn status(&mut self, s: &Settings) -> OcStatus;
    fn verified_driver(&self) -> Option<String>;
    /// The driver the curve write check failed on with a wrong landing, to be persisted.
    fn failed_driver(&self) -> Option<String>;
    fn subsystem(&self) -> u32;
    /// What AstralGuard applied and will restore; no driver call.
    fn managed(&self) -> Vec<String>;
}

impl<G: OcGpu> OcDriver for Overclocker<G> {
    fn apply(&mut self, s: &Settings, guard_engaged: bool) -> OcReport {
        Overclocker::apply(self, s, guard_engaged)
    }
    fn after_release(&mut self, s: &Settings) -> OcReport {
        Overclocker::after_release(self, s)
    }
    fn status(&mut self, s: &Settings) -> OcStatus {
        Overclocker::status(self, s)
    }
    fn verified_driver(&self) -> Option<String> {
        Overclocker::verified_driver(self).map(str::to_string)
    }
    fn failed_driver(&self) -> Option<String> {
        Overclocker::failed_driver(self).map(str::to_string)
    }
    fn subsystem(&self) -> u32 {
        Overclocker::subsystem(self)
    }
    fn managed(&self) -> Vec<String> {
        Overclocker::managed(self)
    }
}

/// Opens the overclocker for a card subsystem: the real GPU in the service, a fake in tests.
type OcFactory = dyn Fn(u32) -> Result<Box<dyn OcDriver + Send>, String> + Send + Sync;

/// How a call on the overclocker went.
pub enum OcAccess<R> {
    Ran(R),
    /// In use elsewhere (a status read for the dashboard); only `try_overclock` returns it.
    Busy,
    /// No card found yet, so no GPU to open.
    NoCard,
    /// The GPU could not be opened.
    Unavailable(String),
}

impl<R> OcAccess<R> {
    /// The same outcome without the result.
    pub fn ran(self) -> OcAccess<()> {
        match self {
            OcAccess::Ran(_) => OcAccess::Ran(()),
            OcAccess::Busy => OcAccess::Busy,
            OcAccess::NoCard => OcAccess::NoCard,
            OcAccess::Unavailable(e) => OcAccess::Unavailable(e),
        }
    }
}

const NO_CARD: &str = "no card connected";
const REFUSED_LIMITED: &str = "GPU is limited by the guard; try again after it releases";
/// The monitor saw the GPU limited after the IPC thread had saved: the release applies it.
const DEFERRED_LIMITED: &str = "Saved; will be applied when the guard releases the GPU";
/// The loop may have been inside the write when the wait ran out.
const TIMED_OUT: &str =
    "Saved, but the guard's loop did not answer in time; the overclock may or may not have been written — see the tab";
/// An overclock driver call on the monitor thread longer than this is logged: the pin reads
/// waited for it (see `try_overclock`).
const SLOW_OC_CALL: std::time::Duration = std::time::Duration::from_millis(250);
/// How long Apply now waits for the monitor loop to run it.
const APPLY_NOW_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Every overclock setting, from `from` into `to`.
fn copy_oc(from: &Settings, to: &mut Settings) {
    to.oc_uv_enabled = from.oc_uv_enabled;
    to.oc_uv_from_mv = from.oc_uv_from_mv;
    to.oc_uv_to_mv = from.oc_uv_to_mv;
    to.oc_uv_target_mhz = from.oc_uv_target_mhz;
    to.oc_power_enabled = from.oc_power_enabled;
    to.oc_power_w = from.oc_power_w;
    to.oc_mem_enabled = from.oc_mem_enabled;
    to.oc_mem_offset_mhz = from.oc_mem_offset_mhz;
    to.oc_apply_delay_seconds = from.oc_apply_delay_seconds;
}

/// Only the oc_* keys of a patch: Apply now must not change anything else, whatever it is sent.
fn oc_only(p: &SettingsPatch) -> SettingsPatch {
    SettingsPatch {
        oc_uv_enabled: p.oc_uv_enabled,
        oc_uv_from_mv: p.oc_uv_from_mv,
        oc_uv_to_mv: p.oc_uv_to_mv,
        oc_uv_target_mhz: p.oc_uv_target_mhz,
        oc_power_enabled: p.oc_power_enabled,
        oc_power_w: p.oc_power_w,
        oc_mem_enabled: p.oc_mem_enabled,
        oc_mem_offset_mhz: p.oc_mem_offset_mhz,
        oc_apply_delay_seconds: p.oc_apply_delay_seconds,
        ..SettingsPatch::default()
    }
}

/// Clears `oc_apply_running` however the Apply now ends, a panic included.
struct ApplyRunning<'a>(&'a AtomicBool);

impl Drop for ApplyRunning<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Any overclock setting ticked.
pub fn oc_ticked(s: &Settings) -> bool {
    s.oc_uv_enabled || s.oc_power_enabled || s.oc_mem_enabled
}

/// The service's overclocker: the real GPU, its marker, and the driver version verified before.
fn real_overclocker(subsystem: u32) -> Result<Box<dyn OcDriver + Send>, String> {
    let gpu = overclock::gpu::RealGpu::open(subsystem)?;
    let verified = config::load_oc_verified(&config::config_path());
    let failed = config::load_oc_vf_failed(&config::config_path());
    logger::info(format!("Overclock: GPU 0x{subsystem:08X} opened."));
    Ok(Box::new(Overclocker::new(gpu, overclock::marker_path(), verified, failed)))
}

/// The warning for an overclock call that held the monitor thread longer than `SLOW_OC_CALL`.
fn slow_call_note(what: &str, took: std::time::Duration) -> Option<String> {
    (took > SLOW_OC_CALL).then(|| {
        format!("Overclock: {what} took {} ms on the monitor thread; the pin reads waited for it", took.as_millis())
    })
}

/// One failed result per setting, for an Apply now that wrote nothing.
fn oc_refused(message: &str) -> OcReport {
    let results = ["undervolt", "power", "memory"]
        .into_iter()
        .map(|setting| OcResult { setting: setting.into(), ok: false, message: message.into() })
        .collect();
    OcReport { results, deferred: false }
}

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
    /// Opened on first need (a status request, Apply now, the start-up apply or a release) for
    /// the card the sensor found, so nothing touches NVML or NVAPI while no overclock setting is
    /// ticked or managed. Never held while taking another of these locks: callers clone the
    /// settings and read `state` first, then lock this alone.
    overclock: Mutex<Option<Box<dyn OcDriver + Send>>>,
    oc_factory: Box<OcFactory>,
    /// The overclock marker, read without opening the GPU (`overclock_managed`).
    oc_marker: std::path::PathBuf,
    /// The last GPU open error logged; a leaf lock, never held with another.
    oc_open_error: Mutex<String>,
    /// Apply now, handed from the IPC thread to the monitor thread (which answers on it), so
    /// the GPU write is ordered against the guard's engage decisions. One slot; a leaf lock.
    oc_mailbox: Mutex<Option<std::sync::mpsc::SyncSender<OcReport>>>,
    /// An Apply now is between its save and its answer; a second one is refused.
    oc_apply_running: AtomicBool,
    /// Where the curve write check's results (`oc_vf_verified_driver`, `oc_vf_failed_driver`)
    /// are saved: config.ini, or nowhere in tests.
    oc_config: Option<std::path::PathBuf>,
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
        let oc_config = (!quiet).then(config::config_path);
        Shared::build(settings, baseline, quiet, overclock::marker_path(), Box::new(real_overclocker), oc_config)
    }

    /// Tests: a quiet service whose overclocker comes from `factory`.
    #[cfg(test)]
    pub fn with_overclock_factory(
        settings: Settings,
        marker: std::path::PathBuf,
        factory: Box<OcFactory>,
    ) -> Arc<Shared> {
        Shared::build(settings, None, true, marker, factory, None)
    }

    /// Tests: as `with_overclock_factory`, with the driver check results saved to `config`.
    #[cfg(test)]
    pub fn with_overclock_config(
        settings: Settings,
        marker: std::path::PathBuf,
        factory: Box<OcFactory>,
        config: std::path::PathBuf,
    ) -> Arc<Shared> {
        Shared::build(settings, None, true, marker, factory, Some(config))
    }

    fn build(
        settings: Settings,
        baseline: Option<SagBaseline>,
        quiet: bool,
        oc_marker: std::path::PathBuf,
        oc_factory: Box<OcFactory>,
        oc_config: Option<std::path::PathBuf>,
    ) -> Arc<Shared> {
        let live = Live {
            version: APP_VERSION.into(),
            last_message: "Starting...".into(),
            card_length_mm: crate::sensor::DEFAULT_CARD_LENGTH_MM,
            // From the start, so a loop that never gets going shows as stalled too.
            loop_at: crate::now(),
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
            overclock: Mutex::new(None),
            oc_factory,
            oc_marker,
            oc_open_error: Mutex::new(String::new()),
            oc_mailbox: Mutex::new(None),
            oc_apply_running: AtomicBool::new(false),
            oc_config,
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

    /// config.ini, except in tests.
    fn save_to_disk(&self, s: &Settings, keys: &[&str]) -> std::io::Result<()> {
        if self.quiet {
            return Ok(());
        }
        config::save_settings(&config::config_path(), s, keys)
    }

    /// Apply a validated change: memory first (the ladder reads it next tick), then disk.
    fn apply(&self, patch: &SettingsPatch) -> Vec<String> {
        self.apply_with(patch, &|s, keys| self.save_to_disk(s, keys))
    }

    fn apply_with(
        &self,
        patch: &SettingsPatch,
        save: &dyn Fn(&Settings, &[&str]) -> std::io::Result<()>,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let (errors, _warnings) = config::check(&self.settings(), patch);
        if !errors.is_empty() {
            return errors;
        }
        let before = self.settings();
        let after = config::merged(&before, patch);
        *lock(&self.settings) = after.clone();
        let keys = config::patch_keys(patch);
        if let Err(e) = save(&after, &keys) {
            failures.push(format!("not saved to config.ini ({e})"));
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

    /// Run `f` on the overclocker for the connected card, opening it first if there is none
    /// or it is for another card. `wait`: the IPC thread waits for the lock, the monitor never
    /// does (a status read can be busy in the driver for a while; the pin loop must not stall).
    fn run_overclock<R>(&self, wait: bool, f: impl FnOnce(&mut dyn OcDriver) -> R) -> OcAccess<R> {
        let Some(subsystem) = self.live_field(|l| l.card_subsystem) else { return OcAccess::NoCard };
        let mut guard = if wait {
            lock(&self.overclock)
        } else {
            match self.overclock.try_lock() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => return OcAccess::Busy,
            }
        };
        let opening = guard.as_ref().map(|d| d.subsystem()) != Some(subsystem);
        if opening {
            *guard = None;
            match (self.oc_factory)(subsystem) {
                Ok(d) => *guard = Some(d),
                Err(e) => {
                    drop(guard);
                    self.note_open_result(Some(&e));
                    return OcAccess::Unavailable(e);
                }
            }
        }
        let Some(driver) = guard.as_mut() else { return OcAccess::NoCard };
        let before = (driver.verified_driver(), driver.failed_driver());
        let r = f(driver.as_mut());
        let after = (driver.verified_driver(), driver.failed_driver());
        drop(guard);
        if opening {
            self.note_open_result(None);
        }
        self.save_driver_checks(before, after);
        OcAccess::Ran(r)
    }

    /// Persist a newly passed or newly failed curve write check, so a restart neither probes a
    /// verified driver again nor writes test offsets on one known to be wrong.
    fn save_driver_checks(&self, before: (Option<String>, Option<String>), after: (Option<String>, Option<String>)) {
        let Some(path) = &self.oc_config else { return };
        if let Some(v) = after.0.filter(|a| Some(a) != before.0.as_ref()) {
            if let Err(e) = config::save_oc_verified(path, &v) {
                logger::warn(format!("Overclock: could not save the verified driver version ({e})"));
            }
        }
        if let Some(v) = after.1.filter(|a| Some(a) != before.1.as_ref()) {
            if let Err(e) = config::save_oc_vf_failed(path, &v) {
                logger::warn(format!("Overclock: could not save the failed driver version ({e})"));
            }
        }
    }

    /// Log a failed GPU open once per distinct error: the monitor retries the start-up apply
    /// and the dashboard asks for status every few seconds. A success re-arms it. True when it
    /// logged.
    fn note_open_result(&self, error: Option<&str>) -> bool {
        let mut last = lock(&self.oc_open_error);
        let Some(e) = error else {
            last.clear();
            return false;
        };
        if *last == e {
            return false;
        }
        *last = e.to_string();
        logger::warn(format!("Overclock unavailable: {e}"));
        true
    }

    /// For the monitor thread: never waits for the lock. The driver calls themselves (and a GPU
    /// open) can still take long, and the pin reads wait for them, so each one that takes longer
    /// than `SLOW_OC_CALL` is logged with its duration; `what` names it ("Apply now", ...).
    pub fn try_overclock<R>(&self, what: &str, f: impl FnOnce(&mut dyn OcDriver) -> R) -> OcAccess<R> {
        let start = std::time::Instant::now();
        let r = self.run_overclock(false, f);
        if let Some(note) = slow_call_note(what, start.elapsed()) {
            logger::warn(note);
        }
        r
    }

    /// What AstralGuard has applied on the connected card, without opening the GPU: from the
    /// overclocker when it is open and free, else from its marker file.
    pub fn overclock_managed(&self) -> Vec<String> {
        let Some(subsystem) = self.live_field(|l| l.card_subsystem) else { return Vec::new() };
        if let Ok(guard) = self.overclock.try_lock() {
            if let Some(d) = guard.as_ref().filter(|d| d.subsystem() == subsystem) {
                return d.managed();
            }
        }
        overclock::managed_in_marker(&self.oc_marker, subsystem)
    }

    /// The Overclock tab's view, read from the driver on request (the dashboard asks while the
    /// tab is visible).
    fn overclock_status(&self) -> OcStatus {
        let s = self.settings();
        let unavailable = |reason: String| OcStatus { available: false, reason, ..OcStatus::default() };
        match self.run_overclock(true, |d| d.status(&s)) {
            OcAccess::Ran(st) => st,
            OcAccess::NoCard | OcAccess::Busy => unavailable(NO_CARD.into()),
            OcAccess::Unavailable(e) => unavailable(e),
        }
    }

    /// Apply now. Saved first, through the same path as every setting, so the next start
    /// applies exactly what was written; then written by the monitor thread (`serve_apply_now`),
    /// never from here: only there is the write ordered against the guard engaging.
    fn apply_overclock(&self, changes: &SettingsPatch) -> OcReport {
        self.apply_overclock_with(changes, &|s, keys| self.save_to_disk(s, keys), APPLY_NOW_WAIT)
    }

    fn apply_overclock_with(
        &self,
        changes: &SettingsPatch,
        save: &dyn Fn(&Settings, &[&str]) -> std::io::Result<()>,
        wait: std::time::Duration,
    ) -> OcReport {
        // Early, so a refused apply saves nothing either; the monitor checks again before writing.
        if self.live_field(|l| l.engaged) {
            return oc_refused(REFUSED_LIMITED);
        }
        if self.oc_apply_running.swap(true, Ordering::SeqCst) {
            return oc_refused("an apply is already running");
        }
        let _running = ApplyRunning(&self.oc_apply_running);
        self.save_and_hand_over(&oc_only(changes), save, wait)
    }

    fn save_and_hand_over(
        &self,
        changes: &SettingsPatch,
        save: &dyn Fn(&Settings, &[&str]) -> std::io::Result<()>,
        wait: std::time::Duration,
    ) -> OcReport {
        let before = self.settings();
        let failures = self.apply_with(changes, save);
        if !failures.is_empty() {
            // Nothing written: a value the check refused, or one that is not saved. An unsaved
            // value is taken back out of memory too, or a later release would write what the
            // next start does not apply.
            copy_oc(&before, &mut lock(&self.settings));
            let results = failures
                .into_iter()
                .map(|f| OcResult {
                    setting: "settings".into(),
                    ok: false,
                    message: format!("{f}; nothing written to the GPU"),
                })
                .collect();
            return OcReport { results, deferred: false };
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *lock(&self.oc_mailbox) = Some(tx);
        match rx.recv_timeout(wait) {
            Ok(report) => report,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Withdrawn, so a late loop does not write what nobody waits for any more.
                lock(&self.oc_mailbox).take();
                oc_refused(TIMED_OUT)
            }
            // The monitor dropped the request: the overclock panicked there (logged).
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                oc_refused("Saved, but the overclock failed with an internal error (see the log)")
            }
        }
    }

    /// The monitor thread's half of Apply now, with its own view of whether the guard limits the
    /// GPU. True when a request was answered. A busy overclocker (a status read) keeps the
    /// request for the next iteration.
    pub fn serve_apply_now(&self, engaged: bool) -> bool {
        let Some(reply) = lock(&self.oc_mailbox).take() else { return false };
        let s = self.settings();
        let report = if engaged {
            oc_refused(DEFERRED_LIMITED)
        } else if !oc_ticked(&s) && self.overclock_managed().is_empty() {
            // Nothing to write and nothing to restore: the GPU is not even opened.
            OcReport::default()
        } else {
            match self.try_overclock("Apply now", |d| d.apply(&s, false)) {
                OcAccess::Ran(report) => report,
                OcAccess::Busy => {
                    lock(&self.oc_mailbox).get_or_insert(reply);
                    return false;
                }
                OcAccess::NoCard => oc_refused(&format!("Saved, but {NO_CARD}.")),
                OcAccess::Unavailable(e) => oc_refused(&format!("Saved, but the GPU cannot be reached: {e}")),
            }
        };
        // A request that timed out meanwhile has no receiver; nothing to do then.
        let _ = reply.send(report);
        true
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
            Request::ApplyOverclock { changes } => Response::Overclock { report: self.apply_overclock(&changes) },
            Request::OverclockStatus => Response::OverclockStatus { status: self.overclock_status() },
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
    use crate::overclock::fake::{FakeGpu, OWNERS_DRIVER, OWNERS_SUBSYSTEM};
    use crate::overclock::Overclocker;
    use std::sync::atomic::AtomicUsize;

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

    /// A quiet service whose overclocker, once opened, drives a fake owner's card. `opened`
    /// counts the opens: zero means the GPU was never touched at all. The marker is removed on drop.
    pub(super) struct FakeService {
        pub shared: Arc<Shared>,
        pub gpu: FakeGpu,
        pub opened: Arc<AtomicUsize>,
        pub marker: std::path::PathBuf,
    }

    impl Drop for FakeService {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.marker);
        }
    }

    /// A fresh marker path for `name` (any old file removed).
    pub(super) fn test_marker(name: &str) -> std::path::PathBuf {
        let marker = std::env::temp_dir().join(format!("ag-svc-oc-{}-{name}.active", std::process::id()));
        let _ = fs::remove_file(&marker);
        marker
    }

    /// With the owner's card connected (as the sensor would report it).
    pub(super) fn fake_service(marker: std::path::PathBuf, settings: Settings) -> FakeService {
        let gpu = FakeGpu::owners();
        let opened = Arc::new(AtomicUsize::new(0));
        let (g, o, m) = (gpu.clone(), Arc::clone(&opened), marker.clone());
        let factory = move |subsystem: u32| -> Result<Box<dyn OcDriver + Send>, String> {
            o.fetch_add(1, Ordering::SeqCst);
            g.state().subsystem = subsystem;
            Ok(Box::new(Overclocker::new(g.clone(), m.clone(), Some(OWNERS_DRIVER.into()), None)))
        };
        let shared = Shared::with_overclock_factory(settings, marker.clone(), Box::new(factory));
        shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        FakeService { shared, gpu, opened, marker }
    }

    fn power_520() -> SettingsPatch {
        SettingsPatch { oc_power_enabled: Some(true), oc_power_w: Some(520.0), ..Default::default() }
    }

    /// Apply now through the IPC handler, with this thread standing in for the monitor loop
    /// (whose view of `engaged` is `loop_engaged`).
    fn apply_now_seen_by(shared: &Arc<Shared>, changes: SettingsPatch, loop_engaged: bool) -> OcReport {
        let ipc = Arc::clone(shared);
        let request = std::thread::spawn(move || ipc.handle(Request::ApplyOverclock { changes }));
        while !request.is_finished() {
            shared.serve_apply_now(loop_engaged);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let Response::Overclock { report } = request.join().unwrap() else { panic!("not an overclock answer") };
        report
    }

    fn apply_now(shared: &Arc<Shared>, changes: SettingsPatch) -> OcReport {
        apply_now_seen_by(shared, changes, false)
    }

    /// Wait until an Apply now has been handed to the (stand-in) monitor loop.
    pub(super) fn wait_for_mailbox(shared: &Shared) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lock(&shared.oc_mailbox).is_none() {
            assert!(std::time::Instant::now() < end, "no Apply now arrived");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn saved(_: &Settings, _: &[&str]) -> std::io::Result<()> {
        Ok(())
    }

    #[test]
    fn apply_now_is_refused_when_the_loop_sees_the_gpu_limited() {
        // The IPC thread saw it released; the guard engaged before the loop got to the write.
        let f = fake_service(test_marker("loop-limited"), Settings::default());
        let report = apply_now_seen_by(&f.shared, power_520(), true);
        assert!(report.results.iter().all(|r| !r.ok) && !report.results.is_empty(), "{report:?}");
        let msg = &report.results[0].message;
        assert!(msg.contains("Saved; will be applied when the guard releases the GPU"), "{report:?}");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);
        // Saved: the release's check writes it once the guard lets go.
        assert!(f.shared.settings().oc_power_enabled);
    }

    #[test]
    fn apply_now_times_out_when_the_loop_does_not_answer() {
        let f = fake_service(test_marker("timeout"), Settings::default());
        let report = f.shared.apply_overclock_with(&power_520(), &saved, std::time::Duration::from_millis(30));
        assert!(report.results.iter().all(|r| !r.ok) && !report.results.is_empty(), "{report:?}");
        let msg = &report.results[0].message;
        assert!(
            msg.contains(
                "the guard's loop did not answer in time; the overclock may or may not have been written — see the tab"
            ),
            "{report:?}"
        );
        assert!(lock(&f.shared.oc_mailbox).is_none(), "withdrawn");
        assert!(!f.shared.serve_apply_now(false), "a late loop finds nothing to write");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        // Not stuck: the next one goes through.
        assert!(apply_now(&f.shared, power_520()).results.iter().all(|r| r.ok));
    }

    #[test]
    fn a_second_apply_now_while_one_is_pending_is_refused() {
        let f = fake_service(test_marker("second"), Settings::default());
        let ipc = Arc::clone(&f.shared);
        let first = std::thread::spawn(move || ipc.handle(Request::ApplyOverclock { changes: power_520() }));
        wait_for_mailbox(&f.shared);
        let second = f.shared.apply_overclock_with(&power_520(), &saved, std::time::Duration::from_secs(5));
        assert!(second.results.iter().all(|r| !r.ok) && !second.results.is_empty(), "{second:?}");
        assert!(second.results[0].message.contains("an apply is already running"), "{second:?}");
        assert!(f.shared.serve_apply_now(false));
        let Response::Overclock { report } = first.join().unwrap() else { panic!("not an overclock answer") };
        assert!(report.results.iter().all(|r| r.ok), "{report:?}");
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(520)".to_string()], "written once");
    }

    #[test]
    fn apply_overclock_is_refused_while_limited() {
        let f = fake_service(test_marker("refused"), Settings::default());
        f.shared.update(|l| l.engaged = true);
        let report = apply_now(&f.shared, power_520());
        assert!(!report.results.is_empty());
        assert!(report.results.iter().all(|r| !r.ok), "{report:?}");
        assert!(report.results[0].message.contains("try again after it releases"), "{report:?}");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);
        assert!(!f.shared.settings().oc_power_enabled, "a refused apply saves nothing either");
    }

    #[test]
    fn apply_overclock_saves_then_applies() {
        let f = fake_service(test_marker("applies"), Settings::default());
        let report = apply_now(&f.shared, power_520());
        assert!(!report.deferred);
        assert!(report.results.iter().all(|r| r.ok), "{report:?}");
        let s = f.shared.settings();
        assert!(s.oc_power_enabled);
        assert_eq!(s.oc_power_w, 520.0);
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(520)".to_string()]);
    }

    /// Apply now carries only the overclock: anything else in its patch is dropped before the
    /// check and the save, so it cannot change protection through the Overclock tab.
    #[test]
    fn apply_overclock_ignores_non_overclock_keys() {
        let f = fake_service(test_marker("oc-only"), Settings::default());
        let saved_keys = Arc::new(Mutex::new(Vec::<String>::new()));
        let k = Arc::clone(&saved_keys);
        let record = move |_: &Settings, keys: &[&str]| {
            k.lock().unwrap().extend(keys.iter().map(|s| s.to_string()));
            Ok(())
        };
        let changes = SettingsPatch { threshold_amps: Some(12.0), ..power_520() };
        let ipc = Arc::clone(&f.shared);
        let request = std::thread::spawn(move || ipc.apply_overclock_with(&changes, &record, APPLY_NOW_WAIT));
        while !request.is_finished() {
            f.shared.serve_apply_now(false);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(request.join().unwrap().results.iter().all(|r| r.ok));
        assert_eq!(f.shared.settings().threshold_amps, Settings::default().threshold_amps);
        assert!(f.shared.settings().oc_power_enabled);
        let keys = saved_keys.lock().unwrap().clone();
        assert!(!keys.contains(&"threshold_amps".to_string()), "{keys:?}");
        assert!(keys.contains(&"oc_power_w".to_string()), "{keys:?}");
    }

    /// A panic between the save and the answer must not leave every later Apply now refused.
    #[test]
    fn a_panicking_apply_now_does_not_block_the_next_one() {
        let f = fake_service(test_marker("panic-flag"), Settings::default());
        let boom = |_: &Settings, _: &[&str]| -> std::io::Result<()> { panic!("save exploded") };
        let shared = Arc::clone(&f.shared);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shared.apply_overclock_with(&power_520(), &boom, APPLY_NOW_WAIT)
        }));
        assert!(r.is_err());
        assert!(!f.shared.oc_apply_running.load(Ordering::SeqCst), "cleared on unwind");
    }

    #[test]
    fn apply_overclock_with_an_invalid_value_writes_nothing() {
        let f = fake_service(test_marker("invalid"), Settings::default());
        let changes = SettingsPatch { oc_power_enabled: Some(true), oc_power_w: Some(-5.0), ..Default::default() };
        let report = apply_now(&f.shared, changes);
        assert!(!report.results.is_empty() && report.results.iter().all(|r| !r.ok), "{report:?}");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert!(!f.shared.settings().oc_power_enabled);
    }

    #[test]
    fn apply_overclock_that_cannot_be_saved_is_taken_back_and_writes_nothing() {
        let f = fake_service(test_marker("unsaved"), Settings::default());
        let disk_full = |_: &Settings, _: &[&str]| Err(std::io::Error::other("disk full"));
        let report = f.shared.apply_overclock_with(&power_520(), &disk_full, std::time::Duration::from_secs(5));
        assert!(!report.results.is_empty() && report.results.iter().all(|r| !r.ok), "{report:?}");
        let msg = &report.results[0].message;
        assert!(msg.contains("not saved to config.ini (disk full)") && msg.contains("nothing written"), "{msg}");
        let s = f.shared.settings();
        assert!(!s.oc_power_enabled, "back to the value before");
        assert_eq!(s.oc_power_w, Settings::default().oc_power_w);
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn apply_overclock_without_a_card_says_why() {
        let f = fake_service(test_marker("nocard"), Settings::default());
        f.shared.update(|l| l.card_subsystem = None);
        let report = apply_now(&f.shared, power_520());
        assert!(!report.results.is_empty() && report.results.iter().all(|r| !r.ok), "{report:?}");
        assert!(report.results[0].message.contains("no card connected"), "{report:?}");
        // Saved anyway: the start-up apply or the next Apply now writes it once the card is there.
        assert!(f.shared.settings().oc_power_enabled);
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn saving_other_settings_never_touches_the_gpu() {
        let f = fake_service(test_marker("other"), Settings::default());
        let changes = SettingsPatch { threshold_amps: Some(9.0), ..Default::default() };
        let Response::Applied { failures } = f.shared.handle(Request::Apply { changes }) else {
            panic!("not an apply answer")
        };
        assert!(failures.is_empty(), "{failures:?}");
        // Even an oc_* key saved through the Settings path only saves.
        let Response::Applied { failures } = f.shared.handle(Request::Apply { changes: power_520() }) else {
            panic!("not an apply answer")
        };
        assert!(failures.is_empty(), "{failures:?}");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0, "the GPU was not even opened");
    }

    fn status(shared: &Shared) -> OcStatus {
        let Response::OverclockStatus { status } = shared.handle(Request::OverclockStatus) else {
            panic!("not a status answer")
        };
        status
    }

    #[test]
    fn the_status_request_opens_the_gpu_once_per_card() {
        let f = fake_service(test_marker("status"), Settings::default());
        f.shared.update(|l| l.card_subsystem = None);
        let st = status(&f.shared);
        assert!(!st.available);
        assert!(st.reason.contains("no card connected"), "{st:?}");
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);

        f.shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        let st = status(&f.shared);
        assert!(st.available, "{st:?}");
        assert_eq!(st.power_w, Some(600));
        status(&f.shared);
        assert_eq!(f.opened.load(Ordering::SeqCst), 1, "kept open for the same card");

        // Another card after a reconnect gets its own overclocker.
        f.shared.update(|l| l.card_subsystem = Some(0x12345678));
        status(&f.shared);
        assert_eq!(f.opened.load(Ordering::SeqCst), 2);
        assert!(f.gpu.calls().is_empty(), "a status only reads: {:?}", f.gpu.calls());
    }

    #[test]
    fn a_gpu_that_will_not_open_is_reported_not_fatal() {
        let marker = test_marker("noopen");
        let shared = Shared::with_overclock_factory(
            Settings::default(),
            marker,
            Box::new(|_| Err("NVML not found".to_string())),
        );
        shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        let st = status(&shared);
        assert!(!st.available && st.reason == "NVML not found", "{st:?}");
        let report = apply_now(&shared, power_520());
        assert!(report.results[0].message.contains("NVML not found"), "{report:?}");
    }

    #[test]
    fn a_slow_overclock_call_on_the_monitor_thread_is_noted() {
        assert_eq!(slow_call_note("Apply now", std::time::Duration::from_millis(250)), None);
        let note = slow_call_note("the start-up apply", std::time::Duration::from_millis(1830)).unwrap();
        assert!(note.contains("the start-up apply") && note.contains("1830 ms"), "{note}");
    }

    #[test]
    fn a_failed_open_is_logged_once_per_error() {
        let f = fake_service(test_marker("openlog"), Settings::default());
        assert!(f.shared.note_open_result(Some("NVML not found")));
        assert!(!f.shared.note_open_result(Some("NVML not found")), "the same error again: quiet");
        assert!(f.shared.note_open_result(Some("no GPU with subsystem 0x89E31043")), "a new error is logged");
        f.shared.note_open_result(None);
        assert!(f.shared.note_open_result(Some("NVML not found")), "an open in between re-arms it");
    }

    /// Fresh marker and config.ini paths for `name`.
    fn oc_files(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let cfg = std::env::temp_dir().join(format!("ag-svc-oc-{}-{name}.ini", std::process::id()));
        let _ = fs::remove_file(&cfg);
        (test_marker(name), cfg)
    }

    /// A service whose overclocker reads and writes the driver checks in `cfg`, as the real one
    /// does in config.ini; the undervolt ticked.
    fn service_on(gpu: &FakeGpu, marker: &std::path::Path, cfg: &std::path::Path) -> Arc<Shared> {
        let (g, m, c) = (gpu.clone(), marker.to_path_buf(), cfg.to_path_buf());
        let factory = move |_: u32| -> Result<Box<dyn OcDriver + Send>, String> {
            let (verified, failed) = (config::load_oc_verified(&c), config::load_oc_vf_failed(&c));
            Ok(Box::new(Overclocker::new(g.clone(), m.clone(), verified, failed)))
        };
        let settings = Settings { oc_uv_enabled: true, ..Settings::default() };
        let shared =
            Shared::with_overclock_config(settings, marker.to_path_buf(), Box::new(factory), cfg.to_path_buf());
        shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        shared
    }

    fn apply_on_the_loop(shared: &Shared) -> OcReport {
        let s = shared.settings();
        match shared.try_overclock("test", |d| d.apply(&s, false)) {
            OcAccess::Ran(r) => r,
            _ => panic!("the overclocker did not run"),
        }
    }

    #[test]
    fn a_probe_mismatch_is_saved_and_a_later_run_does_not_probe_again() {
        let gpu = FakeGpu::owners();
        gpu.state().driver = "621.01".into();
        gpu.state().delta_not_effective = true;
        let (marker, cfg) = oc_files("failed-saved");
        let shared = service_on(&gpu, &marker, &cfg);
        assert!(apply_on_the_loop(&shared).results.iter().all(|r| !r.ok));
        assert_eq!(config::load_oc_vf_failed(&cfg).as_deref(), Some("621.01"));
        assert_eq!(config::load_oc_verified(&cfg), None);
        // A restart: a new service on the same config.ini.
        let shared = service_on(&gpu, &marker, &cfg);
        gpu.clear_calls();
        let r = apply_on_the_loop(&shared);
        assert!(r.results.iter().all(|r| !r.ok), "{r:?}");
        assert!(gpu.calls().is_empty(), "no test write on a known-bad driver: {:?}", gpu.calls());
        let _ = fs::remove_file(&cfg);
        let _ = fs::remove_file(&marker);
    }

    #[test]
    fn a_probe_call_error_is_not_saved() {
        let gpu = FakeGpu::owners();
        gpu.state().driver = "621.01".into();
        gpu.state().curve_write_error = true;
        let (marker, cfg) = oc_files("failed-call");
        let shared = service_on(&gpu, &marker, &cfg);
        assert!(apply_on_the_loop(&shared).results.iter().all(|r| !r.ok));
        assert_eq!(config::load_oc_vf_failed(&cfg), None);
        let _ = fs::remove_file(&cfg);
        let _ = fs::remove_file(&marker);
    }

    #[test]
    fn managed_settings_come_from_the_marker_without_opening_the_gpu() {
        let marker = test_marker("managed");
        let gpu = FakeGpu::owners();
        Overclocker::new(gpu.clone(), marker.clone(), None, None)
            .apply(&Settings { oc_power_enabled: true, ..Settings::default() }, false);
        let f = fake_service(marker, Settings::default());
        assert_eq!(f.shared.overclock_managed(), vec!["power".to_string()]);
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);
        f.shared.update(|l| l.card_subsystem = Some(0x12345678));
        assert!(f.shared.overclock_managed().is_empty(), "another card's marker says nothing here");
    }
}
