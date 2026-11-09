//! The one NVAPI binding of the process (Linux and Windows): the Windows pin sensor's I2C
//! reads and the Overclock tab's V/F curve calls both go through it.
//!
//! One binding, because NVAPI is process-global state: a second user's Unload would tear it
//! down under the pin sensor, the guard's only sensor on Windows. So the library is loaded
//! once, NEVER unloaded, and every call (the sensor's included) runs under one lock.
//!
//! Initialize runs on first use and again on every `with_reinit`, which the sensor's connect
//! path uses: a reconnect after a driver reset gets a fresh Initialize, as it did when the
//! sensor loaded NVAPI itself. Plain `with` reuses the current initialization. NVAPI accepts
//! repeated Initialize calls; with no Unload the count stays unbalanced, which is harmless.
//!
//! The single lock means a hung driver call delays the pin reads for as long as it hangs;
//! the overclock calls NVAPI only on apply and on its status refresh, never in a loop.
//!
//! I2C stays read-only (CLAUDE.md, safety rule 1): NvAPI_I2CWriteEx is not bound here or
//! anywhere else.
//!
//! V/F LAYOUTS (undocumented NVAPI; from ekojsalim/nvcurve, MIT, protocol only, no code
//! copied). VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux). The
//! Windows path (nvapi64.dll, same ids and layouts) is UNVERIFIED.
//!   Every buffer starts with a version word `(1 << 16) | size`; each call is
//!   `fn(gpu, *mut buffer) -> status` (0 = OK).
//!   Mask    0x507B4B59, 0x182C bytes: request bytes 4..36 = 0xFF, the answer's 4..36 is the
//!           mask of points that exist (one bit per point, LSB first).
//!   Curve   0x21537AD4, 0x1C28 bytes, mask in 4..36: entries at 0x48, stride 0x1C,
//!           +0x00 u32 effective frequency kHz, +0x04 u32 voltage µV.
//!   Control 0x23F1B133 (get) / 0x0733E009 (set), 0x2420 bytes, mask in 4..36: entries at
//!           0x44, stride 0x24, +0x00 u32 flags (1 = memory clock point), +0x14 i32 delta kHz.
//!           A set changes only the points whose mask bit is set.
//!   Voltage 0x465F9BCF, 0x4C bytes: +0x28 u32 core voltage µV.

use std::ffi::c_void;
use std::sync::Mutex;

use libloading::Library;

use crate::overclock::curve::RawPoint;

#[cfg(windows)]
const LIBRARY: &str = "nvapi64.dll";
#[cfg(not(windows))]
const LIBRARY: &str = "libnvidia-api.so.1";

const NVAPI_OK: i32 = 0;

const NVAPI_INITIALIZE: u32 = 0x0150E828;
const NVAPI_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC921F;
const NVAPI_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2DDFB66E;
const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;

// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux); Windows UNVERIFIED.
const VF_MASK: u32 = 0x507B4B59;
const VF_MASK_SIZE: usize = 0x182C;
// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux); Windows UNVERIFIED.
const VF_CURVE: u32 = 0x21537AD4;
const VF_CURVE_SIZE: usize = 0x1C28;
const VF_CURVE_ENTRIES_AT: usize = 0x48;
const VF_CURVE_STRIDE: usize = 0x1C;
// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux); Windows UNVERIFIED.
const VF_CONTROL_GET: u32 = 0x23F1B133;
const VF_CONTROL_SET: u32 = 0x0733E009;
const VF_CONTROL_SIZE: usize = 0x2420;
const VF_CONTROL_ENTRIES_AT: usize = 0x44;
const VF_CONTROL_STRIDE: usize = 0x24;
const VF_CONTROL_DELTA_AT: usize = 0x14;
const VF_CONTROL_FLAG_MEMORY: u32 = 1;
// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux); Windows UNVERIFIED.
const CORE_VOLTAGE: u32 = 0x465F9BCF;
const CORE_VOLTAGE_SIZE: usize = 0x4C;
const CORE_VOLTAGE_UV_AT: usize = 0x28;

/// Bytes 4..36 of the V/F buffers: one bit per curve point.
const MASK: std::ops::Range<usize> = 4..36;
/// Points the mask can address (32 bytes); the control table holds 255 of them.
const MASK_POINTS: usize = (MASK.end - MASK.start) * 8;
const CONTROL_POINTS: usize = (VF_CONTROL_SIZE - VF_CONTROL_ENTRIES_AT) / VF_CONTROL_STRIDE;

// `request` builds word buffers, and the fields read out of them must lie inside.
const _: () = {
    assert!(VF_MASK_SIZE.is_multiple_of(4) && VF_CURVE_SIZE.is_multiple_of(4));
    assert!(VF_CONTROL_SIZE.is_multiple_of(4) && CORE_VOLTAGE_SIZE.is_multiple_of(4));
    assert!(MASK.end <= VF_CURVE_ENTRIES_AT && MASK.end <= VF_CONTROL_ENTRIES_AT);
    assert!(CONTROL_POINTS == 255 && CONTROL_POINTS <= MASK_POINTS);
    assert!(CORE_VOLTAGE_UV_AT.is_multiple_of(4) && CORE_VOLTAGE_UV_AT + 4 <= CORE_VOLTAGE_SIZE);
};

type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type FnVoid = unsafe extern "C" fn() -> i32;
type FnEnum = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type FnPci = unsafe extern "C" fn(*mut c_void, *mut u32, *mut u32, *mut u32, *mut u32) -> i32;
type FnBuffer = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

/// The loaded library. Only reachable inside `with`, i.e. under the lock.
pub struct Api {
    // Kept for the life of the process: the static below is never cleared, so this never
    // drops and the library is never unloaded.
    _lib: Library,
    query: QueryInterface,
}

/// An NVAPI physical GPU handle: an opaque token from EnumPhysicalGPUs. Only dereferenced by
/// the driver. It may go stale after a re-Initialize (`with_reinit`, on every sensor connect,
/// for instance after a driver reset), so callers do not keep one: they look it up again in
/// every call (`gpu_by_subsystem`) under the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuHandle(pub *mut c_void);

// The handle is an opaque id, not memory we own; every use of it is a call under the lock.
unsafe impl Send for GpuHandle {}

/// The library plus whether its last Initialize succeeded. Once loaded it stays here (and
/// loaded) for the life of the process, including after a failed Initialize.
struct Binding {
    api: Api,
    initialized: bool,
}

static API: Mutex<Option<Binding>> = Mutex::new(None);

/// Run `f` with the library, loading and initializing it on first use. Holds the process-wide
/// NVAPI lock for the whole of `f`: every NVAPI call goes through here.
///
/// A failed load or Initialize is retried on the next call (the driver may come up after the
/// service). The lock is not reentrant: `f` must not call `with` or `with_reinit`.
pub fn with<R>(f: impl FnOnce(&Api) -> Result<R, String>) -> Result<R, String> {
    enter(false, f)
}

/// `with`, but Initialize runs again first even if it already succeeded: for a (re)connect,
/// so a driver reset since the last Initialize is recovered from. GPU handles from before
/// may be stale afterwards; look them up again.
// Only the Windows pin sensor connects through NVAPI.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn with_reinit<R>(f: impl FnOnce(&Api) -> Result<R, String>) -> Result<R, String> {
    enter(true, f)
}

/// Whether Initialize must run before the caller's calls.
fn must_initialize(initialized: bool, reinit: bool) -> bool {
    !initialized || reinit
}

fn enter<R>(reinit: bool, f: impl FnOnce(&Api) -> Result<R, String>) -> Result<R, String> {
    // A panic under the lock leaves the library just as usable; don't let it take the sensor down.
    let mut guard = API.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let binding = match &mut *guard {
        Some(binding) => binding,
        slot => slot.insert(Binding { api: load()?, initialized: false }),
    };
    if must_initialize(binding.initialized, reinit) {
        binding.initialized = false;
        binding.api.initialize()?;
        binding.initialized = true;
    }
    f(&binding.api)
}

fn load() -> Result<Api, String> {
    unsafe {
        let lib = Library::new(LIBRARY).map_err(|e| format!("{LIBRARY} not loadable ({e})"))?;
        let query: QueryInterface = *lib
            .get::<QueryInterface>(b"nvapi_QueryInterface\0")
            .map_err(|e| format!("nvapi_QueryInterface missing ({e})"))?;
        Ok(Api { _lib: lib, query })
    }
}

impl Api {
    fn initialize(&self) -> Result<(), String> {
        let status = unsafe {
            let init: FnVoid = self.query(NVAPI_INITIALIZE)?;
            init()
        };
        if status != NVAPI_OK {
            return Err(format!("NvAPI_Initialize status {status}"));
        }
        Ok(())
    }

    /// The function behind `id`, as `T` (an `unsafe extern "C" fn` type).
    ///
    /// # Safety
    /// `T` must be a function pointer type matching the real signature of `id`, and it may
    /// only be called inside `with` (under the lock). The pointer itself stays valid forever.
    pub unsafe fn query<T: Copy>(&self, id: u32) -> Result<T, String> {
        if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
            return Err(format!("nvapi query {id:#010x}: target is not a function pointer"));
        }
        let ptr = (self.query)(id);
        if ptr.is_null() {
            return Err(format!("nvapi_QueryInterface({id:#010x}) returned NULL"));
        }
        Ok(std::mem::transmute_copy::<*mut c_void, T>(&ptr))
    }

    /// Every physical GPU with its PCI subsystem id, in NVAPI's order. GPUs whose PCI ids
    /// cannot be read are left out.
    pub fn gpus(&self) -> Result<Vec<(GpuHandle, u32)>, String> {
        unsafe {
            let enum_gpus: FnEnum = self.query(NVAPI_ENUM_PHYSICAL_GPUS)?;
            let pci: FnPci = self.query(NVAPI_GPU_GET_PCI_IDENTIFIERS)?;
            let mut handles = [std::ptr::null_mut::<c_void>(); NVAPI_MAX_PHYSICAL_GPUS];
            let mut count = 0u32;
            let status = enum_gpus(handles.as_mut_ptr(), &mut count);
            if status != NVAPI_OK {
                return Err(format!("NvAPI_EnumPhysicalGPUs status {status}"));
            }
            let mut gpus = Vec::new();
            for &handle in handles.iter().take(count as usize) {
                // Out-parameters: device id, subsystem id, revision, external device id.
                let (mut dev, mut subsys, mut rev, mut ext) = (0u32, 0u32, 0u32, 0u32);
                if pci(handle, &mut dev, &mut subsys, &mut rev, &mut ext) == NVAPI_OK {
                    gpus.push((GpuHandle(handle), subsys));
                }
            }
            Ok(gpus)
        }
    }

    /// The first GPU with this PCI subsystem id.
    pub fn gpu_by_subsystem(&self, subsystem: u32) -> Result<GpuHandle, String> {
        self.gpus()?
            .into_iter()
            .find(|&(_, s)| s == subsystem)
            .map(|(gpu, _)| gpu)
            .ok_or_else(|| format!("no NVAPI GPU with subsystem {subsystem:#010x}"))
    }

    /// One `fn(gpu, *mut buffer)` call. `buf` holds the struct as bytes, in u32 words so it
    /// is 4-byte aligned like the C struct.
    fn call_buffer(&self, id: u32, gpu: GpuHandle, buf: &mut [u32]) -> Result<(), String> {
        unsafe {
            let f: FnBuffer = self.query(id)?;
            let status = f(gpu.0, buf.as_mut_ptr().cast());
            if status != NVAPI_OK {
                return Err(format!("nvapi {id:#010x} status {status}"));
            }
        }
        Ok(())
    }
}

/// A zeroed request of `size` bytes with its version word.
fn request(size: usize) -> Vec<u32> {
    debug_assert!(size.is_multiple_of(4));
    let mut words = vec![0u32; size / 4];
    words[0] = version(size);
    words
}

fn version(size: usize) -> u32 {
    (1 << 16) | size as u32
}

fn bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Bytes 4..36 of a word buffer (4..36 is word-aligned: words 1..9).
fn mask_words(buf: &[u32]) -> [u32; 8] {
    let mut m = [0u32; 8];
    m.copy_from_slice(&buf[1..9]);
    m
}

fn mask_of_points(api: &Api, gpu: GpuHandle) -> Result<[u32; 8], String> {
    let mut req = request(VF_MASK_SIZE);
    req[1..9].fill(u32::MAX);
    api.call_buffer(VF_MASK, gpu, &mut req)?;
    Ok(mask_words(&req))
}

fn control_with_mask(api: &Api, gpu: GpuHandle, mask: &[u32; 8]) -> Result<Vec<u32>, String> {
    let mut control = request(VF_CONTROL_SIZE);
    control[1..9].copy_from_slice(mask);
    api.call_buffer(VF_CONTROL_GET, gpu, &mut control)?;
    Ok(control)
}

/// The V/F curve as the driver reports it: effective frequency, voltage, applied delta.
pub fn read_curve(api: &Api, gpu: GpuHandle) -> Result<Vec<RawPoint>, String> {
    let mask = mask_of_points(api, gpu)?;
    let mut curve = request(VF_CURVE_SIZE);
    curve[1..9].copy_from_slice(&mask);
    api.call_buffer(VF_CURVE, gpu, &mut curve)?;
    let control = control_with_mask(api, gpu, &mask)?;
    Ok(parse_curve(&bytes(&curve), &bytes(&control)))
}

/// Set the frequency delta (kHz) of the given curve points; every other point keeps its own.
/// Not called by anything yet: the Overclocker (behind its compatibility check) will be the
/// only caller.
pub fn write_deltas(api: &Api, gpu: GpuHandle, deltas_khz: &[(usize, i32)]) -> Result<(), String> {
    if deltas_khz.is_empty() {
        return Ok(());
    }
    if let Some(&(i, _)) = deltas_khz.iter().find(|&&(i, _)| i >= CONTROL_POINTS) {
        return Err(format!("curve point {i} out of range (0..{CONTROL_POINTS})"));
    }
    let mask = mask_of_points(api, gpu)?;
    let current = control_with_mask(api, gpu, &mask)?;
    let mut set = words(&build_set_buffer(&bytes(&current), deltas_khz));
    api.call_buffer(VF_CONTROL_SET, gpu, &mut set)
}

/// The core voltage right now, µV.
pub fn core_voltage_uv(api: &Api, gpu: GpuHandle) -> Result<u32, String> {
    let mut buf = request(CORE_VOLTAGE_SIZE);
    api.call_buffer(CORE_VOLTAGE, gpu, &mut buf)?;
    Ok(buf[CORE_VOLTAGE_UV_AT / 4])
}

/// The points of a curve answer (`vfp`) joined with the control answer (`control`) for the
/// same mask. Stops at the first all-zero curve entry or where either buffer ends. The
/// memory clock points follow the core points: from the first with the memory flag onward,
/// every point is memory.
pub fn parse_curve(vfp: &[u8], control: &[u8]) -> Vec<RawPoint> {
    let mut points = Vec::new();
    let mut memory = false;
    for i in 0..MASK_POINTS {
        let at = VF_CURVE_ENTRIES_AT + i * VF_CURVE_STRIDE;
        let cat = VF_CONTROL_ENTRIES_AT + i * VF_CONTROL_STRIDE;
        let (Some(entry), Some(centry)) =
            (vfp.get(at..at + VF_CURVE_STRIDE), control.get(cat..cat + VF_CONTROL_STRIDE))
        else {
            break;
        };
        if entry.iter().all(|&b| b == 0) {
            break;
        }
        let (Some(freq_khz), Some(volt_uv), Some(flags), Some(delta)) =
            (u32_at(entry, 0), u32_at(entry, 4), u32_at(centry, 0), u32_at(centry, VF_CONTROL_DELTA_AT))
        else {
            break;
        };
        memory |= flags == VF_CONTROL_FLAG_MEMORY;
        points.push(RawPoint { freq_khz, volt_uv, delta_khz: delta as i32, memory });
    }
    points
}

/// A control set request built from the current control answer: same bytes, fresh version
/// word, the mask holding only the written points, and their deltas. Points the driver
/// would not change keep their bytes, so a stray mask bit would rewrite a value with itself.
/// Indices past the control table are ignored (`write_deltas` refuses them first).
pub fn build_set_buffer(current_control: &[u8], deltas_khz: &[(usize, i32)]) -> Vec<u8> {
    let mut b = current_control.to_vec();
    b.resize(VF_CONTROL_SIZE, 0);
    b[0..4].copy_from_slice(&version(VF_CONTROL_SIZE).to_le_bytes());
    b[MASK].fill(0);
    for &(i, delta) in deltas_khz {
        if i >= CONTROL_POINTS {
            continue;
        }
        b[MASK.start + i / 8] |= 1 << (i % 8);
        let at = VF_CONTROL_ENTRIES_AT + i * VF_CONTROL_STRIDE + VF_CONTROL_DELTA_AT;
        b[at..at + 4].copy_from_slice(&delta.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE_SIZE: usize = 0x1C28;
    const CURVE_ENTRIES_AT: usize = 0x48;
    const CURVE_STRIDE: usize = 0x1C;
    const CONTROL_SIZE: usize = 0x2420;
    const CONTROL_ENTRIES_AT: usize = 0x44;
    const CONTROL_STRIDE: usize = 0x24;

    fn put_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn curve_with(points: &[(u32, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; CURVE_SIZE];
        put_u32(&mut b, 0, (1 << 16) | CURVE_SIZE as u32);
        for (i, &(khz, uv)) in points.iter().enumerate() {
            let at = CURVE_ENTRIES_AT + i * CURVE_STRIDE;
            put_u32(&mut b, at, khz);
            put_u32(&mut b, at + 4, uv);
        }
        b
    }

    fn control_with(entries: &[(usize, u32, i32)]) -> Vec<u8> {
        let mut b = vec![0u8; CONTROL_SIZE];
        put_u32(&mut b, 0, (1 << 16) | CONTROL_SIZE as u32);
        // A mask the driver echoed back, so a test sees whether the set buffer clears it.
        b[4..36].fill(0xFF);
        for &(i, flags, delta) in entries {
            let at = CONTROL_ENTRIES_AT + i * CONTROL_STRIDE;
            put_u32(&mut b, at, flags);
            b[at + 0x14..at + 0x18].copy_from_slice(&delta.to_le_bytes());
        }
        b
    }

    fn control_with_deltas(deltas: &[(usize, i32)]) -> Vec<u8> {
        control_with(&deltas.iter().map(|&(i, d)| (i, 0, d)).collect::<Vec<_>>())
    }

    fn mask_bits(b: &[u8]) -> Vec<usize> {
        (0..256).filter(|&i| b[4 + i / 8] & (1 << (i % 8)) != 0).collect()
    }

    fn delta_at(b: &[u8], i: usize) -> i32 {
        let at = CONTROL_ENTRIES_AT + i * CONTROL_STRIDE + 0x14;
        i32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn parses_points_and_the_memory_boundary() {
        let vfp = curve_with(&[(1_000_000, 700_000), (1_500_000, 800_000), (2_000_000, 900_000), (14_000_000, 0)]);
        let control = control_with(&[(0, 0, 0), (1, 0, -15_000), (2, 0, 30_000), (3, 1, 0)]);
        let points = parse_curve(&vfp, &control);
        assert_eq!(
            points,
            vec![
                RawPoint { freq_khz: 1_000_000, volt_uv: 700_000, delta_khz: 0, memory: false },
                RawPoint { freq_khz: 1_500_000, volt_uv: 800_000, delta_khz: -15_000, memory: false },
                RawPoint { freq_khz: 2_000_000, volt_uv: 900_000, delta_khz: 30_000, memory: false },
                RawPoint { freq_khz: 14_000_000, volt_uv: 0, delta_khz: 0, memory: true },
            ]
        );
    }

    #[test]
    fn memory_stays_set_after_the_boundary() {
        // Once the memory points start, every later point is memory, whatever its own flags say.
        let vfp = curve_with(&[(1_000_000, 700_000), (14_000_000, 0), (15_000_000, 0)]);
        let control = control_with(&[(1, 1, 0), (2, 0, 0)]);
        let memory: Vec<bool> = parse_curve(&vfp, &control).iter().map(|p| p.memory).collect();
        assert_eq!(memory, vec![false, true, true]);
    }

    #[test]
    fn parse_stops_at_the_end_of_a_short_buffer() {
        // A full table with no zero entry: stops where either buffer ends, never reads past it.
        let mut vfp = vec![0xAAu8; CURVE_ENTRIES_AT + 3 * CURVE_STRIDE + 5];
        put_u32(&mut vfp, 0, 0);
        let control = control_with(&[]);
        assert_eq!(parse_curve(&vfp, &control).len(), 3);
        assert_eq!(parse_curve(&vfp, &control[..CONTROL_ENTRIES_AT + CONTROL_STRIDE]).len(), 1);
    }

    #[test]
    fn set_buffer_touches_only_the_written_points() {
        let cur = control_with_deltas(&[(10, 5000)]);
        let b = build_set_buffer(&cur, &[(59, 862_000)]);
        assert_eq!(b.len(), CONTROL_SIZE);
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), (1 << 16) | 0x2420);
        assert_eq!(b[4 + 59 / 8] & (1 << (59 % 8)), 1 << (59 % 8));
        assert_eq!(mask_bits(&b), vec![59]);
        assert_eq!(delta_at(&b, 59), 862_000);
        assert_eq!(delta_at(&b, 10), 5000); // untouched entry keeps its bytes
    }

    #[test]
    fn set_buffer_writes_negative_deltas_and_several_points() {
        let cur = control_with_deltas(&[]);
        let b = build_set_buffer(&cur, &[(0, -100_000), (7, 1), (8, -1), (254, 42)]);
        assert_eq!(mask_bits(&b), vec![0, 7, 8, 254]);
        assert_eq!(delta_at(&b, 0), -100_000);
        assert_eq!(delta_at(&b, 7), 1);
        assert_eq!(delta_at(&b, 8), -1);
        assert_eq!(delta_at(&b, 254), 42);
        // Nothing outside the version word, the mask and the four delta fields changed.
        let mut expected = cur.clone();
        expected[4..36].copy_from_slice(&b[4..36]);
        for i in [0, 7, 8, 254] {
            let at = CONTROL_ENTRIES_AT + i * CONTROL_STRIDE + 0x14;
            expected[at..at + 4].copy_from_slice(&b[at..at + 4]);
        }
        assert_eq!(b, expected);
    }

    #[test]
    fn set_buffer_ignores_points_past_the_table() {
        let cur = control_with_deltas(&[]);
        let b = build_set_buffer(&cur, &[(CONTROL_POINTS, 1), (MASK_POINTS - 1, 1)]);
        assert!(mask_bits(&b).is_empty());
        assert_eq!(b.len(), CONTROL_SIZE);
    }

    #[test]
    fn initialize_runs_first_after_a_failure_and_on_every_reinit() {
        // Never initialized (first use, or the last Initialize failed): always runs.
        assert!(must_initialize(false, false));
        assert!(must_initialize(false, true));
        // Initialized: plain `with` reuses it, a (re)connect runs it again.
        assert!(!must_initialize(true, false));
        assert!(must_initialize(true, true));
    }

    #[test]
    fn words_round_trip_little_endian() {
        assert_eq!(bytes(&[0x0102_0304]), vec![4, 3, 2, 1]);
        assert_eq!(words(&bytes(&[0x0102_0304, 0xFFFF_FFFE])), vec![0x0102_0304, 0xFFFF_FFFE]);
    }
}
