//! Dedicated Windows Service (`GLPHService`) for GLPH (GamingLaptopPerformanceHub).
//!
//! Runs in Session 0 under `LocalSystem` — starting before user logon — to
//! drive direct hardware cooling (CPU Tctl via PawnIO, GPU temp via NVAPI with
//! D3cold sleep guard, chassis IR via HP WMI 0x23) with zero micro-stutter
//! (`THREAD_PRIORITY_BELOW_NORMAL`), while exposing a strict local Named Pipe
//! (`\\.\pipe\GLPH.Service.v1`) for status and validated config sync.
//!
//! Interactive foreground CCD pinning is intentionally delegated to Session 1
//! (`glph.exe --tray`) because Session-0 services cannot inspect
//! user desktop foreground windows.

#[cfg(not(windows))]
fn main() {
    eprintln!("glph-service requires Windows.");
    std::process::exit(1);
}

#[cfg(windows)]
mod win_service {
    use glph_core::service::{
        SERVICE_NAME, SERVICE_PIPE_NAME, ServiceConfig, ServiceRequest, ServiceResponse,
        ServiceStatus,
    };
    use glph_platform::service_client::{
        OverlappedPipeServer, load_service_config, save_service_config,
    };
    use std::{
        ffi::c_void,
        ptr,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, AtomicIsize, Ordering},
        },
        thread,
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SERVICE_SPECIFIC_ERROR, HANDLE, NO_ERROR, WAIT_OBJECT_0},
        System::{
            Services::*,
            Threading::{CreateEventW, SetEvent, WaitForMultipleObjects},
        },
        UI::WindowsAndMessaging::{PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    struct SharedState {
        config: Mutex<ServiceConfig>,
        status: Mutex<ServiceStatus>,
        stop_requested: AtomicBool,
        suspended: AtomicBool,
        stop_event: AtomicIsize,
        wake_event: AtomicIsize,
        service_status_handle: AtomicIsize,
    }

    impl SharedState {
        fn new(initial_config: ServiceConfig) -> Result<Self, String> {
            // SAFETY: Manual-reset unnamed event owned for the lifetime of the process.
            let stop_event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            if stop_event.is_null() {
                return Err(format!(
                    "Failed to create service stop event: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: Auto-reset unnamed wake event for instant standby/resume/config response.
            let wake_event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
            if wake_event.is_null() {
                unsafe {
                    CloseHandle(stop_event);
                }
                return Err(format!(
                    "Failed to create service wake event: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let status = ServiceStatus {
                process_id: std::process::id(),
                paused: initial_config.paused,
                ..ServiceStatus::default()
            };
            Ok(Self {
                config: Mutex::new(initial_config),
                status: Mutex::new(status),
                stop_requested: AtomicBool::new(false),
                suspended: AtomicBool::new(false),
                stop_event: AtomicIsize::new(stop_event as isize),
                wake_event: AtomicIsize::new(wake_event as isize),
                service_status_handle: AtomicIsize::new(0),
            })
        }

        fn signal_stop(&self) {
            self.stop_requested.store(true, Ordering::SeqCst);
            let handle = self.stop_event.load(Ordering::SeqCst) as HANDLE;
            if !handle.is_null() {
                // SAFETY: Valid manual-reset event created in SharedState::new.
                unsafe {
                    SetEvent(handle);
                }
            }
            self.signal_wake();
        }

        fn signal_wake(&self) {
            let handle = self.wake_event.load(Ordering::SeqCst) as HANDLE;
            if !handle.is_null() {
                // SAFETY: Valid auto-reset event created in SharedState::new.
                unsafe {
                    SetEvent(handle);
                }
            }
        }

        fn is_stopping(&self) -> bool {
            self.stop_requested.load(Ordering::SeqCst)
        }

        /// Waits until `timeout` elapses, `stop_event` fires, or `wake_event`
        /// fires (standby, resume, or live config update). Returns `Ok(true)`
        /// ONLY when an actual service stop was requested — never on standby.
        fn wait_for_stop_or_wake(&self, timeout: Duration) -> Result<bool, String> {
            if self.is_stopping() {
                return Ok(true);
            }
            let stop = self.stop_event.load(Ordering::SeqCst) as HANDLE;
            let wake = self.wake_event.load(Ordering::SeqCst) as HANDLE;
            if stop.is_null() || wake.is_null() {
                thread::sleep(timeout);
                return Ok(self.is_stopping());
            }
            let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            let handles = [stop, wake];
            // SAFETY: Both handles are valid event handles owned by SharedState.
            // `wake` is an auto-reset event: WaitForMultipleObjects resets it
            // atomically on wakeup. Never call ResetEvent(wake) afterwards or
            // a concurrent suspend/config wakeup would be silently lost.
            match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) } {
                WAIT_OBJECT_0 => Ok(true),
                x if x == WAIT_OBJECT_0 + 1 => Ok(self.is_stopping()),
                _ => Ok(self.is_stopping()),
            }
        }

        fn report_scm_state(&self, current_state: u32, win32_exit_code: u32, wait_hint_ms: u32) {
            let handle = self.service_status_handle.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE;
            if handle.is_null() {
                return;
            }
            let controls = if current_state == SERVICE_RUNNING {
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_POWEREVENT
            } else {
                0
            };
            let checkpoint = u32::from(matches!(
                current_state,
                SERVICE_START_PENDING | SERVICE_STOP_PENDING
            ));
            let status = SERVICE_STATUS {
                dwServiceType: SERVICE_WIN32_OWN_PROCESS,
                dwCurrentState: current_state,
                dwControlsAccepted: controls,
                dwWin32ExitCode: win32_exit_code,
                dwServiceSpecificExitCode: if win32_exit_code == ERROR_SERVICE_SPECIFIC_ERROR {
                    1
                } else {
                    0
                },
                dwCheckPoint: checkpoint,
                dwWaitHint: wait_hint_ms,
            };
            // SAFETY: Valid status handle registered by RegisterServiceCtrlHandlerExW.
            unsafe {
                SetServiceStatus(handle, &status);
            }
        }
    }

    impl Drop for SharedState {
        fn drop(&mut self) {
            let stop = self.stop_event.swap(0, Ordering::SeqCst) as HANDLE;
            if !stop.is_null() {
                // SAFETY: Owned event handle.
                unsafe {
                    CloseHandle(stop);
                }
            }
            let wake = self.wake_event.swap(0, Ordering::SeqCst) as HANDLE;
            if !wake.is_null() {
                // SAFETY: Owned event handle.
                unsafe {
                    CloseHandle(wake);
                }
            }
        }
    }

    static GLOBAL_STATE: OnceLock<Arc<SharedState>> = OnceLock::new();

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| d.as_millis().try_into().ok())
            .unwrap_or(0)
    }

    fn handle_ipc_request(state: &SharedState, request: ServiceRequest) -> ServiceResponse {
        match request {
            ServiceRequest::Status {} => {
                let mut status = state.status.lock().map(|g| g.clone()).unwrap_or_default();
                status.suspended = state.suspended.load(Ordering::Relaxed);
                if let Ok(cfg) = state.config.lock() {
                    status.paused = cfg.paused;
                }
                ServiceResponse::Status { status }
            }
            ServiceRequest::GetConfig {} => match state.config.lock() {
                Ok(cfg) => ServiceResponse::Config {
                    config: cfg.clone(),
                },
                Err(_) => ServiceResponse::Error {
                    message: "Service configuration lock poisoned".into(),
                },
            },
            ServiceRequest::ApplyConfig { config } => {
                if let Err(err) = config.validate() {
                    return ServiceResponse::Error { message: err };
                }
                if state.config.is_poisoned() {
                    return ServiceResponse::Error {
                        message: "Service configuration lock poisoned".into(),
                    };
                }
                if let Err(err) = save_service_config(&config) {
                    return ServiceResponse::Error {
                        message: format!("Failed to persist service config: {err}"),
                    };
                }
                match state.config.lock() {
                    Ok(mut guard) => *guard = config.clone(),
                    Err(_) => {
                        return ServiceResponse::Error {
                            message: "Service configuration lock poisoned".into(),
                        };
                    }
                }
                if let Ok(mut status) = state.status.lock() {
                    status.paused = config.paused;
                }
                state.signal_wake();
                ServiceResponse::Ack {
                    message: "Cooling configuration updated".into(),
                }
            }
            ServiceRequest::SetPaused { paused } => {
                // Build candidate config and persist BEFORE mutating in-memory state
                // so a disk/ACL write failure never leaves RAM and disk out of sync.
                let mut candidate = match state.config.lock() {
                    Ok(cfg) => cfg.clone(),
                    Err(_) => {
                        return ServiceResponse::Error {
                            message: "Service configuration lock poisoned".into(),
                        };
                    }
                };
                candidate.paused = paused;
                if let Err(err) = save_service_config(&candidate) {
                    return ServiceResponse::Error {
                        message: format!("Failed to persist pause state: {err}"),
                    };
                }
                match state.config.lock() {
                    Ok(mut cfg) => cfg.paused = paused,
                    Err(_) => {
                        return ServiceResponse::Error {
                            message: "Service configuration lock poisoned".into(),
                        };
                    }
                }
                if let Ok(mut status) = state.status.lock() {
                    status.paused = paused;
                    if paused {
                        status.cooling_state = "paused".into();
                    }
                }
                state.signal_wake();
                ServiceResponse::Ack {
                    message: if paused {
                        "Cooling paused".into()
                    } else {
                        "Cooling resumed".into()
                    },
                }
            }
        }
    }

    fn run_pipe_server(server: OverlappedPipeServer, state: Arc<SharedState>) {
        let stop_handle = state.stop_event.load(Ordering::SeqCst) as HANDLE;
        if stop_handle.is_null() {
            return;
        }
        // SAFETY: `stop_handle` is owned by `state` which stays alive for this entire call.
        unsafe {
            server.run(stop_handle, &state.stop_requested, |req| {
                handle_ipc_request(&state, req)
            });
        }
    }

    fn run_cooling_worker(state: Arc<SharedState>) {
        let inventory = glph_platform::inventory();
        while !state.is_stopping() {
            let state_mode = Arc::clone(&state);
            let state_curve = Arc::clone(&state);
            let state_ir = Arc::clone(&state);
            let state_suspended = Arc::clone(&state);
            let state_report = Arc::clone(&state);
            let state_stop = Arc::clone(&state);

            let outcome = glph_platform::run_session0_cooling(
                &inventory,
                move || {
                    state_mode
                        .config
                        .lock()
                        .map(|c| c.effective_fan_mode())
                        .unwrap_or(glph_core::quick_switch::FanMode::Bios)
                },
                move || {
                    state_curve
                        .config
                        .lock()
                        .map(|c| c.fan_curve.clone())
                        .map_err(|_| "Service config lock poisoned".into())
                },
                move || {
                    state_ir
                        .config
                        .lock()
                        .map(|c| c.effective_ir_limit_c())
                        .unwrap_or(glph_core::fan::IR_EMERGENCY_C)
                },
                move || state_suspended.suspended.load(Ordering::SeqCst),
                move |fan_status| {
                    if let Ok(mut status) = state_report.status.lock() {
                        status.process_id = fan_status.process_id;
                        status.cooling_state = fan_status.state.to_owned();
                        status.detail = fan_status.detail.clone();
                        status.cpu_c = fan_status.cpu_c;
                        status.gpu_c = fan_status.gpu_c;
                        status.ir_c = fan_status.ir_c;
                        status.protection = fan_status.protection;
                        status.cpu_level = fan_status.cpu_level;
                        status.gpu_level = fan_status.gpu_level;
                        status.suspended = state_report.suspended.load(Ordering::Relaxed);
                        status.updated_unix_ms = fan_status.updated_unix_ms;
                    }
                },
                move |wait| state_stop.wait_for_stop_or_wake(wait),
            );

            if state.is_stopping() {
                break;
            }

            if let Err(err) = outcome {
                if let Ok(mut status) = state.status.lock() {
                    status.cooling_state = "error".into();
                    status.detail = err;
                    status.updated_unix_ms = unix_ms();
                }
                // Back off cleanly before retrying so an unsupported machine or
                // transient WMI error never spins a CPU core.
                if state
                    .wait_for_stop_or_wake(Duration::from_secs(5))
                    .unwrap_or(true)
                {
                    break;
                }
            }
        }
    }

    fn run_service_runtime(
        server: OverlappedPipeServer,
        state: Arc<SharedState>,
    ) -> Result<(), String> {
        let pipe_state = Arc::clone(&state);
        let pipe_thread = thread::Builder::new()
            .name("glph-service-ipc".into())
            .spawn(move || run_pipe_server(server, pipe_state))
            .map_err(|e| format!("Failed to spawn service IPC thread: {e}"))?;

        run_cooling_worker(Arc::clone(&state));

        state.signal_stop();
        let _ = pipe_thread.join();
        Ok(())
    }

    unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> i32 {
        if let Some(state) = GLOBAL_STATE.get() {
            state.signal_stop();
            return 1;
        }
        0
    }

    unsafe extern "system" fn service_ctrl_handler(
        control: u32,
        event_type: u32,
        _event_data: *mut c_void,
        _context: *mut c_void,
    ) -> u32 {
        let Some(state) = GLOBAL_STATE.get() else {
            return NO_ERROR;
        };
        match control {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                state.report_scm_state(SERVICE_STOP_PENDING, NO_ERROR, 5_000);
                state.signal_stop();
                NO_ERROR
            }
            SERVICE_CONTROL_POWEREVENT => {
                match event_type {
                    PBT_APMSUSPEND => {
                        state.suspended.store(true, Ordering::SeqCst);
                        state.signal_wake();
                    }
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                        state.suspended.store(false, Ordering::SeqCst);
                        state.signal_wake();
                    }
                    _ => {}
                }
                NO_ERROR
            }
            SERVICE_CONTROL_INTERROGATE => NO_ERROR,
            _ => NO_ERROR,
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let initial_config = load_service_config().unwrap_or_default();
        let Ok(state) = SharedState::new(initial_config) else {
            return;
        };
        let state = Arc::new(state);
        let _ = GLOBAL_STATE.set(Arc::clone(&state));

        let name = wide(SERVICE_NAME);
        // SAFETY: Registers our control handler with the SCM for this service.
        let status_handle = unsafe {
            RegisterServiceCtrlHandlerExW(
                name.as_ptr(),
                Some(service_ctrl_handler),
                ptr::null_mut(),
            )
        };
        if status_handle.is_null() {
            return;
        }
        state
            .service_status_handle
            .store(status_handle as isize, Ordering::SeqCst);
        state.report_scm_state(SERVICE_START_PENDING, NO_ERROR, 4_000);

        // Bind the Named Pipe BEFORE reporting SERVICE_RUNNING so immediate
        // post-Start-Service IPC clients never race ERROR_FILE_NOT_FOUND and
        // bind failures abort startup cleanly instead of dying silently.
        let server = match OverlappedPipeServer::bind() {
            Ok(server) => server,
            Err(_) => {
                state.report_scm_state(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR, 0);
                return;
            }
        };

        state.report_scm_state(SERVICE_RUNNING, NO_ERROR, 0);

        let exit_code = match run_service_runtime(server, Arc::clone(&state)) {
            Ok(()) => NO_ERROR,
            Err(_) => ERROR_SERVICE_SPECIFIC_ERROR,
        };

        state.report_scm_state(SERVICE_STOPPED, exit_code, 0);
    }

    pub fn entry() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.first().map(String::as_str) {
            Some("--help" | "-h") => {
                println!(
                    "glph-service {} ({SERVICE_NAME})\n\
                     Usage:\n  \
                     glph-service              Run as Windows Service (SCM)\n  \
                     glph-service --console    Run interactively in console\n  \
                     glph-service --version    Print service version",
                    env!("CARGO_PKG_VERSION")
                );
                return;
            }
            Some("--version" | "-V") => {
                println!("glph-service {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--console") => {
                let initial_config = load_service_config().unwrap_or_default();
                let state = match SharedState::new(initial_config) {
                    Ok(state) => Arc::new(state),
                    Err(err) => {
                        eprintln!("{err}");
                        std::process::exit(1);
                    }
                };
                let _ = GLOBAL_STATE.set(Arc::clone(&state));
                // SAFETY: Static handler signals SharedState::stop_event on Ctrl+C / console close.
                unsafe {
                    SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
                }
                let server = match OverlappedPipeServer::bind() {
                    Ok(server) => server,
                    Err(err) => {
                        eprintln!("Failed to bind {SERVICE_PIPE_NAME}: {err}");
                        std::process::exit(1);
                    }
                };
                println!("Starting {SERVICE_NAME} in console mode (pipe: {SERVICE_PIPE_NAME})...");
                if let Err(err) = run_service_runtime(server, state) {
                    eprintln!("{err}");
                    std::process::exit(1);
                }
                return;
            }
            _ => {}
        }

        let mut name = wide(SERVICE_NAME);
        let table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: name.as_mut_ptr(),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW {
                lpServiceName: ptr::null_mut(),
                lpServiceProc: None,
            },
        ];
        // SAFETY: Valid null-terminated service dispatch table for this SCM service.
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            eprintln!(
                "StartServiceCtrlDispatcherW failed ({}). Use --console to run interactively or --help for usage.",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use glph_core::{profiles::FanPoint, quick_switch::FanMode};

        #[test]
        fn ipc_handler_validates_and_tracks_pause_state() {
            let state = SharedState::new(ServiceConfig::default()).unwrap();
            let status_resp = handle_ipc_request(&state, ServiceRequest::Status {});
            assert!(matches!(status_resp, ServiceResponse::Status { .. }));

            // Out-of-range chassis limit must be rejected without mutating state.
            let bad_cfg = ServiceConfig {
                schema: 1,
                fan_mode: FanMode::Curve,
                fan_curve: Some(vec![
                    FanPoint {
                        temperature_c: 40,
                        duty_percent: 30,
                    },
                    FanPoint {
                        temperature_c: 80,
                        duty_percent: 90,
                    },
                ]),
                chassis_max_temp_c: Some(20),
                paused: false,
            };
            let err_resp =
                handle_ipc_request(&state, ServiceRequest::ApplyConfig { config: bad_cfg });
            assert!(matches!(err_resp, ServiceResponse::Error { .. }));
        }

        #[test]
        fn wake_event_does_not_lose_signal_and_stop_event_wakes_immediately() {
            let state = SharedState::new(ServiceConfig::default()).unwrap();
            state.signal_wake();
            let started = std::time::Instant::now();
            let stopping = state.wait_for_stop_or_wake(Duration::from_secs(5)).unwrap();
            assert!(!stopping);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "wake_event did not wake wait_for_stop_or_wake promptly"
            );

            // Second wait with no new signal must time out normally (auto-reset worked).
            let started = std::time::Instant::now();
            let stopping = state
                .wait_for_stop_or_wake(Duration::from_millis(40))
                .unwrap();
            assert!(!stopping);
            assert!(started.elapsed() >= Duration::from_millis(25));

            // Stop signal must return Ok(true) immediately.
            state.signal_stop();
            assert!(state.wait_for_stop_or_wake(Duration::from_secs(5)).unwrap());
        }
    }
}

#[cfg(windows)]
fn main() {
    win_service::entry();
}
