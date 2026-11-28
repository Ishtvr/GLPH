//! Local appearance preferences. No OS, controller or startup permissions.
use serde::{Deserialize, Serialize};

/// Visual finish/look selectable per color channel (`Standard` is always index 0).
/// Legacy look variants deserialize cleanly to `Standard`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorLook {
    #[default]
    #[serde(
        alias = "metallic",
        alias = "glass",
        alias = "glossy",
        alias = "soft",
        alias = "neon",
        alias = "matte",
        alias = "contrast"
    )]
    Standard,
    Gradient,
}

impl ColorLook {
    pub const ALL: [Self; 2] = [Self::Standard, Self::Gradient];

    pub const GRADIENT_INDEX: i32 = 1;

    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|&v| v == self).unwrap_or(0) as i32
    }

    pub fn from_index(index: i32) -> Self {
        usize::try_from(index)
            .ok()
            .and_then(|i| Self::ALL.get(i).copied())
            .unwrap_or(Self::Standard)
    }

    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Standard => "settings.look_standard",
            Self::Gradient => "settings.look_gradient",
        }
    }
}

fn default_nav_background() -> String {
    "#000000".into()
}
fn default_bg_secondary() -> String {
    "#14213D".into()
}
fn default_nav_bg_secondary() -> String {
    "#1B2845".into()
}
fn default_fg_secondary() -> String {
    "#A8B0C2".into()
}
fn default_accent_secondary() -> String {
    "#A855F7".into()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PalettePreset {
    #[default]
    Slate,
    Graphite,
    Vesper,
    Light,
    Custom,
}
impl PalettePreset {
    pub const ALL: [Self; 5] = [
        Self::Slate,
        Self::Graphite,
        Self::Vesper,
        Self::Light,
        Self::Custom,
    ];
    pub fn colors(self) -> Colors {
        let (background, nav_background, foreground, accent, bg2, nav2, fg2, acc2) = match self {
            // Default Antigravity monochrome palette (matches user's preferred settings).
            Self::Slate | Self::Custom => (
                "#242424", "#000000", "#F2F3F5", "#000000", "#14213D", "#1B2845", "#A8B0C2",
                "#A855F7",
            ),
            Self::Graphite => (
                "#11141B", "#080A0E", "#ECEEF2", "#7EB9DF", "#162A3E", "#1D3044", "#94A3B8",
                "#38BDF8",
            ),
            Self::Vesper => (
                "#121110", "#080808", "#ECE6E6", "#FFC799", "#2B1C12", "#322116", "#C7B299",
                "#F59E0B",
            ),
            Self::Light => (
                "#FFFFFF", "#F0F2F6", "#1C1E26", "#4D57C8", "#DCE6F8", "#D6DFEE", "#475569",
                "#7C3AED",
            ),
        };
        Colors {
            background: background.into(),
            nav_background: nav_background.into(),
            foreground: foreground.into(),
            accent: accent.into(),
            background_look: ColorLook::Standard,
            nav_background_look: ColorLook::Standard,
            foreground_look: ColorLook::Standard,
            accent_look: ColorLook::Standard,
            background_secondary: bg2.into(),
            nav_background_secondary: nav2.into(),
            foreground_secondary: fg2.into(),
            accent_secondary: acc2.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub background: String,
    #[serde(default = "default_nav_background")]
    pub nav_background: String,
    pub foreground: String,
    pub accent: String,
    #[serde(default)]
    pub background_look: ColorLook,
    #[serde(default)]
    pub nav_background_look: ColorLook,
    #[serde(default)]
    pub foreground_look: ColorLook,
    #[serde(default)]
    pub accent_look: ColorLook,
    #[serde(default = "default_bg_secondary")]
    pub background_secondary: String,
    #[serde(default = "default_nav_bg_secondary")]
    pub nav_background_secondary: String,
    #[serde(default = "default_fg_secondary")]
    pub foreground_secondary: String,
    #[serde(default = "default_accent_secondary")]
    pub accent_secondary: String,
}
impl Default for Colors {
    fn default() -> Self {
        PalettePreset::Slate.colors()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub palette: PalettePreset,
    pub custom: Colors,
}

pub fn parse_hex(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ])
}

fn channel(v: u8) -> f64 {
    let s = f64::from(v) / 255.0;
    if s <= 0.03928 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

pub fn luminance(rgb: [u8; 3]) -> f64 {
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (hi, lo) = (luminance(a), luminance(b));
    let (hi, lo) = if hi >= lo { (hi, lo) } else { (lo, hi) };
    (hi + 0.05) / (lo + 0.05)
}

fn mix(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let f = |x, y| {
        (f64::from(x) * (1.0 - t) + f64::from(y) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

fn rgb_to_hsv(rgb: [u8; 3]) -> (f64, f64, f64) {
    let r = f64::from(rgb[0]) / 255.0;
    let g = f64::from(rgb[1]) / 255.0;
    let b = f64::from(rgb[2]) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d < 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * (((b - r) / d) + 2.0)
    } else {
        60.0 * (((r - g) / d) + 4.0)
    };
    let s = if max < 1e-6 { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to_u8 = |ch: f64| ((ch + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to_u8(r1), to_u8(g1), to_u8(b1)]
}

fn ensure_contrast(fg: [u8; 3], bg: [u8; 3], min_ratio: f64) -> [u8; 3] {
    if contrast(fg, bg) >= min_ratio {
        return fg;
    }
    let target = if contrast([255, 255, 255], bg) >= contrast([12, 14, 20], bg) {
        [255, 255, 255]
    } else {
        [12, 14, 20]
    };
    let mut best = fg;
    for step in 1..=20 {
        let candidate = mix(fg, target, f64::from(step) / 20.0);
        best = candidate;
        if contrast(candidate, bg) >= min_ratio {
            return candidate;
        }
    }
    best
}

fn best_on_color(bg: [u8; 3]) -> [u8; 3] {
    if contrast([0, 0, 0], bg) >= contrast([255, 255, 255], bg) {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

fn chart_colors(accent: [u8; 3], surface: [u8; 3], dark: bool) -> ([u8; 3], [u8; 3]) {
    let cpu = ensure_contrast(accent, surface, 4.3);
    let (h, s, v) = rgb_to_hsv(cpu);
    let gpu_hue = if s < 0.14 {
        34.0
    } else {
        (h + 145.0).rem_euclid(360.0)
    };
    let gpu_sat = s.clamp(0.48, 0.80);
    let gpu_val = if dark {
        v.clamp(0.76, 0.94)
    } else {
        v.clamp(0.42, 0.66)
    };
    let gpu = ensure_contrast(hsv_to_rgb(gpu_hue, gpu_sat, gpu_val), surface, 4.3);
    (cpu, gpu)
}

#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    pub canvas: [u8; 3],
    pub surface: [u8; 3],
    pub control: [u8; 3],
    pub hover: [u8; 3],
    pub line: [u8; 3],
    pub ink: [u8; 3],
    pub muted: [u8; 3],
    pub quiet: [u8; 3],
    pub accent: [u8; 3],
    pub on_accent: [u8; 3],
    /// Contrast-safe accent for sliders, toggles, checkboxes & focus rings even when `accent == #000000`.
    pub ui_accent: [u8; 3],
    pub on_ui_accent: [u8; 3],
    pub dark: bool,
    // Dedicated secondary / navigation chrome channel (auto-contrast-safe)
    pub nav: [u8; 3],
    pub nav_dark: bool,
    pub nav_ink: [u8; 3],
    pub nav_muted: [u8; 3],
    pub nav_quiet: [u8; 3],
    pub nav_line: [u8; 3],
    pub nav_hover: [u8; 3],
    pub nav_accent: [u8; 3],
    // Selected looks & gradient secondary colors
    pub background_look: ColorLook,
    pub nav_background_look: ColorLook,
    pub foreground_look: ColorLook,
    pub accent_look: ColorLook,
    pub canvas_secondary: [u8; 3],
    pub nav_secondary: [u8; 3],
    pub ink_secondary: [u8; 3],
    pub accent_secondary: [u8; 3],
    // Harmonized telemetry chart colors
    pub chart_cpu: [u8; 3],
    pub chart_gpu: [u8; 3],
    pub chart_grid: [u8; 3],
}

impl Colors {
    pub fn tokens(&self) -> Result<Tokens, String> {
        let invalid = || crate::message::msg("settings.invalid_color", &[]);
        let bg = parse_hex(&self.background).ok_or_else(invalid)?;
        let nav = parse_hex(&self.nav_background).ok_or_else(invalid)?;
        let ink = parse_hex(&self.foreground).ok_or_else(invalid)?;
        let accent = parse_hex(&self.accent).ok_or_else(invalid)?;

        let bg2 = if self.background_look == ColorLook::Gradient {
            parse_hex(&self.background_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.background_secondary).unwrap_or([20, 33, 61])
        };
        let nav2 = if self.nav_background_look == ColorLook::Gradient {
            parse_hex(&self.nav_background_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.nav_background_secondary).unwrap_or([27, 40, 69])
        };
        let fg2 = if self.foreground_look == ColorLook::Gradient {
            parse_hex(&self.foreground_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.foreground_secondary).unwrap_or([168, 176, 194])
        };
        let acc2 = if self.accent_look == ColorLook::Gradient {
            parse_hex(&self.accent_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.accent_secondary).unwrap_or([168, 85, 247])
        };

        let dark = luminance(bg) < 0.3;
        let nav_dark = luminance(nav) < 0.3;

        // Antigravity-inspired quiet monochrome/tonal steps & whisper hairline borders.
        let surface = mix(bg, ink, 0.038);
        let control = mix(bg, ink, 0.068);
        let hover = mix(bg, ink, 0.105);
        let line = mix(bg, ink, 0.085);

        if contrast(ink, hover) < 2.0 {
            return Err(crate::message::msg("settings.color_contrast", &[]));
        }
        let muted = if self.foreground_look == ColorLook::Gradient {
            if contrast(fg2, hover) >= 2.2 {
                fg2
            } else {
                mix(ink, fg2, 0.45)
            }
        } else {
            let m = mix(bg, ink, 0.68);
            if contrast(m, hover) >= 4.5 { m } else { ink }
        };

        let on_accent = best_on_color(accent);
        let ui_accent = ensure_contrast(accent, surface, 3.6);
        let on_ui_accent = best_on_color(ui_accent);

        // Auto-contrast navigation typography & accent.
        let nav_ink = ensure_contrast(ink, nav, 4.5);
        let nav_muted_raw = mix(nav, nav_ink, 0.64);
        let nav_muted = if contrast(nav_muted_raw, nav) >= 4.0 {
            nav_muted_raw
        } else {
            ensure_contrast(nav_muted_raw, nav, 4.0)
        };
        let nav_quiet = mix(nav, nav_ink, 0.40);
        let nav_hover = mix(nav, nav_ink, 0.11);
        let nav_line = mix(nav, nav_ink, 0.085);
        let nav_accent = ensure_contrast(accent, nav, 3.6);

        let (chart_cpu, chart_gpu) = chart_colors(accent, surface, dark);
        let chart_grid = mix(surface, ink, 0.09);

        Ok(Tokens {
            canvas: bg,
            surface,
            control,
            hover,
            line,
            ink,
            muted,
            quiet: mix(bg, ink, 0.42),
            accent,
            on_accent,
            ui_accent,
            on_ui_accent,
            dark,
            nav,
            nav_dark,
            nav_ink,
            nav_muted,
            nav_quiet,
            nav_line,
            nav_hover,
            nav_accent,
            background_look: self.background_look,
            nav_background_look: self.nav_background_look,
            foreground_look: self.foreground_look,
            accent_look: self.accent_look,
            canvas_secondary: bg2,
            nav_secondary: nav2,
            ink_secondary: fg2,
            accent_secondary: acc2,
            chart_cpu,
            chart_gpu,
            chart_grid,
        })
    }
}

impl Appearance {
    pub fn colors(&self) -> Colors {
        if self.palette == PalettePreset::Custom {
            self.custom.clone()
        } else {
            self.palette.colors()
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        self.custom.tokens()?;
        self.colors().tokens()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palettes_keep_text_and_focus_readable_on_controls() {
        for palette in PalettePreset::ALL {
            let t = palette.colors().tokens().unwrap();
            assert!(contrast(t.ink, t.hover) >= 4.5);
            assert!(contrast(t.muted, t.hover) >= 4.5);
            assert!(contrast(t.ui_accent, t.surface) >= 3.5);
            assert!(contrast(t.on_accent, t.accent) >= 4.5);
            assert!(contrast(t.on_ui_accent, t.ui_accent) >= 4.5);
            assert!(contrast(t.nav_ink, t.nav) >= 4.5);
            assert!(contrast(t.nav_accent, t.nav) >= 3.5);
            assert!(contrast(t.chart_cpu, t.surface) >= 4.0);
            assert!(contrast(t.chart_gpu, t.surface) >= 4.0);
            assert_ne!(
                t.canvas, t.nav,
                "preset {palette:?} should distinguish canvas & nav"
            );
        }
        // Verify default matches user's preferred Antigravity monochrome palette.
        let def = Colors::default();
        assert_eq!(def.background, "#242424");
        assert_eq!(def.nav_background, "#000000");
        assert_eq!(def.foreground, "#F2F3F5");
        assert_eq!(def.accent, "#000000");
    }

    #[test]
    fn invalid_or_unreadable_custom_colors_do_not_replace_the_last_preview() {
        for value in ["", "#12", "ffffff", "#000000ff", "#GG0000", "#é1234"] {
            assert!(parse_hex(value).is_none());
        }
        assert_eq!(parse_hex("#aB12fF"), Some([171, 18, 255]));
        let mut colors = Colors::default();
        colors.foreground.clone_from(&colors.background);
        assert!(colors.tokens().is_err());
        let a = Appearance {
            palette: PalettePreset::Custom,
            custom: colors,
        };
        assert!(a.validate().is_err());

        let dark_blue_accent = Appearance {
            palette: PalettePreset::Custom,
            custom: Colors {
                background: "#08090A".into(),
                nav_background: "#16181F".into(),
                foreground: "#F2F3F5".into(),
                accent: "#020D7D".into(),
                ..Colors::default()
            },
        };
        assert!(dark_blue_accent.validate().is_ok());
        let tokens = dark_blue_accent.colors().tokens().unwrap();
        assert_eq!(tokens.accent, [2, 13, 125]);
        assert_eq!(tokens.on_accent, [255, 255, 255]);
    }

    #[test]
    fn color_looks_only_expose_standard_and_gradient_and_migrate_legacy_json() {
        assert_eq!(ColorLook::ALL, [ColorLook::Standard, ColorLook::Gradient]);
        assert_eq!(ColorLook::default(), ColorLook::Standard);
        assert_eq!(ColorLook::Gradient.index(), ColorLook::GRADIENT_INDEX);

        let legacy: Colors = serde_json::from_str(
            r##"{"background":"#000000","foreground":"#C5C8CE","accent":"#2D57FF"}"##,
        )
        .unwrap();
        assert_eq!(legacy.background_look, ColorLook::Standard);
        assert!(parse_hex(&legacy.nav_background).is_some());

        for removed in [
            "\"metallic\"",
            "\"glass\"",
            "\"glossy\"",
            "\"soft\"",
            "\"neon\"",
            "\"matte\"",
            "\"contrast\"",
        ] {
            let parsed: ColorLook = serde_json::from_str(removed).unwrap();
            assert_eq!(parsed, ColorLook::Standard);
        }
    }

    #[test]
    fn mixed_light_nav_and_dark_canvas_keeps_nav_text_and_accent_legible() {
        let mixed = Colors {
            background: "#000000".into(),
            nav_background: "#F4F6FA".into(),
            foreground: "#F2F3F5".into(),
            accent: "#B8C4FF".into(),
            ..Colors::default()
        };
        let t = mixed.tokens().unwrap();
        assert!(t.dark);
        assert!(!t.nav_dark);
        assert!(contrast(t.nav_ink, t.nav) >= 4.5);
        assert!(contrast(t.nav_muted, t.nav) >= 4.0);
        assert!(contrast(t.nav_accent, t.nav) >= 3.5);
    }
}
