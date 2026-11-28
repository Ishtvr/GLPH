//! Shared fixed-command UAC child bridge. No caller-defined executable/command.
use crate::{
    i18n::{t, tf},
    native_ui::{token, wide},
};
use glph_platform::profile_store::ProfileStore;
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::*,
    System::Threading::*,
    UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Install,
    Start,
    Stop,
    Remove,
}

impl ServiceAction {
    pub fn arg(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Remove => "remove",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Apply,
    Replace,
    Restore,
    /// Battery return: restore the battery preset's journal without
    /// revoking the sign-in approval (a normal Restore disables it).
    RestoreKeepStartup,
    Startup,
    Mode(glph_core::hp_mode::HpMode),
    ReplaceMode(glph_core::hp_mode::HpMode),
    UnleashedMode(glph_core::hp_mode::HpUnleashedOptions),
    FanService,
    HardwareService(ServiceAction),
    Graphics(glph_core::quick_switch::GraphicsMode),
    /// `true` stops and disables HPOmenCap, `false` restores it.
    HpService(bool),
    /// Discovers this device's hardware/mode defaults (incl. Standard & Performance TDC/EDC).
    DetectDefaults,
    /// Reads the four-zone keyboard lighting state via elevated HP BIOS WMI.
    KeyboardRead,
    /// Applies four-zone keyboard RGB colors and on/off state via elevated HP BIOS WMI.
    KeyboardDirectApply {
        colors: [[u8; 3]; 4],
        enabled: bool,
    },
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Apply | Self::Replace => "apply",
            Self::Restore | Self::RestoreKeepStartup => "restore",
            Self::Startup => "startup",
            Self::Mode(_) | Self::ReplaceMode(_) | Self::UnleashedMode(_) => "mode",
            Self::FanService => "fan",
            Self::HardwareService(_) => "hardware_service",
            Self::Graphics(_) => "mux",
            Self::HpService(_) => "hp_service",
            Self::DetectDefaults => "device_defaults",
            Self::KeyboardRead => "keyboard_read",
            Self::KeyboardDirectApply { .. } => "keyboard_apply",
        }
    }
}
pub fn run(
    id: Option<&str>,
    action: Action,
    stopped: impl FnMut() -> bool,
) -> Result<serde_json::Value, String> {
    run_inner(id, action, stopped, false)
}
/// Serialized replacement waits for the older command's real process exit.
/// A timeout never authorizes a second hardware writer beside it.
pub fn run_serial(id: Option<&str>, action: Action) -> Result<serde_json::Value, String> {
    run_inner(id, action, || false, true)
}
fn run_inner(
    id: Option<&str>,
    action: Action,
    mut stopped: impl FnMut() -> bool,
    wait_finished: bool,
) -> Result<serde_json::Value, String> {
    if let Some(id) = id {
        glph_core::profiles::validate_id(id)?;
    }
    let nonce = token();
    let arguments = match action {
        Action::Restore => id.map_or_else(
            || format!("profile-ui-restore {nonce}"),
            |id| format!("profile-ui-restore {id} {nonce}"),
        ),
        Action::Apply => format!(
            "profile-ui-apply {} {nonce}",
            id.ok_or(t("elevation.save_first"))?
        ),
        Action::RestoreKeepStartup => format!(
            "profile-ui-restore-keep {} {nonce}",
            id.ok_or(t("elevation.save_first"))?
        ),
        Action::Startup => format!(
            "profile-ui-startup-enable {} {nonce}",
            id.ok_or(t("elevation.save_first"))?
        ),
        Action::Mode(mode) => format!("mode-ui-apply {} {nonce}", mode.name()),
        Action::ReplaceMode(mode) => format!("mode-ui-replace {} {nonce}", mode.name()),
        Action::UnleashedMode(opts) => format!(
            "mode-ui-unleashed {} {} {} {} {} {nonce}",
            opts.cpu_pl1_w,
            opts.shared_budget_w,
            u8::from(opts.custom_tgp),
            u8::from(opts.dynamic_boost),
            opts.gpu_max_temp_c
        ),
        Action::Replace => format!(
            "profile-ui-replace {} {nonce}",
            id.ok_or(t("elevation.save_first"))?
        ),
        Action::FanService => format!("fan-service-install {nonce}"),
        Action::HardwareService(svc) => format!("service-ui {} {nonce}", svc.arg()),
        Action::Graphics(mode) => format!("mux-ui-apply {} {nonce}", mode.key()),
        Action::HpService(disable) => format!(
            "hp-service-ui {} {nonce}",
            if disable { "disable" } else { "restore" }
        ),
        Action::DetectDefaults => format!("device-defaults-ui-detect {nonce}"),
        Action::KeyboardRead => format!("keyboard-ui-read {nonce}"),
        Action::KeyboardDirectApply { colors, enabled } => format!(
            "keyboard-ui-direct-apply {:02x}{:02x}{:02x} {:02x}{:02x}{:02x} {:02x}{:02x}{:02x} {:02x}{:02x}{:02x} {} {nonce}",
            colors[0][0],
            colors[0][1],
            colors[0][2],
            colors[1][0],
            colors[1][1],
            colors[1][2],
            colors[2][0],
            colors[2][1],
            colors[2][2],
            colors[3][0],
            colors[3][1],
            colors[3][2],
            u8::from(enabled),
        ),
    };
    let arguments = format!("--lang {} {arguments}", crate::i18n::language_code());
    let file = wide(
        &std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy(),
    );
    let parameters = wide(&arguments);
    let verb = wide("runas");
    let mut execute: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    execute.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    execute.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    execute.lpVerb = verb.as_ptr();
    execute.lpFile = file.as_ptr();
    execute.lpParameters = parameters.as_ptr();
    execute.nShow = SW_HIDE;
    // All strings, structure and process handle have explicit bounded lifetimes.
    if unsafe { ShellExecuteExW(&mut execute) } == 0 {
        return Err(tf(
            "elevation.declined",
            &[&std::io::Error::last_os_error()],
        ));
    }
    if execute.hProcess.is_null() {
        return Err(t("elevation.no_process").into());
    }
    let result = (|| {
        // Apply/Restore/Mode can include a VBIOS-TGP dGPU PnP restart and
        // Code-43 recovery (up to ~100 s); give them a 120 s budget and cap
        // serialized waits with a 180 s watchdog so a hung child never deadlocks.
        let wait_secs = if wait_finished {
            180
        } else if matches!(
            action,
            Action::Apply
                | Action::Replace
                | Action::Restore
                | Action::RestoreKeepStartup
                | Action::Mode(_)
                | Action::ReplaceMode(_)
        ) {
            120
        } else {
            45
        };
        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        loop {
            match unsafe { WaitForSingleObject(execute.hProcess, 50) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT => {}
                _ => return Err(std::io::Error::last_os_error().to_string()),
            }
            if stopped() {
                return Err(t("elevation.window_closed").into());
            }
            if Instant::now() >= deadline {
                return Err(t("elevation.still_running").into());
            }
        }
        let mut code = 1;
        if unsafe { GetExitCodeProcess(execute.hProcess, &mut code) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let store = ProfileStore::user_data()?;
        // Prefer the per-nonce result file so concurrent UI operations
        // never clobber each other; fall back to `last-control-result.json`.
        let record = match store.take_nonce_control_result(&nonce)? {
            Some(record) => record,
            None => store.control_result()?,
        };
        if record["schema"] != 1
            || record["nonce"].as_str() != Some(&nonce)
            || record["operation"].as_str() != Some(action.name())
        {
            return Err(t("elevation.no_record").into());
        }
        if code != 0 || record["result"].get("Err").is_some() {
            // The user sees one short translated line; the complete error and
            // rollback details stay in the recorded control result (journal).
            // The marker is translated once, where the caller shows it.
            return Err(match record["result"]["Err"]["error"].as_str() {
                Some(text) if text.contains(glph_core::message::OPEN) => text.to_owned(),
                _ => glph_core::message::msg("elevation.failed_logged", &[]),
            });
        }
        if record["result"].get("Ok").is_none() {
            return Err(t("elevation.unverified").into());
        }
        if matches!(action, Action::Apply | Action::Replace) {
            // Clear the Boot-Crash-Guard marker after 45 s of stable execution
            // inside the long-lived UI process; unstable CPU Curve Optimizer /
            // Fmax and GPU V/F crashes often manifest 20–30 s after apply.
            crate::gpu_crash_guard::schedule_clear_after(Duration::from_secs(45));
        }
        Ok(record)
    })();
    // Never terminate a transaction in progress, including a WMI provider wait.
    unsafe {
        CloseHandle(execute.hProcess);
    }
    result
}

/// Status of a successful preset Apply, one statement per line: values that
/// were left at their original are named, translated; raw technical reasons
/// stay in the journal.
pub fn applied_status(record: &serde_json::Value) -> String {
    let skipped = skipped_names(record);
    let fan_requested = record["cooling"]["state"] == "requested";
    // Confirmed, partially applied, then what was only requested or replaced.
    let mut status = if fan_requested && skipped.is_empty() {
        t("fan_profile.controls_and_request").into()
    } else if skipped.is_empty() {
        t("preset.applied").into()
    } else {
        tf("preset.applied_except", &[&skipped.join("; ")])
    };
    let mut add = |text: String| {
        status.push('\n');
        status.push_str(&text);
    };
    if fan_requested && !skipped.is_empty() {
        add(t("fan_profile.requested").into());
    }
    let prelude: Vec<String> = record["prelude_failed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(crate::i18n::error_message)
        .collect();
    if !prelude.is_empty() {
        add(tf("preset.prelude_failed", &[&prelude.join("; ")]));
    }
    if let Some(tgp_w) = record["gpu_tgp_w"].as_u64() {
        add(tf("preset.gpu_tgp_applied", &[&tgp_w]));
    } else if record["gpu_tgp_restored"] == true {
        add(t("preset.gpu_tgp_restored").into());
    }
    if let Some(mode) = record["mode_requested"].as_str() {
        add(tf("preset.mode_requested", &[&t(&format!("mode.{mode}"))]));
    }
    if record["unconfirmed"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        add(t("preset.applied_unconfirmed").into());
    }
    let overridden: Vec<String> = record["overridden"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .filter_map(|key| serde_json::from_value(key.into()).ok())
        .filter(|feature: &glph_core::features::Feature| {
            !matches!(
                feature,
                glph_core::features::Feature::HpSharedPowerMw
                    | glph_core::features::Feature::GpuDynamicBoostLimitMw
            )
        })
        .map(|feature: glph_core::features::Feature| t(&format!("field.{feature:?}")).to_owned())
        .collect();
    if !overridden.is_empty() {
        add(tf("preset.overridden", &[&overridden.join(", ")]));
    }
    status
}
/// Status after Save or sign-in approval that first applied the preset: the
/// values left at their original stay named instead of a plain success.
pub fn startup_status(applied: &serde_json::Value, done: &str) -> String {
    if skipped_names(applied).is_empty() {
        done.to_owned()
    } else {
        format!("{}\n{done}", applied_status(applied))
    }
}
fn skipped_names(record: &serde_json::Value) -> Vec<String> {
    record["result"]["Ok"]["skipped"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let reason = item["reason"].as_str().unwrap_or_default();
            if reason.contains(glph_core::message::OPEN) {
                crate::i18n::message(reason)
            } else {
                item["feature"]
                    .as_str()
                    .and_then(|key| serde_json::from_value(key.into()).ok())
                    .map_or_else(
                        || reason.to_owned(),
                        |feature: glph_core::features::Feature| {
                            t(&format!("field.{feature:?}")).to_owned()
                        },
                    )
            }
        })
        .collect()
}
pub fn restored_status(record: &serde_json::Value) -> String {
    let mut text = if record["cooling_preferences_restored"] == true {
        let mut text = if record["direct_restored"] == true {
            t("fan_profile.direct_restored").to_owned() + " "
        } else {
            String::new()
        };
        text.push_str(t("fan_profile.policy_restored"));
        text
    } else {
        t("preset.restored").to_owned()
    };
    if record["gpu_tgp_restored"] == true {
        text.push('\n');
        text.push_str(t("preset.gpu_tgp_restored"));
    }
    text
}

/// A mode SET is not a thermal-mode GET. Keep verified CPU PL1 separate from
/// unqualified PM observations, and retain optional read errors in the message.
pub fn mode_written_status(mode: glph_core::hp_mode::HpMode, record: &serde_json::Value) -> String {
    let mut text = tf("mode.written", &[&t(&format!("mode.{}", mode.name()))]);
    let report = &record["result"]["Ok"];
    if let Some(watts) = report["cpu_pl1_w"].as_i64() {
        text.push(' ');
        text.push_str(&tf("mode.cpu_readback", &[&watts]));
    }
    if let Some(error) = report["cpu_limits_read_error"].as_str() {
        text.push(' ');
        text.push_str(&tf(
            "mode.cpu_unavailable",
            &[&crate::i18n::error_message(error)],
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_result_never_promotes_unqualified_observations_to_cpu_limit_readback() {
        use glph_core::hp_mode::HpMode;
        let unavailable = serde_json::json!({"result":{"Ok":{
            "cpu_pl1_w":null,"cpu_limit_observations_w":[55,80],"cpu_limits_read_error":"reader failed"
        }}});
        let message = mode_written_status(HpMode::Balanced, &unavailable);
        assert!(message.contains("reader failed"));
        assert!(!message.contains(&tf("mode.cpu_readback", &[&55])));
        assert!(!message.contains(&tf("mode.cpu_readback", &[&80])));
        let measured = serde_json::json!({"result":{"Ok":{
            "cpu_pl1_w":60,"cpu_limit_observations_w":[55,60],"cpu_limits_read_error":null
        }}});
        let message = mode_written_status(HpMode::Performance, &measured);
        assert!(message.contains(&tf("mode.cpu_readback", &[&60])));
        assert!(!message.contains(&tf("mode.cpu_readback", &[&55])));
        assert!(message.starts_with(&tf("mode.written", &[&t("mode.performance")])));
    }
}
