//! "Keep preset" in the elevated dashboard process. Every few seconds the
//! applied preset's CPU limits and HP GPU flags are compared with the live
//! values; drifted ones are written again and logged to `preset-hold.log`.
//! The HP mode of a Dynamic Boost preset has no GET; its loss is seen as a
//! locked GPU limit under load. Policy and back-off live in `glph_core::hold`.
use glph_core::{
    features::Feature,
    hold::{self, Action, Hold},
    hp_mode::{self, HpMode},
    profiles::{Setting, Value},
};
use glph_platform::{ControlLease, Controller, profile_store::ProfileStore};
use std::{
    sync::{Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Consecutive locked readings before the mode counts as lost, so one
/// transient sample during a load change does not rewrite it.
const LOCKED_CHECKS: u8 = 2;
/// Curve Optimizer verification reads 16 SMU core margins sequentially. Only
/// run it after a trigger/nudge/new revision or at most every 120 seconds.
const CO_CHECK_INTERVAL: Duration = Duration::from_secs(120);
/// Maximum age for reusing the persistent GPU telemetry worker's cached limit
/// reading inside `keep_mode()` before falling back to a one-shot child probe.
const CACHED_GPU_LIMIT_MAX_AGE: Duration = Duration::from_secs(10);
static NOTICE: Mutex<Option<String>> = Mutex::new(None);
pub fn take_notice() -> Option<String> {
    NOTICE.lock().ok()?.take()
}

/// Debounced trigger from Windows power events and finished Apply/mode work.
static WAKE: Mutex<Option<Instant>> = Mutex::new(None);
static WAKE_SIGNAL: Condvar = Condvar::new();
/// Requests one prompt check after the last of several close events.
pub fn nudge() {
    let mut due = WAKE.lock().unwrap_or_else(|e| e.into_inner());
    *due = Some(Instant::now() + hold::EVENT_DEBOUNCE);
    WAKE_SIGNAL.notify_one();
}
/// Sleeps until `next` or a debounced nudge; true when a nudge fired.
fn wait_until(next: Instant) -> bool {
    let mut due = WAKE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = Instant::now();
        if due.is_some_and(|at| at <= now) {
            *due = None;
            return true;
        }
        if now >= next {
            return false;
        }
        let until = due.map_or(next, |at| at.min(next));
        due = WAKE_SIGNAL
            .wait_timeout(due, until - now)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// What one check means for the pacing.
#[derive(Default)]
struct Outcome {
    /// New revision or a rewrite: keep checking closely for a while.
    trigger: bool,
    /// Gave up: the back-off runs at the slow interval.
    calm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    id: String,
    digest: String,
    nonce: Option<String>,
    settings: Vec<Setting>,
}
impl Applied {
    /// Hold only settings the actual Apply verified. A completed journal also
    /// contains skipped requests for Recovery; those are not active targets.
    fn from_result(result: &serde_json::Value) -> Option<Self> {
        if result["operation"] != "apply" || !result["result"]["Ok"].is_object() {
            return None;
        }
        // Volatile firmware values return to stock at a restart; an Apply
        // from an earlier boot is not re-applied piecemeal without sign-in
        // approval. Records without a boot time predate this rule.
        let boot = result["boot_unix_s"].as_u64()?;
        if boot.abs_diff(glph_platform::boot_unix_s()) > 120 {
            return None;
        }
        let applied = Self {
            id: result["profile_id"].as_str()?.to_owned(),
            digest: result["profile_digest"].as_str()?.to_owned(),
            nonce: result["nonce"].as_str().map(str::to_owned),
            settings: serde_json::from_value(result["result"]["Ok"]["verified"].clone()).ok()?,
        };
        Some(applied)
    }
    fn matches_journal(&self, id: &str, digest: &str, requested: &[Setting]) -> bool {
        self.id == id
            && self.digest == digest
            && self.settings.iter().all(|s| requested.contains(s))
    }
    fn current(&self, store: &ProfileStore) -> Result<bool, String> {
        if !store.app_settings()?.hold_preset || store.pending()? || store.fan_policy_pending()? {
            return Ok(false);
        }
        let Some((id, digest, requested)) = store.applied_settings()? else {
            return Ok(false);
        };
        if !self.matches_journal(&id, &digest, &requested) {
            return Ok(false);
        }
        let Some(result) = store.last_apply_result()? else {
            return Ok(false);
        };
        Ok(match result["operation"].as_str() {
            Some("apply") => Self::from_result(&result).as_ref() == Some(self),
            Some("mode" | "restore") => false,
            _ => true,
        })
    }
}

/// Watches the GPU release of a Dynamic Boost preset. No NVML session lives
/// in this process: the driver library crashed the dashboard with a retained
/// session (22 November 2026, `nvml.dll` access violation), so every reading
/// comes from a disposable child like the dashboard's GPU telemetry.
#[derive(Default)]
struct Release {
    locked: u8,
}

/// Runs for the lifetime of the dashboard process; never blocks the UI.
pub fn spawn() {
    thread::spawn(|| {
        crate::runtime_priority::configure_background_thread();
        let mut hold = Hold::default();
        let mut controller = None;
        let mut active = None;
        let mut release = Release::default();
        let mut last_co_check: Option<Instant> = None;
        let origin = Instant::now();
        let mut pace = hold::Pace::default();
        pace.trigger(origin.elapsed());
        let mut next = origin + hold::FAST_INTERVAL;
        loop {
            let nudged = wait_until(next);
            if nudged {
                pace.trigger(origin.elapsed());
            }
            match tick(
                &mut hold,
                &mut controller,
                &mut active,
                &mut release,
                &mut last_co_check,
                nudged,
            ) {
                Ok(outcome) => {
                    if outcome.trigger {
                        pace.trigger(origin.elapsed());
                    }
                    if outcome.calm {
                        pace.calm();
                    }
                }
                // Driver or helper state may be stale; reopen on the next tick.
                Err(_) => controller = None,
            }
            let mut interval = pace.interval(origin.elapsed());
            if interval == hold::SLOW_INTERVAL
                && glph_platform::power_status().ac_connected == Some(false)
            {
                interval = hold::BATTERY_INTERVAL;
            }
            next = Instant::now() + interval;
        }
    });
}

fn is_hp_gpu_feature(feature: Feature) -> bool {
    matches!(
        feature,
        Feature::GpuCustomTgp
            | Feature::GpuDynamicBoost
            | Feature::GpuUnleashed
            | Feature::GpuTemperatureLimitC
    )
}

fn is_co_feature(feature: Feature) -> bool {
    matches!(
        feature,
        Feature::CpuCurveOptimizerAllCore | Feature::CpuCurveOptimizerPerCore
    )
}

fn hp_gpu_drift(status: &glph_core::hp::GpuPowerStatus, setting: &Setting) -> Option<Value> {
    let live = match setting.feature {
        Feature::GpuCustomTgp => Value::Boolean(status.custom_tgp_enabled),
        Feature::GpuDynamicBoost | Feature::GpuUnleashed => Value::Boolean(status.ppab_enabled),
        Feature::GpuTemperatureLimitC => Value::Integer(i64::from(status.temperature_threshold_c?)),
        _ => return None,
    };
    (live != setting.value).then_some(live)
}

/// `active` is the digest of the preset the user applied last. A later mode
/// or Restore ends holding; other controls (lighting, fans, MUX) do not.
fn tick(
    hold: &mut Hold,
    controller: &mut Option<Controller>,
    active: &mut Option<Applied>,
    release: &mut Release,
    last_co_check: &mut Option<Instant>,
    nudged: bool,
) -> Result<Outcome, String> {
    let store = ProfileStore::user_data()?;
    let stop = |hold: &mut Hold, release: &mut Release, last_co: &mut Option<Instant>| {
        hold.clear();
        *release = Release::default();
        *last_co = None;
    };
    if !store.app_settings().is_ok_and(|s| s.hold_preset) {
        stop(hold, release, last_co_check);
        return Ok(Outcome::default());
    }
    if let Ok(Some(result)) = store.last_apply_result() {
        match result["operation"].as_str() {
            Some("apply") => *active = Applied::from_result(&result),
            Some("mode" | "restore") => *active = None,
            _ => {}
        }
    }
    let Some(applied) = active.as_ref() else {
        stop(hold, release, last_co_check);
        return Ok(Outcome::default());
    };
    // Never during a transaction or a pending recovery.
    if store.pending()? || store.fan_policy_pending()? {
        return Ok(Outcome::default());
    }
    let Some((id, applied_digest, settings)) = store.applied_settings()? else {
        stop(hold, release, last_co_check);
        return Ok(Outcome::default());
    };
    if !applied.matches_journal(&id, &applied_digest, &settings) {
        stop(hold, release, last_co_check);
        return Ok(Outcome::default());
    }
    // Explicit Apply of the same preset is a new attempt, too.
    let new_revision = hold.track(&format!(
        "{}:{}",
        applied.digest,
        applied.nonce.as_deref().unwrap_or_default()
    ));
    let mut outcome = Outcome {
        trigger: new_revision,
        calm: false,
    };
    // An unavailable NVML must not stop keeping the CPU limits and flags.
    match keep_mode(hold, release, &store, applied) {
        Ok(mode_outcome) => {
            outcome.trigger |= mode_outcome.trigger;
            outcome.calm |= mode_outcome.calm;
        }
        Err(_) => *release = Release::default(),
    }
    let targets = hold::targets(&applied.settings);
    if targets.is_empty() {
        return Ok(outcome);
    }
    let controller = match controller {
        Some(controller) => controller,
        None => controller.insert(Controller::new(false, None)?),
    };
    let check_co_now =
        nudged || new_revision || last_co_check.is_none_or(|at| at.elapsed() >= CO_CHECK_INTERVAL);
    let mut co_checked = false;

    // Batch all HP GPU status checks into ONE `0x21` read per tick using the
    // thread-local cached `HpBios` session instead of 3 separate WMI sessions.
    let mut cached_hp_gpu: Option<glph_core::hp::GpuPowerStatus> = None;

    for target in targets {
        if is_co_feature(target.feature) {
            if !check_co_now {
                continue;
            }
            co_checked = true;
        }
        let Ok(_lease) = ControlLease::acquire_background() else {
            break;
        };
        let drift = if is_hp_gpu_feature(target.feature) {
            if cached_hp_gpu.is_none() {
                cached_hp_gpu = Some(glph_platform::read_hp_gpu_status(controller.inventory())?);
            }
            hp_gpu_drift(cached_hp_gpu.as_ref().expect("populated above"), &target)
        } else {
            controller.hold_check(&target)?
        };
        match hold.decide(target.feature, drift.is_some()) {
            Action::Rewrite => {
                // Re-verify revision under the held lease right before writing
                // so a concurrent Apply/Restore is never overwritten.
                if !applied.current(&store)? {
                    break;
                }
                outcome.trigger = true;
                let result = controller.reassert(&target);
                if is_hp_gpu_feature(target.feature) {
                    cached_hp_gpu = None;
                }
                log(&store, &id, &target, drift.as_ref(), &result);
            }
            Action::GiveUp => {
                outcome.calm = true;
                if let Ok(mut notice) = NOTICE.lock() {
                    *notice = Some(crate::i18n::tf(
                        "hold.conflict",
                        &[&crate::i18n::t(&format!("field.{:?}", target.feature))],
                    ));
                }
                log(
                    &store,
                    &id,
                    &target,
                    drift.as_ref(),
                    &Err("given up: overwritten again after every rewrite".into()),
                );
            }
            Action::Keep | Action::Ignore => {}
        }
    }
    if co_checked {
        *last_co_check = Some(Instant::now());
    }
    Ok(outcome)
}

/// A Dynamic Boost preset switched to Unleashed (Performance on battery). A
/// locked enforced limit under load on consecutive checks means the firmware
/// dropped that mode: the mode payload and the preset's TPP are written
/// again; the GPU flags are kept with the other targets.
fn keep_mode(
    hold: &mut Hold,
    release: &mut Release,
    store: &ProfileStore,
    applied: &Applied,
) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let id = &applied.id;
    let settings = &applied.settings;
    let ac = glph_platform::power_status().ac_connected == Some(true);
    // On battery an NVML probe would wake a sleeping dGPU from D3cold; the
    // mode watch pauses until the charger returns.
    if !ac {
        *release = Release::default();
        return Ok(outcome);
    }
    // Never probe NVML during a PnP restart / foreground GPU transaction.
    if ControlLease::gpu_pause_active() {
        return Ok(outcome);
    }
    // The same holds on AC in hybrid mode: a powered-down dGPU has no limit
    // to watch, and the query would wake it.
    if glph_platform::nvidia_gpu_awake() == Some(false) {
        *release = Release::default();
        return Ok(outcome);
    }
    let mode = match HpMode::of_settings(settings, ac) {
        Some(mode @ (HpMode::Unleashed | HpMode::Performance)) => mode,
        _ => {
            *release = Release::default();
            return Ok(outcome);
        }
    };
    let (limit_mw, draw_mw): (u32, u32) =
        if let Some(cached) = crate::telemetry_worker::cached_gpu_limit(CACHED_GPU_LIMIT_MAX_AGE) {
            cached
        } else {
            crate::read_process::capture(crate::read_process::ReadCommand::GpuLimit)?
        };
    if hp_mode::gpu_release_lost(limit_mw, draw_mw) {
        release.locked = release.locked.saturating_add(1);
    } else {
        release.locked = 0;
    }
    let entry = |result: &str| {
        let _ = store.append_hold_log(&format!(
            "{} {id} HpMode limit_mw={limit_mw} draw_mw={draw_mw} mode={} {result}",
            unix_now(),
            mode.name()
        ));
    };
    match hold.decide(
        Feature::OmenPerformanceMode,
        release.locked >= LOCKED_CHECKS,
    ) {
        Action::Rewrite => {
            let Ok(_lease) = ControlLease::acquire_background() else {
                return Ok(outcome);
            };
            if !applied.current(store)? {
                return Ok(outcome);
            }
            outcome.trigger = true;
            release.locked = 0;
            // The transaction leaves TPP out (no readback); the stored preset
            // has it. Without one, the mode's own budget applies.
            let tpp = store
                .load(id)
                .ok()
                .filter(|p| {
                    glph_platform::profile_store::profile_digest(p)
                        .ok()
                        .as_deref()
                        == Some(applied.digest.as_str())
                })
                .and_then(|p| HpMode::shared_budget_of_profile(&p).ok().flatten())
                .unwrap_or(match mode {
                    HpMode::Unleashed => hp_mode::SHARED_BUDGET_MAX_W,
                    _ => 0,
                });
            let inventory = glph_platform::inventory();
            let result = glph_platform::write_hp_thermal_mode(&inventory, mode).and_then(|()| {
                if tpp == 0 {
                    Ok(())
                } else {
                    glph_platform::write_hp_shared_budget(&inventory, tpp)
                }
            });
            match result {
                Ok(()) => entry(&format!("rewritten tpp_w={tpp}")),
                Err(error) => entry(&format!("failed: {error}")),
            }
        }
        Action::GiveUp => {
            outcome.calm = true;
            entry("given up: mode lost again after every rewrite");
        }
        Action::Keep | Action::Ignore => {}
    }
    Ok(outcome)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn log(
    store: &ProfileStore,
    id: &str,
    target: &Setting,
    live: Option<&Value>,
    result: &Result<(), String>,
) {
    let shown = |value: Option<&Value>| {
        value.map_or_else(
            || "?".to_owned(),
            |v| serde_json::to_string(v).unwrap_or_default(),
        )
    };
    let _ = store.append_hold_log(&format!(
        "{} {id} {:?} live={} target={} {}",
        unix_now(),
        target.feature,
        shown(live),
        shown(Some(&target.value)),
        match result {
            Ok(()) => "rewritten".to_owned(),
            Err(error) => format!("failed: {error}"),
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use glph_core::{
        profiles::DeviceBinding,
        transaction::{Captured, Completion, Journal, PendingTransaction},
    };

    fn result(nonce: &str, settings: &[Setting]) -> serde_json::Value {
        serde_json::json!({"operation":"apply","profile_id":"preset-test","profile_digest":"revision","nonce":nonce,"boot_unix_s":glph_platform::boot_unix_s(),"result":{"Ok":{"verified":settings}}})
    }
    #[test]
    fn an_apply_from_an_earlier_boot_is_not_held() {
        let mut record = result("a", &[]);
        assert!(Applied::from_result(&record).is_some());
        record["boot_unix_s"] = serde_json::json!(glph_platform::boot_unix_s() - 3600);
        assert!(Applied::from_result(&record).is_none());
        record.as_object_mut().unwrap().remove("boot_unix_s");
        assert!(Applied::from_result(&record).is_none());
    }
    #[test]
    fn skipped_requests_never_become_hold_targets() {
        let verified = Setting {
            feature: Feature::CpuTemperatureLimitC,
            value: Value::Integer(85),
        };
        let skipped = Setting {
            feature: Feature::CpuPptLimitMw,
            value: Value::Integer(85_000),
        };
        let active = Applied::from_result(&result("a", std::slice::from_ref(&verified))).unwrap();
        assert!(active.matches_journal("preset-test", "revision", &[verified.clone(), skipped]));
        assert_eq!(hold::targets(&active.settings), [verified]);
        assert!(!active.matches_journal("preset-test", "other-revision", &active.settings));
        assert!(
            Applied::from_result(
                &serde_json::json!({"operation":"apply","result":{"Err":{"error":"failed"}}})
            )
            .is_none()
        );
    }
    #[test]
    fn old_revision_cannot_reassert_after_reapply_recovery_or_turning_hold_off() {
        let path = std::env::temp_dir().join(format!(
            "omen-hold-test-{}-{}",
            std::process::id(),
            crate::native_ui::token()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let mut store = ProfileStore::at(path).unwrap();
        let mut options = store.app_settings().unwrap();
        options.hold_preset = true;
        store.save_app_settings(&options).unwrap();
        let setting = Setting {
            feature: Feature::CpuTemperatureLimitC,
            value: Value::Integer(85),
        };
        let saved = PendingTransaction {
            schema: 1,
            profile_id: "preset-test".into(),
            profile_digest: "revision".into(),
            binding: DeviceBinding {
                manufacturer: "Test".into(),
                board: "Test".into(),
                bios: "Test".into(),
                cpu_names: vec![],
                nvidia_uuid: None,
                nvidia_driver: None,
                nvidia_vbios: None,
            },
            original: vec![Captured {
                feature: setting.feature,
                value: Value::Integer(95),
                restore: Value::Integer(95),
            }],
            requested: vec![setting.clone()],
        };
        store.begin(&saved).unwrap();
        <ProfileStore as Journal<Value>>::finish(&mut store, Completion::Applied).unwrap();
        store
            .save_control_result(&result("first", std::slice::from_ref(&setting)))
            .unwrap();
        let old = Applied::from_result(&store.control_result().unwrap()).unwrap();
        assert!(old.current(&store).unwrap());
        let pending_fan = store
            .prepare_fan_policy(
                glph_platform::profile_store::FanPolicyOwner::Library {
                    id: "fan-test".into(),
                },
                "8E35",
                "F.13",
                &[
                    glph_core::profiles::FanPoint {
                        temperature_c: 30,
                        duty_percent: 30,
                    },
                    glph_core::profiles::FanPoint {
                        temperature_c: 90,
                        duty_percent: 100,
                    },
                ],
            )
            .unwrap();
        assert!(
            !old.current(&store).unwrap(),
            "Hold pauses during a fan recovery duty"
        );
        store.restore_fan_policy(&pending_fan).unwrap();
        assert!(old.current(&store).unwrap());
        // Even reapplying the same digest invalidates the old in-flight work.
        store
            .save_control_result(&result("second", std::slice::from_ref(&setting)))
            .unwrap();
        assert!(!old.current(&store).unwrap());
        let current = Applied::from_result(&store.control_result().unwrap()).unwrap();
        options.hold_preset = false;
        store.save_app_settings(&options).unwrap();
        assert!(!current.current(&store).unwrap());
        options.hold_preset = true;
        store.save_app_settings(&options).unwrap();
        store.begin(&saved).unwrap();
        assert!(!current.current(&store).unwrap());
    }
}
