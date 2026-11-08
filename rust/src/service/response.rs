//! Everything that happens once a pin crosses a tier threshold.
//!
//! Tier 1 detection (the sustained timer) lives in the monitor loop; this owns the
//! response: slow the GPU down, give that a grace window to work, hold it long enough not
//! to flap, release once the current has been clean for a while, and escalate to
//! shutdown whenever any of that is not enough.
//!
//! evaluate() never shuts the machine down itself. It returns a reason and the caller
//! acts on it, which is what lets the tests drive the whole ladder.

use std::sync::Arc;

use super::Shared;
use crate::logger;
use crate::platform::EventKind;
use crate::protocol::{ResponseMode, Settings, APP_NAME};

/// Two samples, not one: tier 3 skips every timer, so a single bad read must not reach it.
pub const CATASTROPHIC_SAMPLES_REQUIRED: u32 = 2;
/// How long the slowdown gets after a tier 2 trigger before the guard shuts down.
pub const CRITICAL_GRACE_SECONDS: f64 = 5.0;
pub const CLEAR_HYSTERESIS_AMPS: f64 = 0.2;

/// The GPU slowdown, as the ladder sees it. GpuMitigator in production, a fake in tests.
pub trait Limiter: Send {
    fn is_available(&self) -> bool;
    fn unavailable_reason(&self) -> String;
    fn engage(&mut self, reason: &str) -> bool;
    fn release(&mut self) -> bool;
    /// Release and disconnect.
    fn close(&mut self);
    /// Disconnect but leave the card limited (machine powering off).
    fn abandon(&mut self);
}

impl Limiter for crate::mitigation::GpuMitigator {
    fn is_available(&self) -> bool {
        crate::mitigation::GpuMitigator::is_available(self)
    }
    fn unavailable_reason(&self) -> String {
        self.unavailable_reason.clone().unwrap_or_else(|| "unknown".into())
    }
    fn engage(&mut self, reason: &str) -> bool {
        crate::mitigation::GpuMitigator::engage(self, reason)
    }
    fn release(&mut self) -> bool {
        crate::mitigation::GpuMitigator::release(self)
    }
    fn close(&mut self) {
        crate::mitigation::GpuMitigator::close(self)
    }
    fn abandon(&mut self) {
        crate::mitigation::GpuMitigator::abandon(self)
    }
}

pub type LimiterFactory = Box<dyn FnMut(Option<u32>) -> Box<dyn Limiter> + Send>;

pub struct TieredResponse {
    shared: Arc<Shared>,
    factory: LimiterFactory,
    limiter: Option<Box<dyn Limiter>>,
    connect_tried: bool,
    /// The sensor's card, so NVML binds to the same physical GPU.
    pub subsystem: Option<u32>,

    pub engaged: bool,
    pub engaged_at: Option<f64>,
    pub engaged_tier: Option<u8>,
    grace_deadline: Option<f64>,
    clear_since: Option<f64>,
    released_at: Option<f64>,
    critical_since: Option<f64>,
    catastrophic_samples: u32,
    unavailable_warned: bool,
}

/// One tick's inputs from the monitor loop.
pub struct Tick<'a> {
    pub now: f64,
    pub max_val: f64,
    pub max_label: &'a str,
    pub tier1_expired: bool,
    pub tier1_reason: &'a str,
}

impl TieredResponse {
    pub fn new(shared: Arc<Shared>, factory: LimiterFactory) -> Self {
        TieredResponse {
            shared,
            factory,
            limiter: None,
            connect_tried: false,
            subsystem: None,
            engaged: false,
            engaged_at: None,
            engaged_tier: None,
            grace_deadline: None,
            clear_since: None,
            released_at: None,
            critical_since: None,
            catastrophic_samples: 0,
            unavailable_warned: false,
        }
    }

    /// Bind NVML the first time tiered mode actually needs it. Shutdown-only never gets
    /// here: that mode is the rollback if anything about GPU limiting misbehaves, so it
    /// must not touch NVML at all.
    fn limiter(&mut self, mode: ResponseMode) -> Option<&mut Box<dyn Limiter>> {
        if mode == ResponseMode::Tiered && self.limiter.is_none() && !self.connect_tried {
            self.connect_tried = true;
            let l = (self.factory)(self.subsystem);
            let state = if l.is_available() { "ready".to_string() } else { l.unavailable_reason() };
            self.shared.update(|live| live.mitigation = state);
            self.limiter = Some(l);
        }
        self.limiter.as_mut()
    }

    fn can_mitigate(&mut self, mode: ResponseMode) -> bool {
        self.limiter(mode).is_some_and(|l| l.is_available())
    }

    fn unavailable_reason(&self) -> String {
        match &self.limiter {
            None => "mitigation module unavailable".into(),
            Some(l) => l.unavailable_reason(),
        }
    }

    /// What will actually happen, which is not always what the setting says.
    fn effective_mode(&mut self, setting: ResponseMode) -> ResponseMode {
        if setting != ResponseMode::Tiered {
            return setting;
        }
        if self.can_mitigate(setting) {
            return ResponseMode::Tiered;
        }
        if !self.unavailable_warned {
            self.unavailable_warned = true;
            let reason = self.unavailable_reason();
            let msg = format!(
                "Tiered response is selected but the GPU cannot be limited ({reason}). \
                 Running shutdown-only until that changes."
            );
            logger::warn(&msg);
            self.shared.event(EventKind::Warning, 6004, &msg);
            self.shared.notify(APP_NAME, &format!("GPU limiting unavailable ({reason}). Shutdown-only."));
        }
        ResponseMode::ShutdownOnly
    }

    /// One tick. Returns a shutdown reason, or None to keep running.
    pub fn evaluate(&mut self, t: &Tick, s: &Settings) -> Option<String> {
        let mode = self.effective_mode(s.response_mode);

        // Switching to shutdown-only while limited gives the GPU straight back: the user
        // has taken the decision away from us, as with pausing.
        if self.engaged && mode == ResponseMode::ShutdownOnly {
            self.release(t.now, "response mode switched to shutdown-only", false, s);
        }

        // --- tier 3: catastrophic, no timers ---
        if t.max_val >= s.catastrophic_amps {
            self.catastrophic_samples += 1;
        } else {
            self.catastrophic_samples = 0;
        }
        if self.catastrophic_samples >= CATASTROPHIC_SAMPLES_REQUIRED {
            let detail = format!(
                "{}={:.2}A >= {:.2}A on {} consecutive samples",
                t.max_label, t.max_val, s.catastrophic_amps, CATASTROPHIC_SAMPLES_REQUIRED
            );
            let mut note = "shutting down immediately";
            if mode == ResponseMode::Tiered && self.engage(t.now, 3, &detail, None) {
                note = "GPU limited and shutting down immediately";
            }
            return Some(format!("Tier 3 (catastrophic): {detail}. {note}."));
        }

        // --- tier 2: critical, short window ---
        if t.max_val >= s.critical_amps {
            match self.critical_since {
                None => {
                    self.critical_since = Some(t.now);
                    let msg = format!(
                        "Critical current: {}={:.2}A >= {:.2}A. Responding in {:.0}s if it holds.",
                        t.max_label, t.max_val, s.critical_amps, s.critical_seconds
                    );
                    logger::warn(&msg);
                    self.shared.event(EventKind::Warning, 6001, &msg);
                }
                Some(since) if t.now - since >= s.critical_seconds => {
                    let detail = format!(
                        "{}={:.2}A >= {:.2}A sustained {:.1}s",
                        t.max_label,
                        t.max_val,
                        s.critical_amps,
                        t.now - since
                    );
                    if let Some(reason) = self.trigger(t.now, 2, &detail, CRITICAL_GRACE_SECONDS, mode, s) {
                        return Some(reason);
                    }
                }
                Some(_) => {}
            }
        } else {
            self.critical_since = None;
        }

        // --- tier 1: the sustained window ---
        if t.tier1_expired {
            if let Some(reason) = self.trigger(t.now, 1, t.tier1_reason, s.mitigation_grace_seconds, mode, s) {
                return Some(reason);
            }
        }

        if self.engaged {
            return self.while_mitigated(t, s);
        }
        None
    }

    fn trigger(
        &mut self,
        now: f64,
        tier: u8,
        detail: &str,
        grace: f64,
        mode: ResponseMode,
        s: &Settings,
    ) -> Option<String> {
        // A fault that comes back this soon after a release is persistent. Riding it out
        // worked last time and it came back anyway, so stop riding it out.
        if let Some(released) = self.released_at {
            let since = now - released;
            if since <= s.retrigger_window_seconds {
                let mut note = "shutting down";
                if mode == ResponseMode::Tiered && self.engage(now, tier, detail, None) {
                    note = "GPU limited and shutting down";
                }
                return Some(format!(
                    "Tier {tier} re-triggered {since:.0}s after the last release (within the {:.0}s window): \
                     {detail}. Persistent fault, {note}.",
                    s.retrigger_window_seconds
                ));
            }
        }
        if mode == ResponseMode::ShutdownOnly {
            let why = if s.response_mode == ResponseMode::ShutdownOnly {
                "response mode is shutdown-only".to_string()
            } else {
                format!("GPU limiting unavailable ({})", self.unavailable_reason())
            };
            return Some(format!("Tier {tier}: {detail}. No mitigation attempted, {why}."));
        }
        if !self.engage(now, tier, detail, Some(grace)) {
            return Some(format!(
                "Tier {tier}: {detail}. Tried to limit the GPU and failed ({}), shutting down instead.",
                self.unavailable_reason()
            ));
        }
        None
    }

    /// Apply the slowdown. Idempotent: an existing grace window keeps running.
    fn engage(&mut self, now: f64, tier: u8, detail: &str, grace: Option<f64>) -> bool {
        if self.engaged {
            return true;
        }
        let Some(l) = self.limiter.as_mut() else { return false };
        if !l.engage(&format!("tier {tier}: {detail}")) {
            return false;
        }
        self.engaged = true;
        self.engaged_at = Some(now);
        self.engaged_tier = Some(tier);
        self.grace_deadline = grace.map(|g| now + g);
        self.clear_since = None;

        let mut msg = format!("Tier {tier}: {detail}. GPU limited to its floor clock and minimum power limit");
        if let Some(g) = grace {
            msg += &format!("; shutting down if the current is still high in {g:.0}s");
        }
        logger::warn(&msg);
        self.shared.event(EventKind::Warning, 6002, &msg);
        let cause = if detail.starts_with("pin imbalance") { "pin imbalance" } else { "overcurrent" };
        self.shared.notify(
            &format!("{APP_NAME} - GPU limited"),
            &format!(
                "Tier {tier} {cause}. The GPU has been slowed down to protect the connector. \
                 Save your work and check the cable."
            ),
        );
        true
    }

    fn while_mitigated(&mut self, t: &Tick, s: &Settings) -> Option<String> {
        let threshold = s.threshold_amps;
        // Did the slowdown work? Current this high at floor clocks means the contact is
        // bad enough that no clock speed is safe.
        if let Some(deadline) = self.grace_deadline {
            if t.now >= deadline {
                if t.max_val >= threshold {
                    return Some(format!(
                        "Tier {}: GPU was limited but {}={:.2}A is still >= {:.2}A. Limiting the card did not fix it.",
                        self.engaged_tier.unwrap_or(0),
                        t.max_label,
                        t.max_val,
                        threshold
                    ));
                }
                self.grace_deadline = None;
                let msg = format!(
                    "GPU limit brought the current down to {:.2}A. Holding for at least {:.0}s.",
                    t.max_val, s.mitigation_min_hold_seconds
                );
                logger::info(&msg);
                self.shared.event(EventKind::Info, 6003, &msg);
            }
        }

        if t.max_val < threshold - CLEAR_HYSTERESIS_AMPS {
            self.clear_since.get_or_insert(t.now);
        } else {
            self.clear_since = None;
        }

        if self.grace_deadline.is_none() {
            if let (Some(clear), Some(at)) = (self.clear_since, self.engaged_at) {
                if t.now - clear >= s.mitigation_clear_seconds && t.now - at >= s.mitigation_min_hold_seconds {
                    let why = format!(
                        "current stayed below {:.2}A for {:.0}s",
                        threshold - CLEAR_HYSTERESIS_AMPS,
                        s.mitigation_clear_seconds
                    );
                    self.release(t.now, &why, true, s);
                }
            }
        }
        None
    }

    /// Give the GPU back. arm_retrigger starts the window in which a returning fault is
    /// treated as persistent; a release the user asked for (pause, mode switch) or one
    /// forced by a suspend is not evidence about the connector, so those do not arm it.
    pub fn release(&mut self, now: f64, why: &str, arm_retrigger: bool, s: &Settings) {
        if !self.engaged {
            return;
        }
        let restored = self.limiter.as_mut().map(|l| l.release()).unwrap_or(false);
        self.engaged = false;
        self.engaged_at = None;
        self.engaged_tier = None;
        self.grace_deadline = None;
        self.clear_since = None;
        self.critical_since = None;
        self.catastrophic_samples = 0;
        self.released_at = if arm_retrigger { Some(now) } else { None };

        let mut msg = format!("GPU limit released ({why}).");
        if arm_retrigger {
            msg += &format!(
                " Another overcurrent within {:.0}s will be treated as a persistent fault and shut the machine down.",
                s.retrigger_window_seconds
            );
        }
        if !restored {
            msg += " WARNING: the card may not be fully restored, check the log above.";
        }
        logger::warn(&msg);
        self.shared.event(EventKind::Info, 6005, &msg);
        self.shared.notify(APP_NAME, "GPU restored to normal clocks and power limit.");
    }

    /// Drop every in-flight timer. Used after a suspend/resume gap.
    pub fn reset_timers(&mut self) {
        self.critical_since = None;
        self.catastrophic_samples = 0;
        self.released_at = None;
    }

    /// End of the run. Shutting down while limited: leave the card limited through the
    /// power-off (the marker restores it on the next start). Otherwise put it back.
    pub fn finish(&mut self, shutting_down: bool) {
        let Some(l) = self.limiter.as_mut() else { return };
        if shutting_down && self.engaged {
            logger::info(
                "Leaving the GPU limited through shutdown; the marker file will trigger a restore on the next start.",
            );
            l.abandon();
        } else {
            l.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log {
        engaged: u32,
        released: u32,
    }

    struct FakeLimiter {
        available: bool,
        log: Arc<Mutex<Log>>,
    }

    impl Limiter for FakeLimiter {
        fn is_available(&self) -> bool {
            self.available
        }
        fn unavailable_reason(&self) -> String {
            if self.available {
                String::new()
            } else {
                "not elevated".into()
            }
        }
        fn engage(&mut self, _reason: &str) -> bool {
            self.log.lock().unwrap().engaged += 1;
            self.available
        }
        fn release(&mut self) -> bool {
            self.log.lock().unwrap().released += 1;
            true
        }
        fn close(&mut self) {}
        fn abandon(&mut self) {}
    }

    fn ladder(available: bool) -> (TieredResponse, Arc<Mutex<Log>>, Settings) {
        let shared = Shared::new(Settings::default(), None, true);
        let log = Arc::new(Mutex::new(Log::default()));
        let l2 = Arc::clone(&log);
        let r =
            TieredResponse::new(shared, Box::new(move |_| Box::new(FakeLimiter { available, log: Arc::clone(&l2) })));
        (r, log, Settings::default())
    }

    fn tick(now: f64, amps: f64, tier1: bool) -> Tick<'static> {
        Tick { now, max_val: amps, max_label: "Pin1", tier1_expired: tier1, tier1_reason: "sustained" }
    }

    #[test]
    fn tier1_limits_then_releases_after_hold() {
        let (mut r, log, s) = ladder(true);
        assert!(r.evaluate(&tick(0.0, 10.0, true), &s).is_none());
        assert!(r.engaged);
        assert_eq!(log.lock().unwrap().engaged, 1);
        // current falls within the grace window
        assert!(r.evaluate(&tick(10.0, 1.0, false), &s).is_none());
        // clean for 60 s but held for 120 s minimum
        assert!(r.evaluate(&tick(80.0, 1.0, false), &s).is_none());
        assert!(r.engaged);
        assert!(r.evaluate(&tick(121.0, 1.0, false), &s).is_none());
        assert!(!r.engaged);
        assert_eq!(log.lock().unwrap().released, 1);
    }

    #[test]
    fn grace_expiry_with_high_current_shuts_down() {
        let (mut r, _log, s) = ladder(true);
        r.evaluate(&tick(0.0, 10.0, true), &s);
        let reason = r.evaluate(&tick(10.5, 10.0, false), &s).expect("shutdown");
        assert!(reason.contains("did not fix it"), "{reason}");
    }

    #[test]
    fn retrigger_within_window_is_persistent_fault() {
        let (mut r, _log, s) = ladder(true);
        r.evaluate(&tick(0.0, 10.0, true), &s);
        r.evaluate(&tick(10.0, 1.0, false), &s);
        r.evaluate(&tick(130.0, 1.0, false), &s);
        assert!(!r.engaged);
        let reason = r.evaluate(&tick(200.0, 10.0, true), &s).expect("shutdown");
        assert!(reason.contains("re-triggered"), "{reason}");
    }

    #[test]
    fn tier3_needs_two_samples() {
        let (mut r, _log, s) = ladder(true);
        assert!(r.evaluate(&tick(0.0, 20.0, false), &s).is_none());
        let reason = r.evaluate(&tick(0.5, 20.0, false), &s).expect("shutdown");
        assert!(reason.starts_with("Tier 3"), "{reason}");
        assert!(r.engaged, "tier 3 limits the GPU on the way down");
    }

    #[test]
    fn tier2_after_its_window() {
        let (mut r, _log, s) = ladder(true);
        assert!(r.evaluate(&tick(0.0, 14.0, false), &s).is_none());
        assert!(!r.engaged);
        assert!(r.evaluate(&tick(3.0, 14.0, false), &s).is_none());
        assert_eq!(r.engaged_tier, Some(2));
        // tier 2's grace is 5 s
        let reason = r.evaluate(&tick(8.1, 14.0, false), &s).expect("shutdown");
        assert!(reason.contains("did not fix it"), "{reason}");
    }

    #[test]
    fn unavailable_limiter_means_shutdown() {
        let (mut r, _log, s) = ladder(false);
        let reason = r.evaluate(&tick(0.0, 10.0, true), &s).expect("shutdown");
        assert!(reason.contains("GPU limiting unavailable"), "{reason}");
    }

    #[test]
    fn shutdown_only_never_touches_the_limiter() {
        let (mut r, log, mut s) = ladder(true);
        s.response_mode = ResponseMode::ShutdownOnly;
        let reason = r.evaluate(&tick(0.0, 10.0, true), &s).expect("shutdown");
        assert!(reason.contains("response mode is shutdown-only"), "{reason}");
        assert!(r.limiter.is_none());
        assert_eq!(log.lock().unwrap().engaged, 0);
    }
}
