//! Local appearance preferences. No OS, controller or startup permissions.
use serde::{Deserialize, Serialize};

/// Visual finish/look selectable per color channel (`Standard` is always index 0).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorLook {
    #[default]
    Standard,
    Metallic,
    Glass,
    Glossy,
    Gradient,
    Soft,
    Neon,
    Matte,
    Contrast,
}

impl ColorLook {
    pub const ALL: [Self; 9] = [
        Self::Standard,
        Self::Metallic,
        Self::Glass,
        Self::Glossy,
        Self::Gradient,
        Self::Soft,
        Self::Neon,
        Self::Matte,
        Self::Contrast,
    ];

    pub const GRADIENT_INDEX: i32 = 4;

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
            Self::Metallic => "settings.look_metallic",
            Self::Glass => "settings.look_glass",
            Self::Glossy => "settings.look_glossy",
            Self::Gradient => "settings.look_gradient",
            Self::Soft => "settings.look_soft",
            Self::Neon => "settings.look_neon",
            Self::Matte => "settings.look_matte",
            Self::Contrast => "settings.look_contrast",
        }
    }
}

fn default_nav_background() -> String {
    "#1A1C24".into()
}
fn default_bg_secondary() -> String {
    "#14213D".into()
}
fn default_nav_bg_secondary() -> String {
    "#121C36".into()
}
fn default_fg_secondary() -> String {
    "#9BA8C2".into()
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
            Self::Slate | Self::Custom => (
                "#18191F", "#22242D", "#F2F3F5", "#7C87F8", "#14213D", "#1B2845", "#A8B0C2",
                "#A855F7",
            ),
            Self::Graphite => (
                "#101318", "#1A1F28", "#ECEEF2", "#7EB9DF", "#162A3E", "#1D3044", "#94A3B8",
                "#38BDF8",
            ),
            Self::Vesper => (
                "#0E0E10", "#1A1715", "#ECE6E6", "#FFC799", "#2B1C12", "#322116", "#C7B299",
                "#F59E0B",
            ),
            Self::Light => (
                "#FFFFFF", "#ECEEF4", "#1C1E26", "#4D57C8", "#DCE6F8", "#D6DFEE", "#475569",
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
    pub background_look: ColorLook,
    #[serde(default)]
    pub nav_background_look: ColorLook,
    pub foreground_look: ColorLook,
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

/// Strict, opaque sRGB. Partial edits must never turn the UI black or transparent.
pub fn parse_hex(value: &str) -> Option<[u8; 3]> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&digits[0..2], 16).ok()?,
        u8::from_str_radix(&digits[2..4], 16).ok()?,
        u8::from_str_radix(&digits[4..6], 16).ok()?,
    ])
}
fn luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|v| {
        let s = f64::from(v) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    });
    linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
}
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
fn mix(a: [u8; 3], b: [u8; 3], amount: f64) -> [u8; 3] {
    let t = amount.clamp(0.0, 1.0);
    std::array::from_fn(|i| {
        (f64::from(a[i]) * (1.0 - t) + f64::from(b[i]) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    })
}
fn adjust_saturation(rgb: [u8; 3], factor: f64) -> [u8; 3] {
    let gray = 0.299 * f64::from(rgb[0]) + 0.587 * f64::from(rgb[1]) + 0.114 * f64::from(rgb[2]);
    std::array::from_fn(|i| {
        (gray + (f64::from(rgb[i]) - gray) * factor)
            .round()
            .clamp(0.0, 255.0) as u8
    })
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
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max < 1e-6 { 0.0 } else { d / max };
    (h, s, max)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0).floor() as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
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

fn apply_look_tone(rgb: [u8; 3], look: ColorLook, is_background: bool) -> [u8; 3] {
    let dark = luminance(rgb) < 0.3;
    match look {
        ColorLook::Standard | ColorLook::Gradient => rgb,
        ColorLook::Metallic => {
            // GTA V automotive metallic lacquer: rich saturated base with deep luster
            // (never washed-out gray steel), lifting pure #000000 just enough to show clearcoat depth.
            if is_background {
                if dark {
                    mix(adjust_saturation(rgb, 1.12), [28, 34, 48], 0.10)
                } else {
                    mix(adjust_saturation(rgb, 1.06), [235, 240, 248], 0.14)
                }
            } else {
                mix(adjust_saturation(rgb, 1.14), [236, 242, 255], 0.10)
            }
        }
        ColorLook::Glass => {
            // Warm milky opal glass (Milchglas): creamy warm undertone instead of cold gray-blue.
            if is_background {
                if dark {
                    mix(rgb, [46, 40, 35], 0.18)
                } else {
                    mix(rgb, [252, 245, 235], 0.28)
                }
            } else {
                let warm_milk = if dark {
                    [252, 242, 228]
                } else {
                    [255, 250, 242]
                };
                mix(rgb, warm_milk, 0.14)
            }
        }
        ColorLook::Glossy => {
            // Piano-lacquer / candy-coat enamel: rich saturated base with deep shadow anchor.
            if is_background {
                if dark {
                    mix(adjust_saturation(rgb, 1.08), [6, 8, 14], 0.16)
                } else {
                    mix(adjust_saturation(rgb, 1.08), [255, 255, 255], 0.12)
                }
            } else {
                adjust_saturation(rgb, 1.18)
            }
        }
        ColorLook::Soft => {
            let lifted = if is_background {
                if dark {
                    mix(rgb, [232, 235, 242], 0.09)
                } else {
                    mix(rgb, [36, 40, 50], 0.05)
                }
            } else {
                mix(rgb, [152, 158, 170], 0.16)
            };
            adjust_saturation(lifted, 0.58)
        }
        ColorLook::Neon => {
            let vivid = adjust_saturation(rgb, 1.45);
            if is_background {
                if dark {
                    mix(vivid, [4, 6, 12], 0.12)
                } else {
                    vivid
                }
            } else {
                mix(vivid, [255, 255, 255], 0.12)
            }
        }
        ColorLook::Matte => {
            let calm = adjust_saturation(rgb, 0.72);
            if is_background {
                if dark {
                    mix(calm, [18, 19, 22], 0.22)
                } else {
                    mix(calm, [224, 222, 218], 0.14)
                }
            } else {
                mix(calm, [196, 194, 188], 0.10)
            }
        }
        ColorLook::Contrast => {
            let sharp = adjust_saturation(rgb, 1.24);
            if is_background {
                if dark {
                    mix(sharp, [0, 0, 0], 0.42)
                } else {
                    mix(sharp, [255, 255, 255], 0.36)
                }
            } else if dark {
                mix(sharp, [0, 0, 0], 0.22)
            } else {
                mix(sharp, [255, 255, 255], 0.22)
            }
        }
    }
}

/// Computes `(hi, mid, lo)` perceptual gradient stops for a surface/chrome color.
/// Uses `mix` against specular white/steel/shadow rather than multiplicative scaling,
/// guaranteeing vivid, authentic materials even on pure `#000000` black or `#FFFFFF` white.
fn material_stops(
    base: [u8; 3],
    secondary: [u8; 3],
    accent: [u8; 3],
    look: ColorLook,
    dark: bool,
    strength: f64,
) -> ([u8; 3], [u8; 3], [u8; 3]) {
    let white = [255, 255, 255];
    let black = [0, 0, 0];
    let steel = [212, 222, 238];
    let frost = [215, 235, 255];
    match look {
        ColorLook::Standard | ColorLook::Soft | ColorLook::Matte | ColorLook::Contrast => {
            (base, base, base)
        }
        ColorLook::Metallic => {
            if dark {
                let hi = mix(base, steel, (0.26 * strength).clamp(0.10, 0.36));
                let mid = mix(base, black, (0.26 * strength).clamp(0.10, 0.36));
                let lo = mix(base, steel, (0.14 * strength).clamp(0.06, 0.22));
                (hi, mid, lo)
            } else {
                let hi = mix(base, white, (0.55 * strength).clamp(0.25, 0.70));
                let mid = mix(base, [130, 142, 162], (0.22 * strength).clamp(0.10, 0.30));
                let lo = mix(base, white, (0.26 * strength).clamp(0.12, 0.40));
                (hi, mid, lo)
            }
        }
        ColorLook::Glass => {
            if dark {
                // Distinct frosted contrast: backdrop stays deep sapphire-tinted while
                // cards/controls (higher strength) lift into crisp icy translucency.
                let hi = mix(
                    mix(base, accent, (0.20 * strength).clamp(0.08, 0.24)),
                    frost,
                    (0.26 * strength).clamp(0.08, 0.34),
                );
                let mid = mix(base, accent, (0.10 * strength).clamp(0.04, 0.14));
                let lo = mix(
                    mix(base, accent, (0.12 * strength).clamp(0.06, 0.18)),
                    black,
                    (0.24 * strength).clamp(0.10, 0.32),
                );
                (hi, mid, lo)
            } else {
                let hi = mix(base, white, (0.82 * strength).clamp(0.45, 0.92));
                let mid = mix(base, frost, (0.30 * strength).clamp(0.12, 0.42));
                let lo = mix(base, accent, (0.12 * strength).clamp(0.05, 0.18));
                (hi, mid, lo)
            }
        }
        ColorLook::Glossy => {
            // High-gloss piano-lacquer: polished top reflection (`hi`), silky mid-sheen (`mid`),
            // deep rich lacquered lower half (`lo`).
            if dark {
                let hi = mix(base, white, (0.30 * strength).clamp(0.12, 0.38));
                let mid = mix(base, white, (0.11 * strength).clamp(0.04, 0.16));
                let lo = mix(base, black, (0.48 * strength).clamp(0.22, 0.60));
                (hi, mid, lo)
            } else {
                let hi = mix(base, white, (0.84 * strength).clamp(0.50, 0.94));
                let mid = mix(base, white, (0.34 * strength).clamp(0.16, 0.46));
                let lo = mix(base, [110, 120, 140], (0.14 * strength).clamp(0.07, 0.22));
                (hi, mid, lo)
            }
        }
        ColorLook::Gradient => {
            let mid = mix(base, secondary, 0.50);
            (base, mid, secondary)
        }
        ColorLook::Neon => {
            let hi = mix(base, accent, (0.18 * strength).clamp(0.08, 0.24));
            let mid = base;
            let lo = mix(base, accent, (0.14 * strength).clamp(0.06, 0.20));
            (hi, mid, lo)
        }
    }
}

fn accent_stops(
    accent: [u8; 3],
    secondary: [u8; 3],
    look: ColorLook,
) -> ([u8; 3], [u8; 3], [u8; 3]) {
    let white = [255, 255, 255];
    let black = [0, 0, 0];
    match look {
        ColorLook::Standard | ColorLook::Soft | ColorLook::Matte | ColorLook::Contrast => {
            (accent, accent, accent)
        }
        ColorLook::Metallic => (
            mix(accent, white, 0.46),
            mix(accent, black, 0.20),
            mix(accent, white, 0.22),
        ),
        ColorLook::Glass => (mix(accent, white, 0.36), accent, mix(accent, black, 0.16)),
        ColorLook::Glossy => (
            mix(accent, white, 0.52),
            mix(accent, white, 0.22),
            mix(accent, black, 0.26),
        ),
        ColorLook::Gradient => (accent, mix(accent, secondary, 0.5), secondary),
        ColorLook::Neon => (
            mix(adjust_saturation(accent, 1.35), white, 0.34),
            adjust_saturation(accent, 1.35),
            mix(adjust_saturation(accent, 1.35), black, 0.10),
        ),
    }
}

fn rim_stops(
    base: [u8; 3],
    line: [u8; 3],
    accent: [u8; 3],
    look: ColorLook,
    dark: bool,
) -> ([u8; 3], [u8; 3]) {
    let white = [255, 255, 255];
    let black = [0, 0, 0];
    match look {
        ColorLook::Metallic => {
            if dark {
                (mix(line, [230, 238, 252], 0.62), mix(base, black, 0.45))
            } else {
                (white, mix(line, [90, 102, 122], 0.48))
            }
        }
        ColorLook::Glass => {
            if dark {
                (mix(line, white, 0.68), mix(line, white, 0.18))
            } else {
                (white, mix(line, accent, 0.18))
            }
        }
        ColorLook::Glossy => {
            if dark {
                (mix(line, white, 0.72), mix(base, white, 0.12))
            } else {
                (white, mix(line, [80, 90, 110], 0.32))
            }
        }
        ColorLook::Neon => (mix(accent, white, 0.28), mix(accent, base, 0.35)),
        ColorLook::Contrast => (line, line),
        _ => (line, line),
    }
}

fn chart_colors(accent: [u8; 3], surface: [u8; 3], dark: bool) -> ([u8; 3], [u8; 3]) {
    let cpu = ensure_contrast(accent, surface, 4.3);
    let (h, s, v) = rgb_to_hsv(cpu);
    // Hue-separate GPU curve ~145 deg away so CPU & GPU lines never collide,
    // staying harmonious with any user accent on dark, OLED black, and light themes.
    let gpu_hue = if s < 0.14 {
        // Near-neutral accent: pair with crisp warm amber
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
    // Precomputed perceptual material stops (work cleanly even on #000000 and #FFFFFF)
    pub canvas_hi: [u8; 3],
    pub canvas_mid: [u8; 3],
    pub canvas_lo: [u8; 3],
    pub surface_hi: [u8; 3],
    pub surface_mid: [u8; 3],
    pub surface_lo: [u8; 3],
    pub control_hi: [u8; 3],
    pub control_mid: [u8; 3],
    pub control_lo: [u8; 3],
    pub nav_hi: [u8; 3],
    pub nav_mid: [u8; 3],
    pub nav_lo: [u8; 3],
    pub accent_hi: [u8; 3],
    pub accent_mid: [u8; 3],
    pub accent_lo: [u8; 3],
    pub rim_hi: [u8; 3],
    pub rim_lo: [u8; 3],
    pub nav_rim_hi: [u8; 3],
    pub nav_rim_lo: [u8; 3],
    // Harmonized telemetry chart colors
    pub chart_cpu: [u8; 3],
    pub chart_gpu: [u8; 3],
    pub chart_grid: [u8; 3],
}
impl Colors {
    pub fn tokens(&self) -> Result<Tokens, String> {
        let invalid = || crate::message::msg("settings.invalid_color", &[]);
        let raw_bg = parse_hex(&self.background).ok_or_else(invalid)?;
        let raw_nav = parse_hex(&self.nav_background).ok_or_else(invalid)?;
        let raw_ink = parse_hex(&self.foreground).ok_or_else(invalid)?;
        let raw_accent = parse_hex(&self.accent).ok_or_else(invalid)?;
        let bg2 = if self.background_look == ColorLook::Gradient {
            parse_hex(&self.background_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.background_secondary).unwrap_or([20, 33, 61])
        };
        let nav2 = if self.nav_background_look == ColorLook::Gradient {
            parse_hex(&self.nav_background_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.nav_background_secondary).unwrap_or([18, 28, 54])
        };
        let fg2 = if self.foreground_look == ColorLook::Gradient {
            parse_hex(&self.foreground_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.foreground_secondary).unwrap_or([155, 168, 194])
        };
        let acc2 = if self.accent_look == ColorLook::Gradient {
            parse_hex(&self.accent_secondary).ok_or_else(invalid)?
        } else {
            parse_hex(&self.accent_secondary).unwrap_or([168, 85, 247])
        };

        let bg = apply_look_tone(raw_bg, self.background_look, true);
        let nav = apply_look_tone(raw_nav, self.nav_background_look, true);
        let ink = apply_look_tone(raw_ink, self.foreground_look, false);
        let accent = apply_look_tone(raw_accent, self.accent_look, false);
        let dark = luminance(bg) < 0.3;
        let nav_dark = luminance(nav) < 0.3;

        let hover = mix(
            bg,
            ink,
            match self.background_look {
                ColorLook::Glass | ColorLook::Glossy => 0.14,
                ColorLook::Contrast => 0.16,
                ColorLook::Matte => 0.09,
                _ => 0.12,
            },
        );
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

        let surface_step = match self.background_look {
            ColorLook::Glass => 0.11,
            ColorLook::Metallic => 0.065,
            ColorLook::Glossy => 0.060,
            ColorLook::Neon => 0.055,
            ColorLook::Contrast => 0.070,
            ColorLook::Matte => 0.030,
            _ => 0.045,
        };
        let control_step = match self.background_look {
            ColorLook::Glass => 0.16,
            ColorLook::Metallic => 0.105,
            ColorLook::Glossy => 0.095,
            ColorLook::Contrast => 0.12,
            ColorLook::Matte => 0.055,
            _ => 0.08,
        };
        let line_amount = match self.background_look {
            ColorLook::Contrast => 0.44,
            ColorLook::Glass => 0.28,
            ColorLook::Metallic => 0.26,
            ColorLook::Glossy => 0.24,
            ColorLook::Neon => 0.26,
            ColorLook::Matte => 0.11,
            ColorLook::Soft => 0.14,
            _ => 0.17,
        };
        let surface = mix(bg, ink, surface_step);
        let control = mix(bg, ink, control_step);
        let line = mix(bg, ink, line_amount);

        // Auto-contrast navigation typography & accent so Light nav + OLED Black canvas stays razor-sharp.
        let nav_ink = ensure_contrast(ink, nav, 4.5);
        let nav_muted_raw = mix(nav, nav_ink, 0.64);
        let nav_muted = if contrast(nav_muted_raw, nav) >= 4.0 {
            nav_muted_raw
        } else {
            ensure_contrast(nav_muted_raw, nav, 4.0)
        };
        let nav_quiet = mix(nav, nav_ink, 0.40);
        let nav_hover = mix(
            nav,
            nav_ink,
            match self.nav_background_look {
                ColorLook::Contrast => 0.16,
                ColorLook::Matte => 0.08,
                _ => 0.11,
            },
        );
        let nav_line_amount = match self.nav_background_look {
            ColorLook::Contrast => 0.40,
            ColorLook::Matte => 0.11,
            _ => 0.16,
        };
        let nav_line = mix(nav, nav_ink, nav_line_amount);
        let nav_accent = ensure_contrast(accent, nav, 3.6);

        let canvas_strength = if self.background_look == ColorLook::Glass {
            0.55
        } else {
            0.85
        };
        let nav_strength = if self.nav_background_look == ColorLook::Glass {
            0.60
        } else if self.nav_background_look == ColorLook::Neon {
            0.70
        } else {
            0.95
        };
        let (canvas_hi, canvas_mid, canvas_lo) =
            material_stops(bg, bg2, accent, self.background_look, dark, canvas_strength);
        let (surface_hi, surface_mid, surface_lo) = material_stops(
            surface,
            mix(surface, bg2, 0.55),
            accent,
            self.background_look,
            dark,
            1.15,
        );
        let (control_hi, control_mid, control_lo) = material_stops(
            control,
            mix(control, bg2, 0.45),
            accent,
            self.background_look,
            dark,
            1.25,
        );
        let (nav_hi, nav_mid, nav_lo) = material_stops(
            nav,
            nav2,
            nav_accent,
            self.nav_background_look,
            nav_dark,
            nav_strength,
        );
        let (accent_hi, accent_mid, accent_lo) = accent_stops(accent, acc2, self.accent_look);
        let (rim_hi, rim_lo) = rim_stops(bg, line, accent, self.background_look, dark);
        let (nav_rim_hi, nav_rim_lo) = rim_stops(
            nav,
            nav_line,
            nav_accent,
            self.nav_background_look,
            nav_dark,
        );
        let (chart_cpu, chart_gpu) = chart_colors(accent, surface, dark);
        let chart_grid = mix(surface, ink, if dark { 0.12 } else { 0.14 });

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
            on_accent: if contrast(accent, [255; 3]) >= contrast(accent, [0; 3]) {
                [255; 3]
            } else {
                [0; 3]
            },
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
            canvas_hi,
            canvas_mid,
            canvas_lo,
            surface_hi,
            surface_mid,
            surface_lo,
            control_hi,
            control_mid,
            control_lo,
            nav_hi,
            nav_mid,
            nav_lo,
            accent_hi,
            accent_mid,
            accent_lo,
            rim_hi,
            rim_lo,
            nav_rim_hi,
            nav_rim_lo,
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
            assert!(contrast(t.accent, t.canvas) >= 3.0);
            assert!(contrast(t.on_accent, t.accent) >= 4.5);
            assert!(contrast(t.nav_ink, t.nav) >= 4.5);
            assert!(contrast(t.nav_accent, t.nav) >= 3.5);
            assert!(contrast(t.chart_cpu, t.surface) >= 4.0);
            assert!(contrast(t.chart_gpu, t.surface) >= 4.0);
            assert_ne!(
                t.canvas, t.nav,
                "preset {palette:?} should distinguish canvas & nav"
            );
        }
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
    fn color_looks_default_to_standard_and_support_legacy_json_and_all_looks_on_black() {
        assert_eq!(ColorLook::ALL[0], ColorLook::Standard);
        assert_eq!(ColorLook::default(), ColorLook::Standard);
        assert_eq!(ColorLook::Gradient.index(), ColorLook::GRADIENT_INDEX);
        // Legacy JSON without look/nav/secondary fields deserializes cleanly.
        let legacy: Colors = serde_json::from_str(
            r##"{"background":"#000000","foreground":"#C5C8CE","accent":"#2D57FF"}"##,
        )
        .unwrap();
        assert_eq!(legacy.background_look, ColorLook::Standard);
        assert_eq!(legacy.nav_background_look, ColorLook::Standard);
        assert_eq!(legacy.foreground_look, ColorLook::Standard);
        assert_eq!(legacy.accent_look, ColorLook::Standard);
        assert!(parse_hex(&legacy.nav_background).is_some());

        for look in ColorLook::ALL {
            let mut c = legacy.clone();
            c.background_look = look;
            c.nav_background_look = look;
            c.foreground_look = look;
            c.accent_look = look;
            c.background_secondary = "#1A2238".into();
            c.nav_background_secondary = "#121C36".into();
            c.foreground_secondary = "#88A4D4".into();
            c.accent_secondary = "#9333EA".into();
            let tokens = c.tokens().unwrap();
            assert_eq!(tokens.background_look, look);
            assert_eq!(tokens.nav_background_look, look);
            assert_eq!(tokens.accent_look, look);
            // Metallic, Glass, Glossy, Neon, Gradient MUST produce visible highlight stops even on #000000!
            if matches!(
                look,
                ColorLook::Metallic
                    | ColorLook::Glass
                    | ColorLook::Glossy
                    | ColorLook::Neon
                    | ColorLook::Gradient
            ) {
                assert_ne!(
                    tokens.surface_hi, tokens.surface_lo,
                    "look {look:?} must produce distinct gradient stops on #000000"
                );
                assert_ne!(
                    tokens.accent_hi, tokens.accent_lo,
                    "look {look:?} must produce distinct accent stops"
                );
            }
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
