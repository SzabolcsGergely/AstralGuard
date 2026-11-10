//! The RM control channel on Linux: ioctls on /dev/nvidiactl, with an RM client, device and
//! subdevice of our own (the root service runs it; the GPU is already open through NVML).
//!
//! ABI (NVIDIA's open kernel module headers: nv-ioctl-numbers.h, nv_escape.h, nvos.h; MIT,
//! numbers and layouts only): request = _IOWR('F', escape, sizeof(params)).
//!   RM_FREE    0x29, NVOS00 (16 bytes): hRoot, hObjectParent, hObjectOld, status.
//!   RM_CONTROL 0x2A, NVOS54 (32 bytes): hClient, hObject, cmd, flags, params (u64 pointer,
//!              8-aligned), paramsSize, status.
//!   RM_ALLOC   0x2B, NVOS64 (48 bytes): hRoot, hObjectParent, hObjectNew, hClass,
//!              pAllocParms (u64), pRightsRequested (u64, 0), paramsSize, flags, status.
//! The GPU is found by its PCI subsystem: the root client's attached GPU ids
//! (NV0000 GPU_GET_ATTACHED_IDS), each one's device / subdevice instance (GPU_GET_ID_INFO_V2),
//! and the subdevice's PCI ids (NV2080 BUS_GET_PCI_INFO).
//! VERIFIED (2026-11-11, ROG Astral RTX 5090 OC, driver 620.70.06, Linux): the client, device
//! and subdevice allocate, the subsystem match finds the card, and the clock controls go
//! through (see the parent module for what was checked). The parent module's checks and write
//! check still guard every write, on every driver.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

const MAGIC: u64 = b'F' as u64;
const ESC_RM_FREE: u64 = 0x29;
const ESC_RM_CONTROL: u64 = 0x2A;
const ESC_RM_ALLOC: u64 = 0x2B;

const CLASS_CLIENT: u32 = 0x41;
const CLASS_DEVICE: u32 = 0x80;
const CLASS_SUBDEVICE: u32 = 0x2080;
const DEVICE_PARAMS: usize = 0x38;
const H_DEVICE: u32 = 0xA6A0_0001;
const H_SUBDEVICE: u32 = 0xA6A0_0002;

const CMD_GET_ATTACHED_IDS: u32 = 0x0000_0201;
const CMD_GET_ID_INFO_V2: u32 = 0x0000_0205;
const CMD_BUS_GET_PCI_INFO: u32 = 0x2080_1801;
const ATTACHED_IDS: usize = 32;
const INVALID_GPU_ID: u32 = 0xFFFF_FFFF;

fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (MAGIC << 8) | nr
}

#[repr(C)]
#[derive(Default)]
struct Nvos54 {
    h_client: u32,
    h_object: u32,
    cmd: u32,
    flags: u32,
    params: u64,
    params_size: u32,
    status: u32,
}

#[repr(C)]
#[derive(Default)]
struct Nvos64 {
    h_root: u32,
    h_parent: u32,
    h_new: u32,
    h_class: u32,
    alloc_params: u64,
    rights: u64,
    params_size: u32,
    flags: u32,
    status: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Nvos00 {
    h_root: u32,
    h_parent: u32,
    h_old: u32,
    status: u32,
}

const _: () = {
    assert!(std::mem::size_of::<Nvos54>() == 32);
    assert!(std::mem::size_of::<Nvos64>() == 48);
    assert!(std::mem::size_of::<Nvos00>() == 16);
};

pub struct Session {
    ctl: File,
    client: u32,
}

impl Session {
    pub fn open(subsystem: u32) -> Result<Session, String> {
        let ctl = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/nvidiactl")
            .map_err(|e| format!("/dev/nvidiactl: {e}"))?;
        let mut s = Session { ctl, client: 0 };
        s.client = s.alloc(0, 0, 0, CLASS_CLIENT, &mut [0; 4])?;
        let client = s.client;

        let mut ids = [0u8; ATTACHED_IDS * 4];
        s.raw_control(client, client, CMD_GET_ATTACHED_IDS, &mut ids)?;
        for k in 0..ATTACHED_IDS {
            let id = super::rd(&ids, k * 4).unwrap_or(INVALID_GPU_ID);
            if id == INVALID_GPU_ID || id == 0 {
                continue;
            }
            let mut info = [0u8; 32];
            info[0..4].copy_from_slice(&id.to_le_bytes());
            if s.raw_control(client, client, CMD_GET_ID_INFO_V2, &mut info).is_err() {
                continue;
            }
            let (device, sub) = (super::rd(&info, 8).unwrap_or(0), super::rd(&info, 12).unwrap_or(0));
            let mut dev = [0u8; DEVICE_PARAMS];
            dev[0..4].copy_from_slice(&device.to_le_bytes());
            dev[4..8].copy_from_slice(&client.to_le_bytes());
            if s.alloc(client, client, H_DEVICE, CLASS_DEVICE, &mut dev).is_err() {
                continue;
            }
            let mut sp = sub.to_le_bytes();
            let ok = s.alloc(client, H_DEVICE, H_SUBDEVICE, CLASS_SUBDEVICE, &mut sp).is_ok() && {
                let mut pci = [0u8; 16];
                s.raw_control(client, H_SUBDEVICE, CMD_BUS_GET_PCI_INFO, &mut pci).is_ok()
                    && super::rd(&pci, 4) == Some(subsystem)
            };
            if ok {
                return Ok(s);
            }
            // Not this GPU: the device goes (its subdevice with it), the next one is tried.
            s.free(client, client, H_DEVICE);
        }
        Err(format!("no GPU with subsystem 0x{subsystem:08X} in the driver's attached GPUs"))
    }

    fn ioctl<T>(&self, nr: u64, arg: &mut T) -> Result<(), String> {
        let req = iowr(nr, std::mem::size_of::<T>());
        let rc = unsafe { libc::ioctl(self.ctl.as_raw_fd(), req as _, arg as *mut T) };
        if rc < 0 {
            return Err(format!("nvidiactl ioctl 0x{req:x}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn alloc(&mut self, root: u32, parent: u32, new: u32, class: u32, params: &mut [u8]) -> Result<u32, String> {
        let mut a = Nvos64 {
            h_root: root,
            h_parent: parent,
            h_new: new,
            h_class: class,
            alloc_params: params.as_mut_ptr() as u64,
            params_size: params.len() as u32,
            ..Default::default()
        };
        self.ioctl(ESC_RM_ALLOC, &mut a)?;
        if a.status != 0 {
            return Err(format!("RM alloc of class 0x{class:x} failed (status 0x{:x})", a.status));
        }
        // The client's handle comes back in hObjectNew (and in its params).
        Some(a.h_new).filter(|&h| h != 0).ok_or("RM alloc returned no handle".into())
    }

    fn free(&mut self, root: u32, parent: u32, old: u32) {
        let mut f = Nvos00 { h_root: root, h_parent: parent, h_old: old, status: 0 };
        let _ = self.ioctl(ESC_RM_FREE, &mut f);
    }

    fn raw_control(&mut self, client: u32, object: u32, cmd: u32, params: &mut [u8]) -> Result<(), String> {
        let mut c = Nvos54 {
            h_client: client,
            h_object: object,
            cmd,
            params: params.as_mut_ptr() as u64,
            params_size: params.len() as u32,
            ..Default::default()
        };
        self.ioctl(ESC_RM_CONTROL, &mut c)?;
        if c.status != 0 {
            return Err(format!("RM control 0x{cmd:08X} failed (status 0x{:x})", c.status));
        }
        Ok(())
    }
}

impl super::Rm for Session {
    fn control(&mut self, cmd: u32, params: &mut [u8]) -> Result<(), String> {
        let client = self.client;
        self.raw_control(client, H_SUBDEVICE, cmd, params)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Freeing the client frees everything under it; closing the fd would too.
        let client = self.client;
        if client != 0 {
            self.free(client, client, client);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_request_numbers_match_the_driver_abi() {
        // _IOWR('F', 0x2A, 32) and _IOWR('F', 0x2B, 48), as the driver's own libraries send them.
        assert_eq!(super::iowr(super::ESC_RM_CONTROL, 32), 0xC020_462A);
        assert_eq!(super::iowr(super::ESC_RM_ALLOC, 48), 0xC030_462B);
        assert_eq!(super::iowr(super::ESC_RM_FREE, 16), 0xC010_4629);
    }
}
