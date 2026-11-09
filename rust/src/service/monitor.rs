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

/// Timers that restart from scratch on a new connection or after a resume gap.
#[derive(Default)]
struct Tier1 {
    over_start: Option<f64>,
    consecutive_over: u32,
    clear_start: Option<f64>,
    last_over_notice: f64,
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
    let mut paused_since: Option<f64> = None;
    let mut last_pause_reminder = 0.0;
    let mut shutting_down = false;

    while !shared.stop.load(Ordering::Relaxed) {
        let now = crate::now();
        let s = shared.settings();

        if shared.paused.load(Ordering::Relaxed) {
            // Checked before connecting so a paused guard stays quiet about missing sensors.
            let since = *paused_since.get_or_insert_with(|| {
                last_pause_reminder = now;
                now
            });
            if response.engaged {
                // Pausing hands the decision back to the user; leaving their GPU crippled
                // while no longer watching it is not ours to do.
                response.release(now, "monitoring paused", false, &s);
            }
            if now - last_pause_reminder >= PAUSE_REMINDER_EVERY_SEC {
                let msg = format!(
                    "Monitoring has been paused for {} minutes - the card is unprotected until you resume.",
                    ((now - since) / 60.0) as i64
                );
                logger::warn(&msg);
                shared.notify(&format!("{APP_NAME} - still paused"), &msg);
                last_pause_reminder = now;
            }
            shared.update(|l| {
                l.status = Status::Paused;
                l.last_message = "Paused".into();
                l.engaged = response.engaged;
                l.engaged_tier = response.engaged_tier;
                l.engaged_at = response.engaged_at;
            });
            sleep(0.5);
            continue;
        }
        if let Some(since) = paused_since.take() {
            logger::info(format!("Monitoring resumed after {} min paused.", ((now - since) / 60.0) as i64));
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
                    shared.update(|l| {
                        l.card = model;
                        l.card_subsystem = Some(sub);
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
                flight.dump("shutdown");
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
        // The minute leading up to the engagement is the evidence. When engage and
        // shutdown land in the same tick, the shutdown dump carries the same data.
        if !was_engaged && shutdown.is_none() {
            flight.dump("engaged");
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
