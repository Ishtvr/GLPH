//! Explicit CLI entry points. Profile persistence is separate from hardware apply.
use glph_core::{
    features::{Feature, ValueKind},
    profiles::{Choice, PROFILE_SCHEMA, PerformanceProfile, Setting, Value},
    transaction::{self, Backend},
};
use glph_platform::{
    ControlLease, Controller, RestoreState,
    profile_store::{ProfileStore, profile_digest},
};
use serde::Serialize;
use std::path::Path;

fn print<T: Serialize>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn parse_value(feature: Feature, text: &str) -> Result<Value, String> {
    if feature.retired() {
        return Err(crate::i18n::t("profile.err.removed_intel").into());
    }
    match feature.descriptor().value_kind {
        ValueKind::Integer => text
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|e| e.to_string()),
        ValueKind::Boolean => text
            .parse::<bool>()
            .map(Value::Boolean)
            .map_err(|e| e.to_string()),
        ValueKind::Choice => {
            let encoded = serde_json::to_string(text).map_err(|e| e.to_string())?;
            serde_json::from_str::<Choice>(&encoded)
                .map(Value::Choice)
                .map_err(|e| e.to_string())
        }
        _ => serde_json::from_str::<Value>(text).map_err(|e| e.to_string()),
    }
}

pub fn handles(command: &str) -> bool {
    matches!(
        command,
        "gpu-capabilities"
            | "system-validate-safe"
            | "fan-validate-max"
            | "keyboard-validate-colors"
            | "keyboard-validate-lighting"
            | "keyboard-ui-read"
            | "gpu-curve-read"
            | "gpu-validate-curve"
            | "gpu-validate-memory-curve"
            | "gpu-validate"
            | "profile-catalog"
            | "profile-create"
            | "profile-list"
            | "profile-show"
            | "profile-set"
            | "profile-set-vf"
            | "profile-import"
            | "profile-plan"
            | "profile-verify-current"
            | "profile-apply"
            | "profile-apply-trace"
            | "profile-restore"
            | "profile-ui-apply"
            | "profile-ui-replace"
            | "mode-ui-replace"
            | "mode-ui-apply"
            | "mode-ui-unleashed"
            | "hp-performance-step"
            | "cpu-smu-probe"
            | "fan-service"
            | "fan-service-stop"
            | "mux-ui-apply"
            | "hp-service-ui"
            | "hp-service-restore"
            | "fan-service-install"
            | "service-ui"
            | "cpu-smu-calibrate-limits"
            | "profile-ui-restore"
            | "profile-ui-restore-keep"
            | "profile-ui-startup-enable"
            | "profile-startup-enable"
            | "profile-startup-disable"
            | "profile-startup-apply"
            | "profile-startup-status"
            | "profile-startup-check"
            | "profile-storage"
            | "profile-migrate-legacy"
            | "nvpcf-ppt-status"
            | "nvpcf-ppt-restore"
            | "gpu-tgp-status"
            | "gpu-tgp-restore"
            | "device-defaults-detect"
            | "device-defaults-ui-detect"
            | "keyboard-ui-direct-apply"
    )
}

/// Every fixed GPU trial keeps the full production recovery checkpoint. Even a
/// failed trial may return to that checkpoint only after complete live restore
/// and an exact, closed trial journal; a pending recovery is never hidden.
fn gpu_qualification(store: &mut ProfileStore, command: &str) -> Result<(), String> {
    let _lease = ControlLease::acquire_foreground()?;
    let checkpoint = store.checkpoint_completed_journal()?;
    let mut controller = Controller::new(true, None)?;
    let before_gpu = controller.gpu_report()?;
    let before_curve = controller.gpu_curve()?;
    let (id, digests): (&str, &[&str]) = match command {
        "gpu-validate" => (
            "driver-validation",
            &[
                "fixed-driver-validation-v3-negative",
                "fixed-driver-validation-v3-positive",
            ],
        ),
        "gpu-validate-curve" => ("vf-driver-validation", &["fixed-vf-frequency-trial-v1"]),
        "gpu-validate-memory-curve" => (
            "memory-curve-validation",
            &["fixed-memory-curve-preservation-v1"],
        ),
        _ => return Err("Unknown fixed GPU qualification".into()),
    };
    let result = match command {
        "gpu-validate" => controller
            .validate_gpu_offsets(store)
            .and_then(|value| serde_json::to_value(value).map_err(|e| e.to_string())),
        "gpu-validate-curve" => controller
            .validate_gpu_curve(store)
            .and_then(|value| serde_json::to_value(value).map_err(|e| e.to_string())),
        _ => controller
            .validate_memory_curve_preservation(store)
            .and_then(|value| serde_json::to_value(value).map_err(|e| e.to_string())),
    };
    let after_gpu = controller.gpu_report();
    let after_curve = controller.gpu_curve();
    let final_check: Result<(), String> = (|| {
        let gpu = after_gpu.as_ref().map_err(Clone::clone)?;
        let curve = after_curve.as_ref().map_err(Clone::clone)?;
        if before_gpu.identity != gpu.identity
            || before_gpu.core != gpu.core
            || before_gpu.memory != gpu.memory
            || !before_curve.controls_match(curve)?
        {
            return Err("Fixed GPU trial did not restore every original GPU control".into());
        }
        Ok(())
    })();
    if final_check.is_ok() {
        let completed = store.last_transaction::<RestoreState>()?;
        if completed.profile_id == id && digests.contains(&completed.profile_digest.as_str()) {
            store.restore_completed_trial_checkpoint(checkpoint, id, &completed.profile_digest)?;
        } else if result.is_ok() {
            return Err("Completed GPU trial journal identity differs".into());
        }
    }
    print(
        &serde_json::json!({"before":before_gpu,"before_curve":before_curve,"transaction":result,
        "after":after_gpu,"after_curve":after_curve,"final_check":final_check,
        "hardware_scope":"fixed MHz offsets/curve points; complete immediate restore; no voltage/power field writes"}),
    )?;
    result?;
    final_check
}

/// Runs optional HP vendor prelude ONLY when the device is HP AND the preset
/// explicitly includes legacy HP mode flags (`GpuUnleashed`, `GpuDynamicBoost`,
/// or `HpSharedPowerMw`). Decoupled PerformanceHub presets (direct GPU TGP +
/// direct SMU CPU limits + Windows settings) execute ZERO HP WMI calls.
fn apply_hp_prelude(
    profile: &glph_core::profiles::PerformanceProfile,
    inventory: &glph_core::Inventory,
) -> Result<Vec<String>, String> {
    use glph_core::hp_mode::HpMode;
    let shared_budget = HpMode::shared_budget_of_profile(profile)?;
    if inventory.manufacturer != "HP" {
        return Ok(vec![]);
    }
    let ac = glph_platform::power_status().ac_connected == Some(true);
    let mut failed = vec![];
    if let Some(mode) = HpMode::of_preset(profile, ac) {
        if let Err(error) = glph_platform::apply_hp_mode(inventory, mode) {
            failed.push(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        if let Some(watts) = HpMode::effective_pl1_for_preset(profile, mode)
            && !(mode == HpMode::Unleashed && !(45..=80).contains(&watts))
            && let Err(error) = glph_platform::write_hp_cpu_pl1(inventory, watts)
        {
            failed.push(error);
        }
    }
    if let Some(watts) = shared_budget
        && let Err(error) = glph_platform::write_hp_shared_budget(inventory, watts)
    {
        failed.push(error);
    }
    Ok(failed)
}

fn prepare_apply(
    profile: &glph_core::profiles::PerformanceProfile,
    store: &mut ProfileStore,
    startup: bool,
    replace_pending: bool,
) -> Result<Controller, String> {
    // Recovery and preflight precede even the mode/TPP prelude.
    if replace_pending {
        recover_pending(store)?;
    } else if store.pending()? || store.fan_policy_pending()? {
        return Err(glph_core::message::msg("store.err.pending_checkpoint", &[]));
    }
    let mut controller = Controller::for_profile(profile)?;
    let checked = transaction::plan(profile, &mut controller, startup);
    if !checked.applicable {
        return Err(checked
            .blockers
            .iter()
            .map(|b| b.reason.as_str())
            .collect::<Vec<_>>()
            .join("; "));
    }
    Ok(controller)
}

/// The recorded original is restored/verified before any replacement prelude.
/// Recovery failure retains the pending journal and dispatches no new target.
fn recover_pending(store: &mut ProfileStore) -> Result<(), String> {
    if store.pending()? || store.fan_pending_direct_applied()? {
        let saved = store.last_transaction::<RestoreState>()?;
        let mut controller = Controller::for_recovery(&saved)?;
        transaction::recover(&saved, &mut controller, store)
            .map_err(|e| serde_json::to_string_pretty(&e).unwrap_or(e.error))?;
    }
    crate::profile_fan::recover_pending(store)?;
    Ok(())
}

fn save_ui_result(
    store: &ProfileStore,
    nonce: Option<&str>,
    operation: &str,
    record: &serde_json::Value,
) -> Result<(), String> {
    if let Some(n) = nonce {
        store.save_nonce_control_result(n, record)?;
    }
    if matches!(operation, "apply" | "mode" | "restore") {
        store.save_last_apply_result(record)?;
    }
    store.save_control_result(record)
}

/// Returns `true` when applying `profile` writes GPU overclocking/V/F/TGP or
/// CPU/APU Curve Optimizer, Fmax boost, PBO scalar, or voltage/clock tuning
/// that can trigger a delayed BSOD/freeze (20–30 s after apply) and therefore
/// must arm the Boot-Crash-Guard anti-boot-loop marker.
fn profile_has_stability_risk(profile: &PerformanceProfile) -> bool {
    profile.settings.iter().any(|s| {
        matches!(
            s.feature,
            Feature::GpuVfCurve
                | Feature::GpuCoreOffsetMhz
                | Feature::GpuMemoryOffsetMhz
                | Feature::GpuVoltageOffsetMv
                | Feature::GpuMemoryVoltageOffsetMv
                | Feature::GpuDynamicBoostLimitMw
                | Feature::CpuCurveOptimizerAllCore
                | Feature::CpuCurveOptimizerPerCore
                | Feature::GfxCurveOptimizer
                | Feature::CpuBoostLimitMhz
                | Feature::CpuPboScalar
                | Feature::CpuAllCoreVoltageOffsetMv
                | Feature::AmdCpuVoltageMv
                | Feature::AmdCpuClockMhz
                | Feature::AmdIgpuClockMhz
        )
    })
}

/// Compensates pre-transaction GPU/NVPCF/PPAB writes if a subsequent step
/// fails before or during `transaction::apply`.
fn rollback_pre_transaction(
    inventory: &glph_core::Inventory,
    store: &ProfileStore,
    tgp_applied_now: bool,
    nvpcf_changed: bool,
    ppab_prior: Option<(bool, bool)>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some((prior_ctgp, prior_ppab)) = ppab_prior
        && let Err(e) = glph_platform::write_hp_gpu_power(inventory, prior_ctgp, prior_ppab)
    {
        errors.push(format!("HP GPU power rollback failed: {e}"));
    }
    if nvpcf_changed && let Err(e) = glph_platform::nvpcf_ppt::restore_if_saved(inventory, store) {
        errors.push(format!("NVPCF PPT rollback failed: {e}"));
    }
    if tgp_applied_now
        && let Err(e) = glph_platform::vbios_tgp::restore_if_active(
            inventory,
            store,
            glph_platform::read_nvml_power_limits,
        )
    {
        errors.push(format!("VBIOS TGP rollback failed: {e}"));
    }
    errors
}

pub fn run(args: &[String]) -> Result<(), String> {
    let result = run_inner(args);
    // Return early errors (e.g. access/session/empty journal) to the matching UI
    // request, without overwriting a more detailed transaction/rollback result
    // and without masking the original hardware error if result recording fails.
    if let Err(error) = &result {
        let ui = match args.first().map(String::as_str) {
            Some("keyboard-ui-read") if args.len() == 2 || args.len() == 3 => {
                Some(("keyboard_read", args[1].as_str()))
            }
            Some("profile-ui-apply" | "profile-ui-replace") if args.len() == 3 => {
                Some(("apply", args[2].as_str()))
            }
            Some("mode-ui-apply" | "mode-ui-replace") if args.len() == 3 => {
                Some(("mode", args[2].as_str()))
            }
            Some("mode-ui-unleashed") if args.len() == 7 => Some(("mode", args[6].as_str())),
            Some("fan-service-install") if args.len() == 2 => Some(("fan", args[1].as_str())),
            Some("service-ui") if args.len() == 3 => Some(("hardware_service", args[2].as_str())),
            Some("mux-ui-apply") if args.len() == 3 => Some(("mux", args[2].as_str())),
            Some("hp-service-ui") if args.len() == 3 => Some(("hp_service", args[2].as_str())),
            Some("profile-ui-restore") if args.len() == 2 => Some(("restore", args[1].as_str())),
            Some("profile-ui-restore" | "profile-ui-restore-keep") if args.len() == 3 => {
                Some(("restore", args[2].as_str()))
            }
            Some("profile-ui-startup-enable") if args.len() == 3 => {
                Some(("startup", args[2].as_str()))
            }
            Some("device-defaults-ui-detect") if args.len() == 2 => {
                Some(("device_defaults", args[1].as_str()))
            }
            Some("keyboard-ui-direct-apply") if args.len() == 7 => {
                Some(("keyboard_apply", args[6].as_str()))
            }
            _ => None,
        };
        if let Some((operation, nonce)) = ui
            && validate_nonce(Some(nonce)).is_ok()
            && let Ok(store) = ProfileStore::user_data()
        {
            let already_recorded = store
                .control_result()
                .is_ok_and(|r| r["nonce"].as_str() == Some(nonce));
            let record = serde_json::json!({
                "schema": 1,
                "operation": operation,
                "nonce": nonce,
                "result": { "Err": { "error": error, "rollback_errors": [] } }
            });
            if !already_recorded {
                let _ = save_ui_result(&store, Some(nonce), operation, &record);
            } else {
                let _ = store.save_nonce_control_result(nonce, &record);
            }
        }
    }
    result
}

fn run_inner(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("system-validate-safe") if args.len() == 3 && args[2] == "--confirm-safe-writes" => {
            return crate::system_validation::run(&args[1]);
        }
        Some("cpu-smu-calibrate-limits") if args.len() == 1 => {
            // Fixed approved qualification: lower each limit once, find its
            // PM-table cell, write the previous value back.
            let _lease = ControlLease::acquire_foreground()?;
            let report = glph_platform::smu_calibrate_limits(&glph_platform::inventory())?;
            let store = ProfileStore::user_data()?;
            std::fs::write(
                store.root().join("smu-limit-calibration.json"),
                serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            return print(&report);
        }
        Some("mux-ui-apply") if args.len() == 3 => {
            validate_nonce(Some(&args[2]))?;
            let mode = glph_core::quick_switch::GraphicsMode::parse(&args[1])
                .ok_or("Unknown graphics mode")?;
            let _lease = ControlLease::acquire_foreground()?;
            let report = glph_platform::apply_graphics_mode(&glph_platform::inventory(), mode)?;
            let store = ProfileStore::user_data()?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_secs();
            let record = glph_core::quick_switch::GraphicsRecord::after_write(
                store.graphics_mode(),
                mode,
                report.previous,
                now,
                glph_platform::boot_unix_s(),
            );
            store.save_graphics_mode(&record)?;
            save_ui_result(
                &store,
                Some(&args[2]),
                "mux",
                &serde_json::json!({"schema":1,"operation":"mux","nonce":args[2],"result":{"Ok":report}}),
            )?;
            return print(&report);
        }
        Some("hp-service-ui") if args.len() == 3 => {
            // Fixed target: only HPOmenCap, only its start type and run state.
            validate_nonce(Some(&args[2]))?;
            let disable = match args[1].as_str() {
                "disable" => true,
                "restore" => false,
                _ => return Err("Unknown HP service action".into()),
            };
            let _lease = ControlLease::acquire_foreground()?;
            let report = glph_platform::hp_control::set_oem_service_disabled(disable)?;
            save_ui_result(
                &ProfileStore::user_data()?,
                Some(&args[2]),
                "hp_service",
                &serde_json::json!({"schema":1,"operation":"hp_service","nonce":args[2],"result":{"Ok":report}}),
            )?;
            return print(&report);
        }
        Some("hp-service-restore") if args.len() == 1 => {
            // Uninstaller: return HPOmenCap only if this app disabled it.
            if ProfileStore::user_data()?.hp_service_start_type().is_none() {
                return print(&serde_json::json!({"restored":false}));
            }
            let _lease = ControlLease::acquire_foreground()?;
            let report = glph_platform::hp_control::set_oem_service_disabled(false)?;
            return print(&serde_json::json!({"restored":true,"service":report}));
        }
        Some("fan-service-stop") if args.len() == 1 => {
            return print(
                &serde_json::json!({"stop_requested":glph_platform::stop_fan_service()?}),
            );
        }
        Some("fan-service") if args.len() == 1 => {
            // Runs for the whole logon session from the protected task copy.
            let store = ProfileStore::user_data()?;
            let status = ProfileStore::user_data()?;
            return glph_platform::run_fan_service(
                &glph_platform::inventory(),
                || store.fan_mode(),
                || store.active_fan_curve(),
                || store.ccd_selection(),
                |s| {
                    let _ = status.save_fan_status(s);
                },
            );
        }
        Some("fan-service-install") if args.len() == 2 => {
            validate_nonce(Some(&args[1]))?;
            crate::startup_setup::ensure_fan_service()?;
            save_ui_result(
                &ProfileStore::user_data()?,
                Some(&args[1]),
                "fan",
                &serde_json::json!({"schema":1,"operation":"fan","nonce":args[1],"result":{"Ok":true}}),
            )?;
            return print(&serde_json::json!({"fan_service":"installed"}));
        }
        Some("service-ui") if args.len() == 3 => {
            validate_nonce(Some(&args[2]))?;
            let mode = match args[1].as_str() {
                "install" => "Install",
                "start" => "Start",
                "stop" => "Stop",
                "remove" => "Remove",
                _ => return Err("Unknown hardware service action".into()),
            };
            crate::startup_setup::manage_windows_service(mode)?;
            if matches!(mode, "Install" | "Start")
                && let Ok(store) = ProfileStore::user_data()
            {
                let cfg = glph_core::service::ServiceConfig {
                    schema: glph_core::service::SERVICE_CONFIG_SCHEMA,
                    fan_mode: store.fan_mode(),
                    fan_curve: store.active_fan_curve().ok().flatten(),
                    chassis_max_temp_c: store.chassis_max_temp_c().ok().flatten(),
                    paused: false,
                };
                let _ = glph_platform::service_client::sync_service_config_if_running(&cfg);
            }
            save_ui_result(
                &ProfileStore::user_data()?,
                Some(&args[2]),
                "hardware_service",
                &serde_json::json!({"schema":1,"operation":"hardware_service","nonce":args[2],"result":{"Ok":mode}}),
            )?;
            return print(&serde_json::json!({"hardware_service":mode}));
        }
        Some("cpu-smu-probe") if args.len() == 1 => {
            // Read-only: SMU version, PM table head, PBO scalar and CO margins.
            let report = glph_platform::smu_probe(&glph_platform::inventory())?;
            let store = ProfileStore::user_data()?;
            std::fs::write(
                store.root().join("smu-probe.json"),
                serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            return print(&report);
        }
        Some("nvpcf-ppt-status") if args.len() == 1 => {
            let store = ProfileStore::user_data()?;
            return print(&glph_platform::nvpcf_ppt::inspect(
                &glph_platform::inventory(),
                &store,
            ));
        }
        Some("nvpcf-ppt-restore") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            let store = ProfileStore::user_data()?;
            let restored =
                glph_platform::nvpcf_ppt::restore_if_saved(&glph_platform::inventory(), &store)?;
            return print(&serde_json::json!({"restored": restored}));
        }
        Some("gpu-tgp-status") if args.len() == 1 => {
            let store = ProfileStore::user_data()?;
            return print(&glph_platform::vbios_tgp::inspect(
                &glph_platform::inventory(),
                &store,
            ));
        }
        Some("gpu-tgp-restore") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            let store = ProfileStore::user_data()?;
            let restored = glph_platform::vbios_tgp::restore_if_active(
                &glph_platform::inventory(),
                &store,
                glph_platform::read_nvml_power_limits,
            )?;
            return print(&serde_json::json!({"restored": restored}));
        }
        Some("device-defaults-detect") if args.len() == 1 => {
            let store = ProfileStore::user_data()?;
            let record =
                glph_platform::discover_device_defaults(&glph_platform::inventory(), &store)?;
            return print(&record);
        }
        Some("device-defaults-ui-detect") if args.len() == 2 => {
            validate_nonce(Some(&args[1]))?;
            let store = ProfileStore::user_data()?;
            let record =
                glph_platform::discover_device_defaults(&glph_platform::inventory(), &store)?;
            save_ui_result(
                &store,
                Some(&args[1]),
                "device_defaults",
                &serde_json::json!({
                    "schema": 1,
                    "operation": "device_defaults",
                    "nonce": args[1],
                    "result": { "Ok": record }
                }),
            )?;
            return print(&record);
        }
        Some("mode-ui-apply" | "mode-ui-replace") if args.len() == 3 => {
            validate_nonce(Some(&args[2]))?;
            let mode = glph_core::hp_mode::HpMode::parse(&args[1])?;
            let _lease = ControlLease::acquire_foreground()?;
            let mut user_store = ProfileStore::user_data()?;
            if args[0] == "mode-ui-replace" {
                recover_pending(&mut user_store)?;
            } else if user_store.pending()? || user_store.fan_policy_pending()? {
                return Err(glph_core::message::msg("store.err.pending_checkpoint", &[]));
            }
            let inventory = glph_platform::inventory();
            glph_platform::nvpcf_ppt::restore_if_saved(&inventory, &user_store)?;
            glph_platform::vbios_tgp::restore_if_active(
                &inventory,
                &user_store,
                glph_platform::read_nvml_power_limits,
            )?;
            let report = glph_platform::apply_hp_mode(&inventory, mode)?;
            save_ui_result(
                &user_store,
                Some(&args[2]),
                "mode",
                &serde_json::json!({"schema":1,"operation":"mode","nonce":args[2],"result":{"Ok":report}}),
            )?;
            return print(&report);
        }
        Some("mode-ui-unleashed") if args.len() == 7 => {
            validate_nonce(Some(&args[6]))?;
            let parse_u8 = |s: &str, name: &str| {
                s.parse::<u8>()
                    .map_err(|_| format!("Invalid Unleashed {name}: {s:?}"))
            };
            let parse_bool = |s: &str, name: &str| match s {
                "0" => Ok(false),
                "1" => Ok(true),
                _ => Err(format!("Invalid Unleashed {name}: {s:?}")),
            };
            let opts = glph_core::hp_mode::HpUnleashedOptions {
                cpu_pl1_w: parse_u8(&args[1], "cpu_pl1_w")?,
                shared_budget_w: parse_u8(&args[2], "shared_budget_w")?,
                custom_tgp: parse_bool(&args[3], "custom_tgp")?,
                dynamic_boost: parse_bool(&args[4], "dynamic_boost")?,
                gpu_max_temp_c: parse_u8(&args[5], "gpu_max_temp_c")?,
            };
            opts.validate()?;
            let _lease = ControlLease::acquire_foreground()?;
            let mut user_store = ProfileStore::user_data()?;
            recover_pending(&mut user_store)?;
            let inventory = glph_platform::inventory();
            glph_platform::nvpcf_ppt::restore_if_saved(&inventory, &user_store)?;
            glph_platform::vbios_tgp::restore_if_active(
                &inventory,
                &user_store,
                glph_platform::read_nvml_power_limits,
            )?;
            let report = glph_platform::apply_hp_unleashed(&inventory, opts)?;
            save_ui_result(
                &user_store,
                Some(&args[6]),
                "mode",
                &serde_json::json!({"schema":1,"operation":"mode","nonce":args[6],"result":{"Ok":report}}),
            )?;
            return print(&report);
        }
        Some("hp-performance-step") if args.len() == 2 => {
            // Measurement only: the Hub's Performance slider step L5/L6 thermal
            // byte. Run `mode-ui-apply performance` first for flags, TPP and PL1.
            let _lease = ControlLease::acquire_foreground()?;
            glph_platform::write_hp_performance_step(&glph_platform::inventory(), &args[1])?;
            save_ui_result(
                &ProfileStore::user_data()?,
                None,
                "mode",
                &serde_json::json!({"schema":1,"operation":"mode","nonce":null,"result":{"Ok":{"performance_step":args[1]}}}),
            )?;
            return print(&serde_json::json!({"performance_step":args[1],"readback":false}));
        }
        Some("keyboard-ui-read") if args.len() == 2 || args.len() == 3 => {
            validate_nonce(Some(&args[1]))?;
            let store = ProfileStore::user_data()?;
            let mut controller = if let Some(id) = args.get(2) {
                Controller::for_profile(&store.load(id)?)?
            } else {
                Controller::new(false, None)?
            };
            let state = controller.keyboard_state()?;
            let plan = args
                .get(2)
                .map(|id| {
                    store
                        .load(id)
                        .map(|p| transaction::plan(&p, &mut controller, false))
                })
                .transpose()?;
            let result = serde_json::json!({"state":state,"plan":plan,"binding":controller.context()?.binding});
            save_ui_result(
                &store,
                Some(&args[1]),
                "keyboard_read",
                &serde_json::json!({"schema":1,"operation":"keyboard_read","nonce":args[1],"result":{"Ok":result}}),
            )?;
            return print(&result);
        }
        Some("keyboard-ui-direct-apply") if args.len() == 7 => {
            validate_nonce(Some(&args[6]))?;
            let parse_rgb = |hex: &str| -> Result<[u8; 3], String> {
                if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("Invalid keyboard RGB hex: {hex:?}"));
                }
                Ok([
                    u8::from_str_radix(&hex[0..2], 16).map_err(|e| e.to_string())?,
                    u8::from_str_radix(&hex[2..4], 16).map_err(|e| e.to_string())?,
                    u8::from_str_radix(&hex[4..6], 16).map_err(|e| e.to_string())?,
                ])
            };
            let colors = [
                parse_rgb(&args[1])?,
                parse_rgb(&args[2])?,
                parse_rgb(&args[3])?,
                parse_rgb(&args[4])?,
            ];
            let enabled = match args[5].as_str() {
                "0" => false,
                "1" => true,
                other => return Err(format!("Invalid keyboard power flag: {other:?}")),
            };
            let mut control = glph_platform::DirectLighting::new()?;
            let current = control.read()?;
            let after = control.apply(&current, &colors, enabled)?;
            let store = ProfileStore::user_data()?;
            save_ui_result(
                &store,
                Some(&args[6]),
                "keyboard_apply",
                &serde_json::json!({"schema":1,"operation":"keyboard_apply","nonce":args[6],"result":{"Ok":after}}),
            )?;
            return print(&after);
        }
        Some("gpu-curve-read") if args.len() == 1 => {
            let mut controller = Controller::new(true, None)?;
            return print(&controller.gpu_curve()?);
        }
        Some("profile-storage") if args.len() == 1 => {
            return print(&serde_json::json!({"directory":ProfileStore::user_data()?.root()}));
        }
        Some("profile-migrate-legacy") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            let store = ProfileStore::user_data()?;
            return print(
                &serde_json::json!({"copied_profiles":store.migrate_legacy()?,"directory":store.root(),"startup_enabled":false,"hardware_applied":false}),
            );
        }
        Some("profile-catalog") if args.len() == 1 => {
            return print(
                &Feature::all()
                    .iter()
                    .map(|f| f.descriptor())
                    .collect::<Vec<_>>(),
            );
        }
        Some("gpu-capabilities") if args.len() <= 2 => {
            let mut controller = Controller::new(true, args.get(1).map(String::as_str))?;
            return print(&serde_json::json!({
                "gpu": controller.gpu_report()?, "context": controller.context()?,
                "features": Feature::all().iter().map(|f| controller.capability(*f)).collect::<Vec<_>>()
            }));
        }
        _ => {}
    }
    let mut store = ProfileStore::user_data()?;
    match args.first().map(String::as_str) {
        Some("fan-validate-max") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            if store.pending()? {
                return Err("Pending recovery blocks the fixed max-fan trial".into());
            }
            let mut controller = Controller::new(false, None)?;
            let before = controller.fan_report()?;
            let checkpoint = store.checkpoint_completed_journal()?;
            let result = controller.validate_max_fan(&mut store);
            let after = controller.fan_report();
            let final_check = after.as_ref().map_err(|e| e.clone()).and_then(|r| {
                if r.max_fan == before.max_fan
                    && r.table_sha256 == before.table_sha256
                    && r.topology == before.topology
                {
                    Ok(())
                } else {
                    Err("Final max-fan original/layout differs".into())
                }
            });
            if result.is_ok() && final_check.is_ok() {
                let completed = store.last_transaction::<RestoreState>()?;
                store.restore_completed_trial_checkpoint(
                    checkpoint,
                    "max-fan-validation",
                    &completed.profile_digest,
                )?;
            }
            print(
                &serde_json::json!({"before":before,"transaction":result,"observations":controller.fan_validation_observations(),"after":after,"final_check":final_check,"scope":"Fixed maximum cooling ON only, then original flag restore; no software fan levels, performance, power or voltage SET"}),
            )?;
            result.map(|_| ())?;
            final_check
        }
        Some("keyboard-validate-colors" | "keyboard-validate-lighting") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            if store.pending()? {
                return Err("Pending recovery blocks the fixed keyboard color trial".into());
            }
            let mut controller = Controller::new(false, None)?;
            let before = controller.keyboard_state()?;
            let checkpoint = store.checkpoint_completed_journal()?;
            let mode = args[0] == "keyboard-validate-lighting";
            let result = if mode {
                controller.validate_keyboard_lighting(&mut store)
            } else {
                controller.validate_keyboard_colors(&mut store)
            };
            let after = controller.keyboard_state();
            let final_check = after
                .as_ref()
                .map_err(|e| format!("Final keyboard GET failed: {e}"))
                .and_then(|state| {
                    if *state == before {
                        Ok(())
                    } else {
                        Err("Keyboard state changed after the trial restore".into())
                    }
                });
            if result.is_ok() && final_check.is_ok() {
                let completed = store.last_transaction::<RestoreState>()?;
                store.restore_completed_trial_checkpoint(
                    checkpoint,
                    if mode {
                        "keyboard-lighting-validation"
                    } else {
                        "keyboard-color-validation"
                    },
                    &completed.profile_digest,
                )?;
            }
            print(
                &serde_json::json!({"before":before,"transaction":result,"after":after,"final_check":final_check,"hardware_scope":if mode {"one zone1 RGB byte and exact64/E4 static/preset switch; immediate full restore; no power/voltage writes"}else{"one zone1 RGB byte; immediate full restore; no brightness/power/voltage writes"}}),
            )?;
            result.map(|_| ())?;
            final_check
        }
        Some(command @ ("gpu-validate" | "gpu-validate-curve" | "gpu-validate-memory-curve"))
            if args.len() == 1 =>
        {
            gpu_qualification(&mut store, command)
        }
        Some("profile-create") if args.len() == 3 => {
            let mut controller = Controller::new(true, None)?;
            let profile = PerformanceProfile {
                schema: PROFILE_SCHEMA,
                id: args[1].clone(),
                name: args[2].clone(),
                binding: controller.context()?.binding,
                settings: vec![],
            };
            let path = store.save(&profile)?;
            print(&serde_json::json!({"saved":path,"hardware_applied":false,"profile":profile}))
        }
        Some("profile-list") if args.len() == 1 => print(&store.list()?),
        Some("profile-show") if args.len() == 2 => print(&store.load(&args[1])?),
        Some("profile-import") if args.len() == 2 => {
            let path = store.import(Path::new(&args[1]))?;
            print(&serde_json::json!({"saved":path,"hardware_applied":false}))
        }
        Some("profile-set") if args.len() == 4 => {
            let mut profile = store.load(&args[1])?;
            let encoded = serde_json::to_string(&args[2]).map_err(|e| e.to_string())?;
            let feature = serde_json::from_str::<Feature>(&encoded).map_err(|e| e.to_string())?;
            let setting = Setting {
                feature,
                value: parse_value(feature, &args[3])?,
            };
            setting.validate()?;
            profile.settings.retain(|s| s.feature != feature);
            profile.settings.push(setting);
            let path = store.save(&profile)?;
            print(&serde_json::json!({"saved":path,"hardware_applied":false,"profile":profile}))
        }
        Some("profile-set-vf") if args.len() == 4 => {
            let mut profile = store.load(&args[1])?;
            let mut controller = Controller::new(true, profile.binding.nvidia_uuid.as_deref())?;
            if !profile.binding.matches(&controller.context()?.binding) {
                return Err(
                    "Profile board/BIOS/GPU binding differs from the current device".into(),
                );
            }
            let voltage = args[2]
                .parse::<u32>()
                .map_err(|_| "Voltage anchor must be integer mV")?;
            let frequency = args[3]
                .parse::<u32>()
                .map_err(|_| "Frequency target must be integer MHz")?;
            let value = glph_core::vf::plateau(&controller.gpu_curve()?, voltage, frequency)?;
            profile
                .settings
                .retain(|s| !matches!(s.feature, Feature::GpuCoreOffsetMhz | Feature::GpuVfCurve));
            profile.settings.push(Setting {
                feature: Feature::GpuVfCurve,
                value,
            });
            let path = store.save(&profile)?;
            print(&serde_json::json!({"saved":path,"hardware_applied":false,"profile":profile}))
        }
        Some("profile-plan") if args.len() == 2 => {
            let profile = store.load(&args[1])?;
            let parts = glph_core::fan_profile::split(&profile)?;
            let mut controller = Controller::for_profile(&parts.direct)?;
            let mut plan =
                serde_json::to_value(transaction::plan(&parts.direct, &mut controller, false))
                    .map_err(|e| e.to_string())?;
            if let Some(shared) = parts.shared {
                plan = serde_json::json!({"direct_applicable":plan["applicable"],"direct_plan":plan,"cooling_request":shared,"cooling_request_readiness":"checked_on_apply","cooling_hardware_verified":false});
            }
            print(&plan)
        }
        Some("profile-verify-current") if args.len() == 2 => {
            let _lease = ControlLease::acquire_foreground()?;
            let profile = store.load(&args[1])?;
            // The journal must describe a completed application of this exact
            // revision. Verification only GETs current targets; it never applies
            // a profile, repairs a mismatch, or grants a startup approval.
            let parts = glph_core::fan_profile::split(&profile)?;
            if store.pending()? || store.fan_policy_pending()? {
                return Err(glph_core::message::msg("store.err.pending_checkpoint", &[]));
            }
            let Some((id, digest, requested)) = store.applied_settings()? else {
                return Err(glph_core::message::msg("profile.err.other_profile", &[]));
            };
            if id != profile.id
                || digest != profile_digest(&profile)?
                || requested != parts.direct.effective().settings
            {
                return Err(glph_core::message::msg("profile.err.other_profile", &[]));
            }
            let mut controller = Controller::for_profile(&parts.direct)?;
            controller.verify_applied(&store.last_transaction::<RestoreState>()?)?;
            crate::profile_fan::verify_request(&store, &profile)?;
            print(
                &serde_json::json!({"schema":1,"profile_id":profile.id,"profile_digest":profile_digest(&profile)?,"verified":parts.shared.is_none(),"direct_verified":true,"cooling_request_matches":parts.shared.is_some(),"cooling_hardware_verified":false,"hardware_applied":false}),
            )
        }
        Some(
            "profile-apply" | "profile-apply-trace" | "profile-ui-apply" | "profile-ui-replace",
        ) if args.len() == 2
            || (matches!(args[0].as_str(), "profile-ui-apply" | "profile-ui-replace")
                && args.len() == 3) =>
        {
            let _lease = ControlLease::acquire_foreground()?;
            let nonce = if matches!(args[0].as_str(), "profile-ui-apply" | "profile-ui-replace") {
                args.get(2).map(String::as_str)
            } else {
                None
            };
            validate_nonce(nonce)?;
            let profile = store.load(&args[1])?;
            let max_tgp_w = store.gpu_tgp_max_w()?;
            let requested_tgp_w = profile.gpu_dynamic_boost_limit_w_max(max_tgp_w)?;
            let digest = profile_digest(&profile)?;
            let parts = glph_core::fan_profile::split(&profile)?;
            if profile
                .settings
                .iter()
                .any(|s| s.feature == Feature::CpuCcdAffinity)
                && parts.shared.is_none()
                && !glph_platform::service_client::is_service_running_fast()
            {
                // The background service pins programs; install or update it
                // first so the saved selection takes effect when Session-0
                // PerformanceHubService is not running.
                crate::startup_setup::ensure_fan_service()?;
            }
            let mut controller = prepare_apply(
                &parts.direct,
                &mut store,
                false,
                args[0] == "profile-ui-replace",
            )?;
            let inventory = controller.inventory().clone();
            let armed_crash_guard = profile_has_stability_risk(&profile);
            if armed_crash_guard {
                let _ = crate::gpu_crash_guard::mark_in_flight(&profile.id);
            }
            let tgp_outcome = if requested_tgp_w.is_some() || store.vbios_tgp_state().is_some() {
                // Close any live NVML/NVAPI handles before a potential dGPU PnP restart.
                drop(controller);
                let outcome = match glph_platform::vbios_tgp::apply_for_profile(
                    &inventory,
                    &store,
                    &profile,
                    glph_platform::read_nvml_power_limits,
                ) {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        if armed_crash_guard {
                            crate::gpu_crash_guard::clear_in_flight();
                        }
                        return Err(err);
                    }
                };
                controller = match Controller::for_profile(&parts.direct) {
                    Ok(c) => c,
                    Err(err) => {
                        let rb = rollback_pre_transaction(
                            &inventory,
                            &store,
                            matches!(
                                outcome,
                                glph_platform::vbios_tgp::ApplyOutcome::AppliedNow(_)
                            ),
                            false,
                            None,
                        );
                        if armed_crash_guard {
                            crate::gpu_crash_guard::clear_in_flight();
                        }
                        return Err(if rb.is_empty() {
                            err
                        } else {
                            format!("{err}; rollback: {}", rb.join("; "))
                        });
                    }
                };
                outcome
            } else {
                glph_platform::vbios_tgp::ApplyOutcome::Unchanged
            };
            let tgp_applied_now = matches!(
                tgp_outcome,
                glph_platform::vbios_tgp::ApplyOutcome::AppliedNow(_)
            );
            let ac = glph_platform::power_status().ac_connected == Some(true);
            let nvpcf_mode =
                match glph_platform::nvpcf_ppt::apply_for_profile(&inventory, &store, &profile, ac)
                {
                    Ok(mode) => mode,
                    Err(err) => {
                        drop(controller);
                        let rb = rollback_pre_transaction(
                            &inventory,
                            &store,
                            tgp_applied_now,
                            false,
                            None,
                        );
                        if armed_crash_guard {
                            crate::gpu_crash_guard::clear_in_flight();
                        }
                        return Err(if rb.is_empty() {
                            err
                        } else {
                            format!("{err}; rollback: {}", rb.join("; "))
                        });
                    }
                };
            let nvpcf_changed = nvpcf_mode.is_some();
            if nvpcf_changed {
                controller.set_nvpcf_ppt_unlocked(true);
            }
            // Empirically verified (27 Nov 2026): when using Direct Fixed GPU TGP
            // on HP without legacy Dynamic Boost, setting PPAB=0 stops the -15 W
            // GPU subtraction under simultaneous CPU load (e.g. 140 W stays 140 W
            // even while CPU draws 45-64 W), while preserving cTGP and GPS max-temp.
            let legacy_boost = profile
                .settings
                .iter()
                .any(|s| matches!(s.feature, Feature::GpuUnleashed | Feature::GpuDynamicBoost));
            let mut ppab_prior = None;
            if ac && requested_tgp_w.is_some() && !legacy_boost && inventory.manufacturer == "HP" {
                ppab_prior = glph_platform::read_hp_gpu_status(&inventory)
                    .ok()
                    .map(|s| (s.custom_tgp_enabled, s.ppab_enabled));
                if let Err(err) = glph_platform::write_hp_gpu_power(&inventory, true, false) {
                    drop(controller);
                    let rb = rollback_pre_transaction(
                        &inventory,
                        &store,
                        tgp_applied_now,
                        nvpcf_changed,
                        ppab_prior,
                    );
                    if armed_crash_guard {
                        crate::gpu_crash_guard::clear_in_flight();
                    }
                    return Err(if rb.is_empty() {
                        err
                    } else {
                        format!("{err}; rollback: {}", rb.join("; "))
                    });
                }
            }
            let cooling_request = match crate::profile_fan::prepare(&profile, &store, &inventory) {
                Ok(req) => req,
                Err(err) => {
                    drop(controller);
                    let rb = rollback_pre_transaction(
                        &inventory,
                        &store,
                        tgp_applied_now,
                        nvpcf_changed,
                        ppab_prior,
                    );
                    if armed_crash_guard {
                        crate::gpu_crash_guard::clear_in_flight();
                    }
                    return Err(if rb.is_empty() {
                        err
                    } else {
                        format!("{err}; rollback: {}", rb.join("; "))
                    });
                }
            };
            let prelude_failed = match apply_hp_prelude(&profile, &inventory) {
                Ok(failed) => failed,
                Err(error) => {
                    drop(controller);
                    let msg =
                        crate::profile_fan::prelude_failed(&store, cooling_request.as_ref(), error);
                    let rb = rollback_pre_transaction(
                        &inventory,
                        &store,
                        tgp_applied_now,
                        nvpcf_changed,
                        ppab_prior,
                    );
                    if armed_crash_guard {
                        crate::gpu_crash_guard::clear_in_flight();
                    }
                    return Err(if rb.is_empty() {
                        msg
                    } else {
                        format!("{msg}; rollback: {}", rb.join("; "))
                    });
                }
            };
            let tracing = args[0] == "profile-apply-trace";
            if tracing {
                controller.trace_cpu_readbacks();
            }
            // Logged only: preset values overwrite the HP service's (user rule).
            let notes: Vec<String> =
                glph_platform::hp_control::check_cpu_power_settings(&profile.effective().settings)
                    .err()
                    .into_iter()
                    .collect();
            let result =
                transaction::apply(&parts.direct, &digest, &mut controller, &mut store, false);
            let (mut result, cooling) = crate::profile_fan::finish(
                &mut store,
                &mut controller,
                &profile,
                cooling_request.as_ref(),
                result,
            );
            let cpu_readbacks = if tracing {
                Some(
                    serde_json::to_value(controller.cpu_readback_observations())
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            if let Err(ref mut failure) = result {
                // Close NVML/NVAPI handles before rolling back VBIOS-TGP / NVPCF / PPAB.
                drop(controller);
                let rb = rollback_pre_transaction(
                    &inventory,
                    &store,
                    tgp_applied_now,
                    nvpcf_changed,
                    ppab_prior,
                );
                failure.rollback_errors.extend(rb);
                if armed_crash_guard {
                    crate::gpu_crash_guard::clear_in_flight();
                }
            } else if armed_crash_guard && nonce.is_none() {
                // Headless CLI applies have no long-lived UI process to clear
                // the marker after 45 s; UI applies schedule the 45 s timer in elevation.rs.
                crate::gpu_crash_guard::clear_in_flight();
            }
            // TPP has a SET contract but no exact GET. Do not include it in
            // the verified transaction or imply a fully confirmed firmware state.
            let unconfirmed: Vec<_> = profile
                .settings
                .iter()
                .filter(|s| s.feature == Feature::HpSharedPowerMw)
                .collect();
            // The HP thermal mode (0x1A) has no GET either: requested only.
            let mode = glph_core::hp_mode::HpMode::of_settings(
                &profile.settings,
                glph_platform::power_status().ac_connected == Some(true),
            )
            .map(|mode| mode.name());
            // Settings a documented precedence rule replaced (never written).
            let effective = profile.effective().settings;
            let overridden: Vec<_> = profile
                .settings
                .iter()
                .filter(|s| {
                    !matches!(
                        s.feature,
                        Feature::HpSharedPowerMw | Feature::GpuDynamicBoostLimitMw
                    ) && !effective.iter().any(|e| e.feature == s.feature)
                })
                .map(|s| s.feature)
                .collect();
            // Keep preset holds only an Apply made since the current boot.
            let boot = glph_platform::boot_unix_s();
            let gpu_tgp_w = tgp_outcome.active_watts();
            let gpu_tgp_restored =
                tgp_outcome == glph_platform::vbios_tgp::ApplyOutcome::RestoredToStock;
            let gpu_restarted = tgp_outcome.gpu_restarted();
            save_ui_result(
                &store,
                nonce,
                "apply",
                &serde_json::json!({"schema":1,"operation":"apply","nonce":nonce,"profile_id":profile.id,"profile_digest":digest,"notes":notes,"unconfirmed":unconfirmed,"mode_requested":mode,"overridden":overridden,"prelude_failed":prelude_failed,"gpu_tgp_w":gpu_tgp_w,"gpu_tgp_restored":gpu_tgp_restored,"gpu_restarted":gpu_restarted,"boot_unix_s":boot,"cooling":cooling,"result":result}),
            )?;
            if let Some(readbacks) = cpu_readbacks {
                print(&serde_json::json!({"result":result,"cpu_readbacks":readbacks}))?;
            } else {
                print(&result)?;
            }
            result
                .map(|_| ())
                .map_err(|e| serde_json::to_string_pretty(&e).unwrap_or(e.error))
        }
        Some("profile-restore" | "profile-ui-restore" | "profile-ui-restore-keep")
            if args.len() == 1
                || (args[0] == "profile-ui-restore" && (args.len() == 2 || args.len() == 3))
                || (args[0] == "profile-ui-restore-keep" && args.len() == 3) =>
        {
            let _lease = ControlLease::acquire_foreground()?;
            let nonce = if args.len() == 3 {
                args.get(2)
            } else {
                args.get(1)
            }
            .map(String::as_str);
            validate_nonce(nonce)?;
            if !store.pending()?
                && store.fan_policy_pending()?
                && !store.fan_pending_direct_applied()?
            {
                if args.len()==3 && !store.fan_policy_request()?.is_some_and(|r|matches!(&r.owner,glph_platform::profile_store::FanPolicyOwner::Profile{id,..} if id==&args[1])) {
                    return Err(glph_core::message::msg("profile.err.other_profile",&[]));
                }
                crate::profile_fan::recover_pending(&store)?;
                save_ui_result(
                    &store,
                    nonce,
                    "restore",
                    &serde_json::json!({"schema":1,"operation":"restore","nonce":nonce,"result":{"Ok":null},"cooling_preferences_restored":true,"direct_restored":false}),
                )?;
                return print(
                    &serde_json::json!({"cooling_preferences_restored":true,"hardware_handover_confirmed":false}),
                );
            }
            let saved = store.last_transaction::<RestoreState>()?;
            if args.len() == 3 && saved.profile_id != args[1] {
                return Err(glph_core::message::msg("profile.err.other_profile", &[]));
            }
            crate::gpu_crash_guard::clear_in_flight();
            // Disable further automatic application before explicit recovery.
            // The automatic battery return only undoes its own battery preset
            // and keeps the user's sign-in approval.
            if args[0] != "profile-ui-restore-keep" {
                store.disable_startup()?;
            }
            let mut controller = Controller::for_recovery(&saved)?;
            let inventory = controller.inventory().clone();
            let mut direct_restored = false;
            let mut cooling_restored = false;
            let mut nvpcf_restored = false;
            let mut gpu_tgp_restored = false;
            let keep_startup = args[0] == "profile-ui-restore-keep";
            let mut result =
                transaction::recover(&saved, &mut controller, &mut store).and_then(|()| {
                    direct_restored = true;
                    crate::profile_fan::restore_for(
                        &store,
                        &saved.profile_id,
                        &saved.profile_digest,
                    )
                    .map(|value| {
                        cooling_restored = value;
                    })
                    .map_err(|error| transaction::ApplyFailure {
                        error: glph_core::message::msg(
                            "fan_profile.err.restore_after_direct",
                            &[&error],
                        ),
                        rollback_errors: vec![],
                    })
                });
            drop(controller);
            if !keep_startup {
                // Always attempt NVPCF & VBIOS-TGP restore even if a direct
                // setting reported a partial rollback error, so overrides are
                // never stranded active.
                let mut extra_errors = Vec::new();
                match glph_platform::nvpcf_ppt::restore_if_saved(&inventory, &store) {
                    Ok(v) => nvpcf_restored = v,
                    Err(e) => extra_errors.push(e),
                }
                match glph_platform::vbios_tgp::restore_if_active(
                    &inventory,
                    &store,
                    glph_platform::read_nvml_power_limits,
                ) {
                    Ok(v) => {
                        gpu_tgp_restored = v;
                        if gpu_tgp_restored
                            && inventory.manufacturer == "HP"
                            && let Err(e) =
                                glph_platform::write_hp_gpu_power(&inventory, true, true)
                        {
                            extra_errors.push(e);
                        }
                    }
                    Err(e) => extra_errors.push(e),
                }
                if !extra_errors.is_empty() {
                    match &mut result {
                        Ok(()) => {
                            result = Err(transaction::ApplyFailure {
                                error: extra_errors.join("; "),
                                rollback_errors: vec![],
                            });
                        }
                        Err(failure) => failure.rollback_errors.extend(extra_errors),
                    }
                }
            }
            save_ui_result(
                &store,
                nonce,
                "restore",
                &serde_json::json!({"schema":1,"operation":"restore","nonce":nonce,"result":result,"direct_restored":direct_restored,"cooling_preferences_restored":cooling_restored,"nvpcf_restored":nvpcf_restored,"gpu_tgp_restored":gpu_tgp_restored,"cooling_hardware_verified":false}),
            )?;
            result.map_err(|e| serde_json::to_string_pretty(&e).unwrap_or(e.error))?;
            let gpu_after = if saved.requested.iter().any(|s| s.feature.needs_nvidia()) {
                Some(Controller::for_recovery(&saved)?.gpu_report()?)
            } else {
                None
            };
            print(
                &serde_json::json!({"restored":true,"profile_id":saved.profile_id,"gpu_tgp_restored":gpu_tgp_restored,"nvpcf_restored":nvpcf_restored,"gpu":gpu_after}),
            )
        }
        Some("profile-startup-enable" | "profile-ui-startup-enable")
            if args.len() == 2 || (args[0] == "profile-ui-startup-enable" && args.len() == 3) =>
        {
            let _lease = ControlLease::acquire_foreground()?;
            let nonce = args.get(2).map(String::as_str);
            validate_nonce(nonce)?;
            let profile = store.load(&args[1])?;
            glph_core::fan_profile::startup_allowed(&profile, true)?;
            let direct = glph_core::fan_profile::direct_part(&profile)?;
            let mut controller = Controller::for_profile(&direct)?;
            let plan = transaction::plan(&direct, &mut controller, true);
            if !plan.applicable {
                return Err(serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?);
            }
            store.startup_eligibility(&args[1])?;
            controller.verify_applied(&store.last_transaction::<RestoreState>()?)?;
            if args[0] == "profile-ui-startup-enable" {
                crate::startup_setup::ensure_current_installation()?;
            } else {
                glph_platform::verify_startup_installation()?;
            }
            glph_platform::authorize_startup_profile(&profile)?;
            store.enable_startup(&args[1])?;
            save_ui_result(
                &store,
                nonce,
                "startup",
                &serde_json::json!({"schema":1,"operation":"startup","nonce":nonce,"result":{"Ok":store.startup()?}}),
            )?;
            print(&store.startup()?)
        }
        Some("profile-startup-disable") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            store.disable_startup()?;
            print(&store.startup()?)
        }
        Some("profile-startup-apply") if args.len() == 1 => {
            let _lease = ControlLease::acquire_foreground()?;
            // Boot-Crash-Guard: if a previous GPU OC/V/F test survived across a
            // hard reboot/BSOD, pause startup profile apply automatically to
            // prevent a boot loop.
            if let Some(crashed_id) =
                crate::gpu_crash_guard::check_and_consume_boot_crash(glph_platform::boot_unix_s())
            {
                store.disable_startup()?;
                let pause_msg = glph_core::message::msg("crash_guard.paused", &[&crashed_id]);
                store.save_startup_result(&serde_json::json!({
                    "schema": 1,
                    "recorded_at_unix_seconds": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .ok(),
                    "process_id": std::process::id(),
                    "crash_guard_triggered": true,
                    "crashed_profile_id": crashed_id,
                    "result": { "Err": pause_msg }
                }))?;
                return Err(pause_msg);
            }
            let selection_at_start = store.startup()?;
            // Approval ends for a changed preset, device or binding; a firmware
            // step or value that fails after those checks does not revoke it.
            let mut device_checked = false;
            // Signed in on battery: a preset with its own battery behaviour is
            // not applied. The running dashboard applies that behaviour and
            // returns to this preset when the charger is connected.
            let mut skipped_on_battery = false;
            // The linked curve's helper request, or why it was not made.
            let mut cooling = None;
            let mut cooling_error = None;
            let result = (|| {
                let Some(profile) = store.startup_profile()? else {
                    return Ok(None);
                };
                let selection = glph_core::battery::Selection::Preset(profile.id.clone());
                let ac = glph_platform::power_status().ac_connected;
                skipped_on_battery =
                    ac == Some(false) && store.battery_selection_for(Some(&selection)).is_some();
                // Either way this preset is the one in use on AC from now on,
                // so its battery behaviour and the return apply to it. A new
                // boot also ends the battery session of the earlier one.
                let mut state = store.battery_state().at_boot(glph_platform::boot_unix_s());
                state.ac_selection = Some(selection);
                if skipped_on_battery {
                    store.save_battery_state(&state)?;
                    return Ok(None);
                }
                glph_core::fan_profile::startup_allowed(&profile, true)?;
                glph_platform::verify_startup_profile(&profile)?;
                let digest = profile_digest(&profile)?;
                let direct = glph_core::fan_profile::direct_part(&profile)?;
                let mut controller = prepare_apply(&direct, &mut store, true, false)?;
                let inventory = controller.inventory().clone();
                device_checked = true;
                let armed_crash_guard = profile_has_stability_risk(&profile);
                if armed_crash_guard {
                    let _ = crate::gpu_crash_guard::mark_in_flight(&profile.id);
                }
                let nvpcf_changed = match glph_platform::nvpcf_ppt::apply_for_profile(
                    &inventory,
                    &store,
                    &profile,
                    ac == Some(true),
                ) {
                    Ok(mode) => mode.is_some(),
                    Err(err) => {
                        if armed_crash_guard {
                            crate::gpu_crash_guard::clear_in_flight();
                        }
                        return Err(err);
                    }
                };
                if nvpcf_changed {
                    controller.set_nvpcf_ppt_unlocked(true);
                }
                // The linked curve is requested from the fan helper exactly as
                // a manual Apply does. A helper that cannot take it yet (it
                // may still be starting) does not hold back the other values.
                let request = match crate::profile_fan::prepare(&profile, &store, &inventory) {
                    Ok(request) => request,
                    Err(error) => {
                        cooling_error = Some(error);
                        None
                    }
                };
                if let Err(error) = apply_hp_prelude(&profile, &inventory) {
                    drop(controller);
                    let _ =
                        rollback_pre_transaction(&inventory, &store, false, nvpcf_changed, None);
                    if armed_crash_guard {
                        crate::gpu_crash_guard::clear_in_flight();
                    }
                    return Err(crate::profile_fan::prelude_failed(
                        &store,
                        request.as_ref(),
                        error,
                    ));
                }
                let applied =
                    transaction::apply(&direct, &digest, &mut controller, &mut store, true);
                let (applied, requested) = crate::profile_fan::finish(
                    &mut store,
                    &mut controller,
                    &profile,
                    request.as_ref(),
                    applied,
                );
                cooling = requested;
                let completion = match applied {
                    Ok(c) => c,
                    Err(mut e) => {
                        drop(controller);
                        let rb = rollback_pre_transaction(
                            &inventory,
                            &store,
                            false,
                            nvpcf_changed,
                            None,
                        );
                        e.rollback_errors.extend(rb);
                        if armed_crash_guard {
                            crate::gpu_crash_guard::clear_in_flight();
                        }
                        return Err(serde_json::to_string_pretty(&e).unwrap_or(e.error));
                    }
                };
                if ac != Some(false) {
                    // Best effort: the applied preset stands either way.
                    let _ = store.save_battery_state(&state);
                }
                if armed_crash_guard {
                    // Prove 45 s stability in this background logon task before
                    // clearing the Boot-Crash-Guard marker; unstable CPU Curve
                    // Optimizer / Fmax and GPU V/F tuning frequently crashes
                    // 20–30 s after apply.
                    std::thread::sleep(std::time::Duration::from_secs(45));
                    crate::gpu_crash_guard::clear_in_flight();
                }
                Ok(Some(completion))
            })();
            if result.is_err() && !device_checked {
                store.disable_startup()?;
            }
            store.save_startup_result(&serde_json::json!({"schema":1,"recorded_at_unix_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d|d.as_secs()).ok(),"process_id":std::process::id(),"executable":std::env::current_exe().map_err(|e| e.to_string())?,"profile_directory":store.root(),"selection_at_start":selection_at_start,"skipped_on_battery":skipped_on_battery,"cooling":cooling,"cooling_error":cooling_error,"result":result}))?;
            print(&result)?;
            result.map(|_| ())
        }
        Some("profile-startup-status") if args.len() == 1 => print(
            &serde_json::json!({"selection":store.startup()?,"installation":glph_platform::verify_startup_installation()}),
        ),
        Some("profile-startup-check") if args.len() == 1 => {
            // Inspection only: never apply, enable, authorize or disable startup.
            let selection = store.startup()?;
            let result = (|| -> Result<Option<serde_json::Value>, String> {
                let Some(profile) = store.startup_profile()? else {
                    return Ok(None);
                };
                store.startup_eligibility(&profile.id)?;
                glph_platform::verify_startup_profile(&profile)?;
                let direct = glph_core::fan_profile::direct_part(&profile)?;
                let mut controller = Controller::for_profile(&direct)?;
                let plan = transaction::plan(&direct, &mut controller, true);
                if !plan.applicable {
                    return Err(serde_json::to_string(&plan).map_err(|e| e.to_string())?);
                }
                Ok(Some(
                    serde_json::json!({"profile_id":profile.id,"profile_digest":profile_digest(&profile)?}),
                ))
            })();
            print(
                &serde_json::json!({"schema":1,"selection":selection,"ready":matches!(&result,Ok(Some(_))),"verification":result,"hardware_applied":false}),
            )
        }
        _ => Err("Invalid profile command or argument count; use --help".into()),
    }
}

fn validate_nonce(nonce: Option<&str>) -> Result<(), String> {
    if nonce.is_some_and(|n| {
        n.is_empty() || n.len() > 64 || !n.bytes().all(|b| b.is_ascii_alphanumeric())
    }) {
        return Err("Invalid UI operation token".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_cli_values_cannot_reintroduce_retired_settings() {
        for name in [
            "intel_pl1_mw",
            "intel_cache_voltage_offset_mv",
            "intel_core_ratios",
        ] {
            let feature = serde_json::from_value::<Feature>(serde_json::json!(name)).unwrap();
            assert!(parse_value(feature, "45000").is_err());
        }
        assert_eq!(
            parse_value(Feature::CpuPptLimitMw, "45000").unwrap(),
            Value::Integer(45000)
        );
    }

    #[test]
    fn stability_risk_guard_covers_cpu_and_gpu_tuning_but_ignores_benign_profiles() {
        let profile_with = |settings: Vec<Setting>| PerformanceProfile {
            schema: PROFILE_SCHEMA,
            id: "test".into(),
            name: "Test".into(),
            binding: glph_core::profiles::DeviceBinding {
                manufacturer: "HP".into(),
                board: "8E35".into(),
                bios: "F.10".into(),
                cpu_names: vec!["AMD Ryzen 9 9955HX".into()],
                nvidia_uuid: None,
                nvidia_driver: None,
                nvidia_vbios: None,
            },
            settings,
        };
        assert!(!profile_has_stability_risk(&profile_with(vec![
            Setting {
                feature: Feature::CpuPptLimitMw,
                value: Value::Integer(55_000),
            },
            Setting {
                feature: Feature::DisplayRefreshRateHz,
                value: Value::Integer(240),
            },
        ])));
        for setting in [
            Setting {
                feature: Feature::CpuCurveOptimizerAllCore,
                value: Value::Integer(-15),
            },
            Setting {
                feature: Feature::CpuBoostLimitMhz,
                value: Value::Integer(5550),
            },
            Setting {
                feature: Feature::CpuPboScalar,
                value: Value::Integer(2),
            },
            Setting {
                feature: Feature::GpuCoreOffsetMhz,
                value: Value::Integer(150),
            },
            Setting {
                feature: Feature::GpuDynamicBoostLimitMw,
                value: Value::Integer(125_000),
            },
        ] {
            assert!(
                profile_has_stability_risk(&profile_with(vec![setting])),
                "stability guard should arm for CPU/GPU voltage, clock, or curve tuning"
            );
        }
    }
}
