//! The RM control channel on Windows: the NVIDIA display driver's private escape
//! (D3DKMTEscape, gdi32), with an RM client, device and subdevice of our own.
//!
//! LAYOUT (observed in the driver's own traffic, see the parent module; VERIFIED 2026-11-10,
//! ROG Astral RTX 5090 OC, driver 621.14, Windows, with allocs and reads):
//!   Every escape starts with a 0x30-byte header: "NVDA", 0x00010002, total size, "**VN", the
//!   escape code, then zeros. The driver sets +0x14 to 1 when it handled the escape.
//!   Alloc   0x0500002A: +0x30 hRoot, +0x34 hParent, +0x38 hNew (the client's is filled in by
//!           the driver), +0x3C class, +0x40 params size, +0x44 RM status, +0x48 params.
//!   Control 0x0500002B: +0x30 hClient, +0x34 hObject, +0x38 cmd, +0x3C params size,
//!           +0x40 RM status, +0x44 params.
//!   Classes: 0x41 client (params: a handle, 0 = the driver picks), 0x80 device (0x38 bytes:
//!   device id, the client to share with, the rest 0), 0x2080 subdevice (sub-device id 0).

use std::ffi::c_void;

use libloading::Library;

const ESC_ALLOC: u32 = 0x0500_002A;
const ESC_CONTROL: u32 = 0x0500_002B;
const HEADER: usize = 0x30;
const HANDLED_AT: usize = 0x14;
const CLASS_CLIENT: u32 = 0x41;
const CLASS_DEVICE: u32 = 0x80;
const CLASS_SUBDEVICE: u32 = 0x2080;
const DEVICE_PARAMS: usize = 0x38;
/// Our device and subdevice handles (RM handles are per client, so any unused value works).
const H_DEVICE: u32 = 0xA6A0_0001;
const H_SUBDEVICE: u32 = 0xA6A0_0002;
const NVIDIA: u32 = 0x10DE;
const KMTQAITYPE_PHYSICALADAPTERDEVICEIDS: u32 = 31;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Luid {
    low: u32,
    high: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AdapterInfo {
    h_adapter: u32,
    luid: Luid,
    sources: u32,
    precise: i32,
}

#[repr(C)]
struct EnumAdapters2 {
    count: u32,
    adapters: *mut AdapterInfo,
}

#[repr(C)]
struct QueryAdapterInfo {
    h_adapter: u32,
    kind: u32,
    data: *mut c_void,
    size: u32,
}

#[repr(C)]
struct Escape {
    h_adapter: u32,
    h_device: u32,
    kind: u32,
    flags: u32,
    data: *mut c_void,
    size: u32,
    h_context: u32,
}

#[repr(C)]
struct CloseAdapter {
    h_adapter: u32,
}

type FnPtr = unsafe extern "system" fn(*mut c_void) -> i32;

pub struct Session {
    // Kept so the function pointers stay valid; gdi32 is never unloaded in practice.
    _gdi: Library,
    escape: FnPtr,
    close: FnPtr,
    h_adapter: u32,
    client: u32,
}

// The handles are driver tokens; every use is a call under clkctl's lock.
unsafe impl Send for Session {}

fn nt(status: i32, what: &str) -> Result<(), String> {
    if status != 0 {
        return Err(format!("{what} failed (NTSTATUS 0x{:08X})", status as u32));
    }
    Ok(())
}

impl Session {
    /// Find the NVIDIA adapter whose PCI subsystem is `subsystem`, and allocate our RM objects.
    pub fn open(subsystem: u32) -> Result<Session, String> {
        unsafe {
            let gdi = Library::new("gdi32.dll").map_err(|e| format!("gdi32.dll not loadable ({e})"))?;
            let sym = |name: &[u8]| -> Result<FnPtr, String> {
                gdi.get::<FnPtr>(name)
                    .map(|s| *s)
                    .map_err(|e| format!("gdi32 has no {} ({e})", String::from_utf8_lossy(name)))
            };
            let enum2 = sym(b"D3DKMTEnumAdapters2\0")?;
            let query = sym(b"D3DKMTQueryAdapterInfo\0")?;
            let escape = sym(b"D3DKMTEscape\0")?;
            let close = sym(b"D3DKMTCloseAdapter\0")?;

            let mut e = EnumAdapters2 { count: 0, adapters: std::ptr::null_mut() };
            nt(enum2(&mut e as *mut _ as *mut c_void), "D3DKMTEnumAdapters2")?;
            let mut list = vec![AdapterInfo::default(); e.count as usize];
            e.adapters = list.as_mut_ptr();
            nt(enum2(&mut e as *mut _ as *mut c_void), "D3DKMTEnumAdapters2")?;
            list.truncate(e.count as usize);

            let mut found = None;
            for a in &list {
                // PhysicalAdapterIndex, then VendorID DeviceID SubVendorID SubSystemID RevisionID BusType
                let mut ids = [0u32; 7];
                let mut q = QueryAdapterInfo {
                    h_adapter: a.h_adapter,
                    kind: KMTQAITYPE_PHYSICALADAPTERDEVICEIDS,
                    data: ids.as_mut_ptr() as *mut c_void,
                    size: std::mem::size_of_val(&ids) as u32,
                };
                let ok = query(&mut q as *mut _ as *mut c_void) == 0;
                if found.is_none() && ok && ids[1] == NVIDIA && (ids[4] << 16 | ids[3]) == subsystem {
                    found = Some(a.h_adapter);
                } else {
                    let mut c = CloseAdapter { h_adapter: a.h_adapter };
                    close(&mut c as *mut _ as *mut c_void);
                }
            }
            let h_adapter = found.ok_or(format!("no display adapter with subsystem 0x{subsystem:08X}"))?;
            let mut s = Session { _gdi: gdi, escape, close, h_adapter, client: 0 };
            s.client = s.alloc(0, 0, 0, CLASS_CLIENT, &[0; 4])?;
            let client = s.client;
            let mut dev = [0u8; DEVICE_PARAMS];
            dev[4..8].copy_from_slice(&client.to_le_bytes());
            s.alloc(client, client, H_DEVICE, CLASS_DEVICE, &dev)?;
            s.alloc(client, H_DEVICE, H_SUBDEVICE, CLASS_SUBDEVICE, &[0; 4])?;
            Ok(s)
        }
    }

    fn escape(&mut self, code: u32, body: &[u8]) -> Result<Vec<u8>, String> {
        let size = HEADER + body.len();
        let mut buf = vec![0u8; size];
        buf[0..4].copy_from_slice(b"ADVN");
        buf[4..8].copy_from_slice(&0x0001_0002u32.to_le_bytes());
        buf[8..12].copy_from_slice(&(size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(b"**VN");
        buf[16..20].copy_from_slice(&code.to_le_bytes());
        buf[HEADER..].copy_from_slice(body);
        let mut e = Escape {
            h_adapter: self.h_adapter,
            h_device: 0,
            kind: 0, // D3DKMT_ESCAPE_DRIVERPRIVATE
            flags: 0,
            data: buf.as_mut_ptr() as *mut c_void,
            size: size as u32,
            h_context: 0,
        };
        nt(unsafe { (self.escape)(&mut e as *mut _ as *mut c_void) }, "D3DKMTEscape")?;
        if super::rd(&buf, HANDLED_AT) != Some(1) {
            return Err(format!("escape 0x{code:08X} was not handled by the driver"));
        }
        Ok(buf)
    }

    fn alloc(&mut self, root: u32, parent: u32, new: u32, class: u32, params: &[u8]) -> Result<u32, String> {
        let mut body = Vec::with_capacity(0x18 + params.len());
        for v in [root, parent, new, class, params.len() as u32, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(params);
        let out = self.escape(ESC_ALLOC, &body)?;
        let status = super::rd(&out, 0x44).unwrap_or(u32::MAX);
        if status != 0 {
            return Err(format!("RM alloc of class 0x{class:x} failed (status 0x{status:x})"));
        }
        super::rd(&out, 0x38).filter(|&h| h != 0).ok_or("RM alloc returned no handle".into())
    }
}

impl super::Rm for Session {
    fn control(&mut self, cmd: u32, params: &mut [u8]) -> Result<(), String> {
        let mut body = Vec::with_capacity(0x14 + params.len());
        for v in [self.client, H_SUBDEVICE, cmd, params.len() as u32, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(params);
        let out = self.escape(ESC_CONTROL, &body)?;
        let status = super::rd(&out, 0x40).unwrap_or(u32::MAX);
        if status != 0 {
            return Err(format!("RM control 0x{cmd:08X} failed (status 0x{status:x})"));
        }
        params.copy_from_slice(&out[0x44..0x44 + params.len()]);
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The RM objects go with the process; the adapter handle is ours to close.
        let mut c = CloseAdapter { h_adapter: self.h_adapter };
        unsafe { (self.close)(&mut c as *mut _ as *mut c_void) };
    }
}
