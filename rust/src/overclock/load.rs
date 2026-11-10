//! When the XBAR / SYS offsets are wanted under "only under load": after the GPU has drawn at
//! least the threshold for the load time, and until it has drawn less for the idle time. A
//! raised XBAR keeps the core from dropping to its idle clock, so outside a game or another
//! heavy workload the offsets cost power and buy nothing.
//!
//! Pure and driven by the caller's clock, so it is tested without a GPU. A missing reading (a
//! failed power read) neither starts nor ends a phase: the gate holds where it is.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadGate {
    loaded: bool,
    /// Since when the readings have been on the other side of the threshold.
    since: Option<f64>,
}

impl LoadGate {
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// One power reading at `now`. Returns the new state when it changed.
    pub fn feed(&mut self, now: f64, watts: f64, threshold_w: f64, load_s: f64, idle_s: f64) -> Option<bool> {
        let above = watts >= threshold_w;
        if above == self.loaded {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(now);
        let need = if self.loaded { idle_s } else { load_s };
        if now - since >= need {
            self.loaded = above;
            self.since = None;
            return Some(self.loaded);
        }
        None
    }

    /// Back to idle at once (the guard limited the GPU, the offsets were unticked, ...).
    pub fn reset(&mut self) {
        *self = LoadGate::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(g: &mut LoadGate, from: f64, to: f64, watts: f64) -> Vec<(f64, bool)> {
        let mut out = Vec::new();
        let mut t = from;
        while t <= to + 1e-9 {
            if let Some(s) = g.feed(t, watts, 250.0, 10.0, 30.0) {
                out.push((t, s));
            }
            t += 0.5;
        }
        out
    }

    #[test]
    fn load_needs_the_full_load_time_and_idle_the_full_idle_time() {
        let mut g = LoadGate::default();
        assert!(run(&mut g, 0.0, 9.5, 400.0).is_empty(), "9.5 s of load is not enough");
        assert_eq!(run(&mut g, 10.0, 12.0, 400.0), vec![(10.0, true)]);
        assert!(run(&mut g, 12.5, 42.0, 80.0).is_empty(), "29.5 s below is not idle yet");
        assert_eq!(run(&mut g, 42.5, 43.0, 80.0), vec![(42.5, false)]);
        assert!(!g.loaded());
    }

    #[test]
    fn a_short_dip_or_spike_restarts_the_timer() {
        let mut g = LoadGate::default();
        run(&mut g, 0.0, 8.0, 400.0);
        run(&mut g, 8.5, 8.5, 100.0); // a loading screen
        assert!(run(&mut g, 9.0, 17.5, 400.0).is_empty(), "the 10 s start again after the dip");
        assert_eq!(run(&mut g, 18.0, 19.0, 400.0), vec![(19.0, true)]);
        run(&mut g, 19.5, 40.0, 100.0);
        run(&mut g, 40.5, 40.5, 400.0); // a spike while idling down
        assert!(g.loaded(), "still loaded: the idle time starts again");
    }

    #[test]
    fn exactly_at_the_threshold_counts_as_load() {
        let mut g = LoadGate::default();
        assert_eq!(run(&mut g, 0.0, 10.0, 250.0), vec![(10.0, true)]);
    }

    #[test]
    fn reset_drops_to_idle() {
        let mut g = LoadGate::default();
        run(&mut g, 0.0, 10.0, 400.0);
        g.reset();
        assert!(!g.loaded());
    }
}
