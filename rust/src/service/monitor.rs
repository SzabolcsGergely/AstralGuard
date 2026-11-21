//! The monitor loop: read the pins every 0.5 s, run the tier 1 timer, the imbalance check
//! and the response ladder, and act on what the ladder decides.

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::crosscheck::CrossCheck;
use super::flight::FlightRecorder;
use super::imbalance::ImbalanceMonitor;
use super::response::{LimiterFactory, Tick, TieredResponse, CLEAR_HYSTERESIS_AMPS};
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
    crate::sleep(sec);
}

/// What the loop needs from the machine it guards: `Real` in the service. The end-to-end
/// tests put a scripted card, a recording limiter and a recorded power-off here, with every
/// file in a folder of their own, and run the real loop on a virtual clock.
pub(crate) trait Machine {
    /// Before the loop: the Event Viewer source, and undoing a GPU limit a crashed run left.
    fn at_start(&mut self, shared: &Shared);
    /// Find the card and prove its chip answers.
    fn connect(&mut self) -> Result<PinSensor, String>;
    /// A card is there but its pins give nothing: worth a sensor scan.
    fn scan_possible(&mut self) -> bool;
    /// The OS and GPU driver versions, for the dashboard's header.
    fn system(&mut self) -> (String, String);
    /// The GPU slowdown, for the ladder.
    fn limiter(&mut self) -> LimiterFactory;
    /// Where the emergency-shutdown note for the next start lives.
    fn note_path(&self) -> std::path::PathBuf;
    fn flight(&self) -> FlightRecorder;
    /// The OS's power-off, by absolute path (safety rule 4).
    fn power_off(&mut self) -> Result<(), String>;
}

pub(crate) struct Real;

impl Machine for Real {
    fn at_start(&mut self, shared: &Shared) {
        if let Err(e) = platform::register_event_source() {
            logger::warn(format!("Could not register the Event Viewer source (needs admin once): {e}"));
        }
        startup_recovery(shared);
    }
    fn connect(&mut self) -> Result<PinSensor, String> {
        PinSensor::connect()
    }
    fn scan_possible(&mut self) -> bool {
        crate::sensor::scan_possible()
    }
    fn system(&mut self) -> (String, String) {
        (platform::os_version(), platform::gpu_driver())
    }
    fn limiter(&mut self) -> LimiterFactory {
        Box::new(|subsystem| {
            let mut m = GpuMitigator::default();
            m.connect(subsystem);
            Box::new(m)
        })
    }
    fn note_path(&self) -> std::path::PathBuf {
        config::data_dir().join("last_shutdown.json")
    }
    fn flight(&self) -> FlightRecorder {
        FlightRecorder::new()
    }
    fn power_off(&mut self) -> Result<(), String> {
        platform::shutdown_system()
    }
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

/// Leave a note for the next start: a shutdown while the user is away is otherwise a
/// mystery, Windows gets the blame, and the bad cable goes straight back under load.
fn write_shutdown_note(path: &std::path::Path, reason: &str) {
    let note = ShutdownNote {
        time: crate::now(),
        time_human: crate::clock::human(&crate::clock::now()),
        reason: reason.into(),
        version: APP_VERSION.into(),
    };
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(&note) {
        use std::io::Write;
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(&bytes);
            let _ = f.sync_all();
            let _ = fs::rename(&tmp, path);
        }
    }
}

/// One-shot at startup: if the guard ended the last session, say so loudly. A corrupt
/// note still announces, because it still means the guard shut this machine down.
fn announce_last_shutdown(shared: &Shared, note: &std::path::Path) {
    let Some(msg) = take_shutdown_note(note) else { return };
    logger::warn(&msg);
    shared.event(EventKind::Warning, 5091, &msg);
    shared.notify(&format!("{APP_NAME} - read this"), &msg);
}

/// The message for a note left by the last session, and the note removed so it is told
/// once; None when the last session did not shut the machine down.
fn take_shutdown_note(path: &std::path::Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let note: ShutdownNote = fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let when = if note.time_human.is_empty() { "an earlier session".to_string() } else { note.time_human };
    let reason =
        if note.reason.is_empty() { "sustained overcurrent on a 12VHPWR pin".to_string() } else { note.reason };
    let msg = format!(
        "The last shutdown was an emergency stop by {APP_NAME} ({when}): {reason} Check the connector and \
         cable seating before the next heavy load."
    );
    let _ = fs::remove_file(path.with_extension("json.tmp"));
    let _ = fs::remove_file(path);
    Some(msg)
}

fn shutdown_system(shared: &Shared, reason: &str, machine: &mut dyn Machine) {
    logger::critical(format!("SHUTDOWN TRIGGERED: {reason}"));
    write_shutdown_note(&machine.note_path(), reason);
    shared.notify(&format!("{APP_NAME} - Shutdown"), reason);
    shared.event(EventKind::Error, 5090, reason);
    if let Err(e) = machine.power_off() {
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
    run_on(shared, &mut Real)
}

/// `run` on a given machine: the real one, or the end-to-end tests' bench.
pub(crate) fn run_on(shared: &Arc<Shared>, machine: &mut dyn Machine) -> bool {
    machine.at_start(shared);
    announce_last_shutdown(shared, &machine.note_path());

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

    let mut response = TieredResponse::new(Arc::clone(shared), machine.limiter());
    let mut sensor: Option<PinSensor> = None;
    let mut crosscheck = CrossCheck::new();
    let mut flight = machine.flight();
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
                Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => Some(machine.connect()),
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
                    let (os, driver) = machine.system();
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
                    let scan = machine.scan_possible();
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
                sensor_lost(shared, &e, t1.over_start.is_some(), response.engaged);
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
                shutdown_system(shared, &reason, machine);
                break;
            }
            Err(panic) => {
                loop_error(shared, &panic_text(panic.as_ref()));
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
/// The pins stopped reading. The limit is not touched: losing the ability to measure is
/// not evidence the current stopped being dangerous. It releases normally once readings
/// come back and stay clean.
fn sensor_lost(shared: &Shared, e: &str, timing_over: bool, limited: bool) {
    let msg = format!("Sensor source lost: {e}");
    logger::warn(&msg);
    shared.event(EventKind::Warning, 1004, &msg);
    shared.notify(APP_NAME, "Sensor source lost. Waiting to reconnect...");
    if timing_over {
        logger::warn("Sustained over-threshold timer reset by sensor disconnect.");
    }
    if limited {
        logger::warn("GPU stays limited while the sensor is gone; it will be released once clean readings return.");
    }
    shared.update(|l| {
        l.status = Status::Error;
        l.last_message = "Sensor source lost".into();
        l.card.clear();
    });
}

/// A panic's message, whichever way it was raised.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "unknown panic".into())
}

/// A tick panicked: the loop carries on, and the dashboard says why it shows no status.
fn loop_error(shared: &Shared, what: &str) {
    logger::error(format!("Loop error: {what}"));
    shared.event(EventKind::Error, 9001, &format!("Loop error: {what}"));
    shared.update(|l| {
        l.status = Status::Error;
        l.last_message = format!("Error: {what}");
    });
}

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
    let outcome = decide(shared, sensor, crate::now(), response, t1, last_sample, crosscheck, flight, imbalance);
    if !matches!(outcome, TickOutcome::Continue) {
        return outcome;
    }
    let s = shared.settings();
    // After the pin decision, never before it.
    sag.tick(Some(sensor), crate::now(), response.engaged, &s);
    // Normally a plain sleep; with the dashboard's fast polling the same 0.5 s is spent
    // reading the accelerometer, still in this thread.
    sag.idle(Some(sensor), POLL_INTERVAL_SEC, response.engaged, &s);
    TickOutcome::Continue
}

/// One sample and what the guard does about it, at `now`: read the pins, the tier 1 timer,
/// the resume gap, imbalance, the ladder. Everything but the sag step and the wait, so the
/// tests can run it on a fake card and a clock of their own.
#[allow(clippy::too_many_arguments)]
fn decide(
    shared: &Arc<Shared>,
    sensor: &mut PinSensor,
    now: f64,
    response: &mut TieredResponse,
    t1: &mut Tier1,
    last_sample: &mut Option<f64>,
    crosscheck: &mut CrossCheck,
    flight: &mut FlightRecorder,
    imbalance: &mut ImbalanceMonitor,
) -> TickOutcome {
    let pins = match sensor.read_pins() {
        Ok(p) => p,
        Err(ReadError::Transient(e)) => return TickOutcome::Transient(e),
        Err(ReadError::Gone(e)) => return TickOutcome::Gone(e),
    };
    let s = shared.settings();

    // A long gap means the machine slept or stalled badly, not that current was high the
    // whole time; readings right after a resume are not trustworthy either.
    let sample_now = now;
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
    imbalance.evaluate(&pins, now, &s);

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

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ag-monitor-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_emergency_shutdown_is_told_once_at_the_next_start() {
        let dir = temp_dir("note");
        let path = dir.join("last_shutdown.json");
        assert_eq!(take_shutdown_note(&path), None, "no note, nothing to tell");

        write_shutdown_note(&path, "Tier 3 (catastrophic): Pin2=17.10A.");
        assert!(path.exists() && !path.with_extension("json.tmp").exists(), "written whole, then renamed");
        let msg = take_shutdown_note(&path).expect("told");
        assert!(msg.contains("emergency stop") && msg.contains("Pin2=17.10A"), "{msg}");
        assert!(!msg.contains("an earlier session"), "the time is in it: {msg}");
        assert_eq!(take_shutdown_note(&path), None, "told once");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_shutdown_note_is_still_told() {
        let dir = temp_dir("corrupt");
        let path = dir.join("last_shutdown.json");
        fs::write(&path, b"{not json").unwrap();
        fs::write(path.with_extension("json.tmp"), b"half").unwrap();
        let msg = take_shutdown_note(&path).expect("still told");
        assert!(msg.contains("an earlier session") && msg.contains("sustained overcurrent"), "{msg}");
        assert!(!path.exists() && !path.with_extension("json.tmp").exists(), "both cleaned up");
        let _ = fs::remove_dir_all(dir);
    }

    /// A ladder over `NoLimiter`, limited by a tier 1 trip.
    fn limited_ladder(shared: &Arc<Shared>) -> TieredResponse {
        let mut r = TieredResponse::new(Arc::clone(shared), Box::new(|_| Box::new(NoLimiter)));
        let t = Tick { now: 0.0, max_val: 10.0, max_label: "Pin1", tier1_expired: true, tier1_reason: "sustained" };
        assert!(r.evaluate(&t, &shared.settings()).is_none());
        assert!(r.engaged);
        r
    }

    #[test]
    fn pausing_gives_a_limited_gpu_back_and_reminds_every_half_hour() {
        use crate::protocol::Settings;
        let shared = Shared::new(Settings::default(), None, true);
        let mut response = limited_ladder(&shared);
        let (mut oc, mut pause) = (OcLoop::new(0.0, 3600.0), Pause::default());
        let s = shared.settings();

        paused_tick(&shared, &mut response, &mut oc, &mut pause, 100.0, &s);
        assert!(!response.engaged, "released when paused");
        assert_eq!(pause.since, Some(100.0));
        assert_eq!(shared.snapshot().status, Status::Paused);
        assert!(!shared.snapshot().engaged);

        // Not a fault: a trip right after resuming limits again rather than shutting down.
        let t = Tick { now: 110.0, max_val: 10.0, max_label: "Pin1", tier1_expired: true, tier1_reason: "x" };
        assert!(response.evaluate(&t, &s).is_none());
        assert!(response.engaged);

        // The reminder: at the half hour, and then not again for another half hour.
        let mut pause = Pause::default();
        paused_tick(&shared, &mut response, &mut oc, &mut pause, 0.0, &s);
        paused_tick(&shared, &mut response, &mut oc, &mut pause, PAUSE_REMINDER_EVERY_SEC - 1.0, &s);
        assert_eq!(pause.last_reminder, 0.0);
        paused_tick(&shared, &mut response, &mut oc, &mut pause, PAUSE_REMINDER_EVERY_SEC, &s);
        assert_eq!(pause.last_reminder, PAUSE_REMINDER_EVERY_SEC);
        paused_tick(&shared, &mut response, &mut oc, &mut pause, PAUSE_REMINDER_EVERY_SEC + 60.0, &s);
        assert_eq!(pause.last_reminder, PAUSE_REMINDER_EVERY_SEC, "once per half hour");
        assert_eq!(pause.since, Some(0.0), "the pause started at the first tick");
    }

    fn xbar_settings() -> crate::protocol::Settings {
        crate::protocol::Settings {
            oc_xbar_enabled: true,
            oc_xbar_offset_mhz: 150.0,
            oc_clk_load_only: true,
            oc_clk_load_watts: 250.0,
            oc_clk_load_seconds: 10.0,
            oc_clk_idle_seconds: 30.0,
            ..Default::default()
        }
    }

    /// Feed the load step once a second from `from` to `to`.
    fn feed(f: &crate::service::tests::FakeService, oc: &mut OcLoop, engaged: bool, from: u32, to: u32) {
        for t in from..=to {
            load_step(&f.shared, oc, engaged, 10_000.0 + t as f64);
        }
    }

    #[test]
    fn the_loop_applies_xbar_only_under_sustained_load_and_puts_it_back_at_idle() {
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(test_marker("xbar-load"), xbar_settings());
        let mut oc = due();
        overclock_step(&f.shared, &mut oc, false);
        assert!(oc.timer.applied);
        f.gpu.clear_calls();
        let xbar = |f: &crate::service::tests::FakeService| f.gpu.state().clocks[0].0;

        // Draw 280 W: under load only after the full 10 s.
        f.gpu.state().power_w = 300;
        feed(&f, &mut oc, false, 0, 8);
        assert_eq!(xbar(&f), 0, "not yet: {:?}", f.gpu.calls());
        feed(&f, &mut oc, false, 9, 12);
        assert_eq!(xbar(&f), 150_000, "applied under load: {:?}", f.gpu.calls());

        // Draw 80 W: back to idle only after the full 30 s.
        f.gpu.state().power_w = 100;
        feed(&f, &mut oc, false, 13, 30);
        assert_eq!(xbar(&f), 150_000, "a short idle keeps it");
        feed(&f, &mut oc, false, 31, 50);
        assert_eq!(xbar(&f), 0, "put back at idle: {:?}", f.gpu.calls());
    }

    #[test]
    fn the_load_gate_waits_for_the_start_up_apply_and_holds_while_limited() {
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(test_marker("xbar-hold"), xbar_settings());
        f.gpu.state().power_w = 300;

        // Before the start-up apply the driver may not have settled: no reads, no writes.
        let mut oc = OcLoop::new(crate::now(), 3600.0);
        feed(&f, &mut oc, false, 0, 20);
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
        assert_eq!(f.opened.load(Ordering::SeqCst), 0);

        // Applied, but the guard limits the GPU: the gate holds.
        let mut oc = due();
        overclock_step(&f.shared, &mut oc, false);
        f.gpu.clear_calls();
        feed(&f, &mut oc, true, 0, 20);
        assert!(f.gpu.calls().is_empty(), "{:?}", f.gpu.calls());
    }

    #[test]
    fn unticking_only_under_load_resets_the_gate() {
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(test_marker("xbar-untick"), xbar_settings());
        let mut oc = due();
        overclock_step(&f.shared, &mut oc, false);
        f.gpu.state().power_w = 300;
        feed(&f, &mut oc, false, 0, 12);
        assert!(oc.gate.loaded());
        *super::super::lock(&f.shared.settings) =
            crate::protocol::Settings { oc_clk_load_only: false, ..xbar_settings() };
        feed(&f, &mut oc, false, 13, 13);
        assert!(!oc.gate.loaded(), "the gate starts from idle if it is ticked again");
    }

    #[test]
    fn a_long_gap_between_steps_is_a_resume_and_applies_again_after_the_delay() {
        use crate::service::tests::{fake_service, test_marker};
        let f = fake_service(
            test_marker("resume"),
            crate::protocol::Settings {
                oc_power_enabled: true,
                oc_power_w: 520.0,
                oc_apply_delay_seconds: 30.0,
                ..Default::default()
            },
        );
        let mut oc = due();
        overclock_step(&f.shared, &mut oc, false);
        assert!(oc.timer.applied);

        // The machine slept: the last step is long ago.
        oc.last_step = Some(crate::now() - RESUME_GRACE_SEC - 60.0);
        f.gpu.state().power_w = 600; // the driver put the limit back on wake-up
        f.gpu.clear_calls();
        overclock_step(&f.shared, &mut oc, false);
        assert!(f.gpu.calls().is_empty(), "waits the apply delay first: {:?}", f.gpu.calls());
        assert!(!oc.timer.applied && oc.timer.due > crate::now() + 25.0);

        oc.timer.due = 0.0;
        overclock_step(&f.shared, &mut oc, false);
        assert_eq!(f.gpu.calls(), vec!["set_power_limit_w(520)".to_string()]);
    }

    #[test]
    fn a_lost_sensor_leaves_the_limit_alone_and_says_unknown() {
        use crate::protocol::Settings;
        let shared = Shared::new(Settings::default(), None, true);
        let mut response = limited_ladder(&shared);
        shared.update(|l| {
            l.status = Status::Mitigated;
            l.card = "ROG Astral RTX 5090 OC".into();
        });
        sensor_lost(&shared, "I2C read failed", true, response.engaged);
        assert!(response.engaged, "still limited: no reading is not a safe reading");
        let live = shared.snapshot();
        assert_eq!((live.status, live.status.text()), (Status::Error, "Unknown"));
        assert_eq!(live.last_message, "Sensor source lost");
        assert!(live.card.is_empty(), "the dashboard shows no card until it reconnects");
        // Clean readings after the reconnect release it the normal way.
        let s = shared.settings();
        let t = Tick { now: 10.0, max_val: 1.0, max_label: "Pin1", tier1_expired: false, tier1_reason: "" };
        response.evaluate(&t, &s);
        let t = Tick { now: 200.0, ..t };
        response.evaluate(&t, &s);
        assert!(!response.engaged);
    }

    #[test]
    fn a_panicking_tick_is_reported_with_its_message() {
        use crate::protocol::Settings;
        let shared = Shared::new(Settings::default(), None, true);
        let caught = |f: fn()| panic_text(catch_unwind(f).unwrap_err().as_ref());
        assert_eq!(caught(|| panic!("static text")), "static text");
        assert_eq!(caught(|| panic!("formatted {}", 7)), "formatted 7");
        assert_eq!(caught(|| std::panic::panic_any(42u8)), "unknown panic");
        loop_error(&shared, "index out of bounds");
        let live = shared.snapshot();
        assert_eq!((live.status, live.last_message.as_str()), (Status::Error, "Error: index out of bounds"));
    }

    #[test]
    fn waiting_ends_early_for_a_stop_or_a_scan() {
        use crate::protocol::Settings;
        use std::time::Duration;
        let shared = Shared::new(Settings::default(), None, true);
        let t = std::time::Instant::now();
        wait_or_stop(&shared, 0.3);
        assert!(t.elapsed() >= Duration::from_millis(280), "the full wait otherwise");

        shared.reconnect_now.store(true, Ordering::Relaxed);
        let t = std::time::Instant::now();
        wait_or_stop(&shared, 30.0);
        assert!(t.elapsed() < Duration::from_secs(5), "a Scan's find reconnects at once");
        assert!(!shared.reconnect_now.load(Ordering::Relaxed), "and the request is used up");

        shared.stop.store(true, Ordering::Relaxed);
        let t = std::time::Instant::now();
        wait_or_stop(&shared, 30.0);
        assert!(t.elapsed() < Duration::from_secs(5), "stopping is not held up");
    }
}

/// The protection loop's per-sample decisions, on a fake card and a clock of their own: the
/// tier 1 timer, its reset, the resume gap, read failures and the hand-off to the ladder.
#[cfg(test)]
mod sample_tests {
    use super::*;
    use crate::protocol::{ImbalanceResponse, Settings};
    use crate::sensor::fake::{Card, Reading};
    use crate::service::response::Limiter;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Calls {
        engaged: u32,
        released: u32,
    }

    struct FakeLimiter(Arc<Mutex<Calls>>);

    impl Limiter for FakeLimiter {
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable_reason(&self) -> String {
            String::new()
        }
        fn engage(&mut self, _reason: &str) -> bool {
            self.0.lock().unwrap().engaged += 1;
            true
        }
        fn release(&mut self) -> bool {
            self.0.lock().unwrap().released += 1;
            true
        }
        fn close(&mut self) {}
        fn abandon(&mut self) {}
    }

    /// One guard on one fake card. `at(t)` takes a sample at t seconds.
    struct Guard {
        card: Card,
        sensor: PinSensor,
        shared: Arc<Shared>,
        response: TieredResponse,
        t1: Tier1,
        last_sample: Option<f64>,
        crosscheck: CrossCheck,
        flight: FlightRecorder,
        imbalance: ImbalanceMonitor,
        calls: Arc<Mutex<Calls>>,
        dir: std::path::PathBuf,
    }

    impl Guard {
        fn new(name: &str, settings: Settings) -> Guard {
            let dir = std::env::temp_dir().join(format!("ag-monitor-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let card = Card::new([5.0; 6]);
            let shared = Shared::new(settings, None, true);
            let calls = Arc::new(Mutex::new(Calls::default()));
            let c2 = Arc::clone(&calls);
            let response =
                TieredResponse::new(Arc::clone(&shared), Box::new(move |_| Box::new(FakeLimiter(Arc::clone(&c2)))));
            Guard {
                sensor: card.sensor(),
                card,
                imbalance: ImbalanceMonitor::new(Arc::clone(&shared)),
                shared,
                response,
                t1: Tier1::default(),
                last_sample: None,
                crosscheck: CrossCheck::new(),
                flight: FlightRecorder::in_dir(dir.clone()),
                calls,
                dir,
            }
        }

        fn at(&mut self, t: f64) -> TickOutcome {
            decide(
                &self.shared,
                &mut self.sensor,
                1_000_000.0 + t,
                &mut self.response,
                &mut self.t1,
                &mut self.last_sample,
                &mut self.crosscheck,
                &mut self.flight,
                &mut self.imbalance,
            )
        }

        /// Samples every 0.5 s over [from, to]; the outcome of the last one.
        fn run(&mut self, from: f64, to: f64) -> TickOutcome {
            let mut out = TickOutcome::Continue;
            let mut t = from;
            while t <= to + 1e-9 {
                out = self.at(t);
                if matches!(out, TickOutcome::Shutdown(_)) {
                    break;
                }
                t += POLL_INTERVAL_SEC;
            }
            out
        }

        fn engaged(&self) -> u32 {
            self.calls.lock().unwrap().engaged
        }
        fn released(&self) -> u32 {
            self.calls.lock().unwrap().released
        }
        fn dumps(&self) -> usize {
            fs::read_dir(&self.dir).map(|d| d.count()).unwrap_or(0)
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// One pin at `amps`, the rest at 5 A: over the threshold, but not an imbalance alarm
    /// on its own unless the test asks for one.
    fn one_pin(amps: f64) -> [f64; 6] {
        [5.0, 5.0, 5.0, amps, 5.0, 5.0]
    }

    #[test]
    fn safe_current_never_limits() {
        let mut g = Guard::new("safe", Settings::default());
        g.card.amps([8.0; 6]);
        assert!(matches!(g.run(0.0, 120.0), TickOutcome::Continue));
        assert_eq!(g.engaged(), 0);
        assert_eq!(g.shared.live_field(|l| l.status), Status::Ok);
        assert_eq!(g.shared.live_field(|l| l.max_val), 8.0);
    }

    #[test]
    fn tier1_limits_after_exactly_the_sustained_time() {
        let s = Settings::default(); // 9.5 A for 15 s
        let mut g = Guard::new("tier1", s.clone());
        g.card.amps(one_pin(9.6));
        g.run(0.0, s.sustained_seconds_required - 0.5);
        assert_eq!(g.engaged(), 0, "14.5 s over is not yet 15 s");
        assert_eq!(g.shared.live_field(|l| l.status), Status::Over);
        g.at(s.sustained_seconds_required);
        assert_eq!(g.engaged(), 1, "limited at 15 s");
        assert_eq!(g.shared.live_field(|l| (l.status, l.engaged, l.engaged_tier)), (Status::Mitigated, true, Some(1)));
        assert_eq!(g.dumps(), 1, "the minute before the limit went to the flight recorder");
    }

    #[test]
    fn a_dip_shorter_than_a_second_does_not_reset_the_timer() {
        let mut g = Guard::new("dip-short", Settings::default());
        g.card.amps(one_pin(9.6));
        g.run(0.0, 10.0);
        g.card.amps(one_pin(5.0));
        g.run(10.5, 11.0); // 0.5 s clear: under CLEAR_STABLE_SECONDS
        g.card.amps(one_pin(9.6));
        g.run(11.5, 15.0);
        assert_eq!(g.engaged(), 1, "the timer kept running from t=0");
    }

    #[test]
    fn a_second_of_clear_current_resets_the_timer() {
        let mut g = Guard::new("dip-long", Settings::default());
        g.card.amps(one_pin(9.6));
        g.run(0.0, 10.0);
        g.card.amps(one_pin(5.0));
        g.run(10.5, 12.0); // clear for well over a second
        g.card.amps(one_pin(9.6));
        g.run(12.5, 27.0);
        assert_eq!(g.engaged(), 0, "a new 15 s started at 12.5 s");
        g.at(27.5);
        assert_eq!(g.engaged(), 1, "and ran out at 27.5 s");
    }

    #[test]
    fn hovering_just_under_the_threshold_does_not_reset_the_timer() {
        // Between threshold - hysteresis and the threshold the pin is not "cleared": a load
        // that wobbles around 9.5 A must still trip.
        let mut g = Guard::new("hover", Settings::default());
        let mut t = 0.0;
        while t <= 15.0 {
            g.card.amps(one_pin(if (t * 2.0) as i64 % 2 == 0 { 9.6 } else { 9.5 - CLEAR_HYSTERESIS_AMPS / 2.0 }));
            g.at(t);
            t += POLL_INTERVAL_SEC;
        }
        assert_eq!(g.engaged(), 1);
    }

    #[test]
    fn a_resume_gap_restarts_the_timer() {
        let mut g = Guard::new("gap", Settings::default());
        g.card.amps(one_pin(9.6));
        g.run(0.0, 10.0);
        // The machine slept: the next sample is 20 s later. Not 30 s of overcurrent.
        g.run(30.0, 44.5);
        assert_eq!(g.engaged(), 0, "the timer restarted at the first sample after the gap");
        g.at(45.0);
        assert_eq!(g.engaged(), 1);
    }

    #[test]
    fn a_resume_gap_releases_a_limit() {
        let mut g = Guard::new("gap-release", Settings::default());
        g.card.amps(one_pin(9.6));
        g.run(0.0, 15.0);
        assert_eq!(g.engaged(), 1);
        g.card.amps(one_pin(5.0));
        g.at(60.0);
        assert_eq!(g.released(), 1, "readings across a sleep say nothing about the plug now");
        assert!(!g.shared.live_field(|l| l.engaged));
    }

    #[test]
    fn tier2_limits_sooner() {
        let s = Settings::default(); // 13 A for 3 s
        let mut g = Guard::new("tier2", s.clone());
        g.card.amps(one_pin(13.5));
        g.run(0.0, s.critical_seconds - 0.5);
        assert_eq!(g.engaged(), 0);
        g.run(s.critical_seconds, s.critical_seconds + 0.5);
        assert_eq!(g.engaged(), 1, "13 A for 3 s, long before tier 1's 15 s");
    }

    #[test]
    fn tier3_shuts_down_on_the_second_sample_in_a_row() {
        let mut g = Guard::new("tier3", Settings::default());
        g.card.amps(one_pin(16.5));
        assert!(matches!(g.at(0.0), TickOutcome::Continue), "one sample is not enough");
        assert!(matches!(g.at(0.5), TickOutcome::Shutdown(r) if r.contains("Tier 3")));
        assert_eq!(g.dumps(), 0, "the shutdown path writes its own dump, not decide()");
    }

    #[test]
    fn a_single_tier3_spike_does_not_shut_down() {
        let mut g = Guard::new("tier3-spike", Settings::default());
        for t in 0..20 {
            g.card.amps(one_pin(if t % 2 == 0 { 16.5 } else { 8.0 }));
            assert!(!matches!(g.at(t as f64 * 0.5), TickOutcome::Shutdown(_)), "alternating spikes, t={t}");
        }
    }

    #[test]
    fn a_failed_read_changes_nothing() {
        let mut g = Guard::new("transient", Settings::default());
        g.card.amps(one_pin(9.6));
        g.run(0.0, 10.0);
        g.card.set(Reading::Fails("NACK".into()));
        assert!(matches!(g.at(10.5), TickOutcome::Transient(_)));
        assert_eq!(g.shared.live_field(|l| l.max_val), 9.6, "the last good sample stays shown");
        g.card.amps(one_pin(9.6));
        g.run(11.0, 15.0);
        assert_eq!(g.engaged(), 1, "one lost sample neither resets nor stretches the timer");
    }

    #[test]
    fn a_sensor_that_stops_answering_is_gone_not_a_shutdown() {
        let mut g = Guard::new("gone", Settings::default());
        g.card.amps(one_pin(15.0)); // over tier 2, about to limit
        g.at(0.0);
        g.card.set(Reading::NoRail);
        let outcomes: Vec<_> = (1..=5).map(|i| g.at(i as f64 * 0.5)).collect();
        assert!(outcomes[..4].iter().all(|o| matches!(o, TickOutcome::Transient(_))));
        assert!(matches!(&outcomes[4], TickOutcome::Gone(_)), "five bad samples: the sensor is gone");
        assert!(!outcomes.iter().any(|o| matches!(o, TickOutcome::Shutdown(_))));
    }

    #[test]
    fn imbalance_acts_like_tier1_only_when_set_to_act() {
        let unbalanced = [8.0, 8.0, 8.0, 8.0, 8.0, 1.0]; // all under 9.5 A, 7 A apart
        for (mode, limits) in [(ImbalanceResponse::Warn, false), (ImbalanceResponse::Act, true)] {
            let s = Settings { imbalance_response: mode, ..Settings::default() };
            let mut g = Guard::new(&format!("imbalance-{limits}"), s.clone());
            g.card.amps(unbalanced);
            g.run(0.0, s.imbalance_sustained_seconds + 1.0);
            assert_eq!(g.engaged() > 0, limits, "{mode:?}");
        }
    }
}

/// End to end: the real monitor loop (`run_on`) on a bench machine, a scripted card and a
/// virtual clock. Every scenario starts the loop as the service does and plays minutes of a
/// session in milliseconds: the pins the chip reports, the ladder's decisions, what the GPU
/// limiter is asked, and whether the machine is powered off. The power-off is recorded, never
/// run; every file goes to a folder of the test's own, never the installed guard's.
#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::protocol::{Request, Response, Settings};
    use crate::sensor::fake::{Card, Reading};
    use crate::service::response::Limiter;
    use crate::service::tests::{fake_service, test_marker, FakeService};
    use std::cell::{Cell, RefCell};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    /// The virtual clock's start; scenario times are seconds after it.
    const T0: f64 = 1_800_000_000.0;
    /// A card at a normal gaming load, all pins well under the 9.5 A threshold.
    const LOAD: [f64; 6] = [8.0, 8.1, 7.9, 8.0, 8.2, 7.8];

    /// What the GPU limiter was asked, with the scenario time: "engage", "release", "close",
    /// "abandon".
    type Calls = Arc<Mutex<Vec<(f64, &'static str)>>>;

    struct Limits {
        calls: Calls,
    }

    impl Limits {
        fn log(&self, what: &'static str) {
            self.calls.lock().unwrap().push((crate::now() - T0, what));
        }
    }

    impl Limiter for Limits {
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable_reason(&self) -> String {
            String::new()
        }
        fn engage(&mut self, _reason: &str) -> bool {
            self.log("engage");
            true
        }
        fn release(&mut self) -> bool {
            self.log("release");
            true
        }
        fn close(&mut self) {
            self.log("close");
        }
        fn abandon(&mut self) {
            self.log("abandon");
        }
    }

    /// The machine the loop guards in these tests.
    struct Bench {
        card: Card,
        plugged: Arc<AtomicBool>,
        calls: Calls,
        power_offs: Arc<Mutex<Vec<f64>>>,
        dir: PathBuf,
    }

    impl Machine for Bench {
        fn at_start(&mut self, _shared: &Shared) {}
        fn connect(&mut self) -> Result<PinSensor, String> {
            if self.plugged.load(Ordering::SeqCst) {
                Ok(self.card.sensor())
            } else {
                Err("no card found".into())
            }
        }
        fn scan_possible(&mut self) -> bool {
            false
        }
        fn system(&mut self) -> (String, String) {
            ("Test OS".into(), "Test driver".into())
        }
        fn limiter(&mut self) -> LimiterFactory {
            let calls = Arc::clone(&self.calls);
            Box::new(move |_| Box::new(Limits { calls: Arc::clone(&calls) }))
        }
        fn note_path(&self) -> PathBuf {
            self.dir.join("last_shutdown.json")
        }
        fn flight(&self) -> FlightRecorder {
            FlightRecorder::in_dir(self.dir.join("logs"))
        }
        fn power_off(&mut self) -> Result<(), String> {
            self.power_offs.lock().unwrap().push(crate::now() - T0);
            Ok(())
        }
    }

    /// What a scenario sees and changes as its time passes.
    struct World {
        card: Card,
        plugged: Arc<AtomicBool>,
        shared: Arc<Shared>,
    }

    impl World {
        /// The GPU as the loop last published it.
        fn limited(&self) -> bool {
            self.shared.live_field(|l| l.engaged)
        }
        /// The load, with pin 4 at `amps`.
        fn pin4(&self, amps: f64) {
            let mut a = LOAD;
            a[3] = amps;
            self.card.amps(a);
        }
        fn pause(&self, on: bool) {
            self.shared.paused.store(on, Ordering::SeqCst);
        }
        /// The machine slept for `seconds` (the clock jumps; nothing ran meanwhile).
        fn sleep_machine(&self, seconds: f64) {
            crate::sleep(seconds);
        }
    }

    /// A finished scenario.
    struct Played {
        svc: FakeService,
        /// What `run_on` returned: it fired a shutdown.
        shut_down: bool,
        calls: Vec<(f64, &'static str)>,
        power_offs: Vec<f64>,
        dir: PathBuf,
        /// The scenario time the loop ended at.
        end: f64,
    }

    impl Played {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.iter().map(|c| c.1).collect()
        }
        fn first(&self, what: &str) -> f64 {
            self.calls.iter().find(|c| c.1 == what).unwrap_or_else(|| panic!("no {what}: {:?}", self.calls)).0
        }
        fn note(&self) -> Option<String> {
            fs::read_to_string(self.dir.join("last_shutdown.json")).ok()
        }
        fn notices(&self) -> Vec<String> {
            let Response::Events { notices } = self.svc.shared.handle(Request::Events { since: 0 }) else { panic!() };
            notices.into_iter().map(|n| n.message).collect()
        }
        fn flight_dumps(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.dir.join("logs"))
                .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    impl Drop for Played {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Run the loop from scenario time 0 to `until` (or until it shuts the machine down),
    /// calling `script` at every moment the loop waits. `before` prepares the data folder.
    fn play_with(
        name: &str,
        settings: Settings,
        until: f64,
        before: impl FnOnce(&Path),
        mut script: impl FnMut(f64, &World) + 'static,
    ) -> Played {
        let svc = fake_service(test_marker(&format!("e2e-{name}")), settings);
        let dir = std::env::temp_dir().join(format!("ag-e2e-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // logs/ exists in the service: the logger made it at start.
        fs::create_dir_all(dir.join("logs")).unwrap();
        before(&dir);

        let card = Card::new(LOAD);
        let plugged = Arc::new(AtomicBool::new(true));
        let calls: Calls = Arc::default();
        let power_offs = Arc::new(Mutex::new(Vec::new()));
        let mut bench = Bench {
            card: card.clone(),
            plugged: Arc::clone(&plugged),
            calls: Arc::clone(&calls),
            power_offs: Arc::clone(&power_offs),
            dir: dir.clone(),
        };
        let world = World { card, plugged, shared: Arc::clone(&svc.shared) };
        let end = Rc::new(Cell::new(0.0));
        let seen = Rc::clone(&end);
        let tick = move |t: f64| {
            let s = t - T0;
            seen.set(s);
            if s >= until {
                world.shared.stop.store(true, Ordering::SeqCst);
            } else {
                script(s, &world);
            }
        };
        let shared = Arc::clone(&svc.shared);
        let shut_down = crate::virtual_clock::run(T0, tick, || run_on(&shared, &mut bench));
        let calls = calls.lock().unwrap().clone();
        let power_offs = power_offs.lock().unwrap().clone();
        Played { svc, shut_down, calls, power_offs, dir, end: end.get() }
    }

    fn play(name: &str, settings: Settings, until: f64, script: impl FnMut(f64, &World) + 'static) -> Played {
        play_with(name, settings, until, |_| {}, script)
    }

    /// A limit that works: while limited the current falls to 6 A, otherwise pin 4 draws `amps`.
    fn drawing(w: &World, amps: f64) {
        w.pin4(if w.limited() { 6.0 } else { amps });
    }

    #[test]
    fn a_normal_session_never_acts() {
        let p = play("normal", Settings::default(), 600.0, |s, w| w.pin4(8.0 + (s / 20.0).sin()));
        assert!(!p.shut_down && p.power_offs.is_empty());
        assert_eq!(p.calls(), ["close"], "the GPU was never limited, and is put back at stop anyway");
        let live = p.svc.shared.snapshot();
        assert_eq!(live.status, Status::Ok);
        assert_eq!(live.card, "ROG Astral RTX 5090 OC");
        assert!(live.history.len() > 500, "the dashboard's chart filled: {}", live.history.len());
        assert!(p.note().is_none() && p.flight_dumps().is_empty());
        assert!((p.end - 600.0).abs() < 1.0, "played to the end: {}", p.end);
    }

    #[test]
    fn sustained_overcurrent_is_limited_and_released_once_clean() {
        // 11 A on pin 4 from 20 s while the game runs (until 40 s); the limit brings it down.
        let p = play("tier1", Settings::default(), 400.0, |s, w| {
            drawing(w, if (20.0..40.0).contains(&s) { 11.0 } else { 8.0 })
        });
        assert_eq!(p.calls(), ["engage", "release", "close"]);
        let engaged = p.first("engage");
        assert!((34.5..=36.0).contains(&engaged), "after the 15 s sustained: {engaged}");
        let released = p.first("release");
        assert!(released - engaged >= 120.0, "held at least the 2 minutes: {}", released - engaged);
        assert!(released - engaged < 125.0, "and released once clean: {}", released - engaged);
        assert!(p.power_offs.is_empty() && !p.shut_down);
        assert!(
            p.flight_dumps().iter().any(|n| n.ends_with("_engaged.csv")),
            "the last minute dumped: {:?}",
            p.flight_dumps()
        );
        assert!(!p.svc.shared.live_field(|l| l.engaged));
    }

    #[test]
    fn a_fault_that_comes_back_after_the_release_shuts_down() {
        let p = play("retrigger", Settings::default(), 900.0, |s, w| drawing(w, if s >= 20.0 { 11.0 } else { 8.0 }));
        assert!(p.shut_down);
        assert_eq!(p.calls(), ["engage", "release", "engage", "abandon"], "limited on the way down, and left so");
        assert_eq!(p.power_offs.len(), 1);
        let note = p.note().expect("the note for the next start");
        assert!(note.contains("re-triggered"), "{note}");
        assert!(p.end < 900.0, "the loop ended at the shutdown: {}", p.end);
    }

    #[test]
    fn a_limit_that_does_not_bring_the_current_down_shuts_down() {
        let p = play("grace", Settings::default(), 300.0, |s, w| w.pin4(if s >= 20.0 { 11.0 } else { 8.0 }));
        assert!(p.shut_down);
        assert_eq!(p.calls(), ["engage", "abandon"]);
        let after = p.power_offs[0] - p.first("engage");
        assert!((10.0..=12.0).contains(&after), "after the 10 s grace: {after}");
    }

    #[test]
    fn a_catastrophic_spike_shuts_down_within_a_second() {
        let p = play("tier3", Settings::default(), 300.0, |s, w| w.pin4(if s >= 30.0 { 17.0 } else { 8.0 }));
        assert!(p.shut_down);
        let took = p.power_offs[0] - 30.0;
        assert!(took <= 1.5, "two samples at 0.5 s: {took}");
        assert_eq!(p.calls(), ["engage", "abandon"], "limited on the way down");
        assert!(p.note().unwrap().contains("Tier 3"));
        assert!(p.flight_dumps().iter().any(|n| n.ends_with("_shutdown.csv")), "{:?}", p.flight_dumps());
        assert_eq!(p.svc.shared.live_field(|l| l.status), Status::Shutdown);
    }

    #[test]
    fn shutdown_only_never_touches_the_gpu() {
        let s = Settings { response_mode: crate::protocol::ResponseMode::ShutdownOnly, ..Settings::default() };
        let p = play("shutdown-only", s, 300.0, |s, w| w.pin4(if s >= 20.0 { 11.0 } else { 8.0 }));
        assert!(p.shut_down);
        assert!(p.calls.is_empty(), "never opened, never limited: {:?}", p.calls);
        let at = p.power_offs[0];
        assert!((34.5..=36.0).contains(&at), "after the 15 s sustained: {at}");
    }

    #[test]
    fn a_lost_sensor_keeps_the_gpu_limited_until_clean_readings_return() {
        let during = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&during);
        let p = play("lost", Settings::default(), 400.0, move |s, w| {
            if (60.0..120.0).contains(&s) {
                // The chip stops answering, then the card is gone from the bus.
                w.card.set(Reading::Fails("I2C read failed".into()));
                w.plugged.store(false, Ordering::SeqCst);
                if s > 90.0 {
                    seen.borrow_mut().push((w.limited(), w.shared.live_field(|l| (l.status, l.card.clone()))));
                }
            } else {
                w.plugged.store(true, Ordering::SeqCst);
                drawing(w, if (20.0..40.0).contains(&s) { 11.0 } else { 8.0 });
            }
        });
        assert!(!p.shut_down);
        let during = during.borrow();
        assert!(!during.is_empty());
        assert!(during.iter().all(|(limited, (status, card))| *limited && *status == Status::Error && card.is_empty()));
        assert_eq!(p.calls(), ["engage", "release", "close"]);
        assert!(p.first("release") > 120.0, "not released while blind: {}", p.first("release"));
    }

    #[test]
    fn pausing_gives_the_gpu_back_and_resuming_starts_watching_from_scratch() {
        let paused_status = Rc::new(Cell::new(None));
        let seen = Rc::clone(&paused_status);
        let p = play("pause", Settings::default(), 300.0, move |s, w| {
            w.pause((60.0..200.0).contains(&s));
            if (100.0..101.0).contains(&s) {
                seen.set(Some(w.shared.live_field(|l| l.status)));
            }
            drawing(w, if s >= 20.0 { 11.0 } else { 8.0 });
        });
        assert_eq!(paused_status.get(), Some(Status::Paused));
        assert!(!p.shut_down && p.power_offs.is_empty(), "nothing acts while paused, 11 A or not");
        assert_eq!(p.calls(), ["engage", "release", "engage", "close"]);
        let released = p.calls[1].0;
        assert!((60.0..61.0).contains(&released), "given back when paused: {released}");
        let again = p.calls[2].0;
        assert!((214.5..=216.0).contains(&again), "a fresh 15 s after resuming, and a limit, not a shutdown: {again}");
    }

    #[test]
    fn a_sleep_does_not_count_as_overcurrent() {
        let p = play("suspend", Settings::default(), 200.0, |s, w| {
            if (30.0..30.5).contains(&s) {
                w.sleep_machine(60.0); // 10 s into the sustained timer
            }
            drawing(w, if s >= 20.0 { 11.0 } else { 8.0 });
        });
        let engaged = p.first("engage");
        assert!(engaged >= 105.0, "the timer started over after the gap: {engaged}");
    }

    #[test]
    fn the_last_emergency_shutdown_is_told_at_the_next_start() {
        let p = play_with(
            "note",
            Settings::default(),
            10.0,
            |dir| write_shutdown_note(&dir.join("last_shutdown.json"), "Tier 3 (catastrophic): Pin2=17.10A."),
            |_, _| {},
        );
        assert!(
            p.notices().iter().any(|n| n.contains("emergency stop") && n.contains("Pin2=17.10A")),
            "{:?}",
            p.notices()
        );
        assert!(p.note().is_none(), "told once");
    }

    #[test]
    fn the_overclock_is_applied_after_the_start_delay_and_kept_while_paused() {
        let s =
            Settings { oc_power_enabled: true, oc_power_w: 520.0, oc_apply_delay_seconds: 30.0, ..Settings::default() };
        let applied_at = Rc::new(Cell::new(None));
        let seen = Rc::clone(&applied_at);
        let p = play("overclock", s, 120.0, move |s, w| {
            w.pause(s >= 10.0); // pausing protection keeps the overclock
            if seen.get().is_none() && w.shared.overclock_managed().contains(&"power".to_string()) {
                seen.set(Some(s));
            }
        });
        assert!(p.svc.gpu.calls().contains(&"set_power_limit_w(520)".to_string()), "{:?}", p.svc.gpu.calls());
        let at = applied_at.get().expect("applied");
        assert!((30.0..32.0).contains(&at), "after the 30 s delay: {at}");
    }
}
