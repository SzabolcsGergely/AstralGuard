//! The IT8915 through the Linux kernel's i2c-dev interface.
//!
//! The NVIDIA driver registers the GPU's I2C ports as adapters named
//! "NVIDIA i2c adapter <n> at <bus>:<dev>.<fn>". NVAPI's port 1 is expected to be adapter
//! number 1 of the Astral's PCI device. UNVERIFIED ON HARDWARE: until it has been
//! confirmed, only that one adapter is used (never probe other buses: even the register
//! pointer write must not go to an unknown device). ASTRALGUARD_I2C_BUS=<n> selects
//! /dev/i2c-<n> explicitly. Needs root (or the i2c group) and the i2c-dev module.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

use super::{model_for, CardInfo, I2cBus, IT8915_ADDRESS_7BIT};

const I2C_RDWR: libc::c_ulong = 0x0707;
const I2C_M_RD: u16 = 0x0001;
const PORT_ID: u32 = 1;

#[repr(C)]
struct I2cMsg {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
}

#[repr(C)]
struct I2cRdwrData {
    msgs: *mut I2cMsg,
    nmsgs: u32,
}

struct I2cDev {
    file: File,
}

impl I2cBus for I2cDev {
    fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String> {
        let mut reg = reg;
        let mut msgs = [
            I2cMsg { addr: IT8915_ADDRESS_7BIT as u16, flags: 0, len: 1, buf: &mut reg },
            I2cMsg { addr: IT8915_ADDRESS_7BIT as u16, flags: I2C_M_RD, len: buf.len() as u16, buf: buf.as_mut_ptr() },
        ];
        let mut data = I2cRdwrData { msgs: msgs.as_mut_ptr(), nmsgs: 2 };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_RDWR as _, &mut data) };
        if rc < 0 {
            return Err(format!("I2C_RDWR failed: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

fn read_hex(path: &Path) -> Option<u32> {
    let text = fs::read_to_string(path).ok()?;
    u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

/// The PCI subsystem id in NVAPI's DWORD format: device << 16 | vendor.
fn subsystem_of(pci_dev: &Path) -> Option<u32> {
    let vendor = read_hex(&pci_dev.join("subsystem_vendor"))?;
    let device = read_hex(&pci_dev.join("subsystem_device"))?;
    Some(device << 16 | vendor)
}

pub fn open() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    let forced: Option<u32> = std::env::var("ASTRALGUARD_I2C_BUS").ok().and_then(|v| v.parse().ok());
    let entries = fs::read_dir("/sys/bus/i2c/devices")
        .map_err(|e| format!("no I2C adapters visible ({e}); is the i2c-dev module loaded?"))?;
    let mut found_card = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(num) = name.strip_prefix("i2c-").and_then(|n| n.parse::<u32>().ok()) else { continue };
        let dir = entry.path();
        let adapter_name = fs::read_to_string(dir.join("name")).unwrap_or_default();
        let Some(sub) = subsystem_of(&dir.join("device")) else { continue };
        let Some(model) = model_for(sub) else { continue };
        found_card = Some((sub, model));
        let wanted = match forced {
            Some(n) => n == num,
            None => adapter_name.trim().starts_with(&format!("NVIDIA i2c adapter {PORT_ID} ")),
        };
        if !wanted {
            continue;
        }
        let dev = format!("/dev/i2c-{num}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev)
            .map_err(|e| format!("cannot open {dev} ({e}); run as root and load i2c-dev"))?;
        let gpu_name = fs::read_to_string(dir.join("device/label"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "NVIDIA GPU".into());
        return Ok((Box::new(I2cDev { file }), CardInfo { gpu_name, subsystem: sub, model }));
    }
    Err(match found_card {
        Some((sub, model)) => {
            format!("found {model} (0x{sub:08X}) but not its I2C port {PORT_ID}; set ASTRALGUARD_I2C_BUS")
        }
        None => "no supported ASUS ROG Astral GPU found on any I2C adapter".into(),
    })
}
