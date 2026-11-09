//! The GPU as the Overclocker sees it: a trait, so the apply/restore logic is tested against
//! a fake (`overclock::fake`), and `RealGpu`, its NVML + NVAPI implementation.

use super::curve::RawPoint;
use crate::mitigation::{handle_for_subsystem, Handle, Nvml, NVML_CLOCK_GRAPHICS, NVML_CLOCK_MEM};
use crate::nvapi;

/// Every driver read and write the overclock needs. Units: watts, NVML memory MHz (twice the
/// Afterburner value), kHz for curve deltas.
pub trait OcGpu {
    fn subsystem(&self) -> u32;
    fn driver_version(&mut self) -> Result<String, String>;
    fn power_limit_w(&mut self) -> Result<u32, String>;
    fn power_range_w(&mut self) -> Result<(u32, u32), String>;
    fn set_power_limit_w(&mut self, w: u32) -> Result<(), String>;
    /// (current, min, max) memory clock offset, NVML MHz.
    fn mem_offset(&mut self) -> Result<(i32, i32, i32), String>;
    fn set_mem_offset(&mut self, nvml_mhz: i32) -> Result<(), String>;
    /// NVML's graphics clock offset (set by another tool; AstralGuard never writes it).
    fn core_offset_mhz(&mut self) -> Result<i32, String>;
    fn curve(&mut self) -> Result<Vec<RawPoint>, String>;
    fn set_curve(&mut self, deltas_khz: &[(usize, i32)]) -> Result<(), String>;
    fn core_voltage_mv(&mut self) -> Result<f64, String>;
}

/// The real card: its own NVML instance (never the mitigator's) and the process's one NVAPI
/// binding. Only the subsystem id is kept between calls; both handles are looked up again in
/// every call, because a sensor reconnect re-initializes NVAPI and may invalidate handles
/// (see `nvapi`'s module doc), and an NVML handle is cheap to find.
///
/// UNVERIFIED as a whole: exercised on hardware in a later, supervised step. The individual
/// NVML and NVAPI calls carry their own VERIFIED notes.
pub struct RealGpu {
    subsystem: u32,
    nvml: Nvml,
}

impl RealGpu {
    // Like the impl below: no test may construct one.
    #[cfg_attr(test, allow(dead_code))]
    pub fn open(subsystem: u32) -> Result<RealGpu, String> {
        let nvml = Nvml::init()?;
        handle_for_subsystem(&nvml, subsystem)?;
        Ok(RealGpu { subsystem, nvml })
    }

    fn nvml_handle(&self) -> Result<Handle, String> {
        handle_for_subsystem(&self.nvml, self.subsystem)
    }

    fn with_nvapi<R>(&self, f: impl FnOnce(&nvapi::Api, nvapi::GpuHandle) -> Result<R, String>) -> Result<R, String> {
        let subsystem = self.subsystem;
        nvapi::with(|api| {
            let gpu = api.gpu_by_subsystem(subsystem)?;
            f(api, gpu)
        })
    }
}

// No test may call into the real driver, so under `cargo test` nothing constructs a RealGpu.
#[cfg_attr(test, allow(dead_code))]
impl OcGpu for RealGpu {
    fn subsystem(&self) -> u32 {
        self.subsystem
    }

    fn driver_version(&mut self) -> Result<String, String> {
        self.nvml.driver_version()
    }

    fn power_limit_w(&mut self) -> Result<u32, String> {
        let h = self.nvml_handle()?;
        // NVML limits are whole watts in practice; round rather than truncate a stray mW.
        Ok((self.nvml.power_limit_mw(h)? + 500) / 1000)
    }

    fn power_range_w(&mut self) -> Result<(u32, u32), String> {
        let h = self.nvml_handle()?;
        let (lo, hi) = self.nvml.power_limit_constraints_mw(h)?;
        // Inward, so a watt value inside the range is always inside NVML's mW range.
        Ok((lo.div_ceil(1000), hi / 1000))
    }

    fn set_power_limit_w(&mut self, w: u32) -> Result<(), String> {
        let h = self.nvml_handle()?;
        self.nvml.set_power_limit_mw(h, w * 1000)
    }

    fn mem_offset(&mut self) -> Result<(i32, i32, i32), String> {
        let h = self.nvml_handle()?;
        self.nvml.get_clock_offset(h, NVML_CLOCK_MEM)
    }

    fn set_mem_offset(&mut self, nvml_mhz: i32) -> Result<(), String> {
        let h = self.nvml_handle()?;
        self.nvml.set_clock_offset(h, NVML_CLOCK_MEM, nvml_mhz)
    }

    fn core_offset_mhz(&mut self) -> Result<i32, String> {
        let h = self.nvml_handle()?;
        Ok(self.nvml.get_clock_offset(h, NVML_CLOCK_GRAPHICS)?.0)
    }

    fn curve(&mut self) -> Result<Vec<RawPoint>, String> {
        self.with_nvapi(nvapi::read_curve)
    }

    fn set_curve(&mut self, deltas_khz: &[(usize, i32)]) -> Result<(), String> {
        self.with_nvapi(|api, gpu| nvapi::write_deltas(api, gpu, deltas_khz))
    }

    fn core_voltage_mv(&mut self) -> Result<f64, String> {
        self.with_nvapi(nvapi::core_voltage_uv).map(|uv| uv as f64 / 1000.0)
    }
}
