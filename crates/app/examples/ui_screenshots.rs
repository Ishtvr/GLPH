//! Deterministic headless Slint full-window screenshot harness (1280×800 PNGs).
//!
//! Renders GLPH screens via Slint's software renderer (`MinimalSoftwareWindow`)
//! without UAC, hardware access, or touching the live desktop.
//!
//! Usage:
//!   cargo run -p glph --example ui_screenshots -- [prefix] [out_dir]

#[cfg(not(windows))]
fn main() {
    eprintln!("ui_screenshots is only supported on Windows");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    win::run()
}

#[cfg(windows)]
#[path = "../src/material_textures.rs"]
mod material_textures;

#[cfg(windows)]
mod win {
    use glph_core::app_settings::{TELEMETRY_POLL_OPTIONS_MS, telemetry_poll_label};
    use glph_core::appearance::{Appearance, ColorLook, Colors, PalettePreset, Tokens};
    use glph_core::telemetry_history::{HistoryWindow, TemperatureHistory};
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};
    use slint::{Color, ComponentHandle, ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel};
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::fs::File;
    use std::io::BufWriter;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    slint::include_modules!();

    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 800;
    const DEFAULT_OUT_DIR: &str =
        r"C:\Users\Flo\.gemini\antigravity\brain\a4251786-bfa4-4bb9-949d-8a5d1e16d9a9";

    struct HeadlessPlatform {
        window: Rc<MinimalSoftwareWindow>,
        start: Instant,
    }

    impl Platform for HeadlessPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }

        fn duration_since_start(&self) -> Duration {
            self.start.elapsed()
        }
    }

    fn load_de_dict() -> Rc<HashMap<String, String>> {
        let raw = include_str!("../lang/de.json");
        let map: HashMap<String, String> = serde_json::from_str(raw).expect("valid de.json");
        Rc::new(map)
    }

    fn tr(dict: &HashMap<String, String>, key: &str) -> String {
        dict.get(key).cloned().unwrap_or_else(|| key.to_owned())
    }

    fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
        Rc::new(VecModel::from(
            values.into_iter().map(Into::into).collect::<Vec<_>>(),
        ))
        .into()
    }

    fn color(c: [u8; 3]) -> Color {
        Color::from_rgb_u8(c[0], c[1], c[2])
    }

    fn paint_theme(app: &AppWindow, dict: &Rc<HashMap<String, String>>, tokens: Tokens) {
        let theme = app.global::<Theme>();
        let d = dict.clone();
        theme.on_tr(move |key| tr(&d, key.as_str()).into());
        theme.set_reduce_motion(true);
        theme.set_dark(tokens.dark);
        theme.set_canvas(color(tokens.canvas));
        theme.set_surface(color(tokens.surface));
        theme.set_control(color(tokens.control));
        theme.set_hover(color(tokens.hover));
        theme.set_line(color(tokens.line));
        theme.set_ink(color(tokens.ink));
        theme.set_muted(color(tokens.muted));
        theme.set_quiet(color(tokens.quiet));
        theme.set_accent(color(tokens.accent));
        theme.set_on_accent(color(tokens.on_accent));
        theme.set_ui_accent(color(tokens.ui_accent));
        theme.set_on_ui_accent(color(tokens.on_ui_accent));

        theme.set_nav(color(tokens.nav));
        theme.set_nav_dark(tokens.nav_dark);
        theme.set_nav_ink(color(tokens.nav_ink));
        theme.set_nav_muted(color(tokens.nav_muted));
        theme.set_nav_quiet(color(tokens.nav_quiet));
        theme.set_nav_line(color(tokens.nav_line));
        theme.set_nav_hover(color(tokens.nav_hover));
        theme.set_nav_accent(color(tokens.nav_accent));

        theme.set_background_look(tokens.background_look.index());
        theme.set_nav_background_look(tokens.nav_background_look.index());
        theme.set_foreground_look(tokens.foreground_look.index());
        theme.set_accent_look(tokens.accent_look.index());

        theme.set_canvas_secondary(color(tokens.canvas_secondary));
        theme.set_nav_secondary(color(tokens.nav_secondary));
        theme.set_ink_secondary(color(tokens.ink_secondary));
        theme.set_accent_secondary(color(tokens.accent_secondary));

        theme.set_chart_cpu(color(tokens.chart_cpu));
        theme.set_chart_gpu(color(tokens.chart_gpu));
        theme.set_chart_grid(color(tokens.chart_grid));

        let textures = crate::material_textures::build(&tokens);
        theme.set_nav_texture(textures.nav);
        theme.set_canvas_texture(textures.canvas);
        theme.set_surface_texture(textures.surface);
    }

    fn apply_appearance(
        app: &AppWindow,
        dict: &Rc<HashMap<String, String>>,
        appearance: &Appearance,
    ) {
        let colors = appearance.colors();
        let tokens = colors.tokens().expect("valid appearance tokens");
        paint_theme(app, dict, tokens);

        let wb = app.global::<Workbench>();
        wb.set_color_look_choices(strings(ColorLook::ALL.map(|l| tr(dict, l.i18n_key()))));
        wb.set_palette_index(
            PalettePreset::ALL
                .iter()
                .position(|p| *p == appearance.palette)
                .unwrap_or(0) as i32,
        );
        wb.set_color_background(colors.background.clone().into());
        wb.set_color_nav_background(colors.nav_background.clone().into());
        wb.set_color_foreground(colors.foreground.clone().into());
        wb.set_color_accent(colors.accent.clone().into());
        wb.set_background_look_index(colors.background_look.index());
        wb.set_nav_background_look_index(colors.nav_background_look.index());
        wb.set_foreground_look_index(colors.foreground_look.index());
        wb.set_accent_look_index(colors.accent_look.index());
        wb.set_color_background_secondary(colors.background_secondary.clone().into());
        wb.set_color_nav_background_secondary(colors.nav_background_secondary.clone().into());
        wb.set_color_foreground_secondary(colors.foreground_secondary.clone().into());
        wb.set_color_accent_secondary(colors.accent_secondary.clone().into());

        let parse_or =
            |s: &str, fb: [u8; 3]| color(glph_core::appearance::parse_hex(s).unwrap_or(fb));
        wb.set_background_preview(parse_or(&colors.background, tokens.canvas));
        wb.set_nav_background_preview(parse_or(&colors.nav_background, tokens.nav));
        wb.set_foreground_preview(parse_or(&colors.foreground, tokens.ink));
        wb.set_accent_preview(parse_or(&colors.accent, tokens.accent));
        wb.set_background_secondary_preview(parse_or(
            &colors.background_secondary,
            tokens.canvas_secondary,
        ));
        wb.set_nav_background_secondary_preview(parse_or(
            &colors.nav_background_secondary,
            tokens.nav_secondary,
        ));
        wb.set_foreground_secondary_preview(parse_or(
            &colors.foreground_secondary,
            tokens.ink_secondary,
        ));
        wb.set_accent_secondary_preview(parse_or(
            &colors.accent_secondary,
            tokens.accent_secondary,
        ));

        let palette_choices: Vec<PaletteChoice> = PalettePreset::ALL
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let c = if *p == PalettePreset::Custom {
                    appearance.custom.clone()
                } else {
                    p.colors()
                };
                PaletteChoice {
                    name: match i {
                        0 => tr(dict, "settings.look_standard").into(),
                        1 => tr(dict, "settings.graphite").into(),
                        2 => "Vesper".into(),
                        3 => tr(dict, "settings.light").into(),
                        _ => tr(dict, "settings.custom").into(),
                    },
                    canvas: parse_or(&c.background, [24, 25, 31]),
                    nav: parse_or(&c.nav_background, [34, 36, 45]),
                    ink: parse_or(&c.foreground, [242, 243, 245]),
                    accent: parse_or(&c.accent, [124, 135, 248]),
                }
            })
            .collect();
        wb.set_palette_choices(Rc::new(VecModel::from(palette_choices)).into());
    }

    fn build_smooth_paths(pts: &[(f32, f32)]) -> (String, String) {
        if pts.is_empty() {
            return (String::new(), String::new());
        }
        let vx = |x: f32| (x * 1000.0).clamp(0.0, 1000.0);
        let vy = |y: f32| ((1.0 - y) * 1000.0).clamp(0.0, 1000.0);
        let mut line = String::new();
        let _ = write!(line, "M {:.1} {:.1}", vx(pts[0].0), vy(pts[0].1));
        if pts.len() == 2 {
            let _ = write!(line, " L {:.1} {:.1}", vx(pts[1].0), vy(pts[1].1));
        } else if pts.len() >= 3 {
            let n = pts.len();
            let mut m = vec![0.0_f32; n];
            let mut slopes = Vec::with_capacity(n - 1);
            for i in 0..(n - 1) {
                let dx = (pts[i + 1].0 - pts[i].0).max(1e-5);
                slopes.push((pts[i + 1].1 - pts[i].1) / dx);
            }
            m[0] = slopes[0];
            m[n - 1] = slopes[n - 2];
            for i in 1..(n - 1) {
                if slopes[i - 1] * slopes[i] <= 0.0 {
                    m[i] = 0.0;
                } else {
                    m[i] = (slopes[i - 1] + slopes[i]) * 0.5;
                }
            }
            for i in 0..(n - 1) {
                let dx = pts[i + 1].0 - pts[i].0;
                let c1x = pts[i].0 + dx / 3.0;
                let c1y = (pts[i].1 + m[i] * dx / 3.0).clamp(0.0, 1.0);
                let c2x = pts[i + 1].0 - dx / 3.0;
                let c2y = (pts[i + 1].1 - m[i + 1] * dx / 3.0).clamp(0.0, 1.0);
                let _ = write!(
                    line,
                    " C {:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
                    vx(c1x),
                    vy(c1y),
                    vx(c2x),
                    vy(c2y),
                    vx(pts[i + 1].0),
                    vy(pts[i + 1].1)
                );
            }
        }
        let area = format!(
            "{line} L {:.1} 1000.0 L {:.1} 1000.0 Z",
            vx(pts.last().unwrap().0),
            vx(pts[0].0)
        );
        (line, area)
    }

    fn populate_telemetry_fixture(app: &AppWindow) {
        let mut history = TemperatureHistory::new();
        let total_points = 61usize;
        for i in 0..total_points {
            let t = i as f32 / (total_points - 1) as f32;
            let cpu = {
                let wave = (t * std::f32::consts::TAU * 2.1).sin() * 6.5
                    + (t * std::f32::consts::TAU * 4.7).cos() * 3.0;
                let base = (58.0 + t * 14.0 + wave).clamp(52.0, 82.0);
                if i == total_points - 1 { 72.0 } else { base }
            };
            let gpu = {
                let wave = (t * std::f32::consts::TAU * 1.4 - 0.6).sin() * 4.5
                    + (t * std::f32::consts::TAU * 3.2).cos() * 1.6;
                let base = (53.0 + t * 11.0 + wave).clamp(51.0, 70.0);
                if i >= total_points - 4 {
                    let frac = (i - (total_points - 4)) as f32 / 3.0;
                    base * (1.0 - frac) + 64.0 * frac
                } else {
                    base
                }
            };
            history.push_at(Duration::from_secs(i as u64 * 5), Some(cpu), Some(gpu));
        }

        let view = history.window(HistoryWindow::Min5);
        let (y_min, y_max) = (50.0_f32, 90.0_f32);
        let norm_y = |v: f32| ((v - y_min) / (y_max - y_min)).clamp(0.0, 1.0);

        let cpu_pts: Vec<(f32, f32)> = view
            .samples
            .iter()
            .filter_map(|s| s.cpu_temp_c.map(|v| (s.x_norm, norm_y(v))))
            .collect();
        let gpu_pts: Vec<(f32, f32)> = view
            .samples
            .iter()
            .filter_map(|s| s.gpu_temp_c.map(|v| (s.x_norm, norm_y(v))))
            .collect();
        let (cpu_path, cpu_area) = build_smooth_paths(&cpu_pts);
        let (gpu_path, gpu_area) = build_smooth_paths(&gpu_pts);

        let ts = app.global::<TelemetryState>();
        ts.set_hardware_summary(
            "HP OMEN 16  ·  AMD Ryzen 9 8940HX  ·  NVIDIA GeForce RTX 5070 Laptop GPU".into(),
        );
        ts.set_window_index(1);
        ts.set_cpu_path(cpu_path.into());
        ts.set_cpu_area_path(cpu_area.into());
        ts.set_gpu_path(gpu_path.into());
        ts.set_gpu_area_path(gpu_area.into());
        if let Some(&(x, y)) = cpu_pts.last() {
            ts.set_cpu_has_point(true);
            ts.set_cpu_last_x(x);
            ts.set_cpu_last_y(y);
        }
        if let Some(&(x, y)) = gpu_pts.last() {
            ts.set_gpu_has_point(true);
            ts.set_gpu_last_x(x);
            ts.set_gpu_last_y(y);
        }
        let y_ticks = vec![50, 60, 70, 80, 90]
            .into_iter()
            .map(|deg| TelemetryTick {
                position: (deg as f32 - y_min) / (y_max - y_min),
                label: format!("{deg}°").into(),
            })
            .collect::<Vec<_>>();
        let x_ticks = HistoryWindow::Min5
            .x_axis_ticks()
            .into_iter()
            .enumerate()
            .map(|(i, label)| TelemetryTick {
                position: i as f32 / 4.0,
                label: label.into(),
            })
            .collect::<Vec<_>>();
        ts.set_y_ticks(Rc::new(VecModel::from(y_ticks)).into());
        ts.set_x_ticks(Rc::new(VecModel::from(x_ticks)).into());

        ts.set_cpu_temp_value("72".into());
        ts.set_cpu_temp_valid(true);
        ts.set_cpu_peak_text("Spitze 82 °C".into());
        ts.set_gpu_temp_value("64".into());
        ts.set_gpu_temp_valid(true);
        ts.set_gpu_peak_text("Spitze 70 °C".into());

        ts.set_cpu_power("48 W".into());
        ts.set_cpu_clock("3,84 GHz".into());
        ts.set_cpu_util("34 %".into());
        ts.set_gpu_power("87 W".into());
        ts.set_gpu_clock("2 115 MHz".into());
        ts.set_gpu_util("78 %".into());
    }

    fn key(label: &str, x: f32, y: f32, w: f32, h: f32, zone: usize) -> KeyCap {
        let group = match zone {
            3 => 0,
            2 => 1,
            1 => 2,
            _ => 3,
        };
        KeyCap {
            label: label.into(),
            x,
            y,
            w,
            h,
            zone: zone as i32,
            group,
        }
    }

    fn sample_keyboard_de() -> Vec<KeyCap> {
        let zone = |x: f32| {
            if x < 5.0 {
                2
            } else if x < 10.5 {
                1
            } else {
                0
            }
        };
        let mut keys = vec![key("Esc", 0.0, 0.0, 1.0, 0.8, 2)];
        for i in 0..12 {
            let x = 1.5 + i as f32 * 0.95;
            keys.push(key(&format!("F{}", i + 1), x, 0.0, 0.95, 0.8, zone(x)));
        }
        keys.push(key("Einfg", 13.0, 0.0, 1.0, 0.8, 0));
        keys.push(key("Entf", 14.0, 0.0, 1.0, 0.8, 0));
        let mut row = |labels: &[&str], y: f32, first: f32, last: f32| {
            let mut x = 0.0;
            for (i, label) in labels.iter().enumerate() {
                let w = if i == 0 {
                    first
                } else if i == labels.len() - 1 {
                    last
                } else {
                    1.0
                };
                let z = if matches!(*label, "W" | "A" | "S" | "D") {
                    3
                } else {
                    zone(x)
                };
                keys.push(key(label, x, y, w, 1.0, z));
                x += w;
            }
        };
        row(
            &[
                "^", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "ß", "´", "Back",
            ],
            1.0,
            1.0,
            2.0,
        );
        row(
            &[
                "Tab", "Q", "W", "E", "R", "T", "Z", "U", "I", "O", "P", "Ü", "+",
            ],
            2.0,
            1.5,
            1.0,
        );
        row(
            &[
                "Caps", "A", "S", "D", "F", "G", "H", "J", "K", "L", "Ö", "Ä", "#",
            ],
            3.0,
            1.75,
            0.75,
        );
        row(
            &[
                "Shift", "<", "Y", "X", "C", "V", "B", "N", "M", ",", ".", "-", "Shift",
            ],
            4.0,
            1.25,
            2.75,
        );
        keys.push(key("Enter", 13.5, 2.0, 1.5, 2.0, 0));
        for (label, x, w) in [
            ("Ctrl", 0.0, 1.25),
            ("Fn", 1.25, 1.0),
            ("Alt", 2.25, 1.25),
            ("", 3.5, 6.0),
            ("Alt Gr", 9.5, 1.25),
            ("Ctrl", 10.75, 1.25),
        ] {
            keys.push(key(label, x, 5.1, w, 1.2, zone(x)));
        }
        for (label, x, y) in [
            ("←", 12.0, 5.75),
            ("↑", 13.0, 5.1),
            ("↓", 13.0, 5.75),
            ("→", 14.0, 5.75),
        ] {
            keys.push(key(label, x, y, 1.0, 0.6, 0));
        }
        for (r, labels) in [
            ["Num", "/", "*", "-"],
            ["7", "8", "9", "+"],
            ["4", "5", "6", ""],
            ["1", "2", "3", "Enter"],
        ]
        .iter()
        .enumerate()
        {
            for (c, label) in labels.iter().enumerate() {
                if label.is_empty() {
                    continue;
                }
                keys.push(key(
                    label,
                    16.0 + c as f32,
                    1.0 + r as f32,
                    1.0,
                    if *label == "+" || *label == "Enter" {
                        2.0
                    } else {
                        1.0
                    },
                    0,
                ));
            }
        }
        keys.push(key("0", 16.0, 5.0, 2.0, 1.0, 0));
        keys.push(key(",", 18.0, 5.0, 1.0, 1.0, 0));
        keys
    }

    fn populate_fixture_data(app: &AppWindow, dict: &Rc<HashMap<String, String>>) {
        let wb = app.global::<Workbench>();
        let d = dict.clone();
        wb.on_tr(move |key| tr(&d, key.as_str()).into());

        wb.set_admin(true);
        wb.set_identity("HP OMEN 16 · Board 8D41 · BIOS F.14".into());
        wb.set_power_source("Netzbetrieb (100 %)".into());
        wb.set_gpu_name("NVIDIA GeForce RTX 5070 Laptop GPU".into());
        wb.set_gpu_power("87,0 W".into());
        wb.set_gpu_temp("64,0 °C".into());
        wb.set_gpu_clock("2.115 MHz · Last: 78 %".into());
        wb.set_gpu_tgp("TGP-Limit: 115 W (Max 115 W)".into());
        wb.set_cpu_name("AMD Ryzen 9 8940HX".into());
        wb.set_cpu_power("48,0 W".into());
        wb.set_cpu_temp("72,0 °C".into());
        wb.set_cpu_load("Effektiv max.: 3,84 GHz · Last: 34 %".into());
        wb.set_chassis_temp("Gehäuse (IR): 39 °C · Limit 55 °C".into());
        wb.set_cpu_source("PawnIO · Windows".into());
        wb.set_last_apply("Zuletzt angewendet: Alltag (vor 12 min)".into());

        // Presets
        wb.set_preset_ready(true);
        wb.set_preset_saved(true);
        wb.set_preset_applicable(true);
        wb.set_profiles(strings(vec![
            "Neues Preset …".into(),
            "Alltag".into(),
            "Gaming 115 W".into(),
            "Leise (Akku)".into(),
        ]));
        wb.set_profile_index(1);
        wb.set_preset_name("Alltag".into());
        wb.set_battery_choices(strings(vec!["Aus".into(), "Leise (Akku)".into()]));
        wb.set_battery_index(0);
        wb.set_profile_fan_choices(strings(vec![
            "Kühlung beibehalten".into(),
            "BIOS-Automatik".into(),
            "Leise Kurve".into(),
        ]));
        wb.set_profile_fan_index(0);

        let gpu_rows = Rc::new(VecModel::from(vec![
            FieldData {
                index: 0,
                label: "GPU-Powerlimit (Direkt-TGP)".into(),
                unit: "W".into(),
                included: true,
                value: "115".into(),
                kind: 5,
                choices: ModelRc::default(),
                selected: 0,
                info: "Direktes GPU-Powerlimit.".into(),
                slider_min: 50.0,
                slider_max: 135.0,
                slider_value: 115.0,
            },
            FieldData {
                index: 1,
                label: "GPU-Takt-Offset".into(),
                unit: "MHz".into(),
                included: false,
                value: "0".into(),
                kind: 0,
                choices: ModelRc::default(),
                selected: 0,
                info: "".into(),
                slider_min: 0.0,
                slider_max: 0.0,
                slider_value: 0.0,
            },
        ]));
        let cpu_rows = Rc::new(VecModel::from(vec![FieldData {
            index: 2,
            label: "Langzeit-Limit (SPL)".into(),
            unit: "W".into(),
            included: true,
            value: "65".into(),
            kind: 5,
            choices: ModelRc::default(),
            selected: 0,
            info: "".into(),
            slider_min: 15.0,
            slider_max: 120.0,
            slider_value: 65.0,
        }]));
        wb.set_groups(
            Rc::new(VecModel::from(vec![
                GroupData {
                    section_title: "GPU".into(),
                    title: "GPU-Leistung & Takt".into(),
                    expanded: true,
                    is_per_core: false,
                    rows: gpu_rows.into(),
                },
                GroupData {
                    section_title: "CPU".into(),
                    title: "CPU-Leistungslimits".into(),
                    expanded: false,
                    is_per_core: false,
                    rows: cpu_rows.into(),
                },
            ]))
            .into(),
        );

        // Cooling
        wb.set_fan_index(1);
        wb.set_cooling_status(
            "Eigene Lüfterkurve aktiv (GLPHService · 2.450 / 2.600 U/min)".into(),
        );
        wb.set_fan_library_ready(true);
        wb.set_fan_library_saved(true);
        wb.set_fan_library_names(strings(vec![
            "Leise Kurve".into(),
            "Gaming Kühlung".into(),
            "Maximal".into(),
        ]));
        wb.set_fan_library_index(0);
        wb.set_fan_library_name("Leise Kurve".into());

        // Plugins + HP OMEN nav item
        wb.set_plugin_nav_items(
            Rc::new(VecModel::from(vec![PluginNavData {
                id: "hp-omen-control".into(),
                label: "HP OMEN".into(),
                icon_kind: 8,
                surface_kind: 3,
            }]))
            .into(),
        );
        wb.set_plugins_dir(r"C:\Users\Flo\.glph\plugins".into());
        wb.set_plugins_summary("1 installiert · 1 aktiv".into());
        wb.set_plugins(
            Rc::new(VecModel::from(vec![PluginRowData {
                id: "hp-omen-control".into(),
                folder_name: "hp-omen-control".into(),
                name: "HP OMEN Steuerung".into(),
                description: "4-Zonen-Tastaturbeleuchtung sowie originale HP OMEN Leistungspläne."
                    .into(),
                version: "2.0.0".into(),
                author: "GLPH".into(),
                category_label: "Beleuchtung".into(),
                status_kind: 0,
                status_text: "Aktiv".into(),
                enabled: true,
                can_toggle: true,
                can_remove: true,
                busy: false,
                highlighted: false,
                icon_kind: 2,
                capabilities_text: "HP-Beleuchtung, HP-Leistungsmodi, Seitenleiste".into(),
                diagnostic: "".into(),
            }]))
            .into(),
        );

        // Keyboard / HP OMEN page
        wb.set_active_plugin_id("hp-omen-control".into());
        wb.set_active_plugin_title("HP OMEN".into());
        wb.set_active_plugin_surface_kind(3);
        wb.set_lighting_ready(true);
        wb.set_lighting_on(true);
        wb.set_keyboard_german(true);
        wb.set_keyboard_keys(Rc::new(VecModel::from(sample_keyboard_de())).into());
        wb.set_lighting_group(0);
        wb.set_lighting_color("#2D57FF".into());
        wb.set_lighting_brightness("85".into());
        wb.set_lighting_level(85.0);
        wb.set_zone_colors(
            Rc::new(VecModel::from(vec![
                color([0x4C, 0x8D, 0xFF]),
                color([0x7C, 0x4D, 0xFF]),
                color([0x2D, 0xC6, 0xFF]),
                color([0x2D, 0x57, 0xFF]),
            ]))
            .into(),
        );
        wb.set_omen_mode_index(3);

        // Settings
        wb.set_settings_ready(true);
        wb.set_language_index(1);
        wb.set_autostart(true);
        wb.set_start_in_tray(true);
        wb.set_close_index(0);
        wb.set_hold_preset(true);
        wb.set_poll_rate_choices(strings(
            TELEMETRY_POLL_OPTIONS_MS
                .into_iter()
                .map(|ms| telemetry_poll_label(ms).to_owned()),
        ));
        wb.set_poll_rate_index(1);
        wb.set_gpu_tgp_max_w("135".into());
        wb.set_hw_service_installed(true);
        wb.set_hw_service_running(true);
        wb.set_hw_service_pill_text("Läuft".into());
        wb.set_hw_service_pill_kind(0);
        wb.set_settings_tab_dirty(Rc::new(VecModel::from(vec![false, false, false, false])).into());

        populate_telemetry_fixture(app);
    }

    fn save_png(
        path: &Path,
        width: u32,
        height: u32,
        pixels: &[Rgb8Pixel],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        let mut encoder = png::Encoder::new(writer, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut png_writer = encoder.write_header()?;
        let mut bytes = Vec::with_capacity(pixels.len() * 3);
        for p in pixels {
            bytes.push(p.r);
            bytes.push(p.g);
            bytes.push(p.b);
        }
        png_writer.write_image_data(&bytes)?;
        Ok(())
    }

    fn capture(
        window: &Rc<MinimalSoftwareWindow>,
        buffer: &mut [Rgb8Pixel],
        out_dir: &Path,
        filename: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..2 {
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| {
                renderer.render(buffer, WIDTH as usize);
            });
        }
        let path = out_dir.join(filename);
        save_png(&path, WIDTH, HEIGHT, buffer)?;
        println!("Saved {}", path.display());
        Ok(())
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let prefix = args.first().map(String::as_str).unwrap_or("round1");
        let out_dir = args
            .get(1)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT_DIR));
        std::fs::create_dir_all(&out_dir)?;

        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
        slint::platform::set_platform(Box::new(HeadlessPlatform {
            window: window.clone(),
            start: Instant::now(),
        }))?;

        let dict = load_de_dict();
        let app = AppWindow::new()?;
        app.show()?;
        window.set_size(PhysicalSize::new(WIDTH, HEIGHT));

        populate_fixture_data(&app, &dict);

        let default_appearance = Appearance {
            palette: PalettePreset::Slate,
            custom: Colors::default(),
        };
        apply_appearance(&app, &dict, &default_appearance);

        let mut buffer = vec![Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
        let wb = app.global::<Workbench>();

        // 1. Overview (Slate)
        wb.set_page(0);
        wb.set_curve_open(false);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_overview.png"),
        )?;

        // 2. Settings -> Allgemein
        wb.set_page(4);
        wb.set_settings_tab(0);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_settings_general.png"),
        )?;

        // 3. Settings -> Darstellung
        wb.set_settings_tab(2);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_settings_appearance.png"),
        )?;

        // 4. Settings -> Plugins
        wb.set_settings_tab(1);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_settings_plugins.png"),
        )?;

        // 5. Cooling
        wb.set_page(2);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_cooling.png"),
        )?;

        // 6. Keyboard / HP OMEN
        wb.set_page(3);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_keyboard_omen.png"),
        )?;

        // 7. Theme & Gradient Showcase on Overview
        let looks: &[(&str, Appearance)] = &[
            (
                "gradient",
                Appearance {
                    palette: PalettePreset::Custom,
                    custom: Colors {
                        background_look: ColorLook::Gradient,
                        nav_background_look: ColorLook::Gradient,
                        accent_look: ColorLook::Gradient,
                        background: "#1B1E28".into(),
                        background_secondary: "#12131A".into(),
                        nav_background: "#0B0C10".into(),
                        nav_background_secondary: "#151924".into(),
                        accent: "#4F8BFF".into(),
                        accent_secondary: "#A855F7".into(),
                        ..PalettePreset::Slate.colors()
                    },
                },
            ),
            (
                "graphite",
                Appearance {
                    palette: PalettePreset::Graphite,
                    custom: Colors::default(),
                },
            ),
            (
                "vesper",
                Appearance {
                    palette: PalettePreset::Vesper,
                    custom: Colors::default(),
                },
            ),
            (
                "light",
                Appearance {
                    palette: PalettePreset::Light,
                    custom: Colors::default(),
                },
            ),
        ];

        wb.set_page(0);
        for (name, appearance) in looks {
            apply_appearance(&app, &dict, appearance);
            capture(
                &window,
                &mut buffer,
                &out_dir,
                &format!("{prefix}_look_{name}.png"),
            )?;
        }

        // Capture Settings→Darstellung in Default Antigravity + dirty footer to verify rounded bottom corners
        apply_appearance(&app, &dict, &Appearance::default());
        wb.set_page(4);
        wb.set_settings_tab(2);
        wb.set_settings_dirty(true);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_appearance_dirty.png"),
        )?;

        // Capture Settings→Darstellung in Gradient (showing 2nd color row + clean footer)
        apply_appearance(&app, &dict, &looks[0].1);
        wb.set_page(4);
        wb.set_settings_tab(2);
        wb.set_settings_dirty(true);
        capture(
            &window,
            &mut buffer,
            &out_dir,
            &format!("{prefix}_appearance_gradient.png"),
        )?;
        wb.set_settings_dirty(false);

        Ok(())
    }
}
