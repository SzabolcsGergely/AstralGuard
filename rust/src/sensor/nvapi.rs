//! The IT8915 through NVAPI's I2C read (Windows), on the process-wide NVAPI binding
//! (`crate::nvapi`): it loads the library once, never unloads it, and runs every call, these
//! reads included, under one lock. `open` (every connect and reconnect) goes through
//! `with_reinit`, so each connect gets a fresh NVAPI Initialize, e.g. after a driver reset.
//!
//! Only NvAPI_I2CReadEx is bound. NvAPI_I2CWriteEx is 0x283AC65A, recorded so nobody
//! re-derives it: NEVER BIND IT.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};

use super::{model_for, CardInfo, I2cBus, IT8915_ADDRESS_7BIT};
use crate::nvapi::GpuHandle;

const NVAPI_GPU_GET_FULL_NAME: u32 = 0xCEEE8E9F;
const NVAPI_I2C_READ_EX: u32 = 0x4D7B0709;

const NVAPI_OK: i32 = 0;
const NVAPI_SHORT_STRING_MAX: usize = 64;

const I2C_PORT_ID: u8 = 1; // NOT 4
const I2C_SPEED_DEPRECATED: u32 = 0xFFFF;
const I2C_SPEED_100KHZ: u32 = 4;

/// NV_I2C_INFO_V3, 64 bytes on x64. Field order and padding verified against both
/// witnesses; the compile-time checks below refuse to build a wrong layout, which would
/// put a malformed request on the bus.
#[repr(C)]
struct NvI2cInfoV3 {
    version: u32,         // 0
    display_mask: u32,    // 4
    is_ddc_port: u8,      // 8
    i2c_dev_address: u8,  // 9
    reg_address: *mut u8, // 16
    reg_addr_size: u32,   // 24
    data: *mut u8,        // 32
    size: u32,            // 40
    i2c_speed: u32,       // 44
    i2c_speed_khz: u32,   // 48
    port_id: u8,          // 52
    is_port_id_set: u32,  // 56
}

const _: () = {
    assert!(size_of::<NvI2cInfoV3>() == 64);
    assert!(offset_of!(NvI2cInfoV3, is_ddc_port) == 8);
    assert!(offset_of!(NvI2cInfoV3, i2c_dev_address) == 9);
    assert!(offset_of!(NvI2cInfoV3, reg_address) == 16);
    assert!(offset_of!(NvI2cInfoV3, reg_addr_size) == 24);
    assert!(offset_of!(NvI2cInfoV3, data) == 32);
    assert!(offset_of!(NvI2cInfoV3, size) == 40);
    assert!(offset_of!(NvI2cInfoV3, i2c_speed) == 44);
    assert!(offset_of!(NvI2cInfoV3, i2c_speed_khz) == 48);
    assert!(offset_of!(NvI2cInfoV3, port_id) == 52);
    assert!(offset_of!(NvI2cInfoV3, is_port_id_set) == 56);
};

const NV_I2C_INFO_VER3: u32 = (3 << 16) | 64;

type FnName = unsafe extern "C" fn(*mut c_void, *mut u8) -> i32;
type FnI2cRead = unsafe extern "C" fn(*mut c_void, *mut NvI2cInfoV3, *mut u32) -> i32;

/// The bound read and the card's GPU handle. Holds no library: the shared binding keeps
/// NVAPI loaded for the life of the process, so dropping this unloads nothing.
struct Nvapi {
    i2c_read: FnI2cRead,
    gpu: GpuHandle,
}

impl I2cBus for Nvapi {
    fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String> {
        let (i2c_read, gpu) = (self.i2c_read, self.gpu);
        crate::nvapi::with(|_| {
            let mut reg = reg;
            let mut info = NvI2cInfoV3 {
                version: NV_I2C_INFO_VER3,
                display_mask: 0,
                is_ddc_port: 0,
                i2c_dev_address: IT8915_ADDRESS_7BIT << 1,
                reg_address: &mut reg,
                reg_addr_size: 1,
                data: buf.as_mut_ptr(),
                size: buf.len() as u32,
                i2c_speed: I2C_SPEED_DEPRECATED,
                i2c_speed_khz: I2C_SPEED_100KHZ,
                port_id: I2C_PORT_ID,
                is_port_id_set: 1,
            };
            let mut out = 0u32;
            let status = unsafe { i2c_read(gpu.0, &mut info, &mut out) };
            if status != NVAPI_OK {
                return Err(format!("NvAPI_I2CReadEx status {status}"));
            }
            Ok(())
        })
    }
}

pub fn open() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    crate::nvapi::with_reinit(|api| unsafe {
        let name: FnName = api.query(NVAPI_GPU_GET_FULL_NAME)?;
        let i2c_read: FnI2cRead = api.query(NVAPI_I2C_READ_EX)?;
        for (gpu, subsys) in api.gpus()? {
            let Some(model) = model_for(subsys) else { continue };
            let mut buf = [0u8; NVAPI_SHORT_STRING_MAX];
            let gpu_name = if name(gpu.0, buf.as_mut_ptr()) == NVAPI_OK {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).into_owned()
            } else {
                "NVIDIA GPU".into()
            };
            let bus: Box<dyn I2cBus> = Box::new(Nvapi { i2c_read, gpu });
            return Ok((bus, CardInfo { gpu_name, subsystem: subsys, model }));
        }
        Err("no supported ASUS ROG Astral GPU found".into())
    })
}
