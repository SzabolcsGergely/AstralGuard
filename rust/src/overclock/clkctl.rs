//! The XBAR / SYS clock offsets: the driver's clock-domain control (NV2080 CLK_DOMAINS
//! GET_INFO / GET_CONTROL / SET_CONTROL). Undocumented. On Windows it is reached through the
//! display driver's private escape channel (D3DKMTEscape), the same RM API the driver's own
//! libraries use; on Linux through ioctls on /dev/nvidiactl. The parameter blocks are the
//! same on both: only the way they reach the driver differs.
//!
//! LAYOUTS: observed in a trace of what mVolt+ sends the driver (calls and buffers only; no
//! code of it was read or copied), then confirmed with calls of our own. VERIFIED (2026-11-10,
//! ROG Astral RTX 5090 OC, driver 621.14, Windows): GET_INFO finds XBAR (domain 0x2) at index 1
//! and SYS (0x4) at index 3, both -1000..1000 MHz; GET_CONTROL reads their offsets; the write
//! check passed, and an XBAR +15 MHz apply and its untick read back +15 and +0 MHz; SYS writes
//! VERIFIED too (2026-11-11). Linux: the same blocks through the ioctl ABI of NVIDIA's open
//! kernel module headers, behind the same checks. VERIFIED (2026-11-11, same card, driver
//! 620.70.06, Linux): the GPU found by its subsystem, the write check passed, XBAR +100 and
//! +200 MHz applied and read back, unticks restored +0 MHz, SYS writes, and CLK_MEASURE_FREQ
//! reads XBAR and SYS.
//!
//! Because the layouts may move with any driver, nothing is written on trust:
//! - Every reply is checked against the layout: escape handled, RM status 0, the domain
//!   found by its id (never by a fixed index), a sane range, the control reply echoing the
//!   request, entry type 0x10, the offset inside the range. Anything else refuses.
//! - Before the first write on a driver version, a write check: the whole control block of
//!   every domain is read, +15 MHz written to XBAR alone, everything read again (only that one
//!   field may change, and to exactly that value), the original written back, and the block
//!   compared byte for byte with the first read. It passes once per driver version
//!   (`oc_clk_verified_driver`); a wrong landing fails the driver for good
//!   (`oc_clk_failed_driver`), and no XBAR / SYS write happens on it again.

use super::ClkDomain;

pub const CMD_GET_INFO: u32 = 0x2080_9019;
pub const CMD_GET_CONTROL: u32 = 0x2080_901B;
pub const CMD_SET_CONTROL: u32 = 0x2080_D01C;
/// CLK_MEASURE_FREQ: {domain id, measured kHz}. A read. VERIFIED (2026-11-10, ROG Astral RTX
/// 5090 OC, driver 621.14, Windows): GPC, MCLK, XBAR and SYS read 2916, 14002, 2442 and 2292 MHz
/// under load, MCLK matching NVML's memory clock.
pub const CMD_MEASURE: u32 = 0x2080_9006;
const MEASURE_SIZE: usize = 8;
/// A measured clock above this is not a clock: layout not recognised.
const MAX_MEASURED_KHZ: u32 = 20_000_000;

/// GET_INFO's parameter block.
pub const INFO_SIZE: usize = 0x3030;
const INFO_MASK_AT: usize = 0x04;
const INFO_ENTRIES_AT: usize = 0x34;
const INFO_STRIDE: usize = 0x180;
const INFO_DOMAIN_AT: usize = 0x00;
/// i16, MHz.
const INFO_MIN_AT: usize = 0x22;
/// i32, MHz.
const INFO_MAX_AT: usize = 0x24;

/// GET_CONTROL / SET_CONTROL's parameter block.
pub const CONTROL_SIZE: usize = 0x83C;
const CTL_HEAD_AT: usize = 0x00;
const CTL_MASK_AT: usize = 0x04;
const CTL_FLAGS_AT: usize = 0x08;
const CTL_ENTRIES_AT: usize = 0x3C;
const CTL_STRIDE: usize = 0x40;
const CTL_TYPE_AT: usize = 0x00;
/// i32, kHz.
const CTL_OFFSET_AT: usize = 0x0C;
/// What a GET_CONTROL reply carries at the head, and what a SET carries in its flags word.
const CTL_REPLY_HEAD: u32 = 0xFF;
const CTL_FLAGS: u32 = 0x0101_0000;
const CTL_TYPE: u32 = 0x10;

const DOMAIN_XBAR: u32 = 0x2;
const DOMAIN_SYS: u32 = 0x4;
/// No clock domain on this hardware has an offset range anywhere near this; beyond it the
/// range field is not what we think it is.
const SANE_RANGE_MHZ: i32 = 5000;
const PROBE_KHZ: i32 = 15_000;

const _: () = {
    assert!(CTL_ENTRIES_AT + 32 * CTL_STRIDE <= CONTROL_SIZE);
    assert!(INFO_MAX_AT + 4 <= INFO_STRIDE && CTL_OFFSET_AT + 4 <= CTL_STRIDE);
};

fn domain_id(d: ClkDomain) -> u32 {
    match d {
        ClkDomain::Xbar => DOMAIN_XBAR,
        ClkDomain::Sys => DOMAIN_SYS,
    }
}

fn rd(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn rd_i32(b: &[u8], at: usize) -> Option<i32> {
    rd(b, at).map(|v| v as i32)
}

fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// One clock domain as GET_INFO describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainInfo {
    /// Its index in the driver's tables (bit in the masks, entry in the blocks).
    pub index: usize,
    pub min_khz: i32,
    pub max_khz: i32,
}

/// Every domain in a GET_INFO reply, checked: the mask non-empty, every listed entry inside
/// the block and naming exactly one clock (one bit), no clock twice.
pub fn parse_info(p: &[u8]) -> Result<Vec<(u32, DomainInfo)>, String> {
    if p.len() != INFO_SIZE {
        return Err(format!("domain info is {} bytes, expected {INFO_SIZE}", p.len()));
    }
    let mask = rd(p, INFO_MASK_AT).unwrap_or(0);
    if mask == 0 {
        return Err("domain info lists no domains".into());
    }
    let mut out: Vec<(u32, DomainInfo)> = Vec::new();
    for index in (0..32).filter(|k| mask >> k & 1 == 1) {
        let at = INFO_ENTRIES_AT + index * INFO_STRIDE;
        let entry = p.get(at..at + INFO_STRIDE).ok_or(format!("domain {index} lies outside the info block"))?;
        let domain = rd(entry, INFO_DOMAIN_AT).unwrap_or(0);
        if domain.count_ones() != 1 || out.iter().any(|(d, _)| *d == domain) {
            return Err(format!("domain info entry {index} names clock 0x{domain:x}: layout not recognised"));
        }
        let min = i16::from_le_bytes([entry[INFO_MIN_AT], entry[INFO_MIN_AT + 1]]) as i32;
        let max = rd_i32(entry, INFO_MAX_AT).unwrap_or(0);
        out.push((domain, DomainInfo { index, min_khz: min * 1000, max_khz: max * 1000 }));
    }
    Ok(out)
}

/// The XBAR or SYS domain from a parsed GET_INFO, with a usable offset range.
pub fn find_domain(domains: &[(u32, DomainInfo)], d: ClkDomain) -> Result<DomainInfo, String> {
    let (_, info) = domains
        .iter()
        .find(|(id, _)| *id == domain_id(d))
        .ok_or(format!("the driver lists no {} clock domain", d.name()))?;
    let sane = -SANE_RANGE_MHZ * 1000..=SANE_RANGE_MHZ * 1000;
    if !(info.min_khz < 0 && info.max_khz > 0 && sane.contains(&info.min_khz) && sane.contains(&info.max_khz)) {
        return Err(format!(
            "the {} offset range reads {}..{} kHz: no offset allowed, or layout not recognised",
            d.name(),
            info.min_khz,
            info.max_khz
        ));
    }
    Ok(*info)
}

pub fn control_request(mask: u32) -> Vec<u8> {
    let mut p = vec![0u8; CONTROL_SIZE];
    put(&mut p, CTL_MASK_AT, mask);
    p
}

/// A GET_CONTROL reply for `mask`, checked: the head, the mask echoed, the flags word, and
/// entry type 0x10 for every domain asked for.
pub fn check_control(p: &[u8], mask: u32) -> Result<(), String> {
    if p.len() != CONTROL_SIZE {
        return Err(format!("control block is {} bytes, expected {CONTROL_SIZE}", p.len()));
    }
    let (head, echo, flags) = (rd(p, CTL_HEAD_AT), rd(p, CTL_MASK_AT), rd(p, CTL_FLAGS_AT));
    if head != Some(CTL_REPLY_HEAD) || echo != Some(mask) || flags != Some(CTL_FLAGS) {
        return Err(format!(
            "control reply head 0x{:x} mask 0x{:x} flags 0x{:x} (asked 0x{mask:x}): layout not recognised",
            head.unwrap_or(0),
            echo.unwrap_or(0),
            flags.unwrap_or(0)
        ));
    }
    for index in (0..32).filter(|k| mask >> k & 1 == 1) {
        let ty = rd(p, CTL_ENTRIES_AT + index * CTL_STRIDE + CTL_TYPE_AT).unwrap_or(0) & 0xFF;
        if ty != CTL_TYPE {
            return Err(format!("control entry {index} has type 0x{ty:x}, expected 0x{CTL_TYPE:x}"));
        }
    }
    Ok(())
}

/// The offset of one domain from a checked GET_CONTROL reply, inside its range.
pub fn control_offset(p: &[u8], info: DomainInfo) -> Result<i32, String> {
    let khz = rd_i32(p, CTL_ENTRIES_AT + info.index * CTL_STRIDE + CTL_OFFSET_AT).unwrap_or(i32::MIN);
    if !(info.min_khz..=info.max_khz).contains(&khz) {
        return Err(format!(
            "offset reads {khz} kHz, outside {}..{} kHz: layout not recognised",
            info.min_khz, info.max_khz
        ));
    }
    Ok(khz)
}

/// SET_CONTROL for one domain: its mask bit, the flags word, the entry's type and offset.
/// Nothing else is set (the driver changes only the domains in the mask).
pub fn set_request(index: usize, khz: i32) -> Vec<u8> {
    let mut p = vec![0u8; CONTROL_SIZE];
    put(&mut p, CTL_MASK_AT, 1 << index);
    put(&mut p, CTL_FLAGS_AT, CTL_FLAGS);
    let at = CTL_ENTRIES_AT + index * CTL_STRIDE;
    put(&mut p, at + CTL_TYPE_AT, CTL_TYPE);
    put(&mut p, at + CTL_OFFSET_AT, khz as u32);
    p
}

/// The byte offsets where two blocks differ.
pub fn changed(a: &[u8], b: &[u8]) -> Vec<usize> {
    (0..a.len().max(b.len())).filter(|&i| a.get(i) != b.get(i)).collect()
}

/// The bytes a write of `index`'s offset may change, and nothing else.
fn offset_bytes(index: usize) -> std::ops::Range<usize> {
    let at = CTL_ENTRIES_AT + index * CTL_STRIDE + CTL_OFFSET_AT;
    at..at + 4
}

/// The driver's RM control channel.
pub trait Rm {
    /// Run `cmd` with `params` in and out. `declared` is the parameter size the driver is told
    /// (the struct's real size); the buffer may be shorter when the channel caps it.
    fn control(&mut self, cmd: u32, params: &mut [u8]) -> Result<(), String>;
}

/// Read, check and return the whole control block of every domain in `mask`.
fn read_block(rm: &mut dyn Rm, mask: u32) -> Result<Vec<u8>, String> {
    let mut p = control_request(mask);
    rm.control(CMD_GET_CONTROL, &mut p)?;
    check_control(&p, mask)?;
    Ok(p)
}

pub fn read_domains(rm: &mut dyn Rm) -> Result<Vec<(u32, DomainInfo)>, String> {
    let mut p = vec![0u8; INFO_SIZE];
    rm.control(CMD_GET_INFO, &mut p)?;
    parse_info(&p)
}

/// (current, min, max) offset of `d`, kHz.
pub fn read_offset(rm: &mut dyn Rm, d: ClkDomain) -> Result<(i32, i32, i32), String> {
    let info = find_domain(&read_domains(rm)?, d)?;
    let block = read_block(rm, 1 << info.index)?;
    Ok((control_offset(&block, info)?, info.min_khz, info.max_khz))
}

/// The clock `d` runs at now, kHz, measured by the driver.
pub fn measure(rm: &mut dyn Rm, d: ClkDomain) -> Result<u32, String> {
    let mut p = [0u8; MEASURE_SIZE];
    put(&mut p, 0, domain_id(d));
    rm.control(CMD_MEASURE, &mut p)?;
    let (id, khz) = (rd(&p, 0), rd(&p, 4).unwrap_or(0));
    if id != Some(domain_id(d)) || khz > MAX_MEASURED_KHZ {
        return Err(format!(
            "{} measure replied domain 0x{:x}, {khz} kHz: layout not recognised",
            d.name(),
            id.unwrap_or(0)
        ));
    }
    Ok(khz)
}

/// How the write check ended.
#[derive(Debug, PartialEq)]
pub enum Check {
    Pass,
    /// A call failed and the block came back as it was: try again another time.
    Error(String),
    /// The write landed wrong or the block did not come back: never again on this driver.
    Broken(String),
}

/// The write check (module doc): +15 MHz on XBAR alone, read everything back, undo, compare.
pub fn write_check(rm: &mut dyn Rm) -> Check {
    let domains = match read_domains(rm) {
        Ok(d) => d,
        Err(e) => return Check::Error(e),
    };
    let info = match find_domain(&domains, ClkDomain::Xbar) {
        Ok(i) => i,
        Err(e) => return Check::Error(e),
    };
    let all = domains.iter().fold(0u32, |m, (_, i)| m | 1 << i.index);
    let before = match read_block(rm, all) {
        Ok(b) => b,
        Err(e) => return Check::Error(e),
    };
    let orig = match control_offset(&before, info) {
        Ok(v) => v,
        Err(e) => return Check::Error(e),
    };
    let probe = if orig + PROBE_KHZ <= info.max_khz { orig + PROBE_KHZ } else { orig - PROBE_KHZ };
    let mut problem: Option<String> = None;
    let mut wrong = false;
    let mut set = set_request(info.index, probe);
    match rm.control(CMD_SET_CONTROL, &mut set) {
        Err(e) => problem = Some(format!("test write failed ({e})")),
        Ok(()) => {
            match read_block(rm, all) {
                Err(e) => problem = Some(format!("test read-back failed ({e})")),
                Ok(after) => {
                    let diff = changed(&before, &after);
                    let allowed = offset_bytes(info.index);
                    let now = control_offset(&after, info).ok();
                    if diff.iter().any(|i| !allowed.contains(i)) || now != Some(probe) {
                        wrong = true;
                        problem = Some(format!(
                            "a {probe} kHz write to XBAR read back {now:?} and changed byte(s) {:?}",
                            diff.iter().take(8).collect::<Vec<_>>()
                        ));
                    }
                }
            }
            let mut undo = set_request(info.index, orig);
            if let Err(e) = rm.control(CMD_SET_CONTROL, &mut undo) {
                problem.get_or_insert(format!("could not undo the test write ({e})"));
            }
        }
    }
    match read_block(rm, all) {
        Ok(end) if end == before => match problem {
            None => Check::Pass,
            Some(p) if wrong => Check::Broken(p),
            Some(p) => Check::Error(p),
        },
        Ok(end) => Check::Broken(format!(
            "{}THE CLOCK CONTROL MAY BE ALTERED: byte(s) {:?} differ from before the check",
            problem.map(|p| p + "; ").unwrap_or_default(),
            changed(&before, &end).iter().take(8).collect::<Vec<_>>()
        )),
        Err(e) => Check::Broken(format!(
            "{}the control block could not be read back ({e}); XBAR MAY STILL CARRY the test offset",
            problem.map(|p| p + "; ").unwrap_or_default()
        )),
    }
}

/// Write `khz` to `d` (the caller has checked the range), through a write-checked driver.
pub fn write_offset(rm: &mut dyn Rm, d: ClkDomain, khz: i32) -> Result<(), String> {
    let info = find_domain(&read_domains(rm)?, d)?;
    if !(info.min_khz..=info.max_khz).contains(&khz) {
        return Err(format!("{khz} kHz is outside {}..{} kHz", info.min_khz, info.max_khz));
    }
    let mut p = set_request(info.index, khz);
    rm.control(CMD_SET_CONTROL, &mut p)
}

// ---- the platform side: the session, and the driver-version gate around writes.

#[cfg(windows)]
mod windows_rm;
#[cfg(windows)]
use windows_rm::Session;
#[cfg(target_os = "linux")]
mod linux_rm;
#[cfg(target_os = "linux")]
use linux_rm::Session;

/// The open RM session for one card, kept for the life of the process. Dropped and opened
/// again after any failed call: a driver reset invalidates it.
#[cfg(any(windows, target_os = "linux"))]
static SESSION: std::sync::Mutex<Option<(u32, Session)>> = std::sync::Mutex::new(None);

#[cfg(any(windows, target_os = "linux"))]
fn with_session<R>(subsystem: u32, f: impl FnOnce(&mut dyn Rm) -> Result<R, String>) -> Result<R, String> {
    let mut guard = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    if guard.as_ref().is_none_or(|(s, _)| *s != subsystem) {
        *guard = None;
        *guard = Some((subsystem, Session::open(subsystem)?));
    }
    let r = f(&mut guard.as_mut().unwrap().1);
    if r.is_err() {
        *guard = None;
    }
    r
}

#[cfg(not(any(windows, target_os = "linux")))]
fn with_session<R>(_subsystem: u32, _f: impl FnOnce(&mut dyn Rm) -> Result<R, String>) -> Result<R, String> {
    Err("the XBAR / SYS clock control is not available on this platform".into())
}

/// (current, min, max) offset of `d` on the card, kHz.
#[cfg_attr(test, allow(dead_code))]
pub fn offset_khz(subsystem: u32, d: ClkDomain) -> Result<(i32, i32, i32), String> {
    with_session(subsystem, |rm| read_offset(rm, d))
}

/// The clock `d` runs at now, kHz.
#[cfg_attr(test, allow(dead_code))]
pub fn measure_khz(subsystem: u32, d: ClkDomain) -> Result<u32, String> {
    with_session(subsystem, |rm| measure(rm, d))
}

/// Write `khz` to `d`, after the write check has passed on `driver` (run now if it has not).
#[cfg_attr(test, allow(dead_code))]
pub fn set_offset_khz(subsystem: u32, driver: &str, d: ClkDomain, khz: i32) -> Result<(), String> {
    let cfg = crate::config::config_path();
    // Before any session is opened: a driver that failed the check is not touched at all.
    if let Some(why) = refused(&cfg, driver) {
        return Err(why);
    }
    with_session(subsystem, |rm| gated_write(rm, &cfg, driver, d, khz))
}

/// Some(why) when the write check failed on this driver before (config.ini remembers it).
fn refused(cfg: &std::path::Path, driver: &str) -> Option<String> {
    (crate::config::load_oc_clk_failed(cfg).as_deref() == Some(driver))
        .then(|| format!("the XBAR / SYS write check failed on driver {driver} before; disabled until a driver update"))
}

/// The write, behind safety rule 10's gate: on a driver not yet verified, the write check runs
/// first; a pass is remembered, a wrong landing disables the driver for good, and a check
/// that could not run leaves both unrecorded and writes nothing.
fn gated_write(rm: &mut dyn Rm, cfg: &std::path::Path, driver: &str, d: ClkDomain, khz: i32) -> Result<(), String> {
    if crate::config::load_oc_clk_verified(cfg).as_deref() != Some(driver) {
        match write_check(rm) {
            Check::Pass => {
                crate::logger::info(format!("Overclock: XBAR / SYS writes verified on driver {driver}"));
                if let Err(e) = crate::config::save_oc_clk_verified(cfg, driver) {
                    crate::logger::warn(format!("Overclock: could not save oc_clk_verified_driver ({e})"));
                }
            }
            Check::Error(e) => return Err(format!("the XBAR / SYS write check could not run ({e})")),
            Check::Broken(e) => {
                let why = format!("the XBAR / SYS write check failed on driver {driver}: {e}; XBAR / SYS disabled");
                crate::logger::error(format!("Overclock: {why}"));
                if let Err(e) = crate::config::save_oc_clk_failed(cfg, driver) {
                    crate::logger::warn(format!("Overclock: could not save oc_clk_failed_driver ({e})"));
                }
                return Err(why);
            }
        }
    }
    write_offset(rm, d, khz)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver in memory with this card's layout: XBAR at index 1 and SYS at 3, ±1000 MHz.
    /// `misroute`: a SET for index .0 lands on index .1 instead.
    struct FakeRm {
        offsets: [i32; 9],
        misroute: Option<(usize, usize)>,
        /// A SET also scribbles over the flags word of the next entry (a moved layout).
        scribble: bool,
        sets: usize,
    }

    const DOMAINS: [u32; 9] = [0x1, 0x2, 0x10, 0x4, 0x100000, 0x80000, 0x200000, 0x40, 0x8];

    impl FakeRm {
        fn new() -> Self {
            FakeRm { offsets: [0; 9], misroute: None, scribble: false, sets: 0 }
        }
    }

    impl Rm for FakeRm {
        fn control(&mut self, cmd: u32, p: &mut [u8]) -> Result<(), String> {
            match cmd {
                CMD_GET_INFO => {
                    put(p, INFO_MASK_AT, 0x1FF);
                    for (k, d) in DOMAINS.iter().enumerate() {
                        let at = INFO_ENTRIES_AT + k * INFO_STRIDE;
                        put(p, at + INFO_DOMAIN_AT, *d);
                        let (lo, hi) = if k < 5 { (-1000i16, 1000i32) } else { (0, 0) };
                        p[at + INFO_MIN_AT..at + INFO_MIN_AT + 2].copy_from_slice(&lo.to_le_bytes());
                        put(p, at + INFO_MAX_AT, hi as u32);
                    }
                }
                CMD_GET_CONTROL => {
                    let mask = rd(p, CTL_MASK_AT).unwrap();
                    put(p, CTL_HEAD_AT, CTL_REPLY_HEAD);
                    put(p, CTL_FLAGS_AT, CTL_FLAGS);
                    for k in (0..9).filter(|k| mask >> k & 1 == 1) {
                        let at = CTL_ENTRIES_AT + k * CTL_STRIDE;
                        put(p, at + CTL_TYPE_AT, 0x1010);
                        put(p, at + CTL_OFFSET_AT, self.offsets[k] as u32);
                    }
                }
                CMD_MEASURE => {
                    let id = rd(p, 0).unwrap();
                    put(p, 4, if id == DOMAIN_XBAR { 2_442_000 } else { 2_292_000 });
                }
                CMD_SET_CONTROL => {
                    self.sets += 1;
                    let mask = rd(p, CTL_MASK_AT).unwrap();
                    let k = mask.trailing_zeros() as usize;
                    let v = rd_i32(p, CTL_ENTRIES_AT + k * CTL_STRIDE + CTL_OFFSET_AT).unwrap();
                    let k = match self.misroute {
                        Some((from, to)) if from == k => to,
                        _ => k,
                    };
                    self.offsets[k] = v;
                    if self.scribble {
                        self.offsets[k + 1] = self.offsets[k + 1].wrapping_add(1);
                    }
                }
                _ => return Err(format!("unknown cmd 0x{cmd:x}")),
            }
            Ok(())
        }
    }

    #[test]
    fn the_domains_are_found_by_id_with_their_range() {
        let mut rm = FakeRm::new();
        let d = read_domains(&mut rm).unwrap();
        assert_eq!(
            find_domain(&d, ClkDomain::Xbar).unwrap(),
            DomainInfo { index: 1, min_khz: -1_000_000, max_khz: 1_000_000 }
        );
        assert_eq!(find_domain(&d, ClkDomain::Sys).unwrap().index, 3);
        rm.offsets[3] = 30_000;
        assert_eq!(read_offset(&mut rm, ClkDomain::Sys).unwrap(), (30_000, -1_000_000, 1_000_000));
    }

    #[test]
    fn a_reply_that_does_not_match_the_layout_is_refused() {
        let mut p = vec![0u8; INFO_SIZE];
        assert!(parse_info(&p).is_err(), "no domains");
        put(&mut p, INFO_MASK_AT, 0x3);
        put(&mut p, INFO_ENTRIES_AT, 0x1);
        put(&mut p, INFO_ENTRIES_AT + INFO_STRIDE, 0x3); // two bits: not a clock id
        assert!(parse_info(&p).unwrap_err().contains("not recognised"));
        assert!(parse_info(&p[..100]).is_err(), "wrong size");

        let mut c = control_request(0x2);
        assert!(check_control(&c, 0x2).is_err(), "no reply head");
        put(&mut c, CTL_HEAD_AT, CTL_REPLY_HEAD);
        put(&mut c, CTL_FLAGS_AT, CTL_FLAGS);
        assert!(check_control(&c, 0x2).unwrap_err().contains("type"), "entry type missing");
        put(&mut c, CTL_ENTRIES_AT + CTL_STRIDE, 0x1010);
        assert!(check_control(&c, 0x2).is_ok());
        assert!(check_control(&c, 0x8).is_err(), "mask not echoed");

        let info = DomainInfo { index: 1, min_khz: -1_000_000, max_khz: 1_000_000 };
        put(&mut c, CTL_ENTRIES_AT + CTL_STRIDE + CTL_OFFSET_AT, 5_000_000);
        assert!(control_offset(&c, info).unwrap_err().contains("outside"));
    }

    /// A config.ini of its own per test, removed at the end.
    struct Cfg(std::path::PathBuf);
    impl Cfg {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("ag-clkgate-{name}-{}.ini", std::process::id()));
            let _ = std::fs::remove_file(&p);
            Cfg(p)
        }
    }
    impl Drop for Cfg {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A driver whose every call fails, as after a driver reset.
    struct DeadRm;
    impl Rm for DeadRm {
        fn control(&mut self, _cmd: u32, _p: &mut [u8]) -> Result<(), String> {
            Err("the GPU fell off the bus".into())
        }
    }

    #[test]
    fn the_first_write_on_a_driver_runs_the_check_and_remembers_it() {
        let cfg = Cfg::new("first");
        let mut rm = FakeRm::new();
        gated_write(&mut rm, &cfg.0, "620.70.06", ClkDomain::Xbar, 200_000).unwrap();
        assert_eq!(rm.sets, 3, "the check's +15 MHz, its undo, then the offset");
        assert_eq!(rm.offsets[1], 200_000);
        assert_eq!(crate::config::load_oc_clk_verified(&cfg.0).as_deref(), Some("620.70.06"));

        gated_write(&mut rm, &cfg.0, "620.70.06", ClkDomain::Sys, 50_000).unwrap();
        assert_eq!(rm.sets, 4, "verified: no second check on the same driver");
        assert_eq!(rm.offsets[3], 50_000);

        gated_write(&mut rm, &cfg.0, "621.14", ClkDomain::Xbar, 100_000).unwrap();
        assert_eq!(rm.sets, 7, "a driver update is checked again");
        assert_eq!(crate::config::load_oc_clk_verified(&cfg.0).as_deref(), Some("621.14"));
    }

    #[test]
    fn a_wrong_landing_disables_the_driver_for_good() {
        let cfg = Cfg::new("broken");
        let mut rm = FakeRm::new();
        rm.misroute = Some((1, 2)); // XBAR's write lands on the next domain
        let err = gated_write(&mut rm, &cfg.0, "620.70.06", ClkDomain::Xbar, 200_000).unwrap_err();
        assert!(err.contains("write check failed on driver 620.70.06"), "{err}");
        assert_eq!(rm.offsets[1], 0, "the requested offset was never written");
        assert_eq!(crate::config::load_oc_clk_failed(&cfg.0).as_deref(), Some("620.70.06"));
        assert!(crate::config::load_oc_clk_verified(&cfg.0).is_none());

        let why = refused(&cfg.0, "620.70.06").expect("refused from now on, before any session opens");
        assert!(why.contains("until a driver update"), "{why}");
        assert!(refused(&cfg.0, "621.14").is_none(), "a new driver gets its own check");
    }

    #[test]
    fn a_layout_that_moved_is_caught_by_the_check() {
        let cfg = Cfg::new("scribble");
        let mut rm = FakeRm::new();
        rm.scribble = true; // a SET also changes the entry after it
        assert!(gated_write(&mut rm, &cfg.0, "999.1", ClkDomain::Xbar, 200_000).is_err());
        assert_eq!(crate::config::load_oc_clk_failed(&cfg.0).as_deref(), Some("999.1"));
    }

    #[test]
    fn a_check_that_cannot_run_writes_nothing_and_records_nothing() {
        let cfg = Cfg::new("dead");
        let err = gated_write(&mut DeadRm, &cfg.0, "620.70.06", ClkDomain::Xbar, 200_000).unwrap_err();
        assert!(err.contains("could not run"), "{err}");
        assert!(crate::config::load_oc_clk_verified(&cfg.0).is_none(), "not a pass");
        assert!(crate::config::load_oc_clk_failed(&cfg.0).is_none(), "and not a failure either: try again later");
    }

    #[test]
    fn a_verified_driver_still_refuses_an_offset_out_of_range() {
        let cfg = Cfg::new("range");
        crate::config::save_oc_clk_verified(&cfg.0, "620.70.06").unwrap();
        let mut rm = FakeRm::new();
        let err = gated_write(&mut rm, &cfg.0, "620.70.06", ClkDomain::Xbar, 2_000_000).unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert_eq!(rm.sets, 0, "nothing was sent");
    }

    #[test]
    fn a_measure_reads_the_domain_it_asked_for() {
        let mut rm = FakeRm::new();
        assert_eq!(measure(&mut rm, ClkDomain::Xbar), Ok(2_442_000));
        assert_eq!(measure(&mut rm, ClkDomain::Sys), Ok(2_292_000));
    }

    #[test]
    fn a_domain_without_an_offset_range_is_refused() {
        let d = vec![(DOMAIN_XBAR, DomainInfo { index: 1, min_khz: 0, max_khz: 0 })];
        assert!(find_domain(&d, ClkDomain::Xbar).is_err());
        assert!(find_domain(&d, ClkDomain::Sys).unwrap_err().contains("no SYS"));
    }

    #[test]
    fn the_write_check_passes_and_leaves_everything_as_it_was() {
        let mut rm = FakeRm::new();
        rm.offsets[1] = 50_000;
        assert_eq!(write_check(&mut rm), Check::Pass);
        assert_eq!(rm.offsets[1], 50_000);
        assert_eq!(rm.sets, 2, "the test write and its undo");
    }

    #[test]
    fn a_write_that_lands_on_another_domain_breaks_the_driver_and_is_repaired_or_reported() {
        let mut rm = FakeRm { misroute: Some((1, 2)), ..FakeRm::new() };
        match write_check(&mut rm) {
            Check::Broken(why) => assert!(why.contains("MAY BE ALTERED") || why.contains("read back"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_write_that_also_changes_a_neighbour_breaks_the_driver() {
        let mut rm = FakeRm { scribble: true, ..FakeRm::new() };
        assert!(matches!(write_check(&mut rm), Check::Broken(_)));
    }

    #[test]
    fn a_set_carries_only_the_mask_flags_type_and_offset() {
        let p = set_request(3, -20_000);
        let nonzero: Vec<usize> = (0..p.len()).filter(|&i| p[i] != 0).collect();
        let at = CTL_ENTRIES_AT + 3 * CTL_STRIDE;
        assert!(
            nonzero.iter().all(|&i| (4..8).contains(&i)
                || (8..12).contains(&i)
                || (at..at + 4).contains(&i)
                || offset_bytes(3).contains(&i)),
            "{nonzero:?}"
        );
        assert_eq!(rd_i32(&p, at + CTL_OFFSET_AT), Some(-20_000));
    }
}
