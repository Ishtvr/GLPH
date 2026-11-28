//! Read-only CPU helper. A same-user, local, PID-bound pipe carries fixed
//! prepare/sample/pause/stop requests; no profiles, register IDs or driver blobs
//! in IPC. Preparation has its own budget; samples never run inventory queries.
mod session;
use crate::native_ui::{token, wide};
use glph_core::CpuHardwareSample;
use glph_platform::CpuHardwareReader;
use serde::{Deserialize, Serialize};
use std::{
    io::Read as _,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    ptr,
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
    UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
};

// At most 64 clock rows plus eight bounded warning strings (including JSON
// escaping). This remains a local, PID/nonce/sequence-bound measurement frame.
const MAX_FRAME: usize = 32_768;
const MAX_LAUNCH_FRAME: usize = 4096;
pub(crate) const APPROVAL_FAILED: &str = "CPU_SENSOR_APPROVAL_FAILED";
const SAMPLE: u8 = 1;
const PAUSE: u8 = 2;
const STOP: u8 = 3;
const PREPARE: u8 = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
// Only preparation may run the isolated inventory child (bounded separately at
// five seconds). Never nest that full allowance inside a five-second sample.
const PREPARE_TIMEOUT: Duration = crate::read_process::READ_TIMEOUT.saturating_add(REQUEST_TIMEOUT);
/// Minimum interval between consecutive hardware counter reads (matches
/// `amd_package_watts` / `amd_effective_mhz`'s 200 ms floor plus margin) so
/// live polling-rate changes never trigger spurious `< 200 ms` baseline resets.
const MIN_HARDWARE_SAMPLE_INTERVAL: Duration = Duration::from_millis(215);

/// Classification of CPU sensor read failures:
/// - `Recoverable`: the elevated child & pipe are healthy (child paused its
///   device session); clearing `prepared` lets the next `sample` re-prepare
///   without killing/restarting the child.
/// - `Transport`: pipe disconnect, timeout, wedged state, or protocol error;
///   the bridge should be dropped and cleanly restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleError {
    Recoverable(String),
    Transport(String),
}
impl SampleError {
    pub fn into_detail(self) -> String {
        match self {
            Self::Recoverable(s) | Self::Transport(s) => s,
        }
    }
    #[cfg(test)]
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Recoverable(_))
    }
}

/// RAII guard cancelling pending overlapped pipe I/O before its stack
/// `OVERLAPPED` unwinds on any early `?` error path.
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
            // SAFETY: `self.pipe` and `self.ov` outlive this guard.
            unsafe {
                CancelIoEx(self.pipe, self.ov);
                let mut dummy = 0u32;
                let _ = GetOverlappedResult(self.pipe, self.ov, &mut dummy, 1);
            }
        }
    }
}

fn create_kill_job() -> Result<Handle, String> {
    // SAFETY: Private job object ensuring direct-spawned child exits with parent.
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
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchFrame {
    schema: u32,
    nonce: String,
    result: Result<u32, String>,
}
fn approved_pid(bytes: &[u8], nonce: &str) -> Result<u32, String> {
    if bytes.len() > MAX_LAUNCH_FRAME {
        return Err("CPU launch response exceeds its bounded frame".into());
    }
    let frame: LaunchFrame = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if frame.schema != 1 || frame.nonce != nonce {
        return Err("CPU launch returned a stale or unrelated response".into());
    }
    let pid = frame.result?;
    if pid == 0 {
        return Err("CPU reader process ID is missing".into());
    }
    Ok(pid)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    schema: u32,
    nonce: String,
    sequence: u64,
    result: Result<Option<CpuHardwareSample>, String>,
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn validate_nonce(nonce: &str) -> Result<(), String> {
    if nonce.is_empty() || nonce.len() > 64 || !nonce.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Invalid CPU session token".into());
    }
    Ok(())
}
fn name(nonce: &str) -> Vec<u16> {
    wide(&format!("\\\\.\\pipe\\GLPH.CpuRead.{nonce}"))
}

fn create_io_event() -> Result<Handle, String> {
    // SAFETY: Unnamed manual-reset event owned for overlapped pipe operations.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Handle(event))
}

fn wait_pipe_overlapped(
    pipe: HANDLE,
    ov: &mut OVERLAPPED,
    event: HANDLE,
    child: HANDLE,
    timeout: Duration,
    timeout_msg: &str,
    disconnect_msg: &str,
) -> Result<u32, String> {
    let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    let handles = [event, child];
    // SAFETY: Valid manual-reset I/O event and child process handle.
    let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) };
    if wait == WAIT_OBJECT_0 {
        let mut bytes = 0u32;
        if unsafe { GetOverlappedResult(pipe, ov, &mut bytes, 0) } == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_MORE_DATA {
                return Err("CPU reader response exceeds its bounded frame".into());
            }
            return Err(disconnect_msg.into());
        }
        return Ok(bytes);
    }
    unsafe {
        CancelIoEx(pipe, ov);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(pipe, ov, &mut dummy, 1);
    }
    if wait == WAIT_OBJECT_0 + 1 {
        Err(disconnect_msg.into())
    } else if wait == WAIT_TIMEOUT {
        Err(timeout_msg.into())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

pub struct CpuSensorBridge {
    pipe: Handle,
    _child: Handle,
    _job: Option<Handle>,
    io_event: Handle,
    nonce: String,
    sequence: u64,
    prepared: bool,
    pending: Option<(u64, u8)>,
    wedged: bool,
    last_sample_at: Option<Instant>,
    frame_buf: Vec<u8>,
}
impl CpuSensorBridge {
    pub fn start() -> Result<Self, String> {
        let nonce = token();
        validate_nonce(&nonce)?;
        let sid = glph_platform::current_user_sid()?;
        // No inherited default DACL: only this user and LocalSystem. The
        // expected child PID is checked separately after launch.
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
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                name(&nonce).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_FRAME as u32,
                MAX_FRAME as u32,
                100,
                &attributes,
            )
        };
        unsafe {
            LocalFree(descriptor);
        }
        if pipe == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let pipe = Handle(pipe);
        let io_event = create_io_event()?;

        // Arm overlapped ConnectNamedPipe before launching the helper so there
        // is zero race and zero 20 ms connect polling loop. The RAII guard
        // guarantees kernel I/O cancellation on every early `?` error path.
        let mut connect_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        connect_ov.hEvent = io_event.0;
        let connected = unsafe { ConnectNamedPipe(pipe.0, &mut connect_ov) };
        let connect_err = unsafe { GetLastError() };
        let connect_pending = if connected != 0 || connect_err == ERROR_PIPE_CONNECTED {
            false
        } else if connect_err == ERROR_IO_PENDING {
            true
        } else {
            return Err(std::io::Error::last_os_error().to_string());
        };
        let mut connect_guard = PendingOverlapped::new(pipe.0, &mut connect_ov, connect_pending);

        let (expected, child, job) = if crate::single_instance::elevated() {
            // Already elevated: spawn `cpu-sensor-child` directly inside a
            // kill-on-close job (zero UAC/ShellExecute overhead, instant silent
            // recovery after any crash/timeout).
            let job = create_kill_job()?;
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let spawned = Command::new(exe)
                .args(["cpu-sensor-child", &nonce, &std::process::id().to_string()])
                .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("CPU reader launch: {e}"))?;
            let expected = spawned.id();
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION
                        | PROCESS_SET_QUOTA
                        | PROCESS_TERMINATE
                        | SYNCHRONIZE,
                    0,
                    expected,
                )
            };
            if process.is_null() {
                return Err(format!(
                    "CPU reader process access: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let child = Handle(process);
            unsafe {
                let _ = AssignProcessToJobObject(job.0, child.0);
            }
            (expected, child, Some(job))
        } else {
            // ShellExecute loads COM/shell extensions. Keep that transient state
            // out of both persistent processes. This exact same-executable child
            // returns the approved PID through its private inherited stdout pipe.
            let mut launcher = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
                .args(["cpu-sensor-launch", &nonce, &std::process::id().to_string()])
                .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("CPU reader launch: {e}"))?;
            let mut response = Vec::new();
            launcher
                .stdout
                .take()
                .ok_or("CPU launch pipe missing")?
                .take((MAX_LAUNCH_FRAME + 1) as u64)
                .read_to_end(&mut response)
                .map_err(|e| e.to_string())?;
            let status = launcher.wait().map_err(|e| e.to_string())?;
            if !status.success() {
                return Err("CPU reader launch process failed".into());
            }
            let expected = approved_pid(&response, &nonce)?;
            let process = unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, expected)
            };
            if process.is_null() {
                return Err(format!(
                    "CPU reader process access: {}",
                    std::io::Error::last_os_error()
                ));
            }
            (expected, Handle(process), None)
        };
        if connect_pending {
            // `wait_pipe_overlapped` cancels `connect_ov` itself on timeout/abort.
            connect_guard.disarm();
            wait_pipe_overlapped(
                pipe.0,
                &mut connect_ov,
                io_event.0,
                child.0,
                Duration::from_secs(10),
                "CPU reader failed to connect within its deadline",
                "CPU reader failed to connect within its deadline",
            )?;
        }
        drop(connect_guard);
        let mut actual = 0;
        if unsafe { GetNamedPipeClientProcessId(pipe.0, &mut actual) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if actual != expected {
            return Err("CPU pipe peer differs from the approved process".into());
        }
        let mut bridge = Self {
            pipe,
            _child: child,
            _job: job,
            io_event,
            nonce,
            sequence: 0,
            prepared: false,
            pending: None,
            wedged: false,
            last_sample_at: None,
            frame_buf: vec![0u8; MAX_FRAME],
        };
        bridge.prepare().map_err(SampleError::into_detail)?;
        Ok(bridge)
    }
    fn request(&mut self, command: u8) -> Result<Option<CpuHardwareSample>, SampleError> {
        if self.wedged || self.pending.is_some() {
            return Err(SampleError::Transport(
                "CPU request still outstanding; pause must drain it first".into(),
            ));
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| SampleError::Transport("CPU sequence exhausted".into()))?;
        unsafe { ResetEvent(self.io_event.0) };
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.io_event.0;
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.pipe.0,
                (&command as *const u8).cast(),
                1,
                &mut written,
                &mut ov,
            )
        };
        let count = if ok != 0 {
            written
        } else if unsafe { GetLastError() } == ERROR_IO_PENDING {
            wait_pipe_overlapped(
                self.pipe.0,
                &mut ov,
                self.io_event.0,
                self._child.0,
                REQUEST_TIMEOUT,
                "CPU reader request pipe closed",
                "CPU reader request pipe closed",
            )
            .map_err(|e| {
                self.wedged = true;
                SampleError::Transport(e)
            })?
        } else {
            self.wedged = true;
            return Err(SampleError::Transport(
                "CPU reader request pipe closed".into(),
            ));
        };
        if count != 1 {
            self.wedged = true;
            return Err(SampleError::Transport(
                "CPU reader request pipe closed".into(),
            ));
        }
        self.pending = Some((self.sequence, command));
        self.receive(self.sequence, command)
    }
    fn receive(
        &mut self,
        sequence: u64,
        command: u8,
    ) -> Result<Option<CpuHardwareSample>, SampleError> {
        let timeout = if command == PREPARE {
            PREPARE_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let timeout_msg = if command == PREPARE {
            "CPU reader preparation timed out; no sample was accepted"
        } else {
            "CPU reader timed out; old sensor values were discarded"
        };
        unsafe { ResetEvent(self.io_event.0) };
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.io_event.0;
        let mut bytes = 0u32;
        let ok = unsafe {
            ReadFile(
                self.pipe.0,
                self.frame_buf.as_mut_ptr().cast(),
                self.frame_buf.len() as u32,
                &mut bytes,
                &mut ov,
            )
        };
        let count = if ok != 0 {
            bytes
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_MORE_DATA {
                self.wedged = true;
                return Err(SampleError::Transport(
                    "CPU reader response exceeds its bounded frame".into(),
                ));
            }
            if err != ERROR_IO_PENDING {
                self.wedged = true;
                return Err(SampleError::Transport("CPU reader disconnected".into()));
            }
            wait_pipe_overlapped(
                self.pipe.0,
                &mut ov,
                self.io_event.0,
                self._child.0,
                timeout,
                timeout_msg,
                "CPU reader disconnected before replying",
            )
            .map_err(|e| {
                self.wedged = true;
                SampleError::Transport(e)
            })?
        };
        self.pending = None;
        match parse_response_classified(
            &self.frame_buf[..count as usize],
            &self.nonce,
            sequence,
            command,
        ) {
            Ok(sample) => Ok(sample),
            Err(SampleError::Recoverable(detail)) => {
                // Child paused its device session on hardware error; pipe and
                // process remain healthy so the next sample can re-prepare.
                self.prepared = false;
                Err(SampleError::Recoverable(detail))
            }
            Err(SampleError::Transport(detail)) => {
                self.wedged = true;
                Err(SampleError::Transport(detail))
            }
        }
    }
    pub fn sample_classified(&mut self) -> Result<CpuHardwareSample, SampleError> {
        if self.wedged {
            return Err(SampleError::Transport(
                "CPU reader timed out; old sensor values were discarded".into(),
            ));
        }
        if !self.prepared {
            self.prepare()?;
            self.last_sample_at = None;
        } else if let Some(prev) = self.last_sample_at {
            // Live polling-rate changes may wake the worker shortly after a
            // previous sample; ensure >= 215 ms elapsed so RAPL / APERF deltas
            // never fail their 200 ms lower bound and flash blank values.
            let elapsed = prev.elapsed();
            if elapsed < MIN_HARDWARE_SAMPLE_INTERVAL {
                std::thread::sleep(MIN_HARDWARE_SAMPLE_INTERVAL - elapsed);
            }
        }
        let sample = self
            .request(SAMPLE)?
            .ok_or_else(|| SampleError::Transport("No CPU sample".into()))?;
        self.last_sample_at = Some(Instant::now());
        Ok(sample)
    }
    pub fn pause(&mut self) -> Result<(), String> {
        self.prepared = false;
        self.last_sample_at = None;
        if let Some((sequence, command)) = self.pending {
            // Windows may have frozen both peers during an in-flight sample.
            // Accept only its known nonce/sequence/operation, discard its data,
            // then send PAUSE. Never relabel that late sample as an ack.
            match self.receive(sequence, command) {
                Ok(_) | Err(SampleError::Recoverable(_)) => {
                    self.wedged = false;
                    Ok(())
                }
                Err(SampleError::Transport(e)) => Err(e),
            }?;
        }
        self.request(PAUSE)
            .map(|_| ())
            .map_err(SampleError::into_detail)
    }
    fn prepare(&mut self) -> Result<(), SampleError> {
        self.prepared = false;
        self.request(PREPARE)?;
        self.prepared = true;
        Ok(())
    }
    pub fn process_id(&self) -> u32 {
        unsafe { GetProcessId(self._child.0) }
    }
    #[cfg(test)]
    pub fn is_wedged(&self) -> bool {
        self.wedged
    }
}

fn parse_response_classified(
    bytes: &[u8],
    nonce: &str,
    sequence: u64,
    command: u8,
) -> Result<Option<CpuHardwareSample>, SampleError> {
    if bytes.len() > MAX_FRAME {
        return Err(SampleError::Transport(
            "CPU reader response exceeds its bounded frame".into(),
        ));
    }
    let frame: Frame =
        serde_json::from_slice(bytes).map_err(|e| SampleError::Transport(e.to_string()))?;
    if frame.schema != 1 || frame.nonce != nonce || frame.sequence != sequence {
        return Err(SampleError::Transport(
            "CPU reader returned a stale or unrelated response".into(),
        ));
    }
    let sample = frame.result.map_err(SampleError::Recoverable)?;
    if let Some(sample) = &sample {
        sample.validate().map_err(SampleError::Transport)?;
    }
    if !matches!(command, PREPARE | SAMPLE | PAUSE) || (command == SAMPLE) != sample.is_some() {
        return Err(SampleError::Transport(
            "CPU response does not match its fixed operation".into(),
        ));
    }
    Ok(sample)
}

#[cfg(test)]
fn response(
    bytes: &[u8],
    nonce: &str,
    sequence: u64,
    command: u8,
) -> Result<Option<CpuHardwareSample>, String> {
    parse_response_classified(bytes, nonce, sequence, command).map_err(SampleError::into_detail)
}

pub fn launch(nonce: &str, parent: u32) -> Result<(), String> {
    validate_nonce(nonce)?;
    if parent == 0 {
        return Err("Invalid CPU parent process".into());
    }
    let result = (|| {
        let file = wide(
            &std::env::current_exe()
                .map_err(|e| e.to_string())?
                .to_string_lossy(),
        );
        let params = wide(&format!("cpu-sensor-child {nonce} {parent}"));
        let verb = wide("runas");
        let mut launch: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        launch.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        launch.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        launch.lpFile = file.as_ptr();
        launch.lpParameters = params.as_ptr();
        launch.lpVerb = verb.as_ptr();
        launch.nShow = SW_HIDE;
        if unsafe { ShellExecuteExW(&mut launch) } == 0 {
            return Err(format!(
                "{APPROVAL_FAILED}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if launch.hProcess.is_null() {
            return Err("CPU reader process handle is missing".into());
        }
        let child = Handle(launch.hProcess);
        let pid = unsafe { GetProcessId(child.0) };
        if pid == 0 {
            return Err("CPU reader process ID is missing".into());
        }
        Ok(pid)
    })();
    println!(
        "{}",
        serde_json::to_string(&LaunchFrame {
            schema: 1,
            nonce: nonce.into(),
            result
        })
        .map_err(|e| e.to_string())?
    );
    Ok(())
}
impl Drop for CpuSensorBridge {
    fn drop(&mut self) {
        // Best effort stop, then close the peer. Never stop/unload the driver.
        let command = STOP;
        let mut bytes = 0;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.io_event.0;
        unsafe {
            ResetEvent(self.io_event.0);
            let ok = WriteFile(
                self.pipe.0,
                (&command as *const u8).cast(),
                1,
                &mut bytes,
                &mut ov,
            );
            if ok == 0 && GetLastError() == ERROR_IO_PENDING {
                if WaitForSingleObject(self.io_event.0, 100) != WAIT_OBJECT_0 {
                    CancelIoEx(self.pipe.0, &ov);
                }
                let _ = GetOverlappedResult(self.pipe.0, &ov, &mut bytes, 1);
            }
            DisconnectNamedPipe(self.pipe.0);
        }
    }
}

pub fn child(nonce: &str, parent: u32) -> Result<(), String> {
    validate_nonce(nonce)?;
    if parent == 0 {
        return Err("Invalid CPU parent process".into());
    }
    let file = unsafe {
        CreateFileW(
            name(nonce).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let pipe = Handle(file);
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server) } == 0 || server != parent {
        return Err("CPU reader server PID differs from the specified parent".into());
    }
    let mut session = session::ReaderSession::new();
    let mut sequence = 0u64;
    loop {
        let mut command = 0u8;
        let mut read = 0;
        if unsafe {
            ReadFile(
                pipe.0,
                (&mut command as *mut u8).cast(),
                1,
                &mut read,
                ptr::null_mut(),
            )
        } == 0
            || read != 1
        {
            break;
        }
        sequence = sequence.checked_add(1).ok_or("CPU sequence exhausted")?;
        let result = match command {
            PREPARE => session
                .prepare(crate::inventory::try_read, CpuHardwareReader::new)
                .map(|()| None),
            SAMPLE => session.sample(CpuHardwareReader::sample).map(Some),
            PAUSE => {
                session.pause();
                Ok(None)
            }
            STOP => break,
            _ => return Err("Unknown CPU sensor request; no hardware operation dispatched".into()),
        };
        if result.is_err() {
            session.pause();
        }
        let frame = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: nonce.into(),
            sequence,
            result,
        })
        .map_err(|e| e.to_string())?;
        if frame.len() > MAX_FRAME {
            return Err("CPU response exceeds its bound".into());
        }
        let mut written = 0;
        if unsafe {
            WriteFile(
                pipe.0,
                frame.as_ptr().cast(),
                frame.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        } == 0
            || written as usize != frame.len()
        {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_drains_only_the_exact_outstanding_sample_before_its_own_ack() {
        for stale in [false, true] {
            let nonce = token();
            let handle = unsafe {
                CreateNamedPipeW(
                    name(&nonce).as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    MAX_FRAME as u32,
                    MAX_FRAME as u32,
                    100,
                    ptr::null(),
                )
            };
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            let pipe = Handle(handle);
            let io_event = create_io_event().unwrap();
            let mut connect_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            connect_ov.hEvent = io_event.0;
            let connected = unsafe { ConnectNamedPipe(pipe.0, &mut connect_ov) };
            let connect_err = unsafe { GetLastError() };
            let connect_pending = if connected != 0 || connect_err == ERROR_PIPE_CONNECTED {
                false
            } else {
                assert_eq!(connect_err, ERROR_IO_PENDING);
                true
            };
            let peer_nonce = nonce.clone();
            // A local test peer exercises the real message pipe, never UAC,
            // inventory, a device, a driver, or a hardware sampling operation.
            let peer = std::thread::spawn(move || {
                let handle = unsafe {
                    CreateFileW(
                        name(&peer_nonce).as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        0,
                        ptr::null(),
                        OPEN_EXISTING,
                        0,
                        ptr::null_mut(),
                    )
                };
                assert_ne!(handle, INVALID_HANDLE_VALUE);
                let peer = Handle(handle);
                let mut command = 0u8;
                let mut count = 0;
                assert_ne!(
                    unsafe {
                        ReadFile(
                            peer.0,
                            (&mut command as *mut u8).cast(),
                            1,
                            &mut count,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
                assert_eq!(command, SAMPLE);
                std::thread::sleep(Duration::from_millis(20));
                let frame = Frame {
                    schema: 1,
                    nonce: if stale {
                        "unrelated".into()
                    } else {
                        peer_nonce.clone()
                    },
                    sequence: 1,
                    result: Ok(Some(CpuHardwareSample {
                        schema: 1,
                        source: glph_core::CpuHardwareSource::PawnioAmdRapl,
                        temperature_kind: glph_core::CpuTemperatureKind::AmdTctl,
                        package_power_w: None,
                        temperature_c: Some(50.0),
                        power_interval_ms: None,
                        effective_clocks: None,
                        warnings: vec![],
                    })),
                };
                let reply = serde_json::to_vec(&frame).unwrap();
                assert_ne!(
                    unsafe {
                        WriteFile(
                            peer.0,
                            reply.as_ptr().cast(),
                            reply.len() as u32,
                            &mut count,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
                if stale {
                    return;
                }
                assert_ne!(
                    unsafe {
                        ReadFile(
                            peer.0,
                            (&mut command as *mut u8).cast(),
                            1,
                            &mut count,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
                assert_eq!(command, PAUSE);
                let ack = serde_json::to_vec(&Frame {
                    schema: 1,
                    nonce: peer_nonce,
                    sequence: 2,
                    result: Ok(None),
                })
                .unwrap();
                assert_ne!(
                    unsafe {
                        WriteFile(
                            peer.0,
                            ack.as_ptr().cast(),
                            ack.len() as u32,
                            &mut count,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
                // Keep the writer alive until the parent consumes its ack.
                unsafe {
                    FlushFileBuffers(peer.0);
                }
            });
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                    0,
                    std::process::id(),
                )
            };
            assert!(!process.is_null());
            let child = Handle(process);
            if connect_pending {
                wait_pipe_overlapped(
                    pipe.0,
                    &mut connect_ov,
                    io_event.0,
                    child.0,
                    Duration::from_secs(2),
                    "connect timeout",
                    "connect disconnect",
                )
                .unwrap();
            }
            let mut bridge = CpuSensorBridge {
                pipe,
                _child: child,
                _job: None,
                io_event,
                nonce,
                sequence: 1,
                prepared: true,
                pending: Some((1, SAMPLE)),
                wedged: false,
                last_sample_at: None,
                frame_buf: vec![0u8; MAX_FRAME],
            };
            let mut written = 0;
            let command = SAMPLE;
            let mut write_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            write_ov.hEvent = bridge.io_event.0;
            unsafe { ResetEvent(bridge.io_event.0) };
            let ok = unsafe {
                WriteFile(
                    bridge.pipe.0,
                    (&command as *const u8).cast(),
                    1,
                    &mut written,
                    &mut write_ov,
                )
            };
            if ok == 0 {
                assert_eq!(unsafe { GetLastError() }, ERROR_IO_PENDING);
                wait_pipe_overlapped(
                    bridge.pipe.0,
                    &mut write_ov,
                    bridge.io_event.0,
                    bridge._child.0,
                    Duration::from_secs(2),
                    "write timeout",
                    "write disconnect",
                )
                .unwrap();
            }
            let result = bridge.pause();
            if stale {
                assert!(result.unwrap_err().contains("stale or unrelated"));
            } else {
                assert!(result.is_ok());
                assert_eq!(bridge.sequence, 2);
                assert!(bridge.pending.is_none());
            }
            assert!(!bridge.prepared);
            drop(bridge);
            peer.join().unwrap();
        }
    }

    #[test]
    fn preparation_acknowledgement_cannot_become_a_sample_or_cross_a_sequence() {
        let ack = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: "current".into(),
            sequence: 4,
            result: Ok(None),
        })
        .unwrap();
        assert!(response(&ack, "current", 4, PREPARE).unwrap().is_none());
        assert!(response(&ack, "current", 4, PAUSE).unwrap().is_none());
        assert!(response(&ack, "current", 4, SAMPLE).is_err());
        assert!(response(&ack, "old", 4, PREPARE).is_err());
        assert!(response(&ack, "current", 5, PREPARE).is_err());
        assert!(response(&ack, "current", 4, STOP).is_err());

        let sample = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: "current".into(),
            sequence: 5,
            result: Ok(Some(CpuHardwareSample {
                schema: 1,
                source: glph_core::CpuHardwareSource::PawnioAmdRapl,
                temperature_kind: glph_core::CpuTemperatureKind::AmdTctl,
                package_power_w: None,
                temperature_c: Some(50.0),
                power_interval_ms: None,
                effective_clocks: None,
                warnings: vec![],
            })),
        })
        .unwrap();
        assert!(response(&sample, "current", 5, SAMPLE).unwrap().is_some());
        assert!(response(&sample, "current", 5, PREPARE).is_err());
        assert!(response(&sample, "current", 5, PAUSE).is_err());
        assert!(response(&sample, "current", 4, SAMPLE).is_err());
    }

    #[test]
    fn preparation_budget_includes_both_bounded_phases_and_preserves_errors() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(5));
        assert!(PREPARE_TIMEOUT >= crate::read_process::READ_TIMEOUT + REQUEST_TIMEOUT);
        let error = "Isolated read exceeded the 5-second deadline";
        let bytes = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: "current".into(),
            sequence: 1,
            result: Err(error.into()),
        })
        .unwrap();
        assert_eq!(response(&bytes, "current", 1, PREPARE).unwrap_err(), error);
        assert!(response(&vec![b' '; MAX_FRAME + 1], "current", 1, PREPARE).is_err());
    }

    #[test]
    fn launch_reply_binds_approval_to_session_and_preserves_cancelled_uac() {
        let frame = |nonce: &str, result| {
            serde_json::to_vec(&LaunchFrame {
                schema: 1,
                nonce: nonce.into(),
                result,
            })
            .unwrap()
        };
        assert_eq!(approved_pid(&frame("session", Ok(42)), "session"), Ok(42));
        assert!(approved_pid(&frame("old", Ok(42)), "session").is_err());
        assert!(approved_pid(&frame("session", Ok(0)), "session").is_err());
        assert!(approved_pid(&vec![b' '; MAX_LAUNCH_FRAME + 1], "session").is_err());
        let cancelled = format!("{APPROVAL_FAILED}: OS error 1223");
        assert_eq!(
            approved_pid(&frame("session", Err(cancelled.clone())), "session"),
            Err(cancelled)
        );
        assert!(
            approved_pid(
                br#"{"schema":2,"nonce":"session","result":{"Ok":42}}"#,
                "session"
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_frame_carries_all_clocks_and_worst_case_escaped_warnings() {
        use glph_core::{CpuHardwareSource, CpuTemperatureKind, cpu::*};
        let sample = CpuHardwareSample {
            schema: 1,
            source: CpuHardwareSource::PawnioAmdRapl,
            temperature_kind: CpuTemperatureKind::AmdTctl,
            package_power_w: Some(0.1234567890123456),
            temperature_c: Some(83.375),
            power_interval_ms: Some(10_000),
            effective_clocks: Some(CpuEffectiveClocks {
                source: CpuClockSource::AmdAperfReadOnly,
                logical_processor_count: 64,
                processors: (0..64)
                    .map(|logical_processor| CpuLogicalClock {
                        logical_processor,
                        effective_mhz: 0.1234567890123456,
                        interval_ms: 10_000,
                    })
                    .collect(),
            }),
            warnings: vec!["\u{0001}".repeat(512); 8],
        };
        sample.validate().unwrap();
        let bytes = serde_json::to_vec(&Frame {
            schema: 1,
            nonce: "a".repeat(64),
            sequence: u64::MAX,
            result: Ok(Some(sample)),
        })
        .unwrap();
        assert!(
            bytes.len() < MAX_FRAME,
            "{} bytes exceeded bounded frame",
            bytes.len()
        );
        let decoded: Frame = serde_json::from_slice(&bytes).unwrap();
        decoded.result.unwrap().unwrap().validate().unwrap();
    }

    #[test]
    fn legacy_hardware_samples_remain_readable_without_effective_clocks() {
        let sample: CpuHardwareSample = serde_json::from_str(
            r#"{"schema":1,"source":"pawnio_amd_rapl","temperature_kind":"amd_tctl",
                "package_power_w":null,"temperature_c":50.0,"power_interval_ms":null,"warnings":[]}"#,
        ).unwrap();
        assert!(sample.effective_clocks.is_none());
        assert!(sample.validate().is_ok());
    }

    #[test]
    fn recoverable_hardware_error_reprepares_without_killing_bridge() {
        let nonce = token();
        let handle = unsafe {
            CreateNamedPipeW(
                name(&nonce).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_FRAME as u32,
                MAX_FRAME as u32,
                100,
                ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let pipe = Handle(handle);
        let io_event = create_io_event().unwrap();
        let mut connect_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        connect_ov.hEvent = io_event.0;
        let connected = unsafe { ConnectNamedPipe(pipe.0, &mut connect_ov) };
        let connect_err = unsafe { GetLastError() };
        let connect_pending = connected == 0 && connect_err == ERROR_IO_PENDING;

        let peer_nonce = nonce.clone();
        let peer = std::thread::spawn(move || {
            let file = unsafe {
                CreateFileW(
                    name(&peer_nonce).as_ptr(),
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
            let send = |seq: u64, result: Result<Option<CpuHardwareSample>, String>| {
                let bytes = serde_json::to_vec(&Frame {
                    schema: 1,
                    nonce: peer_nonce.clone(),
                    sequence: seq,
                    result,
                })
                .unwrap();
                let mut written = 0u32;
                assert_ne!(
                    unsafe {
                        WriteFile(
                            p.0,
                            bytes.as_ptr().cast(),
                            bytes.len() as u32,
                            &mut written,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
            };
            let recv_cmd = || -> u8 {
                let mut cmd = 0u8;
                let mut count = 0u32;
                assert_ne!(
                    unsafe {
                        ReadFile(
                            p.0,
                            (&mut cmd as *mut u8).cast(),
                            1,
                            &mut count,
                            ptr::null_mut(),
                        )
                    },
                    0
                );
                cmd
            };
            // 1. SAMPLE -> transient hardware error (child calls session.pause()).
            assert_eq!(recv_cmd(), SAMPLE);
            send(
                1,
                Err("CPU temperature reader could not acquire the shared PCI mutex".into()),
            );
            // 2. Parent must automatically issue PREPARE before the next SAMPLE!
            assert_eq!(recv_cmd(), PREPARE);
            send(2, Ok(None));
            // 3. SAMPLE -> succeeds!
            assert_eq!(recv_cmd(), SAMPLE);
            send(
                3,
                Ok(Some(CpuHardwareSample {
                    schema: 1,
                    source: glph_core::CpuHardwareSource::PawnioAmdRapl,
                    temperature_kind: glph_core::CpuTemperatureKind::AmdTctl,
                    package_power_w: Some(42.0),
                    temperature_c: Some(65.5),
                    power_interval_ms: Some(1000),
                    effective_clocks: None,
                    warnings: vec![],
                })),
            );
            unsafe { FlushFileBuffers(p.0) };
        });

        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                std::process::id(),
            )
        };
        assert!(!process.is_null());
        let child = Handle(process);
        if connect_pending {
            wait_pipe_overlapped(
                pipe.0,
                &mut connect_ov,
                io_event.0,
                child.0,
                Duration::from_secs(2),
                "connect timeout",
                "connect disconnect",
            )
            .unwrap();
        }
        let mut bridge = CpuSensorBridge {
            pipe,
            _child: child,
            _job: None,
            io_event,
            nonce,
            sequence: 0,
            prepared: true,
            pending: None,
            wedged: false,
            last_sample_at: None,
            frame_buf: vec![0u8; MAX_FRAME],
        };
        let err = bridge.sample_classified().unwrap_err();
        assert!(err.is_recoverable());
        assert!(!bridge.prepared);
        assert!(!bridge.is_wedged());
        let recovered = bridge.sample_classified().unwrap();
        assert_eq!(recovered.temperature_c, Some(65.5));
        assert_eq!(recovered.package_power_w, Some(42.0));
        assert!(bridge.prepared);
        drop(bridge);
        peer.join().unwrap();
    }

    #[test]
    fn disconnect_marks_bridge_wedged_for_clean_restart() {
        let nonce = token();
        let handle = unsafe {
            CreateNamedPipeW(
                name(&nonce).as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                MAX_FRAME as u32,
                MAX_FRAME as u32,
                100,
                ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let pipe = Handle(handle);
        let io_event = create_io_event().unwrap();
        let mut connect_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        connect_ov.hEvent = io_event.0;
        let connected = unsafe { ConnectNamedPipe(pipe.0, &mut connect_ov) };
        let connect_err = unsafe { GetLastError() };
        let connect_pending = connected == 0 && connect_err == ERROR_IO_PENDING;

        let peer_nonce = nonce.clone();
        let peer = std::thread::spawn(move || {
            let file = unsafe {
                CreateFileW(
                    name(&peer_nonce).as_ptr(),
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
            let mut cmd = 0u8;
            let mut count = 0u32;
            let _ = unsafe {
                ReadFile(
                    p.0,
                    (&mut cmd as *mut u8).cast(),
                    1,
                    &mut count,
                    ptr::null_mut(),
                )
            };
            // Drop pipe immediately without replying (simulates child crash).
        });

        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                std::process::id(),
            )
        };
        assert!(!process.is_null());
        let child = Handle(process);
        if connect_pending {
            wait_pipe_overlapped(
                pipe.0,
                &mut connect_ov,
                io_event.0,
                child.0,
                Duration::from_secs(2),
                "connect timeout",
                "connect disconnect",
            )
            .unwrap();
        }
        let mut bridge = CpuSensorBridge {
            pipe,
            _child: child,
            _job: None,
            io_event,
            nonce,
            sequence: 0,
            prepared: true,
            pending: None,
            wedged: false,
            last_sample_at: None,
            frame_buf: vec![0u8; MAX_FRAME],
        };
        let err = bridge.sample_classified().unwrap_err();
        assert!(!err.is_recoverable());
        assert!(bridge.is_wedged());
        // Subsequent sample fails fast as Transport instead of hanging.
        let again = bridge.sample_classified().unwrap_err();
        assert!(!again.is_recoverable());
        drop(bridge);
        peer.join().unwrap();
    }
}
