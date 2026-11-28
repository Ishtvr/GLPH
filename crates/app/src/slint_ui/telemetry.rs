//! Presentation adapter for the live Overview temperature graph and sensor table.
use super::*;
use glph_core::telemetry_history::{
    CurrentMetrics, HistorySample, HistoryWindow, TemperatureHistory,
};
use std::fmt::Write;
use std::time::Duration;

const DASH: &str = "—";

pub(super) struct TelemetryUiState {
    history: TemperatureHistory,
    window: HistoryWindow,
    y_ticks: Rc<VecModel<TelemetryTick>>,
    x_ticks: Rc<VecModel<TelemetryTick>>,
}

impl Default for TelemetryUiState {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryUiState {
    pub(super) fn new() -> Self {
        Self {
            history: TemperatureHistory::new(),
            window: HistoryWindow::Min5,
            y_ticks: Rc::new(VecModel::default()),
            x_ticks: Rc::new(VecModel::default()),
        }
    }

    pub(super) fn attach(&self, w: &AppWindow) {
        let state = w.global::<TelemetryState>();
        state.set_window_index(self.window.index());
        state.set_y_ticks(self.y_ticks.clone().into());
        state.set_x_ticks(self.x_ticks.clone().into());
        sync_ticks(&self.y_ticks, build_y_ticks(50.0, 90.0));
        sync_ticks(&self.x_ticks, build_x_ticks(self.window));
        state.set_cpu_peak_text(format_peak(None).into());
        state.set_gpu_peak_text(format_peak(None).into());
    }

    pub(super) fn select_window(&mut self, w: &AppWindow, index: i32) {
        let next = HistoryWindow::from_index(index);
        self.window = next;
        w.global::<TelemetryState>().set_window_index(next.index());
        self.render_window(w);
    }

    pub(super) fn push_snapshot(&mut self, w: &AppWindow, s: &Snapshot) {
        self.history.push_snapshot(s);
        self.apply_metrics(w, s);
        self.render_window(w);
    }

    /// Seeds a realistic deterministic 5-minute dual-curve history for `--ui-test` mode.
    pub(super) fn seed_test_history(&mut self, w: &AppWindow, s: &Snapshot) {
        self.history.clear();
        let cpu_end = s
            .cpu
            .as_ref()
            .and_then(|c| c.temperature_c)
            .map_or(68.5_f32, |v| v as f32);
        let gpu_end = s
            .gpu
            .as_ref()
            .and_then(|g| g.temperature_c)
            .map_or(64.0_f32, |v| v as f32);
        let total_points = 61usize;
        for i in 0..total_points {
            let t = i as f32 / (total_points - 1) as f32;
            let cpu = if i == total_points - 1 {
                cpu_end
            } else {
                let wave = (t * std::f32::consts::TAU * 2.1).sin() * 6.5
                    + (t * std::f32::consts::TAU * 4.7).cos() * 3.0;
                (58.0 + t * 14.0 + wave).clamp(48.0, 88.0)
            };
            let gpu = if i == total_points - 1 {
                gpu_end
            } else {
                let wave = (t * std::f32::consts::TAU * 1.4 - 0.6).sin() * 4.5
                    + (t * std::f32::consts::TAU * 3.2).cos() * 1.6;
                (53.0 + t * 11.5 + wave).clamp(44.0, 78.0)
            };
            self.history
                .push_at(Duration::from_secs(i as u64 * 5), Some(cpu), Some(gpu));
        }
        self.apply_metrics(w, s);
        self.render_window(w);
    }

    fn apply_metrics(&self, w: &AppWindow, s: &Snapshot) {
        let state = w.global::<TelemetryState>();
        let metrics = CurrentMetrics::from_snapshot(s);

        let cpu_name = s.inventory.cpus.first().map(String::as_str).unwrap_or("");
        let gpu_name = s
            .gpu
            .as_ref()
            .map(|g| g.name.as_str())
            .or_else(|| s.inventory.gpus.first().map(String::as_str))
            .unwrap_or("");
        let summary = match (cpu_name.is_empty(), gpu_name.is_empty()) {
            (false, false) => format!("{cpu_name}  ·  {gpu_name}"),
            (false, true) => cpu_name.to_owned(),
            (true, false) => gpu_name.to_owned(),
            (true, true) => String::new(),
        };
        set_if_changed(
            state.get_hardware_summary(),
            |v| state.set_hardware_summary(v),
            summary,
        );

        let (cpu_t, cpu_valid) = format_hero_temp(metrics.cpu_temp_c);
        set_if_changed(
            state.get_cpu_temp_value(),
            |v| state.set_cpu_temp_value(v),
            cpu_t,
        );
        if state.get_cpu_temp_valid() != cpu_valid {
            state.set_cpu_temp_valid(cpu_valid);
        }

        let (gpu_t, gpu_valid) = format_hero_temp(metrics.gpu_temp_c);
        set_if_changed(
            state.get_gpu_temp_value(),
            |v| state.set_gpu_temp_value(v),
            gpu_t,
        );
        if state.get_gpu_temp_valid() != gpu_valid {
            state.set_gpu_temp_valid(gpu_valid);
        }

        set_if_changed(
            state.get_cpu_power(),
            |v| state.set_cpu_power(v),
            format_watts(metrics.cpu_watts),
        );
        set_if_changed(
            state.get_cpu_clock(),
            |v| state.set_cpu_clock(v),
            format_cpu_clock_ghz(metrics.cpu_clock_ghz),
        );
        set_if_changed(
            state.get_cpu_util(),
            |v| state.set_cpu_util(v),
            format_util_pct(metrics.cpu_util_pct),
        );

        set_if_changed(
            state.get_gpu_power(),
            |v| state.set_gpu_power(v),
            format_watts(metrics.gpu_watts),
        );
        set_if_changed(
            state.get_gpu_clock(),
            |v| state.set_gpu_clock(v),
            format_gpu_clock_mhz(metrics.gpu_clock_mhz),
        );
        set_if_changed(
            state.get_gpu_util(),
            |v| state.set_gpu_util(v),
            format_util_pct(metrics.gpu_util_pct.map(f32::from)),
        );
    }

    fn render_window(&self, w: &AppWindow) {
        let state = w.global::<TelemetryState>();
        let view = self.history.window(self.window);
        let (y_min, y_max) = nice_y_bounds(view.y_min_c, view.y_max_c);

        let cpu = build_series_paths(&view.samples, y_min, y_max, |s| s.cpu_temp_c);
        let gpu = build_series_paths(&view.samples, y_min, y_max, |s| s.gpu_temp_c);

        set_if_changed(
            state.get_cpu_path(),
            |v| state.set_cpu_path(v),
            cpu.line_path,
        );
        set_if_changed(
            state.get_cpu_area_path(),
            |v| state.set_cpu_area_path(v),
            cpu.area_path,
        );
        set_if_changed(
            state.get_gpu_path(),
            |v| state.set_gpu_path(v),
            gpu.line_path,
        );
        set_if_changed(
            state.get_gpu_area_path(),
            |v| state.set_gpu_area_path(v),
            gpu.area_path,
        );

        match cpu.last_point {
            Some((x, y)) => {
                state.set_cpu_has_point(true);
                state.set_cpu_last_x(x);
                state.set_cpu_last_y(y);
            }
            None => state.set_cpu_has_point(false),
        }
        match gpu.last_point {
            Some((x, y)) => {
                state.set_gpu_has_point(true);
                state.set_gpu_last_x(x);
                state.set_gpu_last_y(y);
            }
            None => state.set_gpu_has_point(false),
        }

        set_if_changed(
            state.get_cpu_peak_text(),
            |v| state.set_cpu_peak_text(v),
            format_peak(view.cpu_peak_c),
        );
        set_if_changed(
            state.get_gpu_peak_text(),
            |v| state.set_gpu_peak_text(v),
            format_peak(view.gpu_peak_c),
        );

        sync_ticks(&self.y_ticks, build_y_ticks(y_min, y_max));
        sync_ticks(&self.x_ticks, build_x_ticks(self.window));
    }
}

fn set_if_changed(
    current: slint::SharedString,
    set: impl FnOnce(slint::SharedString),
    next: String,
) {
    if current.as_str() != next.as_str() {
        set(next.into());
    }
}

fn sync_ticks(model: &VecModel<TelemetryTick>, next: Vec<TelemetryTick>) {
    if model.row_count() == next.len() {
        for (idx, item) in next.into_iter().enumerate() {
            if model.row_data(idx).as_ref() != Some(&item) {
                model.set_row_data(idx, item);
            }
        }
    } else {
        model.set_vec(next);
    }
}

pub(crate) fn format_watts(value: Option<f32>) -> String {
    match value {
        Some(v) if v.is_finite() && v >= 0.0 => format!("{} W", v.round() as i32),
        _ => DASH.to_owned(),
    }
}

/// CPU clock formatted in GHz with EXACTLY 2 decimal places (localized decimal separator).
pub(crate) fn format_cpu_clock_ghz(value: Option<f32>) -> String {
    match value {
        Some(v) if v.is_finite() && v >= 0.0 => {
            format!("{} GHz", crate::i18n::number(&format!("{v:.2}")))
        }
        _ => DASH.to_owned(),
    }
}

/// GPU clock formatted in whole MHz with readable thousands grouping (e.g. `2 115 MHz`).
pub(crate) fn format_gpu_clock_mhz(value: Option<u32>) -> String {
    match value {
        Some(v) if v >= 1000 => format!("{} {:03} MHz", v / 1000, v % 1000),
        Some(v) => format!("{v} MHz"),
        None => DASH.to_owned(),
    }
}

pub(crate) fn format_util_pct(value: Option<f32>) -> String {
    match value {
        Some(v) if v.is_finite() => format!("{} %", v.round().clamp(0.0, 100.0) as i32),
        _ => DASH.to_owned(),
    }
}

pub(crate) fn format_hero_temp(value: Option<f32>) -> (String, bool) {
    match value {
        Some(v) if v.is_finite() => (format!("{}", v.round() as i32), true),
        _ => (DASH.to_owned(), false),
    }
}

pub(crate) fn format_peak(value: Option<f32>) -> String {
    match value {
        Some(v) if v.is_finite() => format!("{} {} °C", t("telemetry.peak"), v.round() as i32),
        _ => format!("{} {DASH}", t("telemetry.peak")),
    }
}

/// Rounds bounds cleanly to 10 °C grid steps with at least a 40 °C span.
pub(crate) fn nice_y_bounds(raw_min: f32, raw_max: f32) -> (f32, f32) {
    if !raw_min.is_finite() || !raw_max.is_finite() || raw_max <= raw_min {
        return (50.0, 90.0);
    }
    let mut lo = ((raw_min / 10.0).floor() * 10.0).clamp(10.0, 80.0);
    let mut hi = ((raw_max / 10.0).ceil() * 10.0).clamp(lo + 20.0, 110.0);
    while hi - lo < 40.0 {
        if hi < 100.0 {
            hi += 10.0;
        } else if lo > 20.0 {
            lo -= 10.0;
        } else {
            break;
        }
    }
    (lo, hi)
}

pub(crate) fn build_y_ticks(y_min: f32, y_max: f32) -> Vec<TelemetryTick> {
    let span = (y_max - y_min).max(10.0);
    let step = if span > 60.0 { 20.0 } else { 10.0 };
    let mut ticks = Vec::new();
    let mut v = y_min;
    while v <= y_max + 0.01 {
        let pos = ((v - y_min) / span).clamp(0.0, 1.0);
        ticks.push(TelemetryTick {
            position: pos,
            label: format!("{}°", v.round() as i32).into(),
        });
        v += step;
    }
    ticks
}

pub(crate) fn build_x_ticks(window: HistoryWindow) -> Vec<TelemetryTick> {
    let labels = window.x_axis_ticks();
    let count = labels.len().max(2);
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| TelemetryTick {
            position: i as f32 / (count - 1) as f32,
            label: (*label).into(),
        })
        .collect()
}

pub(crate) struct SeriesPath {
    pub(crate) line_path: String,
    pub(crate) area_path: String,
    /// `(x_norm, y_norm)` of the latest valid point in `0..1` (`y_norm`: 0=bottom, 1=top).
    pub(crate) last_point: Option<(f32, f32)>,
}

/// Builds smooth monotone cubic Bézier (`C`) SVG commands (zero overshoot) for a
/// temperature series, cleanly splitting segments across missing (`None`) gaps.
pub(crate) fn build_series_paths(
    samples: &[HistorySample],
    y_min: f32,
    y_max: f32,
    extract: impl Fn(&HistorySample) -> Option<f32>,
) -> SeriesPath {
    let span = (y_max - y_min).max(1.0);
    let mut segments: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut current: Vec<(f32, f32)> = Vec::new();
    let mut last_point = None;

    for sample in samples {
        if let Some(temp) = extract(sample).filter(|v| v.is_finite()) {
            let xn = sample.x_norm.clamp(0.0, 1.0);
            let yn = ((temp - y_min) / span).clamp(0.0, 1.0);
            current.push((xn * 1000.0, (1.0 - yn) * 1000.0));
            last_point = Some((xn, yn));
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }

    let mut line_path = String::new();
    let mut area_path = String::new();

    for seg in segments {
        if seg.len() < 2 {
            continue;
        }
        let seg_cmd = monotone_cubic_segment(&seg);
        if !line_path.is_empty() {
            line_path.push(' ');
        }
        line_path.push_str(&seg_cmd);

        if !area_path.is_empty() {
            area_path.push(' ');
        }
        let first_x = seg[0].0;
        let last_x = seg[seg.len() - 1].0;
        let _ = write!(
            area_path,
            "{seg_cmd} L {last_x:.2} 1000.00 L {first_x:.2} 1000.00 Z"
        );
    }

    SeriesPath {
        line_path,
        area_path,
        last_point,
    }
}

/// Fritsch-Carlson monotone Hermite spline emitted as SVG cubic Bézier (`C`)
/// commands so curves stay smooth without ever overshooting measured temperatures.
fn monotone_cubic_segment(pts: &[(f32, f32)]) -> String {
    let n = pts.len();
    let mut out = String::with_capacity(n * 34);
    let _ = write!(out, "M {:.2} {:.2}", pts[0].0, pts[0].1);
    if n == 2 {
        let _ = write!(out, " L {:.2} {:.2}", pts[1].0, pts[1].1);
        return out;
    }

    let mut dx = vec![0.0_f32; n - 1];
    let mut slope = vec![0.0_f32; n - 1];
    for i in 0..n - 1 {
        let d = (pts[i + 1].0 - pts[i].0).max(0.001);
        dx[i] = d;
        slope[i] = (pts[i + 1].1 - pts[i].1) / d;
    }

    let mut tangents = vec![0.0_f32; n];
    tangents[0] = slope[0];
    tangents[n - 1] = slope[n - 2];
    for i in 1..n - 1 {
        if slope[i - 1] * slope[i] <= 0.0 {
            tangents[i] = 0.0;
        } else {
            tangents[i] = (slope[i - 1] + slope[i]) * 0.5;
        }
    }

    // Clamp tangents for strict monotonicity (Fritsch-Carlson condition).
    for i in 0..n - 1 {
        if slope[i].abs() < 1e-6 {
            tangents[i] = 0.0;
            tangents[i + 1] = 0.0;
        } else {
            let a = tangents[i] / slope[i];
            let b = tangents[i + 1] / slope[i];
            let sum_sq = a * a + b * b;
            if sum_sq > 9.0 {
                let scale = 3.0 / sum_sq.sqrt();
                tangents[i] = scale * a * slope[i];
                tangents[i + 1] = scale * b * slope[i];
            }
        }
    }

    for i in 0..n - 1 {
        let h = dx[i];
        let c1x = pts[i].0 + h / 3.0;
        let c1y = (pts[i].1 + tangents[i] * h / 3.0).clamp(0.0, 1000.0);
        let c2x = pts[i + 1].0 - h / 3.0;
        let c2y = (pts[i + 1].1 - tangents[i + 1] * h / 3.0).clamp(0.0, 1000.0);
        let _ = write!(
            out,
            " C {c1x:.2} {c1y:.2} {c2x:.2} {c2y:.2} {:.2} {:.2}",
            pts[i + 1].0,
            pts[i + 1].1
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_table_cells_per_user_spec() {
        assert_eq!(format_watts(Some(48.4)), "48 W");
        assert_eq!(format_watts(Some(86.6)), "87 W");
        assert_eq!(format_watts(None), "—");

        let cpu_clk = format_cpu_clock_ghz(Some(3.84));
        assert!(
            cpu_clk == "3,84 GHz" || cpu_clk == "3.84 GHz",
            "expected 2-decimal GHz, got {cpu_clk}"
        );
        let cpu_clk_whole = format_cpu_clock_ghz(Some(4.0));
        assert!(
            cpu_clk_whole == "4,00 GHz" || cpu_clk_whole == "4.00 GHz",
            "must keep exactly 2 decimals: {cpu_clk_whole}"
        );
        assert_eq!(format_cpu_clock_ghz(None), "—");

        assert_eq!(format_gpu_clock_mhz(Some(2115)), "2 115 MHz");
        assert_eq!(format_gpu_clock_mhz(Some(850)), "850 MHz");
        assert_eq!(format_gpu_clock_mhz(None), "—");

        assert_eq!(format_util_pct(Some(34.4)), "34 %");
        assert_eq!(format_util_pct(Some(78.0)), "78 %");
        assert_eq!(format_util_pct(None), "—");
    }

    #[test]
    fn x_axis_ticks_are_strictly_duration_never_wall_clock() {
        for window in [HistoryWindow::Sec60, HistoryWindow::Min5] {
            let ticks = build_x_ticks(window);
            assert_eq!(ticks.len(), 5);
            assert!((ticks[0].position - 0.0).abs() < 1e-5);
            assert!((ticks[4].position - 1.0).abs() < 1e-5);
            assert_eq!(ticks[4].label.as_str(), "0:00");
            for tick in &ticks {
                // Must be m:ss or -m:ss (1 colon only, never HH:MM:SS).
                assert_eq!(
                    tick.label.matches(':').count(),
                    1,
                    "bad tick: {}",
                    tick.label
                );
            }
        }
    }

    #[test]
    fn series_paths_split_across_gaps_and_stay_in_viewbox() {
        let samples = vec![
            HistorySample {
                age_s: 60.0,
                x_norm: 0.0,
                cpu_temp_c: Some(60.0),
                gpu_temp_c: None,
            },
            HistorySample {
                age_s: 45.0,
                x_norm: 0.25,
                cpu_temp_c: Some(72.0),
                gpu_temp_c: None,
            },
            HistorySample {
                age_s: 30.0,
                x_norm: 0.5,
                cpu_temp_c: None,
                gpu_temp_c: None,
            },
            HistorySample {
                age_s: 15.0,
                x_norm: 0.75,
                cpu_temp_c: Some(70.0),
                gpu_temp_c: None,
            },
            HistorySample {
                age_s: 0.0,
                x_norm: 1.0,
                cpu_temp_c: Some(75.0),
                gpu_temp_c: None,
            },
        ];
        let path = build_series_paths(&samples, 50.0, 90.0, |s| s.cpu_temp_c);
        assert_eq!(path.line_path.matches('M').count(), 2);
        assert_eq!(path.area_path.matches('Z').count(), 2);
        let (lx, ly) = path.last_point.expect("last point");
        assert!((lx - 1.0).abs() < 1e-5);
        assert!((ly - 0.625).abs() < 1e-5);
    }
}
