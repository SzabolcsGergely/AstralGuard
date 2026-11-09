//! Per-point V/F curve offsets for an undervolt. Pure arithmetic, no hardware access.

/// One curve entry as the driver reports it. `freq_khz` is the effective frequency (stock plus delta).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPoint {
    pub freq_khz: u32,
    pub volt_uv: u32,
    pub delta_khz: i32,
    pub memory: bool,
}

/// Stock clock of a point: the effective frequency with the currently applied delta taken back out.
pub fn stock_mhz(p: &RawPoint) -> f64 {
    (p.freq_khz as i64 - p.delta_khz as i64) as f64 / 1000.0
}

/// The deltas to write, one entry per core point (curve index, delta in kHz), in curve order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UvPlan {
    pub offset_mhz: i32,
    pub deltas_khz: Vec<(usize, i32)>,
}

fn volt_mv(p: &RawPoint) -> f64 {
    p.volt_uv as f64 / 1000.0
}

/// Plan the per-point deltas for an undervolt.
///
/// Points with voltage in `[from_mv, to_mv]` get one shared offset that lands the highest of them
/// (the highest core point at or below `to_mv`) on `target_mhz`. Points above `to_mv` are capped to
/// `target_mhz` so the card cannot boost past the target at a higher voltage. Points below `from_mv`
/// stay stock. Every core point gets an entry, so the plan owns the whole curve and re-planning over
/// an already-applied curve is idempotent because everything is measured from stock.
///
/// Points from the first memory-flagged one onward belong to the memory domain and never get a delta.
/// A delta beyond `max_delta_mhz` is refused rather than clamped: a clamped plan would silently not
/// be the curve the user asked for.
pub fn plan_undervolt(
    points: &[RawPoint],
    from_mv: f64,
    to_mv: f64,
    target_mhz: f64,
    max_delta_mhz: i32,
) -> Result<UvPlan, String> {
    if from_mv >= to_mv {
        return Err(format!("From ({from_mv} mV) must be below To ({to_mv} mV)"));
    }
    let core_len = points.iter().position(|p| p.memory).unwrap_or(points.len());
    let core = &points[..core_len];
    let (Some(lowest), Some(highest)) = (core.first(), core.last()) else {
        return Err("the curve has no core points".into());
    };
    if from_mv < volt_mv(lowest) || to_mv > volt_mv(highest) {
        return Err(format!(
            "range {from_mv}-{to_mv} mV is outside the curve ({}-{} mV)",
            volt_mv(lowest),
            volt_mv(highest)
        ));
    }
    let Some(top) = core.iter().rev().find(|p| volt_mv(p) <= to_mv) else {
        return Err(format!("no curve point at or below {to_mv} mV"));
    };
    let offset_mhz = (target_mhz - stock_mhz(top)).round() as i32;

    let mut deltas_khz = Vec::with_capacity(core.len());
    for (i, p) in core.iter().enumerate() {
        let mv = volt_mv(p);
        let delta_mhz = if mv < from_mv {
            0
        } else if mv <= to_mv {
            offset_mhz
        } else {
            (target_mhz - stock_mhz(p)).round() as i32
        };
        if delta_mhz.abs() > max_delta_mhz {
            return Err(format!("offset {delta_mhz} MHz at {mv} mV exceeds the +/-{max_delta_mhz} MHz limit"));
        }
        deltas_khz.push((i, delta_mhz * 1000));
    }
    Ok(UvPlan { offset_mhz, deltas_khz })
}

/// Afterburner shows the memory clock as the effective rate halved; NVML wants twice that.
pub fn ab_to_nvml_mem(ab_mhz: i32) -> i32 {
    ab_mhz * 2
}

/// Inverse of [`ab_to_nvml_mem`]; integer division truncates toward zero.
pub fn nvml_to_ab_mem(nvml_mhz: i32) -> i32 {
    nvml_mhz / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(mv: u32, mhz: u32) -> RawPoint {
        RawPoint { freq_khz: mhz * 1000, volt_uv: mv * 1000, delta_khz: 0, memory: false }
    }

    /// The owner's stock curve, plus one memory-domain point at the end.
    fn fixture() -> Vec<RawPoint> {
        let mut v = vec![pt(810, 757), pt(815, 847), pt(820, 930), pt(895, 1965), pt(900, 2047), pt(1240, 3232)];
        v.push(RawPoint { freq_khz: 14_000_000, volt_uv: 1_240_000, delta_khz: 0, memory: true });
        v
    }

    fn delta_mhz(plan: &UvPlan, mv: f64) -> i32 {
        let pts = fixture();
        let (_, d) = plan.deltas_khz.iter().find(|(i, _)| pts[*i].volt_uv as f64 / 1000.0 == mv).unwrap();
        d / 1000
    }

    #[test]
    fn owners_profile_gives_plus_862() {
        let p = plan_undervolt(&fixture(), 820.0, 895.0, 2827.0, 1000).unwrap();
        assert_eq!(p.offset_mhz, 862);
        assert_eq!(delta_mhz(&p, 810.0), 0);
        assert_eq!(delta_mhz(&p, 815.0), 0);
        assert_eq!(delta_mhz(&p, 820.0), 862);
        assert_eq!(delta_mhz(&p, 895.0), 862);
        assert_eq!(delta_mhz(&p, 900.0), 780);
        assert_eq!(delta_mhz(&p, 1240.0), -405);
        assert_eq!(p.deltas_khz.len(), 6);
        assert!(p.deltas_khz.iter().all(|(i, _)| !fixture()[*i].memory));
    }

    #[test]
    fn to_between_points_uses_the_highest_point_at_or_below() {
        let a = plan_undervolt(&fixture(), 820.0, 897.0, 2827.0, 1000).unwrap();
        let b = plan_undervolt(&fixture(), 820.0, 895.0, 2827.0, 1000).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn stock_is_effective_minus_current_delta() {
        let p = RawPoint { freq_khz: 2_827_000, volt_uv: 895_000, delta_khz: 862_000, memory: false };
        assert_eq!(stock_mhz(&p), 1965.0);
    }

    #[test]
    fn replanning_over_an_applied_curve_gives_the_same_plan() {
        let mut applied = fixture();
        let first = plan_undervolt(&applied, 820.0, 895.0, 2827.0, 1000).unwrap();
        for (i, d) in &first.deltas_khz {
            applied[*i].delta_khz = *d;
            applied[*i].freq_khz = (applied[*i].freq_khz as i64 + *d as i64) as u32;
        }
        assert_eq!(plan_undervolt(&applied, 820.0, 895.0, 2827.0, 1000).unwrap(), first);
    }

    #[test]
    fn a_delta_beyond_the_limit_is_refused_not_clamped() {
        let e = plan_undervolt(&fixture(), 820.0, 895.0, 4000.0, 1000).unwrap_err();
        assert!(e.contains("1000"), "{e}");
    }

    #[test]
    fn an_empty_or_outside_range_is_refused() {
        assert!(plan_undervolt(&fixture(), 895.0, 820.0, 2827.0, 1000).is_err());
        assert!(plan_undervolt(&fixture(), 820.0, 820.0, 2827.0, 1000).is_err());
        assert!(plan_undervolt(&fixture(), 700.0, 895.0, 2827.0, 1000).is_err());
        assert!(plan_undervolt(&fixture(), 820.0, 1300.0, 2827.0, 1000).is_err());
    }

    #[test]
    fn memory_units() {
        assert_eq!(ab_to_nvml_mem(2000), 4000);
        assert_eq!(nvml_to_ab_mem(6000), 3000);
        assert_eq!(nvml_to_ab_mem(-2000), -1000);
        assert_eq!(nvml_to_ab_mem(3), 1);
        assert_eq!(nvml_to_ab_mem(-3), -1);
    }
}
