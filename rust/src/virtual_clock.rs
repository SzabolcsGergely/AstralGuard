//! Tests: a clock for this thread that moves only when the code under test sleeps, so the
//! monitor loop plays minutes of a scenario in milliseconds. Other threads keep real time.

use std::cell::RefCell;

struct Clock {
    t: f64,
    /// Sees every new time: the scenario changes the card, pauses, stops.
    script: Option<Box<dyn FnMut(f64)>>,
}

thread_local! {
    static CLOCK: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

/// Run `f` on a virtual clock starting at `t0`; `script` is called with each new time.
pub fn run<R>(t0: f64, script: impl FnMut(f64) + 'static, f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CLOCK.with(|c| *c.borrow_mut() = None);
        }
    }
    CLOCK.with(|c| *c.borrow_mut() = Some(Clock { t: t0, script: Some(Box::new(script)) }));
    let _reset = Reset;
    f()
}

pub(crate) fn now() -> Option<f64> {
    CLOCK.with(|c| c.borrow().as_ref().map(|c| c.t))
}

/// Move the clock instead of sleeping; false without a virtual clock on this thread.
pub(crate) fn advance(sec: f64) -> bool {
    let moved = CLOCK.with(|c| {
        c.borrow_mut().as_mut().map(|c| {
            c.t += sec.max(0.0);
            (c.t, c.script.take())
        })
    });
    let Some((t, mut script)) = moved else { return false };
    // Called with the clock free: the script may read it, and the code it calls may too.
    if let Some(s) = script.as_mut() {
        s(t);
    }
    CLOCK.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            c.script = script;
        }
    });
    true
}
