//! Process and thread priority policy for zero micro-stutters while gaming/working.
//! The UI runs at `NORMAL_PRIORITY_CLASS` (never `HIGH_PRIORITY_CLASS`, which
//! would preempt foreground games, audio, and DWM even under `THREAD_PRIORITY_BELOW_NORMAL`).
//! All background telemetry, watchdog, and sensor workers run strictly below normal.
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread, GetPriorityClass,
    NORMAL_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

/// Sets the foreground UI / control process to `NORMAL_PRIORITY_CLASS`.
pub fn configure() -> Result<(), String> {
    set_process_priority(NORMAL_PRIORITY_CLASS)
}

/// Sets a background sensor/telemetry child process to
/// `BELOW_NORMAL_PRIORITY_CLASS` and its main thread to
/// `THREAD_PRIORITY_BELOW_NORMAL` so hardware sweeps never preempt games or audio.
pub fn configure_background() -> Result<(), String> {
    set_process_priority(BELOW_NORMAL_PRIORITY_CLASS)?;
    configure_background_thread();
    Ok(())
}

/// Lowers only the calling background worker thread (e.g. `preset_hold`,
/// `fan_watch`, `ui::worker`, `telemetry_worker`) to `THREAD_PRIORITY_BELOW_NORMAL`.
pub fn configure_background_thread() {
    // SAFETY: Adjusts only the current thread's own scheduling priority.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

fn set_process_priority(class: u32) -> Result<(), String> {
    // SAFETY: The pseudo-handle belongs to this process and must not be closed.
    unsafe {
        let process = GetCurrentProcess();
        if SetPriorityClass(process, class) == 0 {
            return Err(glph_core::message::msg(
                "process.err.priority",
                &[&std::io::Error::last_os_error()],
            ));
        }
        if GetPriorityClass(process) != class {
            return Err(glph_core::message::msg(
                "process.err.priority_readback",
                &[],
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::GetThreadPriority;

    #[test]
    fn normal_and_background_priorities_are_set_without_preempting_foreground_apps() {
        struct Restore {
            process_class: u32,
            thread_prio: i32,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: Restore this test process/thread's original scheduling priorities.
                unsafe {
                    SetPriorityClass(GetCurrentProcess(), self.process_class);
                    SetThreadPriority(GetCurrentThread(), self.thread_prio);
                }
            }
        }
        let process_class = unsafe { GetPriorityClass(GetCurrentProcess()) };
        let thread_prio = unsafe { GetThreadPriority(GetCurrentThread()) };
        assert_ne!(process_class, 0);
        let _restore = Restore {
            process_class,
            thread_prio,
        };

        configure().unwrap();
        assert_eq!(
            unsafe { GetPriorityClass(GetCurrentProcess()) },
            NORMAL_PRIORITY_CLASS
        );

        configure_background().unwrap();
        assert_eq!(
            unsafe { GetPriorityClass(GetCurrentProcess()) },
            BELOW_NORMAL_PRIORITY_CLASS
        );
        assert_eq!(
            unsafe { GetThreadPriority(GetCurrentThread()) },
            THREAD_PRIORITY_BELOW_NORMAL
        );
    }
}
