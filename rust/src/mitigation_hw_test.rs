//! The opt-in test of mitigation.rs on the real card. In a file of its own so that its
//! lines, which run only by hand, do not count against mitigation.rs's coverage floor.

use super::*;

/// The real card, once, by hand: limits it for 2 s and checks it comes back exactly. Needs
/// root and ASTRALGUARD_HW_TEST=1 (CI's weekly --include-ignored run must never limit a
/// card); uses its own marker file, not the running guard's. Run with:
///
///   cargo test --no-run, then as root:
///   ASTRALGUARD_HW_TEST=1 <test binary> real_card_limits_and_restores --ignored --nocapture
#[test]
#[ignore = "limits the real GPU for 2 s; needs root and ASTRALGUARD_HW_TEST=1"]
fn real_card_limits_and_restores() {
    if std::env::var("ASTRALGUARD_HW_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set ASTRALGUARD_HW_TEST=1 to limit the real card");
        return;
    }
    assert!(crate::platform::is_elevated(), "run as root: setting NVML limits needs it");

    /// Released however the test ends, so a failed assertion cannot leave the card limited.
    struct Released(GpuMitigator);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.close();
        }
    }

    let marker = std::env::temp_dir().join(format!("ag-hw-test-{}.active", std::process::id()));
    clear_marker(&marker);
    let mut m = Released(GpuMitigator::new(Box::new(open_nvml), marker.clone()));
    assert!(m.0.connect(None), "no card: {:?}", m.0.unavailable_reason);
    let sub = m.0.subsystem_id.expect("the card's subsystem");

    // An independent view of the card, to read what the mitigator does to it.
    let nvml = Nvml::init().unwrap();
    let h = handle_for_subsystem(&nvml, sub).unwrap();
    let read = |what: &str| {
        let limit = nvml.power_limit_mw(h).unwrap();
        let clock = nvml.clock_mhz(h, NVML_CLOCK_GRAPHICS).unwrap_or(0);
        eprintln!("{what:>9}: power limit {} W, graphics clock {clock} MHz", limit / 1000);
        limit
    };

    let before = read("before");
    assert!(m.0.engage("hardware test"), "engage failed");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let during = read("limited");
    assert_eq!(during, m.0.power_limit_min_mw, "the card's minimum power limit");
    let mk = read_marker(&marker).expect("a marker while limited");
    assert_eq!(mk.saved_power_limit_mw, Some(before), "the limit from before, saved");

    assert!(m.0.release(), "release reported a problem");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let after = read("restored");
    assert_eq!(after, before, "exactly the limit from before");
    assert!(read_marker(&marker).is_none());
    eprintln!("ok: limited to {} W and restored to {} W", during / 1000, after / 1000);
}
