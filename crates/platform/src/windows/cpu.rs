//! Public read-only Windows CPU load timing (`GetSystemTimes`).
//! CPU temperature and effective clocks are NEVER read from Windows APIs;
//! they come exclusively from direct hardware counters via `cpu_hardware`.
use glph_core::{
    CpuSample,
    cpu::{CpuTimes, busy_percent},
};
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::FILETIME,
    System::Threading::{GetActiveProcessorCount, GetSystemTimes},
};

fn ticks(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn system_times() -> Result<CpuTimes, String> {
    let mut idle = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut kernel, mut user) = (idle, idle);
    // SAFETY: Three correctly sized writable FILETIME structures.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return Err(format!(
            "CPU timing unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(CpuTimes {
        idle: ticks(idle),
        kernel: ticks(kernel),
        user: ticks(user),
    })
}

pub struct Cpu {
    count: u32,
    supported: bool,
    previous: Option<CpuTimes>,
    last_busy_percent: Option<f64>,
    sampled_at: Instant,
}
impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}
impl Cpu {
    pub fn new() -> Self {
        // SAFETY: ALL_PROCESSOR_GROUPS is the documented sentinel; this only reads a count.
        let count = unsafe { GetActiveProcessorCount(u16::MAX) };
        let supported = (1..=64).contains(&count);
        Self {
            count,
            supported,
            // Beyond one group GetSystemTimes does not report a global total.
            // Until a group-aware adapter exists, avoid showing a misleading partial result.
            previous: if supported { system_times().ok() } else { None },
            last_busy_percent: None,
            sampled_at: Instant::now(),
        }
    }
    pub fn sample(&mut self) -> CpuSample {
        let mut sample = CpuSample {
            logical_processors: (self.count != 0).then_some(self.count),
            ..Default::default()
        };
        if !self.supported {
            sample.warnings.push("CPU telemetry currently requires a single processor group (1–64 logical processors)".into());
            return sample;
        }
        if self.sampled_at.elapsed() >= Duration::from_millis(200) {
            match system_times() {
                Ok(current) => {
                    if let Some(previous) = self.previous.replace(current) {
                        sample.busy_percent = busy_percent(previous, current);
                        self.last_busy_percent = sample.busy_percent;
                        if sample.busy_percent.is_none() {
                            sample.warnings.push("CPU timing counters reset or were inconsistent; taking a fresh baseline".into());
                        }
                    }
                }
                Err(error) => {
                    self.previous = None;
                    self.last_busy_percent = None;
                    sample.warnings.push(error);
                }
            }
            self.sampled_at = Instant::now();
        } else {
            // Preserve the most recent valid reading during rapid polls (e.g. live
            // poll-rate changes) so UI telemetry never flashes blank (`—`).
            sample.busy_percent = self.last_busy_percent;
        }
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_sample_within_200ms_preserves_last_busy_percent() {
        let mut cpu = Cpu::new();
        cpu.last_busy_percent = Some(42.5);
        cpu.sampled_at = Instant::now();
        let sample = cpu.sample();
        assert_eq!(sample.busy_percent, Some(42.5));
    }
}
