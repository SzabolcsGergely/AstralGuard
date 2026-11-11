//! The sensor scan (`astralguard probe --detect`, or Scan in the dashboard): find which
//! of the Astral's I2C adapters carries the IT8915, prove it, and save it for the service
//! (Linux only).
//!
//! The one place that talks to more than one adapter. The service runs it on the dashboard's
//! request, or by itself with `sensor_auto_scan` on (off by default; at most once each time
//! the pins stop reading), and only while no sensor is reading; otherwise it uses the single
//! saved adapter (see i2cdev.rs).
//!
//! Per adapter of the Astral's own PCI device (never another device's bus):
//! 1. a pure 1-byte read at 0x2B, no register pointer write. No ack: skip the adapter.
//! 2. only where something acknowledged: the normal pointer+read of 0x80 (pins) and 0x70
//!    (accelerometer), a few samples, judged by `judge`.
//!
//! An adapter is accepted only if every sample shows all six pins on the 12 V rail, the
//! accelerometer at about 1 g, and the pins' total power fitting NVML's board power. And
//! only if exactly one adapter passes: none or several, nothing is saved.

use std::time::Duration;

use super::i2cdev::{adapters, Adapter, I2cDev};
use super::{
    is_plausible_g, parse_gsensor_block, parse_pin_block, I2cBus, GSENSOR_DATA_SIZE, GSENSOR_REG, PIN_DATA_SIZE,
    PIN_REG,
};
use crate::config::{self, I2cChoice};
use crate::protocol::ScanReport;

const SAMPLES: usize = 6;
const SAMPLE_GAP: Duration = Duration::from_millis(500);
/// Every pin of a live connector sits on the 12 V rail (ATX: ±5 %, wider for a moment
/// under load). Six values in this window, in the right byte layout, are the fingerprint
/// another chip does not produce by chance.
pub const RAIL_MIN_V: f64 = 10.8;
pub const RAIL_MAX_V: f64 = 13.2;
/// The PCIe slot supplies up to 75 W that never passes the connector, so the pins carry
/// the board power minus at most that. Margins cover the two readings not being taken at
/// the same instant and NVML's averaging.
pub const SLOT_MAX_W: f64 = 75.0;
pub const POWER_MARGIN_W: f64 = 20.0;
pub const POWER_MARGIN_RATIO: f64 = 1.10;

/// One sample from a candidate adapter.
#[derive(Clone, Debug)]
pub struct Sample {
    /// (pin, volts, amps), as parse_pin_block returns them
    pub pins: Vec<(u8, f64, f64)>,
    pub g: [f64; 3],
    /// NVML board power at the same moment; None when NVML is not available
    pub board_w: Option<f64>,
}

/// Ok(summary) when the samples are the IT8915, Err(the first reason they are not).
pub fn judge(samples: &[Result<Sample, String>]) -> Result<String, String> {
    if samples.is_empty() {
        return Err("no samples".into());
    }
    let (mut vmin, mut vmax, mut pins_w_last, mut board_last, mut g_last) = (f64::MAX, f64::MIN, 0.0, None, 0.0);
    for (i, s) in samples.iter().enumerate() {
        let s = s.as_ref().map_err(|e| format!("sample {}: read failed ({e})", i + 1))?;
        if s.pins.len() != 6 {
            return Err(format!("sample {}: {} pins, expected 6", i + 1, s.pins.len()));
        }
        for &(pin, v, a) in &s.pins {
            if !(RAIL_MIN_V..=RAIL_MAX_V).contains(&v) {
                return Err(format!("sample {}: Pin{pin} at {v:.3} V is not on the 12 V rail", i + 1));
            }
            if !(0.0..=super::PLAUSIBLE_MAX_AMPS).contains(&a) {
                return Err(format!("sample {}: Pin{pin} at {a:.3} A is not a current", i + 1));
            }
            vmin = vmin.min(v);
            vmax = vmax.max(v);
        }
        if !is_plausible_g(s.g) {
            let m = (s.g[0] * s.g[0] + s.g[1] * s.g[1] + s.g[2] * s.g[2]).sqrt();
            return Err(format!("sample {}: accelerometer reads {m:.3} g, not about 1 g", i + 1));
        }
        let pins_w: f64 = s.pins.iter().map(|&(_, v, a)| v * a).sum();
        if let Some(board) = s.board_w {
            let low = board - SLOT_MAX_W - POWER_MARGIN_W;
            let high = board * POWER_MARGIN_RATIO + POWER_MARGIN_W;
            if !(low..=high).contains(&pins_w) {
                return Err(format!(
                    "sample {}: pins carry {pins_w:.0} W but the board draws {board:.0} W (expected {:.0}-{high:.0} W)",
                    i + 1,
                    low.max(0.0)
                ));
            }
        }
        pins_w_last = pins_w;
        board_last = s.board_w;
        g_last = (s.g[0] * s.g[0] + s.g[1] * s.g[1] + s.g[2] * s.g[2]).sqrt();
    }
    let power = match board_last {
        Some(b) => format!("pins {pins_w_last:.0} W of {b:.0} W board power"),
        None => format!("pins {pins_w_last:.0} W (board power not compared: NVML unavailable)"),
    };
    Ok(format!("rail {vmin:.2}-{vmax:.2} V, {g_last:.2} g, {power}"))
}

/// One adapter as the scan uses it: the SMBus receive byte, then (only where that was
/// acknowledged) register reads. Both read-only; there is no write here to call.
pub(super) trait Probe: I2cBus {
    fn answers(&mut self) -> bool;
}

impl Probe for I2cDev {
    fn answers(&mut self) -> bool {
        I2cDev::answers(self)
    }
}

/// NVML's board power, in W; None when it cannot be read this time.
pub(super) type BoardWatts = Box<dyn Fn() -> Option<f64>>;

/// What a scan needs from the system: the real adapters, NVML and the clock in the guard;
/// fakes in the tests.
pub(super) trait ScanEnv {
    fn adapters(&mut self) -> Result<Vec<Adapter>, String>;
    fn open(&mut self, num: u32) -> Result<Box<dyn Probe>, String>;
    fn board_power(&mut self, subsystem: u32) -> Result<BoardWatts, String>;
    fn pause(&mut self, d: Duration);
}

struct System;

impl ScanEnv for System {
    fn adapters(&mut self) -> Result<Vec<Adapter>, String> {
        adapters()
    }
    fn open(&mut self, num: u32) -> Result<Box<dyn Probe>, String> {
        Ok(Box::new(I2cDev::open(num)?))
    }
    fn board_power(&mut self, subsystem: u32) -> Result<BoardWatts, String> {
        let p = crate::mitigation::BoardPower::open(subsystem)?;
        Ok(Box::new(move || p.watts().ok()))
    }
    fn pause(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

fn sample(bus: &mut dyn Probe, power: Option<&BoardWatts>) -> Result<Sample, String> {
    let mut raw = [0u8; PIN_DATA_SIZE];
    bus.read(PIN_REG, &mut raw)?;
    let mut graw = [0u8; GSENSOR_DATA_SIZE];
    bus.read(GSENSOR_REG, &mut graw)?;
    let board_w = power.and_then(|p| p());
    Ok(Sample { pins: parse_pin_block(&raw), g: parse_gsensor_block(&graw), board_w })
}

enum Outcome {
    Silent,
    Unopenable(String),
    Rejected(String),
    Accepted(String),
}

fn try_adapter(env: &mut dyn ScanEnv, a: &Adapter, power: Option<&BoardWatts>) -> Outcome {
    let mut bus = match env.open(a.num) {
        Ok(b) => b,
        Err(e) => return Outcome::Unopenable(e),
    };
    if !bus.answers() {
        return Outcome::Silent;
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        if i > 0 {
            env.pause(SAMPLE_GAP);
        }
        samples.push(sample(bus.as_mut(), power));
    }
    match judge(&samples) {
        Ok(s) => Outcome::Accepted(s),
        Err(e) => Outcome::Rejected(e),
    }
}

/// The card part of an adapter name: "NVIDIA i2c adapter 3 at 1:00.0" -> "1:00.0".
fn card_of(name: &str) -> &str {
    name.rsplit_once(" at ").map(|(_, at)| at).unwrap_or("")
}

/// Scan the card's adapters; with `save`, store the one that passes. Every outcome is a
/// report, never a panic or an exit: the service runs this on the dashboard's request.
pub fn scan(save: bool) -> ScanReport {
    scan_with(&mut System, save, &config::config_path())
}

fn scan_with(env: &mut dyn ScanEnv, save: bool, path: &std::path::Path) -> ScanReport {
    let fail = |message: String, lines: Vec<String>| ScanReport { found: false, message, lines };
    let all = match env.adapters() {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => return fail("No supported ASUS ROG Astral GPU found on any I2C adapter.".into(), vec![]),
        Err(e) => return fail(e, vec![]),
    };
    let first = &all[0];
    if all.iter().any(|a| a.subsystem != first.subsystem || card_of(&a.name) != card_of(&first.name)) {
        return fail(
            "More than one Astral card: the scan handles one. Set ASTRALGUARD_I2C_BUS=<n> instead.".into(),
            vec![],
        );
    }
    let mut lines = vec![format!(
        "{} [{}] (subsystem 0x{:08X}), {} I2C adapters",
        first.gpu_name,
        first.model,
        first.subsystem,
        all.len()
    )];
    let power = match env.board_power(first.subsystem) {
        Ok(p) => Some(p),
        Err(e) => {
            lines.push(format!("note: NVML unavailable ({e}); board power not compared"));
            None
        }
    };
    let mut accepted = Vec::new();
    for a in &all {
        let verdict = match try_adapter(env, a, power.as_ref()) {
            Outcome::Silent => "nothing at 0x2B".to_string(),
            Outcome::Unopenable(e) => format!("skipped: {e}"),
            Outcome::Rejected(e) => format!("not the pin monitor: {e}"),
            Outcome::Accepted(s) => {
                accepted.push(a);
                format!("pin monitor: {s}")
            }
        };
        lines.push(format!("/dev/i2c-{} {}: {verdict}", a.num, a.name));
    }
    let chosen = match accepted.as_slice() {
        [one] => *one,
        [] => return fail("No adapter showed the pin monitor. Nothing saved.".into(), lines),
        many => {
            let list: Vec<String> = many.iter().map(|a| format!("/dev/i2c-{}", a.num)).collect();
            return fail(
                format!("{} adapters passed ({}): ambiguous, nothing saved.", many.len(), list.join(", ")),
                lines,
            );
        }
    };
    let at = format!("/dev/i2c-{} (\"{}\")", chosen.num, chosen.name);
    if !save {
        return ScanReport { found: true, message: format!("Pin monitor on {at}. Not saved (dry run)."), lines };
    }
    let choice = I2cChoice { adapter: chosen.name.clone(), subsystem: chosen.subsystem };
    match config::save_i2c_choice(path, &choice) {
        Ok(()) => {
            ScanReport { found: true, message: format!("Pin monitor on {at}, saved to {}.", path.display()), lines }
        }
        Err(e) => {
            fail(format!("Pin monitor on {at}, but saving to {} failed ({e}). Run as root.", path.display()), lines)
        }
    }
}

/// `astralguard probe --detect [--dry-run]`
pub fn run(save: bool) -> i32 {
    println!("Looking for the pin monitor at 0x2B: a 1-byte read per adapter, then pin + accelerometer reads where it answers.");
    let report = scan(save);
    for line in &report.lines {
        println!("  {line}");
    }
    println!("{}", report.message);
    if report.found && save {
        println!("Restart the guard to use it (systemctl restart astralguard), or press Scan in the dashboard.");
    }
    if report.found {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(volts: f64, amps: f64, g: [f64; 3], board_w: Option<f64>) -> Result<Sample, String> {
        Ok(Sample { pins: (1..=6u8).map(|p| (p, volts, amps)).collect(), g, board_w })
    }
    const LEVEL: [f64; 3] = [-0.0085, -0.008, 0.9985];

    #[test]
    fn a_healthy_idle_card_passes() {
        // 6 x 12.05 V x 0.75 A = 54 W of a 70 W board
        let samples = vec![s(12.05, 0.75, LEVEL, Some(70.0)); 6];
        assert!(judge(&samples).is_ok(), "{:?}", judge(&samples));
    }

    #[test]
    fn what_adapter_1_answered_is_rejected() {
        // Seen 2026-11-09: something at 0x2B on adapter 1, all zeros, card at 70 W.
        let samples = vec![s(0.0, 0.0, [0.0, 0.0, 0.0], Some(70.0)); 6];
        let e = judge(&samples).unwrap_err();
        assert!(e.contains("12 V rail"), "{e}");
    }

    #[test]
    fn a_rail_without_gravity_is_rejected() {
        let samples = vec![s(12.0, 0.7, [0.0, 0.0, 0.0], Some(70.0)); 6];
        assert!(judge(&samples).unwrap_err().contains("accelerometer"));
    }

    #[test]
    fn pins_that_do_not_fit_the_board_power_are_rejected() {
        // 6 x 12 V x 8 A = 576 W while NVML says 70 W
        let too_much = vec![s(12.0, 8.0, LEVEL, Some(70.0)); 6];
        assert!(judge(&too_much).unwrap_err().contains("board draws"));
        // ~0 W on the pins while the board draws 500 W: the slot cannot supply 500 W
        let too_little = vec![s(12.0, 0.0, LEVEL, Some(500.0)); 6];
        assert!(judge(&too_little).unwrap_err().contains("board draws"));
        // Heavy load, slot supplying ~60 W of 575 W: fine
        let gaming = vec![s(11.9, 7.2, LEVEL, Some(575.0)); 6];
        assert!(judge(&gaming).is_ok(), "{:?}", judge(&gaming));
    }

    #[test]
    fn one_failed_read_rejects_the_adapter() {
        let mut samples = vec![s(12.05, 0.75, LEVEL, Some(70.0)); 6];
        samples[3] = Err("I2C_RDWR failed".into());
        assert!(judge(&samples).unwrap_err().contains("read failed"));
        assert!(judge(&[]).is_err());
    }

    #[test]
    fn without_nvml_the_other_checks_still_decide() {
        let ok = vec![s(12.05, 0.75, LEVEL, None); 6];
        assert!(judge(&ok).unwrap().contains("not compared"));
        let bad = vec![s(0.0, 0.0, LEVEL, None); 6];
        assert!(judge(&bad).is_err());
    }

    #[test]
    fn card_part_of_the_adapter_name() {
        assert_eq!(card_of("NVIDIA i2c adapter 3 at 1:00.0"), "1:00.0");
    }

    // ---- whole scans on fake adapters: safety rule 1's order, and what gets saved.

    use crate::sensor::fake::encode_pin_block;
    use crate::sensor::GSENSOR_COUNTS_PER_G;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    #[derive(Clone, Copy)]
    enum Chip {
        /// Nothing acknowledges 0x2B.
        Silent,
        /// The IT8915, every pin at `amps` on 12.1 V, level.
        Monitor { amps: f64 },
        /// Something at 0x2B that is not the pin monitor (it answers with zeros).
        Other,
        /// The device node cannot be opened.
        Locked,
    }

    struct FakeProbe {
        num: u32,
        chip: Chip,
        log: Log,
    }

    impl I2cBus for FakeProbe {
        fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String> {
            self.log.lock().unwrap().push(format!("i2c-{} read 0x{reg:02X}", self.num));
            let Chip::Monitor { amps } = self.chip else {
                buf.fill(0);
                return Ok(());
            };
            match reg {
                PIN_REG => buf.copy_from_slice(&encode_pin_block(&[(12.1, amps); 6])),
                GSENSOR_REG => {
                    let g = |v: f64| ((v * GSENSOR_COUNTS_PER_G) as i16).to_be_bytes();
                    buf[..6].copy_from_slice(&[g(0.0), g(0.0), g(1.0)].concat());
                    buf[6] = 0;
                }
                _ => return Err("no such register".into()),
            }
            Ok(())
        }
    }

    impl Probe for FakeProbe {
        fn answers(&mut self) -> bool {
            self.log.lock().unwrap().push(format!("i2c-{} receive byte", self.num));
            !matches!(self.chip, Chip::Silent)
        }
    }

    struct FakeEnv {
        adapters: Vec<(u32, &'static str, u32, Chip)>,
        board_w: Option<f64>,
        log: Log,
    }

    impl FakeEnv {
        /// Card 0x89E31043 at 1:00.0 with these chips on adapters 9, 10, ...
        fn card(chips: &[Chip]) -> FakeEnv {
            let names = ["1", "2", "3", "4", "5", "6", "7"];
            FakeEnv {
                adapters: chips.iter().enumerate().map(|(i, &c)| (9 + i as u32, names[i], 0x89E3_1043, c)).collect(),
                board_w: Some(6.0 * 12.1 * 8.0 + 30.0), // the pins plus some slot power
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn ops(&self, num: u32) -> Vec<String> {
            let p = format!("i2c-{num} ");
            self.log.lock().unwrap().iter().filter(|l| l.starts_with(&p)).map(|l| l[p.len()..].to_string()).collect()
        }
    }

    impl ScanEnv for FakeEnv {
        fn adapters(&mut self) -> Result<Vec<Adapter>, String> {
            Ok(self
                .adapters
                .iter()
                .map(|&(num, n, subsystem, _)| Adapter {
                    num,
                    name: format!("NVIDIA i2c adapter {n} at {}:00.0", if subsystem == 0x89E3_1043 { 1 } else { 2 }),
                    subsystem,
                    model: "ROG Astral RTX 5090 OC",
                    gpu_name: "NVIDIA GPU".into(),
                })
                .collect())
        }
        fn open(&mut self, num: u32) -> Result<Box<dyn Probe>, String> {
            self.log.lock().unwrap().push(format!("i2c-{num} open"));
            let chip = self.adapters.iter().find(|a| a.0 == num).unwrap().3;
            if matches!(chip, Chip::Locked) {
                return Err("Permission denied".into());
            }
            Ok(Box::new(FakeProbe { num, chip, log: Arc::clone(&self.log) }))
        }
        fn board_power(&mut self, _subsystem: u32) -> Result<BoardWatts, String> {
            let w = self.board_w.ok_or("NVML unavailable (no driver)")?;
            Ok(Box::new(move || Some(w)))
        }
        fn pause(&mut self, _d: Duration) {}
    }

    struct TempIni(std::path::PathBuf);
    impl TempIni {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("ag-scan-{name}-{}.ini", std::process::id()));
            let _ = std::fs::remove_file(&p);
            TempIni(p)
        }
        fn saved(&self) -> Option<I2cChoice> {
            config::load_i2c_choice(&self.0)
        }
    }
    impl Drop for TempIni {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    const MONITOR: Chip = Chip::Monitor { amps: 8.0 };

    #[test]
    fn the_one_adapter_with_the_pin_monitor_is_found_and_saved() {
        let ini = TempIni::new("found");
        let mut env = FakeEnv::card(&[Chip::Silent, MONITOR, Chip::Other, Chip::Silent]);
        let r = scan_with(&mut env, true, &ini.0);
        assert!(r.found, "{}", r.message);
        assert!(r.message.contains("/dev/i2c-10"), "{}", r.message);
        let saved = ini.saved().expect("saved");
        assert_eq!((saved.adapter.as_str(), saved.subsystem), ("NVIDIA i2c adapter 2 at 1:00.0", 0x89E3_1043));
        assert!(r.lines.iter().any(|l| l.contains("i2c-11") && l.contains("not the pin monitor")), "{:?}", r.lines);
    }

    #[test]
    fn a_silent_adapter_gets_the_receive_byte_and_nothing_else() {
        // Safety rule 1: no register pointer is written to an adapter until something at
        // 0x2B has acknowledged the plain receive byte.
        let ini = TempIni::new("order");
        let mut env = FakeEnv::card(&[Chip::Silent, MONITOR, Chip::Other]);
        scan_with(&mut env, true, &ini.0);
        assert_eq!(env.ops(9), ["open", "receive byte"]);
        for num in [10, 11] {
            let ops = env.ops(num);
            assert_eq!(&ops[..2], ["open", "receive byte"], "the receive byte first on i2c-{num}");
            assert_eq!(ops.len(), 2 + 2 * SAMPLES, "then the pin and accelerometer reads, {SAMPLES} samples");
            assert!(ops[2..].iter().all(|o| o == "read 0x80" || o == "read 0x70"), "only those two: {ops:?}");
        }
    }

    #[test]
    fn two_adapters_that_pass_save_nothing() {
        let ini = TempIni::new("ambiguous");
        let mut env = FakeEnv::card(&[MONITOR, Chip::Silent, MONITOR]);
        let r = scan_with(&mut env, true, &ini.0);
        assert!(!r.found);
        assert!(r.message.contains("2 adapters passed") && r.message.contains("nothing saved"), "{}", r.message);
        assert!(ini.saved().is_none(), "a guess would be worse than no answer");
    }

    #[test]
    fn no_adapter_that_passes_saves_nothing() {
        let ini = TempIni::new("none");
        let mut env = FakeEnv::card(&[Chip::Silent, Chip::Other, Chip::Silent]);
        let r = scan_with(&mut env, true, &ini.0);
        assert!(!r.found && r.message.contains("Nothing saved"), "{}", r.message);
        assert!(ini.saved().is_none());
    }

    #[test]
    fn two_cards_are_refused_before_any_bus_is_touched() {
        let ini = TempIni::new("two-cards");
        let mut env = FakeEnv::card(&[MONITOR, Chip::Silent]);
        env.adapters.push((20, "1", 0x89FF_1043, MONITOR)); // a second Astral
        let r = scan_with(&mut env, true, &ini.0);
        assert!(!r.found && r.message.contains("More than one Astral card"), "{}", r.message);
        assert!(env.log.lock().unwrap().is_empty(), "not one adapter opened");
        assert!(ini.saved().is_none());
    }

    #[test]
    fn a_dry_run_finds_but_does_not_save() {
        let ini = TempIni::new("dry");
        let mut env = FakeEnv::card(&[MONITOR]);
        let r = scan_with(&mut env, false, &ini.0);
        assert!(r.found && r.message.contains("Not saved (dry run)"), "{}", r.message);
        assert!(ini.saved().is_none());
    }

    #[test]
    fn without_nvml_the_monitor_is_judged_on_the_pins_alone() {
        let ini = TempIni::new("no-nvml");
        let mut env = FakeEnv::card(&[MONITOR]);
        env.board_w = None;
        let r = scan_with(&mut env, true, &ini.0);
        assert!(r.found, "{}", r.message);
        assert!(r.lines.iter().any(|l| l.contains("board power not compared")), "{:?}", r.lines);
    }

    #[test]
    fn an_adapter_that_cannot_be_opened_is_skipped_not_fatal() {
        let ini = TempIni::new("locked");
        let mut env = FakeEnv::card(&[Chip::Locked, MONITOR]);
        let r = scan_with(&mut env, true, &ini.0);
        assert!(r.found, "{}", r.message);
        assert!(
            r.lines.iter().any(|l| l.contains("i2c-9") && l.contains("skipped: Permission denied")),
            "{:?}",
            r.lines
        );
    }

    #[test]
    fn board_power_that_does_not_fit_the_pins_rejects_the_chip() {
        let ini = TempIni::new("power");
        let mut env = FakeEnv::card(&[MONITOR]);
        env.board_w = Some(2000.0); // the pins carry ~580 W: not this card's pins
        let r = scan_with(&mut env, true, &ini.0);
        assert!(!r.found);
        assert!(r.lines.iter().any(|l| l.contains("board draws 2000 W")), "{:?}", r.lines);
    }
}
