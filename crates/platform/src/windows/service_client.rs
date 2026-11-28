//! Client, read-only SCM inspection, OVERLAPPED Named Pipe server helper, and
//! machine-wide ProgramData storage for `GLPHService` (`glph-service.exe`).
use super::startup;
use glph_core::service::{
    MAX_IPC_FRAME_BYTES, SERVICE_NAME, SERVICE_PIPE_NAME, ServiceConfig, ServiceRequest,
    ServiceResponse, ServiceStatus, decode_ipc, encode_ipc,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        MoveFileExW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT, SetNamedPipeHandleState,
            WaitNamedPipeW,
        },
        Services::*,
        Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects, WaitForSingleObject},
    },
    UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath},
};

const MAX_CONFIG_BYTES: u64 = 65_536;
const PIPE_CONNECT_TIMEOUT_MS: u32 = 1_500;
const PIPE_IO_TIMEOUT_MS: u32 = 2_000;
const PIPE_DRAIN_TIMEOUT_MS: u32 = 250;
const SERVICE_CONFIG_FILE: &str = "service-config.json";
const SERVICE_RECEIPT_FILE: &str = "service-receipt.json";
const SERVICE_EXE_NAME: &str = "glph-service.exe";
/// Single-instance named pipe: prevents unprivileged processes from creating
/// additional pipe instances (named-pipe squatting / MITM).
const MAX_PIPE_INSTANCES: u32 = 1;
/// SYSTEM + Administrators get full control; local Interactive Users (IU) get
/// FILE_GENERIC_READ (0x00120089) | FILE_WRITE_DATA (0x02) | FILE_WRITE_ATTRIBUTES (0x100)
/// = 0x0012018B — intentionally omitting FILE_CREATE_PIPE_INSTANCE (0x04).
const SERVICE_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012018B;;;IU)";
const CLIENT_PIPE_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES;

static CONFIG_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

struct ScHandle(SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Handle obtained from OpenSCManagerW / OpenServiceW.
            unsafe {
                CloseServiceHandle(self.0);
            }
        }
    }
}

struct WinHandle(HANDLE);
impl Drop for WinHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: Handle obtained from CreateFileW / CreateEventW / CreateNamedPipeW.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    Paused,
    Unknown(u32),
}

impl ScmServiceState {
    pub fn from_win32(code: u32) -> Self {
        match code {
            SERVICE_STOPPED => Self::Stopped,
            SERVICE_START_PENDING => Self::StartPending,
            SERVICE_STOP_PENDING => Self::StopPending,
            SERVICE_RUNNING => Self::Running,
            SERVICE_PAUSED | SERVICE_PAUSE_PENDING | SERVICE_CONTINUE_PENDING => Self::Paused,
            other => Self::Unknown(other),
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::StartPending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledServiceInfo {
    pub state: ScmServiceState,
    pub start_type: Option<u32>,
    pub binary_path: Option<String>,
    pub pid: Option<u32>,
    pub receipt_matches_sibling: bool,
}

impl Default for InstalledServiceInfo {
    fn default() -> Self {
        Self {
            state: ScmServiceState::NotInstalled,
            start_type: None,
            binary_path: None,
            pid: None,
            receipt_matches_sibling: false,
        }
    }
}

/// Fast, zero-disk-I/O check whether `PerformanceHubService` is currently
/// running or starting. Safe to call on periodic telemetry/watchdog ticks
/// (uses only `QueryServiceStatusEx`, never hashes the executable on disk).
pub fn is_service_running_fast() -> bool {
    // SAFETY: Public read-only SCM connect + status query on our fixed service name.
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return false;
    }
    let manager = ScHandle(manager);
    let service =
        unsafe { OpenServiceW(manager.0, wide(SERVICE_NAME).as_ptr(), SERVICE_QUERY_STATUS) };
    if service.is_null() {
        return false;
    }
    let service = ScHandle(service);
    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0u32;
    let ok = unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    ok != 0 && ScmServiceState::from_win32(status.dwCurrentState).is_running()
}

/// Read-only inspection of `PerformanceHubService` via the Windows Service
/// Control Manager. Requires NO administrator privileges (`SC_MANAGER_CONNECT`).
pub fn query_service_info() -> Result<InstalledServiceInfo, String> {
    // SAFETY: Public read-only SCM connect.
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(format!(
            "Service Control Manager unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let manager = ScHandle(manager);
    // SAFETY: Fixed read-only query on our own service name.
    let service = unsafe {
        OpenServiceW(
            manager.0,
            wide(SERVICE_NAME).as_ptr(),
            SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
        )
    };
    if service.is_null() {
        let code = unsafe { GetLastError() };
        if code == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(InstalledServiceInfo::default());
        }
        return Err(format!(
            "Unable to inspect {SERVICE_NAME}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let service = ScHandle(service);

    let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0u32;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(format!(
            "Service status query failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut start_type = None;
    let mut binary_path = None;
    let mut config_buf = [0usize; 8192 / size_of::<usize>()];
    if unsafe {
        QueryServiceConfigW(
            service.0,
            config_buf.as_mut_ptr().cast(),
            size_of_val(&config_buf) as u32,
            &mut needed,
        )
    } != 0
    {
        let cfg = unsafe { &*config_buf.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
        start_type = Some(cfg.dwStartType);
        let start = config_buf.as_ptr() as usize;
        let end = start + size_of_val(&config_buf);
        let addr = cfg.lpBinaryPathName as usize;
        if addr >= start && addr < end && addr % align_of::<u16>() == 0 {
            let cells =
                unsafe { std::slice::from_raw_parts(cfg.lpBinaryPathName, (end - addr) / 2) };
            if let Some(len) = cells.iter().position(|c| *c == 0) {
                binary_path = String::from_utf16(&cells[..len]).ok();
            }
        }
    }

    Ok(InstalledServiceInfo {
        state: ScmServiceState::from_win32(status.dwCurrentState),
        start_type,
        binary_path,
        pid: (status.dwProcessId != 0).then_some(status.dwProcessId),
        receipt_matches_sibling: receipt_matches_sibling_binary(),
    })
}

/// Checks whether the installed service binary hash matches the sibling
/// `performance-hub-service.exe` next to the current `performance-hub.exe`.
pub fn receipt_matches_sibling_binary() -> bool {
    (|| -> Result<bool, String> {
        let root = startup::installation_root()?;
        let mut bytes = Vec::new();
        File::open(root.join(SERVICE_RECEIPT_FILE))
            .map_err(|e| e.to_string())?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Ok(false);
        }
        let receipt: serde_json::Value =
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
                .map_err(|e| e.to_string())?;
        let sibling = sibling_service_binary()?;
        let expected = startup::file_hash(&sibling)?;
        Ok(receipt["schema"] == 1
            && receipt["sha256"]
                .as_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&expected)))
    })()
    .unwrap_or(false)
}

/// Locates `performance-hub-service.exe` next to the currently running executable.
pub fn sibling_service_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe
        .parent()
        .ok_or("Executable parent directory is unavailable")?;
    let candidate = dir.join(SERVICE_EXE_NAME);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "Sibling service binary not found at {}",
            candidate.display()
        ))
    }
}

/// Resolves the machine-wide `C:\ProgramData\GLPH` directory via the
/// fixed `FOLDERID_ProgramData` Known Folder (safe under both interactive users
/// and Session-0 `LocalSystem`).
pub fn program_data_root() -> Result<PathBuf, String> {
    let mut directory = ptr::null_mut();
    // SAFETY: Fixed machine Known Folder ID.
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, ptr::null_mut(), &mut directory) };
    if result < 0 || directory.is_null() {
        return Err("ProgramData Known Folder is unavailable".into());
    }
    let path = (|| {
        let mut count = 0;
        unsafe {
            while count < 32768 && *directory.add(count) != 0 {
                count += 1;
            }
        }
        if count == 32768 {
            return Err("ProgramData path exceeds its bound".into());
        }
        let text = String::from_utf16(unsafe { std::slice::from_raw_parts(directory, count) })
            .map_err(|e| e.to_string())?;
        Ok(PathBuf::from(text).join("GLPH"))
    })();
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(directory.cast()));
    }
    path
}

fn reject_reparse_point(path: &Path) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(format!(
                "Refusing redirected path (reparse point): {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub fn load_service_config_from(dir: &Path) -> Result<ServiceConfig, String> {
    reject_reparse_point(dir)?;
    let path = dir.join(SERVICE_CONFIG_FILE);
    reject_reparse_point(&path)?;
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServiceConfig::default());
        }
        Err(err) => return Err(err.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("Service configuration exceeds maximum size".into());
    }
    let config: ServiceConfig =
        serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
            .map_err(|e| e.to_string())?;
    config.validate()?;
    Ok(config)
}

pub fn save_service_config_to(dir: &Path, config: &ServiceConfig) -> Result<(), String> {
    config.validate()?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    reject_reparse_point(dir)?;
    let target = dir.join(SERVICE_CONFIG_FILE);
    reject_reparse_point(&target)?;
    let tmp = dir.join(format!(
        ".{SERVICE_CONFIG_FILE}-{}-{}.tmp",
        std::process::id(),
        CONFIG_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    reject_reparse_point(&tmp)?;
    let payload = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    let write_res = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        file.write_all(&payload).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        let src: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let dst: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: Same-directory atomic replacement with write-through flush.
        if unsafe {
            MoveFileExW(
                src.as_ptr(),
                dst.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

pub fn load_service_config() -> Result<ServiceConfig, String> {
    load_service_config_from(&program_data_root()?)
}

pub fn save_service_config(config: &ServiceConfig) -> Result<(), String> {
    save_service_config_to(&program_data_root()?, config)
}

/// Waits on an overlapped I/O event (and optional stop event) with a hard
/// timeout, canceling any in-flight I/O via `CancelIoEx` before returning so
/// stack-resident `OVERLAPPED` / buffer references are never left dangling.
fn finish_overlapped(
    handle: HANDLE,
    overlapped: &mut OVERLAPPED,
    io_event: HANDLE,
    stop_event: Option<HANDLE>,
    timeout_ms: u32,
) -> Result<u32, u32> {
    let wait = if let Some(stop) = stop_event {
        let handles = [io_event, stop];
        // SAFETY: Both handles are valid event handles.
        unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, timeout_ms) }
    } else {
        // SAFETY: Valid manual-reset event handle.
        unsafe { WaitForSingleObject(io_event, timeout_ms) }
    };
    if wait == WAIT_OBJECT_0 {
        let mut transferred = 0u32;
        // SAFETY: Overlapped operation completed; retrieve byte count / status.
        if unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) } != 0 {
            return Ok(transferred);
        }
        return Err(unsafe { GetLastError() });
    }
    // Timed out or stop_event fired: cancel the pending I/O and wait for the
    // kernel to release `overlapped` before stack unwinding.
    // SAFETY: Valid handle + matching OVERLAPPED pointer.
    unsafe {
        CancelIoEx(handle, overlapped);
        let mut dummy = 0u32;
        let _ = GetOverlappedResult(handle, overlapped, &mut dummy, 1);
    }
    Err(if wait == WAIT_TIMEOUT {
        WAIT_TIMEOUT
    } else {
        ERROR_BROKEN_PIPE
    })
}

fn create_manual_event() -> Result<WinHandle, String> {
    // SAFETY: Unnamed manual-reset event, initially nonsignaled.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(format!(
            "Failed to create overlapped I/O event: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(WinHandle(event))
}

fn create_overlapped_server_pipe(
    pipe_name: &str,
    first_instance: bool,
) -> Result<WinHandle, String> {
    // SYSTEM + Administrators get full control; local Interactive Users (IU)
    // can read status and send validated cooling configuration updates without
    // FILE_CREATE_PIPE_INSTANCE. Remote clients are blocked by PIPE_REJECT_REMOTE_CLIENTS.
    let sddl = wide(SERVICE_PIPE_SDDL);
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
        return Err(format!(
            "Service pipe security descriptor failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let pipe = unsafe {
        CreateNamedPipeW(
            wide(pipe_name).as_ptr(),
            open_mode,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            MAX_PIPE_INSTANCES,
            MAX_IPC_FRAME_BYTES as u32,
            MAX_IPC_FRAME_BYTES as u32,
            PIPE_IO_TIMEOUT_MS,
            &attrs,
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    if pipe == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CreateNamedPipeW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(WinHandle(pipe))
}

fn write_overlapped(
    pipe: HANDLE,
    io_event: HANDLE,
    stop_event: Option<HANDLE>,
    bytes: &[u8],
    timeout_ms: u32,
) -> Result<(), u32> {
    unsafe {
        ResetEvent(io_event);
    }
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = io_event;
    let mut written = 0u32;
    let ok = unsafe {
        WriteFile(
            pipe,
            bytes.as_ptr().cast(),
            bytes.len() as u32,
            &mut written,
            &mut ov,
        )
    };
    let actual = if ok != 0 {
        written
    } else {
        let err = unsafe { GetLastError() };
        if err == ERROR_IO_PENDING {
            finish_overlapped(pipe, &mut ov, io_event, stop_event, timeout_ms)?
        } else {
            return Err(err);
        }
    };
    if actual as usize == bytes.len() {
        Ok(())
    } else {
        Err(ERROR_BROKEN_PIPE)
    }
}

fn read_overlapped(
    pipe: HANDLE,
    io_event: HANDLE,
    stop_event: Option<HANDLE>,
    buffer: &mut [u8],
    timeout_ms: u32,
) -> Result<usize, u32> {
    unsafe {
        ResetEvent(io_event);
    }
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = io_event;
    let mut read_bytes = 0u32;
    let ok = unsafe {
        ReadFile(
            pipe,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut read_bytes,
            &mut ov,
        )
    };
    if ok != 0 {
        Ok(read_bytes as usize)
    } else {
        let err = unsafe { GetLastError() };
        if err == ERROR_IO_PENDING {
            finish_overlapped(pipe, &mut ov, io_event, stop_event, timeout_ms).map(|n| n as usize)
        } else {
            Err(err)
        }
    }
}

/// Pre-bound `FILE_FLAG_OVERLAPPED` Named Pipe server. Binding before reporting
/// `SERVICE_RUNNING` to SCM guarantees clients can connect immediately and that
/// pipe creation failures surface cleanly instead of dying silently on a worker thread.
pub struct OverlappedPipeServer {
    pipe: WinHandle,
    io_event: WinHandle,
}

// SAFETY: Win32 named-pipe and event handles are process-wide kernel objects
// and ownership is transferred exclusively to the single IPC thread before use.
unsafe impl Send for OverlappedPipeServer {}

impl OverlappedPipeServer {
    pub fn bind() -> Result<Self, String> {
        Self::bind_named(SERVICE_PIPE_NAME)
    }

    fn bind_named(pipe_name: &str) -> Result<Self, String> {
        let pipe = create_overlapped_server_pipe(pipe_name, true)?;
        let io_event = create_manual_event()?;
        Ok(Self { pipe, io_event })
    }

    /// Runs the event-driven `FILE_FLAG_OVERLAPPED` Named Pipe loop until
    /// `stop_event` or `stop_flag` signals shutdown.
    ///
    /// # Safety
    /// `stop_event` must be a valid manual-reset event handle that outlives this call.
    pub unsafe fn run<F>(self, stop_event: HANDLE, stop_flag: &AtomicBool, handler: F)
    where
        F: Fn(ServiceRequest) -> ServiceResponse,
    {
        let mut buffer = vec![0u8; MAX_IPC_FRAME_BYTES];
        let mut drain_buf = [0u8; 1];

        while !stop_flag.load(Ordering::SeqCst) {
            unsafe {
                ResetEvent(self.io_event.0);
            }
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = self.io_event.0;
            let connected = unsafe { ConnectNamedPipe(self.pipe.0, &mut ov) };
            let client_ready = if connected != 0 {
                true
            } else {
                let err = unsafe { GetLastError() };
                if err == ERROR_PIPE_CONNECTED {
                    true
                } else if err == ERROR_IO_PENDING {
                    // Block with zero CPU wakeups until either a client connects or stop_event fires.
                    let handles = [self.io_event.0, stop_event];
                    let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) };
                    if wait == WAIT_OBJECT_0 {
                        let mut dummy = 0u32;
                        unsafe { GetOverlappedResult(self.pipe.0, &ov, &mut dummy, 0) != 0 }
                    } else {
                        unsafe {
                            CancelIoEx(self.pipe.0, &ov);
                            let mut dummy = 0u32;
                            let _ = GetOverlappedResult(self.pipe.0, &ov, &mut dummy, 1);
                        }
                        break;
                    }
                } else {
                    // Transient error: sleep briefly on stop_event before retrying.
                    let wait = unsafe { WaitForSingleObject(stop_event, 100) };
                    if wait == WAIT_OBJECT_0 {
                        break;
                    }
                    false
                }
            };

            if !client_ready {
                continue;
            }
            if stop_flag.load(Ordering::SeqCst) {
                unsafe {
                    DisconnectNamedPipe(self.pipe.0);
                }
                break;
            }

            let mut response_sent = false;
            match read_overlapped(
                self.pipe.0,
                self.io_event.0,
                Some(stop_event),
                &mut buffer,
                PIPE_IO_TIMEOUT_MS,
            ) {
                Ok(len) => {
                    let response = match decode_ipc::<ServiceRequest>(&buffer[..len]) {
                        Ok(req) => handler(req),
                        Err(err) => ServiceResponse::Error {
                            message: format!("Invalid IPC request: {err}"),
                        },
                    };
                    if let Ok(bytes) = encode_ipc(&response) {
                        response_sent = write_overlapped(
                            self.pipe.0,
                            self.io_event.0,
                            Some(stop_event),
                            &bytes,
                            PIPE_IO_TIMEOUT_MS,
                        )
                        .is_ok();
                    }
                }
                Err(ERROR_MORE_DATA) => {
                    let response = ServiceResponse::Error {
                        message: "IPC request exceeded frame limit".into(),
                    };
                    if let Ok(bytes) = encode_ipc(&response) {
                        response_sent = write_overlapped(
                            self.pipe.0,
                            self.io_event.0,
                            Some(stop_event),
                            &bytes,
                            PIPE_IO_TIMEOUT_MS,
                        )
                        .is_ok();
                    }
                }
                Err(_) => {}
            }

            // Calling DisconnectNamedPipe immediately after WriteFile discards
            // unread response bytes in the kernel pipe buffer before the client
            // calls ReadFile. Wait boundedly for the client to finish reading
            // and close its handle (returns ERROR_BROKEN_PIPE in <1 ms on normal
            // peers, and cancels cleanly after PIPE_DRAIN_TIMEOUT_MS on stalled peers).
            if response_sent && !stop_flag.load(Ordering::SeqCst) {
                let _ = read_overlapped(
                    self.pipe.0,
                    self.io_event.0,
                    Some(stop_event),
                    &mut drain_buf,
                    PIPE_DRAIN_TIMEOUT_MS,
                );
            }

            unsafe {
                DisconnectNamedPipe(self.pipe.0);
            }
        }
    }
}

/// Runs the event-driven `FILE_FLAG_OVERLAPPED` Named Pipe server for
/// `GLPHService`. Zero idle polling, bounded per-client read/write
/// timeouts, immediate wakeup on `stop_event`, and bounded client drain.
///
/// # Safety
/// `stop_event` must be a valid manual-reset event handle that outlives this call.
pub unsafe fn run_overlapped_pipe_server<F>(stop_event: HANDLE, stop_flag: &AtomicBool, handler: F)
where
    F: Fn(ServiceRequest) -> ServiceResponse,
{
    if let Ok(server) = OverlappedPipeServer::bind() {
        // SAFETY: Caller guarantees `stop_event` outlives this call.
        unsafe { server.run(stop_event, stop_flag, handler) };
    }
}

/// Sends one bounded JSON request to `GLPHService` over
/// `\\.\pipe\GLPH.Service.v1` using `FILE_FLAG_OVERLAPPED` I/O with
/// bounded connect and read/write timeouts.
pub fn send_service_request(request: &ServiceRequest) -> Result<ServiceResponse, String> {
    send_service_request_named(SERVICE_PIPE_NAME, request)
}

fn connect_client_pipe(pipe_name_str: &str) -> Result<WinHandle, String> {
    let pipe_name = wide(pipe_name_str);
    let connect_deadline =
        Instant::now() + Duration::from_millis(u64::from(PIPE_CONNECT_TIMEOUT_MS));
    let handle = loop {
        // SAFETY: Fixed local pipe name, overlapped mode, least-privilege client rights.
        let h = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                CLIENT_PIPE_ACCESS,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            break WinHandle(h);
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY {
            let remaining = connect_deadline.saturating_duration_since(Instant::now());
            let wait_ms = remaining
                .as_millis()
                .min(u128::from(PIPE_CONNECT_TIMEOUT_MS)) as u32;
            if wait_ms == 0 || unsafe { WaitNamedPipeW(pipe_name.as_ptr(), wait_ms) } == 0 {
                return Err("GLPHService pipe timed out".into());
            }
            continue;
        }
        if err == ERROR_FILE_NOT_FOUND {
            return Err("GLPHService is not running".into());
        }
        return Err(format!(
            "Could not connect to GLPHService: {}",
            std::io::Error::last_os_error()
        ));
    };

    let mode = PIPE_READMODE_MESSAGE;
    // SAFETY: Valid connected client pipe handle.
    if unsafe { SetNamedPipeHandleState(handle.0, &mode, ptr::null(), ptr::null()) } == 0 {
        return Err(format!(
            "Service pipe mode setup failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(handle)
}

fn send_service_request_named(
    pipe_name_str: &str,
    request: &ServiceRequest,
) -> Result<ServiceResponse, String> {
    let encoded = encode_ipc(request)?;
    let handle = connect_client_pipe(pipe_name_str)?;

    let io_event = create_manual_event()?;
    write_overlapped(handle.0, io_event.0, None, &encoded, PIPE_IO_TIMEOUT_MS).map_err(|err| {
        if err == WAIT_TIMEOUT {
            "Timed out writing request to GLPHService".to_owned()
        } else {
            format!("Failed to write request to GLPHService (os error {err})")
        }
    })?;

    let mut response = vec![0u8; MAX_IPC_FRAME_BYTES];
    let read_len = read_overlapped(
        handle.0,
        io_event.0,
        None,
        &mut response,
        PIPE_IO_TIMEOUT_MS,
    )
    .map_err(|err| {
        if err == ERROR_MORE_DATA {
            "GLPHService response exceeded frame size limit".to_owned()
        } else if err == WAIT_TIMEOUT {
            "Timed out reading response from GLPHService".to_owned()
        } else {
            format!("Failed to read response from GLPHService (os error {err})")
        }
    })?;
    response.truncate(read_len);
    decode_ipc(&response)
}

/// Queries live runtime telemetry/cooling status from `GLPHService`.
/// Returns `Ok(None)` when the service is not running.
pub fn query_live_service_status() -> Result<Option<ServiceStatus>, String> {
    match send_service_request(&ServiceRequest::Status {}) {
        Ok(ServiceResponse::Status { status }) => Ok(Some(status)),
        Ok(ServiceResponse::Error { message }) => Err(message),
        Ok(_) => Err("Unexpected response from GLPHService".into()),
        Err(err) if err.contains("is not running") => Ok(None),
        Err(err) => Err(err),
    }
}

/// Pushes an updated cooling configuration to a running `GLPHService`.
/// Returns `Ok(true)` if synced live, `Ok(false)` if the service is not running.
pub fn sync_service_config_if_running(config: &ServiceConfig) -> Result<bool, String> {
    config.validate()?;
    match send_service_request(&ServiceRequest::ApplyConfig {
        config: config.clone(),
    }) {
        Ok(ServiceResponse::Ack { .. }) => Ok(true),
        Ok(ServiceResponse::Error { message }) => Err(message),
        Ok(_) => Err("Unexpected response from GLPHService".into()),
        Err(err) if err.contains("is not running") => Ok(false),
        Err(err) => Err(err),
    }
}

/// Helper kept for callers that want to wait briefly after a service start/stop.
pub fn wait_for_service_state(target_running: bool, timeout: Duration) -> Result<bool, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let info = query_service_info()?;
        if info.state.is_running() == target_running {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glph_core::{profiles::FanPoint, quick_switch::FanMode};
    use std::sync::Arc;
    use windows_sys::Win32::System::Threading::SetEvent;

    #[test]
    fn scm_state_codes_map_cleanly() {
        assert_eq!(
            ScmServiceState::from_win32(SERVICE_RUNNING),
            ScmServiceState::Running
        );
        assert!(ScmServiceState::Running.is_running());
        assert_eq!(
            ScmServiceState::from_win32(SERVICE_STOPPED),
            ScmServiceState::Stopped
        );
        assert!(!ScmServiceState::Stopped.is_running());
        assert_eq!(
            ScmServiceState::from_win32(SERVICE_PAUSED),
            ScmServiceState::Paused
        );
    }

    #[test]
    fn service_config_persists_atomically_and_validates() {
        let temp = std::env::temp_dir().join(format!("glph-svc-cfg-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);

        // Missing file returns default safe BIOS config.
        let initial = load_service_config_from(&temp).unwrap();
        assert_eq!(initial, ServiceConfig::default());

        let custom = ServiceConfig {
            schema: 1,
            fan_mode: FanMode::Curve,
            fan_curve: Some(vec![
                FanPoint {
                    temperature_c: 45,
                    duty_percent: 30,
                },
                FanPoint {
                    temperature_c: 85,
                    duty_percent: 95,
                },
            ]),
            chassis_max_temp_c: Some(46),
            paused: false,
        };
        save_service_config_to(&temp, &custom).unwrap();
        let reloaded = load_service_config_from(&temp).unwrap();
        assert_eq!(reloaded, custom);

        // Invalid config is refused before touching disk.
        let mut invalid = custom;
        invalid.chassis_max_temp_c = Some(99);
        assert!(save_service_config_to(&temp, &invalid).is_err());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn overlapped_pipe_round_trip_and_immediate_stop() {
        let pipe_name = format!(
            r"\\.\pipe\GLPH.TestPipe.{}.{}",
            std::process::id(),
            CONFIG_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let bound = OverlappedPipeServer::bind_named(&pipe_name).unwrap();
        // Second server instance on the same pipe must be rejected (no pipe squatting).
        assert!(OverlappedPipeServer::bind_named(&pipe_name).is_err());

        let stop_event = create_manual_event().unwrap();
        let stop_raw = stop_event.0 as isize;
        let stop_flag = Arc::new(AtomicBool::new(false));
        let server_flag = Arc::clone(&stop_flag);
        let server = std::thread::spawn(move || unsafe {
            bound.run(stop_raw as HANDLE, &server_flag, |req| match req {
                ServiceRequest::Status {} => ServiceResponse::Status {
                    status: ServiceStatus::default(),
                },
                _ => ServiceResponse::Ack {
                    message: "ok".into(),
                },
            });
        });

        // Pre-bound pipe is immediately connectable with zero startup race.
        let resp = send_service_request_named(&pipe_name, &ServiceRequest::Status {})
            .expect("pipe roundtrip failed");
        assert!(matches!(resp, ServiceResponse::Status { .. }));

        // Delayed client read after write must still receive the complete response
        // without premature DisconnectNamedPipe discarding unread kernel buffer bytes.
        let client = connect_client_pipe(&pipe_name).expect("delayed client connect failed");
        let io_event = create_manual_event().unwrap();
        let req_bytes = encode_ipc(&ServiceRequest::Status {}).unwrap();
        write_overlapped(client.0, io_event.0, None, &req_bytes, PIPE_IO_TIMEOUT_MS).unwrap();
        // Deliberately pause before reading so the server has already written the reply.
        std::thread::sleep(Duration::from_millis(60));
        let mut resp_buf = vec![0u8; MAX_IPC_FRAME_BYTES];
        let n = read_overlapped(
            client.0,
            io_event.0,
            None,
            &mut resp_buf,
            PIPE_IO_TIMEOUT_MS,
        )
        .expect("delayed client read must receive full reply before disconnect");
        let delayed: ServiceResponse = decode_ipc(&resp_buf[..n]).unwrap();
        assert!(matches!(delayed, ServiceResponse::Status { .. }));
        drop(client);

        stop_flag.store(true, Ordering::SeqCst);
        unsafe {
            SetEvent(stop_event.0);
        }
        server.join().unwrap();
    }

    #[test]
    fn service_pipe_sddl_denies_pipe_instance_creation_to_interactive_users() {
        // 0x0012018B = FILE_GENERIC_READ | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES
        // Bit 2 (0x04 = FILE_CREATE_PIPE_INSTANCE) must NOT be granted to IU.
        assert_eq!(CLIENT_PIPE_ACCESS & 0x0000_0004, 0);
        assert_eq!(MAX_PIPE_INSTANCES, 1);
        assert!(SERVICE_PIPE_SDDL.contains("(A;;0x0012018B;;;IU)"));
        assert!(!SERVICE_PIPE_SDDL.contains("GW;;;IU"));
    }
}
