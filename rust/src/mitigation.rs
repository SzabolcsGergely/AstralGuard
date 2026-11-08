//! Reversible GPU slowdown: the guard's first response to a pin over threshold.
//!
//! Lock the graphics clock to its floor and drop the power limit to the card's minimum,
//! then put both back exactly as they were. The card stops pulling current within about
//! a second, instead of the user losing their session to a shutdown.
//!
//! Nothing here touches I2C. This is NVML, the documented API behind `nvidia-smi -lgc`
//! and `nvidia-smi -pl`. Set calls need an elevated process (root on Linux).
//!
//! VERIFIED (2026-08-06, ROG Astral RTX 5090 OC, driver 610.88): nvmlDeviceGetPciInfo_v3
//! reports the subsystem id in the same DWORD format as NVAPI (0x89E31043). Power limit
//! 400-600 W. 400 W alone is NOT enough (~5.5 A per pin balanced); the clock lock is the
//! lever that works: idle sits at 412 MHz and 0.3-0.9 A per pin. Both are applied.
//!
//! Both changes are volatile; a driver reload or reboot clears them. The marker file
//! covers the case that is not self-healing: a crash while limited, then a restart.

use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;

use libloading::Library;
use serde::{Deserialize, Serialize};

use crate::logger;

const NVML_SUCCESS: i32 = 0;
const NVML_ERROR_INSUFFICIENT_SIZE: i32 = 7;
const NVML_DEVICE_NAME_BUFFER_SIZE: usize = 96;
const NVML_MAX_CLOCK_ENTRIES: u32 = 256;

/// Clock floors to try if the driver refuses the enumerated minimum. Each step is higher
/// and less effective, so this only walks upward as a last resort.
const CLOCK_FLOOR_LADDER: [u32; 3] = [500, 700, 1000];

#[cfg(windows)]
const NVML_LIBRARY: &str = "nvml.dll";
#[cfg(not(windows))]
const NVML_LIBRARY: &str = "libnvidia-ml.so.1";

pub fn marker_path() -> PathBuf {
    crate::config::data_dir().join("mitigation.active")
}

#[repr(C)]
struct NvmlPciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [u8; 32],
}

type Handle = *mut c_void;

/// Thin binding over the handful of NVML calls the guard needs. Every call returns
/// Err(status text) on a non-zero status; the caller decides which failures matter.
struct Nvml {
    lib: Library,
}

macro_rules! call {
    ($self:ident, $name:literal, $ty:ty, $($arg:expr),*) => {{
        let f = unsafe { $self.lib.get::<$ty>(concat!($name, "\0").as_bytes()) }
            .map_err(|_| format!("{} missing from NVML (driver too old)", $name))?;
        let status = unsafe { f($($arg),*) };
        if status != NVML_SUCCESS {
            return Err(format!("{} failed with NVML status {}", $name, status));
        }
    }};
}

impl Nvml {
    fn init() -> Result<Nvml, String> {
        let lib = unsafe { Library::new(NVML_LIBRARY) }.map_err(|e| format!("{NVML_LIBRARY} not loadable ({e})"))?;
        let nvml = Nvml { lib };
        call!(nvml, "nvmlInit_v2", unsafe extern "C" fn() -> i32,);
        Ok(nvml)
    }

    fn device_count(&self) -> Result<u32, String> {
        let mut n = 0u32;
        call!(self, "nvmlDeviceGetCount_v2", unsafe extern "C" fn(*mut u32) -> i32, &mut n);
        Ok(n)
    }

    fn handle(&self, index: u32) -> Result<Handle, String> {
        let mut h: Handle = std::ptr::null_mut();
        call!(self, "nvmlDeviceGetHandleByIndex_v2", unsafe extern "C" fn(u32, *mut Handle) -> i32, index, &mut h);
        Ok(h)
    }

    fn subsystem_id(&self, h: Handle) -> Result<u32, String> {
        let mut info: NvmlPciInfo = unsafe { std::mem::zeroed() };
        call!(self, "nvmlDeviceGetPciInfo_v3", unsafe extern "C" fn(Handle, *mut NvmlPciInfo) -> i32, h, &mut info);
        Ok(info.pci_sub_system_id)
    }

    fn name(&self, h: Handle) -> Result<String, String> {
        let mut buf = [0u8; NVML_DEVICE_NAME_BUFFER_SIZE];
        call!(
            self,
            "nvmlDeviceGetName",
            unsafe extern "C" fn(Handle, *mut u8, u32) -> i32,
            h,
            buf.as_mut_ptr(),
            NVML_DEVICE_NAME_BUFFER_SIZE as u32
        );
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    fn power_limit_mw(&self, h: Handle) -> Result<u32, String> {
        let mut v = 0u32;
        call!(self, "nvmlDeviceGetPowerManagementLimit", unsafe extern "C" fn(Handle, *mut u32) -> i32, h, &mut v);
        Ok(v)
    }

    fn power_limit_constraints_mw(&self, h: Handle) -> Result<(u32, u32), String> {
        let (mut lo, mut hi) = (0u32, 0u32);
        call!(
            self,
            "nvmlDeviceGetPowerManagementLimitConstraints",
            unsafe extern "C" fn(Handle, *mut u32, *mut u32) -> i32,
            h,
            &mut lo,
            &mut hi
        );
        Ok((lo, hi))
    }

    fn set_power_limit_mw(&self, h: Handle, mw: u32) -> Result<(), String> {
        call!(self, "nvmlDeviceSetPowerManagementLimit", unsafe extern "C" fn(Handle, u32) -> i32, h, mw);
        Ok(())
    }

    fn set_locked_clocks(&self, h: Handle, min: u32, max: u32) -> Result<(), String> {
        call!(self, "nvmlDeviceSetGpuLockedClocks", unsafe extern "C" fn(Handle, u32, u32) -> i32, h, min, max);
        Ok(())
    }

    fn reset_locked_clocks(&self, h: Handle) -> Result<(), String> {
        call!(self, "nvmlDeviceResetGpuLockedClocks", unsafe extern "C" fn(Handle) -> i32, h);
        Ok(())
    }

    /// NVML's two-step list protocol: ask for the size, then for the values. Empty on any
    /// failure; GeForce drivers often answer NOT_SUPPORTED, which is why the ladder exists.
    fn enumerate(&self, name: &[u8], h: Handle, extra: Option<u32>) -> Vec<u32> {
        unsafe {
            let mut count = 0u32;
            let first;
            let mut buf;
            match extra {
                None => {
                    let Ok(f) = self.lib.get::<unsafe extern "C" fn(Handle, *mut u32, *mut u32) -> i32>(name) else {
                        return vec![];
                    };
                    first = f(h, &mut count, std::ptr::null_mut());
                    if first != NVML_SUCCESS && first != NVML_ERROR_INSUFFICIENT_SIZE {
                        return vec![];
                    }
                    count = count.min(NVML_MAX_CLOCK_ENTRIES);
                    buf = vec![0u32; count as usize];
                    if count == 0 || f(h, &mut count, buf.as_mut_ptr()) != NVML_SUCCESS {
                        return vec![];
                    }
                }
                Some(mem) => {
                    let Ok(f) = self.lib.get::<unsafe extern "C" fn(Handle, u32, *mut u32, *mut u32) -> i32>(name)
                    else {
                        return vec![];
                    };
                    first = f(h, mem, &mut count, std::ptr::null_mut());
                    if first != NVML_SUCCESS && first != NVML_ERROR_INSUFFICIENT_SIZE {
                        return vec![];
                    }
                    count = count.min(NVML_MAX_CLOCK_ENTRIES);
                    buf = vec![0u32; count as usize];
                    if count == 0 || f(h, mem, &mut count, buf.as_mut_ptr()) != NVML_SUCCESS {
                        return vec![];
                    }
                }
            }
            buf.truncate(count as usize);
            buf
        }
    }

    /// Lowest graphics clock the driver admits to supporting.
    fn min_supported_graphics_clock_mhz(&self, h: Handle) -> Option<u32> {
        let mem = self.enumerate(b"nvmlDeviceGetSupportedMemoryClocks\0", h, None);
        let (first, last) = (*mem.first()?, *mem.last()?);
        [first, last]
            .iter()
            .filter_map(|&m| self.enumerate(b"nvmlDeviceGetSupportedGraphicsClocks\0", h, Some(m)).into_iter().min())
            .min()
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if let Ok(f) = unsafe { self.lib.get::<unsafe extern "C" fn() -> i32>(b"nvmlShutdown\0") } {
            unsafe {
                f();
            }
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct Marker {
    #[serde(default)]
    engaged_at: f64,
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    subsystem_id: Option<u32>,
    #[serde(default)]
    saved_power_limit_mw: Option<u32>,
    #[serde(default)]
    applied_power_limit: bool,
    #[serde(default)]
    applied_clocks: bool,
}

/// Applies and removes the emergency slowdown.
///
/// connect() never fails loudly: no driver, no rights or no matching card are all normal
/// "unavailable" answers, and the guard falls back to shutdown-only protection. engage()
/// and release() are idempotent.
pub struct GpuMitigator {
    nvml: Option<Nvml>,
    handle: Handle,
    saved_power_limit_mw: Option<u32>,
    applied_power: bool,
    applied_clocks: bool,
    locked_at: Option<u32>,
    pub gpu_name: String,
    pub subsystem_id: Option<u32>,
    pub power_limit_min_mw: u32,
    pub power_limit_max_mw: u32,
    pub clock_floor_mhz: Option<u32>,
    pub engaged: bool,
    pub engaged_reason: String,
    /// None when mitigation can run.
    pub unavailable_reason: Option<String>,
}

// The NVML handle is only used from the monitor thread.
unsafe impl Send for GpuMitigator {}

impl Default for GpuMitigator {
    fn default() -> Self {
        GpuMitigator {
            nvml: None,
            handle: std::ptr::null_mut(),
            saved_power_limit_mw: None,
            applied_power: false,
            applied_clocks: false,
            locked_at: None,
            gpu_name: String::new(),
            subsystem_id: None,
            power_limit_min_mw: 0,
            power_limit_max_mw: 0,
            clock_floor_mhz: None,
            engaged: false,
            engaged_reason: String::new(),
            unavailable_reason: Some("not connected".into()),
        }
    }
}

fn say_warn(msg: &str) {
    logger::warn(format!("Mitigation: {msg}"));
}

impl GpuMitigator {
    pub fn is_available(&self) -> bool {
        self.nvml.is_some() && self.unavailable_reason.is_none()
    }

    /// Bind to the GPU whose pins we are watching. True when mitigation can run.
    pub fn connect(&mut self, subsystem_id: Option<u32>) -> bool {
        if self.is_available() {
            return true;
        }
        if !crate::platform::is_elevated() {
            self.unavailable_reason = Some("not elevated".into());
            say_warn(
                "unavailable (guard is not running elevated). Protection falls back to \
                 shutdown-only. Install as a scheduled task/service, or run as admin/root.",
            );
            return false;
        }
        let nvml = match Nvml::init() {
            Ok(n) => n,
            Err(e) => {
                self.unavailable_reason = Some(format!("NVML unavailable ({e})"));
                say_warn(&format!("unavailable ({e}). Protection falls back to shutdown-only."));
                return false;
            }
        };
        let result = (|| -> Result<Option<Handle>, String> {
            let Some(h) = self.match_gpu(&nvml, subsystem_id)? else { return Ok(None) };
            self.gpu_name = nvml.name(h).unwrap_or_else(|_| "NVIDIA GPU".into());
            let (lo, hi) = nvml.power_limit_constraints_mw(h)?;
            self.power_limit_min_mw = lo;
            self.power_limit_max_mw = hi;
            self.clock_floor_mhz = nvml.min_supported_graphics_clock_mhz(h);
            Ok(Some(h))
        })();
        match result {
            Ok(Some(h)) => {
                self.nvml = Some(nvml);
                self.handle = h;
                self.unavailable_reason = None;
                let floor = self.clock_floor_mhz.unwrap_or(CLOCK_FLOOR_LADDER[0]);
                logger::info(format!(
                    "Mitigation: ready on {} - power limit floor {:.0}W (of {:.0}W max), clock lock target {} MHz{}",
                    self.gpu_name,
                    self.power_limit_min_mw as f64 / 1000.0,
                    self.power_limit_max_mw as f64 / 1000.0,
                    floor,
                    if self.clock_floor_mhz.is_some() { "" } else { " (driver did not enumerate clocks)" }
                ));
                true
            }
            Ok(None) => {
                self.unavailable_reason = Some("no matching NVIDIA GPU".into());
                say_warn("no matching GPU found. Protection falls back to shutdown-only.");
                false
            }
            Err(e) => {
                self.unavailable_reason = Some(format!("NVML query failed ({e})"));
                say_warn(&format!("unavailable ({e}). Protection falls back to shutdown-only."));
                false
            }
        }
    }

    /// The NVML handle of the card the sensor is reading: by subsystem id when known,
    /// else a known Astral id, else the only GPU. Several GPUs and no identification is
    /// the one case where we refuse to guess.
    fn match_gpu(&mut self, nvml: &Nvml, subsystem_id: Option<u32>) -> Result<Option<Handle>, String> {
        let count = nvml.device_count()?;
        let handles: Vec<(Handle, u32)> = (0..count)
            .filter_map(|i| {
                let h = nvml.handle(i).ok()?;
                Some((h, nvml.subsystem_id(h).ok()?))
            })
            .collect();
        let pick = subsystem_id
            .and_then(|want| handles.iter().find(|(_, s)| *s == want))
            .or_else(|| handles.iter().find(|(_, s)| crate::sensor::model_for(*s).is_some()))
            .or(if handles.len() == 1 { handles.first() } else { None });
        Ok(pick.map(|&(h, s)| {
            self.subsystem_id = Some(s);
            h
        }))
    }

    /// Lock clocks to the floor and drop the power limit. Idempotent. True if the GPU is
    /// now limited; a partial result counts and is logged, because the ladder measures
    /// the outcome in amps and escalates if the current does not fall.
    pub fn engage(&mut self, reason: &str) -> bool {
        if self.engaged {
            return true;
        }
        if !self.is_available() {
            return false;
        }
        let nvml = self.nvml.as_ref().unwrap();
        // Captured before anything changes: release() must restore the user's own limit.
        self.saved_power_limit_mw = match nvml.power_limit_mw(self.handle) {
            Ok(v) => Some(v),
            Err(e) => {
                say_warn(&format!("could not read current power limit ({e})"));
                None
            }
        };
        // Clocks first: that is what actually moves current.
        let clocks = self.lock_clocks();
        let power = self.drop_power_limit();
        if !clocks && !power {
            logger::error("Mitigation: engage failed, nothing applied - shutdown remains the only protection");
            return false;
        }
        self.applied_clocks = clocks;
        self.applied_power = power;
        self.engaged = true;
        self.engaged_reason = reason.to_string();
        self.write_marker();
        logger::warn(format!(
            "Mitigation: ENGAGED ({reason}) - clocks {}, power limit {}",
            match (clocks, self.locked_at) {
                (true, Some(m)) => format!("locked to {m} MHz"),
                _ => "NOT locked".into(),
            },
            if power { format!("set to {}W", self.power_limit_min_mw / 1000) } else { "unchanged".into() }
        ));
        true
    }

    fn lock_clocks(&mut self) -> bool {
        self.locked_at = None;
        let nvml = self.nvml.as_ref().unwrap();
        let mut tried = Vec::new();
        for mhz in self.clock_floor_mhz.into_iter().chain(CLOCK_FLOOR_LADDER) {
            if tried.contains(&mhz) {
                continue;
            }
            tried.push(mhz);
            match nvml.set_locked_clocks(self.handle, mhz, mhz) {
                Ok(()) => {
                    self.locked_at = Some(mhz);
                    return true;
                }
                Err(e) => say_warn(&format!("clock lock at {mhz} MHz refused ({e}), trying higher")),
            }
        }
        logger::error("Mitigation: could not lock clocks at any frequency");
        false
    }

    fn drop_power_limit(&mut self) -> bool {
        if self.power_limit_min_mw == 0 {
            return false;
        }
        match self.nvml.as_ref().unwrap().set_power_limit_mw(self.handle, self.power_limit_min_mw) {
            Ok(()) => true,
            Err(e) => {
                say_warn(&format!("power limit change refused ({e})"));
                false
            }
        }
    }

    /// Undo exactly what engage() changed. Idempotent and safe when not engaged.
    pub fn release(&mut self) -> bool {
        if !self.engaged {
            clear_marker();
            return true;
        }
        let mut restored = true;
        if let Some(nvml) = self.nvml.as_ref() {
            if self.applied_power {
                match self.saved_power_limit_mw {
                    Some(saved) if saved > 0 => {
                        if let Err(e) = nvml.set_power_limit_mw(self.handle, saved) {
                            restored = false;
                            logger::error(format!("Mitigation: could not restore power limit ({e})"));
                        }
                    }
                    // Never invent a value: the floor left in place is visible and fixable.
                    _ => {
                        restored = false;
                        logger::error(
                            "Mitigation: no saved power limit to restore - the card may still be at its minimum limit",
                        );
                    }
                }
            }
            if self.applied_clocks {
                if let Err(e) = nvml.reset_locked_clocks(self.handle) {
                    restored = false;
                    logger::error(format!("Mitigation: could not reset locked clocks ({e})"));
                }
            }
        }
        self.engaged = false;
        self.engaged_reason.clear();
        self.applied_power = false;
        self.applied_clocks = false;
        clear_marker();
        if restored {
            logger::info("Mitigation: released - clocks unlocked and power limit restored");
        }
        restored
    }

    /// Undo a slowdown left behind by a guard that died while engaged. Only touches NVML
    /// when the marker exists. Returns a message if it acted.
    pub fn startup_recovery(&mut self) -> Option<String> {
        let marker = read_marker()?;
        if !self.connect(marker.subsystem_id) {
            clear_marker();
            return Some(format!(
                "Found a leftover mitigation marker but cannot reach the GPU ({}). If the card is \
                 still limited, reboot or reset it with: nvidia-smi -rgc",
                self.unavailable_reason.clone().unwrap_or_default()
            ));
        }
        let nvml = self.nvml.as_ref().unwrap();
        let mut problems = Vec::new();
        if let Err(e) = nvml.reset_locked_clocks(self.handle) {
            problems.push(format!("clock reset failed ({e})"));
        }
        if marker.applied_power_limit {
            match marker.saved_power_limit_mw {
                Some(saved) if saved > 0 => {
                    if let Err(e) = nvml.set_power_limit_mw(self.handle, saved) {
                        problems.push(format!("power limit restore failed ({e})"));
                    }
                }
                _ => problems.push(
                    "no saved power limit in the marker; the card may still be at its minimum limit \
                     (fix with nvidia-smi -pl)"
                        .into(),
                ),
            }
        }
        clear_marker();
        self.engaged = false;
        self.applied_power = false;
        self.applied_clocks = false;
        let detail =
            if problems.is_empty() { "clocks and power limit restored".to_string() } else { problems.join("; ") };
        let reason = if marker.reason.is_empty() { "unknown".to_string() } else { marker.reason };
        Some(format!("Previous run ended while the GPU was limited ({reason}) - {detail}"))
    }

    fn write_marker(&self) {
        let m = Marker {
            engaged_at: crate::now(),
            pid: std::process::id(),
            reason: self.engaged_reason.clone(),
            subsystem_id: self.subsystem_id,
            saved_power_limit_mw: self.saved_power_limit_mw,
            applied_power_limit: self.applied_power,
            applied_clocks: self.applied_clocks,
        };
        let path = marker_path();
        let tmp = path.with_extension("active.tmp");
        let result = serde_json::to_vec(&m).map_err(|e| e.to_string()).and_then(|bytes| {
            use std::io::Write;
            let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&tmp, &path).map_err(|e| e.to_string())
        });
        if let Err(e) = result {
            // Not fatal: the marker only matters if we crash before releasing.
            say_warn(&format!("could not write recovery marker ({e})"));
        }
    }

    /// Release, then drop NVML. Never leaves the card limited.
    pub fn close(&mut self) {
        self.release();
        self.nvml = None;
        self.handle = std::ptr::null_mut();
        self.unavailable_reason = Some("not connected".into());
    }

    /// Close without releasing: the machine is about to power off and the card should
    /// stay limited until it does. The marker restores it on the next start.
    pub fn abandon(&mut self) {
        self.nvml = None;
        self.handle = std::ptr::null_mut();
    }
}

fn read_marker() -> Option<Marker> {
    let path = marker_path();
    if !path.exists() {
        return None;
    }
    // A corrupt marker still means "a previous run was limited".
    Some(fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

fn clear_marker() {
    let path = marker_path();
    let _ = fs::remove_file(path.with_extension("active.tmp"));
    let _ = fs::remove_file(path);
}
