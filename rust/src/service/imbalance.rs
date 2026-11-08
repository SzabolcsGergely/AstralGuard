//! The highest and lowest pin drifting too far apart: the early sign of a bad contact.
//!
//! Trips when a pin carries at least imbalance_min_amps and either the spread reaches
//! imbalance_delta_amps or (max - min) / max reaches imbalance_percent, held for
//! imbalance_sustained_seconds. Clears with hysteresis, or once the load drops below the
//! minimum current (at idle every spread is meaningless). Only ever warns by itself; in
//! "act" mode the monitor hands action_reason() to the ladder as a tier 1 crossing.

use std::sync::Arc;

use super::Shared;
use crate::logger;
use crate::platform::EventKind;
use crate::protocol::{ImbalanceResponse, Settings, APP_NAME};
use crate::sensor::{max_pin, min_pin, Pin};

/// Clears once both the percentage and the delta drop below this fraction.
const CLEAR_FRACTION: f64 = 0.8;
/// A cable that stays bad would otherwise notify on every load change.
const RENOTIFY_SEC: f64 = 600.0;

pub struct ImbalanceMonitor {
    shared: Arc<Shared>,
    pub active: bool,
    reason: String,
    since: Option<f64>,
    last_notified: f64,
    notified_this_episode: bool,
    worst: (f64, f64),
}

impl ImbalanceMonitor {
    pub fn new(shared: Arc<Shared>) -> Self {
        ImbalanceMonitor {
            shared,
            active: false,
            reason: String::new(),
            since: None,
            last_notified: f64::NEG_INFINITY,
            notified_this_episode: false,
            worst: (0.0, 0.0),
        }
    }

    pub fn evaluate(&mut self, pins: &[Pin], now: f64, s: &Settings) {
        if !s.imbalance_enabled || pins.len() < 2 {
            if self.active {
                logger::info("Pin imbalance warning dropped (imbalance check disabled).");
            }
            self.reset();
            self.shared.update(|l| {
                l.imbalance_active = false;
                l.imbalance_delta = None;
                l.imbalance_pct = None;
            });
            return;
        }
        let (Some(hi), Some(lo)) = (max_pin(pins), min_pin(pins)) else { return };
        let delta = hi.amps - lo.amps;
        let pct = if hi.amps > 0.0 { delta / hi.amps * 100.0 } else { 0.0 };
        let loaded = hi.amps >= s.imbalance_min_amps;

        if !self.active {
            let tripped = loaded && (pct >= s.imbalance_percent || delta >= s.imbalance_delta_amps);
            match (tripped, self.since) {
                (false, _) => self.since = None,
                (true, None) => self.since = Some(now),
                (true, Some(since)) if now - since >= s.imbalance_sustained_seconds => {
                    self.raise(pins, hi, lo, delta, pct, now - since, now, s);
                    self.since = None;
                }
                _ => {}
            }
        } else {
            if (delta, pct) > self.worst {
                self.worst = (delta, pct);
            }
            let cleared = !loaded
                || (pct < s.imbalance_percent * CLEAR_FRACTION && delta < s.imbalance_delta_amps * CLEAR_FRACTION);
            match (cleared, self.since) {
                (false, _) => self.since = None,
                (true, None) => self.since = Some(now),
                (true, Some(since)) if now - since >= s.imbalance_sustained_seconds => self.clear(loaded),
                _ => {}
            }
        }
        let active = self.active;
        self.shared.update(|l| {
            l.imbalance_active = active;
            l.imbalance_delta = Some(delta);
            l.imbalance_pct = Some(pct);
        });
    }

    /// Non-empty while tripped in act mode: the tier 1 reason for the ladder.
    pub fn action_reason(&self, s: &Settings) -> &str {
        if self.active && s.imbalance_response == ImbalanceResponse::Act {
            &self.reason
        } else {
            ""
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn raise(&mut self, pins: &[Pin], hi: &Pin, lo: &Pin, delta: f64, pct: f64, held: f64, now: f64, s: &Settings) {
        self.active = true;
        self.worst = (delta, pct);
        self.reason = format!(
            "pin imbalance {}={:.2}A vs {}={:.2}A ({delta:.2}A, {pct:.0}%) sustained {held:.1}s",
            hi.name, hi.amps, lo.name, lo.amps
        );
        let spread = pins.iter().map(|p| format!("{} {:.2}A", p.name, p.amps)).collect::<Vec<_>>().join(", ");
        let acting = s.imbalance_response == ImbalanceResponse::Act;
        let msg = format!(
            "Pin current imbalance: {} {:.2} A vs {} {:.2} A ({delta:.2} A, {pct:.0}%) for {held:.0} s. \
             An uneven split usually means a bad contact - check that the 16-pin plug is fully seated \
             at both ends. [{spread}]{}",
            hi.name,
            hi.amps,
            lo.name,
            lo.amps,
            if acting { " Imbalance response is 'act': handing it to the response ladder." } else { "" }
        );
        logger::warn(&msg);
        // The log gets every trip; the user one notification per RENOTIFY_SEC. In act mode
        // the ladder sends its own notification about what it did.
        self.notified_this_episode = now - self.last_notified >= RENOTIFY_SEC;
        if self.notified_this_episode {
            self.last_notified = now;
            self.shared.event(EventKind::Warning, 8001, &msg);
            if !acting {
                self.shared.notify(
                    &format!("{APP_NAME} - Pin imbalance"),
                    &format!(
                        "{} {:.1} A vs {} {:.1} A ({pct:.0}%). Check the 16-pin plug is fully seated.",
                        hi.name, hi.amps, lo.name, lo.amps
                    ),
                );
            }
        }
    }

    fn clear(&mut self, loaded: bool) {
        let (delta, pct) = self.worst;
        let why = if loaded { "max delta back to normal" } else { "load dropped below the minimum current" };
        let msg = format!("Pin imbalance cleared ({why}); worst seen {delta:.2} A / {pct:.0}%.");
        logger::info(&msg);
        if self.notified_this_episode {
            self.shared.event(EventKind::Info, 8002, &msg);
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.active = false;
        self.reason.clear();
        self.since = None;
        self.notified_this_episode = false;
        self.worst = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(amps: &[f64]) -> Vec<Pin> {
        amps.iter().enumerate().map(|(i, &a)| Pin { name: format!("Pin{}", i + 1), amps: a, volts: 12.0 }).collect()
    }

    #[test]
    fn trips_after_sustain_and_clears() {
        let shared = Shared::new(Settings::default(), None, true);
        let s = Settings { imbalance_response: ImbalanceResponse::Act, ..Default::default() };
        let mut m = ImbalanceMonitor::new(shared);
        let bad = pins(&[8.0, 8.0, 8.0, 8.0, 8.0, 3.0]);
        m.evaluate(&bad, 0.0, &s);
        m.evaluate(&bad, 4.9, &s);
        assert!(!m.active);
        m.evaluate(&bad, 5.0, &s);
        assert!(m.active);
        assert!(m.action_reason(&s).starts_with("pin imbalance Pin1=8.00A vs Pin6=3.00A"));
        let idle = pins(&[1.0, 1.0, 1.0, 1.0, 1.0, 0.2]);
        m.evaluate(&idle, 6.0, &s);
        m.evaluate(&idle, 11.0, &s);
        assert!(!m.active);
    }

    #[test]
    fn idle_spread_is_ignored() {
        let shared = Shared::new(Settings::default(), None, true);
        let s = Settings::default();
        let mut m = ImbalanceMonitor::new(shared);
        let idle = pins(&[1.0, 1.0, 1.0, 1.0, 1.0, 0.1]);
        for t in 0..20 {
            m.evaluate(&idle, t as f64, &s);
        }
        assert!(!m.active);
    }
}
