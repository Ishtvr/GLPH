//! Declarative presentation of typed preset features. No register/API knowledge.
use crate::i18n::{t, tf};
use omen_core::{
    device_defaults::DeviceDefaultsRecord,
    features::{Feature as F, Unit, ValueKind},
    profiles::{Choice as C, Value},
};

/// Language keys of the collapsible groups, in display order,
/// cleanly separated into 4 independent sections: GPU, CPU, Thermals, Windows.
pub const GROUPS: [&str; 8] = [
    "group.gpu",
    "group.limits",
    "group.pbo",
    "group.curve_optimizer",
    "group.curve_optimizer_per_core",
    "group.thermals",
    "group.processor",
    "group.energy_display",
];
/// First group of each top-level section on the Overview page.
pub const SECTIONS: &[(usize, &str)] = &[
    (0, "section.gpu"),
    (1, "section.cpu"),
    (5, "section.thermals"),
    (6, "section.windows"),
];
pub struct Field {
    pub feature: F,
    pub group: usize,
    pub seed: i64,
}
/// Legacy keys remain readable. Explicit Save retires only the removed options.
/// Dynamic Boost has its own qualified adapter; it is not a retired control.
pub const REMOVED_GPU_OPTIONS: &[F] = &[
    F::GpuVoltageOffsetMv,
    F::GpuMemoryVoltageOffsetMv,
    F::GpuVoltageLimitMv,
    F::GpuCoreClockLimitMhz,
    F::GpuMemoryClockLimitMhz,
    F::GpuFrameRateLimit,
    // The HP mode is the dashboard's switch (30 October 2026).
    F::OmenPerformanceMode,
    // NVML refuses the power-limit GET/SET on the RTX 5070 Laptop (status 3,
    // not supported); direct fixed VBIOS TGP sets the GPU envelope instead.
    F::GpuPowerLimitMw,
];
/// Independent HP STAPM writes failed qualification. The CPU slow/fast
/// ceilings use their own measured RSMU messages, not an APU STAPM control.
/// APU power/VRM/soft-clock and integrated-graphics fields do not belong in the
/// requested CPU-only tuning UI. Retain decoding; retire intent on explicit Save.
pub const REMOVED_APU_OPTIONS: &[F] = &[
    F::CpuStapmLimitMw,
    F::CpuFastPptLimitMw,
    F::CpuStapmTimeSeconds,
    F::CpuSlowPptTimeSeconds,
    F::SocTdcMa,
    F::SocEdcMa,
    F::GfxTdcMa,
    F::GfxEdcMa,
    F::GfxCurveOptimizer,
    F::AmdIgpuClockMhz,
    F::IgpuSoftMinMhz,
    F::IgpuSoftMaxMhz,
    F::CpuSoftMinMhz,
    F::CpuSoftMaxMhz,
    F::DataSoftMinMhz,
    F::DataSoftMaxMhz,
    F::FabricSoftMinMhz,
    F::FabricSoftMaxMhz,
    F::VcnSoftMinMhz,
    F::VcnSoftMaxMhz,
    F::SocSoftMinMhz,
    F::SocSoftMaxMhz,
];
/// No Dragon Range control exists for these: the SMU and the original Hub
/// offer no AMD boost profile (Windows EPP/boost cover it) and no all-core
/// millivolt offset (Curve Optimizer is the Zen 4 offset). Fixed clocks,
/// and fixed voltages are removed at the user's request
/// (17 October 2026): most laptop firmware locks them, so the editor keeps
/// to the UXTU power, PBO and Curve Optimizer settings.
pub const REMOVED_CPU_OPTIONS: &[F] = &[
    F::CpuBoostProfile,
    F::CpuAllCoreVoltageOffsetMv,
    F::AmdCpuVoltageMv,
    F::AmdCpuClockMhz,
];
/// Max-Fan belongs to the separate cooling control. Legacy profile intent is
/// removed only by explicit Save; its recovery images remain fully supported.
pub const REMOVED_PRESET_OPTIONS: &[F] = &[F::MaxFan];
/// Volatile values the firmware and driver set again at every boot. While no
/// control ran since boot, the live reading is this device's own default.
#[allow(dead_code)]
pub const DEVICE_DEFAULTS: &[F] = &[
    F::CpuPptLimitMw,
    F::CpuSlowPptLimitMw,
    F::CpuTdcMa,
    F::CpuEdcMa,
    F::CpuTemperatureLimitC,
    F::CpuPboScalar,
    F::CpuCurveOptimizerAllCore,
    F::GpuCoreOffsetMhz,
    F::GpuMemoryOffsetMhz,
];
/// Independent watt and °C sliders (no cross-coupling between GPU, CPU & Thermals):
/// - `GpuDynamicBoostLimitMw` (Direct GPU Power Limit / TGP): 50 W up to
///   `gpu_tgp_max_w` (default 135 W, configurable up to 175 W in Settings).
/// - `CpuPptLimitMw` (Fast Limit / PPT): 15–160 W via direct AMD SMU.
/// - `CpuSlowPptLimitMw` (Slow Limit): 15–80 W via direct AMD SMU (hard SMU
///   firmware cap at 80 W verified on SMU 0x546F00).
/// - `CpuTemperatureLimitC`: 60–100 °C via direct AMD SMU (`RSMU 0x59`).
/// - `GpuTemperatureLimitC`: 65–87 °C via HP WMI `0x22` / NVML GPS target.
/// - `CpuSkinTemperatureLimitC`: 35–55 °C chassis IR surface guard (`0x23`).
pub fn slider_watts_with_gpu_max(
    feature: F,
    _dynamic_boost: bool,
    gpu_tgp_max_w: u32,
) -> Option<(i64, i64)> {
    let gpu_max = i64::from(gpu_tgp_max_w.clamp(
        omen_core::profiles::GPU_DYNAMIC_BOOST_LIMIT_MIN_W,
        omen_core::profiles::GPU_DYNAMIC_BOOST_LIMIT_HARD_MAX_W,
    ));
    match feature {
        F::GpuDynamicBoostLimitMw => Some((
            i64::from(omen_core::profiles::GPU_DIRECT_TGP_MIN_W),
            gpu_max,
        )),
        F::CpuPptLimitMw => Some((15, 160)),
        F::CpuSlowPptLimitMw => Some((15, 80)),
        F::HpSharedPowerMw => Some((55, 72)),
        F::CpuTemperatureLimitC => Some((60, 100)),
        F::GpuTemperatureLimitC => Some((65, 87)),
        F::CpuSkinTemperatureLimitC => Some((
            i64::from(omen_core::fan::IR_LIMIT_MIN_C),
            i64::from(omen_core::fan::IR_LIMIT_MAX_C),
        )),
        _ => None,
    }
}
#[cfg(test)]
pub fn slider_watts(feature: F, dynamic_boost: bool) -> Option<(i64, i64)> {
    slider_watts_with_gpu_max(
        feature,
        dynamic_boost,
        omen_core::profiles::GPU_DYNAMIC_BOOST_LIMIT_DEFAULT_MAX_W,
    )
}
pub fn retired(feature: F) -> bool {
    feature.retired()
        || REMOVED_GPU_OPTIONS.contains(&feature)
        || REMOVED_APU_OPTIONS.contains(&feature)
        || REMOVED_CPU_OPTIONS.contains(&feature)
        || REMOVED_PRESET_OPTIONS.contains(&feature)
}
/// Present the same Dynamic Boost spelling/precedence that Apply resolves.
/// Saving still uses the existing alias-aware optimistic merge in core.
pub fn display_value(
    profile: &omen_core::profiles::PerformanceProfile,
    feature: F,
) -> Option<&Value> {
    omen_core::preset::display_value(
        profile,
        if feature == F::GpuUnleashed {
            F::GpuDynamicBoost
        } else {
            feature
        },
    )
}
pub fn fields() -> Vec<Field> {
    let rows = [
        // Section 1: GPU (direct fixed TGP + clocks + V/F curve — 0 CPU coupling)
        (F::GpuDynamicBoostLimitMw, 0, 115000),
        (F::GpuCoreOffsetMhz, 0, 0),
        (F::GpuMemoryOffsetMhz, 0, 0),
        (F::GpuVfCurve, 0, 0),
        // Section 2: CPU (direct AMD SMU limits, Fmax boost, PBO, Curve Optimizer)
        (F::CpuPptLimitMw, 1, 80000),
        (F::CpuSlowPptLimitMw, 1, 55000),
        (F::CpuTdcMa, 1, 108000),
        (F::CpuEdcMa, 1, 160000),
        (F::CpuBoostLimitMhz, 2, 5350),
        (F::CpuPboScalar, 2, 1),
        (F::CpuCurveOptimizerAllCore, 3, 0),
        (F::CpuCurveOptimizerPerCore, 4, 0),
        // Section 3: Thermals (CPU Tctl, GPU GPS target, Chassis IR surface limit)
        (F::CpuTemperatureLimitC, 5, 100),
        (F::GpuTemperatureLimitC, 5, 87),
        (F::CpuSkinTemperatureLimitC, 5, 50),
        // Section 4: Windows (processor scheduling, power plans, display & MUX)
        (F::WindowsBoostMode, 6, 2),
        (F::WindowsMinimumProcessorPercent, 6, 5),
        (F::WindowsMaximumProcessorPercent, 6, 100),
        (F::WindowsProcessorEppPercent, 6, 50),
        (F::WindowsCoreParkingMinPercent, 6, 0),
        (F::WindowsCoreParkingMaxPercent, 6, 100),
        (F::CpuCcdAffinity, 6, 3),
        (F::WindowsEnergyMode, 7, 1),
        (F::WindowsPowerPlan, 7, 1),
        (F::DisplayRefreshRateHz, 7, 60),
        (F::GraphicsMode, 7, 1),
    ];
    rows.iter()
        .map(|(feature, group, seed)| Field {
            feature: *feature,
            group: *group,
            seed: *seed,
        })
        .collect()
}
impl Field {
    /// Preserve existing native automation IDs as new fields are inserted.
    /// Layout order is independent of control identity.
    pub fn control_slot(&self) -> usize {
        const LEGACY: &[(F, usize)] = &[
            (F::GpuCoreOffsetMhz, 0),
            (F::GpuMemoryOffsetMhz, 1),
            (F::GpuVfCurve, 2),
            (F::GpuUnleashed, 4),
            (F::CpuTemperatureLimitC, 5),
            (F::CpuSlowPptLimitMw, 6),
            (F::CpuPptLimitMw, 7),
            (F::CpuTdcMa, 8),
            (F::CpuEdcMa, 9),
            (F::CpuPboScalar, 10),
            (F::CpuCurveOptimizerAllCore, 11),
            (F::CpuCurveOptimizerPerCore, 12),
            (F::WindowsBoostMode, 18),
            (F::WindowsMinimumProcessorPercent, 19),
            (F::WindowsMaximumProcessorPercent, 20),
            (F::WindowsProcessorEppPercent, 21),
            (F::WindowsCoreParkingMinPercent, 22),
            (F::WindowsCoreParkingMaxPercent, 23),
            (F::CpuCcdAffinity, 24),
            (F::WindowsEnergyMode, 25),
            (F::WindowsPowerPlan, 26),
            (F::DisplayRefreshRateHz, 27),
            (F::GraphicsMode, 28),
        ];
        LEGACY
            .iter()
            .find_map(|(feature, slot)| (*feature == self.feature).then_some(*slot))
            .unwrap_or_else(|| {
                64 + F::all()
                    .iter()
                    .position(|f| *f == self.feature)
                    .expect("Catalog feature")
            })
    }
    /// Display name from the language file (`field.<Feature>`).
    pub fn label(&self) -> &'static str {
        t(&format!("field.{:?}", self.feature))
    }
    /// What the setting changes and what it does (`info.<Feature>`).
    pub fn info(&self) -> &'static str {
        t(&format!("info.{:?}", self.feature))
    }
    pub fn scale(&self) -> i64 {
        if matches!(
            self.feature.descriptor().unit,
            Unit::Milliwatt | Unit::Milliampere
        ) {
            1000
        } else {
            1
        }
    }
    pub fn unit(&self) -> &'static str {
        match self.feature.descriptor().unit {
            Unit::Mhz => "MHz",
            Unit::Milliwatt => "W",
            Unit::Milliampere => "A",
            Unit::Millivolt => "mV",
            Unit::Celsius => "°C",
            Unit::Seconds => "s",
            Unit::Percent => "%",
            Unit::Hz => "Hz",
            Unit::FramesPerSecond => "FPS",
            Unit::CurveStep => t("unit.steps"),
            Unit::None => "",
        }
    }
    pub fn choices(&self) -> &'static [C] {
        match self.feature {
            F::WindowsBoostMode => &[
                C::Disabled,
                C::Enabled,
                C::Aggressive,
                C::EfficientEnabled,
                C::EfficientAggressive,
                C::AggressiveAtGuaranteed,
                C::EfficientAggressiveAtGuaranteed,
            ],
            F::WindowsEnergyMode => &[C::BestEfficiency, C::Balanced, C::BestPerformance],
            F::WindowsPowerPlan => &[C::PowerSaver, C::Balanced, C::HighPerformance],
            F::GraphicsMode => &[C::IntegratedOnly, C::Hybrid, C::DiscreteOnly],
            _ => &[],
        }
    }
    pub fn initial(&self) -> Value {
        match self.feature.descriptor().value_kind {
            ValueKind::Choice => {
                let choices = self.choices();
                let idx = usize::try_from(self.seed)
                    .ok()
                    .filter(|&i| i < choices.len())
                    .unwrap_or(0);
                Value::Choice(choices[idx])
            }
            ValueKind::Boolean => Value::Boolean(self.seed != 0),
            ValueKind::PerCore => Value::PerCore(omen_core::device_defaults::zero_per_core(16)),
            ValueKind::VfCurve => Value::VfCurve(vec![]),
            _ => Value::Integer(self.seed),
        }
    }
}

pub fn choice_label(choice: C) -> &'static str {
    t(&format!("choice.{choice:?}"))
}

/// Formats this device's discovered Standard (Balanced) and Performance TDC/EDC
/// and Slow/PL1 defaults for the Settings summary line.
pub fn format_device_defaults_summary(record: &DeviceDefaultsRecord) -> String {
    let (std_tdc, std_edc) = record.mode_tdc_edc(false);
    let (perf_tdc, perf_edc) = record.mode_tdc_edc(true);
    let std_slow = record.mode_slow_ppt_mw(false);
    let perf_slow = record
        .modes
        .performance
        .as_ref()
        .and_then(|m| m.slow_ppt_mw);
    let amps = |v: Option<i64>| {
        v.map_or_else(
            || t("common.unknown").to_owned(),
            |ma| format!("{} A", ma / 1000),
        )
    };
    let watts = |v: Option<i64>| {
        v.map_or_else(
            || t("common.unknown").to_owned(),
            |mw| format!("{} W", mw / 1000),
        )
    };
    let board = if record.board.is_empty() {
        t("common.unknown")
    } else {
        record.board.as_str()
    };
    let bios = if record.bios.is_empty() {
        t("common.unknown")
    } else {
        record.bios.as_str()
    };
    tf(
        "settings.device_defaults_summary",
        &[
            &board,
            &bios,
            &amps(std_tdc),
            &amps(std_edc),
            &watts(std_slow),
            &amps(perf_tdc),
            &amps(perf_edc),
            &watts(perf_slow),
        ],
    )
}

/// Enriches a preset field's explanation text with the device's own discovered
/// Standard & Performance defaults where applicable.
pub fn field_info_with_defaults(field: &Field, record: Option<&DeviceDefaultsRecord>) -> String {
    let base = field.info();
    let Some(record) = record else {
        return base.to_owned();
    };
    let extra = match field.feature {
        F::CpuTdcMa => {
            let (std_tdc, _) = record.mode_tdc_edc(false);
            let (perf_tdc, _) = record.mode_tdc_edc(true);
            match (std_tdc, perf_tdc) {
                (Some(s), Some(p)) => Some(tf("defaults.info_tdc", &[&(s / 1000), &(p / 1000)])),
                _ => None,
            }
        }
        F::CpuEdcMa => {
            let (_, std_edc) = record.mode_tdc_edc(false);
            let (_, perf_edc) = record.mode_tdc_edc(true);
            match (std_edc, perf_edc) {
                (Some(s), Some(p)) => Some(tf("defaults.info_edc", &[&(s / 1000), &(p / 1000)])),
                _ => None,
            }
        }
        F::CpuSlowPptLimitMw => {
            let std_w = record.mode_slow_ppt_mw(false);
            let perf_w = record
                .modes
                .performance
                .as_ref()
                .and_then(|m| m.slow_ppt_mw);
            let unl_w = record.modes.unleashed.as_ref().and_then(|m| m.slow_ppt_mw);
            match (std_w, perf_w, unl_w) {
                (Some(s), Some(p), Some(u)) => Some(tf(
                    "defaults.info_slow_ppt",
                    &[&(s / 1000), &(p / 1000), &(u / 1000)],
                )),
                _ => None,
            }
        }
        F::CpuTemperatureLimitC => match record.get(F::CpuTemperatureLimitC) {
            Some(Value::Integer(c)) => Some(tf("defaults.info_tctl", &[c])),
            _ => None,
        },
        _ => None,
    };
    match extra {
        Some(line) => format!("{base}\n\n{line}"),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_boost_display_and_saved_edit_follow_apply_precedence() {
        use omen_core::profiles::{DeviceBinding, PerformanceProfile, Setting};
        let mut profile = PerformanceProfile {
            schema: 1,
            id: "alias-test".into(),
            name: "Alias".into(),
            binding: DeviceBinding {
                manufacturer: "HP".into(),
                board: "8E35".into(),
                bios: "F.13".into(),
                cpu_names: vec!["Test CPU".into()],
                nvidia_uuid: Some("GPU-test".into()),
                nvidia_driver: Some("test".into()),
                nvidia_vbios: Some("test".into()),
            },
            settings: vec![Setting {
                feature: F::GpuDynamicBoost,
                value: Value::Boolean(true),
            }],
        };
        assert_eq!(
            display_value(&profile, F::GpuUnleashed),
            Some(&Value::Boolean(true))
        );
        profile.settings.push(Setting {
            feature: F::GpuUnleashed,
            value: Value::Boolean(false),
        });
        assert_eq!(
            display_value(&profile, F::GpuUnleashed),
            Some(&Value::Boolean(true))
        );
        let edited = omen_core::preset::merge(
            &profile,
            &profile,
            "Alias",
            &[(F::GpuUnleashed, Some(Value::Boolean(false)))],
        )
        .unwrap();
        assert_eq!(
            display_value(&edited, F::GpuUnleashed),
            Some(&Value::Boolean(false))
        );
        assert!(
            edited
                .effective()
                .settings
                .iter()
                .all(|s| s.value == Value::Boolean(false))
        );
    }
    #[test]
    fn decoupled_sections_separate_gpu_cpu_and_windows_with_independent_sliders() {
        let group = |feature| {
            fields()
                .iter()
                .find(|f| f.feature == feature)
                .map(|f| f.group)
        };
        // GPU section holds direct GPU TGP + clocks + curve only (no CPU sliders!)
        for feature in [
            F::GpuDynamicBoostLimitMw,
            F::GpuCoreOffsetMhz,
            F::GpuMemoryOffsetMhz,
            F::GpuVfCurve,
        ] {
            assert_eq!(group(feature), Some(0), "{feature:?}");
        }
        // CPU limits section holds Fast PPT, Slow PPT, TDC, EDC together.
        for feature in [
            F::CpuPptLimitMw,
            F::CpuSlowPptLimitMw,
            F::CpuTdcMa,
            F::CpuEdcMa,
        ] {
            assert_eq!(group(feature), Some(1), "{feature:?}");
        }
        // Thermals section holds CPU Max-Temp, GPU Max-Temp, and Chassis Max-Temp.
        for feature in [
            F::CpuTemperatureLimitC,
            F::GpuTemperatureLimitC,
            F::CpuSkinTemperatureLimitC,
        ] {
            assert_eq!(group(feature), Some(5), "{feature:?}");
        }
        // CPU Boost & PBO holds Fmax + PBO Scalar.
        assert_eq!(group(F::CpuBoostLimitMhz), Some(2));
        assert_eq!(group(F::CpuPboScalar), Some(2));
        assert_eq!(GROUPS[0], "group.gpu");
        assert_eq!(GROUPS[1], "group.limits");
        assert_eq!(GROUPS[5], "group.thermals");
        assert_eq!(
            slider_watts(F::GpuDynamicBoostLimitMw, false),
            Some((50, 135))
        );
        assert_eq!(
            slider_watts_with_gpu_max(F::GpuDynamicBoostLimitMw, false, 160),
            Some((50, 160))
        );
        assert_eq!(
            slider_watts_with_gpu_max(F::GpuDynamicBoostLimitMw, false, 999),
            Some((50, 175))
        );
        // Fast PPT (15-160 W) and Slow PPT (15-80 W SMU cap) are independent of GPU flags.
        assert_eq!(slider_watts(F::CpuPptLimitMw, true), Some((15, 160)));
        assert_eq!(slider_watts(F::CpuPptLimitMw, false), Some((15, 160)));
        assert_eq!(slider_watts(F::CpuSlowPptLimitMw, true), Some((15, 80)));
        assert_eq!(slider_watts(F::CpuSlowPptLimitMw, false), Some((15, 80)));
        // Thermal sliders cover CPU (60-100 °C), GPU (65-87 °C), Chassis IR (35-55 °C).
        assert_eq!(
            slider_watts(F::CpuTemperatureLimitC, false),
            Some((60, 100))
        );
        assert_eq!(slider_watts(F::GpuTemperatureLimitC, false), Some((65, 87)));
        assert_eq!(
            slider_watts(F::CpuSkinTemperatureLimitC, false),
            Some((35, 55))
        );
        // The seeds start inside their sliders.
        for field in fields() {
            if let Some((lo, hi)) = slider_watts(field.feature, false) {
                let scale = field.scale();
                assert!(
                    (lo * scale..=hi * scale).contains(&field.seed),
                    "{:?}",
                    field.feature
                );
            }
        }
    }
    #[test]
    fn current_fields_have_no_retired_controls_or_empty_groups() {
        let fields = fields();
        for field in &fields {
            assert!(!retired(field.feature));
            assert!(F::all().contains(&field.feature));
            assert!(field.group < GROUPS.len());
            assert!(!(13..=17).contains(&field.control_slot()));
        }
        for group in 0..GROUPS.len() {
            assert!(fields.iter().any(|field| field.group == group));
        }
        assert_eq!(
            SECTIONS,
            &[
                (0, "section.gpu"),
                (1, "section.cpu"),
                (5, "section.thermals"),
                (6, "section.windows")
            ]
        );
        let old = serde_json::from_str::<F>("\"intel_pl1_mw\"").unwrap();
        assert!(retired(old));
        assert!(retired(F::MaxFan));
        assert!(
            !F::MaxFan.retired(),
            "The independent fan/recovery contract is still supported"
        );
    }
    #[test]
    fn new_power_field_keeps_chart_cpu_windows_control_ids_and_has_a_unique_identity() {
        let fields = fields();
        assert_eq!(
            fields
                .iter()
                .find(|f| f.feature == F::GpuVfCurve)
                .unwrap()
                .control_slot(),
            2
        );
        assert_eq!(
            fields
                .iter()
                .find(|f| f.feature == F::CpuPptLimitMw)
                .unwrap()
                .control_slot(),
            7
        );
        assert_eq!(
            fields
                .iter()
                .find(|f| f.feature == F::WindowsBoostMode)
                .unwrap()
                .control_slot(),
            18
        );
        let slots: std::collections::BTreeSet<_> = fields.iter().map(Field::control_slot).collect();
        assert_eq!(slots.len(), fields.len());
        assert!(
            fields
                .iter()
                .find(|f| f.feature == F::CpuBoostLimitMhz)
                .unwrap()
                .control_slot()
                >= 64
        );
        // Unsupported on this GPU, hidden from the editor.
        assert!(fields.iter().all(|f| f.feature != F::GpuPowerLimitMw));
        // Coupled legacy HP Dynamic Boost / TPP rows are no longer shown on Overview.
        assert!(fields.iter().all(|f| !matches!(
            f.feature,
            F::GpuUnleashed | F::GpuCustomTgp | F::HpSharedPowerMw
        )));
        assert!(!F::GpuCustomTgp.needs_nvidia() && F::GpuVfCurve.needs_nvidia());
        // Sane factory seeds: 100 °C Tctl, Balanced Windows defaults, Hybrid MUX, 16 zeroed cores.
        let initial = |feature| {
            fields
                .iter()
                .find(|f| f.feature == feature)
                .unwrap()
                .initial()
        };
        assert_eq!(initial(F::CpuTemperatureLimitC), Value::Integer(100));
        assert_eq!(initial(F::WindowsPowerPlan), Value::Choice(C::Balanced));
        assert_eq!(initial(F::WindowsEnergyMode), Value::Choice(C::Balanced));
        assert_eq!(initial(F::WindowsBoostMode), Value::Choice(C::Aggressive));
        assert_eq!(initial(F::GraphicsMode), Value::Choice(C::Hybrid));
        assert!(matches!(
            initial(F::CpuCurveOptimizerPerCore),
            Value::PerCore(ref cores) if cores.len() == 16 && cores.iter().all(|c| c.steps == 0)
        ));
        let rec = omen_core::device_defaults::resolve_defaults("8E35", "F.13", None);
        let summary = format_device_defaults_summary(&rec);
        assert!(summary.contains("108 A") && summary.contains("160 A"));
        let tdc_field = fields.iter().find(|f| f.feature == F::CpuTdcMa).unwrap();
        assert!(field_info_with_defaults(tdc_field, Some(&rec)).contains("108 A"));
        assert!(!DEVICE_DEFAULTS.is_empty());
    }
}
