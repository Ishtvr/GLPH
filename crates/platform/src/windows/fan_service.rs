//! Background fan control that replaces the OMEN Hub's software fan loop.
//! It runs elevated from the user's logon task, follows the saved shared
//! curve and stops sending when paused or shut down. Firmware ownership has
//! no qualified GET; stopping alone is not proof of a BIOS handover.
//! The same service pins the foreground program to the saved CCD selection.
use super::{
    ControlLease, CpuHardwareReader,
    hp::{HpBios, Read},
    nvidia::GpuTemperature,
};
use glph_core::{
    Inventory,
    cooling::FirmwareIrTemperature,
    fan::{self, FanCurve, Smoother},
    profiles::FanPoint,
    quick_switch::FanMode,
};
use serde::Serialize;
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, GetCurrentProcess, GetCurrentThread,
            GetProcessTimes, GetThreadPriority, OpenEventW, ReleaseMutex, ResetEvent,
            SYNCHRONIZATION_SYNCHRONIZE, SetEvent, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
            WaitForSingleObject,
        },
    },
};

const CYCLE: Duration = Duration::from_secs(2);
/// Slice interval used while the loop holds an open NVAPI session (`gpu.is_some()`),
/// allowing a foreground VBIOS/PnP restart (`Global\GLPH.GpuPause`) to force an
/// immediate driver handle drop instead of waiting up to `CYCLE` (2 s).
const GPU_PAUSE_POLL: Duration = Duration::from_millis(100);
/// Below this GPU temperature the driver is asked only every
/// [`GPU_IDLE_POLL`], with no driver session kept in between, so a dGPU in
/// hybrid mode can power down. The OMEN Hub reads the GPU every 30 s in idle.
const GPU_IDLE_C: i32 = 60;
const GPU_IDLE_POLL: Duration = Duration::from_secs(30);
/// The chassis sensor is a firmware call; the OMEN Hub reads it every 30 s.
const IR_POLL: Duration = Duration::from_secs(10);
/// Vendor Hub process scan cadence (allocation-free UTF-16 check).
const HUB_POLL: Duration = Duration::from_secs(10);
/// Cross-session single-authority cooling owner mutex (Session 0 vs Session 1).
const GLOBAL_FAN_OWNER: &str = "Global\\GLPH.FanControllerOwner";

/// How the GPU temperature of one cycle is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuRead {
    /// The dGPU is powered down: no query (it would wake it).
    Asleep,
    /// Cool and read recently: reuse that reading, no driver session.
    Rest(i32),
    /// Ask the driver now.
    Query,
}
/// `awake` is the device-tree power state (`None`: unknown, ask as before);
/// `last` the previous reading with its age.
fn gpu_read(awake: Option<bool>, last: Option<(i32, Duration)>) -> GpuRead {
    if awake == Some(false) {
        return GpuRead::Asleep;
    }
    match last {
        Some((temperature, age)) if temperature < GPU_IDLE_C && age < GPU_IDLE_POLL => {
            GpuRead::Rest(temperature)
        }
        _ => GpuRead::Query,
    }
}
/// The Hub sends its heartbeat every 30 s; stay well inside that window.
const HEARTBEAT: Duration = Duration::from_secs(20);
const LEASE_RETRY: Duration = Duration::from_millis(50);
const CYCLE_LEASE_ATTEMPTS: usize = 6;
const RELEASE_LEASE_ATTEMPTS: usize = 41;
const HARDWARE_BUSY: &str = super::control_lease::HARDWARE_BUSY;

enum WriteAccess<T> {
    Acquired(T),
    Busy,
    Stop,
}
/// Every acquisition is nonblocking. Bound only the explicit retry waits;
/// failed access/handle creation remains an error, not presumed contention.
fn acquire_write_access<T>(
    attempts: usize,
    mut acquire: impl FnMut() -> Result<T, String>,
    mut wait: impl FnMut(Duration) -> Result<bool, String>,
) -> Result<WriteAccess<T>, String> {
    for attempt in 0..attempts {
        match acquire() {
            Ok(lease) => return Ok(WriteAccess::Acquired(lease)),
            Err(error) if error == HARDWARE_BUSY => {
                if attempt + 1 < attempts && wait(LEASE_RETRY)? {
                    return Ok(WriteAccess::Stop);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(WriteAccess::Busy)
}

fn wait_for_stop_or_gpu_pause_with_clock<T>(
    gpu: &mut Option<T>,
    delay: Duration,
    mut pause_active: impl FnMut() -> bool,
    mut wait_step: impl FnMut(Duration) -> Result<(bool, Duration), String>,
) -> Result<bool, String> {
    if delay.is_zero() || gpu.is_none() {
        return wait_step(delay).map(|(stopping, _)| stopping);
    }
    let mut remaining = delay;
    while !remaining.is_zero() {
        if pause_active() {
            *gpu = None;
            return wait_step(remaining).map(|(stopping, _)| stopping);
        }
        let step = remaining.min(GPU_PAUSE_POLL);
        let (stopping, elapsed) = wait_step(step)?;
        if stopping {
            return Ok(true);
        }
        // Honor early wakeups (e.g. Session-0 standby or live config update)
        // instead of sleeping through the rest of the cycle.
        if step >= Duration::from_millis(20) && elapsed < step / 2 {
            return Ok(false);
        }
        remaining = remaining.saturating_sub(step);
    }
    Ok(false)
}

/// Sleeps for `delay` while keeping an open NVAPI session (`gpu.is_some()`)
/// responsive to `Global\GLPH.GpuPause`. When `gpu` is already `None`, waits
/// in a single `stop(delay)` call with zero extra wakeups.
fn wait_for_stop_or_gpu_pause<T>(
    gpu: &mut Option<T>,
    delay: Duration,
    pause_active: impl FnMut() -> bool,
    stop: &mut impl FnMut(Duration) -> Result<bool, String>,
) -> Result<bool, String> {
    wait_for_stop_or_gpu_pause_with_clock(gpu, delay, pause_active, |step| {
        let before = Instant::now();
        let stopping = stop(step)?;
        Ok((stopping, before.elapsed()))
    })
}

#[derive(Clone, Copy)]
struct ProcessIdentity {
    id: u32,
    started: u64,
}
impl ProcessIdentity {
    fn current() -> Result<Self, String> {
        let mut created: FILETIME = unsafe { std::mem::zeroed() };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        // SAFETY: Current-process pseudo-handle and exact writable FILETIMEs.
        if unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let started = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        if started == 0 {
            return Err("Fan process creation identity is unavailable".into());
        }
        Ok(Self {
            id: std::process::id(),
            started,
        })
    }
}
fn heartbeat_needed(
    previous: Option<FanMode>,
    mode: FanMode,
    elapsed: Option<Duration>,
    blocked: bool,
) -> bool {
    (mode == FanMode::Max && previous != Some(FanMode::Max))
        || blocked
        || elapsed.is_none_or(|elapsed| elapsed >= HEARTBEAT)
}
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|time| time.as_millis().try_into().ok())
        .unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
enum CoolingRelease {
    NoWrite,
    Maximum,
    Vendor,
}
fn release_locked<T>(
    control: &mut fan::SoftwareControl,
    acquire: impl FnOnce() -> Result<T, String>,
    vendor: impl FnOnce() -> Result<bool, String>,
    maximum: impl FnOnce() -> Result<(), String>,
) -> Result<CoolingRelease, String> {
    let mut vendor_took_over = false;
    let issued = control.release(|| {
        let _lease = acquire()?;
        if vendor()? {
            vendor_took_over = true;
            Ok(())
        } else {
            maximum()
        }
    })?;
    if vendor_took_over {
        control.vendor_active();
        Ok(CoolingRelease::Vendor)
    } else if issued {
        Ok(CoolingRelease::Maximum)
    } else {
        Ok(CoolingRelease::NoWrite)
    }
}
fn release_maximum(
    control: &mut fan::SoftwareControl,
    bios: &HpBios,
) -> Result<CoolingRelease, String> {
    release_locked(
        control,
        || match acquire_write_access(RELEASE_LEASE_ATTEMPTS, ControlLease::acquire, |delay| {
            std::thread::sleep(delay);
            Ok(false)
        })? {
            WriteAccess::Acquired(lease) => Ok(lease),
            _ => Err(
                "Hardware transaction stayed busy; maximum-cooling handover is unconfirmed".into(),
            ),
        },
        || {
            hub_running().ok_or(
                "Controller ownership is unknown; safe cooling release cannot be dispatched".into(),
            )
        },
        || {
            bios.set_fan_levels(fan::MAX_LEVEL, fan::MAX_LEVEL)
                .map_err(|e| e.to_string())
        },
    )
}
/// While the original Hub runs it owns the fans; the service waits.
const HUB_PROCESSES: [&[u8]; 2] = [
    b"omencommandcenterbackground.exe",
    b"hp.omen.application.background.tasks.container.exe",
];

fn utf16_eq_ascii_ignore_case(wide: &[u16], ascii_lower: &[u8]) -> bool {
    if wide.len() != ascii_lower.len() {
        return false;
    }
    wide.iter()
        .zip(ascii_lower.iter())
        .all(|(&w, &a)| w <= 0x7F && (w as u8).to_ascii_lowercase() == a)
}

/// Cross-session single-authority guard ensuring Session-0 `GLPHService`
/// and the legacy Session-1 `fan-service` task never write fans concurrently.
struct GlobalFanOwner {
    mutex: HANDLE,
}
impl GlobalFanOwner {
    fn acquire() -> Result<Self, String> {
        let mutex = super::control_lease::create_or_open_mutex(GLOBAL_FAN_OWNER)?;
        // SAFETY: Valid SDDL mutex handle, non-blocking ownership check.
        let wait = unsafe { WaitForSingleObject(mutex, 0) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                CloseHandle(mutex);
            }
            return Err("Another GLPH fan controller is already active".into());
        }
        Ok(Self { mutex })
    }
}
impl Drop for GlobalFanOwner {
    fn drop(&mut self) {
        // SAFETY: This thread acquired `self.mutex`.
        unsafe {
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
    }
}

/// One controller per interactive user/session, with an interruptible wait.
/// Installers request a graceful stop so CCD originals can be released before
/// replacing the executable. This is independent of the short hardware lease.
struct Lifecycle {
    mutex: HANDLE,
    stop: HANDLE,
}
fn object_name(kind: &str) -> Result<Vec<u16>, String> {
    Ok(
        format!("Local\\GLPH.Fan.{kind}.{}", super::current_user_sid()?)
            .encode_utf16()
            .chain(Some(0))
            .collect(),
    )
}
impl Lifecycle {
    fn start() -> Result<Self, String> {
        let sid = super::current_user_sid()?;
        let name = object_name("Owner")?;
        // Per-user explicit DACL + Medium integrity label so an elevated
        // `glph-fan.exe` still allows the same interactive user to signal Stop.
        let mutex_sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100001;;;{sid})");
        let mutex_sec = super::control_lease::SddlSecurity::parse(&mutex_sddl)?;
        // SAFETY: Terminated per-user name, explicit per-user security, no initial owner.
        let mutex = unsafe { CreateMutexW(mutex_sec.as_ptr(), 0, name.as_ptr()) };
        if mutex.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: Valid mutex, bounded acquisition on this thread.
        let wait = unsafe { WaitForSingleObject(mutex, 0) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                CloseHandle(mutex);
            }
            return Err("A GLPH fan controller is already running in this session".into());
        }
        let mut owner = Self {
            mutex,
            stop: std::ptr::null_mut(),
        };
        let name = object_name("Stop")?;
        let stop_sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100002;;;{sid})");
        let stop_sec = super::control_lease::SddlSecurity::parse(&stop_sddl)?;
        // SAFETY: Manual-reset event, fixed per-user name and explicit per-user DACL.
        owner.stop = unsafe { CreateEventW(stop_sec.as_ptr(), 1, 0, name.as_ptr()) };
        if owner.stop.is_null() {
            // Reopen if an older instance still holds the named stop event.
            owner.stop = unsafe {
                OpenEventW(
                    SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                    0,
                    name.as_ptr(),
                )
            };
        }
        if owner.stop.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // A completed prior instance may have left the named event signalled.
        if unsafe { ResetEvent(owner.stop) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(owner)
    }
    fn wait(&self, timeout: Duration) -> Result<bool, String> {
        // SAFETY: Owned event; each wait is bounded and interruptible.
        match unsafe {
            WaitForSingleObject(
                self.stop,
                timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32,
            )
        } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error().to_string()),
        }
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        // SAFETY: This thread acquired the mutex and owns both handles.
        unsafe {
            if !self.stop.is_null() {
                CloseHandle(self.stop);
            }
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
    }
}
pub fn request_stop() -> Result<bool, String> {
    let name = object_name("Stop")?;
    // SAFETY: Opens only the per-user fixed stop event, without inheritance.
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if event.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(2) {
            Ok(false)
        } else {
            Err(error.to_string())
        };
    }
    let success = unsafe { SetEvent(event) } != 0;
    let error = std::io::Error::last_os_error();
    unsafe {
        CloseHandle(event);
    }
    if success {
        Ok(true)
    } else {
        Err(error.to_string())
    }
}

#[derive(Debug, Serialize)]
pub struct FanServiceStatus {
    pub schema: u32,
    pub process_id: u32,
    /// Kernel process creation FILETIME, paired with PID to reject stale files.
    pub process_started: u64,
    /// Last successful heartbeat in this process, never a status refresh time.
    pub last_heartbeat_unix_ms: Option<u64>,
    pub state: &'static str,
    pub detail: String,
    pub cpu_c: Option<f64>,
    pub gpu_c: Option<i32>,
    pub ir_c: Option<u8>,
    pub protection: u8,
    pub cpu_level: u8,
    pub gpu_level: u8,
    /// Programs currently pinned to the saved CCD selection.
    pub ccd_pinned: usize,
    pub updated_unix_s: u64,
    pub updated_unix_ms: u64,
}

/// None means enumeration failed, not that a vendor actually took ownership.
fn hub_running() -> Option<bool> {
    // SAFETY: Snapshot handle is closed below; the entry is sized as required.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            // Unknown ownership must not start a second controller.
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        if !ok {
            CloseHandle(snapshot);
            return None;
        }
        while ok && !found {
            let length = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let slice = &entry.szExeFile[..length];
            found = HUB_PROCESSES
                .iter()
                .any(|target| utf16_eq_ascii_ignore_case(slice, target));
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        // A failure in the middle of enumeration must not masquerade as an
        // absent controller. ERROR_NO_MORE_FILES is the normal end marker.
        let complete = found || std::io::Error::last_os_error().raw_os_error() == Some(18);
        CloseHandle(snapshot);
        complete.then_some(found)
    }
}

/// Gives every pinned program its affinity back when the service ends,
/// including an early error return.
struct Pinning {
    enforcer: glph_core::ccd::Enforcer,
    processes: super::ccd::WindowsProcesses,
}
impl Drop for Pinning {
    fn drop(&mut self) {
        self.enforcer.release(&mut self.processes);
    }
}

fn curve_of(points: &[FanPoint]) -> Result<FanCurve, String> {
    FanCurve::new(
        points
            .iter()
            .map(|p| fan::Point {
                temperature_c: i32::from(p.temperature_c),
                percent: p.duty_percent,
            })
            .collect(),
    )
    .map_err(String::from)
}

fn requested_level(
    mode: FanMode,
    curve: Option<&FanCurve>,
    full: bool,
    temperature: Option<i32>,
) -> u8 {
    if mode == FanMode::Max || full {
        return fan::MAX_LEVEL;
    }
    curve.map_or(fan::MAX_LEVEL, |curve| {
        fan::level(curve.target(temperature))
    })
}

/// Runs until the process ends. Auto pauses the software loop after a safe
/// release; it does not assert a firmware ownership GET. Curve follows `load`
/// and Max runs both fans at maximum. `ccd` returns the saved CCD selection.
/// `report` persists each status.
pub fn run(
    inventory: &Inventory,
    mode: impl FnMut() -> FanMode,
    load: impl FnMut() -> Result<Option<Vec<FanPoint>>, String>,
    ccd: impl FnMut() -> Result<Option<u8>, String>,
    report: impl FnMut(&FanServiceStatus),
) -> Result<(), String> {
    let lifecycle = Lifecycle::start()?;
    run_until(inventory, mode, load, ccd, report, |wait| {
        lifecycle.wait(wait)
    })
}

const THREAD_PRIORITY_ERROR: i32 = i32::MAX;

/// Session-1 foreground CCD pinning helper. Runs in the interactive user
/// session (e.g. `performance-hub.exe --tray`) because Session-0 Windows
/// services are isolated from the desktop foreground window.
pub struct ForegroundCcdPinner {
    masks: Result<Vec<usize>, String>,
    pinning: Pinning,
}

impl Default for ForegroundCcdPinner {
    fn default() -> Self {
        Self::new()
    }
}

impl ForegroundCcdPinner {
    pub fn new() -> Self {
        Self {
            masks: super::ccd::ccd_masks(),
            pinning: Pinning {
                enforcer: glph_core::ccd::Enforcer::default(),
                processes: super::ccd::WindowsProcesses::default(),
            },
        }
    }

    /// Evaluates the current interactive foreground window and applies/releases
    /// the saved CCD affinity mask. Returns the number of currently pinned programs.
    pub fn tick(&mut self, selection: Option<u8>) -> usize {
        let target = match (&self.masks, selection) {
            (Ok(masks), Some(sel)) => glph_core::ccd::affinity(sel, masks).unwrap_or(None),
            _ => None,
        };
        let Pinning {
            enforcer,
            processes,
        } = &mut self.pinning;
        enforcer.tick(processes, target);
        enforcer.pinned()
    }
}

struct BelowNormalThreadGuard {
    previous: i32,
}
impl BelowNormalThreadGuard {
    fn enter() -> Self {
        // SAFETY: Adjusts only the current cooling worker thread priority so
        // periodic hardware polling never preempts foreground games or audio.
        let previous = unsafe { GetThreadPriority(GetCurrentThread()) };
        if previous != THREAD_PRIORITY_ERROR && previous > THREAD_PRIORITY_BELOW_NORMAL {
            unsafe {
                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            Self { previous }
        } else {
            Self {
                previous: THREAD_PRIORITY_ERROR,
            }
        }
    }
}
impl Drop for BelowNormalThreadGuard {
    fn drop(&mut self) {
        if self.previous != THREAD_PRIORITY_ERROR {
            unsafe {
                SetThreadPriority(GetCurrentThread(), self.previous);
            }
        }
    }
}

/// Session-0 Windows Service cooling loop (runs under `LocalSystem` before user
/// logon). Skips interactive foreground CCD pinning (handled in Session 1 by
/// [`ForegroundCcdPinner`]) and reads the chassis IR limit from the caller's
/// machine-wide config (`C:\ProgramData\PerformanceHub\service-config.json`).
/// When `is_suspended()` returns `true`, cooling pauses cleanly without
/// triggering a 100 % `release_maximum()` fan blast into standby.
#[allow(clippy::too_many_arguments)]
pub fn run_session0_cooling(
    inventory: &Inventory,
    mode: impl FnMut() -> FanMode,
    load: impl FnMut() -> Result<Option<Vec<FanPoint>>, String>,
    ir_limit: impl FnMut() -> u8,
    is_suspended: impl FnMut() -> bool,
    report: impl FnMut(&FanServiceStatus),
    stop: impl FnMut(Duration) -> Result<bool, String>,
) -> Result<(), String> {
    let _prio = BelowNormalThreadGuard::enter();
    run_loop(
        inventory,
        mode,
        load,
        || Ok(None),
        ir_limit,
        is_suspended,
        false,
        report,
        stop,
    )
}

/// Same production loop with a bounded stop source for qualification. Stopping
/// releases CCD affinities and ceases firmware heartbeat/level writes.
pub fn run_until(
    inventory: &Inventory,
    mode: impl FnMut() -> FanMode,
    load: impl FnMut() -> Result<Option<Vec<FanPoint>>, String>,
    ccd: impl FnMut() -> Result<Option<u8>, String>,
    report: impl FnMut(&FanServiceStatus),
    stop: impl FnMut(Duration) -> Result<bool, String>,
) -> Result<(), String> {
    let _prio = BelowNormalThreadGuard::enter();
    run_loop(
        inventory,
        mode,
        load,
        ccd,
        || {
            crate::profile_store::ProfileStore::user_data()
                .ok()
                .and_then(|s| s.chassis_max_temp_c().ok())
                .flatten()
                .unwrap_or(fan::IR_EMERGENCY_C)
        },
        || false,
        true,
        report,
        stop,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    inventory: &Inventory,
    mut mode: impl FnMut() -> FanMode,
    mut load: impl FnMut() -> Result<Option<Vec<FanPoint>>, String>,
    mut ccd: impl FnMut() -> Result<Option<u8>, String>,
    mut ir_limit: impl FnMut() -> u8,
    mut is_suspended: impl FnMut() -> bool,
    enable_ccd_pinning: bool,
    mut report: impl FnMut(&FanServiceStatus),
    mut stop: impl FnMut(Duration) -> Result<bool, String>,
) -> Result<(), String> {
    if inventory.manufacturer != "HP" || inventory.board != "8E35" || inventory.bios != "F.13" {
        return Err("Fan control is qualified only for HP8E35/F.13".into());
    }
    let _global_owner = GlobalFanOwner::acquire()?;
    let instance = ProcessIdentity::current()?;
    let bios = HpBios::new(inventory).map_err(|e| e.to_string())?;
    // Idle BIOS/Hub-waiting states do not open a kernel reader or NVIDIA DLL.
    let mut cpu = None;
    let mut gpu: Option<GpuTemperature> = None;
    let mut last_gpu: Option<(i32, Instant)> = None;
    let mut last_ir: Option<(u8, Instant)> = None;
    let mut last_hub: Option<(bool, Instant)> = None;
    let mut smoother = (Smoother::default(), Smoother::default());
    let mut last_heartbeat: Option<Instant> = None;
    let mut last_heartbeat_unix_ms = None;
    let mut last_written_mode = None;
    let mut last_written_levels: Option<(u8, u8)> = None;
    let mut write_was_blocked = false;
    let mut control = fan::SoftwareControl::default();
    let mut protection = 0u8;
    let ccd_masks = if enable_ccd_pinning {
        super::ccd::ccd_masks()
    } else {
        Err("Session-0 service delegates foreground CCD pinning to Session 1".into())
    };
    let mut pinning = Pinning {
        enforcer: glph_core::ccd::Enforcer::default(),
        processes: super::ccd::WindowsProcesses::default(),
    };
    let result = (|| -> Result<(), String> {
        loop {
            if stop(Duration::ZERO)? {
                return Ok(());
            }
            let started = Instant::now();
            // CCD pinning runs only in interactive Session-1 contexts. An
            // unreadable selection or topology releases every pinned program.
            let ccd_pinned = if enable_ccd_pinning {
                let target = match (&ccd_masks, ccd()) {
                    (Ok(masks), Ok(Some(selection))) => {
                        glph_core::ccd::affinity(selection, masks).unwrap_or(None)
                    }
                    _ => None,
                };
                let Pinning {
                    enforcer,
                    processes,
                } = &mut pinning;
                enforcer.tick(processes, target);
                enforcer.pinned()
            } else {
                0
            };
            let heartbeat_for_idle = last_heartbeat_unix_ms;
            let idle = |state: &'static str, detail: String| {
                let updated_unix_ms = unix_ms();
                FanServiceStatus {
                    schema: 1,
                    process_id: instance.id,
                    process_started: instance.started,
                    last_heartbeat_unix_ms: heartbeat_for_idle,
                    state,
                    detail,
                    cpu_c: None,
                    gpu_c: None,
                    ir_c: None,
                    protection: 0,
                    cpu_level: 0,
                    gpu_level: 0,
                    ccd_pinned,
                    updated_unix_s: updated_unix_ms / 1000,
                    updated_unix_ms,
                }
            };
            if is_suspended() {
                // Entering standby: release sensor handles and pause cleanly
                // without blasting 100 % fans via `release_maximum()`.
                cpu = None;
                gpu = None;
                last_heartbeat = None;
                last_written_mode = None;
                last_written_levels = None;
                last_hub = None;
                report(&idle(
                    "suspended",
                    "System is entering standby; cooling loop paused".into(),
                ));
                if stop(Duration::from_secs(2))? {
                    return Ok(());
                }
                continue;
            }
            let pause = |control: &mut fan::SoftwareControl,
                         detail: String|
             -> Result<FanServiceStatus, String> {
                let release = release_maximum(control, &bios)?;
                if release == CoolingRelease::Vendor {
                    return Ok(idle(
                        "waiting_for_hub",
                        glph_core::message::msg("fan.status.hub_running", &[]),
                    ));
                }
                if control.handover_pending() {
                    Ok(FanServiceStatus {
                        cpu_level: fan::MAX_LEVEL,
                        gpu_level: fan::MAX_LEVEL,
                        ..idle(
                            "handover_pending",
                            glph_core::message::msg("fan.status.handover", &[]),
                        )
                    })
                } else {
                    Ok(idle("bios", detail))
                }
            };
            // Pause our controller by sending nothing. Throttled to HUB_POLL
            // (10 s) in steady state using an allocation-free UTF-16 scan.
            let vendor = if let Some((cached, at)) = last_hub
                && at.elapsed() < HUB_POLL
            {
                cached
            } else {
                let live = hub_running()
                    .ok_or("Original controller enumeration failed; ownership is unknown")?;
                last_hub = Some((live, Instant::now()));
                live
            };
            if vendor {
                control.vendor_active();
                last_heartbeat = None;
                last_written_mode = None;
                last_written_levels = None;
                cpu = None;
                gpu = None;
                report(&idle(
                    "waiting_for_hub",
                    glph_core::message::msg("fan.status.hub_running", &[]),
                ));
                if stop(Duration::from_secs(2))? {
                    return Ok(());
                }
                continue;
            }
            let mode = mode();
            if mode == FanMode::Bios {
                last_heartbeat = None;
                last_written_levels = None;
                cpu = None;
                gpu = None;
                report(&pause(
                    &mut control,
                    glph_core::message::msg("fan.status.bios", &[]),
                )?);
                if stop(Duration::from_secs(2))? {
                    return Ok(());
                }
                continue;
            }
            let max = mode == FanMode::Max;
            let curve = match load().and_then(|p| p.map(|p| curve_of(&p)).transpose()) {
                Ok(curve) if max => curve,
                Ok(Some(curve)) => Some(curve),
                Ok(None) => {
                    last_heartbeat = None;
                    last_written_levels = None;
                    cpu = None;
                    gpu = None;
                    report(&pause(
                        &mut control,
                        glph_core::message::msg("fan.status.no_curve", &[]),
                    )?);
                    if stop(Duration::from_secs(2))? {
                        return Ok(());
                    }
                    continue;
                }
                Err(_) if max => None,
                Err(e) => {
                    last_heartbeat = None;
                    last_written_levels = None;
                    cpu = None;
                    gpu = None;
                    report(&pause(
                        &mut control,
                        glph_core::message::msg("fan.status.invalid_curve", &[&e]),
                    )?);
                    if stop(Duration::from_secs(2))? {
                        return Ok(());
                    }
                    continue;
                }
            };
            if last_heartbeat.is_none() {
                // Qualify the complete measured fan layout before the first
                // heartbeat/level SET, including a new arm after BIOS/Hub waiting.
                super::cooling::FanControl::new(inventory)?.capture()?;
            }
            if max {
                cpu = None;
                gpu = None;
            }
            if !max && cpu.is_none() {
                cpu = CpuHardwareReader::new(inventory).ok();
            }
            let cpu_c = match cpu.as_mut().map(|r| r.temperature_sample()) {
                Some(Ok(sample)) => sample.temperature_c,
                Some(Err(_)) => {
                    // Drop a stale/broken driver handle (e.g. after S0ix or driver
                    // reload) so the next tick cleanly reopens CpuHardwareReader
                    // instead of latching emergency full cooling indefinitely.
                    cpu = None;
                    None
                }
                None => None,
            };
            let gpu_paused = ControlLease::gpu_pause_active();
            let gpu_plan = if max || gpu_paused {
                if gpu_paused {
                    // Release NVML/NVAPI handles immediately before a PnP restart.
                    gpu = None;
                    match last_gpu {
                        Some((temperature, at)) if at.elapsed() < Duration::from_secs(60) => {
                            GpuRead::Rest(temperature)
                        }
                        _ => GpuRead::Asleep,
                    }
                } else {
                    GpuRead::Asleep
                }
            } else {
                gpu_read(
                    super::dgpu::nvidia_gpu_awake(),
                    last_gpu.map(|(temperature, at)| (temperature, at.elapsed())),
                )
            };
            let gpu_asleep = !max && gpu_plan == GpuRead::Asleep;
            let gpu_c = match gpu_plan {
                GpuRead::Asleep => {
                    gpu = None;
                    last_gpu = None;
                    None
                }
                GpuRead::Rest(temperature) => Some(temperature),
                GpuRead::Query => {
                    if gpu.is_none() {
                        gpu = GpuTemperature::new().ok();
                    }
                    let reading = gpu.as_ref().and_then(|reader| reader.sample().ok());
                    last_gpu = reading.map(|temperature| (temperature, Instant::now()));
                    // A cool GPU is not polled until the reading expires;
                    // release the driver session so it may power down. Also
                    // release immediately if a foreground PnP pause began
                    // while `sample()` was executing.
                    if reading.is_none_or(|temperature| temperature < GPU_IDLE_C)
                        || ControlLease::gpu_pause_active()
                    {
                        gpu = None;
                    }
                    reading
                }
            };
            if last_ir.is_none_or(|(_, at)| at.elapsed() >= IR_POLL) {
                last_ir = bios
                    .get(Read::SurfaceSensor)
                    .ok()
                    .and_then(|(code, bytes)| FirmwareIrTemperature::decode(code, &bytes).ok())
                    .map(|t| (t.celsius, Instant::now()));
            }
            let ir_c = last_ir.map(|(celsius, _)| celsius);
            // All sensor/layout reads above happen without the hardware lease.
            // A bounded busy cycle preserves responsibility and stays alive;
            // it must not publish a fresh Max acknowledgement or renew OMCC.
            let write_lease = match acquire_write_access(
                CYCLE_LEASE_ATTEMPTS,
                ControlLease::acquire,
                |delay| {
                    if ControlLease::gpu_pause_active() {
                        gpu = None;
                    }
                    stop(delay)
                },
            )? {
                WriteAccess::Acquired(lease) => lease,
                WriteAccess::Stop => return Ok(()),
                WriteAccess::Busy => {
                    // A foreground hardware transaction (e.g. preset apply or
                    // VBIOS TGP PnP restart) owns the lease: drop any live
                    // NVAPI session immediately before sleeping out the cycle.
                    gpu = None;
                    write_was_blocked = true;
                    report(&idle("waiting_for_control", "Hardware transaction is busy; cooling output is pending and maximum will be established when the lease returns".into()));
                    if stop(CYCLE.saturating_sub(started.elapsed()))? {
                        return Ok(());
                    }
                    continue;
                }
            };
            if stop(Duration::ZERO)? {
                return Ok(());
            }
            // Re-verify vendor absence on initial arm or once per HUB_POLL window.
            let vendor_before_write = if last_heartbeat.is_none()
                || last_hub.is_none_or(|(_, at)| at.elapsed() >= HUB_POLL)
            {
                let live = hub_running()
                    .ok_or("Original controller enumeration failed before fan write")?;
                last_hub = Some((live, Instant::now()));
                live
            } else {
                last_hub.is_some_and(|(cached, _)| cached)
            };
            if vendor_before_write {
                control.vendor_active();
                last_heartbeat = None;
                last_written_mode = None;
                last_written_levels = None;
                cpu = None;
                gpu = None;
                drop(write_lease);
                report(&idle(
                    "waiting_for_hub",
                    glph_core::message::msg("fan.status.hub_running", &[]),
                ));
                if stop(CYCLE.saturating_sub(started.elapsed()))? {
                    return Ok(());
                }
                continue;
            }
            if heartbeat_needed(
                last_written_mode,
                mode,
                last_heartbeat.map(|time| time.elapsed()),
                write_was_blocked,
            ) {
                control.arm();
                protection = bios.heartbeat().map_err(|e| e.to_string())?;
                last_heartbeat = Some(Instant::now());
                last_heartbeat_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|time| time.as_millis().try_into().ok());
            }
            // Maximum is unconditional and works without loading sensor drivers.
            // A curve may never lower cooling when its GPU reader is unavailable;
            // a powered-down dGPU is not a failed reader and produces no heat.
            let ir_limit_c = ir_limit();
            let full = !max
                && (write_was_blocked
                    || (gpu_c.is_none() && !gpu_asleep)
                    || fan::emergency_with_ir(
                        cpu_c.map(|t| t as f32),
                        gpu_c.map(|t| t as f32),
                        ir_c,
                        ir_limit_c,
                        protection,
                    ));
            let target = |t: Option<i32>| requested_level(mode, curve.as_ref(), full, t);
            let cpu_t = cpu_c.map(|t| t.round() as i32);
            // The GPU fan follows the hotter of GPU and CPU when the GPU sleeps.
            let gpu_t = gpu_c.or(cpu_t);
            let cpu_level = smoother.0.step(target(cpu_t));
            let gpu_level = smoother.1.step(target(gpu_t));
            let levels_changed = last_written_levels != Some((cpu_level, gpu_level))
                || last_written_mode != Some(mode)
                || write_was_blocked;
            if levels_changed {
                control.arm();
                bios.set_fan_levels(cpu_level, gpu_level)
                    .map_err(|e| e.to_string())?;
                last_written_levels = Some((cpu_level, gpu_level));
            }
            last_written_mode = Some(mode);
            write_was_blocked = false;
            drop(write_lease);
            let updated_unix_ms = unix_ms();
            report(&FanServiceStatus {
                schema: 1,
                process_id: instance.id,
                process_started: instance.started,
                last_heartbeat_unix_ms,
                state: if full {
                    "full_cooling"
                } else if max {
                    "max"
                } else {
                    "curve"
                },
                detail: String::new(),
                cpu_c,
                gpu_c,
                ir_c,
                protection,
                cpu_level,
                gpu_level,
                ccd_pinned,
                updated_unix_s: updated_unix_ms / 1000,
                updated_unix_ms,
            });
            if wait_for_stop_or_gpu_pause(
                &mut gpu,
                CYCLE.saturating_sub(started.elapsed()),
                ControlLease::gpu_pause_active,
                &mut stop,
            )? {
                return Ok(());
            }
        }
    })();
    // A clean stop and a failed attempt use the same conservative release.
    // Never overwrite a positively identified vendor controller. If ownership
    // cannot be enumerated, retain an explicit error instead of claiming Auto.
    let release = match hub_running() {
        Some(true) => {
            control.vendor_active();
            Ok(CoolingRelease::Vendor)
        }
        Some(false) => release_maximum(&mut control, &bios),
        None => Err("Controller ownership is unknown; safe release could not be dispatched".into()),
    };
    drop(pinning);
    let updated_unix_ms = unix_ms();
    if control.handover_pending() && release.is_ok() {
        report(&FanServiceStatus {
            schema: 1,
            process_id: instance.id,
            process_started: instance.started,
            last_heartbeat_unix_ms,
            state: "handover_pending",
            detail: glph_core::message::msg("fan.status.handover", &[]),
            cpu_c: None,
            gpu_c: None,
            ir_c: None,
            protection: 0,
            cpu_level: fan::MAX_LEVEL,
            gpu_level: fan::MAX_LEVEL,
            ccd_pinned: 0,
            updated_unix_s: updated_unix_ms / 1000,
            updated_unix_ms,
        });
    } else if let Err(error) = &release {
        report(&FanServiceStatus {
            schema: 1,
            process_id: instance.id,
            process_started: instance.started,
            last_heartbeat_unix_ms,
            state: "handover_failed",
            detail: error.clone(),
            cpu_c: None,
            gpu_c: None,
            ir_c: None,
            protection: 0,
            cpu_level: 0,
            gpu_level: 0,
            ccd_pinned: 0,
            updated_unix_s: updated_unix_ms / 1000,
            updated_unix_ms,
        });
    } else if let Err(error) = &result {
        // The loop ended with an error after a safe release: record why, so a
        // stale "curve" status cannot outlive the process.
        report(&FanServiceStatus {
            schema: 1,
            process_id: instance.id,
            process_started: instance.started,
            last_heartbeat_unix_ms,
            state: "failed",
            detail: error.clone(),
            cpu_c: None,
            gpu_c: None,
            ir_c: None,
            protection: 0,
            cpu_level: 0,
            gpu_level: 0,
            ccd_pinned: 0,
            updated_unix_s: updated_unix_ms / 1000,
            updated_unix_ms,
        });
    }
    match (result, release) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(error)) => Err(format!("Safe fan release failed: {error}")),
        (Err(error), Err(release)) => Err(format!("{error}; safe fan release failed: {release}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleeping_or_cool_gpu_is_not_queried_every_cycle() {
        let age = Duration::from_secs;
        // Powered down: never ask, whatever was read before.
        assert_eq!(gpu_read(Some(false), None), GpuRead::Asleep);
        assert_eq!(gpu_read(Some(false), Some((80, age(1)))), GpuRead::Asleep);
        // Awake or unknown state without a reading: ask.
        for awake in [Some(true), None] {
            assert_eq!(gpu_read(awake, None), GpuRead::Query);
        }
        // Cool and fresh: rest on the reading until it expires.
        assert_eq!(gpu_read(Some(true), Some((55, age(10)))), GpuRead::Rest(55));
        assert_eq!(gpu_read(None, Some((59, age(29)))), GpuRead::Rest(59));
        assert_eq!(
            gpu_read(Some(true), Some((55, GPU_IDLE_POLL))),
            GpuRead::Query
        );
        // Warm: every cycle, as before.
        assert_eq!(
            gpu_read(Some(true), Some((GPU_IDLE_C, age(1)))),
            GpuRead::Query
        );
        assert_eq!(gpu_read(Some(true), Some((85, age(1)))), GpuRead::Query);
    }
    #[test]
    fn maximum_and_emergency_ignore_a_saved_quiet_curve() {
        let curve = curve_of(&[
            FanPoint {
                temperature_c: 30,
                duty_percent: 25,
            },
            FanPoint {
                temperature_c: 80,
                duty_percent: 70,
            },
        ])
        .unwrap();
        assert!(requested_level(FanMode::Curve, Some(&curve), false, Some(35)) < fan::MAX_LEVEL);
        assert_eq!(
            requested_level(FanMode::Max, Some(&curve), false, Some(35)),
            fan::MAX_LEVEL
        );
        assert_eq!(
            requested_level(FanMode::Curve, Some(&curve), true, Some(35)),
            fan::MAX_LEVEL
        );
        assert_eq!(
            requested_level(FanMode::Curve, None, false, None),
            fan::MAX_LEVEL
        );
    }

    #[test]
    fn curve_of_validates_and_interpolates() {
        let valid = vec![
            FanPoint {
                temperature_c: 40,
                duty_percent: 30,
            },
            FanPoint {
                temperature_c: 80,
                duty_percent: 90,
            },
        ];
        let curve = curve_of(&valid).unwrap();
        assert_eq!(curve.target(Some(60)), 60);

        let single = vec![FanPoint {
            temperature_c: 40,
            duty_percent: 30,
        }];
        assert!(curve_of(&single).is_err());

        let non_monotonic = vec![
            FanPoint {
                temperature_c: 80,
                duty_percent: 90,
            },
            FanPoint {
                temperature_c: 40,
                duty_percent: 30,
            },
        ];
        assert!(curve_of(&non_monotonic).is_err());
    }

    #[test]
    fn fan_service_status_serializes_with_schema() {
        let status = FanServiceStatus {
            schema: 1,
            process_id: 42,
            process_started: 1234,
            last_heartbeat_unix_ms: Some(1234567889000),
            state: "curve",
            detail: String::new(),
            cpu_c: Some(65.5),
            gpu_c: Some(58),
            ir_c: Some(42),
            protection: 0,
            cpu_level: 25,
            gpu_level: 25,
            ccd_pinned: 1,
            updated_unix_s: 1234567890,
            updated_unix_ms: 1234567890123,
        };
        let text = serde_json::to_string(&status).unwrap();
        assert!(text.contains(r#""schema":1"#));
        assert!(text.contains(r#""state":"curve""#));
        assert!(text.contains(r#""cpu_level":25"#));
        assert!(text.contains(r#""process_id":42"#));
        assert!(text.contains(r#""process_started":1234"#));
        assert!(text.contains(r#""last_heartbeat_unix_ms":1234567889000"#));
        assert!(text.contains(r#""updated_unix_ms":1234567890123"#));
    }

    #[test]
    fn write_contention_has_bounded_wait_and_honors_stop() {
        let mut calls = 0;
        let mut waited = Duration::ZERO;
        let access = acquire_write_access(
            CYCLE_LEASE_ATTEMPTS,
            || {
                calls += 1;
                Err::<(), _>(HARDWARE_BUSY.into())
            },
            |duration| {
                waited += duration;
                Ok(false)
            },
        )
        .unwrap();
        assert!(matches!(access, WriteAccess::Busy));
        assert_eq!(calls, CYCLE_LEASE_ATTEMPTS);
        assert_eq!(waited, Duration::from_millis(250));
        let access = acquire_write_access(
            CYCLE_LEASE_ATTEMPTS,
            || Err::<(), _>(HARDWARE_BUSY.into()),
            |_| Ok(true),
        )
        .unwrap();
        assert!(matches!(access, WriteAccess::Stop));
        assert!(
            acquire_write_access(
                CYCLE_LEASE_ATTEMPTS,
                || Err::<(), _>("access denied".into()),
                |_| panic!("permission failure must not retry")
            )
            .is_err()
        );
    }

    #[test]
    fn cooling_release_holds_lease_and_rechecks_vendor_before_writing() {
        use std::{cell::Cell, rc::Rc};
        struct Lease(Rc<Cell<bool>>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let held = Rc::new(Cell::new(false));
        let mut control = fan::SoftwareControl::default();
        assert_eq!(
            release_locked::<()>(
                &mut control,
                || panic!("idle must not acquire"),
                || panic!("idle must not query"),
                || panic!("idle must not write")
            )
            .unwrap(),
            CoolingRelease::NoWrite
        );
        control.arm();
        assert_eq!(
            release_locked(
                &mut control,
                || {
                    held.set(true);
                    Ok(Lease(held.clone()))
                },
                || {
                    assert!(held.get());
                    Ok(false)
                },
                || {
                    assert!(held.get());
                    Ok(())
                }
            )
            .unwrap(),
            CoolingRelease::Maximum
        );
        assert!(!held.get());
        control.arm();
        assert_eq!(
            release_locked(
                &mut control,
                || {
                    held.set(true);
                    Ok(Lease(held.clone()))
                },
                || {
                    assert!(held.get());
                    Ok(true)
                },
                || panic!("do not overwrite a newly active vendor")
            )
            .unwrap(),
            CoolingRelease::Vendor
        );
        assert!(!held.get());
        assert!(!control.handover_pending());
    }

    #[test]
    fn failed_lease_does_not_acknowledge_maximum_or_drop_responsibility() {
        let mut control = fan::SoftwareControl::default();
        control.arm();
        assert!(
            release_locked::<()>(
                &mut control,
                || Err("busy".into()),
                || panic!("no lease"),
                || panic!("no lease")
            )
            .is_err()
        );
        assert!(!control.handover_pending());
        assert_eq!(
            release_locked(&mut control, || Ok(()), || Ok(false), || Ok(())).unwrap(),
            CoolingRelease::Maximum
        );
        assert!(control.handover_pending());
    }

    #[test]
    fn entering_max_and_resuming_after_contention_require_a_fresh_heartbeat() {
        let recent = Some(Duration::from_secs(1));
        assert!(heartbeat_needed(
            Some(FanMode::Curve),
            FanMode::Max,
            recent,
            false
        ));
        assert!(heartbeat_needed(None, FanMode::Max, recent, false));
        assert!(!heartbeat_needed(
            Some(FanMode::Max),
            FanMode::Max,
            recent,
            false
        ));
        assert!(heartbeat_needed(
            Some(FanMode::Max),
            FanMode::Max,
            Some(HEARTBEAT),
            false
        ));
        assert!(heartbeat_needed(
            Some(FanMode::Curve),
            FanMode::Curve,
            recent,
            true
        ));
        assert!(heartbeat_needed(
            Some(FanMode::Max),
            FanMode::Max,
            None,
            false
        ));
    }

    #[test]
    fn wait_for_stop_or_gpu_pause_drops_gpu_handle_promptly_when_pause_signals() {
        // Warm GPU handle active: pause flips true on the 3rd check, drops `gpu`,
        // and finishes the remainder in one single wait call.
        let mut gpu = Some(42u32);
        let mut checks = 0usize;
        let mut steps = Vec::new();
        let stopped = wait_for_stop_or_gpu_pause_with_clock(
            &mut gpu,
            Duration::from_secs(2),
            || {
                checks += 1;
                checks >= 3
            },
            |step| {
                steps.push(step);
                Ok((false, step))
            },
        )
        .unwrap();
        assert!(!stopped);
        assert!(gpu.is_none());
        assert_eq!(
            steps,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(100),
                Duration::from_millis(1800),
            ]
        );

        // No open GPU handle: single wait call, zero pause polling.
        let mut none_gpu: Option<u32> = None;
        let mut single_steps = Vec::new();
        let stopped = wait_for_stop_or_gpu_pause_with_clock(
            &mut none_gpu,
            Duration::from_secs(2),
            || panic!("must not poll GPU pause when no GPU handle is open"),
            |step| {
                single_steps.push(step);
                Ok((false, step))
            },
        )
        .unwrap();
        assert!(!stopped);
        assert_eq!(single_steps, vec![Duration::from_secs(2)]);

        // Early wake (standby / live config update): exits immediately without
        // burning the remaining 1.9 s.
        let mut warm_gpu = Some(7u32);
        let mut wake_steps = Vec::new();
        let stopped = wait_for_stop_or_gpu_pause_with_clock(
            &mut warm_gpu,
            Duration::from_secs(2),
            || false,
            |step| {
                wake_steps.push(step);
                Ok((false, Duration::from_millis(1)))
            },
        )
        .unwrap();
        assert!(!stopped);
        assert_eq!(wake_steps, vec![Duration::from_millis(100)]);
        assert!(warm_gpu.is_some());
    }
}
