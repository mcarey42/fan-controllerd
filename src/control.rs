//! Pure fan-control math: curve interpolation, sensor-demand merging, inlet
//! bias, and slew-rate limiting. No I/O — everything here takes numbers in
//! and returns numbers out, which makes it trivially unit-testable.

use crate::config::{InletBiasConfig, SensorConfig, SlewConfig};

/// Linearly interpolate a duty% from a sorted curve.
///
/// Below the first point: returns the first point's duty (do not extrapolate
/// downward — we don't want to dip below the configured floor).
/// Above the last point: returns the last point's duty (i.e. clamp to top of
/// curve — the hard-ceiling tripwire is what handles runaway).
///
/// Curve must have >= 2 points sorted ascending by temp (config validates this).
pub fn interpolate_curve(curve: &[(f32, u8)], temp_c: f32) -> u8 {
    debug_assert!(curve.len() >= 2, "curve must have at least 2 points");
    if temp_c <= curve[0].0 {
        return curve[0].1;
    }
    if temp_c >= curve[curve.len() - 1].0 {
        return curve[curve.len() - 1].1;
    }
    for win in curve.windows(2) {
        let (t0, d0) = (win[0].0, win[0].1 as f32);
        let (t1, d1) = (win[1].0, win[1].1 as f32);
        if temp_c >= t0 && temp_c <= t1 {
            let frac = (temp_c - t0) / (t1 - t0);
            let duty = d0 + (d1 - d0) * frac;
            return duty.round().clamp(0.0, 100.0) as u8;
        }
    }
    // Unreachable given the clamp checks above, but be safe.
    curve[curve.len() - 1].1
}

/// Compute the extra duty% to add when the inlet is hotter than the configured
/// threshold. Capped at `max_bias_pct`. Returns 0 if no inlet reading or no
/// bias configured.
pub fn inlet_bias_pct(inlet_temp_c: Option<f32>, bias: Option<&InletBiasConfig>) -> u8 {
    let (Some(bias), Some(t)) = (bias, inlet_temp_c) else {
        return 0;
    };
    if t <= bias.threshold_c {
        return 0;
    }
    let extra = (t - bias.threshold_c) * bias.percent_per_degree_above;
    extra.round().clamp(0.0, bias.max_bias_pct as f32) as u8
}

/// Compute demanded duty for a single sensor: interpolate curve, then add the
/// inlet bias, then clamp to [min_duty, max_duty]. Per-sensor — the merge
/// across sensors happens in `merge_demands`.
pub fn demand_for_sensor(
    sensor: &SensorConfig,
    temp_c: f32,
    bias_pct: u8,
    slew: &SlewConfig,
) -> u8 {
    let base = interpolate_curve(&sensor.curve, temp_c);
    let biased = base.saturating_add(bias_pct);
    biased.clamp(slew.min_duty, slew.max_duty)
}

/// Loudest wins: the merged demand is the max across all sensors. None inputs
/// (failed reads) are skipped. If all are None, returns min_duty as a safe
/// floor — caller may want to trip safety in that case (handled in Stage 3).
pub fn merge_demands(demands: &[Option<u8>], slew: &SlewConfig) -> u8 {
    demands
        .iter()
        .copied()
        .flatten()
        .max()
        .unwrap_or(slew.min_duty)
}

/// Slew-rate limiter: move `current` toward `target` by at most `max_rise`
/// when rising, `max_fall` when falling. Result is clamped to [min, max].
pub fn slew_limit(current: u8, target: u8, slew: &SlewConfig) -> u8 {
    let next = if target > current {
        let delta = (target - current).min(slew.max_rise_per_tick);
        current.saturating_add(delta)
    } else if target < current {
        let delta = (current - target).min(slew.max_fall_per_tick);
        current.saturating_sub(delta)
    } else {
        current
    };
    next.clamp(slew.min_duty, slew.max_duty)
}

/// Suppress a duty change that's smaller than `deadband_pct`. Returns the
/// previous `current` when the proposed `target` is within the band, so the
/// caller skips the IPMI write. `deadband_pct = 0` disables suppression.
/// First tick (`current = None`) always passes through.
pub fn apply_deadband(current: Option<u8>, target: u8, deadband_pct: u8) -> u8 {
    match current {
        Some(curr) if target.abs_diff(curr) < deadband_pct => curr,
        _ => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InletBiasConfig, SensorConfig, SensorSpec, SlewConfig};

    fn sensor(curve: Vec<(f32, u8)>) -> SensorConfig {
        SensorConfig {
            name: "t".into(),
            spec: SensorSpec::Hwmon {
                chip: "coretemp".into(),
                label: "Package id 0".into(),
            },
            hard_ceiling_c: 95.0,
            curve,
        }
    }

    fn default_slew() -> SlewConfig {
        SlewConfig {
            max_rise_per_tick: 10,
            max_fall_per_tick: 3,
            min_duty: 20,
            max_duty: 100,
            deadband_pct: 0, // existing tests assume no deadband suppression
        }
    }

    // --- interpolate_curve ---

    #[test]
    fn interp_below_first_returns_first() {
        let c = vec![(40.0, 20), (80.0, 80)];
        assert_eq!(interpolate_curve(&c, 30.0), 20);
        assert_eq!(interpolate_curve(&c, 40.0), 20);
    }

    #[test]
    fn interp_above_last_returns_last() {
        let c = vec![(40.0, 20), (80.0, 80)];
        assert_eq!(interpolate_curve(&c, 80.0), 80);
        assert_eq!(interpolate_curve(&c, 99.0), 80);
    }

    #[test]
    fn interp_midpoint_is_linear() {
        let c = vec![(40.0, 20), (80.0, 80)];
        // halfway between 40 and 80 is 60; halfway between 20 and 80 is 50.
        assert_eq!(interpolate_curve(&c, 60.0), 50);
    }

    #[test]
    fn interp_multi_segment() {
        let c = vec![(40.0, 20), (60.0, 30), (80.0, 80)];
        assert_eq!(interpolate_curve(&c, 50.0), 25); // halfway in first segment
        assert_eq!(interpolate_curve(&c, 70.0), 55); // halfway in second segment
    }

    // --- inlet_bias ---

    #[test]
    fn no_bias_when_inlet_cool() {
        let b = InletBiasConfig {
            sensor: "Inlet Temp".into(),
            threshold_c: 27.0,
            percent_per_degree_above: 2.0,
            max_bias_pct: 30,
        };
        assert_eq!(inlet_bias_pct(Some(25.0), Some(&b)), 0);
        assert_eq!(inlet_bias_pct(Some(27.0), Some(&b)), 0);
    }

    #[test]
    fn bias_scales_with_overage() {
        let b = InletBiasConfig {
            sensor: "Inlet Temp".into(),
            threshold_c: 27.0,
            percent_per_degree_above: 2.0,
            max_bias_pct: 30,
        };
        assert_eq!(inlet_bias_pct(Some(30.0), Some(&b)), 6); // 3 deg over * 2 = 6
        assert_eq!(inlet_bias_pct(Some(40.0), Some(&b)), 26);
    }

    #[test]
    fn bias_clamped_to_max() {
        let b = InletBiasConfig {
            sensor: "Inlet Temp".into(),
            threshold_c: 27.0,
            percent_per_degree_above: 5.0,
            max_bias_pct: 20,
        };
        assert_eq!(inlet_bias_pct(Some(50.0), Some(&b)), 20); // would be 115 unclamped
    }

    #[test]
    fn no_bias_when_no_config_or_reading() {
        assert_eq!(inlet_bias_pct(Some(40.0), None), 0);
        let b = InletBiasConfig {
            sensor: "Inlet Temp".into(),
            threshold_c: 27.0,
            percent_per_degree_above: 2.0,
            max_bias_pct: 30,
        };
        assert_eq!(inlet_bias_pct(None, Some(&b)), 0);
    }

    // --- demand_for_sensor ---

    #[test]
    fn demand_clamps_to_min_duty() {
        let s = sensor(vec![(40.0, 5), (80.0, 80)]); // curve dips below floor
        let d = demand_for_sensor(&s, 40.0, 0, &default_slew());
        assert_eq!(d, 20); // clamped up to min_duty
    }

    #[test]
    fn demand_adds_bias_before_clamp() {
        let s = sensor(vec![(40.0, 20), (80.0, 80)]);
        // base at 60C = 50, plus bias 10 = 60
        assert_eq!(demand_for_sensor(&s, 60.0, 10, &default_slew()), 60);
    }

    #[test]
    fn demand_clamps_to_max_duty() {
        let mut slew = default_slew();
        slew.max_duty = 85;
        let s = sensor(vec![(40.0, 20), (80.0, 95)]);
        // base at 80C = 95, clamped to 85
        assert_eq!(demand_for_sensor(&s, 80.0, 0, &slew), 85);
    }

    // --- merge_demands ---

    #[test]
    fn merge_takes_max() {
        let slew = default_slew();
        assert_eq!(merge_demands(&[Some(40), Some(70), Some(50)], &slew), 70);
    }

    #[test]
    fn merge_ignores_none() {
        let slew = default_slew();
        assert_eq!(merge_demands(&[Some(40), None, Some(50)], &slew), 50);
    }

    #[test]
    fn merge_all_none_returns_min_duty() {
        let slew = default_slew();
        assert_eq!(merge_demands(&[None, None], &slew), 20);
    }

    // --- slew_limit ---

    #[test]
    fn slew_rises_capped() {
        let slew = default_slew(); // rise 10, fall 3
        assert_eq!(slew_limit(30, 100, &slew), 40);
    }

    #[test]
    fn slew_falls_capped() {
        let slew = default_slew();
        assert_eq!(slew_limit(50, 20, &slew), 47);
    }

    #[test]
    fn slew_reaches_target_when_within_step() {
        let slew = default_slew();
        assert_eq!(slew_limit(30, 35, &slew), 35); // +5 within +10 cap
        assert_eq!(slew_limit(50, 48, &slew), 48); // -2 within -3 cap
    }

    #[test]
    fn slew_clamps_to_min() {
        let slew = default_slew();
        // current 21, target 10 — fall capped to 3, so 18; then clamped to 20
        assert_eq!(slew_limit(21, 10, &slew), 20);
    }

    #[test]
    fn slew_clamps_to_max() {
        let mut slew = default_slew();
        slew.max_duty = 90;
        // current 85, target 100 — rise capped to +10 = 95; clamped to 90
        assert_eq!(slew_limit(85, 100, &slew), 90);
    }

    // --- apply_deadband ---

    #[test]
    fn deadband_first_tick_passes_through() {
        // No prior duty — write the target whatever the band.
        assert_eq!(apply_deadband(None, 25, 2), 25);
        assert_eq!(apply_deadband(None, 100, 50), 100);
    }

    #[test]
    fn deadband_suppresses_changes_inside_band() {
        // diff 1 < band 2 → hold previous
        assert_eq!(apply_deadband(Some(28), 29, 2), 28);
        assert_eq!(apply_deadband(Some(28), 27, 2), 28);
    }

    #[test]
    fn deadband_allows_changes_at_or_above_band() {
        // diff 2 >= band 2 → pass
        assert_eq!(apply_deadband(Some(28), 30, 2), 30);
        assert_eq!(apply_deadband(Some(28), 26, 2), 26);
    }

    #[test]
    fn deadband_zero_disables_suppression() {
        // Every change goes through.
        assert_eq!(apply_deadband(Some(28), 29, 0), 29);
        assert_eq!(apply_deadband(Some(28), 28, 0), 28);
    }

    #[test]
    fn deadband_no_change_returns_current() {
        assert_eq!(apply_deadband(Some(28), 28, 2), 28);
    }
}
