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
    /// What the card draws right now, W. A read for the Overclock tab; nothing decides on it.
    fn power_draw_w(&mut self) -> Result<u32, String>;
    fn set_power_limit_w(&mut self, w: u32) -> Result<(), String>;
    /// (current, min, max) memory clock offset, NVML MHz.
    fn mem_offset(&mut self) -> Result<(i32, i32, i32), String>;
    fn set_mem_offset(&mut self, nvml_mhz: i32) -> Result<(), String>;
    /// The memory clock at full load (P0 max), MHz, with the offset included.
    fn mem_clock_mhz(&mut self) -> Result<u32, String>;
    /// NVML's graphics clock offset (set by another tool; AstralGuard never writes it).
    fn core_offset_mhz(&mut self) -> Result<i32, String>;
    fn curve(&mut self) -> Result<Vec<RawPoint>, String>;
    fn set_curve(&mut self, deltas_khz: &[(usize, i32)]) -> Result<(), String>;
    fn core_voltage_mv(&mut self) -> Result<f64, String>;
    /// (current, min, max) offset of the XBAR or SYS clock domain, kHz.
    fn clock_offset_khz(&mut self, d: ClkDomain) -> Result<(i32, i32, i32), String>;
    fn set_clock_offset_khz(&mut self, d: ClkDomain, khz: i32) -> Result<(), String>;
    /// What the card runs now. Reads only; no NVAPI.
    fn live(&mut self) -> crate::protocol::OcLive;
}

/// The clock domains besides core and memory that the overclock can offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkDomain {
    /// The crossbar between the GPCs and the memory partitions.
    Xbar,
    Sys,
}

impl ClkDomain {
    pub fn name(self) -> &'static str {
        match self {
            ClkDomain::Xbar => "XBAR",
            ClkDomain::Sys => "SYS",
        }
    }
}

/// The real card: its own NVML instance (never the mitigator's) and the process's one NVAPI
/// binding. Only the subsystem id is kept between calls; both handles are looked up again in
/// every call, because a sensor reconnect re-initializes NVAPI and may invalidate handles
/// (see `nvapi`'s module doc), and an NVML handle is cheap to find.
///
/// VERIFIED as a whole (2026-11-10, ROG Astral RTX 5090 OC, driver 621.14, Windows): apply and
/// untick of all three settings, back exactly at stock. The individual NVML and NVAPI calls
/// carry their own notes, Linux included.
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
    fn live(&mut self) -> crate::protocol::OcLive {
        let mut l = crate::protocol::OcLive::default();
        if let Ok(h) = self.nvml_handle() {
            l.power_draw_w = self.nvml.power_usage_mw(h).ok().map(|mw| (mw + 500) / 1000);
            l.power_limit_w = self.nvml.power_limit_mw(h).ok().map(|mw| (mw + 500) / 1000);
            l.core_mhz = self.nvml.clock_mhz(h, NVML_CLOCK_GRAPHICS).ok();
            l.mem_mhz = self.nvml.clock_mhz(h, NVML_CLOCK_MEM).ok();
            l.temp_c = self.nvml.temperature_c(h).ok();
        }
        let measure = |d| super::clkctl::measure_khz(self.subsystem, d).ok().map(|k| k / 1000);
        l.xbar_mhz = measure(ClkDomain::Xbar);
        l.sys_mhz = measure(ClkDomain::Sys);
        l
    }

    fn clock_offset_khz(&mut self, d: ClkDomain) -> Result<(i32, i32, i32), String> {
        super::clkctl::offset_khz(self.subsystem, d)
    }

    fn set_clock_offset_khz(&mut self, d: ClkDomain, khz: i32) -> Result<(), String> {
        let driver = self.nvml.driver_version()?;
        super::clkctl::set_offset_khz(self.subsystem, &driver, d, khz)
    }

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

    // VERIFIED (2026-11-10, ROG Astral RTX 5090 OC, driver 621.14, Windows):
    // nvmlDeviceGetPowerUsage gives the figure nvidia-smi shows as power.draw (83.08 W idle).
    fn power_draw_w(&mut self) -> Result<u32, String> {
        let h = self.nvml_handle()?;
        Ok((self.nvml.power_usage_mw(h)? + 500) / 1000)
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

    /// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux): a memory offset set
    /// on P0 reads back on every P-state (P8 included); +4000 NVML raised the P0 memory clock
    /// 14001 -> 16001 MHz (= +2000 in Afterburner units).
    fn set_mem_offset(&mut self, nvml_mhz: i32) -> Result<(), String> {
        let h = self.nvml_handle()?;
        self.nvml.set_clock_offset(h, NVML_CLOCK_MEM, nvml_mhz)
    }

    fn mem_clock_mhz(&mut self) -> Result<u32, String> {
        let h = self.nvml_handle()?;
        self.nvml.p0_max_mem_clock_mhz(h)
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
