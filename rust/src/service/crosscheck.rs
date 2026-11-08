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
        if hw.len() != pins.len() || hw.is_empty() {
            return;
        }
        let direct_max = pins.iter().map(|p| p.amps).fold(f64::MIN, f64::max);
        let hwinfo_max = hw.iter().copied().fold(f64::MIN, f64::max);
        let worst = pins.iter().zip(&hw).map(|(p, h)| (p.amps - h).abs()).fold(0.0, f64::max);
        self.samples += 1;
        self.worst = self.worst.max(worst);
        if !self.announced {
            self.announced = true;
            self.summary_due = now + SUMMARY_EVERY_SEC;
            logger::info(format!(
                "Crosscheck active: direct max {direct_max:.2}A vs HWiNFO max {hwinfo_max:.2}A \
                 (worst per-pin delta {worst:.2}A); hourly summaries follow"
            ));
        }
        if worst >= WARN_DELTA_AMPS {
            logger::warn(format!(
                "Crosscheck disagreement: direct max {direct_max:.2}A vs HWiNFO max {hwinfo_max:.2}A \
                 (worst per-pin delta {worst:.2}A)"
            ));
        }
        if self.summary_due > 0.0 && now >= self.summary_due {
            logger::info(format!(
                "Crosscheck: {} samples in the last hour, worst per-pin delta {:.2}A",
                self.samples, self.worst
            ));
            self.samples = 0;
            self.worst = 0.0;
            self.summary_due = now + SUMMARY_EVERY_SEC;
        }
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

    const MAGIC: u32 = u32::from_le_bytes(*b"HWiS");
    const STALE_AFTER_SEC: f64 = 30.0;
    // SYNCHRONIZE | MUTEX_MODIFY_STATE: the least that lets us wait and release.
    const MUTEX_NEEDED: u32 = 0x0010_0000 | 0x0001;

    #[repr(C, packed)]
    #[derive(Clone, Copy)]
    struct Header {
        magic: u32,
        version: u32,
        version2: u32,
        last_update: i64,
        sensor_section_offset: u32,
        sensor_element_size: u32,
        sensor_element_count: u32,
        entry_section_offset: u32,
        entry_element_size: u32,
        entry_element_count: u32,
    }

    const ENTRY_NAME_ORIG: usize = 12;
    const ENTRY_NAME_USER: usize = 12 + 128;
    const ENTRY_VALUE: usize = 12 + 128 + 128 + 16;
    const ENTRY_MIN_SIZE: usize = ENTRY_VALUE + 8;

    fn cstr(b: &[u8]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).trim().to_string()
    }

    /// HWiNFO's own labels, "GPU 12VHPWR Pin1 Current" .. Pin6, in pin order.
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
        let result = (|| {
            let base = view.Value as *const u8;
            let header: Header = std::ptr::read_unaligned(base as *const Header);
            if header.magic != MAGIC {
                return None;
            }
            // A timestamp in the future counts as fresh.
            if crate::now() - header.last_update as f64 > STALE_AFTER_SEC {
                return None;
            }
            let size = header.entry_element_size as usize;
            if header.entry_section_offset == 0 || size < ENTRY_MIN_SIZE {
                return None;
            }
            let mut pins = [None::<f64>; 6];
            for i in 0..header.entry_element_count as usize {
                let entry = std::slice::from_raw_parts(
                    base.add(header.entry_section_offset as usize + i * size),
                    ENTRY_MIN_SIZE,
                );
                let orig = cstr(&entry[ENTRY_NAME_ORIG..ENTRY_NAME_ORIG + 128]);
                let user = cstr(&entry[ENTRY_NAME_USER..ENTRY_NAME_USER + 128]);
                for (n, slot) in pins.iter_mut().enumerate() {
                    let label = format!("GPU 12VHPWR Pin{} Current", n + 1);
                    if orig == label || user == label {
                        let mut v = [0u8; 8];
                        v.copy_from_slice(&entry[ENTRY_VALUE..ENTRY_VALUE + 8]);
                        *slot = Some(f64::from_le_bytes(v));
                    }
                }
            }
            pins.iter().copied().collect::<Option<Vec<f64>>>()
        })();
        if let Some(m) = mutex {
            let _ = ReleaseMutex(m);
        }
        result
    }
}
