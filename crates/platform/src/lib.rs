#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub fn inventory() -> glph_core::Inventory {
    glph_core::Inventory {
        warnings: vec!["Hardware backend requires Windows".into()],
        ..Default::default()
    }
}

#[cfg(not(windows))]
pub fn power_status() -> glph_core::PowerStatus {
    Default::default()
}
#[cfg(any(windows, test))]
mod cooling_store;
#[cfg(any(windows, test))]
mod ownership_store;
pub mod plugin_store;
pub mod profile_store;
#[cfg(any(windows, test))]
mod tray_task;
