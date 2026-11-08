//! The IT8915 through NVAPI's I2C read (Windows).
//!
//! Only NvAPI_I2CReadEx is bound. NvAPI_I2CWriteEx is 0x283AC65A, recorded so nobody
//! re-derives it: NEVER BIND IT.

use std::ffi::c_void;
use std::mem::{offset_of, size_of};

use libloading::Library;

use super::{model_for, CardInfo, I2cBus, IT8915_ADDRESS_7BIT};

const NVAPI_INITIALIZE: u32 = 0x0150E828;
const NVAPI_UNLOAD: u32 = 0xD22BDD7E;
const NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC921F;
const NVAPI_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2DDFB66E;
const NVAPI_GPU_GET_FULL_NAME: u32 = 0xCEEE8E9F;
const NVAPI_I2C_READ_EX: u32 = 0x4D7B0709;

const NVAPI_OK: i32 = 0;
const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;
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

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type FnVoid = unsafe extern "C" fn() -> i32;
type FnEnum = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type FnPci = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32, *mut u32, *mut u32) -> i32;
type FnName = unsafe extern "C" fn(*mut c_void, *mut u8) -> i32;
type FnI2cRead = unsafe extern "C" fn(*mut c_void, *mut NvI2cInfoV3, *mut u32) -> i32;

struct Nvapi {
    _lib: Library,
    unload: FnVoid,
    i2c_read: FnI2cRead,
    handle: *mut c_void,
}

// The GPU handle is an opaque token NVAPI hands out; all use stays on one thread at a time
// (the monitor thread owns the sensor).
unsafe impl Send for Nvapi {}

impl Drop for Nvapi {
    fn drop(&mut self) {
        unsafe {
            (self.unload)();
        }
    }
}

impl I2cBus for Nvapi {
    fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String> {
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
        let status = unsafe { (self.i2c_read)(self.handle, &mut info, &mut out) };
        if status != NVAPI_OK {
            return Err(format!("NvAPI_I2CReadEx status {status}"));
        }
        Ok(())
    }
}

unsafe fn bind<T: Copy>(query: QueryInterface, id: u32) -> Result<T, String> {
    let ptr = query(id);
    if ptr.is_null() {
        return Err(format!("nvapi_QueryInterface({id:#010x}) returned NULL"));
    }
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&ptr))
}

pub fn open() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    unsafe {
        let lib = Library::new("nvapi64.dll").map_err(|e| format!("nvapi64.dll not loadable ({e})"))?;
        let query: QueryInterface = *lib
            .get::<QueryInterface>(b"nvapi_QueryInterface\0")
            .map_err(|e| format!("nvapi_QueryInterface missing ({e})"))?;
        let init: FnVoid = bind(query, NVAPI_INITIALIZE)?;
        let unload: FnVoid = bind(query, NVAPI_UNLOAD)?;
        let enum_gpus: FnEnum = bind(query, NVAPI_ENUM_PHYSICAL_GPUS)?;
        let pci: FnPci = bind(query, NVAPI_GPU_GET_PCI_IDENTIFIERS)?;
        let name: FnName = bind(query, NVAPI_GPU_GET_FULL_NAME)?;
        let i2c_read: FnI2cRead = bind(query, NVAPI_I2C_READ_EX)?;

        if init() != NVAPI_OK {
            return Err("NvAPI_Initialize failed".into());
        }
        // From here on, dropping `api` unloads NVAPI.
        let mut api = Nvapi { _lib: lib, unload, i2c_read, handle: std::ptr::null_mut() };

        let mut handles = [std::ptr::null_mut::<c_void>(); NVAPI_MAX_PHYSICAL_GPUS];
        let mut count = 0u32;
        if enum_gpus(handles.as_mut_ptr(), &mut count) != NVAPI_OK {
            return Err("NvAPI_EnumPhysicalGPUs failed".into());
        }
        for &handle in handles.iter().take(count as usize) {
            let (mut dev, mut subsys, mut rev, mut ext) = (0u32, 0u32, 0u32, 0u32);
            if pci(handle, &mut dev, &mut subsys, &mut rev, &mut ext) != NVAPI_OK {
                continue;
            }
            let Some(model) = model_for(subsys) else { continue };
            let mut buf = [0u8; NVAPI_SHORT_STRING_MAX];
            let gpu_name = if name(handle, buf.as_mut_ptr()) == NVAPI_OK {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).into_owned()
            } else {
                "NVIDIA GPU".into()
            };
            api.handle = handle;
            return Ok((Box::new(api), CardInfo { gpu_name, subsystem: subsys, model }));
        }
        Err("no supported ASUS ROG Astral GPU found".into())
    }
}
