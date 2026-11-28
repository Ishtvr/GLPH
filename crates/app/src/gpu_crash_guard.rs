//! Boot-Crash-Guard for GPU overclock / V/F curve / TGP applies.
//!
//! Writes an atomic marker file (`in-flight-gpu-test.json`) immediately before
//! applying GPU tuning controls and schedules it to clear after 15 s of stable
//! execution. If the system resets/BSODs during that window, the marker survives
//! into the next boot (`boot_unix_s` differs by >120 s), allowing startup apply
//! to pause automatically and warn the user instead of entering a boot loop.
use glph_platform::profile_store::ProfileStore;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MARKER_SCHEMA: u32 = 1;
const MARKER_FILE: &str = "in-flight-gpu-test.json";
const ARCHIVE_FILE: &str = "last-gpu-crash.json";
/// Tolerance for boot wall-clock jitter (matches `preset_hold.rs`).
const BOOT_DRIFT_TOLERANCE_S: u64 = 120;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InFlightGpuMarker {
    pub schema: u32,
    pub profile_id: String,
    pub boot_unix_s: u64,
    pub started_unix_s: u64,
}

fn now_unix_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(MARKER_FILE)
}

fn archive_path(root: &Path) -> PathBuf {
    root.join(ARCHIVE_FILE)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Marker path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".gpu-crash-guard-{}-{seq}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(error.to_string());
    }
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: Both paths are valid null-terminated UTF-16 strings.
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            let _ = fs::remove_file(&temp);
            return Err(err.to_string());
        }
    }
    #[cfg(not(windows))]
    {
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(error.to_string());
        }
    }
    Ok(())
}

fn read_marker_at(root: &Path) -> Option<InFlightGpuMarker> {
    let bytes = fs::read(marker_path(root)).ok()?;
    let marker: InFlightGpuMarker = serde_json::from_slice(&bytes).ok()?;
    (marker.schema == MARKER_SCHEMA && !marker.profile_id.is_empty()).then_some(marker)
}

fn mark_in_flight_at(
    root: &Path,
    profile_id: &str,
    boot_unix_s: u64,
    started_unix_s: u64,
) -> Result<InFlightGpuMarker, String> {
    if profile_id.trim().is_empty() {
        return Err("GPU crash guard requires a non-empty profile id".into());
    }
    let marker = InFlightGpuMarker {
        schema: MARKER_SCHEMA,
        profile_id: profile_id.to_owned(),
        boot_unix_s,
        started_unix_s,
    };
    let bytes = serde_json::to_vec_pretty(&marker).map_err(|e| e.to_string())?;
    atomic_write(&marker_path(root), &bytes)?;
    Ok(marker)
}

fn clear_in_flight_at(root: &Path, expected: Option<&InFlightGpuMarker>) {
    if let Some(expected) = expected
        && read_marker_at(root).as_ref() != Some(expected)
    {
        return;
    }
    let _ = fs::remove_file(marker_path(root));
}

fn check_and_consume_boot_crash_at(root: &Path, current_boot_unix_s: u64) -> Option<String> {
    let marker = read_marker_at(root)?;
    if marker.boot_unix_s.abs_diff(current_boot_unix_s) <= BOOT_DRIFT_TOLERANCE_S {
        return None;
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&marker) {
        let _ = atomic_write(&archive_path(root), &bytes);
    }
    let _ = fs::remove_file(marker_path(root));
    Some(marker.profile_id)
}

/// Atomically records that `profile_id` is currently applying/testing GPU settings.
pub fn mark_in_flight(profile_id: &str) -> Result<(), String> {
    let store = ProfileStore::user_data()?;
    mark_in_flight_at(
        store.root(),
        profile_id,
        glph_platform::boot_unix_s(),
        now_unix_s(),
    )?;
    Ok(())
}

/// Immediately clears any in-flight GPU test marker.
pub fn clear_in_flight() {
    if let Ok(store) = ProfileStore::user_data() {
        clear_in_flight_at(store.root(), None);
    }
}

/// Spawns a lightweight background timer thread that clears the current
/// in-flight marker after `delay` (e.g. 15 s) of stable execution, without
/// disturbing a newer apply's marker.
pub fn schedule_clear_after(delay: Duration) {
    let Ok(store) = ProfileStore::user_data() else {
        return;
    };
    let root = store.root().to_path_buf();
    let expected = read_marker_at(&root);
    let _ = std::thread::Builder::new()
        .name("gpu-crash-guard-clear".into())
        .spawn(move || {
            std::thread::sleep(delay);
            clear_in_flight_at(&root, expected.as_ref());
        });
}

/// Returns `Some(profile_id)` if an in-flight GPU test marker survived across
/// a system reset/reboot (different `boot_unix_s`), archiving and removing the
/// marker so startup apply pauses safely instead of boot-looping.
pub fn check_and_consume_boot_crash(current_boot_unix_s: u64) -> Option<String> {
    let store = ProfileStore::user_data().ok()?;
    check_and_consume_boot_crash_at(store.root(), current_boot_unix_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "glph-gpu-crash-guard-{label}-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mark_and_clear_round_trip() {
        let dir = TempDir::new("roundtrip");
        let marker = mark_in_flight_at(&dir.0, "gaming-uv", 1_700_000_000, 1_700_000_500).unwrap();
        assert_eq!(read_marker_at(&dir.0), Some(marker));
        clear_in_flight_at(&dir.0, None);
        assert_eq!(read_marker_at(&dir.0), None);
    }

    #[test]
    fn same_boot_marker_is_not_treated_as_boot_crash() {
        let dir = TempDir::new("same-boot");
        mark_in_flight_at(&dir.0, "gaming-uv", 1_700_000_000, 1_700_000_500).unwrap();
        assert_eq!(check_and_consume_boot_crash_at(&dir.0, 1_700_000_060), None);
        assert!(read_marker_at(&dir.0).is_some());
    }

    #[test]
    fn previous_boot_marker_is_detected_consumed_and_archived() {
        let dir = TempDir::new("boot-crash");
        mark_in_flight_at(&dir.0, "aggressive-oc", 1_700_000_000, 1_700_000_500).unwrap();
        let crashed = check_and_consume_boot_crash_at(&dir.0, 1_700_003_600);
        assert_eq!(crashed.as_deref(), Some("aggressive-oc"));
        assert_eq!(read_marker_at(&dir.0), None);
        assert!(archive_path(&dir.0).is_file());
        // Consumed once; second check does not re-trigger.
        assert_eq!(check_and_consume_boot_crash_at(&dir.0, 1_700_003_600), None);
    }

    #[test]
    fn stale_timer_does_not_clear_newer_marker() {
        let dir = TempDir::new("stale-timer");
        let first = mark_in_flight_at(&dir.0, "first-oc", 1_700_000_000, 1_700_000_500).unwrap();
        let second = mark_in_flight_at(&dir.0, "second-oc", 1_700_000_000, 1_700_000_510).unwrap();
        clear_in_flight_at(&dir.0, Some(&first));
        assert_eq!(read_marker_at(&dir.0), Some(second.clone()));
        clear_in_flight_at(&dir.0, Some(&second));
        assert_eq!(read_marker_at(&dir.0), None);
    }
}
