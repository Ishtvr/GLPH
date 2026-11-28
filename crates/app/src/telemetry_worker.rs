//! Persistent, crash-isolated GPU telemetry worker (`gpu-sensor-worker`).
//!
//! Keeps NVAPI/NVML out of the dashboard process (so an `nvml.dll` access
//! violation can never crash the UI or Session-0 cooling service) while
//! eliminating per-tick `CreateProcessW` + DLL load/unload micro-stutters.
//! Runs at `BELOW_NORMAL_PRIORITY_CLASS`, uses `FILE_FLAG_OVERLAPPED` pipe I/O
//! (zero polling wakeups), honors `Global\GLPH.GpuPause` to drop driver handles
//! before PnP restarts (without keeping the kernel event pinned alive), and
//! recovers automatically with short bounded backoff on timeout or worker crash.
use crate::{
    i18n::{t, tf},
    native_ui::{token, wide},
};
use glph_core::GpuSample;
use serde::{Deserialize, Serialize};
use std::{
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::*,
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        JobObjects::*,
        Pipes::*,
        Threading::*,
    },
};

const MAX_FRAME: usize = 16_384;
const CMD_SAMPLE: u8 = 1;
const CMD_PAUSE: u8 = 2;
const CMD_STOP: u8 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
/// Strictly greater than `CONNECT_TIMEOUT + REQUEST_TIMEOUT` so a cold worker
/// start + first NVML/NVAPI sample never spuriously times out and desyncs `rx`.
const SAMPLE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// If `Global\GLPH.GpuPause` stays signaled longer than this while no
/// foreground hardware lease is held, treat the pause as orphaned from a
/// crashed helper so telemetry does not remain blank forever. Must exceed a
/// full `vbios_tgp` Code-43 recovery cycle (~45 s) so telemetry never reopens
/// NVML/NVAPI handles mid-`CM_Disable_DevNode`.
const STUCK_GPU_PAUSE_CEILING: Duration = Duration::from_secs(65);
const GPU_PAUSE_EVENT_NAME: &str = "Global\\GLPH.GpuPause";

/// Live telemetry polling interval in milliseconds (updated without restart).
static LIVE_POLL_MS: AtomicU32 = AtomicU32::new(glph_core::app_settings::TELEMETRY_POLL_DEFAULT_MS);

pub(crate) fn set_live_poll_interval_ms(ms: u32) {
    if glph_core::app_settings::is_valid_telemetry_poll_ms(ms) {
        LIVE_POLL_MS.store(ms, Ordering::Relaxed);
    }
}

pub(crate) fn live_poll_interval_ms() -> u32 {
    LIVE_POLL_MS.load(Ordering::Relaxed)
}

/// Short bounded exponential backoff for telemetry worker / reader reconnects
/// (1s -> 2s -> 5s -> 10s max), replacing the old 30s blackout.
pub(crate) fn retry_backoff(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(5),
        _ => Duration::from_secs(10),
    }
}

/// Latest `(enforced_limit_mw, draw_mw, captured_at)` from the persistent GPU
/// worker so `preset_hold::keep_mode()` can read fresh values with zero extra
/// process spawns when telemetry is active.
static LAST_GPU_LIMIT: Mutex<Option<(u32, u32, Instant)>> = Mutex::new(None);

pub(crate) fn cached_gpu_limit(max_age: Duration) -> Option<(u32, u32)> {
    let guard = LAST_GPU_LIMIT.lock().ok()?;
    let (limit_mw, draw_mw, at) = (*guard)?;
    (at.elapsed() <= max_age).then_some((limit_mw, draw_mw))
}

fn record_cached_limit(sample: Option<&GpuSample>) {
    if let Some(s) = sample
        && let (Some(limit_w), Some(power_w)) = (s.enforced_limit_w, s.power_w)
        && limit_w.is_finite()
        && limit_w > 0.0
        && power_w.is_finite()
        && power_w >= 0.0
        && let Ok(mut guard) = LAST_GPU_LIMIT.lock()
    {
        *guard = Some((
            (limit_w * 1000.0).round() as u32,
            (power_w * 1000.0).round() as u32,
            Instant::now(),
        ));
    }
}

enum Request {
    Sample(u64),
    Stop,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Readout {
    sample: Option<GpuSample>,
    warnings: Vec<String>,
    lost: bool,
    #[serde(default)]
    transient_busy: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    schema: u32,
    nonce: String,
    sequence: u64,
    readout: Readout,
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: Owned Win32 handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// RAII guard cancelling a pending overlapped I/O before its stack `OVERLAPPED`
/// unwinds on any early `?` error return.
struct PendingOverlapped {
    pipe: HANDLE,
    ov: *mut OVERLAPPED,
    active: bool,
}
impl PendingOverlapped {
    fn new(pipe: HANDLE, ov: &mut OVERLAPPED, active: bool) -> Self {
        Self { pipe, ov, active }
    }
    fn disarm(&mut self) {
        self.active = false;
    }
}
impl Drop for PendingOverlapped {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: `self.pipe` and `self.ov` outlive this guard; cancel
            // pending kernel I/O and wait for completion before stack unwind.
            unsafe {
                CancelIoEx(self.pipe, self.ov);
                let mut dummy = 0u32;
                let _ = GetOverlappedResult(self.pipe, self.ov, &mut dummy, 1);
            }
        }
    }
}

fn pipe_name(nonce: &str) -> Vec<u16> {
    wide(&format!("\\\\.\\pipe\\GLPH.GpuSensor.{nonce}"))
}

fn validate_nonce(nonce: &str) -> Result<(), String> {
    if nonce.is_empty() || nonce.len() > 64 || !nonce.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Invalid GPU worker session token".into());
    }
    Ok(())
}

fn create_event() -> Result<Handle, String> {
    // SAFETY: Unnamed manual-reset event owned by caller.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Handle(event))
}

fn create_kill_job() -> Result<Handle, String> {
    // SAFETY: Private job object ensuring child termination if parent exits.
    unsafe {
        let job = CreateJobObjectW(ptr::null(), ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let job = Handle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(job)
    }
}

/// Completes a pending overlapped I/O operation on `pipe`, aborting early if
/// `abort_handle` (e.g. child exit handle) signals or `timeout` expires.
fn wait_overlapped(
    pipe: HANDLE,
    ov: &mut OVERLAPPED,
    event: HANDLE,
    abort_handle: Option<HANDLE>,
    timeout: Duration,
) -> Result<u32, String> {
    let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    let wait = unsafe {
        if let Some(abort) = abort_handle {
            let handles = [event, abort];
            WaitForMultipleObjects(2, handles.as_ptr(), 0, ms)
        } else {
            WaitForSingleObject(event, ms)
        }
    };
    if wait == WAIT_OBJECT_0 {
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(pipe, ov, &mut transferred, 0) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        return Ok(transferred);
    }
    // Cancel pending I/O and wait briefly for the kernel to release `ov`.
    unsafe {
        CancelIoEx(pipe, ov);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(pipe, ov, &mut dummy, 1);
    }
    if wait == WAIT_OBJECT_0 + 1 {
        Err("GPU sensor worker exited unexpectedly".into())
    } else if wait == WAIT_TIMEOUT {
        Err("GPU sensor worker timed out".into())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn overlapped_write(
    pipe: HANDLE,
    data: &[u8],
    event: HANDLE,
    abort_handle: Option<HANDLE>,
    timeout: Duration,
) -> Result<(), String> {
    unsafe { ResetEvent(event) };
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event;
    let mut written = 0u32;
    let ok = unsafe {
        WriteFile(
            pipe,
            data.as_ptr().cast(),
            data.len() as u32,
            &mut written,
            &mut ov,
        )
    };
    let count = if ok != 0 {
        written
    } else if unsafe { GetLastError() } == ERROR_IO_PENDING {
        wait_overlapped(pipe, &mut ov, event, abort_handle, timeout)?
    } else {
        return Err(std::io::Error::last_os_error().to_string());
    };
    if count as usize != data.len() {
        return Err("GPU worker short pipe write".into());
    }
    Ok(())
}

fn overlapped_read(
    pipe: HANDLE,
    buf: &mut [u8],
    event: HANDLE,
    abort_handle: Option<HANDLE>,
    timeout: Duration,
) -> Result<usize, String> {
    unsafe { ResetEvent(event) };
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = event;
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            pipe,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut read,
            &mut ov,
        )
    };
    let count = if ok != 0 {
        read
    } else if unsafe { GetLastError() } == ERROR_IO_PENDING {
        wait_overlapped(pipe, &mut ov, event, abort_handle, timeout)?
    } else {
        return Err(std::io::Error::last_os_error().to_string());
    };
    Ok(count as usize)
}

/// Legacy single-shot child entry (`gpu-telemetry-read`), kept for diagnostics.
pub(crate) fn read_in_child() -> Readout {
    if glph_platform::ControlLease::gpu_pause_active() {
        return Readout {
            sample: None,
            warnings: vec![],
            lost: false,
            transient_busy: true,
        };
    }
    let Ok(_lease) = glph_platform::ControlLease::acquire_background() else {
        return Readout {
            sample: None,
            warnings: vec![],
            lost: false,
            transient_busy: true,
        };
    };
    let mut nvidia = glph_platform::Nvidia::new();
    let sample = nvidia.sample();
    Readout {
        sample,
        warnings: nvidia.warnings().to_vec(),
        lost: nvidia.needs_reconnect(),
        transient_busy: false,
    }
}

fn open_gpu_pause_event() -> Option<Handle> {
    let name = wide(GPU_PAUSE_EVENT_NAME);
    // SAFETY: Fixed named manual-reset event; read-only synchronize handle.
    let h = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    (!h.is_null()).then_some(Handle(h))
}

/// Persistent out-of-process worker entry (`performance-hub.exe gpu-sensor-worker <nonce> <parent_pid>`).
/// Runs at `BELOW_NORMAL_PRIORITY_CLASS` + `THREAD_PRIORITY_BELOW_NORMAL`.
pub(crate) fn worker_child(nonce: &str, parent_pid: u32) -> Result<(), String> {
    validate_nonce(nonce)?;
    if parent_pid == 0 {
        return Err("Invalid GPU worker parent PID".into());
    }
    let file = unsafe {
        CreateFileW(
            pipe_name(nonce).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let pipe = Handle(file);
    let mut server_pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server_pid) } == 0
        || server_pid != parent_pid
    {
        return Err("GPU worker pipe server PID mismatch".into());
    }
    let mode = PIPE_READMODE_MESSAGE;
    if unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let io_event = create_event()?;
    let mut nvidia: Option<glph_platform::Nvidia> = None;
    let mut sequence = 0u64;
    // Only hold a synchronize handle to `Global\GLPH.GpuPause` while NVML/NVAPI
    // handles are open; dropping it whenever `nvidia` is None prevents this
    // worker from pinning an abandoned manual-reset event alive in the kernel.
    let mut pause_event: Option<Handle> = None;
    let mut pause_started: Option<Instant> = None;

    loop {
        if nvidia.is_none() {
            pause_event = None;
        }
        // Wait for the next 1-byte command via OVERLAPPED I/O. If we currently
        // hold open NVML/NVAPI handles, also wake immediately on `GpuPause` (or
        // check every 250 ms if the named event does not exist yet) so handles
        // are ALWAYS released before a VBIOS/PnP restart!
        unsafe { ResetEvent(io_event.0) };
        let mut cmd = [0u8; 1];
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = io_event.0;
        let mut read = 0u32;
        let ok = unsafe { ReadFile(pipe.0, cmd.as_mut_ptr().cast(), 1, &mut read, &mut ov) };
        if ok == 0 {
            if unsafe { GetLastError() } != ERROR_IO_PENDING {
                break;
            }
            let got_command = loop {
                if nvidia.is_some() && pause_event.is_none() {
                    pause_event = open_gpu_pause_event();
                } else if nvidia.is_none() {
                    pause_event = None;
                }
                let wait = if nvidia.is_some() {
                    if let Some(pe) = pause_event.as_ref() {
                        let handles = [io_event.0, pe.0];
                        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) }
                    } else {
                        unsafe { WaitForSingleObject(io_event.0, 100) }
                    }
                } else {
                    unsafe { WaitForSingleObject(io_event.0, INFINITE) }
                };
                if wait == WAIT_OBJECT_0 {
                    let mut transferred = 0u32;
                    if unsafe { GetOverlappedResult(pipe.0, &ov, &mut transferred, 0) } == 0
                        || transferred != 1
                    {
                        break false;
                    }
                    break true;
                }
                if wait == WAIT_OBJECT_0 + 1 || wait == WAIT_TIMEOUT {
                    if glph_platform::ControlLease::gpu_pause_active() {
                        nvidia = None;
                        pause_event = None;
                        pause_started.get_or_insert_with(Instant::now);
                    }
                    continue;
                }
                unsafe {
                    CancelIoEx(pipe.0, &ov);
                    let mut dummy = 0u32;
                    let _ = GetOverlappedResult(pipe.0, &ov, &mut dummy, 1);
                }
                break false;
            };
            if !got_command {
                break;
            }
        } else if read != 1 {
            break;
        }

        sequence = sequence.wrapping_add(1);
        let readout = match cmd[0] {
            CMD_STOP => break,
            CMD_PAUSE => {
                nvidia = None;
                pause_event = None;
                Readout {
                    sample: None,
                    warnings: vec![],
                    lost: false,
                    transient_busy: true,
                }
            }
            CMD_SAMPLE => {
                let pause_active = glph_platform::ControlLease::gpu_pause_active();
                let pause_stuck = if pause_active {
                    let since = *pause_started.get_or_insert_with(Instant::now);
                    since.elapsed() >= STUCK_GPU_PAUSE_CEILING
                } else {
                    pause_started = None;
                    false
                };
                if glph_platform::nvidia_gpu_awake() == Some(false) {
                    nvidia = None;
                    pause_event = None;
                    Readout {
                        sample: None,
                        warnings: vec![],
                        lost: false,
                        transient_busy: false,
                    }
                } else if pause_active && !pause_stuck {
                    nvidia = None;
                    pause_event = None;
                    Readout {
                        sample: None,
                        warnings: vec![],
                        lost: false,
                        transient_busy: true,
                    }
                } else if let Ok(_lease) = glph_platform::ControlLease::acquire_background() {
                    if pause_stuck {
                        // No foreground controller holds the hardware lease after
                        // STUCK_GPU_PAUSE_CEILING; recover from orphaned pause.
                        pause_started = None;
                    }
                    let session = nvidia.get_or_insert_with(glph_platform::Nvidia::new);
                    let sample = session.sample();
                    let warnings = session.warnings().to_vec();
                    let lost = session.needs_reconnect();
                    if lost {
                        nvidia = None;
                        pause_event = None;
                    }
                    Readout {
                        sample,
                        warnings,
                        lost,
                        transient_busy: false,
                    }
                } else {
                    // Foreground hardware transaction holds the lease; drop our
                    // handles so any PnP restart succeeds cleanly.
                    nvidia = None;
                    pause_event = None;
                    Readout {
                        sample: None,
                        warnings: vec![],
                        lost: false,
                        transient_busy: true,
                    }
                }
            }
            _ => break,
        };

        let frame = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: nonce.to_owned(),
            sequence,
            readout,
        })
        .map_err(|e| e.to_string())?;
        if frame.len() > MAX_FRAME {
            return Err("GPU worker response exceeds bounded frame".into());
        }
        if overlapped_write(pipe.0, &frame, io_event.0, None, REQUEST_TIMEOUT).is_err() {
            break;
        }
    }
    Ok(())
}

struct WorkerClient {
    pipe: Handle,
    child: Handle,
    _job: Handle,
    io_event: Handle,
    nonce: String,
    sequence: u64,
    buf: Vec<u8>,
}

impl WorkerClient {
    fn start() -> Result<Self, String> {
        let nonce = token();
        validate_nonce(&nonce)?;
        let sid = glph_platform::current_user_sid()?;
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name(&nonce).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_FRAME as u32,
                MAX_FRAME as u32,
                100,
                &attrs,
            )
        };
        unsafe {
            LocalFree(descriptor);
        }
        if pipe == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let pipe = Handle(pipe);
        let io_event = create_event()?;
        let job = create_kill_job()?;

        // Arm overlapped ConnectNamedPipe BEFORE spawning the child so the pipe
        // is already listening when the child calls CreateFileW. The RAII guard
        // guarantees kernel I/O cancellation on every early `?` error path.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = io_event.0;
        let connected_now = unsafe { ConnectNamedPipe(pipe.0, &mut ov) };
        let connect_err = unsafe { GetLastError() };
        let pending_connect = if connected_now != 0 || connect_err == ERROR_PIPE_CONNECTED {
            false
        } else if connect_err == ERROR_IO_PENDING {
            true
        } else {
            return Err(std::io::Error::last_os_error().to_string());
        };
        let mut connect_guard = PendingOverlapped::new(pipe.0, &mut ov, pending_connect);

        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let child = Command::new(exe)
            .args(["gpu-sensor-worker", &nonce, &std::process::id().to_string()])
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("GPU worker launch: {e}"))?;
        let child_pid = child.id();
        let child_handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION
                    | PROCESS_SET_QUOTA
                    | PROCESS_TERMINATE
                    | SYNCHRONIZE,
                0,
                child_pid,
            )
        };
        if child_handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let child_handle = Handle(child_handle);
        if unsafe { AssignProcessToJobObject(job.0, child_handle.0) } == 0 {
            // Continue even if nested job assignment fails on restricted hosts;
            // explicit Stop + Drop still terminates cleanly.
        }

        if pending_connect {
            // `wait_overlapped` cancels `ov` itself on timeout/abort.
            connect_guard.disarm();
            wait_overlapped(
                pipe.0,
                &mut ov,
                io_event.0,
                Some(child_handle.0),
                CONNECT_TIMEOUT,
            )?;
        }
        drop(connect_guard);
        let mut peer_pid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut peer_pid) } == 0
            || peer_pid != child_pid
        {
            return Err("GPU worker pipe client PID mismatch".into());
        }
        Ok(Self {
            pipe,
            child: child_handle,
            _job: job,
            io_event,
            nonce,
            sequence: 0,
            buf: vec![0u8; MAX_FRAME],
        })
    }

    fn request(&mut self, cmd: u8) -> Result<Readout, String> {
        self.sequence = self.sequence.wrapping_add(1);
        overlapped_write(
            self.pipe.0,
            &[cmd],
            self.io_event.0,
            Some(self.child.0),
            REQUEST_TIMEOUT,
        )?;
        let len = overlapped_read(
            self.pipe.0,
            &mut self.buf,
            self.io_event.0,
            Some(self.child.0),
            REQUEST_TIMEOUT,
        )?;
        let frame: Frame = serde_json::from_slice(&self.buf[..len])
            .map_err(|e| format!("GPU worker frame: {e}"))?;
        if frame.schema != 1 || frame.nonce != self.nonce || frame.sequence != self.sequence {
            return Err("GPU worker returned stale or unrelated frame".into());
        }
        Ok(frame.readout)
    }
}

impl Drop for WorkerClient {
    fn drop(&mut self) {
        let _ = overlapped_write(
            self.pipe.0,
            &[CMD_STOP],
            self.io_event.0,
            Some(self.child.0),
            Duration::from_millis(150),
        );
        unsafe {
            DisconnectNamedPipe(self.pipe.0);
        }
    }
}

pub struct GpuTelemetry {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<(u64, Readout)>,
    thread: Option<thread::JoinHandle<()>>,
    next_id: u64,
    warnings: Vec<String>,
    lost: bool,
    transient_busy: bool,
}

impl GpuTelemetry {
    pub fn new() -> Self {
        let (tx, requests) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            crate::runtime_priority::configure_background_thread();
            let mut client: Option<WorkerClient> = None;
            let mut reopen_after: Option<Instant> = None;
            let mut consecutive_failures = 0u32;
            let mut pause_started: Option<Instant> = None;

            while let Ok(Request::Sample(req_id)) = requests.recv() {
                let pause_active = glph_platform::ControlLease::gpu_pause_active();
                let pause_stuck = if pause_active {
                    let since = *pause_started.get_or_insert_with(Instant::now);
                    since.elapsed() >= STUCK_GPU_PAUSE_CEILING
                } else {
                    pause_started = None;
                    false
                };
                if pause_active && !pause_stuck {
                    if let Some(worker) = client.as_mut()
                        && worker.request(CMD_PAUSE).is_err()
                    {
                        client = None;
                    }
                    if results
                        .send((
                            req_id,
                            Readout {
                                sample: None,
                                warnings: vec![],
                                lost: false,
                                transient_busy: true,
                            },
                        ))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }

                if client.is_none()
                    && let Some(until) = reopen_after
                    && Instant::now() < until
                {
                    if results
                        .send((
                            req_id,
                            Readout {
                                sample: None,
                                warnings: vec![t("warning.nvidia_lost").into()],
                                lost: false,
                                transient_busy: false,
                            },
                        ))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }

                let readout = (|| -> Result<Readout, String> {
                    if client.is_none() {
                        client = Some(WorkerClient::start()?);
                        reopen_after = None;
                    }
                    client.as_mut().expect("started above").request(CMD_SAMPLE)
                })()
                .unwrap_or_else(|error| {
                    client = None;
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    reopen_after = Some(Instant::now() + retry_backoff(consecutive_failures));
                    Readout {
                        sample: None,
                        warnings: vec![tf("warning.gpu_reader", &[&error])],
                        lost: true,
                        transient_busy: false,
                    }
                });

                record_cached_limit(readout.sample.as_ref());
                if readout.sample.is_some() {
                    consecutive_failures = 0;
                    reopen_after = None;
                } else if readout.lost {
                    client = None;
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    reopen_after = Some(Instant::now() + retry_backoff(consecutive_failures));
                }
                if results.send((req_id, readout)).is_err() {
                    break;
                }
            }
            drop(client);
        });
        Self {
            tx,
            rx,
            thread: Some(thread),
            next_id: 0,
            warnings: vec![],
            lost: false,
            transient_busy: false,
        }
    }

    pub fn sample(&mut self) -> Option<GpuSample> {
        self.next_id = self.next_id.wrapping_add(1);
        let expected_id = self.next_id;
        // Drain any stale replies from a prior timed-out request so responses
        // can never drift out of phase with requests.
        while self.rx.try_recv().is_ok() {}
        if self.tx.send(Request::Sample(expected_id)).is_err() {
            self.lost = true;
            self.transient_busy = false;
            self.warnings = vec![t("warning.nvidia_ended").into()];
            return None;
        }
        let deadline = Instant::now() + SAMPLE_WAIT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok((id, readout)) if id == expected_id => {
                    self.warnings = readout.warnings;
                    self.lost = readout.lost;
                    self.transient_busy = readout.transient_busy;
                    return readout.sample;
                }
                Ok(_) => continue,
                Err(_) => {
                    self.lost = true;
                    self.transient_busy = false;
                    self.warnings = vec![t("warning.nvidia_timeout").into()];
                    return None;
                }
            }
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn needs_reconnect(&self) -> bool {
        self.lost
    }

    pub fn last_was_transient_busy(&self) -> bool {
        self.transient_busy
    }
}

impl Drop for GpuTelemetry {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            for _ in 0..20 {
                if thread.is_finished() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_escalates_boundedly_without_thirty_second_blackouts() {
        assert_eq!(retry_backoff(0), Duration::ZERO);
        assert_eq!(retry_backoff(1), Duration::from_secs(1));
        assert_eq!(retry_backoff(2), Duration::from_secs(2));
        assert_eq!(retry_backoff(3), Duration::from_secs(5));
        assert_eq!(retry_backoff(4), Duration::from_secs(10));
        assert_eq!(retry_backoff(50), Duration::from_secs(10));
    }

    #[test]
    fn live_poll_interval_accepts_only_validated_discrete_options() {
        let original = live_poll_interval_ms();
        set_live_poll_interval_ms(500);
        assert_eq!(live_poll_interval_ms(), 500);
        set_live_poll_interval_ms(777); // invalid -> ignored
        assert_eq!(live_poll_interval_ms(), 500);
        set_live_poll_interval_ms(5000);
        assert_eq!(live_poll_interval_ms(), 5000);
        set_live_poll_interval_ms(original);
    }

    #[test]
    fn sample_channel_drains_stale_replies_and_recovers_after_timeout_or_exit() {
        let (tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, rx) = mpsc::channel::<(u64, Readout)>();
        let handle = thread::spawn(move || {
            while let Ok(Request::Sample(id)) = req_rx.recv() {
                // Inject a stale reply with id-1 before the matching reply.
                if id == 1 {
                    let _ = res_tx.send((
                        0,
                        Readout {
                            sample: Some(GpuSample {
                                name: "STALE".into(),
                                temperature_c: Some(99),
                                ..Default::default()
                            }),
                            warnings: vec![],
                            lost: false,
                            transient_busy: false,
                        },
                    ));
                }
                let sample = GpuSample {
                    name: "RTX 5070".into(),
                    temperature_c: Some(61),
                    clock_mhz: Some(2100),
                    power_w: Some(75.0),
                    utilization_percent: Some(68),
                    ..Default::default()
                };
                if res_tx
                    .send((
                        id,
                        Readout {
                            sample: Some(sample),
                            warnings: vec![],
                            lost: false,
                            transient_busy: false,
                        },
                    ))
                    .is_err()
                {
                    break;
                }
                if id == 2 {
                    // Simulate worker thread exiting unexpectedly after reply #2.
                    break;
                }
            }
        });
        let mut telemetry = GpuTelemetry {
            tx,
            rx,
            thread: Some(handle),
            next_id: 0,
            warnings: vec![],
            lost: false,
            transient_busy: false,
        };
        let first = telemetry.sample().expect("first matching sample");
        assert_eq!(first.name, "RTX 5070");
        assert_eq!(first.utilization_percent, Some(68));
        assert!(!telemetry.needs_reconnect());
        let second = telemetry.sample().expect("second matching sample");
        assert_eq!(second.temperature_c, Some(61));
        // Wait briefly for mock thread to exit, then verify crash is detected cleanly.
        thread::sleep(Duration::from_millis(20));
        assert!(telemetry.sample().is_none());
        assert!(telemetry.needs_reconnect());
    }

    #[test]
    fn worker_client_rejects_mismatched_sequence_and_handles_clean_frame() {
        let nonce = token();
        let pipe_handle = unsafe {
            CreateNamedPipeW(
                pipe_name(&nonce).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_FRAME as u32,
                MAX_FRAME as u32,
                100,
                ptr::null(),
            )
        };
        assert_ne!(pipe_handle, INVALID_HANDLE_VALUE);
        let pipe = Handle(pipe_handle);
        let io_event = create_event().unwrap();
        let job = create_kill_job().unwrap();
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = io_event.0;
        let connected = unsafe { ConnectNamedPipe(pipe.0, &mut ov) };
        let err = unsafe { GetLastError() };
        let pending = connected == 0 && err == ERROR_IO_PENDING;
        let mut guard = PendingOverlapped::new(pipe.0, &mut ov, pending);

        let peer_nonce = nonce.clone();
        let peer = thread::spawn(move || {
            let file = unsafe {
                CreateFileW(
                    pipe_name(&peer_nonce).as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    0,
                    ptr::null_mut(),
                )
            };
            assert_ne!(file, INVALID_HANDLE_VALUE);
            let p = Handle(file);
            let mut cmd = [0u8; 1];
            let mut count = 0u32;
            assert_ne!(
                unsafe { ReadFile(p.0, cmd.as_mut_ptr().cast(), 1, &mut count, ptr::null_mut()) },
                0
            );
            assert_eq!(cmd[0], CMD_SAMPLE);
            let frame = serde_json::to_vec(&Frame {
                schema: 1,
                nonce: peer_nonce.clone(),
                sequence: 1,
                readout: Readout {
                    sample: Some(GpuSample {
                        name: "Mock GPU".into(),
                        temperature_c: Some(58),
                        utilization_percent: Some(42),
                        ..Default::default()
                    }),
                    warnings: vec![],
                    lost: false,
                    transient_busy: false,
                },
            })
            .unwrap();
            assert_ne!(
                unsafe {
                    WriteFile(
                        p.0,
                        frame.as_ptr().cast(),
                        frame.len() as u32,
                        &mut count,
                        ptr::null_mut(),
                    )
                },
                0
            );
            // Second request: return stale sequence 99 -> client must reject.
            assert_ne!(
                unsafe { ReadFile(p.0, cmd.as_mut_ptr().cast(), 1, &mut count, ptr::null_mut()) },
                0
            );
            let bad = serde_json::to_vec(&Frame {
                schema: 1,
                nonce: peer_nonce,
                sequence: 99,
                readout: Readout {
                    sample: None,
                    warnings: vec![],
                    lost: false,
                    transient_busy: false,
                },
            })
            .unwrap();
            assert_ne!(
                unsafe {
                    WriteFile(
                        p.0,
                        bad.as_ptr().cast(),
                        bad.len() as u32,
                        &mut count,
                        ptr::null_mut(),
                    )
                },
                0
            );
            unsafe { FlushFileBuffers(p.0) };
        });

        let self_proc = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                std::process::id(),
            )
        };
        assert!(!self_proc.is_null());
        let child = Handle(self_proc);
        if pending {
            guard.disarm();
            wait_overlapped(
                pipe.0,
                &mut ov,
                io_event.0,
                Some(child.0),
                Duration::from_secs(2),
            )
            .unwrap();
        }
        drop(guard);

        let mut client = WorkerClient {
            pipe,
            child,
            _job: job,
            io_event,
            nonce,
            sequence: 0,
            buf: vec![0u8; MAX_FRAME],
        };
        let ok = client.request(CMD_SAMPLE).unwrap();
        assert_eq!(ok.sample.as_ref().unwrap().utilization_percent, Some(42));
        let stale = client.request(CMD_SAMPLE);
        assert!(stale.is_err());
        drop(client);
        peer.join().unwrap();
    }
}
