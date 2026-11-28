//! Platform-free preset editing: exact units and optimistic, per-field merging.
use crate::{
    features::Feature,
    message::msg,
    profiles::{DeviceBinding, PerformanceProfile, Setting, Value},
};
use std::collections::BTreeSet;

/// Parse a displayed integer/decimal without float rounding or NaN. scale=1000
/// displays watts/amperes while keeping the serialized milliwatt/milliamp units.
pub fn parse_number(input: &str, scale: i64) -> Result<i64, String> {
    if !matches!(scale, 1 | 1000) {
        return Err("Unsupported display scale".into());
    }
    let input = input.trim().replace(',', ".");
    let negative = input.starts_with('-');
    let unsigned = input.strip_prefix(['-', '+']).unwrap_or(&input);
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next().unwrap_or("");
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || parts.next().is_some()
        || (scale == 1 && !fraction.is_empty())
        || fraction.len() > 3
    {
        return Err(msg("preset.err.number", &[]));
    }
    let whole: i64 = whole
        .parse()
        .map_err(|_| msg("preset.err.too_large", &[]))?;
    let fraction: i64 = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<3}")
            .parse()
            .map_err(|_| msg("preset.err.decimals", &[]))?
    };
    let number = whole
        .checked_mul(scale)
        .and_then(|n| n.checked_add(fraction))
        .ok_or_else(|| msg("preset.err.too_large", &[]))?;
    if negative {
        number
            .checked_neg()
            .ok_or_else(|| msg("preset.err.too_small", &[]))
    } else {
        Ok(number)
    }
}
pub fn format_number(value: i64, scale: i64) -> String {
    if scale == 1 {
        return value.to_string();
    }
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    if magnitude % 1000 == 0 {
        format!("{sign}{}", magnitude / 1000)
    } else {
        format!("{sign}{}.{:03}", magnitude / 1000, magnitude % 1000)
            .trim_end_matches('0')
            .to_string()
    }
}
/// Apply only fields the user changed. Preserve another editor's independent
/// changes, hidden settings and the complete stored GPU curve byte-for-value.
/// The two stored Dynamic Boost names describe one control. Prefer the editor's
/// requested spelling, but display a legacy/new-only record as included too.
pub fn display_value(profile: &PerformanceProfile, feature: Feature) -> Option<&Value> {
    let exact = |wanted| {
        profile
            .settings
            .iter()
            .find(|s| s.feature == wanted)
            .map(|s| &s.value)
    };
    exact(feature).or_else(|| match feature {
        Feature::GpuUnleashed => exact(Feature::GpuDynamicBoost),
        Feature::GpuDynamicBoost => exact(Feature::GpuUnleashed),
        _ => None,
    })
}

pub fn merge(
    baseline: &PerformanceProfile,
    fresh: &PerformanceProfile,
    desired_name: &str,
    edits: &[(Feature, Option<Value>)],
) -> Result<PerformanceProfile, String> {
    merge_with_binding(baseline, fresh, desired_name, edits, None)
}

/// Like [`merge`], but supplies an optional live [`DeviceBinding`] so editing a
/// previously CPU-only preset to add NVIDIA GPU settings populates the required
/// GPU identity before validation runs.
pub fn merge_with_binding(
    baseline: &PerformanceProfile,
    fresh: &PerformanceProfile,
    desired_name: &str,
    edits: &[(Feature, Option<Value>)],
    gpu_binding: Option<&DeviceBinding>,
) -> Result<PerformanceProfile, String> {
    if baseline.id != fresh.id || baseline.binding != fresh.binding {
        return Err(msg("preset.err.binding_changed", &[]));
    }
    let mut result = fresh.clone();
    if desired_name != baseline.name {
        if fresh.name != baseline.name {
            return Err(msg("preset.err.name_changed", &[]));
        }
        result.name = desired_name.into();
    }
    let mut unique = BTreeSet::new();
    let get = |p: &PerformanceProfile, f: Feature| {
        p.settings
            .iter()
            .find(|s| s.feature == f)
            .map(|s| s.value.clone())
    };
    for (feature, desired) in edits {
        let domain = if *feature == Feature::GpuDynamicBoost {
            Feature::GpuUnleashed
        } else {
            *feature
        };
        if !unique.insert(domain) {
            return Err("Doppelte Preset-Einstellung".into());
        }
        if matches!(feature, Feature::GpuUnleashed | Feature::GpuDynamicBoost) {
            let domain = [Feature::GpuUnleashed, Feature::GpuDynamicBoost];
            let before_domain = domain.map(|f| get(baseline, f));
            let fresh_domain = domain.map(|f| get(fresh, f));
            let conflict = matches!(&before_domain,[Some(a),Some(b)] if a!=b);
            if display_value(baseline, *feature) == desired.as_ref() && !conflict {
                continue;
            }
            if before_domain != fresh_domain {
                return Err(msg(
                    "preset.err.changed_elsewhere",
                    &[&format!("@field.{feature:?}")],
                ));
            }
            let mut keys: Vec<_> = domain
                .into_iter()
                .filter(|f| get(fresh, *f).is_some())
                .collect();
            if keys.is_empty() {
                keys.push(*feature);
            }
            result.settings.retain(|s| !domain.contains(&s.feature));
            if let Some(value) = desired {
                for key in keys {
                    let setting = Setting {
                        feature: key,
                        value: value.clone(),
                    };
                    setting.validate()?;
                    result.settings.push(setting);
                }
            }
            continue;
        }
        let before = get(baseline, *feature);
        if before == *desired {
            continue;
        }
        if get(fresh, *feature) != before {
            return Err(msg(
                "preset.err.changed_elsewhere",
                &[&format!("@field.{feature:?}")],
            ));
        }
        result.settings.retain(|s| s.feature != *feature);
        if let Some(value) = desired {
            let setting = Setting {
                feature: *feature,
                value: value.clone(),
            };
            setting.validate()?;
            result.settings.push(setting);
        }
    }
    let needs_nvidia = result.settings.iter().any(|s| s.feature.needs_nvidia());
    if needs_nvidia
        && (result.binding.nvidia_uuid.is_none()
            || result.binding.nvidia_driver.is_none()
            || result.binding.nvidia_vbios.is_none())
        && let Some(gpu) = gpu_binding
    {
        if result.binding.nvidia_uuid.is_none() {
            result.binding.nvidia_uuid = gpu.nvidia_uuid.clone();
        }
        if result.binding.nvidia_driver.is_none() {
            result.binding.nvidia_driver = gpu.nvidia_driver.clone();
        }
        if result.binding.nvidia_vbios.is_none() {
            result.binding.nvidia_vbios = gpu.nvidia_vbios.clone();
        }
    }
    if gpu_binding.is_some() {
        result.binding = result.binding.for_settings(&result.settings);
    }
    result.validate()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{DeviceBinding, PROFILE_SCHEMA, VfPoint};
    #[test]
    fn boost_aliases_are_one_editable_control_without_losing_concurrent_changes() {
        let mut profile = PerformanceProfile {
            schema: PROFILE_SCHEMA,
            id: "aliases".into(),
            name: "Aliases".into(),
            binding: DeviceBinding {
                manufacturer: "HP".into(),
                board: "8E35".into(),
                bios: "F.13".into(),
                cpu_names: vec![],
                nvidia_uuid: Some("GPU".into()),
                nvidia_driver: Some("driver".into()),
                nvidia_vbios: Some("vbios".into()),
            },
            settings: vec![Setting {
                feature: Feature::GpuDynamicBoost,
                value: Value::Boolean(true),
            }],
        };
        let legacy = Feature::GpuUnleashed;
        assert_eq!(display_value(&profile, legacy), Some(&Value::Boolean(true)));
        let unchanged = merge(
            &profile,
            &profile,
            "Aliases",
            &[(legacy, Some(Value::Boolean(true)))],
        )
        .unwrap();
        assert_eq!(unchanged, profile);
        let off = merge(
            &profile,
            &profile,
            "Aliases",
            &[(legacy, Some(Value::Boolean(false)))],
        )
        .unwrap();
        assert_eq!(off.settings.len(), 1);
        assert_eq!(off.settings[0].feature, Feature::GpuDynamicBoost);
        assert_eq!(off.settings[0].value, Value::Boolean(false));
        profile.settings.push(Setting {
            feature: legacy,
            value: Value::Boolean(true),
        });
        let dual = merge(
            &profile,
            &profile,
            "Aliases",
            &[(legacy, Some(Value::Boolean(false)))],
        )
        .unwrap();
        assert_eq!(dual.settings.len(), 2);
        assert!(
            dual.settings
                .iter()
                .all(|s| s.value == Value::Boolean(false))
        );
        let removed = merge(&profile, &profile, "Aliases", &[(legacy, None)]).unwrap();
        assert!(removed.settings.is_empty());
        let mut foreign = profile.clone();
        foreign.settings[0].value = Value::Boolean(false);
        assert!(
            merge(
                &profile,
                &foreign,
                "Aliases",
                &[(legacy, Some(Value::Boolean(false)))]
            )
            .is_err()
        );
        let fixed = merge(
            &foreign,
            &foreign,
            "Aliases",
            &[(legacy, Some(Value::Boolean(true)))],
        )
        .unwrap();
        assert!(fixed.validate_intent().is_ok());
    }
    #[test]
    fn exact_display_units_reject_fraction_loss_nan_and_overflow() {
        assert_eq!(parse_number("45,125", 1000).unwrap(), 45125);
        assert_eq!(parse_number("-0.015", 1000).unwrap(), -15);
        assert_eq!(format_number(-15, 1000), "-0.015");
        for value in ["NaN", "inf", "1e3", "1.0001", "1.2.3", ""] {
            assert!(parse_number(value, 1000).is_err());
        }
        assert!(parse_number("0.5", 1).is_err());
        assert!(parse_number("9223372036854775807", 1000).is_err());
    }
    #[test]
    fn independent_edits_merge_full_curve_and_hidden_settings_but_conflicts_stop() {
        let base = PerformanceProfile {
            schema: PROFILE_SCHEMA,
            id: "merge".into(),
            name: "Original".into(),
            binding: DeviceBinding {
                manufacturer: "HP".into(),
                board: "8E35".into(),
                bios: "F.13".into(),
                cpu_names: vec!["Ryzen".into()],
                nvidia_uuid: Some("test".into()),
                nvidia_driver: Some("617.14".into()),
                nvidia_vbios: Some("test".into()),
            },
            settings: vec![
                Setting {
                    feature: Feature::GpuVfCurve,
                    value: Value::VfCurve(vec![
                        VfPoint {
                            voltage_mv: 700,
                            frequency_mhz: 1500,
                        },
                        VfPoint {
                            voltage_mv: 900,
                            frequency_mhz: 1800,
                        },
                    ]),
                },
                Setting {
                    feature: Feature::KeyboardColors,
                    value: Value::Colors(vec![[1, 2, 3]; 4]),
                },
                Setting {
                    feature: Feature::GpuMemoryOffsetMhz,
                    value: Value::Integer(0),
                },
            ],
        };
        let mut fresh = base.clone();
        fresh.name = "Concurrent rename".into();
        fresh.settings[0].value = Value::VfCurve(vec![
            VfPoint {
                voltage_mv: 700,
                frequency_mhz: 1515,
            },
            VfPoint {
                voltage_mv: 900,
                frequency_mhz: 1815,
            },
        ]);
        let edits = vec![(Feature::GpuMemoryOffsetMhz, Some(Value::Integer(-24)))];
        let result = merge(&base, &fresh, &base.name, &edits).unwrap();
        assert_eq!(result.name, fresh.name);
        assert_eq!(result.settings[..2], fresh.settings[..2]);
        assert!(merge(&base, &fresh, "My rename", &edits).is_err());
        fresh.settings[2].value = Value::Integer(24);
        assert!(merge(&base, &fresh, &base.name, &edits).is_err());
    }

    #[test]
    fn cpu_only_preset_accepts_new_nvidia_setting_with_live_gpu_binding() {
        let cpu_only = PerformanceProfile {
            schema: PROFILE_SCHEMA,
            id: "mein-preset".into(),
            name: "Mein Preset".into(),
            binding: DeviceBinding {
                manufacturer: "HP".into(),
                board: "8E35".into(),
                bios: "F.13".into(),
                cpu_names: vec!["AMD Ryzen 9 8940HX".into()],
                nvidia_uuid: None,
                nvidia_driver: None,
                nvidia_vbios: None,
            },
            settings: vec![Setting {
                feature: Feature::CpuTemperatureLimitC,
                value: Value::Integer(95),
            }],
        };
        let live_gpu = DeviceBinding {
            manufacturer: "HP".into(),
            board: "8E35".into(),
            bios: "F.13".into(),
            cpu_names: vec!["AMD Ryzen 9 8940HX".into()],
            nvidia_uuid: Some("GPU-3200".into()),
            nvidia_driver: Some("621.40".into()),
            nvidia_vbios: Some("98.06.2a.40.da".into()),
        };
        let merged = merge_with_binding(
            &cpu_only,
            &cpu_only,
            "Mein Preset",
            &[(Feature::GpuMaxClockMhz, Some(Value::Integer(2400)))],
            Some(&live_gpu),
        )
        .expect("adding an NVIDIA setting to a CPU-only preset must succeed");
        assert_eq!(merged.binding.nvidia_uuid.as_deref(), Some("GPU-3200"));
        assert_eq!(merged.binding.nvidia_driver.as_deref(), Some("621.40"));
        assert_eq!(
            merged.binding.nvidia_vbios.as_deref(),
            Some("98.06.2a.40.da")
        );

        let stripped = merge_with_binding(
            &merged,
            &merged,
            "Mein Preset",
            &[(Feature::GpuMaxClockMhz, None)],
            Some(&live_gpu),
        )
        .expect("removing the NVIDIA setting must succeed");
        assert_eq!(stripped.binding.nvidia_uuid, None);
    }
}
