//! One-time local logon setup from this exact executable, after explicit UAC.
//! The fixed script is compiled in; no downloaded/user-supplied command runs.
use crate::i18n::t;
use std::{os::windows::process::CommandExt, path::PathBuf};
use windows_sys::Win32::{
    System::SystemInformation::GetSystemDirectoryW, UI::Shell::IsUserAnAdmin,
};
pub fn ensure_current_installation() -> Result<(), String> {
    if glph_platform::verify_startup_installation().is_ok() {
        return Ok(());
    }
    run_installer(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/install-startup.ps1"
    )))?;
    glph_platform::verify_startup_installation().map(|_| ())
}
/// Installs or updates the logon fan service from this exact executable.
pub fn ensure_fan_service() -> Result<(), String> {
    if glph_platform::fan_service_installed() {
        return Ok(());
    }
    run_installer(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/install-fan-service.ps1"
    )))?;
    if glph_platform::fan_service_installed() {
        Ok(())
    } else {
        Err(t("setup.fan_service_missing").into())
    }
}

/// Manages the dedicated Session-0 `PerformanceHubService` Windows service
/// (`Install`, `Start`, `Stop`, or `Remove`) via the compiled-in fixed script.
pub fn manage_windows_service(mode: &str) -> Result<(), String> {
    let source = if mode == "Install" {
        Some(glph_platform::service_client::sibling_service_binary()?)
    } else {
        None
    };
    run_installer_with(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/install-service.ps1"
        )),
        mode,
        source.as_deref(),
    )
}

fn run_installer(installer: &str) -> Result<(), String> {
    let source = std::env::current_exe().map_err(|e| e.to_string())?;
    run_installer_with(installer, "Install", Some(&source))
}

fn run_installer_with(
    installer: &str,
    mode: &str,
    source_binary: Option<&std::path::Path>,
) -> Result<(), String> {
    if unsafe { IsUserAnAdmin() } == 0 {
        return Err(t("setup.admin_required").into());
    }
    let mut system = [0u16; 32768];
    let count = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    if count == 0 || count >= system.len() {
        return Err(t("setup.system_dir").into());
    }
    let shell = PathBuf::from(String::from_utf16(&system[..count]).map_err(|e| e.to_string())?)
        .join("WindowsPowerShell/v1.0/powershell.exe");
    let source_arg = match source_binary {
        Some(path) => {
            let quoted = path
                .to_str()
                .ok_or(t("setup.path_encoding"))?
                // PowerShell also ends single-quoted strings at U+2018..U+201B.
                .chars()
                .flat_map(|c| {
                    let quote =
                        matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}');
                    std::iter::repeat_n(c, if quote { 2 } else { 1 })
                })
                .collect::<String>();
            format!(" -SourceBinary '{quoted}'")
        }
        None => String::new(),
    };
    let script = format!("& {{\n{installer}\n}} -Mode {mode}{source_arg}");
    let output = std::process::Command::new(shell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(script)
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let summary = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("PowerShell installer exited with error")
            .chars()
            .take(300)
            .collect::<String>();
        return Err(glph_core::message::msg("setup.failed", &[&summary]));
    }
    Ok(())
}
