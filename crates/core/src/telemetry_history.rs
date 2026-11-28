//! Rolling temperature history ring buffer and clean telemetry UI contract.
//!
//! Stores at least 5 minutes of CPU/GPU temperature samples at the highest
//! supported polling rate (`500 ms` -> 600 samples; buffer retains up to
//! [`MAX_HISTORY_SAMPLES`] = 720 samples) with monotonic elapsed timestamps
//! so X-axis coordinates are always expressed in `m:ss` relative duration,
//! never wall-clock time.
use crate::{CpuHardwareStatus, Snapshot};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Maximum retained points: 720 samples covers 6 minutes at 500 ms (headroom
/// beyond the 5-minute window so edge interpolation stays smooth).
pub const MAX_HISTORY_SAMPLES: usize = 720;
const MAX_RETENTION_SECS: f64 = 315.0;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HistoryWindow {
    #[default]
    Sec60,
    Min5,
}

impl HistoryWindow {
    pub const fn duration_secs(self) -> f64 {
        match self {
            Self::Sec60 => 60.0,
            Self::Min5 => 300.0,
        }
    }

    pub const fn from_index(index: i32) -> Self {
        match index {
            0 => Self::Sec60,
            _ => Self::Min5,
        }
    }

    pub const fn index(self) -> i32 {
        match self {
            Self::Sec60 => 0,
            Self::Min5 => 1,
        }
    }

    /// Five evenly-spaced relative-duration X-axis tick labels (`m:ss` format,
    /// oldest on left, `0:00` newest on right). Never wall-clock timestamps.
    pub const fn x_axis_ticks(self) -> [&'static str; 5] {
        match self {
            Self::Sec60 => ["-1:00", "-0:45", "-0:30", "-0:15", "0:00"],
            Self::Min5 => ["-5:00", "-3:45", "-2:30", "-1:15", "0:00"],
        }
    }
}

/// Formats a non-negative duration in seconds as `m:ss` (e.g. `0:00`, `1:15`, `5:00`).
pub fn format_duration_mm_ss(seconds: u32) -> String {
    let mins = seconds / 60;
    let secs = seconds % 60;
    format!("{mins}:{secs:02}")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistorySample {
    /// Elapsed seconds before the newest sample (`0.0` = newest, positive = older).
    pub age_s: f64,
    /// Normalized horizontal position in `[0.0, 1.0]` within the active window
    /// (`0.0` = left / oldest edge of window, `1.0` = right / newest sample).
    pub x_norm: f32,
    pub cpu_temp_c: Option<f32>,
    pub gpu_temp_c: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowView {
    pub window: HistoryWindow,
    /// Samples inside the active window, ordered oldest-to-newest (`x_norm` ascending).
    pub samples: Vec<HistorySample>,
    /// Peak CPU temperature (°C) across samples inside this window.
    pub cpu_peak_c: Option<f32>,
    /// Peak GPU temperature (°C) across samples inside this window.
    pub gpu_peak_c: Option<f32>,
    /// Most recent valid CPU temperature (°C) inside this window.
    pub cpu_current_c: Option<f32>,
    /// Most recent valid GPU temperature (°C) inside this window.
    pub gpu_current_c: Option<f32>,
    /// Auto-scaled lower Y-axis bound (°C), rounded down to a 10°C step.
    pub y_min_c: f32,
    /// Auto-scaled upper Y-axis bound (°C), rounded up to a 10°C step (at least `y_min_c + 40.0`).
    pub y_max_c: f32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CurrentMetrics {
    pub cpu_temp_c: Option<f32>,
    pub cpu_watts: Option<f32>,
    /// Effective/boost CPU clock in GHz (e.g. `3.84` for `3840 MHz`).
    pub cpu_clock_ghz: Option<f32>,
    pub cpu_util_pct: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_watts: Option<f32>,
    pub gpu_clock_mhz: Option<u32>,
    pub gpu_util_pct: Option<u8>,
}

impl CurrentMetrics {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let cpu = snapshot.cpu.as_ref();
        let gpu = snapshot.gpu.as_ref();
        let cpu_clock_ghz = matches!(snapshot.cpu_hardware_status, CpuHardwareStatus::Active)
            .then(|| {
                cpu.and_then(|c| c.hardware.as_ref())
                    .and_then(|h| h.effective_clocks.as_ref())
                    .and_then(|clocks| clocks.max_mhz())
                    .filter(|mhz| mhz.is_finite() && *mhz > 0.0)
                    .map(|mhz| (mhz / 1000.0) as f32)
            })
            .flatten();
        Self {
            cpu_temp_c: cpu
                .and_then(|c| c.temperature_c)
                .filter(|v| v.is_finite() && (0.0..=150.0).contains(v))
                .map(|v| v as f32),
            cpu_watts: cpu
                .and_then(|c| c.package_power_w)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| v as f32),
            cpu_clock_ghz,
            cpu_util_pct: cpu
                .and_then(|c| c.busy_percent)
                .filter(|v| v.is_finite())
                .map(|v| v.clamp(0.0, 100.0) as f32),
            gpu_temp_c: gpu
                .and_then(|g| g.temperature_c)
                .filter(|&v| (0..=150).contains(&v))
                .map(|v| v as f32),
            gpu_watts: gpu
                .and_then(|g| g.power_w)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| v as f32),
            gpu_clock_mhz: gpu.and_then(|g| g.clock_mhz),
            gpu_util_pct: gpu
                .and_then(|g| g.utilization_percent)
                .map(|pct| pct.min(100)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RawSample {
    at: Duration,
    cpu_temp_c: Option<f32>,
    gpu_temp_c: Option<f32>,
}

fn sanitize_temp(temp: Option<f32>) -> Option<f32> {
    temp.filter(|v| v.is_finite() && (0.0..=150.0).contains(v))
}

fn compute_y_bounds(min_observed: Option<f32>, max_observed: Option<f32>) -> (f32, f32) {
    let (Some(lo), Some(hi)) = (min_observed, max_observed) else {
        return (30.0, 90.0);
    };
    let mut y_min = ((lo - 5.0) / 10.0).floor() * 10.0;
    let mut y_max = ((hi + 5.0) / 10.0).ceil() * 10.0;
    y_min = y_min.clamp(0.0, 80.0);
    y_max = y_max.clamp(40.0, 120.0);
    if y_max - y_min < 40.0 {
        let mid = ((y_min + y_max) * 0.5 / 10.0).round() * 10.0;
        y_min = (mid - 20.0).max(0.0);
        y_max = (y_min + 40.0).min(120.0);
        if y_max - y_min < 40.0 {
            y_min = (y_max - 40.0).max(0.0);
        }
    }
    (y_min, y_max)
}

/// Bounded ring buffer of monotonic temperature samples supporting `60 s` and
/// `5 min` views with computed peaks and auto-scaled Y-axis ranges.
pub struct TemperatureHistory {
    origin: Instant,
    samples: VecDeque<RawSample>,
    capacity: usize,
}

impl Default for TemperatureHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl TemperatureHistory {
    pub fn new() -> Self {
        Self::with_capacity(MAX_HISTORY_SAMPLES)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        Self {
            origin: Instant::now(),
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.origin = Instant::now();
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Appends a sample at an explicit monotonic offset `at` from buffer origin.
    /// Useful for deterministic unit tests and offline screenshot harnesses.
    pub fn push_at(&mut self, at: Duration, cpu_temp_c: Option<f32>, gpu_temp_c: Option<f32>) {
        let cpu_temp_c = sanitize_temp(cpu_temp_c);
        let gpu_temp_c = sanitize_temp(gpu_temp_c);
        if let Some(last) = self.samples.back_mut() {
            if at < last.at {
                // Monotonic clock went backwards (synthetic reset): clear stale points.
                self.samples.clear();
            } else if at == last.at {
                last.cpu_temp_c = cpu_temp_c.or(last.cpu_temp_c);
                last.gpu_temp_c = gpu_temp_c.or(last.gpu_temp_c);
                return;
            }
        }
        if self.samples.len() >= self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(RawSample {
            at,
            cpu_temp_c,
            gpu_temp_c,
        });
        while self.samples.len() > 2
            && let Some(front) = self.samples.front()
            && at.saturating_sub(front.at).as_secs_f64() > MAX_RETENTION_SECS
        {
            self.samples.pop_front();
        }
    }

    /// Appends a live sample using the buffer's monotonic `Instant` clock.
    pub fn push_now(&mut self, cpu_temp_c: Option<f32>, gpu_temp_c: Option<f32>) {
        self.push_at(self.origin.elapsed(), cpu_temp_c, gpu_temp_c);
    }

    /// Extracts CPU/GPU temperatures from `snapshot` and appends them now.
    pub fn push_snapshot(&mut self, snapshot: &Snapshot) {
        let metrics = CurrentMetrics::from_snapshot(snapshot);
        self.push_now(metrics.cpu_temp_c, metrics.gpu_temp_c);
    }

    /// Returns samples, peaks, latest values, and auto-scaled Y bounds for `window`.
    pub fn window(&self, window: HistoryWindow) -> WindowView {
        let span_s = window.duration_secs();
        let Some(latest) = self.samples.back().map(|s| s.at) else {
            return WindowView {
                window,
                samples: vec![],
                cpu_peak_c: None,
                gpu_peak_c: None,
                cpu_current_c: None,
                gpu_current_c: None,
                y_min_c: 30.0,
                y_max_c: 90.0,
            };
        };

        let mut samples = Vec::new();
        let mut cpu_peak: Option<f32> = None;
        let mut gpu_peak: Option<f32> = None;
        let mut cpu_current: Option<f32> = None;
        let mut gpu_current: Option<f32> = None;
        let mut min_temp: Option<f32> = None;
        let mut max_temp: Option<f32> = None;

        for raw in &self.samples {
            let age_s = latest.saturating_sub(raw.at).as_secs_f64();
            if age_s > span_s + 1e-6 {
                continue;
            }
            let x_norm = ((span_s - age_s) / span_s).clamp(0.0, 1.0) as f32;
            if let Some(c) = raw.cpu_temp_c {
                cpu_peak = Some(cpu_peak.map_or(c, |p| p.max(c)));
                cpu_current = Some(c);
                min_temp = Some(min_temp.map_or(c, |m| m.min(c)));
                max_temp = Some(max_temp.map_or(c, |m| m.max(c)));
            }
            if let Some(g) = raw.gpu_temp_c {
                gpu_peak = Some(gpu_peak.map_or(g, |p| p.max(g)));
                gpu_current = Some(g);
                min_temp = Some(min_temp.map_or(g, |m| m.min(g)));
                max_temp = Some(max_temp.map_or(g, |m| m.max(g)));
            }
            samples.push(HistorySample {
                age_s,
                x_norm,
                cpu_temp_c: raw.cpu_temp_c,
                gpu_temp_c: raw.gpu_temp_c,
            });
        }

        let (y_min_c, y_max_c) = compute_y_bounds(min_temp, max_temp);
        WindowView {
            window,
            samples,
            cpu_peak_c: cpu_peak,
            gpu_peak_c: gpu_peak,
            cpu_current_c: cpu_current,
            gpu_current_c: gpu_current,
            y_min_c,
            y_max_c,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CpuHardwareSample, CpuHardwareSource, CpuSample, CpuTemperatureKind, GpuSample, cpu::*,
    };

    #[test]
    fn ring_buffer_retains_five_minutes_at_500ms_and_computes_window_peaks() {
        let mut hist = TemperatureHistory::new();
        // Push 660 samples at 500 ms spacing = 330 seconds total.
        for i in 0..660u64 {
            let t = Duration::from_millis(i * 500);
            // Spike early (at t = 100s -> age 229.5s, inside 5m but outside 60s)
            let cpu = if i == 200 {
                94.0
            } else {
                62.0 + (i % 10) as f32
            };
            // Spike recently (at t = 310s -> age 19.5s, inside both 60s and 5m)
            let gpu = if i == 620 {
                79.0
            } else {
                52.0 + (i % 8) as f32
            };
            hist.push_at(t, Some(cpu), Some(gpu));
        }

        assert!(hist.len() >= 600);
        assert!(hist.len() <= MAX_HISTORY_SAMPLES);

        let v60 = hist.window(HistoryWindow::Sec60);
        assert_eq!(v60.window, HistoryWindow::Sec60);
        // 60 seconds at 500ms inclusive = 121 samples.
        assert_eq!(v60.samples.len(), 121);
        assert!((v60.samples.first().unwrap().x_norm - 0.0).abs() < 0.01);
        assert!((v60.samples.last().unwrap().x_norm - 1.0).abs() < 1e-6);
        assert_eq!(v60.samples.last().unwrap().age_s, 0.0);
        // Early 94°C CPU spike is outside 60s window!
        assert_eq!(v60.cpu_peak_c, Some(71.0));
        assert_eq!(v60.gpu_peak_c, Some(79.0));

        let v5m = hist.window(HistoryWindow::Min5);
        assert_eq!(v5m.window, HistoryWindow::Min5);
        // 300 seconds at 500ms inclusive = 601 samples.
        assert_eq!(v5m.samples.len(), 601);
        assert!((v5m.samples.first().unwrap().x_norm - 0.0).abs() < 0.005);
        assert!((v5m.samples.last().unwrap().x_norm - 1.0).abs() < 1e-6);
        // Early 94°C CPU spike IS inside 5m window!
        assert_eq!(v5m.cpu_peak_c, Some(94.0));
        assert_eq!(v5m.gpu_peak_c, Some(79.0));
        assert!(v5m.y_max_c >= 100.0);
        assert!(v5m.y_min_c <= 50.0);
        assert!(v5m.y_max_c - v5m.y_min_c >= 40.0);
    }

    #[test]
    fn empty_and_missing_sensor_values_produce_clean_defaults() {
        let mut hist = TemperatureHistory::new();
        let empty = hist.window(HistoryWindow::Sec60);
        assert!(empty.samples.is_empty());
        assert_eq!(empty.cpu_peak_c, None);
        assert_eq!(empty.gpu_peak_c, None);
        assert_eq!((empty.y_min_c, empty.y_max_c), (30.0, 90.0));

        // Non-finite and out-of-range temperatures are ignored cleanly.
        hist.push_at(Duration::from_secs(1), Some(f32::NAN), Some(-10.0));
        hist.push_at(Duration::from_secs(2), Some(68.5), None);
        let view = hist.window(HistoryWindow::Sec60);
        assert_eq!(view.cpu_peak_c, Some(68.5));
        assert_eq!(view.cpu_current_c, Some(68.5));
        assert_eq!(view.gpu_peak_c, None);
        assert_eq!(view.gpu_current_c, None);
    }

    #[test]
    fn axis_ticks_are_duration_only_never_wall_clock() {
        assert_eq!(
            HistoryWindow::Sec60.x_axis_ticks(),
            ["-1:00", "-0:45", "-0:30", "-0:15", "0:00"]
        );
        assert_eq!(
            HistoryWindow::Min5.x_axis_ticks(),
            ["-5:00", "-3:45", "-2:30", "-1:15", "0:00"]
        );
        assert_eq!(format_duration_mm_ss(0), "0:00");
        assert_eq!(format_duration_mm_ss(75), "1:15");
        assert_eq!(format_duration_mm_ss(300), "5:00");
    }

    #[test]
    fn current_metrics_extracts_cpu_ghz_and_gpu_util_from_snapshot() {
        let snapshot = Snapshot {
            cpu_hardware_status: CpuHardwareStatus::Active,
            cpu: Some(CpuSample {
                busy_percent: Some(34.2),
                package_power_w: Some(48.3),
                temperature_c: Some(74.5),
                hardware: Some(CpuHardwareSample {
                    schema: 1,
                    source: CpuHardwareSource::PawnioAmdRapl,
                    temperature_kind: CpuTemperatureKind::AmdTctl,
                    package_power_w: Some(48.3),
                    temperature_c: Some(74.5),
                    power_interval_ms: Some(1000),
                    effective_clocks: Some(CpuEffectiveClocks {
                        source: CpuClockSource::AmdAperfReadOnly,
                        logical_processor_count: 2,
                        processors: vec![
                            CpuLogicalClock {
                                logical_processor: 0,
                                effective_mhz: 2100.0,
                                interval_ms: 1000,
                            },
                            CpuLogicalClock {
                                logical_processor: 1,
                                effective_mhz: 3840.0,
                                interval_ms: 1000,
                            },
                        ],
                    }),
                    warnings: vec![],
                }),
                ..Default::default()
            }),
            gpu: Some(GpuSample {
                name: "RTX 5070 Laptop GPU".into(),
                temperature_c: Some(66),
                clock_mhz: Some(2115),
                power_w: Some(87.4),
                utilization_percent: Some(78),
                ..Default::default()
            }),
            ..Default::default()
        };

        let m = CurrentMetrics::from_snapshot(&snapshot);
        assert_eq!(m.cpu_temp_c, Some(74.5));
        assert_eq!(m.cpu_watts, Some(48.3));
        assert!((m.cpu_clock_ghz.unwrap() - 3.84).abs() < 1e-4);
        assert!((m.cpu_util_pct.unwrap() - 34.2).abs() < 1e-4);
        assert_eq!(m.gpu_temp_c, Some(66.0));
        assert!((m.gpu_watts.unwrap() - 87.4).abs() < 1e-4);
        assert_eq!(m.gpu_clock_mhz, Some(2115));
        assert_eq!(m.gpu_util_pct, Some(78));
    }
}
