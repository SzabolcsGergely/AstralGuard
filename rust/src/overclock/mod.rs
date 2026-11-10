//! Overclock tab backend: an optional undervolt (per-point V/F curve offsets through NVAPI),
//! power limit and memory offset (NVML). Pure curve math lives in `curve` so it can be tested
//! without a GPU; `Overclocker` holds the apply/restore rules and is tested against `fake`.
//!
//! The rules, all of which exist so the user's card is never left in a state they did not ask
//! for:
//! - An unticked setting is not managed: never written, unless AstralGuard applied it before,
//!   in which case the value from before is written back once and the entry is dropped.
//! - The value from before is recorded in the `overclock.active` marker (next to
//!   `mitigation.active`) before the first write, and only then: a second apply, or a restart
//!   with the overclock still in place, must not record AstralGuard's own value as "before".
//! - Next to each value from before, the value AstralGuard applied. An untick writes the value
//!   from before only while the card still runs the applied one: after a reboot, or another
//!   tool's change, the card runs something AstralGuard did not set, and that is left alone.
//! - Nothing is written while the guard is limiting the GPU. The mitigator saved the power
//!   limit it found and restores exactly that, so a power limit written under it would be
//!   lost or, worse, restored over.
//! - Each setting stands alone: one refusal or driver error never stops the others.

pub mod curve;
pub mod gpu;

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use gpu::OcGpu;

use crate::logger;
use crate::protocol::{OcReport, OcResult, OcStatus, Settings};
use curve::{ab_to_nvml_mem, nvml_to_ab_mem, plan_undervolt, stock_mhz, RawPoint};

/// The driver's per-point offset range. ClkDomainsGetInfo reports it; this card reports
/// ±1000 MHz (ROG Astral RTX 5090 OC), so the constant is used instead of another driver call.
const MAX_CURVE_DELTA_MHZ: i32 = 1000;
/// The compatibility check's test write on the middle core point: 15 MHz.
/// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux): a small per-point
/// delta is not applied by the driver. A +5 MHz delta reads back in ClkVfPointsGetControl but
/// does not move the effective clock in ClkVfPointsGetStatus, while +15, +30 and +862 MHz move
/// it exactly (895 mV: 1965 + 862 = 2827.0 MHz reported). The threshold below which deltas are
/// ignored is not known; 15 MHz is a safe probe. The probe checks that the effective clock
/// moves by the probe delta, so it must be large enough to be applied.
const PROBE_DELTA_KHZ: i32 = 15_000;
/// How far an effective frequency may be off and still count as "moved by the probe delta" or
/// "did not move": 1 MHz.
const PROBE_FREQ_TOLERANCE_KHZ: i64 = 1000;

const UNDERVOLT: &str = "undervolt";
const POWER: &str = "power";
const MEMORY: &str = "memory";

pub fn marker_path() -> PathBuf {
    crate::config::data_dir().join("overclock.active")
}

/// What AstralGuard applied, as the value from before. A field is `Some` exactly while
/// AstralGuard manages that setting; with all three `None` the file is removed.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
struct Marker {
    subsystem: u32,
    #[serde(default)]
    power_before_w: Option<u32>,
    #[serde(default)]
    mem_before_nvml: Option<i32>,
    /// Every core point's delta before the undervolt (the undervolt writes them all).
    #[serde(default)]
    curve_before_khz: Option<Vec<(usize, i32)>>,
    /// What AstralGuard last wrote (or found already in place), saved before the write. Missing
    /// (a marker from an older build) means "not known": an untick then writes nothing.
    #[serde(default)]
    power_applied_w: Option<u32>,
    #[serde(default)]
    mem_applied_nvml: Option<i32>,
    #[serde(default)]
    curve_applied_khz: Option<Vec<(usize, i32)>>,
}

impl Marker {
    fn is_empty(&self) -> bool {
        self.power_before_w.is_none() && self.mem_before_nvml.is_none() && self.curve_before_khz.is_none()
    }

    /// "undervolt", "power", "memory": what AstralGuard applied and restores on untick.
    fn managed(&self) -> Vec<String> {
        [
            (UNDERVOLT, self.curve_before_khz.is_some()),
            (POWER, self.power_before_w.is_some()),
            (MEMORY, self.mem_before_nvml.is_some()),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| name.to_string())
        .collect()
    }
}

/// What the marker says AstralGuard manages on this card, without opening the GPU and without
/// touching the file (a marker from another card is left for `Overclocker::new` to judge).
pub fn managed_in_marker(path: &std::path::Path, subsystem: u32) -> Vec<String> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Marker>(&b).ok())
        .filter(|m| m.subsystem == subsystem)
        .map(|m| m.managed())
        .unwrap_or_default()
}

/// The marker for this card. A marker from another card describes a GPU that is not here
/// (swapped since); its values must not be written to this one, so it is dropped.
fn load_marker(path: &std::path::Path, subsystem: u32) -> Marker {
    let fresh = Marker { subsystem, ..Marker::default() };
    let Ok(bytes) = fs::read(path) else { return fresh };
    match serde_json::from_slice::<Marker>(&bytes) {
        Ok(m) if m.subsystem == subsystem => m,
        Ok(m) => {
            logger::warn(format!(
                "Overclock: dropping a marker from another card (0x{:08X}, this is 0x{subsystem:08X}); nothing restored",
                m.subsystem
            ));
            let _ = fs::remove_file(path);
            fresh
        }
        Err(e) => {
            // Without it the values from before are unknown; the next apply records the live
            // ones, which is all that can be done.
            logger::warn(format!("Overclock: unreadable marker {} ({e}); starting without it", path.display()));
            fresh
        }
    }
}

/// Written to a temporary file, synced and renamed into place, like config.ini: a crash
/// mid-write must not lose the values to restore.
fn save_marker(path: &std::path::Path, m: &Marker) -> Result<(), String> {
    let tmp = path.with_extension("active.tmp");
    if m.is_empty() {
        let _ = fs::remove_file(&tmp);
        return match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let bytes = serde_json::to_vec(m).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    use std::io::Write;
    let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn core_points(points: &[RawPoint]) -> &[RawPoint] {
    &points[..points.iter().position(|p| p.memory).unwrap_or(points.len())]
}

/// A setting that went through fine: written, already as configured (no call made), or not
/// restored because the card no longer runs what AstralGuard applied (no call made).
enum Done {
    Wrote(String),
    InEffect(String),
    LeftAsIs(String),
}

/// The untick's answer when the live value is not the one AstralGuard applied.
fn left_as_is(what: &str) -> Done {
    Done::LeftAsIs(format!("{what} changed since AstralGuard applied it; left as is"))
}

/// The result, and whether anything happened (a write, or a failure worth reporting).
fn outcome(setting: &str, r: Result<Done, String>) -> (OcResult, bool) {
    let (ok, message, acted) = match r {
        Ok(Done::Wrote(m)) => (true, m, true),
        Ok(Done::InEffect(m)) => (true, format!("{m} (already in effect)"), false),
        Ok(Done::LeftAsIs(m)) => (true, m, true),
        Err(m) => (false, m, true),
    };
    (OcResult { setting: setting.into(), ok, message }, acted)
}

/// How a compatibility probe ended.
enum Probe {
    Pass,
    /// A call failed, and the curve was read back unchanged: worth trying again later.
    Error(String),
    /// The write landed wrong, or the curve did not come back as it was: never again on this
    /// driver (`mismatch`, persisted as `oc_vf_failed_driver`). Without `mismatch` the curve could
    /// not be read back at the end: not trusted for this run, but not remembered either.
    Broken {
        why: String,
        mismatch: bool,
    },
}

/// A driver the check failed on, and why. `persist`: a wrong landing, for the service to save.
struct FailedDriver {
    version: String,
    why: String,
    persist: bool,
}

/// Every curve index, memory-domain points included, whose delta differs between `was` and
/// `now`. A point present on one side only counts as differing. The memory points matter: an
/// off-by-one layout sends a write to the last core point onto the first memory point.
fn changed_points(was: &[RawPoint], now: &[RawPoint]) -> Vec<usize> {
    (0..was.len().max(now.len()))
        .filter(|&i| was.get(i).map(|p| p.delta_khz) != now.get(i).map(|p| p.delta_khz))
        .collect()
}

/// The probe's independent check, from GetStatus rather than the control table it wrote:
/// the effective frequency at `probed` moved by the probe delta and no other point's moved
/// (each within 1 MHz). None when it did; otherwise the points that are off. Only points present
/// on both sides are compared; a length change is the delta comparison's to report.
fn effective_mismatch(was: &[RawPoint], now: &[RawPoint], probed: usize) -> Option<Vec<usize>> {
    let off: Vec<usize> = (0..was.len().min(now.len()))
        .filter(|&i| {
            let expect = was[i].freq_khz as i64 + if i == probed { PROBE_DELTA_KHZ as i64 } else { 0 };
            (now[i].freq_khz as i64 - expect).abs() > PROBE_FREQ_TOLERANCE_KHZ
        })
        .collect();
    (!off.is_empty()).then_some(off)
}

/// "point(s) 4, 6", plus the length change when there is one.
fn describe_change(was: &[RawPoint], now: &[RawPoint], diff: &[usize]) -> String {
    let mut d = format!("point(s) {}", list(diff));
    if was.len() != now.len() {
        d.push_str(&format!(" (the curve changed from {} to {} points)", was.len(), now.len()));
    }
    d
}

fn list(points: &[usize]) -> String {
    points.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

fn watts(s: &Settings) -> u32 {
    s.oc_power_w.round().max(0.0) as u32
}

fn mem_nvml(s: &Settings) -> i32 {
    ab_to_nvml_mem(s.oc_mem_offset_mhz.round() as i32)
}

fn plan(s: &Settings, points: &[RawPoint]) -> Result<curve::UvPlan, String> {
    plan_undervolt(points, s.oc_uv_from_mv, s.oc_uv_to_mv, s.oc_uv_target_mhz, MAX_CURVE_DELTA_MHZ)
}

pub struct Overclocker<G: OcGpu> {
    gpu: G,
    marker_path: PathBuf,
    marker: Marker,
    /// The driver version the compatibility check last passed on (persisted by the caller).
    verified_driver: Option<String>,
    /// The driver version the check failed on: not probed again on that driver.
    failed_driver: Option<FailedDriver>,
}

impl<G: OcGpu> Overclocker<G> {
    /// `verified_driver` and `failed_driver`: the driver versions the check passed and failed on
    /// before (persisted by the caller as `oc_vf_verified_driver` / `oc_vf_failed_driver`).
    pub fn new(gpu: G, marker: PathBuf, verified_driver: Option<String>, failed_driver: Option<String>) -> Self {
        let m = load_marker(&marker, gpu.subsystem());
        let failed_driver = failed_driver.map(|version| FailedDriver {
            why: format!(
                "the V/F curve write check failed on driver {version} before; undervolt disabled until a driver update"
            ),
            version,
            persist: true,
        });
        Overclocker { gpu, marker_path: marker, marker: m, verified_driver, failed_driver }
    }

    pub fn verified_driver(&self) -> Option<&str> {
        self.verified_driver.as_deref()
    }

    /// The driver version the check failed on with a wrong landing (for the caller to persist).
    pub fn failed_driver(&self) -> Option<&str> {
        self.failed_driver.as_ref().filter(|f| f.persist).map(|f| f.version.as_str())
    }

    pub fn subsystem(&self) -> u32 {
        self.gpu.subsystem()
    }

    /// What AstralGuard applied (from the marker in memory; no driver call).
    pub fn managed(&self) -> Vec<String> {
        self.marker.managed()
    }

    /// Write the ticked settings, restore the unticked ones AstralGuard applied. Writes nothing
    /// while the guard is limiting the GPU (`deferred`); `after_release` finishes it.
    pub fn apply(&mut self, s: &Settings, guard_engaged: bool) -> OcReport {
        if guard_engaged {
            logger::info("Overclock: not applied while the guard is limiting the GPU; waiting for release");
            return OcReport { results: Vec::new(), deferred: true };
        }
        let results = self.run(s).into_iter().map(|(r, _)| r).collect::<Vec<_>>();
        log_results("apply", &results);
        OcReport { results, deferred: false }
    }

    /// After the guard released the GPU: the same as `apply`, so an apply deferred during the
    /// trip (a new tick, an untick) takes effect now, and anything the release cleared is
    /// written again.
    ///
    /// VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux):
    /// nvmlDeviceSetGpuLockedClocks(180, 180) held for 2 s, then nvmlDeviceResetGpuLockedClocks,
    /// left all curve offsets and the memory offset unchanged. The re-apply here is a safety net.
    /// Reports only what was written or failed; a setting already as configured is not listed.
    pub fn after_release(&mut self, s: &Settings) -> OcReport {
        let mut results: Vec<OcResult> = self.run(s).into_iter().filter(|(_, acted)| *acted).map(|(r, _)| r).collect();
        for r in &mut results {
            r.message = format!("after the guard's release: {}", r.message);
        }
        log_results("after release", &results);
        OcReport { results, deferred: false }
    }

    /// The one path behind `apply` and `after_release`: ticked = ensure applied, unticked and
    /// managed = restore, unticked and not managed = no call at all.
    fn run(&mut self, s: &Settings) -> Vec<(OcResult, bool)> {
        let mut results = Vec::new();
        if s.oc_power_enabled {
            results.push(outcome(POWER, self.apply_power(s)));
        } else if let Some(before) = self.marker.power_before_w {
            results.push(outcome(POWER, self.restore_power(before, self.marker.power_applied_w)));
        }
        // VERIFIED (2026-11-10, ROG Astral RTX 5090 OC, driver 621.14, Windows): a memory offset
        // write resets every V/F curve delta to 0. So the curve is restored before the memory
        // step and applied after it, and a restored curve that had offsets of its own (another
        // tool's) is written again when the memory step wrote.
        let restored_curve = match self.marker.curve_before_khz.clone() {
            Some(before) if !s.oc_uv_enabled => {
                let applied = self.marker.curve_applied_khz.clone();
                let r = self.restore_curve(&before, applied.as_deref());
                let wrote = matches!(r, Ok(Done::Wrote(_)));
                results.push(outcome(UNDERVOLT, r));
                wrote.then_some(before)
            }
            _ => None,
        };
        // The curve from before the undervolt is recorded ahead of a memory write that would
        // wipe it (apply_undervolt keeps an entry already there).
        let mut prerecorded = false;
        if s.oc_uv_enabled && s.oc_mem_enabled && self.marker.curve_before_khz.is_none() {
            if let Ok(points) = self.gpu.curve() {
                let before: Vec<(usize, i32)> =
                    core_points(&points).iter().enumerate().map(|(i, pt)| (i, pt.delta_khz)).collect();
                match self.record(|m| m.curve_before_khz = Some(before)) {
                    Ok(()) => prerecorded = true,
                    Err(e) => logger::warn(format!("Overclock: {e}")),
                }
            }
        }
        let mem = if s.oc_mem_enabled {
            Some(self.apply_memory(s))
        } else {
            self.marker.mem_before_nvml.map(|before| self.restore_memory(before, self.marker.mem_applied_nvml))
        };
        let mem_wrote = matches!(mem, Some(Ok(Done::Wrote(_))));
        if let Some(r) = mem {
            results.push(outcome(MEMORY, r));
        }
        if s.oc_uv_enabled {
            let r = self.apply_undervolt(s);
            // Failed before it recorded what it applies (the check, say): nothing of the curve
            // is AstralGuard's, so the early entry goes again.
            if r.is_err() && prerecorded && self.marker.curve_applied_khz.is_none() {
                self.forget(|m| m.curve_before_khz = None);
            }
            results.push(outcome(UNDERVOLT, r));
        } else if let Some(before) = restored_curve.filter(|b| mem_wrote && b.iter().any(|&(_, d)| d != 0)) {
            if let Err(e) = self.gpu.set_curve(&before) {
                results.push(outcome(
                    UNDERVOLT,
                    Err(format!("could not restore the V/F curve after the memory change ({e})")),
                ));
            }
        }
        results
    }

    /// Everything the Overclock tab shows, read from the driver. Reads only.
    pub fn status(&mut self, s: &Settings) -> OcStatus {
        let driver = self.gpu.driver_version();
        let mut st = OcStatus { available: driver.is_ok(), ..OcStatus::default() };
        if let Err(e) = &driver {
            st.reason = e.clone();
        }
        st.power_w = self.gpu.power_limit_w().ok();
        st.power_range_w = self.gpu.power_range_w().ok();
        if let Ok((cur, min, max)) = self.gpu.mem_offset() {
            st.mem_ab = Some(nvml_to_ab_mem(cur));
            st.mem_range_ab = Some((nvml_to_ab_mem(min), nvml_to_ab_mem(max)));
        }
        st.mem_clock_mhz = self.gpu.mem_clock_mhz().ok();
        st.foreign_core_offset_mhz = self.gpu.core_offset_mhz().unwrap_or(0);
        st.live_mv = self.gpu.core_voltage_mv().ok().map(|v| v as f32);
        if let Ok(points) = self.gpu.curve() {
            st.uv_offset_mhz = plan(s, &points).ok().map(|p| p.offset_mhz);
            let core = core_points(&points);
            let mv = |p: &RawPoint| p.volt_uv as f32 / 1000.0;
            st.stock_curve = core.iter().map(|p| (mv(p), stock_mhz(p) as f32)).collect();
            st.current_curve = core.iter().map(|p| (mv(p), p.freq_khz as f32 / 1000.0)).collect();
            st.live_mhz = st.live_mv.and_then(|live| {
                let at = core.iter().rev().find(|p| mv(p) <= live).or(core.first())?;
                Some(at.freq_khz as f32 / 1000.0)
            });
        }
        if let Ok(d) = &driver {
            st.curve_compatible = if self.verified_driver.as_deref() == Some(d.as_str()) {
                Some(true)
            } else if self.failed_driver.as_ref().is_some_and(|f| f.version == *d) {
                Some(false)
            } else {
                None
            };
        }
        st.managed = self.marker.managed();
        st
    }

    /// Record a value from before in the marker, durably, before the write it protects. On a
    /// failed save the write must not happen: after a crash there would be nothing to restore.
    fn record(&mut self, f: impl FnOnce(&mut Marker)) -> Result<(), String> {
        let prev = self.marker.clone();
        f(&mut self.marker);
        save_marker(&self.marker_path, &self.marker).map_err(|e| {
            self.marker = prev;
            format!("could not record the value to restore ({e}); nothing written")
        })
    }

    /// Drop an entry after its restore. A failed save leaves the old entry on disk, which
    /// would restore the same value again on a later untick: logged, not fatal.
    fn forget(&mut self, f: impl FnOnce(&mut Marker)) {
        f(&mut self.marker);
        if let Err(e) = save_marker(&self.marker_path, &self.marker) {
            logger::warn(format!("Overclock: could not update the marker ({e})"));
        }
    }

    fn apply_power(&mut self, s: &Settings) -> Result<Done, String> {
        let w = watts(s);
        let (lo, hi) = self.gpu.power_range_w().map_err(|e| format!("could not read the card's power range ({e})"))?;
        if !(lo..=hi).contains(&w) {
            return Err(format!("{w} W is outside the card's range {lo}-{hi} W"));
        }
        let live = self.gpu.power_limit_w().map_err(|e| format!("could not read the power limit ({e})"))?;
        if self.marker.power_before_w.is_none() || self.marker.power_applied_w != Some(w) {
            self.record(|m| {
                m.power_before_w.get_or_insert(live);
                m.power_applied_w = Some(w);
            })?;
        }
        if live == w {
            return Ok(Done::InEffect(format!("power limit {w} W")));
        }
        // On a failed write the entry stays: the value from before is still the right one to
        // restore, and the write may have half happened.
        self.gpu.set_power_limit_w(w).map_err(|e| format!("could not set the power limit ({e})"))?;
        Ok(Done::Wrote(format!("power limit {w} W")))
    }

    fn restore_power(&mut self, before: u32, applied: Option<u32>) -> Result<Done, String> {
        let live = self.gpu.power_limit_w().map_err(|e| format!("could not read the power limit ({e})"))?;
        if applied != Some(live) {
            self.forget(|m| (m.power_before_w, m.power_applied_w) = (None, None));
            return Ok(left_as_is(&format!("power limit ({live} W now)")));
        }
        self.gpu.set_power_limit_w(before).map_err(|e| format!("could not restore the power limit ({e})"))?;
        self.forget(|m| (m.power_before_w, m.power_applied_w) = (None, None));
        Ok(Done::Wrote(format!("power limit restored to {before} W")))
    }

    fn apply_memory(&mut self, s: &Settings) -> Result<Done, String> {
        let want = mem_nvml(s);
        let (cur, min, max) = self.gpu.mem_offset().map_err(|e| format!("could not read the memory offset ({e})"))?;
        if !(min..=max).contains(&want) {
            return Err(format!(
                "{:+} MHz is outside the card's range {:+}..{:+} MHz",
                nvml_to_ab_mem(want),
                nvml_to_ab_mem(min),
                nvml_to_ab_mem(max)
            ));
        }
        if self.marker.mem_before_nvml.is_none() || self.marker.mem_applied_nvml != Some(want) {
            self.record(|m| {
                m.mem_before_nvml.get_or_insert(cur);
                m.mem_applied_nvml = Some(want);
            })?;
        }
        let msg = format!("memory offset {:+} MHz", nvml_to_ab_mem(want));
        if cur == want {
            return Ok(Done::InEffect(msg));
        }
        self.gpu.set_mem_offset(want).map_err(|e| format!("could not set the memory offset ({e})"))?;
        Ok(Done::Wrote(msg))
    }

    fn restore_memory(&mut self, before: i32, applied: Option<i32>) -> Result<Done, String> {
        let (live, _, _) = self.gpu.mem_offset().map_err(|e| format!("could not read the memory offset ({e})"))?;
        if applied != Some(live) {
            self.forget(|m| (m.mem_before_nvml, m.mem_applied_nvml) = (None, None));
            return Ok(left_as_is(&format!("memory offset ({:+} MHz now)", nvml_to_ab_mem(live))));
        }
        self.gpu.set_mem_offset(before).map_err(|e| format!("could not restore the memory offset ({e})"))?;
        self.forget(|m| (m.mem_before_nvml, m.mem_applied_nvml) = (None, None));
        Ok(Done::Wrote(format!("memory offset restored to {:+} MHz", nvml_to_ab_mem(before))))
    }

    fn apply_undervolt(&mut self, s: &Settings) -> Result<Done, String> {
        // Not subtracted and not zeroed: how it stacks with the curve is unverified, and
        // zeroing it would change a value the user did not ask AstralGuard to touch.
        // VERIFIED (2026-11-09, ROG Astral RTX 5090 OC, driver 620.70.06, Linux): with the curve
        // applied through NVAPI per-point deltas, NVML's graphics clock offset still reads +0 on
        // every P-state, so AstralGuard's own curve is not mistaken for a foreign offset.
        let core = self.gpu.core_offset_mhz().map_err(|e| format!("could not read the core clock offset ({e})"))?;
        if core != 0 {
            return Err(format!("a core clock offset of {core:+} MHz is set outside AstralGuard; remove it first"));
        }
        let points = self.gpu.curve().map_err(|e| format!("could not read the V/F curve ({e})"))?;
        let p = plan(s, &points)?;
        let msg = format!(
            "{:+} MHz on {}-{} mV, capped at {} MHz",
            p.offset_mhz, s.oc_uv_from_mv, s.oc_uv_to_mv, s.oc_uv_target_mhz
        );
        let in_place = p.deltas_khz.iter().all(|&(i, d)| points.get(i).is_some_and(|pt| pt.delta_khz == d));
        if !in_place {
            self.check_curve_writes(&points)?;
        }
        if self.marker.curve_before_khz.is_none() || self.marker.curve_applied_khz.as_ref() != Some(&p.deltas_khz) {
            let before: Vec<(usize, i32)> =
                core_points(&points).iter().enumerate().map(|(i, pt)| (i, pt.delta_khz)).collect();
            let applied = p.deltas_khz.clone();
            self.record(|m| {
                m.curve_before_khz.get_or_insert(before);
                m.curve_applied_khz = Some(applied);
            })?;
        }
        if in_place {
            return Ok(Done::InEffect(msg));
        }
        self.gpu.set_curve(&p.deltas_khz).map_err(|e| format!("could not write the V/F curve ({e})"))?;
        Ok(Done::Wrote(msg))
    }

    fn restore_curve(&mut self, before: &[(usize, i32)], applied: Option<&[(usize, i32)]>) -> Result<Done, String> {
        let points = self.gpu.curve().map_err(|e| format!("could not read the V/F curve ({e})"))?;
        let core = core_points(&points).len();
        let drop_entry = |m: &mut Marker| (m.curve_before_khz, m.curve_applied_khz) = (None, None);
        // The recorded indices mean the curve as it was then. A curve whose core part has another
        // length (a driver update, say) may put a memory point at one of them: never written.
        let recorded = before.len();
        if recorded != core || before.iter().chain(applied.unwrap_or_default()).any(|&(i, _)| i >= core) {
            self.forget(drop_entry);
            return Err(format!(
                "the V/F curve now has {core} core points, {recorded} when AstralGuard applied the undervolt; \
                 not restored (the offsets stay until the next reboot or driver reset)"
            ));
        }
        let still_applied =
            applied.is_some_and(|a| a.iter().all(|&(i, d)| points.get(i).is_some_and(|pt| pt.delta_khz == d)));
        if !still_applied {
            self.forget(drop_entry);
            return Ok(left_as_is("V/F curve"));
        }
        // A restore is a curve write too: on a driver whose layout changed it could land anywhere.
        self.check_curve_writes(&points)?;
        self.gpu.set_curve(before).map_err(|e| format!("could not restore the V/F curve ({e})"))?;
        self.forget(drop_entry);
        Ok(Done::Wrote("V/F curve restored".into()))
    }

    /// The curve layouts are undocumented, so before the first curve write on a driver
    /// version, prove that a write lands where it should. A failed call with the curve intact
    /// is an error for this apply only; a wrong landing, or a curve that did not come back as
    /// it was, fails the driver for this run.
    fn check_curve_writes(&mut self, points: &[RawPoint]) -> Result<(), String> {
        let driver = self.gpu.driver_version().map_err(|e| format!("could not read the driver version ({e})"))?;
        if self.verified_driver.as_deref() == Some(driver.as_str()) {
            return Ok(());
        }
        if let Some(f) = self.failed_driver.as_ref().filter(|f| f.version == driver) {
            return Err(f.why.clone());
        }
        match self.probe(points) {
            Probe::Pass => {
                logger::info(format!("Overclock: V/F curve writes verified on driver {driver}"));
                self.verified_driver = Some(driver);
                Ok(())
            }
            Probe::Error(e) => Err(format!("the V/F curve write check could not run ({e})")),
            Probe::Broken { why, mismatch } => {
                let why = format!("the V/F curve write check failed on driver {driver}: {why}; undervolt disabled");
                logger::error(format!("Overclock: {why}"));
                self.failed_driver = Some(FailedDriver { version: driver, why: why.clone(), persist: mismatch });
                Err(why)
            }
        }
    }

    /// One driver step (15 MHz) on the middle core point, read back, the original delta written back; then, in
    /// every case, the whole curve is compared with `points` (the curve before the probe).
    /// The check exists for writes that land in the wrong place, and the undo lands in the
    /// same wrong place, so the undo alone proves nothing. The read-back must show the delta in
    /// the control table AND the effective frequency (GetStatus, another structure) moved, at
    /// that point only: a write that lands in a field that merely reads back as the delta fails.
    /// The middle point, not the top one: the top may sit at a clock ceiling that hides the step.
    fn probe(&mut self, points: &[RawPoint]) -> Probe {
        let core = core_points(points).len();
        if core == 0 {
            return Probe::Error("the curve has no core points".into());
        }
        let probed = core / 2;
        let orig = points[probed].delta_khz;
        let mut problem: Option<String> = None;
        let mut landed_wrong = false;
        // A failed write gets no undo of its own: it may have half happened, and the
        // comparison below repairs whatever it did.
        match self.gpu.set_curve(&[(probed, orig + PROBE_DELTA_KHZ)]) {
            Err(e) => problem = Some(format!("test write failed ({e})")),
            Ok(()) => {
                match self.gpu.curve() {
                    Err(e) => problem = Some(format!("test read-back failed ({e})")),
                    Ok(after) => {
                        let mut expect = points.to_vec();
                        expect[probed].delta_khz = orig + PROBE_DELTA_KHZ;
                        let wrong = changed_points(&expect, &after);
                        if !wrong.is_empty() {
                            landed_wrong = true;
                            problem = Some(format!(
                                "a +{PROBE_DELTA_KHZ} kHz write to point {probed} read back wrong at {}",
                                describe_change(&expect, &after, &wrong)
                            ));
                        } else if let Some(off) = effective_mismatch(points, &after, probed) {
                            landed_wrong = true;
                            problem = Some(format!(
                                "a +{PROBE_DELTA_KHZ} kHz write to point {probed} read back as written, but the effective \
                                 clock did not move by it at point {probed} only (off at point(s) {})",
                                list(&off)
                            ));
                        }
                    }
                }
                if let Err(e) = self.gpu.set_curve(&[(probed, orig)]) {
                    problem.get_or_insert(format!("could not undo the test write ({e})"));
                }
            }
        }
        match self.restore_probe_points(points, probed) {
            None => match problem {
                None => Probe::Pass,
                Some(why) if landed_wrong => Probe::Broken { why, mismatch: true },
                Some(p) => Probe::Error(p),
            },
            Some((damage, mismatch)) => {
                let p = problem.unwrap_or_else(|| "the test write did not undo cleanly".into());
                Probe::Broken { why: format!("{p}; {damage}"), mismatch: mismatch || landed_wrong }
            }
        }
    }

    /// Make the curve what it was before the probe. None if it already was; otherwise which
    /// points differed and whether writing their original deltas back fixed them, and whether a
    /// difference was seen (false: the curve could not be read back at all).
    fn restore_probe_points(&mut self, points: &[RawPoint], probed: usize) -> Option<(String, bool)> {
        let now = match self.gpu.curve() {
            Ok(c) => c,
            Err(e) => {
                return Some((format!(
                    "the curve could not be read back ({e}): point {probed} MAY STILL CARRY the +{PROBE_DELTA_KHZ} kHz test offset"
                ), false))
            }
        };
        let diff = changed_points(points, &now);
        if diff.is_empty() {
            return None;
        }
        // A point on one side only cannot be put back: one that vanished cannot be written, one
        // that appeared has no original. Those are reported, the rest written back once.
        let (fix, one_side): (Vec<usize>, Vec<usize>) = diff.iter().partition(|&&i| i < points.len() && i < now.len());
        let fix: Vec<(usize, i32)> = fix.iter().map(|&i| (i, points[i].delta_khz)).collect();
        let written = fix.is_empty() || self.gpu.set_curve(&fix).is_ok();
        let fixed = written && self.gpu.curve().is_ok_and(|c| changed_points(points, &c).is_empty());
        let what = describe_change(points, &now, &diff);
        let mut damage = if fixed {
            format!("the probe changed curve {what}; their original offsets were written back and read back correctly")
        } else {
            format!("THE V/F CURVE MAY BE ALTERED at {what}: writing the original offsets back did not restore them")
        };
        if !one_side.is_empty() {
            damage
                .push_str(&format!("; point(s) {} exist on one side only and cannot be written back", list(&one_side)));
        }
        logger::error(format!("Overclock: {damage}"));
        Some((damage, true))
    }
}

fn log_results(when: &str, results: &[OcResult]) {
    for r in results {
        let line = format!("Overclock ({when}): {}: {}", r.setting, r.message);
        if r.ok {
            logger::info(line);
        } else {
            logger::warn(line);
        }
    }
}

/// A GPU in memory for the Overclocker's tests (and the service's): the owner's ranges and
/// curve, every set call recorded. Clones share one state, so a test keeps a handle to look
/// at what the Overclocker did.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex, MutexGuard};

    use super::curve::RawPoint;
    use super::OcGpu;

    pub(crate) const OWNERS_SUBSYSTEM: u32 = 0x89E31043;
    pub(crate) const OWNERS_DRIVER: &str = "620.70.06";

    pub(crate) struct State {
        pub subsystem: u32,
        pub driver: String,
        pub power_w: u32,
        pub power_range_w: (u32, u32),
        /// (current, min, max), NVML MHz.
        pub mem: (i32, i32, i32),
        pub core_offset_mhz: i32,
        pub curve: Vec<RawPoint>,
        /// A driver whose curve layout does not match: writes are accepted but change nothing.
        pub ignore_curve_writes: bool,
        /// A driver whose layout is off for one point: a write to curve index `.0` lands on `.1`.
        pub misroute: Option<(usize, usize)>,
        /// A driver whose layout changes under a write: the next curve write also drops the last point.
        pub drop_last_point_on_next_write: bool,
        /// A driver whose control layout moved: the write lands in a field that reads back as the
        /// delta, but the effective frequency (GetStatus) does not change.
        pub delta_not_effective: bool,
        /// Every curve write fails with an error and changes nothing (a plain call error).
        pub curve_write_error: bool,
        /// The Windows driver: a memory offset write resets every curve delta to 0.
        pub mem_write_resets_curve: bool,
        pub live_mv: f64,
        /// Every set call, e.g. "set_power_limit_w(520)".
        pub calls: Vec<String>,
    }

    #[derive(Clone)]
    pub(crate) struct FakeGpu(Arc<Mutex<State>>);

    fn pt(mv: u32, mhz: u32) -> RawPoint {
        RawPoint { freq_khz: mhz * 1000, volt_uv: mv * 1000, delta_khz: 0, memory: false }
    }

    /// The owner's stock curve (as in `curve`'s tests), plus one memory-domain point.
    pub(crate) fn owners_curve() -> Vec<RawPoint> {
        let mut v = vec![pt(810, 757), pt(815, 847), pt(820, 930), pt(895, 1965), pt(900, 2047), pt(1240, 3232)];
        v.push(RawPoint { freq_khz: 14_000_000, volt_uv: 1_240_000, delta_khz: 0, memory: true });
        v
    }

    impl FakeGpu {
        /// The owner's card: 400-600 W at 600 W, NVML memory -2000..6000 at 0, stock curve.
        pub(crate) fn owners() -> FakeGpu {
            FakeGpu(Arc::new(Mutex::new(State {
                subsystem: OWNERS_SUBSYSTEM,
                driver: OWNERS_DRIVER.into(),
                power_w: 600,
                power_range_w: (400, 600),
                mem: (0, -2000, 6000),
                core_offset_mhz: 0,
                curve: owners_curve(),
                ignore_curve_writes: false,
                misroute: None,
                drop_last_point_on_next_write: false,
                delta_not_effective: false,
                curve_write_error: false,
                mem_write_resets_curve: false,
                live_mv: 880.0,
                calls: Vec::new(),
            })))
        }

        pub(crate) fn state(&self) -> MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.state().calls.clone()
        }

        pub(crate) fn clear_calls(&self) {
            self.state().calls.clear();
        }

        /// What a guard release might do to the curve: every delta back to 0.
        pub(crate) fn clear_curve_deltas(&self) {
            for p in &mut self.state().curve {
                p.freq_khz = (p.freq_khz as i64 - p.delta_khz as i64) as u32;
                p.delta_khz = 0;
            }
        }

        pub(crate) fn delta_khz(&self, index: usize) -> i32 {
            self.state().curve[index].delta_khz
        }
    }

    impl OcGpu for FakeGpu {
        fn subsystem(&self) -> u32 {
            self.state().subsystem
        }
        fn driver_version(&mut self) -> Result<String, String> {
            Ok(self.state().driver.clone())
        }
        fn power_limit_w(&mut self) -> Result<u32, String> {
            Ok(self.state().power_w)
        }
        fn power_range_w(&mut self) -> Result<(u32, u32), String> {
            Ok(self.state().power_range_w)
        }
        fn set_power_limit_w(&mut self, w: u32) -> Result<(), String> {
            let mut s = self.state();
            s.calls.push(format!("set_power_limit_w({w})"));
            s.power_w = w;
            Ok(())
        }
        fn mem_offset(&mut self) -> Result<(i32, i32, i32), String> {
            Ok(self.state().mem)
        }
        fn set_mem_offset(&mut self, nvml_mhz: i32) -> Result<(), String> {
            let mut s = self.state();
            s.calls.push(format!("set_mem_offset({nvml_mhz})"));
            s.mem.0 = nvml_mhz;
            if s.mem_write_resets_curve {
                for p in s.curve.iter_mut().filter(|p| !p.memory) {
                    p.freq_khz = (p.freq_khz as i64 - p.delta_khz as i64) as u32;
                    p.delta_khz = 0;
                }
            }
            Ok(())
        }
        fn mem_clock_mhz(&mut self) -> Result<u32, String> {
            // stock 14001 MHz; the offset is NVML units, the clock moves by half of it
            Ok((14001 + self.state().mem.0 / 2) as u32)
        }
        fn core_offset_mhz(&mut self) -> Result<i32, String> {
            Ok(self.state().core_offset_mhz)
        }
        fn curve(&mut self) -> Result<Vec<RawPoint>, String> {
            Ok(self.state().curve.clone())
        }
        fn set_curve(&mut self, deltas_khz: &[(usize, i32)]) -> Result<(), String> {
            let mut s = self.state();
            s.calls.push(format!("set_curve({deltas_khz:?})"));
            if s.curve_write_error {
                return Err("NVAPI_ERROR".into());
            }
            if std::mem::take(&mut s.drop_last_point_on_next_write) {
                s.curve.pop();
            }
            if s.ignore_curve_writes {
                return Ok(());
            }
            for &(i, d) in deltas_khz {
                let i = match s.misroute {
                    Some((from, to)) if from == i => to,
                    _ => i,
                };
                let effective = !s.delta_not_effective;
                let p = s.curve.get_mut(i).ok_or_else(|| format!("no curve point {i}"))?;
                if effective {
                    p.freq_khz = (p.freq_khz as i64 - p.delta_khz as i64 + d as i64) as u32;
                }
                p.delta_khz = d;
            }
            Ok(())
        }
        fn core_voltage_mv(&mut self) -> Result<f64, String> {
            Ok(self.state().live_mv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeGpu, OWNERS_DRIVER};
    use super::*;

    fn marker(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ag-oc-marker-{}-{name}.active", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn oc(gpu: &FakeGpu, marker: &std::path::Path) -> Overclocker<FakeGpu> {
        Overclocker::new(gpu.clone(), marker.to_path_buf(), Some(OWNERS_DRIVER.into()), None)
    }

    /// The owner's profile (the defaults: 820-895 mV to 2827 MHz, 520 W, +2000) with the given ticks.
    fn settings(uv: bool, power: bool, mem: bool) -> Settings {
        Settings { oc_uv_enabled: uv, oc_power_enabled: power, oc_mem_enabled: mem, ..Settings::default() }
    }

    fn result<'a>(r: &'a OcReport, setting: &str) -> &'a crate::protocol::OcResult {
        r.results.iter().find(|x| x.setting == setting).unwrap_or_else(|| panic!("no {setting} result in {r:?}"))
    }

    #[test]
    fn status_reports_the_memory_clock_with_the_offset() {
        let (gpu, m) = (FakeGpu::owners(), marker("mem-clock"));
        let mut o = oc(&gpu, &m);
        assert_eq!(o.status(&settings(false, false, false)).mem_clock_mhz, Some(14001));
        o.apply(&settings(false, false, true), false);
        assert_eq!(o.status(&settings(false, false, true)).mem_clock_mhz, Some(16001));
    }

    #[test]
    fn applies_owners_profile() {
        let (gpu, m) = (FakeGpu::owners(), marker("profile"));
        let r = oc(&gpu, &m).apply(&settings(true, true, true), false);
        assert!(!r.deferred);
        assert!(r.results.iter().all(|x| x.ok), "{r:?}");
        assert_eq!(r.results.len(), 3);
        let calls = gpu.calls();
        assert!(calls.contains(&"set_power_limit_w(520)".to_string()), "{calls:?}");
        assert!(calls.contains(&"set_mem_offset(4000)".to_string()), "{calls:?}");
        // Driver already verified: no probe, one curve write.
        assert_eq!(calls.iter().filter(|c| c.starts_with("set_curve")).count(), 1, "{calls:?}");
        assert_eq!(gpu.delta_khz(0), 0);
        assert_eq!(gpu.delta_khz(1), 0);
        assert_eq!(gpu.delta_khz(2), 862_000); // 820 mV
        assert_eq!(gpu.delta_khz(3), 862_000); // 895 mV
        assert_eq!(gpu.delta_khz(4), 780_000);
        assert_eq!(gpu.delta_khz(5), -405_000);
        assert_eq!(gpu.delta_khz(6), 0, "the memory point is never written");
        assert!(result(&r, "undervolt").message.contains("+862 MHz"), "{r:?}");
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn unticked_settings_are_never_touched() {
        let (gpu, m) = (FakeGpu::owners(), marker("unticked"));
        let r = oc(&gpu, &m).apply(&settings(false, false, false), false);
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        assert!(r.results.is_empty(), "{r:?}");
        assert!(!m.exists());
    }

    #[test]
    fn untick_restores_only_what_was_applied() {
        let (gpu, m) = (FakeGpu::owners(), marker("untick"));
        let mut o = oc(&gpu, &m);
        assert!(o.apply(&settings(false, true, false), false).results[0].ok);
        assert_eq!(o.status(&settings(false, true, false)).managed, vec!["power".to_string()]);
        gpu.clear_calls();
        let r = o.apply(&settings(false, false, false), false);
        assert_eq!(gpu.calls(), vec!["set_power_limit_w(600)".to_string()]);
        assert!(result(&r, "power").ok, "{r:?}");
        assert_eq!(r.results.len(), 1, "{r:?}");
        assert!(!m.exists(), "nothing is managed any more");
    }

    #[test]
    fn second_apply_keeps_the_original_before_values() {
        let (gpu, m) = (FakeGpu::owners(), marker("second"));
        oc(&gpu, &m).apply(&settings(false, true, false), false);
        // A restart: a new Overclocker on the same marker, the card still at 520 W.
        let mut o = oc(&gpu, &m);
        o.apply(&settings(false, true, false), false);
        gpu.clear_calls();
        o.apply(&settings(false, false, false), false);
        assert_eq!(gpu.calls(), vec!["set_power_limit_w(600)".to_string()]);
    }

    #[test]
    fn deferred_while_the_guard_is_limiting() {
        let (gpu, m) = (FakeGpu::owners(), marker("deferred"));
        let r = oc(&gpu, &m).apply(&settings(true, true, true), true);
        assert!(r.deferred);
        assert!(r.results.is_empty(), "{r:?}");
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        assert!(!m.exists());
    }

    #[test]
    fn an_out_of_range_power_limit_is_refused_others_apply() {
        let (gpu, m) = (FakeGpu::owners(), marker("range"));
        gpu.state().power_range_w = (400, 450);
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(false, true, true), false);
        assert!(!result(&r, "power").ok, "{r:?}");
        assert!(result(&r, "memory").ok, "{r:?}");
        assert_eq!(gpu.calls(), vec!["set_mem_offset(4000)".to_string()]);
        assert_eq!(o.status(&settings(false, true, true)).managed, vec!["memory".to_string()]);
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn foreign_core_offset_blocks_only_the_undervolt() {
        let (gpu, m) = (FakeGpu::owners(), marker("foreign"));
        gpu.state().core_offset_mhz = 150;
        let r = oc(&gpu, &m).apply(&settings(true, true, false), false);
        let uv = result(&r, "undervolt");
        assert!(!uv.ok && uv.message.contains("+150 MHz"), "{uv:?}");
        assert!(result(&r, "power").ok, "{r:?}");
        assert_eq!(gpu.calls(), vec!["set_power_limit_w(520)".to_string()]);
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn compatibility_check_reruns_on_a_new_driver_and_failure_blocks_the_curve() {
        let (gpu, m) = (FakeGpu::owners(), marker("compat-fail"));
        gpu.state().driver = "621.01".into();
        gpu.state().ignore_curve_writes = true;
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(true, false, false), false);
        assert!(!result(&r, "undervolt").ok, "{r:?}");
        // The probe (one driver step on the middle core point) and its restore, nothing else.
        assert_eq!(
            gpu.calls(),
            vec![format!("set_curve([(3, {PROBE_DELTA_KHZ})])"), "set_curve([(3, 0)])".to_string()]
        );
        assert_eq!(o.verified_driver(), Some(OWNERS_DRIVER));
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
        assert!(!m.exists(), "a refused undervolt records nothing");
    }

    #[test]
    fn compatibility_check_passes_and_is_remembered() {
        let (gpu, m) = (FakeGpu::owners(), marker("compat-pass"));
        gpu.state().driver = "621.01".into();
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(true, false, false), false);
        assert!(result(&r, "undervolt").ok, "{r:?}");
        assert_eq!(o.verified_driver(), Some("621.01"));
        let calls = gpu.calls();
        assert_eq!(calls[..2], [format!("set_curve([(3, {PROBE_DELTA_KHZ})])"), "set_curve([(3, 0)])".to_string()]);
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(gpu.delta_khz(3), 862_000);
        // Already in place: no call at all.
        gpu.clear_calls();
        assert!(result(&o.apply(&settings(true, false, false), false), "undervolt").ok);
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        // Cleared (say by a driver reset): written again, without a second probe on the same driver.
        gpu.clear_curve_deltas();
        o.apply(&settings(true, false, false), false);
        assert_eq!(gpu.calls().len(), 1, "no second probe on the same driver: {:?}", gpu.calls());
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn a_probe_that_lands_on_the_wrong_point_leaves_the_curve_as_it_was() {
        let (gpu, m) = (FakeGpu::owners(), marker("misroute"));
        {
            let mut st = gpu.state();
            st.driver = "621.01".into();
            st.curve[4].delta_khz = 20_000; // another tool's offset, so the misrouted undo cannot fix it by luck
            st.curve[4].freq_khz += 20_000;
            st.misroute = Some((3, 4));
        }
        let before = gpu.state().curve.clone();
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(true, false, false), false);
        let uv = result(&r, "undervolt");
        assert!(!uv.ok, "{uv:?}");
        assert!(uv.message.contains("point(s) 4"), "says which point: {uv:?}");
        assert!(uv.message.contains("written back"), "says it was repaired: {uv:?}");
        assert_eq!(gpu.state().curve, before, "the curve ends as it was before the probe");
        assert!(!m.exists(), "a refused undervolt records nothing");
        // Cached as a failure: not probed again on this driver.
        gpu.clear_calls();
        assert!(!result(&o.apply(&settings(true, false, false), false), "undervolt").ok);
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
    }

    #[test]
    fn a_probe_misrouted_to_the_memory_point_leaves_the_curve_as_it_was() {
        let (gpu, m) = (FakeGpu::owners(), marker("misroute-mem"));
        {
            let mut st = gpu.state();
            st.driver = "621.01".into();
            st.curve[6].delta_khz = 50_000; // the memory-domain point, with its own offset
            st.curve[6].freq_khz += 50_000;
            st.misroute = Some((3, 6)); // the probed point's writes hit the memory point
        }
        let before = gpu.state().curve.clone();
        let mut o = oc(&gpu, &m);
        let uv = o.apply(&settings(true, false, false), false).results.remove(0);
        assert!(!uv.ok, "{uv:?}");
        assert!(uv.message.contains("point(s) 6"), "names the memory point: {uv:?}");
        assert!(uv.message.contains("written back"), "{uv:?}");
        assert_eq!(gpu.state().curve, before, "the memory point has its own delta back");
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
        assert!(!m.exists());
    }

    /// The control read-back looks right, but the driver did not apply it: GetStatus's effective
    /// frequency did not move. The same-structure read-back alone cannot see this.
    #[test]
    fn a_probe_whose_write_does_not_change_the_effective_clock_fails() {
        let (gpu, m) = (FakeGpu::owners(), marker("not-effective"));
        gpu.state().driver = "621.01".into();
        gpu.state().delta_not_effective = true;
        let before = gpu.state().curve.clone();
        let mut o = oc(&gpu, &m);
        let uv = o.apply(&settings(true, false, true), false).results.into_iter().find(|r| r.setting == UNDERVOLT);
        let uv = uv.expect("an undervolt result");
        assert!(!uv.ok, "{uv:?}");
        assert!(uv.message.contains("effective"), "says what did not move: {uv:?}");
        let curve_calls: Vec<String> = gpu.calls().into_iter().filter(|c| c.starts_with("set_curve")).collect();
        assert_eq!(
            curve_calls,
            vec![format!("set_curve([(3, {PROBE_DELTA_KHZ})])"), "set_curve([(3, 0)])".to_string()]
        );
        assert_eq!(gpu.state().curve, before);
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
        assert_eq!(o.managed(), vec![MEMORY.to_string()], "only the memory offset applied");
        let _ = std::fs::remove_file(&m);
    }

    /// Another point's effective clock moving under the probe is a wrong landing too.
    #[test]
    fn a_probe_that_moves_another_points_clock_fails() {
        assert_eq!(effective_mismatch(&fake::owners_curve(), &fake::owners_curve(), 3), Some(vec![3]));
        let mut after = fake::owners_curve();
        after[3].freq_khz += PROBE_DELTA_KHZ as u32;
        assert_eq!(effective_mismatch(&fake::owners_curve(), &after, 3), None);
        after[3].freq_khz += 1000; // within the 1 MHz tolerance
        assert_eq!(effective_mismatch(&fake::owners_curve(), &after, 3), None);
        after[5].freq_khz += 15_000;
        assert_eq!(effective_mismatch(&fake::owners_curve(), &after, 3), Some(vec![5]));
    }

    /// A wrong landing is reported for persisting (`oc_vf_failed_driver`); a plain call error is not.
    #[test]
    fn only_a_wrong_landing_marks_the_driver_failed() {
        let (gpu, m) = (FakeGpu::owners(), marker("failed-persist"));
        gpu.state().driver = "621.01".into();
        gpu.state().curve_write_error = true;
        let mut o = oc(&gpu, &m);
        assert!(!result(&o.apply(&settings(true, false, false), false), UNDERVOLT).ok);
        assert_eq!(o.failed_driver(), None, "a call error is retried later, not remembered");
        gpu.state().curve_write_error = false;
        gpu.state().delta_not_effective = true;
        assert!(!result(&o.apply(&settings(true, false, false), false), UNDERVOLT).ok);
        assert_eq!(o.failed_driver(), Some("621.01"));
    }

    /// A driver the check failed on in an earlier run: no test write at all, undervolt refused.
    #[test]
    fn a_persisted_failed_driver_skips_the_probe_and_refuses_the_undervolt() {
        let (gpu, m) = (FakeGpu::owners(), marker("failed-loaded"));
        gpu.state().driver = "621.01".into();
        let mut o = Overclocker::new(gpu.clone(), m.clone(), Some(OWNERS_DRIVER.into()), Some("621.01".into()));
        let uv = result(&o.apply(&settings(true, false, false), false), UNDERVOLT).clone();
        assert!(!uv.ok && uv.message.contains("621.01"), "{uv:?}");
        assert!(gpu.calls().is_empty(), "no probe: {:?}", gpu.calls());
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
        assert!(!m.exists());
        // A driver update clears it: the probe runs again.
        gpu.state().driver = "622.00".into();
        assert!(result(&o.apply(&settings(true, false, false), false), UNDERVOLT).ok);
        assert_eq!(o.verified_driver(), Some("622.00"));
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn a_curve_that_changes_length_under_the_probe_is_reported() {
        let (gpu, m) = (FakeGpu::owners(), marker("length"));
        gpu.state().driver = "621.01".into();
        gpu.state().drop_last_point_on_next_write = true;
        let mut o = oc(&gpu, &m);
        let uv = o.apply(&settings(true, false, false), false).results.remove(0);
        assert!(!uv.ok, "{uv:?}");
        assert!(uv.message.contains("from 7 to 6 points"), "names the length change: {uv:?}");
        assert!(uv.message.contains("point(s) 6"), "{uv:?}");
        assert!(uv.message.contains("cannot be written back"), "{uv:?}");
        assert!(!uv.message.contains("point(s) ;"), "never an empty point list: {uv:?}");
        assert_eq!(gpu.delta_khz(3), 0, "the probe's step is undone");
        assert_eq!(o.status(&settings(true, false, false)).curve_compatible, Some(false));
        assert!(!m.exists());
    }

    #[test]
    fn a_marker_from_another_card_is_ignored() {
        let (gpu, m) = (FakeGpu::owners(), marker("other-card"));
        let other = format!(
            r#"{{"subsystem":{},"power_before_w":450,"mem_before_nvml":1000,"curve_before_khz":[[3,100000]]}}"#,
            0x89FF1043u32
        );
        std::fs::write(&m, other).unwrap();
        let r = oc(&gpu, &m).apply(&settings(false, false, false), false);
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        assert!(r.results.is_empty(), "{r:?}");
    }

    /// The Windows driver resets the curve on every memory offset write. Ticking all three
    /// still ends with the undervolt in place, and unticking all three ends exactly at stock,
    /// with the curve restored, not "changed since AstralGuard applied it".
    #[test]
    fn memory_write_that_resets_the_curve_keeps_apply_and_untick_exact() {
        let (gpu, m) = (FakeGpu::owners(), marker("mem-resets-curve"));
        gpu.state().mem_write_resets_curve = true;
        let stock = gpu.state().curve.clone();
        let mut o = oc(&gpu, &m);
        o.apply(&settings(true, true, true), false);
        assert_ne!(gpu.delta_khz(3), 0, "undervolt in place after the memory write");
        let r = o.apply(&settings(false, false, false), false);
        let uv = result(&r, "undervolt");
        assert!(uv.ok && uv.message.contains("restored"), "{r:?}");
        assert_eq!(gpu.state().curve, stock);
        assert_eq!(gpu.state().mem.0, 0);
        assert!(!m.exists());
    }

    /// Another tool's curve offsets from before the undervolt survive an untick that also
    /// restores the memory offset (whose write wipes the curve on Windows).
    #[test]
    fn untick_restores_another_tools_curve_after_the_memory_write() {
        let (gpu, m) = (FakeGpu::owners(), marker("mem-resets-foreign"));
        {
            let mut st = gpu.state();
            st.mem_write_resets_curve = true;
            st.curve[0].delta_khz = 15_000;
            st.curve[0].freq_khz += 15_000;
        }
        let mut o = oc(&gpu, &m);
        o.apply(&settings(true, true, true), false);
        let r = o.apply(&settings(false, false, false), false);
        assert!(r.results.iter().all(|x| x.ok), "{r:?}");
        assert_eq!(gpu.delta_khz(0), 15_000);
        assert_eq!(gpu.delta_khz(3), 0);
    }

    #[test]
    fn untick_undervolt_restores_the_curve_before() {
        let (gpu, m) = (FakeGpu::owners(), marker("untick-uv"));
        gpu.state().curve[0].delta_khz = 15_000; // another tool's offset below From
        gpu.state().curve[0].freq_khz += 15_000;
        let mut o = oc(&gpu, &m);
        o.apply(&settings(true, false, false), false);
        assert_eq!(gpu.delta_khz(0), 0, "the undervolt owns every core point");
        let r = o.apply(&settings(false, false, false), false);
        assert!(result(&r, "undervolt").ok, "{r:?}");
        assert_eq!(gpu.delta_khz(0), 15_000);
        assert_eq!(gpu.delta_khz(3), 0);
        assert!(!m.exists());
    }

    /// A reboot (a new Overclocker on the same marker) after which another tool changed every
    /// setting AstralGuard had applied: an untick writes nothing and drops the entries.
    #[test]
    fn untick_after_another_tool_changed_the_value_leaves_it() {
        let (gpu, m) = (FakeGpu::owners(), marker("changed-since"));
        oc(&gpu, &m).apply(&settings(true, true, true), false);
        {
            let mut st = gpu.state();
            st.power_w = 550;
            st.mem.0 = 1000;
            st.curve[3].delta_khz = 100_000;
        }
        gpu.clear_calls();
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(false, false, false), false);
        assert!(gpu.calls().is_empty(), "nothing written: {:?}", gpu.calls());
        assert_eq!(r.results.len(), 3, "{r:?}");
        for x in &r.results {
            assert!(x.message.contains("changed since AstralGuard applied it; left as is"), "{x:?}");
        }
        assert!(o.managed().is_empty());
        assert!(!m.exists(), "every entry dropped");
        assert_eq!(gpu.state().power_w, 550);
    }

    /// The same reboot with the values untouched: restored as before.
    #[test]
    fn untick_after_a_restart_with_the_value_unchanged_restores() {
        let (gpu, m) = (FakeGpu::owners(), marker("unchanged-since"));
        oc(&gpu, &m).apply(&settings(true, true, true), false);
        gpu.clear_calls();
        let mut o = oc(&gpu, &m);
        let r = o.apply(&settings(false, false, false), false);
        assert!(r.results.iter().all(|x| x.ok && x.message.contains("restored")), "{r:?}");
        let calls = gpu.calls();
        assert!(calls.contains(&"set_power_limit_w(600)".to_string()), "{calls:?}");
        assert!(calls.contains(&"set_mem_offset(0)".to_string()), "{calls:?}");
        assert_eq!(gpu.delta_khz(3), 0);
        assert!(!m.exists());
    }

    /// A marker from before the applied values were recorded: nothing to compare with, so
    /// treated as changed since.
    #[test]
    fn an_old_marker_without_applied_values_is_never_written_back() {
        let (gpu, m) = (FakeGpu::owners(), marker("old-marker"));
        let old = format!(
            r#"{{"subsystem":{},"power_before_w":450,"mem_before_nvml":1000,"curve_before_khz":[[0,0],[1,0],[2,0],[3,0],[4,0],[5,0]]}}"#,
            super::fake::OWNERS_SUBSYSTEM
        );
        std::fs::write(&m, old).unwrap();
        let r = oc(&gpu, &m).apply(&settings(false, false, false), false);
        assert!(gpu.calls().is_empty(), "{:?}", gpu.calls());
        assert_eq!(r.results.len(), 3, "{r:?}");
        assert!(r.results.iter().all(|x| x.message.contains("left as is")), "{r:?}");
        assert!(!m.exists());
    }

    #[test]
    fn the_marker_records_the_applied_values() {
        let (gpu, m) = (FakeGpu::owners(), marker("applied"));
        oc(&gpu, &m).apply(&settings(true, true, true), false);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&m).unwrap()).unwrap();
        assert_eq!(saved["power_applied_w"], 520);
        assert_eq!(saved["mem_applied_nvml"], 4000);
        assert_eq!(saved["curve_applied_khz"][3], serde_json::json!([3, 862_000]));
        let _ = std::fs::remove_file(&m);
    }

    /// A driver update shortened the core curve: the recorded indices no longer mean the same
    /// points, and one of them may now be a memory point. Refused, entry dropped, nothing written.
    #[test]
    fn restore_curve_refuses_a_curve_whose_core_length_changed() {
        let (gpu, m) = (FakeGpu::owners(), marker("short-core"));
        let mut o = oc(&gpu, &m);
        o.apply(&settings(true, false, false), false);
        {
            // The highest core point is gone; index 5 is now the memory point.
            let mut st = gpu.state();
            st.curve.remove(5);
        }
        gpu.clear_calls();
        let r = o.apply(&settings(false, false, false), false);
        let uv = result(&r, "undervolt");
        assert!(!uv.ok, "{uv:?}");
        assert!(uv.message.contains("6") && uv.message.contains("5"), "names both lengths: {uv:?}");
        assert!(gpu.calls().is_empty(), "nothing written: {:?}", gpu.calls());
        assert!(o.managed().is_empty(), "entry dropped");
        assert!(!m.exists());
    }

    #[test]
    fn the_marker_sits_next_to_the_mitigation_marker() {
        let (ours, theirs) = (marker_path(), crate::mitigation::marker_path());
        assert_eq!(ours.parent(), theirs.parent());
        assert_eq!(ours.file_name().unwrap(), "overclock.active");
    }

    #[test]
    fn the_marker_records_the_values_from_before() {
        let (gpu, m) = (FakeGpu::owners(), marker("contents"));
        oc(&gpu, &m).apply(&settings(false, true, true), false);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&m).unwrap()).unwrap();
        assert_eq!(saved["subsystem"], super::fake::OWNERS_SUBSYSTEM);
        assert_eq!(saved["power_before_w"], 600);
        assert_eq!(saved["mem_before_nvml"], 0);
        assert!(saved["curve_before_khz"].is_null());
        assert!(!m.with_extension("active.tmp").exists());
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn untick_during_guard_is_restored_on_release() {
        let (gpu, m) = (FakeGpu::owners(), marker("untick-guard"));
        let mut o = oc(&gpu, &m);
        o.apply(&settings(false, true, false), false);
        gpu.clear_calls();
        let off = settings(false, false, false);
        assert!(o.apply(&off, true).deferred);
        assert!(gpu.calls().is_empty());
        let r = o.after_release(&off);
        assert_eq!(gpu.calls(), vec!["set_power_limit_w(600)".to_string()]);
        assert!(result(&r, "power").ok, "{r:?}");
        assert!(!m.exists());
    }

    #[test]
    fn newly_ticked_during_guard_is_applied_on_release() {
        let (gpu, m) = (FakeGpu::owners(), marker("tick-guard"));
        let mut o = oc(&gpu, &m);
        let mem = settings(false, false, true);
        assert!(o.apply(&mem, true).deferred);
        assert!(gpu.calls().is_empty());
        let r = o.after_release(&mem);
        assert_eq!(gpu.calls(), vec!["set_mem_offset(4000)".to_string()]);
        assert!(result(&r, "memory").ok, "{r:?}");
        assert_eq!(o.status(&mem).managed, vec!["memory".to_string()]);
        let _ = std::fs::remove_file(&m);
    }

    #[test]
    fn after_release_reapplies_what_was_cleared() {
        let (gpu, m) = (FakeGpu::owners(), marker("release"));
        let mut o = oc(&gpu, &m);
        let s = settings(true, true, true);
        o.apply(&s, false);
        gpu.clear_calls();
        gpu.clear_curve_deltas();
        let r = o.after_release(&s);
        let calls = gpu.calls();
        assert_eq!(calls.len(), 1, "power and memory still match: {calls:?}");
        assert!(calls[0].starts_with("set_curve"), "{calls:?}");
        assert_eq!(gpu.delta_khz(3), 862_000);
        assert_eq!(r.results.len(), 1, "{r:?}");
        assert!(result(&r, "undervolt").ok, "{r:?}");
        // Nothing cleared: nothing to do.
        gpu.clear_calls();
        assert!(o.after_release(&s).results.is_empty());
        assert!(gpu.calls().is_empty());
        let _ = std::fs::remove_file(&m);
    }
}
