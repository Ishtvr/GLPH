//! V/F policies in physical units. Voltage anchors are identities, not write fields.
use crate::features::Feature;
use crate::profiles::{Setting, Value, VfPoint};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfNode {
    pub index: u16,
    pub voltage_uv: u32,
    pub frequency_khz: u32,
    pub delta_khz: i32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfState {
    pub nodes: Vec<VfNode>,
    pub minimum_delta_khz: i32,
    pub maximum_delta_khz: i32,
    pub first_index: u16,
    pub last_index: u16,
}
impl VfState {
    /// Driver-reported frequencies can move while all programmed deltas remain
    /// unchanged. Restore verifies the complete writable image and voltage grid,
    /// not an old instantaneous boost-frequency observation.
    pub fn controls_match(&self, actual: &Self) -> Result<bool, String> {
        Ok(self.domain_matches(actual)?
            && self
                .nodes
                .iter()
                .zip(&actual.nodes)
                .all(|(a, b)| a.delta_khz == b.delta_khz))
    }
    pub fn domain_matches(&self, actual: &Self) -> Result<bool, String> {
        self.validate()?;
        actual.validate()?;
        Ok(self.first_index == actual.first_index
            && self.last_index == actual.last_index
            && self.minimum_delta_khz == actual.minimum_delta_khz
            && self.maximum_delta_khz == actual.maximum_delta_khz
            && self.nodes.len() == actual.nodes.len()
            && self.nodes.iter().zip(&actual.nodes).all(|(before, now)| {
                before.index == now.index && before.voltage_uv == now.voltage_uv
            }))
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.last_index < self.first_index
            || self.last_index >= 255
            || self.nodes.len() != usize::from(self.last_index - self.first_index) + 1
            || self.minimum_delta_khz > self.maximum_delta_khz
        {
            return Err(crate::message::msg("vf.err.domain", &[]));
        }
        for (position, node) in self.nodes.iter().enumerate() {
            if usize::from(node.index) != usize::from(self.first_index) + position
                || node.voltage_uv == 0
                || node.frequency_khz == 0
                || !(self.minimum_delta_khz..=self.maximum_delta_khz).contains(&node.delta_khz)
            {
                return Err(crate::message::msg("vf.err.node", &[]));
            }
        }
        if self
            .nodes
            .windows(2)
            .any(|p| p[0].voltage_uv >= p[1].voltage_uv)
        {
            return Err(crate::message::msg("vf.err.grid_order", &[]));
        }
        Ok(())
    }
    pub fn value(&self) -> Result<Value, String> {
        self.validate()?;
        if self
            .nodes
            .iter()
            .any(|p| p.voltage_uv % 1000 != 0 || p.frequency_khz % 1000 != 0)
        {
            return Err(crate::message::msg("vf.err.units", &[]));
        }
        let value = Value::VfCurve(
            self.nodes
                .iter()
                .map(|p| VfPoint {
                    voltage_mv: p.voltage_uv / 1000,
                    frequency_mhz: p.frequency_khz / 1000,
                    delta_mhz: None,
                })
                .collect(),
        );
        Setting {
            feature: Feature::GpuVfCurve,
            value: value.clone(),
        }
        .validate()?;
        Ok(value)
    }
    /// Monotonic factory base frequency (kHz) per node with programmed deltas
    /// removed and clamped low anchors lowered to the next higher anchor's
    /// frequency. Shared by `factory()`, `target_deltas()`, and curve limits so
    /// resetting to the factory curve always yields zero deltas and never trips
    /// `vf.err.bounds`.
    pub fn base_frequencies_khz(&self) -> Result<Vec<u32>, String> {
        self.validate()?;
        let mut base: Vec<u32> = self
            .nodes
            .iter()
            .map(|node| {
                let raw = i64::from(node.frequency_khz) - i64::from(node.delta_khz);
                if raw <= 0 && node.delta_khz > 0 {
                    Ok(1_000)
                } else {
                    u32::try_from(raw)
                        .ok()
                        .filter(|&v| v > 0)
                        .ok_or_else(|| crate::message::msg("vf.err.frequency", &[]))
                }
            })
            .collect::<Result<_, _>>()?;
        let mut ceiling = u32::MAX;
        for freq in base.iter_mut().rev() {
            *freq = (*freq).min(ceiling);
            ceiling = *freq;
        }
        Ok(base)
    }
    pub fn target_deltas(&self, target: &[VfPoint]) -> Result<Vec<(u16, i32)>, String> {
        self.validate()?;
        Setting {
            feature: Feature::GpuVfCurve,
            value: Value::VfCurve(target.to_vec()),
        }
        .validate()?;
        if target.len() != self.nodes.len() {
            return Err(crate::message::msg("vf.err.complete_grid", &[]));
        }
        if target.iter().all(|p| p.delta_mhz.is_some()) {
            return self
                .nodes
                .iter()
                .zip(target)
                .map(|(original, wanted)| {
                    if u64::from(wanted.voltage_mv) * 1000 != u64::from(original.voltage_uv) {
                        return Err(crate::message::msg("vf.err.anchors", &[]));
                    }
                    let delta = i64::from(wanted.delta_mhz.expect("checked above")) * 1000;
                    let delta = i32::try_from(delta)
                        .map_err(|_| crate::message::msg("vf.err.delta_representation", &[]))?;
                    if !(self.minimum_delta_khz..=self.maximum_delta_khz).contains(&delta) {
                        return Err(crate::message::msg("vf.err.bounds", &[]));
                    }
                    Ok((original.index, delta))
                })
                .collect();
        }
        let bases = self.base_frequencies_khz()?;
        let deltas: Vec<(u16, i32)> = self
            .nodes
            .iter()
            .zip(bases)
            .zip(target)
            .map(|((original, base_khz), wanted)| {
                if u64::from(wanted.voltage_mv) * 1000 != u64::from(original.voltage_uv) {
                    return Err(crate::message::msg("vf.err.anchors", &[]));
                }
                let delta = i64::from(wanted.frequency_mhz) * 1000 - i64::from(base_khz);
                let delta = i32::try_from(delta)
                    .map_err(|_| crate::message::msg("vf.err.delta_representation", &[]))?;
                if !(self.minimum_delta_khz..=self.maximum_delta_khz).contains(&delta) {
                    return Err(crate::message::msg("vf.err.bounds", &[]));
                }
                Ok((original.index, delta))
            })
            .collect::<Result<_, _>>()?;
        if legacy_curve_has_pstate_drift(target, &deltas) {
            return Err(crate::message::msg("vf.err.legacy_drift", &[]));
        }
        Ok(deltas)
    }
    /// Derive once from the durable capture, so recovery can reconstruct the
    /// exact controls even if boost observations change before or after SET.
    /// The driver's own curve: every programmed frequency delta removed. At the
    /// lowest anchors the driver clamps the reported frequency, so "reported
    /// minus delta" can overshoot there (measured 30 October 2026: 903, 888,
    /// then 386 MHz). Such points are lowered to the next higher anchor's
    /// frequency, which keeps the curve non-decreasing as editors require.
    pub fn factory(&self) -> Result<Self, String> {
        let bases = self.base_frequencies_khz()?;
        let mut state = self.clone();
        for (node, base_khz) in state.nodes.iter_mut().zip(bases) {
            node.frequency_khz = base_khz;
            node.delta_khz = 0;
        }
        state.validate()?;
        Ok(state)
    }
    pub fn target_state(&self, target: &[VfPoint]) -> Result<Self, String> {
        let bases = self.base_frequencies_khz()?;
        let deltas = self.target_deltas(target)?;
        let mut state = self.clone();
        for (((node, base_khz), (_, delta)), point) in
            state.nodes.iter_mut().zip(bases).zip(deltas).zip(target)
        {
            node.delta_khz = delta;
            node.frequency_khz = if point.delta_mhz.is_some() {
                let freq = (i64::from(base_khz) + i64::from(delta)).max(1_000);
                u32::try_from(freq).map_err(|_| crate::message::msg("vf.err.frequency", &[]))?
            } else {
                point
                    .frequency_mhz
                    .checked_mul(1000)
                    .ok_or(crate::message::msg("vf.err.frequency", &[]))?
            };
        }
        Ok(state)
    }
}

/// Detects dynamic GPU P-state / Vmin-ramp shifts (measured on RTX 50-series
/// Blackwell: up to +900..+1050 MHz shift at 600–670 mV between P8 idle and
/// P4/P0 load) on legacy absolute-MHz curves that omit `delta_mhz`.
///
/// Refuses to silently write a localized ±350+ MHz crater/spike on non-plateau
/// anchors while leaving legitimate top-end flat undervolt plateaus and
/// uniform whole-curve offsets untouched.
fn legacy_curve_has_pstate_drift(target: &[VfPoint], deltas: &[(u16, i32)]) -> bool {
    if target.len() < 8 || deltas.len() != target.len() {
        return false;
    }
    let max_freq = target.last().map_or(0, |p| p.frequency_mhz);
    let mid = target.len() / 2;
    let mut upper: Vec<i64> = deltas[mid..]
        .iter()
        .zip(&target[mid..])
        .filter(|(_, p)| p.frequency_mhz < max_freq)
        .map(|((_, d), _)| i64::from(*d))
        .collect();
    if upper.len() < 3 {
        upper = deltas[mid..].iter().map(|(_, d)| i64::from(*d)).collect();
    }
    upper.sort_unstable();
    let ref_delta = upper[upper.len() / 2];

    for i in 0..target.len() {
        // High-voltage flat plateau ceilings legitimately carry large negative deltas.
        if target[i].frequency_mhz >= max_freq {
            continue;
        }
        let d = i64::from(deltas[i].1);
        let spread = (d - ref_delta).abs();
        if d.abs() > 350_000 && spread > 350_000 {
            return true;
        }
        if i > 0 {
            let prev = i64::from(deltas[i - 1].1);
            if (d - prev).abs() > 200_000 && spread > 250_000 {
                return true;
            }
        }
    }
    false
}

/// Keep the lower curve and flatten from an existing voltage anchor. This is a
/// frequency-at-voltage target, not a guaranteed physical voltage ceiling.
pub fn plateau(state: &VfState, voltage_mv: u32, frequency_mhz: u32) -> Result<Value, String> {
    let Value::VfCurve(mut points) = state.value()? else {
        unreachable!()
    };
    let first = points
        .iter()
        .position(|p| p.voltage_mv == voltage_mv)
        .ok_or(crate::message::msg("vf.err.anchor_required", &[]))?;
    for point in &mut points[first..] {
        point.frequency_mhz = frequency_mhz;
    }
    let deltas = state.target_deltas(&points)?;
    for (point, (_, delta_khz)) in points.iter_mut().zip(deltas) {
        point.delta_mhz = Some(delta_khz / 1000);
    }
    Ok(Value::VfCurve(points))
}

/// Fixed qualification operations preserve the existing curve, with only the
/// selected frequency deltas shifted. No assumed factory image is substituted.
#[derive(Debug, Clone, Copy)]
pub enum CurveTrial {
    AllPointsMinus15,
    FirstPointMinus15,
}

impl CurveTrial {
    pub fn target(self, state: &VfState) -> Result<Value, String> {
        let Value::VfCurve(mut points) = state.value()? else {
            unreachable!()
        };
        for (index, point) in points.iter_mut().enumerate() {
            if matches!(self, Self::AllPointsMinus15) || index == 0 {
                point.frequency_mhz = point
                    .frequency_mhz
                    .checked_sub(15)
                    .filter(|value| *value > 0)
                    .ok_or("Frequency is too low for the fixed trial")?;
            }
        }
        self.validate(state, &points)?;
        Ok(Value::VfCurve(points))
    }

    pub fn validate(self, state: &VfState, points: &[VfPoint]) -> Result<(), String> {
        for (position, ((_, delta), original)) in state
            .target_deltas(points)?
            .iter()
            .zip(&state.nodes)
            .enumerate()
        {
            let shift = if matches!(self, Self::AllPointsMinus15) || position == 0 {
                15_000
            } else {
                0
            };
            if original.delta_khz.checked_sub(shift) != Some(*delta) {
                return Err(
                    "Fixed curve trial permits only its selected minus-15-MHz delta".into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> VfState {
        VfState {
            nodes: vec![
                VfNode {
                    index: 3,
                    voltage_uv: 800000,
                    frequency_khz: 1000000,
                    delta_khz: 15000,
                },
                VfNode {
                    index: 4,
                    voltage_uv: 900000,
                    frequency_khz: 1200000,
                    delta_khz: 30000,
                },
                VfNode {
                    index: 5,
                    voltage_uv: 1000000,
                    frequency_khz: 1400000,
                    delta_khz: 45000,
                },
            ],
            minimum_delta_khz: -500000,
            maximum_delta_khz: 500000,
            first_index: 3,
            last_index: 5,
        }
    }
    #[test]
    fn deltas_preserve_existing_tuning_and_target_the_exact_grid() {
        let state = state();
        let Value::VfCurve(points) = plateau(&state, 900, 1200).unwrap() else {
            panic!()
        };
        assert_eq!(
            state.target_deltas(&points).unwrap(),
            [(3, 15000), (4, 30000), (5, -155000)]
        );
        let mut wrong = points.clone();
        wrong[1].voltage_mv = 905;
        assert!(state.target_deltas(&wrong).is_err());
        assert!(state.target_deltas(&points[..2]).is_err());
        assert!(plateau(&state, 900, 999).is_err());
        assert!(plateau(&state, 900, u32::MAX).is_err());
    }
    #[test]
    fn factory_curve_removes_every_programmed_delta() {
        let factory = state().factory().unwrap();
        assert!(factory.nodes.iter().all(|p| p.delta_khz == 0));
        assert_eq!(
            factory
                .nodes
                .iter()
                .map(|p| p.frequency_khz)
                .collect::<Vec<_>>(),
            [985000, 1170000, 1355000]
        );
        assert!(state().domain_matches(&factory).unwrap());
    }
    #[test]
    fn clamped_low_anchors_never_make_the_factory_curve_fall() {
        // Measured shape: the driver reports a clamped low frequency, so
        // "reported minus delta" overshoots at the first anchors.
        let mut clamped = state();
        clamped.nodes[0].frequency_khz = 400000;
        clamped.nodes[0].delta_khz = -500000; // 900 MHz derived
        clamped.nodes[1].frequency_khz = 570000;
        clamped.nodes[1].delta_khz = 184000; // 386 MHz derived
        clamped.nodes[2].frequency_khz = 1400000;
        clamped.nodes[2].delta_khz = 0;
        let factory = clamped.factory().unwrap();
        let mhz: Vec<_> = factory
            .nodes
            .iter()
            .map(|p| p.frequency_khz / 1000)
            .collect();
        assert_eq!(mhz, [386, 386, 1400]);
        let Value::VfCurve(factory_points) = factory.value().unwrap() else {
            panic!()
        };
        // Resetting a clamped curve back to factory must yield zero deltas, never vf.err.bounds.
        assert_eq!(
            clamped.target_deltas(&factory_points).unwrap(),
            [(3, 0), (4, 0), (5, 0)]
        );
    }
    #[test]
    fn incomplete_and_nonrepresentable_states_are_not_rounded() {
        let mut state = state();
        state.nodes[1].frequency_khz += 500;
        assert!(state.value().is_err());
        state.nodes.remove(1);
        assert!(state.validate().is_err());
    }
    #[test]
    fn restore_compares_all_controls_without_claiming_a_fixed_boost_observation() {
        let original = state();
        let mut live = original.clone();
        for point in &mut live.nodes {
            point.frequency_khz += 30000;
        }
        assert!(original.controls_match(&live).unwrap());
        live.nodes[1].delta_khz += 15000;
        assert!(!original.controls_match(&live).unwrap());
        live.nodes[1].delta_khz = original.nodes[1].delta_khz;
        live.nodes[1].voltage_uv += 1000;
        assert!(!original.controls_match(&live).unwrap());
    }

    #[test]
    fn fixed_trials_preserve_an_existing_nonuniform_curve_and_select_exact_points() {
        let original = state();
        let Value::VfCurve(all) = CurveTrial::AllPointsMinus15.target(&original).unwrap() else {
            panic!()
        };
        assert_eq!(
            original.target_deltas(&all).unwrap(),
            [(3, 0), (4, 15000), (5, 30000)]
        );
        let Value::VfCurve(first) = CurveTrial::FirstPointMinus15.target(&original).unwrap() else {
            panic!()
        };
        assert_eq!(
            original.target_deltas(&first).unwrap(),
            [(3, 0), (4, 30000), (5, 45000)]
        );
        assert!(
            CurveTrial::AllPointsMinus15
                .validate(&original, &first)
                .is_err()
        );
        assert!(
            CurveTrial::FirstPointMinus15
                .validate(&original, &all)
                .is_err()
        );
        assert_eq!(
            original
                .nodes
                .iter()
                .map(|n| n.delta_khz)
                .collect::<Vec<_>>(),
            [15000, 30000, 45000]
        );
    }

    #[test]
    fn trial_rejects_changed_observation_grid_and_insufficient_headroom() {
        let original = state();
        let Value::VfCurve(target) = CurveTrial::AllPointsMinus15.target(&original).unwrap() else {
            panic!()
        };
        let mut changed = original.clone();
        changed.nodes[1].frequency_khz += 15000;
        assert!(
            CurveTrial::AllPointsMinus15
                .validate(&changed, &target)
                .is_err()
        );
        changed = original.clone();
        changed.nodes[0].voltage_uv += 1000;
        assert!(
            CurveTrial::AllPointsMinus15
                .validate(&changed, &target)
                .is_err()
        );
        changed = original;
        changed.minimum_delta_khz = 15000;
        assert!(CurveTrial::AllPointsMinus15.target(&changed).is_err());
        changed.minimum_delta_khz = -500000;
        changed.nodes[0].frequency_khz = 10000;
        assert!(CurveTrial::FirstPointMinus15.target(&changed).is_err());
    }

    #[test]
    fn delta_encoded_curve_survives_blackwell_pstate_vmin_ramp_shift() {
        // Idle P8 state: 600..670 mV ramp starts at 180..1365 MHz.
        let p8_freqs = [
            180, 180, 180, 180, 225, 390, 547, 712, 1425, 2000, 2600, 3127,
        ];
        let p8_mv = [450, 500, 550, 620, 625, 635, 640, 645, 675, 800, 960, 1240];
        let mk_state = |freqs: &[u32]| VfState {
            nodes: p8_mv
                .iter()
                .zip(freqs)
                .enumerate()
                .map(|(i, (&mv, &mhz))| VfNode {
                    index: i as u16,
                    voltage_uv: mv * 1000,
                    frequency_khz: mhz * 1000,
                    delta_khz: 0,
                })
                .collect(),
            minimum_delta_khz: -1_000_000,
            maximum_delta_khz: 1_000_000,
            first_index: 0,
            last_index: (p8_mv.len() - 1) as u16,
        };
        let p8 = mk_state(&p8_freqs);
        // Save a plateau at 960 mV -> 2600 MHz using delta-aware helper.
        let Value::VfCurve(points) = plateau(&p8, 960, 2600).unwrap() else {
            panic!()
        };
        assert!(points.iter().all(|p| p.delta_mhz.is_some()));
        assert_eq!(points[5].delta_mhz, Some(0)); // 635 mV untouched
        assert_eq!(points[11].delta_mhz, Some(-527)); // 1240 mV plateau

        // Load P4/P0 state: Blackwell shifts 620..645 mV up by +700..+1050 MHz.
        let p0_freqs = [
            180, 180, 180, 950, 1180, 1420, 1440, 1460, 1500, 2060, 2630, 3090,
        ];
        let p0 = mk_state(&p0_freqs);
        let deltas = p0.target_deltas(&points).unwrap();
        assert_eq!(deltas[5], (5, 0)); // never becomes -1030 MHz or vf.err.bounds!
        assert_eq!(deltas[11], (11, -527_000));
        assert!(p0.target_state(&points).is_ok());
    }

    #[test]
    fn legacy_absolute_curve_blocks_blackwell_vmin_drift_but_keeps_plateaus_and_uniform_oc() {
        let p8_mv = [450, 500, 550, 620, 625, 635, 640, 645, 675, 800, 960, 1240];
        let p8_freqs = [
            180, 180, 180, 180, 225, 390, 547, 712, 1425, 2000, 2600, 3127,
        ];
        let p4_freqs = [
            180, 180, 180, 907, 1117, 1290, 1320, 1342, 1485, 2070, 2625, 3082,
        ];
        let mk_state = |freqs: &[u32]| VfState {
            nodes: p8_mv
                .iter()
                .zip(freqs)
                .enumerate()
                .map(|(i, (&mv, &mhz))| VfNode {
                    index: i as u16,
                    voltage_uv: mv * 1000,
                    frequency_khz: mhz * 1000,
                    delta_khz: 0,
                })
                .collect(),
            minimum_delta_khz: -1_000_000,
            maximum_delta_khz: 1_000_000,
            first_index: 0,
            last_index: (p8_mv.len() - 1) as u16,
        };
        let p8 = mk_state(&p8_freqs);
        let p4 = mk_state(&p4_freqs);

        // Legacy idle-saved curve applied under load (-900 MHz crater at 635 mV) -> blocked!
        let Value::VfCurve(legacy_p8) = p8.value().unwrap() else {
            panic!()
        };
        let err = p4.target_deltas(&legacy_p8).unwrap_err();
        assert!(err.contains("vf.err.legacy_drift"), "unexpected: {err}");

        // Legacy load-saved curve applied at idle (+900 MHz OC spike at 635 mV) -> blocked!
        let Value::VfCurve(legacy_p4) = p4.value().unwrap() else {
            panic!()
        };
        let err = p8.target_deltas(&legacy_p4).unwrap_err();
        assert!(err.contains("vf.err.legacy_drift"), "unexpected: {err}");

        // Same-state legacy apply still passes.
        assert!(p8.target_deltas(&legacy_p8).is_ok());

        // Legitimate legacy top-end undervolt plateau (-527 MHz on flat 2600 MHz ceiling) passes.
        let mut legacy_plateau = legacy_p8.clone();
        legacy_plateau[10].frequency_mhz = 2600;
        legacy_plateau[11].frequency_mhz = 2600;
        assert!(p8.target_deltas(&legacy_plateau).is_ok());

        // Legitimate legacy uniform +400 MHz whole-curve offset passes.
        let legacy_uniform_oc: Vec<VfPoint> = legacy_p8
            .iter()
            .map(|p| VfPoint {
                voltage_mv: p.voltage_mv,
                frequency_mhz: p.frequency_mhz + 400,
                delta_mhz: None,
            })
            .collect();
        assert!(p8.target_deltas(&legacy_uniform_oc).is_ok());
    }
}
