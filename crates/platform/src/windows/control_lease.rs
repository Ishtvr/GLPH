//! Cross-process ownership for GLPH hardware transactions.
use std::{
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, LocalFree,
        WAIT_ABANDONED, WAIT_OBJECT_0,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
    System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, OpenMutexW, ReleaseMutex,
        ResetEvent, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
    },
};

/// Stable machine-readable contention signal. Control-flow consumers compare
/// this code, never translated display prose.
pub(super) const HARDWARE_BUSY: &str = "⟦control.err.busy⟧";

/// Session-0 LocalSystem + Administrators full access; Interactive Users get
/// SYNCHRONIZE (0x00100000) | MUTEX_MODIFY_STATE (0x0001) so Session-1 helpers
/// can coordinate cleanly without `ERROR_ACCESS_DENIED`.
const MUTEX_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100001;;;IU)";
/// Same cross-session DACL for manual-reset events (SYNCHRONIZE | EVENT_MODIFY_STATE).
const EVENT_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100002;;;IU)";
const MUTEX_MODIFY_STATE_ACCESS: u32 = 0x0001;

pub const GPU_PAUSE_EVENT: &str = "Global\\GLPH.GpuPause";

/// Tracks nested `GpuPauseGuard` instances in this process so an inner drop
/// never prematurely resets `Global\GLPH.GpuPause` while an outer guard still
/// requires telemetry to stay paused.
static GPU_PAUSE_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub struct ControlLease {
    _hardware: MutexGuard,
    _foreground: Option<MutexGuard>,
}

/// RAII guard that holds `Global\GLPH.GpuPause` signaled while a
/// hardware controller performs a GPU PnP restart or intrusive VBIOS write.
/// Readers (`gpu-sensor-worker`, Session-0 cooling) observe the event and
/// immediately release NVML/NVAPI handles until the guard is dropped.
pub struct GpuPauseGuard {
    handle: HANDLE,
    tracked_global: bool,
}

impl Drop for GpuPauseGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            let should_reset = if self.tracked_global {
                let mut current = GPU_PAUSE_DEPTH.load(Ordering::SeqCst);
                loop {
                    let next = current.saturating_sub(1);
                    match GPU_PAUSE_DEPTH.compare_exchange_weak(
                        current,
                        next,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(prev) => break prev <= 1,
                        Err(actual) => current = actual,
                    }
                }
            } else {
                true
            };
            // SAFETY: Valid manual-reset event owned by this guard.
            unsafe {
                if should_reset {
                    ResetEvent(self.handle);
                }
                CloseHandle(self.handle);
            }
        }
    }
}

struct MutexGuard {
    handle: HANDLE,
}

const HARDWARE: &str = "Global\\GLPH.HardwareController";
const FOREGROUND: &str = "Global\\GLPH.ForegroundController";
/// Manual controls wait on workers, never on the window thread. Each mutex
/// has a finite deadline; an in-flight hardware transaction is never killed.
const MANUAL_WAIT_MS: u32 = 15_000;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Builds a `SECURITY_ATTRIBUTES` backed by a `LocalAlloc` security descriptor
/// from an SDDL string. The descriptor is freed automatically on drop.
pub(super) struct SddlSecurity {
    descriptor: *mut std::ffi::c_void,
    pub(super) attrs: SECURITY_ATTRIBUTES,
}

impl SddlSecurity {
    pub(super) fn parse(sddl: &str) -> Result<Self, String> {
        let sddl_w = wide(sddl);
        let mut descriptor = ptr::null_mut();
        // SAFETY: Valid NUL-terminated SDDL string, SDDL_REVISION_1 = 1.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self {
            descriptor,
            attrs: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }

    pub(super) fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attrs
    }
}

impl Drop for SddlSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: Allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe {
                LocalFree(self.descriptor);
            }
        }
    }
}

/// Convenience free function delegating to [`ControlLease::signal_gpu_pause`].
pub fn signal_gpu_pause() -> Option<GpuPauseGuard> {
    ControlLease::signal_gpu_pause()
}

/// Convenience free function delegating to [`ControlLease::gpu_pause_active`].
pub fn gpu_pause_active() -> bool {
    ControlLease::gpu_pause_active()
}

impl ControlLease {
    pub fn acquire() -> Result<Self, String> {
        Ok(Self {
            _hardware: MutexGuard::named(HARDWARE, 0)?,
            _foreground: None,
        })
    }
    /// Explicit Apply/Restore has precedence over opportunistic preset keeping.
    pub fn acquire_foreground() -> Result<Self, String> {
        Self::foreground(HARDWARE, FOREGROUND, MANUAL_WAIT_MS)
    }
    fn foreground(hardware: &str, foreground: &str, wait_ms: u32) -> Result<Self, String> {
        let intent = MutexGuard::named(foreground, wait_ms)?;
        let hardware = MutexGuard::named(hardware, wait_ms)?;
        Ok(Self {
            _hardware: hardware,
            _foreground: Some(intent),
        })
    }
    /// Background checks yield as soon as a manual command is waiting. The
    /// second check closes the race between opening the intent and hardware.
    pub fn acquire_background() -> Result<Self, String> {
        Self::background(HARDWARE, FOREGROUND)
    }
    fn background(hardware: &str, foreground: &str) -> Result<Self, String> {
        if foreground_exists(foreground) {
            return Err(HARDWARE_BUSY.into());
        }
        let lease = Self {
            _hardware: MutexGuard::named(hardware, 0)?,
            _foreground: None,
        };
        if foreground_exists(foreground) {
            return Err(HARDWARE_BUSY.into());
        }
        Ok(lease)
    }
    pub(crate) fn hp_ownership() -> Result<Self, String> {
        Ok(Self {
            _hardware: MutexGuard::named("Global\\GLPH.HpControllerOwner", 0)?,
            _foreground: None,
        })
    }

    /// Signals `Global\GLPH.GpuPause` so background GPU telemetry
    /// (`gpu-sensor-worker`, Session-0 cooling) immediately closes NVML/NVAPI
    /// handles before a GPU PnP restart or VBIOS write. Resets on drop.
    pub fn signal_gpu_pause() -> Option<GpuPauseGuard> {
        Self::signal_gpu_pause_named(GPU_PAUSE_EVENT)
    }

    fn signal_gpu_pause_named(name: &str) -> Option<GpuPauseGuard> {
        let name_w = wide(name);
        let security = SddlSecurity::parse(EVENT_SDDL).ok()?;
        // SAFETY: Valid NUL-terminated name and security attributes; manual-reset event.
        let mut handle = unsafe { CreateEventW(security.as_ptr(), 1, 0, name_w.as_ptr()) };
        if handle.is_null() {
            let err = unsafe { GetLastError() };
            if err == ERROR_ACCESS_DENIED {
                // Existing event owned by another security principal: open with minimal rights.
                handle = unsafe {
                    OpenEventW(
                        SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                        0,
                        name_w.as_ptr(),
                    )
                };
            }
        }
        if handle.is_null() {
            return None;
        }
        // SAFETY: Valid manual-reset event handle.
        if unsafe { SetEvent(handle) } == 0 {
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }
        let tracked_global = name == GPU_PAUSE_EVENT;
        if tracked_global {
            GPU_PAUSE_DEPTH.fetch_add(1, Ordering::SeqCst);
        }
        Some(GpuPauseGuard {
            handle,
            tracked_global,
        })
    }

    /// Non-blocking check whether a foreground GPU control currently holds
    /// `Global\PerformanceHub.GpuPause` signaled.
    pub fn gpu_pause_active() -> bool {
        Self::gpu_pause_active_named(GPU_PAUSE_EVENT)
    }

    fn gpu_pause_active_named(name: &str) -> bool {
        let name_w = wide(name);
        // SAFETY: Probe-only open of fixed named event; handle closed immediately.
        unsafe {
            let handle = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name_w.as_ptr());
            if handle.is_null() {
                return GetLastError() != ERROR_FILE_NOT_FOUND;
            }
            let signaled = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
            CloseHandle(handle);
            signaled
        }
    }
}

/// Cross-session mutex acquisition helper shared by `fan_service`
/// (`Global\GLPH.FanControllerOwner`) and PawnIO CPU/SMU access
/// (`Global\Access_PCI`).
pub(super) fn create_or_open_mutex(name: &str) -> Result<HANDLE, String> {
    let name_w = wide(name);
    let security = SddlSecurity::parse(MUTEX_SDDL)?;
    // SAFETY: Fixed NUL-terminated name and valid SDDL security attributes.
    let mut handle = unsafe { CreateMutexW(security.as_ptr(), 0, name_w.as_ptr()) };
    if handle.is_null() {
        let err = unsafe { GetLastError() };
        if err == ERROR_ACCESS_DENIED {
            // Existing mutex created by Session-0 LocalSystem or elevated Admin:
            // open with SYNCHRONIZE | MUTEX_MODIFY_STATE (no MUTEX_ALL_ACCESS needed).
            handle = unsafe {
                OpenMutexW(
                    SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE_ACCESS,
                    0,
                    name_w.as_ptr(),
                )
            };
        }
    }
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(handle)
}

fn foreground_exists(name: &str) -> bool {
    let name = wide(name);
    // SAFETY: Probe only this fixed named object; any handle closes immediately.
    unsafe {
        let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            return GetLastError() != ERROR_FILE_NOT_FOUND;
        }
        CloseHandle(handle);
        true
    }
}

impl MutexGuard {
    fn named(name: &str, wait_ms: u32) -> Result<Self, String> {
        let handle = create_or_open_mutex(name)?;
        // SAFETY: Valid mutex handle. Only worker/command paths request a wait.
        let wait = unsafe { WaitForSingleObject(handle, wait_ms) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            // SAFETY: This branch did not acquire the mutex; close only the handle.
            unsafe {
                CloseHandle(handle);
            }
            return Err(HARDWARE_BUSY.into());
        }
        // An abandoned mutex is owned now; the durable journal separately blocks retry.
        Ok(Self { handle })
    }
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        // SAFETY: This thread acquired the mutex. There is no Send/Sync implementation.
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_code_matches_the_language_neutral_message_contract() {
        assert_eq!(
            HARDWARE_BUSY,
            glph_core::message::msg("control.err.busy", &[])
        );
    }

    #[test]
    fn foreground_waits_for_an_existing_check_and_background_yields() {
        use std::{sync::mpsc, thread, time::Duration};
        let suffix = format!(
            "{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let hardware = format!("Local\\OmenHubPP.TestHardware.{suffix}");
        let foreground = format!("Local\\OmenHubPP.TestIntent.{suffix}");
        let background = ControlLease::background(&hardware, &foreground).unwrap();
        let (tx, rx) = mpsc::channel();
        let (release, done) = mpsc::channel();
        let (h, f) = (hardware.clone(), foreground.clone());
        let worker = thread::spawn(move || {
            let _manual = ControlLease::foreground(&h, &f, 2_000).unwrap();
            tx.send(()).unwrap();
            done.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        for _ in 0..100 {
            if foreground_exists(&foreground) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(foreground_exists(&foreground));
        assert!(rx.try_recv().is_err());
        assert!(ControlLease::background(&hardware, &foreground).is_err());
        drop(background);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(ControlLease::background(&hardware, &foreground).is_err());
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(ControlLease::background(&hardware, &foreground).is_ok());
    }

    #[test]
    fn gpu_pause_guard_signals_and_resets_cleanly() {
        let suffix = format!(
            "{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let event_name = format!("Local\\OmenHubPP.TestGpuPause.{suffix}");
        assert!(!ControlLease::gpu_pause_active_named(&event_name));
        let guard = ControlLease::signal_gpu_pause_named(&event_name).unwrap();
        assert!(ControlLease::gpu_pause_active_named(&event_name));
        drop(guard);
        assert!(!ControlLease::gpu_pause_active_named(&event_name));
    }

    #[test]
    fn create_or_open_mutex_reopens_existing_named_mutex() {
        let suffix = format!(
            "{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let name = format!("Local\\GLPH.TestAccessPci.{suffix}");
        let first = create_or_open_mutex(&name).expect("create initial mutex");
        let second = create_or_open_mutex(&name).expect("reopen existing mutex");
        // SAFETY: Both handles were created/opened above and are not owned.
        unsafe {
            CloseHandle(second);
            CloseHandle(first);
        }
    }

    #[test]
    fn cross_session_sddls_parse_and_grant_required_rights() {
        assert!(SddlSecurity::parse(MUTEX_SDDL).is_ok());
        assert!(SddlSecurity::parse(EVENT_SDDL).is_ok());
        for sddl in [MUTEX_SDDL, EVENT_SDDL] {
            assert!(sddl.starts_with("D:P"));
            assert!(sddl.contains("(A;;GA;;;SY)"));
            assert!(sddl.contains("(A;;GA;;;BA)"));
            assert!(sddl.contains(";;;IU)"));
        }
    }

    #[test]
    fn nested_global_gpu_pause_guards_stay_signaled_until_last_drop() {
        // Test the refcount logic cleanly with synthetic handles pointing to a
        // local test event so we never interfere with any live Global\GLPH.GpuPause.
        let suffix = format!(
            "{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let event_name = format!("Local\\GLPH.TestNestedGpuPause.{suffix}");
        let outer_raw = ControlLease::signal_gpu_pause_named(&event_name).unwrap();
        let inner_raw = ControlLease::signal_gpu_pause_named(&event_name).unwrap();
        // Enable refcount tracking for both handles to exercise the exact Drop path.
        let outer = GpuPauseGuard {
            handle: outer_raw.handle,
            tracked_global: true,
        };
        let inner = GpuPauseGuard {
            handle: inner_raw.handle,
            tracked_global: true,
        };
        std::mem::forget(outer_raw);
        std::mem::forget(inner_raw);
        GPU_PAUSE_DEPTH.fetch_add(2, Ordering::SeqCst);

        assert!(ControlLease::gpu_pause_active_named(&event_name));
        drop(inner);
        assert!(
            ControlLease::gpu_pause_active_named(&event_name),
            "inner GpuPauseGuard drop must not reset event while outer guard is held"
        );
        drop(outer);
        assert!(!ControlLease::gpu_pause_active_named(&event_name));
    }

    #[test]
    fn no_global_kernel_mutex_or_event_uses_null_security_descriptor() {
        // Regression guardrail for the Session-0 LocalSystem vs Session-1
        // `Global\Access_PCI` ERROR_ACCESS_DENIED (os error 5) bug class:
        // every Global\ mutex/event creation in crates/platform/src/windows
        // must route through explicit SDDL security descriptors.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/windows");
        let mut stack = vec![root];
        let mut scanned = 0usize;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    scanned += 1;
                    let text = std::fs::read_to_string(&path).unwrap();
                    let code = text.split("#[cfg(test)]").next().unwrap_or(&text);
                    if code.contains("Global\\\\") {
                        for (idx, line) in code.lines().enumerate() {
                            assert!(
                                !line.contains("CreateMutexW(ptr::null")
                                    && !line.contains("CreateMutexW(std::ptr::null")
                                    && !line.contains("CreateEventW(ptr::null")
                                    && !line.contains("CreateEventW(std::ptr::null"),
                                "Null security attributes used alongside Global\\ kernel object in {}:{}: {}",
                                path.display(),
                                idx + 1,
                                line.trim()
                            );
                        }
                    }
                }
            }
        }
        assert!(scanned >= 10, "expected to scan platform windows modules");
    }
}
