//! The sensor scan (`astralguard probe --detect`, or Scan in the dashboard): find which
//! of the Astral's I2C adapters carries the IT8915, prove it, and save it for the service
//! (Linux only).
//!
//! The one place that talks to more than one adapter, and only when a person asks for it.
//! The service runs it only on the dashboard's request, and only while no sensor is
//! reading; otherwise it uses the single saved adapter (see i2cdev.rs).
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

fn sample(bus: &mut I2cDev, power: Option<&crate::mitigation::BoardPower>) -> Result<Sample, String> {
    let mut raw = [0u8; PIN_DATA_SIZE];
    bus.read(PIN_REG, &mut raw)?;
    let mut graw = [0u8; GSENSOR_DATA_SIZE];
    bus.read(GSENSOR_REG, &mut graw)?;
    let board_w = power.and_then(|p| p.watts().ok());
    Ok(Sample { pins: parse_pin_block(&raw), g: parse_gsensor_block(&graw), board_w })
}

enum Outcome {
    Silent,
    Unopenable(String),
    Rejected(String),
    Accepted(String),
}

fn try_adapter(a: &Adapter, power: Option<&crate::mitigation::BoardPower>) -> Outcome {
    let mut bus = match I2cDev::open(a.num) {
        Ok(b) => b,
        Err(e) => return Outcome::Unopenable(e),
    };
    if !bus.answers() {
        return Outcome::Silent;
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        if i > 0 {
            std::thread::sleep(SAMPLE_GAP);
        }
        samples.push(sample(&mut bus, power));
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
    let fail = |message: String, lines: Vec<String>| ScanReport { found: false, message, lines };
    let all = match adapters() {
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
    let power = match crate::mitigation::BoardPower::open(first.subsystem) {
        Ok(p) => Some(p),
        Err(e) => {
            lines.push(format!("note: NVML unavailable ({e}); board power not compared"));
            None
        }
    };
    let mut accepted = Vec::new();
    for a in &all {
        let verdict = match try_adapter(a, power.as_ref()) {
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
    let path = config::config_path();
    let choice = I2cChoice { adapter: chosen.name.clone(), subsystem: chosen.subsystem };
    match config::save_i2c_choice(&path, &choice) {
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
}
