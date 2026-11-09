//! The IT8915 through the Linux kernel's i2c-dev interface.
//!
//! The NVIDIA driver registers the GPU's I2C ports as adapters named
//! "NVIDIA i2c adapter <n> at <bus>:<dev>.<fn>". VERIFIED (2026-11-09, ROG Astral RTX
//! 5090 OC, driver 620.70): adapter 1 carries the IT8915, like NVAPI's port 1, but only
//! SMBus block reads return its data (see `read`).
//!
//! The adapter is chosen, in order:
//! 1. ASTRALGUARD_I2C_BUS=<n> (/dev/i2c-<n>), an explicit override;
//! 2. the adapter detection (`probe --detect` or the dashboard's Scan) verified and saved
//!    in config.ini, used only while the same card (subsystem id) is on the same PCI address;
//! 3. adapter 1. Verified on one card; other cards and drivers may differ, which is what
//!    detection is for.
//!
//! Only one adapter is ever used here; nothing in this file scans buses. Detection
//! (sensor/detect.rs) is a separate, user-run command. Needs root (or the i2c group) and
//! the i2c-dev module.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use super::{model_for, CardInfo, I2cBus, IT8915_ADDRESS_7BIT};

const PORT_ID: u32 = 1;
const I2C_SLAVE: libc::c_ulong = 0x0703;
const I2C_SMBUS: libc::c_ulong = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_I2C_BLOCK_DATA: u32 = 8;
const I2C_SMBUS_BYTE: u32 = 1;
const I2C_FUNCS: libc::c_ulong = 0x0705;
const I2C_FUNC_SMBUS_READ_BYTE: libc::c_ulong = 0x0002_0000;
const I2C_SMBUS_BLOCK_MAX: usize = 32;

/// struct i2c_smbus_ioctl_data; `data` points at a union i2c_smbus_data (34 bytes:
/// block[0] is the length, then up to 32 data bytes).
#[repr(C)]
struct I2cSmbusIoctlData {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut u8,
}

pub(super) struct I2cDev {
    file: File,
    /// I2C_SLAVE 0x2B done (needed before SMBus calls)
    addressed: bool,
}

impl I2cDev {
    pub(super) fn open(num: u32) -> Result<I2cDev, String> {
        let dev = format!("/dev/i2c-{num}");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev)
            .map_err(|e| format!("cannot open {dev} ({e}); run as root and load i2c-dev"))?;
        Ok(I2cDev { file, addressed: false })
    }

    /// Is anything acknowledging 0x2B on this bus? One SMBus "receive byte" (start, address
    /// with the read bit, one byte, stop), with no register pointer write before it: the least
    /// a transaction can do, so it is the only thing detection sends to an adapter before that
    /// adapter has shown it is the IT8915. Through I2C_SMBUS, not I2C_RDWR: this driver's
    /// I2C_RDWR reports success with zero data (see `read`), so it cannot be trusted to report
    /// a NACK either. An adapter that does not offer the receive byte is not tried at all.
    /// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux): the NVIDIA driver
    /// offers the SMBus receive byte; the scan found the IT8915 on adapter 1 and reported
    /// "nothing at 0x2B" (an error for the NACK) on five of the other six.
    pub(super) fn answers(&mut self) -> bool {
        let mut funcs: libc::c_ulong = 0;
        if unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_FUNCS as _, &mut funcs) } < 0
            || funcs & I2C_FUNC_SMBUS_READ_BYTE == 0
        {
            return false;
        }
        if self.address().is_err() {
            return false;
        }
        let mut data = [0u8; I2C_SMBUS_BLOCK_MAX + 2];
        let mut args =
            I2cSmbusIoctlData { read_write: I2C_SMBUS_READ, command: 0, size: I2C_SMBUS_BYTE, data: data.as_mut_ptr() };
        unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_SMBUS as _, &mut args) >= 0 }
    }

    /// I2C_SLAVE 0x2B, once per open: the SMBus calls go to that address.
    fn address(&mut self) -> Result<(), String> {
        if !self.addressed {
            let rc =
                unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_SLAVE as _, IT8915_ADDRESS_7BIT as libc::c_ulong) };
            if rc < 0 {
                return Err(format!("I2C_SLAVE 0x2B failed: {}", std::io::Error::last_os_error()));
            }
            self.addressed = true;
        }
        Ok(())
    }
}

/// The SMBus "I2C block read": register pointer write, repeated start, read N bytes. The
/// same transaction on the wire as a two-message I2C_RDWR, but through the driver's SMBus
/// path. VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70): via I2C_RDWR the
/// NVIDIA driver returns success with all-zero data on adapter 1; via I2C_SMBUS the same
/// read returns 12.104 V / 0.920 A per pin and 1.00 g. Never a write command: read_write
/// is always I2C_SMBUS_READ.
impl I2cBus for I2cDev {
    fn read(&mut self, reg: u8, buf: &mut [u8]) -> Result<(), String> {
        if buf.len() > I2C_SMBUS_BLOCK_MAX {
            return Err(format!("{} bytes is more than one SMBus block", buf.len()));
        }
        self.address()?;
        let mut data = [0u8; I2C_SMBUS_BLOCK_MAX + 2];
        data[0] = buf.len() as u8;
        let mut args = I2cSmbusIoctlData {
            read_write: I2C_SMBUS_READ,
            command: reg,
            size: I2C_SMBUS_I2C_BLOCK_DATA,
            data: data.as_mut_ptr(),
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), I2C_SMBUS as _, &mut args) };
        if rc < 0 {
            return Err(format!("I2C_SMBUS block read failed: {}", std::io::Error::last_os_error()));
        }
        let got = data[0] as usize;
        if got != buf.len() {
            return Err(format!("short SMBus block read: {got} of {} bytes", buf.len()));
        }
        buf.copy_from_slice(&data[1..=got]);
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

/// The PCI device an I2C adapter belongs to. Older drivers give the adapter a `device` link;
/// the NVIDIA driver registers it as a child of the PCI device instead, with no link.
/// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, kernel 7.3): `.../0000:01:00.0/i2c-9`
/// has no `device` entry, its parent is the GPU.
fn pci_device_of(adapter: &Path) -> Option<PathBuf> {
    let link = adapter.join("device");
    if link.exists() {
        return Some(link);
    }
    fs::canonicalize(adapter).ok()?.parent().map(Path::to_path_buf)
}

/// A subsystem id is unique only together with the chip's own vendor: an ASUS motherboard's
/// SMBus controller also carries subsystem vendor 0x1043, and its bus must never be touched.
/// So the PCI device must be NVIDIA's (0x10DE) and the adapter one the NVIDIA driver made.
fn is_nvidia_gpu_adapter(pci_vendor: Option<u32>, adapter_name: &str) -> bool {
    pci_vendor == Some(0x10DE) && adapter_name.starts_with("NVIDIA i2c adapter")
}

/// An I2C adapter that belongs to a supported Astral card.
pub(super) struct Adapter {
    /// N in /dev/i2c-N
    pub num: u32,
    /// The driver's name, e.g. "NVIDIA i2c adapter 3 at 1:00.0" (includes the PCI address)
    pub name: String,
    pub subsystem: u32,
    pub model: &'static str,
    pub gpu_name: String,
}

/// Every adapter of every supported card, by adapter number. Reads sysfs only.
pub(super) fn adapters() -> Result<Vec<Adapter>, String> {
    let entries = fs::read_dir("/sys/bus/i2c/devices")
        .map_err(|e| format!("no I2C adapters visible ({e}); is the i2c-dev module loaded?"))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(num) = file_name.strip_prefix("i2c-").and_then(|n| n.parse::<u32>().ok()) else { continue };
        let dir = entry.path();
        let Some(pci_dev) = pci_device_of(&dir) else { continue };
        let Some(subsystem) = subsystem_of(&pci_dev) else { continue };
        let Some(model) = model_for(subsystem) else { continue };
        let name = fs::read_to_string(dir.join("name")).unwrap_or_default().trim().to_string();
        if !is_nvidia_gpu_adapter(read_hex(&pci_dev.join("vendor")), &name) {
            continue;
        }
        let gpu_name = fs::read_to_string(pci_dev.join("label"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "NVIDIA GPU".into());
        out.push(Adapter { num, name, subsystem, model, gpu_name });
    }
    out.sort_by_key(|a| a.num);
    Ok(out)
}

/// Which adapter to use, by the rules in the module doc. Pure, for the tests.
fn choose<'a>(
    adapters: &'a [Adapter],
    forced: Option<u32>,
    saved: Option<&crate::config::I2cChoice>,
) -> Result<&'a Adapter, String> {
    if let Some(n) = forced {
        return adapters.iter().find(|a| a.num == n).ok_or_else(|| {
            format!("ASTRALGUARD_I2C_BUS={n}: /dev/i2c-{n} is not an adapter of a supported Astral card")
        });
    }
    let first = adapters.first().ok_or("no supported ASUS ROG Astral GPU found on any I2C adapter")?;
    if let Some(saved) = saved.filter(|s| adapters.iter().any(|a| a.subsystem == s.subsystem)) {
        return adapters.iter().find(|a| a.subsystem == saved.subsystem && a.name == saved.adapter).ok_or_else(|| {
            format!(
                "the I2C adapter saved by `probe --detect` (\"{}\") is gone; run `sudo astralguard probe --detect` again",
                saved.adapter
            )
        });
    }
    let prefix = format!("NVIDIA i2c adapter {PORT_ID} ");
    adapters.iter().find(|a| a.name.starts_with(&prefix)).ok_or_else(|| {
        format!(
            "found {} (0x{:08X}) but not its I2C port {PORT_ID}; run `sudo astralguard probe --detect`",
            first.model, first.subsystem
        )
    })
}

pub fn open() -> Result<(Box<dyn I2cBus>, CardInfo), String> {
    let forced: Option<u32> = std::env::var("ASTRALGUARD_I2C_BUS").ok().and_then(|v| v.parse().ok());
    let saved = crate::config::load_i2c_choice(&crate::config::config_path());
    let all = adapters()?;
    let a = choose(&all, forced, saved.as_ref())?;
    let dev = I2cDev::open(a.num)?;
    Ok((Box::new(dev), CardInfo { gpu_name: a.gpu_name.clone(), subsystem: a.subsystem, model: a.model }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_nvidia_gpu_adapters_count() {
        assert!(is_nvidia_gpu_adapter(Some(0x10DE), "NVIDIA i2c adapter 1 at 1:00.0"));
        // An ASUS board's SMBus (Intel / AMD vendor), even with a matching subsystem id.
        assert!(!is_nvidia_gpu_adapter(Some(0x8086), "SMBus I801 adapter at efa0"));
        assert!(!is_nvidia_gpu_adapter(Some(0x1022), "NVIDIA i2c adapter 1 at 1:00.0"));
        // An NVIDIA device whose adapter another driver made (e.g. a USB-C controller).
        assert!(!is_nvidia_gpu_adapter(Some(0x10DE), "i2c-nvidia-gpu"));
        assert!(!is_nvidia_gpu_adapter(None, "NVIDIA i2c adapter 1 at 1:00.0"));
    }

    fn pci_dev(root: &Path) -> PathBuf {
        let dev = root.join("0000:01:00.0");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("subsystem_vendor"), "0x1043\n").unwrap();
        fs::write(dev.join("subsystem_device"), "0x89e3\n").unwrap();
        dev
    }

    #[test]
    fn adapter_as_child_of_the_pci_device() {
        let root = std::env::temp_dir().join(format!("ag-i2c-child-{}", std::process::id()));
        let dev = pci_dev(&root);
        let adapter = dev.join("i2c-9");
        fs::create_dir_all(&adapter).unwrap();
        // /sys/bus/i2c/devices/i2c-9 is a symlink to the adapter
        let bus = root.join("bus");
        fs::create_dir_all(&bus).unwrap();
        std::os::unix::fs::symlink(&adapter, bus.join("i2c-9")).unwrap();
        let found = pci_device_of(&bus.join("i2c-9")).unwrap();
        assert_eq!(subsystem_of(&found), Some(0x89E31043));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn adapter_with_a_device_link() {
        let root = std::env::temp_dir().join(format!("ag-i2c-link-{}", std::process::id()));
        let dev = pci_dev(&root);
        let adapter = root.join("i2c-3");
        fs::create_dir_all(&adapter).unwrap();
        std::os::unix::fs::symlink(&dev, adapter.join("device")).unwrap();
        let found = pci_device_of(&adapter).unwrap();
        assert_eq!(subsystem_of(&found), Some(0x89E31043));
        fs::remove_dir_all(&root).unwrap();
    }

    fn card() -> Vec<Adapter> {
        (1..=7u32)
            .map(|n| Adapter {
                num: n + 8,
                name: format!("NVIDIA i2c adapter {n} at 1:00.0"),
                subsystem: 0x89E31043,
                model: "ROG Astral RTX 5090 OC",
                gpu_name: "NVIDIA GPU".into(),
            })
            .collect()
    }

    fn saved(adapter: &str, subsystem: u32) -> crate::config::I2cChoice {
        crate::config::I2cChoice { adapter: adapter.into(), subsystem }
    }

    #[test]
    fn without_a_saved_choice_adapter_1_is_used() {
        assert_eq!(choose(&card(), None, None).unwrap().num, 9);
    }

    #[test]
    fn the_detected_adapter_wins_over_the_guess() {
        let s = saved("NVIDIA i2c adapter 3 at 1:00.0", 0x89E31043);
        assert_eq!(choose(&card(), None, Some(&s)).unwrap().num, 11);
    }

    #[test]
    fn a_saved_adapter_that_vanished_is_an_error_not_a_guess() {
        // Same card, but its adapters were renamed (moved slot): never fall back silently.
        let s = saved("NVIDIA i2c adapter 3 at 2:00.0", 0x89E31043);
        assert!(choose(&card(), None, Some(&s)).is_err());
    }

    #[test]
    fn a_choice_saved_for_another_card_is_ignored() {
        let s = saved("NVIDIA i2c adapter 3 at 1:00.0", 0x89FF1043);
        assert_eq!(choose(&card(), None, Some(&s)).unwrap().num, 9);
    }

    #[test]
    fn the_environment_override_wins() {
        let s = saved("NVIDIA i2c adapter 3 at 1:00.0", 0x89E31043);
        assert_eq!(choose(&card(), Some(14), Some(&s)).unwrap().num, 14);
        assert!(choose(&card(), Some(2), None).is_err());
    }
}
