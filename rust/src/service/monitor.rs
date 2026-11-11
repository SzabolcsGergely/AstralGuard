//! The monitor loop: read the pins every 0.5 s, run the tier 1 timer, the imbalance check
//! and the response ladder, and act on what the ladder decides.

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::crosscheck::CrossCheck;
use super::flight::FlightRecorder;
use super::imbalance::ImbalanceMonitor;
use super::response::{Tick, TieredResponse, CLEAR_HYSTERESIS_AMPS};
use super::sag::SagMonitor;
use super::Shared;
use super::{oc_ticked, OcAccess};
use crate::config;
use crate::logger;
use crate::mitigation::GpuMitigator;
use crate::platform::{self, EventKind};
use crate::protocol::{Status, APP_NAME, APP_VERSION};
use crate::sensor::{max_pin, min_pin, PinSensor, ReadError};

pub const POLL_INTERVAL_SEC: f64 = 0.5;
/// A gap this long between samples means suspend/resume or a severe stall, not a
/// measurement: sustained timers are reset rather than trusted across it.
const RESUME_GRACE_SEC: f64 = 5.0;
const SUSTAINED_PROGRESS_LOG_EVERY_SEC: f64 = 5.0;
const CLEAR_STABLE_SECONDS: f64 = 1.0;
/// A paused guard protects nothing; remind at a noticeable but bearable cadence.
const PAUSE_REMINDER_EVERY_SEC: f64 = 1800.0;
const RECONNECT_EVERY_SEC: f64 = 3.0;
/// How often a start-up apply whose GPU would not open is tried again.
const OC_OPEN_RETRY_SEC: f64 = 30.0;
/// How often the XBAR / SYS load gate reads the GPU's power (an NVML read, no NVAPI).
const LOAD_POLL_SEC: f64 = 1.0;

fn sleep(sec: f64) {
    std::thread::sleep(Duration::from_secs_f64(sec));
}

#[derive(Serialize, Deserialize, Default)]
struct ShutdownNote {
    #[serde(default)]
    time: f64,
    #[serde(default)]
    time_human: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    version: String,
}

fn shutdown_note_path() -> std::path::PathBuf {
    config::data_dir().join("last_shutdown.json")
}

/// Leave a note for the next start: a shutdown while the user is away is otherwise a
/// mystery, Windows gets the blame, and the bad cable goes straight back under load.
fn note_shutdown_reason(reason: &str) {
    let note = ShutdownNote {
        time: crate::now(),
        time_human: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        reason: reason.into(),
        version: APP_VERSION.into(),
    };
    let path = shutdown_note_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(&note) {
        use std::io::Write;
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(&bytes);
            let _ = f.sync_all();
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// One-shot at startup: if the guard ended the last session, say so loudly. A corrupt
/// note still announces, because it still means the guard shut this machine down.
fn announce_last_shutdown(shared: &Shared) {
    let path = shutdown_note_path();
    if !path.exists() {
        return;
    }
    let note: ShutdownNote = fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let when = if note.time_human.is_empty() { "an earlier session".to_string() } else { note.time_human };
    let reason =
        if note.reason.is_empty() { "sustained overcurrent on a 12VHPWR pin".to_string() } else { note.reason };
    let msg = format!(
        "The last shutdown was an emergency stop by {APP_NAME} ({when}): {reason} Check the connector and \
         cable seating before the next heavy load."
    );
    logger::warn(&msg);
    shared.event(EventKind::Warning, 5091, &msg);
    shared.notify(&format!("{APP_NAME} - read this"), &msg);
    let _ = fs::remove_file(path.with_extension("json.tmp"));
    let _ = fs::remove_file(path);
}

fn shutdown_system(shared: &Shared, reason: &str) {
    logger::critical(format!("SHUTDOWN TRIGGERED: {reason}"));
    note_shutdown_reason(reason);
    shared.notify(&format!("{APP_NAME} - Shutdown"), reason);
    shared.event(EventKind::Error, 5090, reason);
    if let Err(e) = platform::shutdown_system() {
        logger::critical(format!("Shutdown command failed: {e}"));
    }
}

/// Undo a GPU limit left behind by a previous run that died while limited. Touches NVML
/// only when the marker file is there.
fn startup_recovery(shared: &Shared) {
    if !crate::mitigation::marker_path().exists() {
        return;
    }
    let mut m = GpuMitigator::default();
    if let Some(msg) = m.startup_recovery() {
        logger::warn(&msg);
        shared.event(EventKind::Warning, 6006, &msg);
        shared.notify(APP_NAME, "Restored GPU settings left behind by a previous run.");
    }
    m.close();
}

/// What the overclock should do this loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcAction {
    None,
    /// The start-up apply, once per service start.
    Apply,
    /// The guard released the GPU: put the saved overclock back where the release changed it.
    AfterRelease,
}

/// When the overclock runs: once per service start after the configured delay, again the same
/// delay after each resume from sleep, never while the guard limits the GPU, and after each
/// release once the start-up apply has run.
struct OcTimer {
    due: f64,
    applied: bool,
    release_pending: bool,
}

impl OcTimer {
    fn new(start: f64, delay: f64) -> Self {
        OcTimer { due: start + delay, applied: false, release_pending: false }
    }

    /// The start-up apply could not open the GPU: try again in `OC_OPEN_RETRY_SEC` (a driver
    /// that comes up late), not every iteration. Release checks are not retried this way; the
    /// next release tries again.
    fn open_failed(&mut self, now: f64) {
        self.applied = false;
        self.due = now + OC_OPEN_RETRY_SEC;
    }

    /// The action could not run (no GPU yet, or an Apply now held it): ask again next time.
    /// The machine slept: the driver puts the power limit back to stock on resume (VERIFIED
    /// 2026-11-10, ROG Astral RTX 5090 OC, driver 620.70.06, Linux; the memory offset and the
    /// curve survived that resume), so the full apply runs again after the configured delay,
    /// which also lets the driver settle. Values still in effect are only logged as such.
    fn resumed(&mut self, now: f64, delay: f64) {
        self.applied = false;
        self.due = now + delay;
    }

    fn retry(&mut self, action: OcAction) {
        match action {
            OcAction::None => {}
            OcAction::Apply => self.applied = false,
            OcAction::AfterRelease => self.release_pending = true,
        }
    }
}

fn oc_step(t: &mut OcTimer, now: f64, engaged: bool, released_now: bool) -> OcAction {
    if released_now {
        t.release_pending = true;
    }
    if engaged {
        // Limited again before a pending check ran: the next release asks for it again.
        t.release_pending = false;
        return OcAction::None;
    }
    if !t.applied {
        // Before the start-up apply a release has nothing to put back; that apply is the full one.
        t.release_pending = false;
        if now >= t.due {
            t.applied = true;
            return OcAction::Apply;
        }
        return OcAction::None;
    }
    if std::mem::take(&mut t.release_pending) {
        return OcAction::AfterRelease;
    }
    OcAction::None
}

/// The overclock's bookkeeping in the monitor loop.
struct OcLoop {
    timer: OcTimer,
    was_engaged: bool,
    /// When this step last ran. The loop runs it every iteration, sensor or not, so a gap longer
    /// than `RESUME_GRACE_SEC` means the machine slept (or the loop stalled; re-applying is
    /// harmless then).
    last_step: Option<f64>,
    /// XBAR / SYS "only under load".
    gate: crate::overclock::load::LoadGate,
    next_power_read: f64,
}

impl OcLoop {
    fn new(now: f64, delay: f64) -> Self {
        OcLoop {
            timer: OcTimer::new(now, delay),
            was_engaged: false,
            last_step: None,
            gate: Default::default(),
            next_power_read: 0.0,
        }
    }
}

/// After the pin decision of an iteration: a pending Apply now, the start-up apply and the
/// check after a release. Nothing in the overclock may end the protection loop, so a panic in
/// it is caught and logged here (and returned, for the tests); a waiting Apply now then gets a
/// failure, its reply channel being dropped.
fn overclock_step(shared: &Shared, oc: &mut OcLoop, engaged: bool) -> Option<String> {
    let result = catch_unwind(AssertUnwindSafe(|| overclock_step_inner(shared, oc, engaged)));
    // From the end of the step: its own driver calls are not a sleep either.
    oc.last_step = Some(crate::now());
    let panic = result.err()?;
    let what = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".into());
    let msg = format!("Overclock error (protection carries on): {what}");
    logger::error(&msg);
    shared.event(EventKind::Error, 9001, &msg);
    Some(what)
}

/// The GPU is touched only for Apply now (asked for), and for the start-up apply and the release
/// check only when an overclock setting is ticked or AstralGuard still manages one (an untick to
/// restore); otherwise no NVML or NVAPI call at all. Never waits for the overclocker.
fn overclock_step_inner(shared: &Shared, oc: &mut OcLoop, engaged: bool) {
    // With this loop's own `engaged`: the write is ordered against the ladder's decisions.
    shared.serve_apply_now(engaged);
    let released_now = oc.was_engaged && !engaged;
    oc.was_engaged = engaged;
    let now = crate::now();
    if oc.last_step.is_some_and(|last| now - last > RESUME_GRACE_SEC) {
        let delay = shared.settings().oc_apply_delay_seconds;
        if oc_ticked(&shared.settings()) {
            logger::info(format!("Overclock: resume from sleep detected, applying again in {delay:.0} s."));
        }
        oc.timer.resumed(now, delay);
    }
    oc.last_step = Some(now);
    let action = oc_step(&mut oc.timer, now, engaged, released_now);
    if action == OcAction::None {
        load_step(shared, oc, engaged, now);
        return;
    }
    let s = shared.settings();
    if !oc_ticked(&s) && shared.overclock_managed().is_empty() {
        return;
    }
    // The overclocker logs its own results; a failed open is logged once by `Shared`.
    let access = match action {
        OcAction::None => return,
        // "start-up apply" covers its retries (no GPU yet, or one that would not open).
        OcAction::Apply => shared.try_overclock("the start-up/after-resume apply", |d| d.apply(&s, engaged)).ran(),
        OcAction::AfterRelease => shared.try_overclock("the after-release apply", |d| d.after_release(&s)).ran(),
    };
    match access {
        // A status read holds it, or the card is not found yet: cheap to ask again next time.
        OcAccess::Busy | OcAccess::NoCard => oc.timer.retry(action),
        OcAccess::Unavailable(_) if action == OcAction::Apply => oc.timer.open_failed(crate::now()),
        OcAccess::Unavailable(_) | OcAccess::Ran(()) => {}
    }
}

/// XBAR / SYS "only under load": once a second read what the GPU draws, and when the load
/// gate flips, apply or put back the offsets. Only after the start-up apply (the driver has
/// settled), never while the guard limits the GPU (it holds the gate; the after-release apply
/// follows the overclocker's state). A power read that fails, or an overclocker held by a
/// status read, leaves the gate as it is.
fn load_step(shared: &Shared, oc: &mut OcLoop, engaged: bool, now: f64) {
    let s = shared.settings();
    if !(s.oc_clk_load_only && (s.oc_xbar_enabled || s.oc_sys_enabled)) {
        oc.gate.reset();
        return;
    }
    if engaged || !oc.timer.applied || now < oc.next_power_read {
        return;
    }
    oc.next_power_read = now + LOAD_POLL_SEC;
    let gate = &mut oc.gate;
    let _ = shared.try_overclock("the XBAR / SYS load check", |d| {
        let Ok(w) = d.power_draw_w() else { return };
        let flipped = gate.feed(now, w as f64, s.oc_clk_load_watts, s.oc_clk_load_seconds, s.oc_clk_idle_seconds);
        // Also when the overclocker disagrees (reopened after a reconnect, say).
        if flipped.is_some() || d.loaded() != gate.loaded() {
            let loaded = gate.loaded();
            logger::info(format!(
                "Overclock: GPU {} ({w} W); {} the XBAR / SYS offsets",
                if loaded { "under load" } else { "idle" },
                if loaded { "applying" } else { "putting back" }
            ));
            d.set_loaded(&s, loaded, engaged);
        }
    });
}

/// The log line for a trip while AstralGuard has overclock settings applied.
fn trip_note(managed: &[String]) -> Option<String> {
    (!managed.is_empty()).then(|| format!("overclock active: {}", managed.join(", ")))
}

/// The flight dump's event (its file name says why it was written), with the overclock noted.
fn flight_event(base: &str, managed: &[String]) -> String {
    if managed.is_empty() {
        base.to_string()
    } else {
        format!("{base}-overclock-{}", managed.join("-"))
    }
}

/// Timers that restart from scratch on a new connection or after a resume gap.
#[derive(Default)]
struct Tier1 {
    over_start: Option<f64>,
    consecutive_over: u32,
    clear_start: Option<f64>,
    last_over_notice: f64,
}

/// Since when protection has been paused, and when the user was last reminded.
#[derive(Default)]
struct Pause {
    since: Option<f64>,
    last_reminder: f64,
}

/// One loop iteration while protection is paused: no pin decisions, a GPU limit in place
/// released, a reminder every half hour. Checked before connecting, so a paused guard stays
/// quiet about missing sensors. Pausing is the protection's only: the overclock keeps its
/// start-up apply, Apply now, the after-release apply and the XBAR / SYS load gate.
fn paused_tick(
    shared: &Shared,
    response: &mut TieredResponse,
    oc: &mut OcLoop,
    pause: &mut Pause,
    now: f64,
    s: &crate::protocol::Settings,
) {
    let since = *pause.since.get_or_insert_with(|| {
        pause.last_reminder = now;
        now
    });
    if response.engaged {
        // Pausing hands the decision back to the user; leaving their GPU crippled
        // while no longer watching it is not ours to do.
        response.release(now, "protection paused", false, s);
    }
    if now - pause.last_reminder >= PAUSE_REMINDER_EVERY_SEC {
        let msg = format!(
            "Protection has been paused for {} minutes - the card is unprotected until you resume.",
            ((now - since) / 60.0) as i64
        );
        logger::warn(&msg);
        shared.notify(&format!("{APP_NAME} - still paused"), &msg);
        pause.last_reminder = now;
    }
    shared.update(|l| {
        l.status = Status::Paused;
        l.last_message = "Paused".into();
        l.engaged = response.engaged;
        l.engaged_tier = response.engaged_tier;
        l.engaged_at = response.engaged_at;
    });
    let _ = overclock_step(shared, oc, response.engaged);
}

/// Runs until the guard is told to stop or shuts the machine down. True when it fired a
/// shutdown.
pub fn run(shared: &Arc<Shared>) -> bool {
    if let Err(e) = platform::register_event_source() {
        logger::warn(format!("Could not register the Event Viewer source (needs admin once): {e}"));
    }
    startup_recovery(shared);
    announce_last_shutdown(shared);

    let s = shared.settings();
    logger::info(format!("Starting {APP_NAME} v{APP_VERSION} (service)..."));
    logger::info(format!("Log file: {}", logger::path().map(|p| p.display().to_string()).unwrap_or_default()));
    logger::info(format!(
        "Threshold={:.2}A | Poll={POLL_INTERVAL_SEC}s | Sustained={}s | ForceClose=True",
        s.threshold_amps,
        config::g(s.sustained_seconds_required)
    ));
    logger::info(format!(
        "Response mode={} | Tier 2 critical={:.2}A/{:.0}s | Tier 3 catastrophic={:.2}A",
        s.response_mode.key(),
        s.critical_amps,
        s.critical_seconds,
        s.catastrophic_amps
    ));
    // The watchdog restarts a guard that died and says why: a crash must not pass
    // silently, the connector was unprotected until the restart.
    if let Ok(restart) = std::env::var("GUARD_WATCHDOG_RESTART") {
        let msg = format!("Restarted by the watchdog: the previous run ended unexpectedly ({restart}).");
        logger::warn(&msg);
        shared.notify(APP_NAME, "Restarted after a crash. Monitoring again.");
        shared.event(EventKind::Warning, 1005, &msg);
    } else {
        shared.notify(APP_NAME, "Monitoring started.");
        shared.event(EventKind::Info, 1000, &format!("{APP_NAME} started."));
    }
    if !shared.watchdog {
        logger::info("Running without the watchdog: a crash will not be restarted.");
    }

    let factory_shared = Arc::clone(shared);
    let mut response = TieredResponse::new(
        Arc::clone(shared),
        Box::new(move |subsystem| {
            let mut m = GpuMitigator::default();
            m.connect(subsystem);
            let _ = &factory_shared;
            Box::new(m)
        }),
    );
    let mut sensor: Option<PinSensor> = None;
    let mut crosscheck = CrossCheck::new();
    let mut flight = FlightRecorder::new();
    let baseline = *shared.baseline.lock().unwrap_or_else(|e| e.into_inner());
    let mut sag = SagMonitor::new(Arc::clone(shared), baseline);
    let mut imbalance = ImbalanceMonitor::new(Arc::clone(shared));
    let mut t1 = Tier1::default();
    let mut last_sample: Option<f64> = None;
    let mut missing_notified = false;
    let mut missing_logged = false;
    // The automatic sensor scan runs at most once per time the sensor cannot be read;
    // a successful connection re-arms it.
    let mut auto_scanned = false;
    let mut connected_notified = false;
    let mut pause = Pause::default();
    let mut shutting_down = false;
    let mut oc = OcLoop::new(crate::now(), s.oc_apply_delay_seconds);

    while !shared.stop.load(Ordering::Relaxed) {
        let now = crate::now();
        let s = shared.settings();
        // Every iteration, before any path that `continue`s: the dashboard's "loop not responding".
        shared.update(|l| l.loop_at = now);

        if shared.paused.load(Ordering::Relaxed) {
            paused_tick(shared, &mut response, &mut oc, &mut pause, now, &s);
            sleep(0.5);
            continue;
        }
        if let Some(since) = pause.since.take() {
            logger::info(format!("Protection resumed after {} min paused.", ((now - since) / 60.0) as i64));
        }

        // (Re)connect whenever there is no live sensor.
        if sensor.is_none() {
            shared.update(|l| {
                l.status = Status::Error;
                l.last_message = "Waiting for pin sensors...".into();
            });
            if !missing_logged {
                logger::info("Waiting for the card's pin sensors...");
            }
            // A sensor scan holds the bus lock while it talks to the adapters: no connect then
            // (try_lock: the loop never waits for a scan). Held until the card is published, so
            // a scan that starts right after sees a connected sensor and refuses.
            let bus = shared.bus.try_lock();
            let attempt = match &bus {
                Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => Some(PinSensor::connect()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
            match attempt {
                None => {
                    drop(bus);
                    wait_or_stop(shared, 0.5);
                    continue;
                }
                Some(Ok(new)) => {
                    let msg = format!("Connected: {}", new.description());
                    logger::info(&msg);
                    shared.event(EventKind::Info, 1002, &msg);
                    if !connected_notified {
                        shared.notify(APP_NAME, &format!("Monitoring {}.", new.info.model));
                        connected_notified = true;
                    }
                    response.subsystem = Some(new.info.subsystem);
                    let (model, sub) = (new.info.model.to_string(), new.info.subsystem);
                    // Read again on every connect: a driver update or reset reconnects.
                    let (os, driver) = (platform::os_version(), platform::gpu_driver());
                    shared.update(|l| {
                        l.card = model;
                        l.card_subsystem = Some(sub);
                        l.os = os;
                        l.driver = driver;
                        l.sensor_scan_suggested = false;
                        l.sensor_problem.clear();
                    });
                    sensor = Some(new);
                    missing_notified = false;
                    missing_logged = false;
                    auto_scanned = false;
                    drop(bus);
                    // A new connection starts its timers from scratch.
                    t1 = Tier1::default();
                    last_sample = None;
                }
                Some(Err(detail)) => {
                    // Released before an automatic scan, which takes it.
                    drop(bus);
                    // A card that is there but unreadable is worth a scan; no card is not.
                    let scan = crate::sensor::scan_possible();
                    shared.update(|l| {
                        l.sensor_scan_suggested = scan;
                        l.sensor_problem = detail.clone();
                    });
                    if scan && !auto_scanned && shared.settings().sensor_auto_scan {
                        auto_scanned = true;
                        if shared.scan_sensor("automatically (the setting is on)").found {
                            shared.reconnect_now.store(false, Ordering::Relaxed);
                            continue;
                        }
                    }
                    if !missing_notified {
                        shared.notify(
                            APP_NAME,
                            if scan {
                                "The card was found, but its pin sensors give no valid readings. Open the dashboard and press Scan."
                            } else {
                                "Cannot read the card's pin sensors. Only ASUS ROG Astral cards and derivatives are supported."
                            },
                        );
                        missing_notified = true;
                        connected_notified = false;
                    }
                    if !missing_logged {
                        logger::warn(format!("No sensor source available: {detail}"));
                        missing_logged = true;
                    }
                    wait_or_stop(shared, RECONNECT_EVERY_SEC);
                    continue;
                }
            }
        }

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            tick(
                shared,
                sensor.as_mut().unwrap(),
                &mut response,
                &mut t1,
                &mut last_sample,
                &mut crosscheck,
                &mut flight,
                &mut imbalance,
                &mut sag,
            )
        }));
        match outcome {
            Ok(TickOutcome::Continue) => {}
            Ok(TickOutcome::Transient(e)) => {
                // One bad sample: skip the tick without disturbing any timer.
                logger::debug(format!("Transient read error: {e}"));
                sleep(POLL_INTERVAL_SEC);
            }
            Ok(TickOutcome::Gone(e)) => {
                let msg = format!("Sensor source lost: {e}");
                logger::warn(&msg);
                shared.event(EventKind::Warning, 1004, &msg);
                shared.notify(APP_NAME, "Sensor source lost. Waiting to reconnect...");
                if t1.over_start.is_some() {
                    logger::warn("Sustained over-threshold timer reset by sensor disconnect.");
                }
                if response.engaged {
                    // Deliberately stays limited: losing the ability to measure is not
                    // evidence the current stopped being dangerous. It releases normally
                    // once readings come back and stay clean.
                    logger::warn(
                        "GPU stays limited while the sensor is gone; it will be released once clean readings return.",
                    );
                }
                shared.update(|l| {
                    l.status = Status::Error;
                    l.last_message = "Sensor source lost".into();
                    l.card.clear();
                });
                sensor = None;
                last_sample = None;
                missing_notified = false;
                missing_logged = false;
                connected_notified = false;
            }
            Ok(TickOutcome::Shutdown(reason)) => {
                shared.update(|l| {
                    l.status = Status::Shutdown;
                    l.last_message = "Shutting down...".into();
                });
                shutting_down = true;
                // From the marker, like the engage dump: no driver call on the way down.
                flight.dump(&flight_event("shutdown", &shared.overclock_managed()));
                shutdown_system(shared, &reason);
                break;
            }
            Err(panic) => {
                let what = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown panic".into());
                logger::error(format!("Loop error: {what}"));
                shared.event(EventKind::Error, 9001, &format!("Loop error: {what}"));
                shared.update(|l| {
                    l.status = Status::Error;
                    l.last_message = format!("Error: {what}");
                });
                sleep(1.0);
            }
        }
        // Catches and logs its own panics. Its driver calls run on this thread, so the time they
        // take is not a gap between samples: a slow call (over RESUME_GRACE_SEC) must delay the
        // tier timers, never reset them as if the machine had slept.
        let started = crate::now();
        let _ = overclock_step(shared, &mut oc, response.engaged);
        if let Some(t) = last_sample.as_mut() {
            *t += crate::now() - started;
        }
    }

    sag.close();
    drop(sensor);
    response.finish(shutting_down);
    logger::info(format!("{APP_NAME} stopped."));
    shared.event(EventKind::Info, 1003, &format!("{APP_NAME} stopped."));
    shutting_down
}

/// Sleep up to `seconds`; returns early to stop, or when a sensor scan saved an adapter.
fn wait_or_stop(shared: &Shared, seconds: f64) {
    let end = crate::now() + seconds;
    while crate::now() < end && !shared.stop.load(Ordering::Relaxed) {
        if shared.reconnect_now.swap(false, Ordering::Relaxed) {
            return;
        }
        sleep(0.1);
    }
}

enum TickOutcome {
    Continue,
    Transient(String),
    Gone(String),
    Shutdown(String),
}

#[allow(clippy::too_many_arguments)]
fn tick(
    shared: &Arc<Shared>,
    sensor: &mut PinSensor,
    response: &mut TieredResponse,
    t1: &mut Tier1,
    last_sample: &mut Option<f64>,
    crosscheck: &mut CrossCheck,
    flight: &mut FlightRecorder,
    imbalance: &mut ImbalanceMonitor,
    sag: &mut SagMonitor,
) -> TickOutcome {
    let pins = match sensor.read_pins() {
        Ok(p) => p,
        Err(ReadError::Transient(e)) => return TickOutcome::Transient(e),
        Err(ReadError::Gone(e)) => return TickOutcome::Gone(e),
    };
    let s = shared.settings();

    // A long gap means the machine slept or stalled badly, not that current was high the
    // whole time; readings right after a resume are not trustworthy either.
    let sample_now = crate::now();
    if let Some(last) = *last_sample {
        let gap = sample_now - last;
        if gap > RESUME_GRACE_SEC {
            logger::warn(format!("Sample gap {gap:.1}s detected (suspend/resume?) - sustained timer reset."));
            *t1 = Tier1::default();
            if response.engaged {
                response.release(sample_now, "suspend/resume gap", false, &s);
            }
            response.reset_timers();
        }
    }
    *last_sample = Some(sample_now);

    crosscheck.maybe_log(&pins, sample_now);
    flight.record(sample_now, &pins, shared.live_field(|l| l.sag_deg));

    let (Some(max), Some(min)) = (max_pin(&pins).cloned(), min_pin(&pins).cloned()) else {
        return TickOutcome::Transient("no pins in the sample".into());
    };
    shared.update(|l| {
        l.max_label = max.name.clone();
        l.max_val = max.amps;
        l.min_label = min.name.clone();
        l.min_val = Some(min.amps);
        l.pins = pins.iter().map(|p| (p.name.clone(), p.amps)).collect();
        l.volts = Some(pins.iter().map(|p| p.volts).collect());
    });
    shared.record_pins(sample_now, pins.iter().map(|p| p.amps).collect());

    // Warn-only and independent of the tiers.
    imbalance.evaluate(&pins, crate::now(), &s);

    let now = crate::now();
    let threshold = s.threshold_amps;
    let over = max.amps >= threshold;
    let cleared = max.amps < threshold - CLEAR_HYSTERESIS_AMPS;
    let mut tier1_expired = false;
    let mut tier1_reason = String::new();

    if over {
        shared.update(|l| {
            l.status = Status::Over;
            l.last_message = format!("Over: {} {:.2}A", max.name, max.amps);
        });
        t1.consecutive_over += 1;
        t1.clear_start = None;
        if t1.over_start.is_none() {
            t1.over_start = Some(now);
            t1.consecutive_over = 1;
            t1.last_over_notice = 0.0;
            let msg = format!(
                "Threshold exceeded: {}={:.2}A >= {threshold:.2}A. Starting {:.0}s sustained timer.",
                max.name, max.amps, s.sustained_seconds_required
            );
            logger::warn(&msg);
            shared.event(EventKind::Warning, 3001, &msg);
            shared.notify(&format!("{APP_NAME} - Warning"), &msg);
        }
        let over_start = t1.over_start.unwrap();
        if now - t1.last_over_notice >= SUSTAINED_PROGRESS_LOG_EVERY_SEC {
            let msg = format!(
                "Still over threshold for {:.1}s: {}={:.2}A (needs {:.1}s)",
                now - over_start,
                max.name,
                max.amps,
                s.sustained_seconds_required
            );
            logger::warn(&msg);
            shared.event(EventKind::Warning, 3002, &msg);
            t1.last_over_notice = now;
        }
        let sustained = now - over_start;
        if sustained >= s.sustained_seconds_required {
            // What happens next is the ladder's call.
            tier1_expired = true;
            tier1_reason = format!(
                "sustained {sustained:.1}s >= {:.1}s, {}={:.2}A >= {threshold:.2}A",
                s.sustained_seconds_required, max.name, max.amps
            );
        }
    } else if cleared {
        shared.update(|l| {
            l.status = Status::Ok;
            l.last_message = format!("Safe (max {:.2}A)", max.amps);
        });
        t1.consecutive_over = 0;
        match t1.clear_start {
            None => t1.clear_start = Some(now),
            Some(start) if now - start >= CLEAR_STABLE_SECONDS => {
                if t1.over_start.is_some() {
                    let msg = format!(
                        "Load cleared below {:.2}A for {CLEAR_STABLE_SECONDS:.1}s. Timer reset.",
                        threshold - CLEAR_HYSTERESIS_AMPS
                    );
                    logger::info(&msg);
                    shared.event(EventKind::Info, 4001, &msg);
                    shared.notify(APP_NAME, "Load cleared. Timer reset.");
                }
                t1.over_start = None;
                t1.last_over_notice = 0.0;
            }
            Some(_) => {}
        }
    } else {
        shared.update(|l| {
            l.status = Status::Ok;
            l.last_message = format!("Safe (near threshold: {:.2}A)", max.amps);
        });
        t1.consecutive_over = 0;
        t1.clear_start = None;
    }

    // imbalance_response = act: a sustained imbalance counts as a tier 1 crossing.
    let imbalance_reason = imbalance.action_reason(&s).to_string();
    if !imbalance_reason.is_empty() && !tier1_expired {
        tier1_expired = true;
        tier1_reason = imbalance_reason;
    }

    let was_engaged = response.engaged;
    let shutdown = response.evaluate(
        &Tick { now, max_val: max.amps, max_label: &max.name, tier1_expired, tier1_reason: &tier1_reason },
        &s,
    );
    shared.update(|l| {
        l.engaged = response.engaged;
        l.engaged_tier = response.engaged_tier;
        l.engaged_at = response.engaged_at;
    });
    if response.engaged {
        shared.update(|l| {
            l.status = Status::Mitigated;
            l.last_message = format!("GPU limited: {} {:.2}A", max.name, max.amps);
        });
        if !was_engaged {
            // From the overclocker's marker: no driver call in the pin path, and never waits.
            let managed = shared.overclock_managed();
            if let Some(note) = trip_note(&managed) {
                logger::warn(format!("GPU limited with {note}"));
            }
            // The minute leading up to the engagement is the evidence. When engage and
            // shutdown land in the same tick, the shutdown dump carries the same data.
            if shutdown.is_none() {
                flight.dump(&flight_event("engaged", &managed));
            }
        }
    }
    if let Some(reason) = shutdown {
        return TickOutcome::Shutdown(reason);
    }

    // After the pin decision, never before it.
    sag.tick(Some(sensor), crate::now(), response.engaged, &s);
    // Normally a plain sleep; with the dashboard's fast polling the same 0.5 s is spent
    // reading the accelerometer, still in this thread.
    sag.idle(Some(sensor), POLL_INTERVAL_SEC, response.engaged, &s);
    TickOutcome::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Service started at t=100 with a 30 s delay.
    fn timer() -> OcTimer {
        OcTimer::new(100.0, 30.0)
    }

    #[test]
    fn a_resume_applies_again_after_the_delay() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        t.resumed(1000.0, 30.0);
        assert_eq!(oc_step(&mut t, 1000.0, false, false), OcAction::None, "waits for the driver");
        assert_eq!(oc_step(&mut t, 1029.9, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 1030.0, false, false), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 1030.5, false, false), OcAction::None);
    }

    #[test]
    fn a_resume_while_limited_applies_after_the_release() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        t.resumed(1000.0, 30.0);
        assert_eq!(oc_step(&mut t, 1040.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 1050.0, false, true), OcAction::Apply, "the full apply, not AfterRelease");
    }

    #[test]
    fn waits_for_the_delay_then_applies_once() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 100.0, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 129.9, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 130.5, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 5000.0, false, false), OcAction::None);
    }

    #[test]
    fn a_delay_ending_while_limited_applies_after_release() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 125.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 140.0, true, false), OcAction::None, "pending while limited");
        // The release tick runs the start-up apply (it is the full apply), once.
        assert_eq!(oc_step(&mut t, 150.0, false, true), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 150.5, false, false), OcAction::None);
    }

    #[test]
    fn every_release_after_the_first_apply_checks_again() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 200.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 210.0, false, true), OcAction::AfterRelease);
        assert_eq!(oc_step(&mut t, 210.5, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 300.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 310.0, false, true), OcAction::AfterRelease);
    }

    #[test]
    fn a_release_before_the_delay_does_not_apply_early() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 105.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 110.0, false, true), OcAction::None);
        assert_eq!(oc_step(&mut t, 129.0, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 131.0, false, false), OcAction::None, "no stale after-release");
    }

    #[test]
    fn an_action_that_could_not_run_is_tried_again() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        t.retry(OcAction::Apply);
        assert_eq!(oc_step(&mut t, 130.5, false, false), OcAction::Apply);
        assert_eq!(oc_step(&mut t, 200.0, false, true), OcAction::AfterRelease);
        t.retry(OcAction::AfterRelease);
        assert_eq!(oc_step(&mut t, 200.5, false, false), OcAction::AfterRelease);
        // Limited again before the retry got through: the next release checks instead.
        t.retry(OcAction::AfterRelease);
        assert_eq!(oc_step(&mut t, 201.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 201.5, false, false), OcAction::None);
    }

    /// Already due: started long ago with no delay.
    fn due() -> OcLoop {
        OcLoop::new(0.0, 0.0)
    }

    #[test]
    fn the_loop_applies_once_due_rechecks_after_release_and_never_waits() {
        use crate::protocol::Settings;
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(
            test_marker("loop"),
            Settings { oc_power_enabled: true, oc_power_w: 520.0, ..Settings::default() },
        );
        let mut oc = due();

        overclock_step(&f.shared, &mut oc, true);
        assert!(f.gpu.calls().is_empty(), "nothing while limited: {:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);

        // An Apply now holds the overclocker: the loop moves on and tries next time.
        {
            let _held = super::super::lock(&f.shared.overclock);
            overclock_step(&f.shared, &mut oc, false);
        }
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        overclock_step(&f.shared, &mut oc, false);
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(520)".to_string()]);

        // Something reset the limit while the guard held the GPU: the release puts it back.
        f.gpu.clear_calls();
        overclock_step(&f.shared, &mut oc, true);
        f.gpu.state().power_w = 600;
        overclock_step(&f.shared, &mut oc, false);
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(520)".to_string()]);
        f.gpu.clear_calls();
        overclock_step(&f.shared, &mut oc, false);
        assert!(f.gpu.calls().is_empty(), "once per release: {:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nothing_ticked_and_nothing_managed_never_opens_the_gpu() {
        use crate::protocol::Settings;
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(test_marker("untouched"), Settings::default());
        // The delay has ended (the timing itself is `oc_step`'s tests); then a trip and a release.
        let mut oc = due();
        overclock_step(&f.shared, &mut oc, false);
        overclock_step(&f.shared, &mut oc, true);
        overclock_step(&f.shared, &mut oc, false);
        overclock_step(&f.shared, &mut oc, false);
        assert!(oc.timer.applied, "the start-up apply came due");
        assert_eq!(f.opened.load(Ordering::SeqCst), 0, "no NVML or NVAPI at all");
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
    }

    #[test]
    fn an_untick_left_from_the_last_run_is_restored_at_start_up() {
        use crate::overclock::fake::FakeGpu;
        use crate::overclock::Overclocker;
        use crate::protocol::Settings;
        use crate::service::tests::{fake_service, test_marker};
        let marker = test_marker("untick-at-start");
        // The last run applied 520 W; it was unticked in config.ini before this start.
        let before = FakeGpu::owners();
        Overclocker::new(before.clone(), marker.clone(), None, None)
            .apply(&Settings { oc_power_enabled: true, ..Settings::default() }, false);
        let f = fake_service(marker, Settings::default());
        f.gpu.state().power_w = before.state().power_w;
        overclock_step(&f.shared, &mut due(), false);
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(600)".to_string()]);
    }

    #[test]
    fn a_start_up_apply_whose_gpu_would_not_open_is_retried_every_30_s() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        t.open_failed(130.0);
        assert_eq!(oc_step(&mut t, 130.5, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 159.9, false, false), OcAction::None, "not before 30 s");
        assert_eq!(oc_step(&mut t, 160.0, false, false), OcAction::Apply);
        // Opened this time: no more attempts, and releases check as usual.
        assert_eq!(oc_step(&mut t, 160.5, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 500.0, false, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 510.0, false, true), OcAction::AfterRelease);
    }

    #[test]
    fn a_retry_that_comes_due_while_limited_waits_for_the_release() {
        let mut t = timer();
        assert_eq!(oc_step(&mut t, 130.0, false, false), OcAction::Apply);
        t.open_failed(130.0);
        assert_eq!(oc_step(&mut t, 165.0, true, false), OcAction::None);
        assert_eq!(oc_step(&mut t, 170.0, false, true), OcAction::Apply);
    }

    #[test]
    fn the_loop_retries_a_failed_open_for_the_start_up_apply_only() {
        use crate::overclock::fake::OWNERS_SUBSYSTEM;
        use crate::protocol::Settings;
        use crate::service::tests::test_marker;
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        let tries = Arc::new(AtomicUsize::new(0));
        let t2 = Arc::clone(&tries);
        let shared = Shared::with_overclock_factory(
            Settings { oc_power_enabled: true, ..Settings::default() },
            test_marker("open-retry"),
            Box::new(move |_| {
                t2.fetch_add(1, O::SeqCst);
                Err("NVML not found".to_string())
            }),
        );
        shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        let mut oc = due();
        overclock_step(&shared, &mut oc, false);
        assert_eq!(tries.load(O::SeqCst), 1);
        overclock_step(&shared, &mut oc, false);
        assert_eq!(tries.load(O::SeqCst), 1, "not every iteration");
        assert!(!oc.timer.applied && oc.timer.due > crate::now() + 29.0, "next try in 30 s");
    }

    /// An overclocker whose every write panics (a bug in the binding, say).
    struct Panicking;

    impl crate::service::OcDriver for Panicking {
        fn apply(&mut self, _: &crate::protocol::Settings, _: bool) -> crate::protocol::OcReport {
            panic!("driver binding bug")
        }
        fn after_release(&mut self, _: &crate::protocol::Settings) -> crate::protocol::OcReport {
            panic!("driver binding bug")
        }
        fn status(&mut self, _: &crate::protocol::Settings) -> crate::protocol::OcStatus {
            crate::protocol::OcStatus::default()
        }
        fn verified_driver(&self) -> Option<String> {
            None
        }
        fn failed_driver(&self) -> Option<String> {
            None
        }
        fn subsystem(&self) -> u32 {
            crate::overclock::fake::OWNERS_SUBSYSTEM
        }
        fn managed(&self) -> Vec<String> {
            Vec::new()
        }
        fn set_loaded(&mut self, _: &crate::protocol::Settings, _: bool, _: bool) -> crate::protocol::OcReport {
            panic!("driver binding bug")
        }
        fn power_draw_w(&mut self) -> Result<u32, String> {
            Ok(0)
        }
        fn loaded(&self) -> bool {
            false
        }
        fn live(&mut self) -> crate::protocol::OcLive {
            crate::protocol::OcLive::default()
        }
    }

    fn panicking_service() -> Arc<Shared> {
        use crate::protocol::Settings;
        let shared = Shared::with_overclock_factory(
            Settings { oc_power_enabled: true, ..Settings::default() },
            crate::service::tests::test_marker("panics"),
            Box::new(|_| Ok(Box::new(Panicking))),
        );
        shared.update(|l| l.card_subsystem = Some(crate::overclock::fake::OWNERS_SUBSYSTEM));
        shared
    }

    #[test]
    fn a_panic_in_the_overclock_does_not_end_the_loop() {
        let shared = panicking_service();
        let mut oc = due();
        let panic = overclock_step(&shared, &mut oc, false);
        assert_eq!(panic.as_deref(), Some("driver binding bug"), "caught and reported");
        // The loop carries on, and so does the overclock bookkeeping.
        assert_eq!(overclock_step(&shared, &mut oc, false), None);
        overclock_step(&shared, &mut oc, true);
        assert_eq!(overclock_step(&shared, &mut oc, false).as_deref(), Some("driver binding bug"));
    }

    #[test]
    fn a_panic_during_apply_now_still_answers_the_request() {
        use crate::protocol::{Request, Response, SettingsPatch};
        let shared = panicking_service();
        let ipc = Arc::clone(&shared);
        let request = std::thread::spawn(move || {
            ipc.handle(Request::ApplyOverclock {
                changes: SettingsPatch { oc_power_w: Some(520.0), ..Default::default() },
            })
        });
        crate::service::tests::wait_for_mailbox(&shared);
        // Not due yet: only the Apply now runs (and panics).
        let mut oc = OcLoop::new(crate::now(), 3600.0);
        assert!(overclock_step(&shared, &mut oc, false).is_some());
        let Response::Overclock { report } = request.join().unwrap() else { panic!("not an overclock answer") };
        assert!(report.results.iter().all(|r| !r.ok) && !report.results.is_empty(), "{report:?}");
        assert!(report.results[0].message.contains("internal error"), "{report:?}");
    }

    #[test]
    fn the_trip_note_names_what_is_managed() {
        assert_eq!(trip_note(&[]), None);
        let managed = vec!["power".to_string(), "memory".to_string()];
        assert_eq!(trip_note(&managed).as_deref(), Some("overclock active: power, memory"));
        assert_eq!(flight_event("engaged", &managed), "engaged-overclock-power-memory");
        assert_eq!(flight_event("engaged", &[]), "engaged");
        assert_eq!(flight_event("shutdown", &managed), "shutdown-overclock-power-memory");
    }

    /// A limiter that is never needed: the ladder is not engaged in these tests.
    struct NoLimiter;

    impl super::super::response::Limiter for NoLimiter {
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable_reason(&self) -> String {
            String::new()
        }
        fn engage(&mut self, _reason: &str) -> bool {
            true
        }
        fn release(&mut self) -> bool {
            true
        }
        fn close(&mut self) {}
        fn abandon(&mut self) {}
    }

    #[test]
    fn pausing_protection_leaves_the_overclock_running() {
        use crate::overclock::fake::OWNERS_SUBSYSTEM;
        use crate::protocol::Settings;
        use crate::service::tests::test_marker;
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        let opens = Arc::new(AtomicUsize::new(0));
        let o2 = Arc::clone(&opens);
        let shared = Shared::with_overclock_factory(
            Settings { oc_power_enabled: true, ..Settings::default() },
            test_marker("paused"),
            Box::new(move |_| {
                o2.fetch_add(1, O::SeqCst);
                Err("NVML not found".to_string())
            }),
        );
        shared.update(|l| l.card_subsystem = Some(OWNERS_SUBSYSTEM));
        shared.paused.store(true, O::SeqCst);
        let mut response = TieredResponse::new(Arc::clone(&shared), Box::new(|_| Box::new(NoLimiter)));
        let (mut oc, mut pause) = (due(), Pause::default());
        let now = crate::now();
        paused_tick(&shared, &mut response, &mut oc, &mut pause, now, &shared.settings());
        assert_eq!(opens.load(O::SeqCst), 1, "the start-up apply is not held back by the pause");
        assert_eq!(shared.snapshot().status, Status::Paused);
        assert_eq!(pause.since, Some(now));
    }
}
