//! Observational only: compare the chip's readings against HWiNFO's when HWiNFO happens
//! to be running with shared memory on (Windows). Never influences a decision, silently
//! does nothing without HWiNFO. Samples every minute, logs one summary per hour, and a
//! real disagreement (beyond any sampling skew; field data tops out at 0.44 A) at once.

use crate::logger;
use crate::sensor::Pin;

const EVERY_SEC: f64 = 60.0;
const SUMMARY_EVERY_SEC: f64 = 3600.0;
const WARN_DELTA_AMPS: f64 = 2.0;

pub struct CrossCheck {
    next: f64,
    announced: bool,
    summary_due: f64,
    samples: u32,
    worst: f64,
}

impl CrossCheck {
    pub fn new() -> Self {
        CrossCheck { next: 0.0, announced: false, summary_due: 0.0, samples: 0, worst: 0.0 }
    }

    pub fn maybe_log(&mut self, pins: &[Pin], now: f64) {
        if now < self.next {
            return;
        }
        self.next = now + EVERY_SEC;
        let Some(hw) = hwinfo::read_pins() else { return };
        let direct: Vec<f64> = pins.iter().map(|p| p.amps).collect();
        for line in self.compare(&direct, &hw, now) {
            match line {
                Line::Info(m) => logger::info(m),
                Line::Warn(m) => logger::warn(m),
            }
        }
    }

    /// One sample of both sources: what to log. Nothing when they don't line up pin for pin.
    fn compare(&mut self, direct: &[f64], hw: &[f64], now: f64) -> Vec<Line> {
        let mut out = Vec::new();
        if hw.len() != direct.len() || hw.is_empty() {
            return out;
        }
        let direct_max = direct.iter().copied().fold(f64::MIN, f64::max);
        let hwinfo_max = hw.iter().copied().fold(f64::MIN, f64::max);
        let worst = direct.iter().zip(hw).map(|(d, h)| (d - h).abs()).fold(0.0, f64::max);
        self.samples += 1;
        self.worst = self.worst.max(worst);
        if !self.announced {
            self.announced = true;
            self.summary_due = now + SUMMARY_EVERY_SEC;
            out.push(Line::Info(format!(
                "Crosscheck active: direct max {direct_max:.2}A vs HWiNFO max {hwinfo_max:.2}A \
                 (worst per-pin delta {worst:.2}A); hourly summaries follow"
            )));
        }
        if worst >= WARN_DELTA_AMPS {
            out.push(Line::Warn(format!(
                "Crosscheck disagreement: direct max {direct_max:.2}A vs HWiNFO max {hwinfo_max:.2}A \
                 (worst per-pin delta {worst:.2}A)"
            )));
        }
        if self.summary_due > 0.0 && now >= self.summary_due {
            out.push(Line::Info(format!(
                "Crosscheck: {} samples in the last hour, worst per-pin delta {:.2}A",
                self.samples, self.worst
            )));
            self.samples = 0;
            self.worst = 0.0;
            self.summary_due = now + SUMMARY_EVERY_SEC;
        }
        out
    }
}

#[derive(Debug, PartialEq)]
enum Line {
    Info(String),
    Warn(String),
}

/// HWiNFO's SM2 shared memory: a packed header, then fixed-size entries. Plain byte parsing,
/// so it is tested on every OS; only the mapping itself is Windows-only.
#[cfg_attr(not(windows), allow(dead_code))]
mod sm2 {
    const MAGIC: u32 = u32::from_le_bytes(*b"HWiS");
    const STALE_AFTER_SEC: f64 = 30.0;
    /// The packed header: magic, version, version2, last_update (i64), the sensor section's
    /// offset / size / count, the entry section's offset / size / count.
    pub const HEADER_LEN: usize = 44;
    const LAST_UPDATE: usize = 12;
    const ENTRY_OFFSET: usize = 32;
    const ENTRY_SIZE: usize = 36;
    const ENTRY_COUNT: usize = 40;

    const ENTRY_NAME_ORIG: usize = 12;
    const ENTRY_NAME_USER: usize = 12 + 128;
    const ENTRY_VALUE: usize = 12 + 128 + 128 + 16;
    const ENTRY_MIN_SIZE: usize = ENTRY_VALUE + 8;

    fn u32_at(b: &[u8], at: usize) -> usize {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
    }

    /// The bytes the whole block takes, from its header; None when it is not HWiNFO's, is
    /// stale (HWiNFO closed without removing it) or has no usable entries.
    pub fn total_len(header: &[u8], now: f64) -> Option<usize> {
        if header.len() < HEADER_LEN || u32_at(header, 0) != MAGIC as usize {
            return None;
        }
        let last_update = i64::from_le_bytes(header[LAST_UPDATE..LAST_UPDATE + 8].try_into().unwrap());
        // A timestamp in the future counts as fresh.
        if now - last_update as f64 > STALE_AFTER_SEC {
            return None;
        }
        let (offset, size, count) =
            (u32_at(header, ENTRY_OFFSET), u32_at(header, ENTRY_SIZE), u32_at(header, ENTRY_COUNT));
        if offset < HEADER_LEN || size < ENTRY_MIN_SIZE {
            return None;
        }
        size.checked_mul(count)?.checked_add(offset)
    }

    fn cstr(b: &[u8]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).trim().to_string()
    }

    /// HWiNFO's own labels, "GPU 12VHPWR Pin1 Current" .. Pin6, in pin order (the original
    /// name or one the user gave it); None unless all six are there.
    pub fn pins(block: &[u8], now: f64) -> Option<Vec<f64>> {
        let len = total_len(block, now)?;
        if block.len() < len {
            return None;
        }
        let (offset, size) = (u32_at(block, ENTRY_OFFSET), u32_at(block, ENTRY_SIZE));
        let mut pins = [None::<f64>; 6];
        for entry in block[offset..len].chunks_exact(size) {
            let orig = cstr(&entry[ENTRY_NAME_ORIG..ENTRY_NAME_ORIG + 128]);
            let user = cstr(&entry[ENTRY_NAME_USER..ENTRY_NAME_USER + 128]);
            for (n, slot) in pins.iter_mut().enumerate() {
                let label = format!("GPU 12VHPWR Pin{} Current", n + 1);
                if orig == label || user == label {
                    *slot = Some(f64::from_le_bytes(entry[ENTRY_VALUE..ENTRY_VALUE + 8].try_into().unwrap()));
                }
            }
        }
        pins.iter().copied().collect()
    }
}

#[cfg(not(windows))]
mod hwinfo {
    pub fn read_pins() -> Option<Vec<f64>> {
        None
    }
}

#[cfg(windows)]
mod hwinfo {
    //! HWiNFO's SM2 shared memory: a packed header, then fixed-size entries. Read under
    //! HWiNFO's mutex and copied out before it is released, so we never race its writer.

    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    };
    use windows::Win32::System::Threading::{
        OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_ACCESS_RIGHTS,
    };

    // SYNCHRONIZE | MUTEX_MODIFY_STATE: the least that lets us wait and release.
    const MUTEX_NEEDED: u32 = 0x0010_0000 | 0x0001;

    use super::sm2;

    /// The six pin currents as HWiNFO sees them, when it runs with shared memory on.
    pub fn read_pins() -> Option<Vec<f64>> {
        unsafe {
            let map = OpenFileMappingW(FILE_MAP_READ.0, false, w!("Global\\HWiNFO_SENS_SM2")).ok()?;
            let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                return None;
            }
            let mutex: Option<HANDLE> =
                OpenMutexW(SYNCHRONIZATION_ACCESS_RIGHTS(MUTEX_NEEDED), false, w!("Global\\HWiNFO_SM2_MUTEX")).ok();
            let result = read_locked(view, mutex);
            if let Some(m) = mutex {
                let _ = CloseHandle(m);
            }
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(map);
            result
        }
    }

    unsafe fn read_locked(view: MEMORY_MAPPED_VIEW_ADDRESS, mutex: Option<HANDLE>) -> Option<Vec<f64>> {
        if let Some(m) = mutex {
            let wait = WaitForSingleObject(m, 2000);
            if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                return None;
            }
        }
        // HWiNFO's header says how long the block is; the view maps all of it.
        let base = view.Value as *const u8;
        let now = crate::now();
        let result = sm2::total_len(std::slice::from_raw_parts(base, sm2::HEADER_LEN), now)
            .and_then(|len| sm2::pins(std::slice::from_raw_parts(base, len), now));
        if let Some(m) = mutex {
            let _ = ReleaseMutex(m);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIX: [f64; 6] = [8.0, 8.1, 7.9, 8.2, 8.0, 7.8];

    #[test]
    fn the_first_match_is_announced_then_quiet_until_the_hour() {
        let mut c = CrossCheck::new();
        let hw = SIX.map(|a| a + 0.3);
        let first = c.compare(&SIX, &hw, 0.0);
        assert_eq!(first.len(), 1);
        assert!(matches!(&first[0], Line::Info(m) if m.starts_with("Crosscheck active") && m.contains("delta 0.30A")));
        for t in 1..60 {
            assert!(c.compare(&SIX, &hw, t as f64 * 60.0).is_empty(), "nothing in between");
        }
        let summary = c.compare(&SIX, &SIX, SUMMARY_EVERY_SEC);
        assert_eq!(summary, [Line::Info("Crosscheck: 61 samples in the last hour, worst per-pin delta 0.30A".into())]);
        assert!(c.compare(&SIX, &SIX, SUMMARY_EVERY_SEC + 60.0).is_empty(), "counting starts over");
        let next = c.compare(&SIX, &SIX, 2.0 * SUMMARY_EVERY_SEC);
        assert_eq!(next, [Line::Info("Crosscheck: 2 samples in the last hour, worst per-pin delta 0.00A".into())]);
    }

    #[test]
    fn a_real_disagreement_is_logged_at_once() {
        let mut c = CrossCheck::new();
        c.compare(&SIX, &SIX, 0.0);
        let mut hw = SIX;
        hw[3] = 5.7; // 2.5 A off on one pin
        let out = c.compare(&SIX, &hw, 60.0);
        assert!(matches!(&out[..], [Line::Warn(m)] if m.contains("delta 2.50A")), "{out:?}");
        hw[3] = 8.2 - 1.9; // sampling skew: not a disagreement
        assert!(c.compare(&SIX, &hw, 120.0).is_empty());
    }

    #[test]
    fn sources_that_do_not_line_up_are_ignored() {
        let mut c = CrossCheck::new();
        assert!(c.compare(&SIX, &SIX[..5], 0.0).is_empty());
        assert!(c.compare(&[], &[], 0.0).is_empty());
        assert_eq!(c.samples, 0);
        assert!(!c.announced);
    }

    #[test]
    fn maybe_log_samples_once_a_minute() {
        let mut c = CrossCheck::new();
        c.maybe_log(&[], 100.0);
        assert_eq!(c.next, 100.0 + EVERY_SEC);
        c.maybe_log(&[], 130.0);
        assert_eq!(c.next, 100.0 + EVERY_SEC, "not due yet");
    }

    /// An SM2 block as HWiNFO writes it: the header, then `entries` of (orig, user, amps).
    fn block(last_update: i64, entries: &[(&str, &str, f64)]) -> Vec<u8> {
        const SIZE: usize = 12 + 128 + 128 + 16 + 8 + 24; // HWiNFO's entries are longer than what is read
        let mut b = Vec::new();
        for v in [u32::from_le_bytes(*b"HWiS"), 2, 1] {
            b.extend(v.to_le_bytes());
        }
        b.extend(last_update.to_le_bytes());
        for v in [0u32, 0, 0, sm2::HEADER_LEN as u32, SIZE as u32, entries.len() as u32] {
            b.extend(v.to_le_bytes());
        }
        for (orig, user, amps) in entries {
            let mut e = vec![0u8; SIZE];
            e[12..12 + orig.len()].copy_from_slice(orig.as_bytes());
            e[140..140 + user.len()].copy_from_slice(user.as_bytes());
            e[284..292].copy_from_slice(&amps.to_le_bytes());
            b.extend(e);
        }
        b
    }

    fn six_pins() -> Vec<(String, String, f64)> {
        (1..=6).map(|n| (format!("GPU 12VHPWR Pin{n} Current"), String::new(), n as f64)).collect()
    }

    fn as_refs(v: &[(String, String, f64)]) -> Vec<(&str, &str, f64)> {
        v.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), *c)).collect()
    }

    #[test]
    fn hwinfo_pins_are_read_by_their_label_in_pin_order() {
        let mut entries = six_pins();
        entries.reverse();
        entries.insert(2, ("GPU Temperature".into(), String::new(), 61.0));
        let b = block(1000, &as_refs(&entries));
        assert_eq!(sm2::pins(&b, 1010.0), Some(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
    }

    #[test]
    fn a_renamed_hwinfo_entry_still_counts() {
        let mut entries = six_pins();
        entries[4] = ("Some other name".into(), "GPU 12VHPWR Pin5 Current".into(), 5.0);
        assert_eq!(sm2::pins(&block(0, &as_refs(&entries)), 0.0).unwrap()[4], 5.0);
    }

    #[test]
    fn a_hwinfo_block_that_is_missing_a_pin_stale_foreign_or_cut_short_is_ignored() {
        let entries = six_pins();
        let good = block(1000, &as_refs(&entries));
        assert!(sm2::pins(&good, 1000.0).is_some());
        assert_eq!(sm2::pins(&block(1000, &as_refs(&entries[..5])), 1000.0), None, "five pins");
        assert_eq!(sm2::pins(&good, 1031.0), None, "HWiNFO stopped updating it");
        assert!(sm2::pins(&good, 900.0).is_some(), "a clock ahead of ours is still fresh");
        let mut foreign = good.clone();
        foreign[0] = b'X';
        assert_eq!(sm2::pins(&foreign, 1000.0), None);
        assert_eq!(sm2::pins(&good[..good.len() - 1], 1000.0), None, "cut short");
        assert_eq!(sm2::pins(&good[..20], 1000.0), None, "not even a header");
        let mut tiny = good.clone();
        tiny[36..40].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(sm2::total_len(&tiny, 1000.0), None, "entries too small to hold a value");
    }
}
