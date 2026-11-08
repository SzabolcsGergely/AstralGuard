//! Per-pin 12V-2x6 current and the accelerometer, read from the card's own ITE IT8915FN
//! monitor chip over the GPU's I2C bus. The guard's only sensor source.
//!
//! READ-ONLY. Every transaction is "set register pointer, read N bytes" to the IT8915 at
//! 0x2B. Nothing here may ever write a data register: a write to the wrong device on a GPU
//! I2C bus can permanently damage hardware.
//!
//! VERIFIED CONSTANTS (2026-08-04, dual witness: Timic3/astral-power-monitoring and
//! LibreHardwareMonitor's TryReadAstral12VHPwrPinSensors; protocol only, no code copied)
//!   Port 1, device 0x2B, register 0x80, 24 bytes: 6 blocks of (u16 BE mV, u16 BE mA) in
//!   REVERSE pin order: Pin1 = bytes 20/22 ... Pin6 = bytes 0/2. Confirmed on a ROG Astral
//!   RTX 5090 OC (0x89E31043): within 0.06 A of HWiNFO.
//! G-SENSOR (2026-09-30): the card's Bosch BMI321, relayed by the same IT8915 (GPU Tweak
//!   III's ExpanModule.dll reads it this way). Registers 0x70..0x76: X, Y, Z as s16 BE at
//!   ~4096 counts per g, then a status byte. Card horizontal: X=-35 Y=-33 Z=+4090.

#[cfg(target_os = "linux")]
mod i2cdev;
#[cfg(windows)]
mod nvapi;

use std::fmt;

pub const IT8915_ADDRESS_7BIT: u8 = 0x2B;
pub const PIN_REG: u8 = 0x80;
pub const PIN_DATA_SIZE: usize = 24;
pub const GSENSOR_REG: u8 = 0x70;
pub const GSENSOR_DATA_SIZE: usize = 7;
pub const GSENSOR_COUNTS_PER_G: f64 = 4096.0;

/// A garbage read (0xFFFF -> 65.535) must never reach the threshold engine. Real danger
/// currents (10-20 A) pass untouched; only impossible values are rejected.
pub const PLAUSIBLE_MAX_AMPS: f64 = 30.0;
pub const PLAUSIBLE_MAX_VOLTS: f64 = 16.0;
/// At rest the accelerometer sees 1 g. Far off is a bad read or a knock mid-sample.
pub const PLAUSIBLE_MIN_G: f64 = 0.7;
pub const PLAUSIBLE_MAX_G: f64 = 1.3;
/// Consecutive transient failures before the sensor is declared gone.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Supported cards by PCI subsystem id, with the length (mm, ASUS spec) used to turn a
/// sag angle into mm of droop at the far end.
pub const ASTRAL_CARDS: &[(u32, &str, f64)] = &[
    (0x89EA1043, "ROG Astral RTX 5090D OC", 357.6),
    (0x8A611043, "ROG Astral RTX 5090 Matrix", 370.3),
    (0x89EC1043, "ROG Astral RTX 5090 LC", 288.46),
    (0x89E31043, "ROG Astral RTX 5090 OC", 357.6),
    (0x89DE1043, "ROG Astral RTX 5080 OC", 357.6),
    (0x8A2E1043, "ROG Astral RTX 5090 OC White", 357.6),
    (0x8A2B1043, "ROG Astral RTX 5080 OC White", 357.6),
    (0x8A451043, "ROG Astral RTX 5080 OC Hatsune Miku", 357.6),
    // BTF cards: the card-side sensors only carry the load on the 16-pin cable in a
    // standard board, not through a GC-HPWR slot.
    (0x8A5A1043, "ROG Astral RTX 5090 BTF OC", 357.6),
    (0x8A3C1043, "ROG Astral RTX 5090 BTF OC", 357.6),
    (0x89DF1043, "ROG Astral RTX 5080 (non-OC)", 357.6),
];
/// Every air-cooled Astral shares this cooler.
pub const DEFAULT_CARD_LENGTH_MM: f64 = 357.6;

pub fn model_for(subsystem: u32) -> Option<&'static str> {
    ASTRAL_CARDS.iter().find(|c| c.0 == subsystem).map(|c| c.1)
}

pub fn card_length_mm(subsystem: Option<u32>) -> f64 {
    subsystem.and_then(|s| ASTRAL_CARDS.iter().find(|c| c.0 == s)).map(|c| c.2).unwrap_or(DEFAULT_CARD_LENGTH_MM)
}

#[derive(Debug)]
pub enum ReadError {
    /// One sample failed or was implausible: skip this tick, keep the connection.
    Transient(String),
    /// The source died; tear down and reconnect.
    Gone(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Transient(m) | ReadError::Gone(m) => f.write_str(m),
        }
    }
}

/// One pin sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    /// "Pin1".."Pin6"
    pub name: String,
    pub amps: f64,
    pub volts: f64,
}

/// The highest pin; the first one on a tie, as the Python guard reported it.
pub fn max_pin(pins: &[Pin]) -> Option<&Pin> {
    pins.iter().reduce(|a, b| if b.amps > a.amps { b } else { a })
}

/// The lowest pin; the first one on a tie.
pub fn min_pin(pins: &[Pin]) -> Option<&Pin> {
    pins.iter().reduce(|a, b| if b.amps < a.amps { b } else { a })
}

/// A raw read-only I2C transaction to the IT8915 on the card's sensor bus.
pub trait I2cBus: Send {
    /// Set the register pointer to `reg` and read `buf.len()` bytes.
    fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String>;
}

pub struct CardInfo {
    pub gpu_name: String,
    pub subsystem: u32,
    pub model: &'static str,
}

/// 24 raw bytes -> pins 1..6 as (volts, amps). Blocks are stored in reverse pin order.
pub fn parse_pin_block(raw: &[u8; PIN_DATA_SIZE]) -> Vec<(u8, f64, f64)> {
    let u16be = |o: usize| ((raw[o] as u16) << 8 | raw[o + 1] as u16) as f64;
    (1..=6u8)
        .map(|pin| {
            let base = (6 - pin as usize) * 4;
            (pin, u16be(base) * 0.001, u16be(base + 2) * 0.001)
        })
        .collect()
}

pub fn is_plausible(volts: f64, amps: f64) -> bool {
    (0.0..=PLAUSIBLE_MAX_VOLTS).contains(&volts) && (0.0..=PLAUSIBLE_MAX_AMPS).contains(&amps)
}

/// 7 raw bytes from 0x70 -> (x, y, z) in g.
pub fn parse_gsensor_block(raw: &[u8; GSENSOR_DATA_SIZE]) -> [f64; 3] {
    let s16be = |o: usize| i16::from_be_bytes([raw[o], raw[o + 1]]) as f64 / GSENSOR_COUNTS_PER_G;
    [s16be(0), s16be(2), s16be(4)]
}

pub fn is_plausible_g(v: [f64; 3]) -> bool {
    let g = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (PLAUSIBLE_MIN_G..=PLAUSIBLE_MAX_G).contains(&g)
}

/// The connected card's sensor chip.
pub struct PinSensor {
    bus: Box<dyn I2cBus>,
    pub info: CardInfo,
    failures: u32,
}

impl PinSensor {
    /// Find a supported card and prove its chip answers. Err explains why not.
    pub fn connect() -> Result<PinSensor, String> {
        let (bus, info) = open_bus()?;
        let mut sensor = PinSensor { bus, info, failures: 0 };
        sensor.read_pins().map_err(|e| format!("sensor chip did not answer: {e}"))?;
        Ok(sensor)
    }

    pub fn description(&self) -> String {
        format!("{} [{}]", self.info.gpu_name, self.info.model)
    }

    /// One sample of all six pins. Transient for a single bad sample, Gone once failures
    /// persist past MAX_CONSECUTIVE_FAILURES.
    pub fn read_pins(&mut self) -> Result<Vec<Pin>, ReadError> {
        match self.read_pins_once() {
            Ok(pins) => {
                self.failures = 0;
                Ok(pins)
            }
            Err(msg) => {
                self.failures += 1;
                if self.failures >= MAX_CONSECUTIVE_FAILURES {
                    Err(ReadError::Gone(format!("{} consecutive failed I2C reads ({msg})", self.failures)))
                } else {
                    Err(ReadError::Transient(msg))
                }
            }
        }
    }

    fn read_pins_once(&mut self) -> Result<Vec<Pin>, String> {
        let mut raw = [0u8; PIN_DATA_SIZE];
        self.bus.read(PIN_REG, &mut raw)?;
        let parsed = parse_pin_block(&raw);
        if let Some((_, v, a)) = parsed.iter().find(|(_, v, a)| !is_plausible(*v, *a)) {
            return Err(format!("implausible sample (V={v:.3} A={a:.3})"));
        }
        Ok(parsed.into_iter().map(|(pin, volts, amps)| Pin { name: format!("Pin{pin}"), amps, volts }).collect())
    }

    /// One accelerometer sample in g. Never counts against the pin failure counter: a
    /// flaky tilt reading must never be able to disconnect the power guard.
    pub fn read_gsensor(&mut self) -> Result<[f64; 3], String> {
        let mut raw = [0u8; GSENSOR_DATA_SIZE];
        self.bus.read(GSENSOR_REG, &mut raw)?;
        let v = parse_gsensor_block(&raw);
        if !is_plausible_g(v) {
            return Err(format!("implausible g-sensor sample ({:.3}, {:.3}, {:.3})", v[0], v[1], v[2]));
        }
        Ok(v)
    }
}

#[cfg(windows)]
fn open_bus() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    nvapi::open()
}

#[cfg(target_os = "linux")]
fn open_bus() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    i2cdev::open()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open_bus() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    Err("no I2C backend for this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_block_is_reverse_order_be() {
        let mut raw = [0u8; 24];
        // Pin1 at bytes 20..24: 12.100 V, 8.250 A
        raw[20..22].copy_from_slice(&12100u16.to_be_bytes());
        raw[22..24].copy_from_slice(&8250u16.to_be_bytes());
        // Pin6 at bytes 0..4: 11.900 V, 0.500 A
        raw[0..2].copy_from_slice(&11900u16.to_be_bytes());
        raw[2..4].copy_from_slice(&500u16.to_be_bytes());
        let p = parse_pin_block(&raw);
        assert_eq!(p[0], (1, 12.1, 8.25));
        assert_eq!(p[5].0, 6);
        assert!((p[5].1 - 11.9).abs() < 1e-9 && (p[5].2 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn garbage_is_implausible() {
        assert!(!is_plausible(65.535, 1.0));
        assert!(!is_plausible(12.0, 65.535));
        assert!(is_plausible(12.0, 22.0));
    }

    #[test]
    fn gsensor_signed() {
        let mut raw = [0u8; 7];
        raw[0..2].copy_from_slice(&(-35i16).to_be_bytes());
        raw[2..4].copy_from_slice(&(-33i16).to_be_bytes());
        raw[4..6].copy_from_slice(&4090i16.to_be_bytes());
        let v = parse_gsensor_block(&raw);
        assert!((v[0] + 35.0 / 4096.0).abs() < 1e-12);
        assert!(is_plausible_g(v));
        assert!(!is_plausible_g([0.0, 0.0, 0.0]));
    }
}
