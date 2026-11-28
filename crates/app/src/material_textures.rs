//! Procedural optical surface textures for GLPH material looks.
//!
//! Renders subtle, physically inspired material textures (smoked mirror glass,
//! frosted crystalline glass, brushed anodized metal, smooth 135° gradients,
//! subtle neon ambient corner bloom, and fine-tooth matte grain) into cached
//! RGBA `slint::Image` buffers.
//!
//! All textures meant for rounded containers apply an exact anti-aliased
//! signed-distance-field (SDF) rounded-rectangle alpha mask + inner safety
//! inset so zero pixels ever bleed past rounded borders in Slint's software or
//! GPU renderers.

use glph_core::appearance::{ColorLook, Tokens};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

pub struct MaterialTextures {
    pub nav: Image,
    pub canvas: Image,
    pub surface: Image,
}

#[derive(Clone, Copy)]
enum SurfaceKind {
    /// Full-window outer chrome (`border-radius: 0`, full bleed).
    Nav,
    /// Inset `12px`-rounded `workspace` canvas (SDF rounded mask).
    Canvas,
    /// Variable-aspect `10px`-rounded cards (corner-safe feathered SDF mask).
    Card,
}

pub fn build(tokens: &Tokens) -> MaterialTextures {
    let nav = render_surface(
        tokens.nav_background_look,
        tokens.nav,
        tokens.nav_secondary,
        tokens.nav_accent,
        tokens.nav_dark,
        SurfaceKind::Nav,
        640,
        400,
    );
    let canvas = render_surface(
        tokens.background_look,
        tokens.canvas,
        tokens.canvas_secondary,
        tokens.accent,
        tokens.dark,
        SurfaceKind::Canvas,
        536,
        377,
    );
    let surface = render_surface(
        tokens.background_look,
        tokens.surface,
        tokens.surface_lo,
        tokens.accent,
        tokens.dark,
        SurfaceKind::Card,
        480,
        192,
    );
    MaterialTextures {
        nav,
        canvas,
        surface,
    }
}

fn empty_image() -> Image {
    let buf = SharedPixelBuffer::<Rgba8Pixel>::new(1, 1);
    Image::from_rgba8(buf)
}

#[inline]
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic fast 2D hash in `[-1.0, 1.0]` (zero tiling artifacts).
#[inline]
fn hash2d(x: u32, y: u32) -> f32 {
    let mut n = x
        .wrapping_mul(374_761_393)
        .wrapping_add(y.wrapping_mul(668_265_263))
        .wrapping_add(0x9E37_79B9);
    n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
    n ^= n >> 16;
    ((n & 0xFFFF) as f32 / 32767.5) - 1.0
}

/// Exact SDF rounded-rectangle mask in `[0.0, 1.0]` that stays strictly inside
/// the container's rounded border so zero pixels ever peek outside corners.
#[inline]
fn container_mask(x: u32, y: u32, w: u32, h: u32, kind: SurfaceKind) -> f32 {
    match kind {
        SurfaceKind::Nav => 1.0,
        SurfaceKind::Canvas => {
            // Canvas is rendered at ~0.5x logical size; use r=8.5px (~17px at 2x)
            // + 1.5px inset so corners stay strictly inside the 12px Slint radius
            // even under extreme window aspect ratios.
            sdf_mask(
                x as f32 + 0.5,
                y as f32 + 0.5,
                w as f32,
                h as f32,
                8.5,
                1.5,
                2.0,
            )
        }
        SurfaceKind::Card => {
            // Variable-aspect cards: generous 16px SDF radius + 2px inset + soft 8px
            // inner feather makes corner bleed mathematically impossible at any size.
            sdf_mask(
                x as f32 + 0.5,
                y as f32 + 0.5,
                w as f32,
                h as f32,
                16.0,
                2.0,
                8.0,
            )
        }
    }
}

#[inline]
fn sdf_mask(px: f32, py: f32, w: f32, h: f32, radius: f32, inset: f32, feather: f32) -> f32 {
    let hx = (w * 0.5 - inset).max(1.0);
    let hy = (h * 0.5 - inset).max(1.0);
    let r = radius.min(hx).min(hy);
    let qx = (px - w * 0.5).abs() - (hx - r);
    let qy = (py - h * 0.5).abs() - (hy - r);
    let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    ((-dist) / feather).clamp(0.0, 1.0)
}

#[inline]
fn to_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

#[allow(clippy::too_many_arguments)]
fn render_surface(
    look: ColorLook,
    base: [u8; 3],
    secondary: [u8; 3],
    accent: [u8; 3],
    dark: bool,
    kind: SurfaceKind,
    w: u32,
    h: u32,
) -> Image {
    if matches!(
        look,
        ColorLook::Standard | ColorLook::Soft | ColorLook::Contrast
    ) {
        return empty_image();
    }

    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    let pixels = buffer.make_mut_slice();
    let inv_w = 1.0 / (w.max(1) as f32);
    let inv_h = 1.0 / (h.max(1) as f32);
    let card_scale = match kind {
        SurfaceKind::Nav => 0.88_f32,
        SurfaceKind::Canvas => 1.0_f32,
        SurfaceKind::Card => 0.92_f32,
    };

    for y in 0..h {
        let v = (y as f32 + 0.5) * inv_h;
        let row_offset = (y * w) as usize;
        for x in 0..w {
            let mask = container_mask(x, y, w, h, kind);
            if mask <= 0.001 {
                pixels[row_offset + x as usize] = Rgba8Pixel::new(0, 0, 0, 0);
                continue;
            }
            let u = (x as f32 + 0.5) * inv_w;

            let (r, g, b, a) = match look {
                // GLÄNZEND — Sleek architectural smoked mirror glass:
                // Crisp slanted optical window reflections + subtle Fresnel grazing light,
                // zero retro 2005 horizontal white/black bar!
                ColorLook::Glossy => {
                    // ~33° oblique reflection coordinate across the glass pane
                    let d = u * 0.78 + v * 0.54;

                    // 1. Broad primary studio-glass reflection with crisp leading edge & silky falloff
                    let primary = smoothstep(0.24, 0.285, d) * (1.0 - smoothstep(0.36, 0.54, d));

                    // 2. Narrow razor-crisp parallel secondary reflection streak (classic mirror-glass cue)
                    let secondary_streak =
                        smoothstep(0.555, 0.578, d) * (1.0 - smoothstep(0.595, 0.645, d));

                    // 3. Quiet distant low-angle corner reflection for continuous optical depth
                    let far_sheen = smoothstep(0.84, 0.92, d) * (1.0 - smoothstep(0.98, 1.14, d));

                    // 4. Delicate top Fresnel grazing reflection (pure smooth exponential decay)
                    let top_fresnel = (-v * 9.5).exp();

                    // 5. Subtle cool specular glass tint (clean optical silver-white)
                    let refl = if dark {
                        [236.0_f32, 243.0, 255.0]
                    } else {
                        [255.0_f32, 255.0, 255.0]
                    };

                    // Tiny micro-smoothness dither to prevent 8-bit gradient banding on dark glass
                    let dither = hash2d(x, y) * 0.003;
                    let intensity = (primary * if dark { 0.105 } else { 0.34 }
                        + secondary_streak * if dark { 0.088 } else { 0.28 }
                        + far_sheen * if dark { 0.042 } else { 0.16 }
                        + top_fresnel * if dark { 0.075 } else { 0.24 }
                        + dither)
                        * card_scale;

                    // Slight dark mirror depth at the lower half so reflection contrast pops naturally
                    let mirror_depth = smoothstep(0.50, 1.0, v) * if dark { 0.18 } else { 0.06 };
                    if intensity >= 0.003 {
                        (refl[0], refl[1], refl[2], intensity.clamp(0.0, 0.52))
                    } else if dark && mirror_depth > 0.005 {
                        (0.0, 0.0, 0.0, mirror_depth * card_scale)
                    } else {
                        (0.0, 0.0, 0.0, 0.0)
                    }
                }

                // GLASIG — Silky, glossy warm milky glass (Milchglas / opal glass):
                // Zero coarse grain — smooth creamy warm-ivory diffused light bloom +
                // soft glossy diagonal reflection veil for a luminous, warm translucent feel.
                ColorLook::Glass => {
                    // Wide, silky diffused warm-milky light entry from top-left
                    let dx1 = (u - 0.18) * 1.05;
                    let dy1 = (v - 0.12) * 1.25;
                    let warm_bloom = (1.0 - (dx1 * dx1 + dy1 * dy1).sqrt() / 1.15).max(0.0);
                    let warm_bloom = warm_bloom * warm_bloom;

                    // Smooth, glossy milky diagonal reflection (softer & wider than Mirror Glossy)
                    let d = u * 0.72 + v * 0.56;
                    let glossy_veil = smoothstep(0.16, 0.34, d) * (1.0 - smoothstep(0.42, 0.72, d));
                    let soft_rim = (-v * 7.5).exp();

                    // Gentle warm amber-cream ambient warmth in the lower-right depth
                    let dx2 = (u - 0.84) * 1.05;
                    let dy2 = (v - 0.86) * 1.20;
                    let warm_depth = (1.0 - (dx2 * dx2 + dy2 * dy2).sqrt() / 1.05).max(0.0);
                    let warm_depth = warm_depth * warm_depth;

                    // Invisible 8-bit de-banding dither only (zero visible grain!)
                    let dither = hash2d(x, y) * 0.002;

                    // Warm milky pearl & soft warm cream tones
                    let milk_hi = if dark {
                        [255.0_f32, 245.0, 232.0]
                    } else {
                        [255.0_f32, 252.0, 246.0]
                    };
                    let milk_warm = if dark {
                        [246.0_f32, 224.0, 198.0]
                    } else {
                        [252.0_f32, 240.0, 224.0]
                    };

                    let w_hi = (warm_bloom * if dark { 0.125 } else { 0.34 }
                        + glossy_veil * if dark { 0.095 } else { 0.26 }
                        + soft_rim * if dark { 0.070 } else { 0.22 }
                        + dither)
                        * card_scale;
                    let w_warm = warm_depth * if dark { 0.085 } else { 0.12 } * card_scale;
                    let total_a = (w_hi + w_warm).clamp(0.0, 0.46);
                    if total_a > 0.002 {
                        let inv = 1.0 / total_a;
                        let r = (milk_hi[0] * w_hi + milk_warm[0] * w_warm) * inv;
                        let g = (milk_hi[1] * w_hi + milk_warm[1] * w_warm) * inv;
                        let b = (milk_hi[2] * w_hi + milk_warm[2] * w_warm) * inv;
                        (r, g, b, total_a)
                    } else {
                        (0.0, 0.0, 0.0, 0.0)
                    }
                }

                // METALLISCH — GTA V-inspired automotive metallic clearcoat paint:
                // Smooth sculptural body-curve metallic sheen, subtle pearlescent accent
                // color-shift along the highlight ridge, deep glossy paint depth, and
                // ultra-fine metallic paint flake sparkle strictly inside the lit ridge.
                ColorLook::Metallic => {
                    // Smooth curved automotive-body highlight ridge (sculptural arc, not flat stripes)
                    let curve = u * 0.68 + v * 0.56 + 0.09 * ((u * std::f32::consts::PI).sin());
                    // Broad metallic base-coat curve reflection
                    let broad_sheen =
                        smoothstep(0.12, 0.42, curve) * (1.0 - smoothstep(0.46, 0.82, curve));
                    // Tight clearcoat glossy ridge along the crown of the metallic curve
                    let clearcoat_crown =
                        smoothstep(0.32, 0.42, curve) * (1.0 - smoothstep(0.45, 0.57, curve));
                    // Secondary lower-right pearlescent contour highlight
                    let pearl_contour =
                        smoothstep(0.76, 0.90, curve) * (1.0 - smoothstep(0.94, 1.14, curve));
                    // Smooth upper clearcoat Fresnel gloss
                    let top_gloss = (-v * 8.0).exp();

                    // Ultra-fine automotive metallic paint flakes (tiny glints ONLY where light hits!)
                    let flake_raw = hash2d(x, y);
                    let flake_sparkle = if flake_raw > 0.72 {
                        (flake_raw - 0.72) / 0.28
                    } else {
                        0.0
                    };
                    let flake_lit = flake_sparkle
                        * (broad_sheen * 0.65 + clearcoat_crown * 1.1 + pearl_contour * 0.5);

                    // Pearlescent highlight color: base lifted with luminous silver + subtle accent pearl
                    let base_f = [f32::from(base[0]), f32::from(base[1]), f32::from(base[2])];
                    let acc_f = [
                        f32::from(accent[0]),
                        f32::from(accent[1]),
                        f32::from(accent[2]),
                    ];
                    let pearl_r = (base_f[0] * 0.35 + acc_f[0] * 0.30 + 255.0 * 0.35).min(255.0);
                    let pearl_g = (base_f[1] * 0.35 + acc_f[1] * 0.30 + 255.0 * 0.35).min(255.0);
                    let pearl_b = (base_f[2] * 0.35 + acc_f[2] * 0.30 + 255.0 * 0.35).min(255.0);
                    let spec_r = if dark { 238.0 } else { 255.0 };
                    let spec_g = if dark { 244.0 } else { 255.0 };
                    let spec_b = 255.0;

                    let w_pearl = (broad_sheen * if dark { 0.145 } else { 0.22 }
                        + pearl_contour * if dark { 0.085 } else { 0.14 })
                        * card_scale;
                    let w_spec = (clearcoat_crown * if dark { 0.115 } else { 0.28 }
                        + top_gloss * if dark { 0.065 } else { 0.18 }
                        + flake_lit * if dark { 0.11 } else { 0.14 })
                        * card_scale;

                    let shadow_trough =
                        (1.0 - (broad_sheen + pearl_contour).min(1.0)) * smoothstep(0.45, 1.0, v);
                    let total_light = (w_pearl + w_spec).clamp(0.0, 0.50);
                    if total_light > 0.004 {
                        let inv = 1.0 / total_light;
                        let r = (pearl_r * w_pearl + spec_r * w_spec) * inv;
                        let g = (pearl_g * w_pearl + spec_g * w_spec) * inv;
                        let b = (pearl_b * w_pearl + spec_b * w_spec) * inv;
                        (r, g, b, total_light)
                    } else if dark && shadow_trough > 0.02 {
                        (
                            0.0,
                            0.0,
                            0.0,
                            (shadow_trough * 0.16 * card_scale).clamp(0.0, 0.20),
                        )
                    } else {
                        (0.0, 0.0, 0.0, 0.0)
                    }
                }

                // FARBVERLAUF — Silky 135° diagonal gradient with zero corner bleed or cracks.
                ColorLook::Gradient => {
                    let t = smoothstep(0.0, 1.0, u * 0.58 + v * 0.42);
                    let dither = hash2d(x, y) * 0.6;
                    let r = f32::from(base[0]) * (1.0 - t) + f32::from(secondary[0]) * t + dither;
                    let g = f32::from(base[1]) * (1.0 - t) + f32::from(secondary[1]) * t + dither;
                    let b = f32::from(base[2]) * (1.0 - t) + f32::from(secondary[2]) * t + dither;
                    // On cards, blend softly over the card surface so cards stay elevated above canvas
                    let alpha = match kind {
                        SurfaceKind::Nav | SurfaceKind::Canvas => 0.94,
                        SurfaceKind::Card => 0.36,
                    };
                    (r, g, b, alpha)
                }

                // NEON / LEUCHTEND — Subtle corner ambient accent glow keeping center razor-crisp.
                ColorLook::Neon => {
                    let d1 = ((u - 0.08).hypot(v - 0.05) / 0.72).min(1.0);
                    let d2 = ((u - 0.94).hypot(v - 0.95) / 0.68).min(1.0);
                    let g1 = (1.0 - d1) * (1.0 - d1);
                    let g2 = (1.0 - d2) * (1.0 - d2);
                    let alpha =
                        ((g1 * 0.14 + g2 * 0.10) * if dark { 1.0 } else { 0.55 } * card_scale)
                            .clamp(0.0, 0.24);
                    (
                        f32::from(accent[0]),
                        f32::from(accent[1]),
                        f32::from(accent[2]),
                        alpha,
                    )
                }

                // MATT / TIEF — Ultra-subtle velvety soft-touch matte micro-grain (zero shine).
                ColorLook::Matte => {
                    let grain = hash2d(x, y);
                    if grain > 0.0 {
                        let tone = if dark { 210.0 } else { 40.0 };
                        (tone, tone, tone, grain * 0.022 * card_scale)
                    } else {
                        let tone = if dark { 0.0 } else { 255.0 };
                        (tone, tone, tone, (-grain) * 0.024 * card_scale)
                    }
                }

                ColorLook::Standard | ColorLook::Soft | ColorLook::Contrast => (0.0, 0.0, 0.0, 0.0),
            };

            let final_a = (a * mask).clamp(0.0, 1.0);
            pixels[row_offset + x as usize] =
                Rgba8Pixel::new(to_u8(r), to_u8(g), to_u8(b), to_u8(final_a * 255.0));
        }
    }

    Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glph_core::appearance::{Colors, PalettePreset};

    #[test]
    fn rounded_surface_textures_keep_all_four_corners_strictly_transparent() {
        for look in ColorLook::ALL {
            let mut colors = PalettePreset::Slate.colors();
            colors.background_look = look;
            colors.nav_background_look = look;
            let tokens = colors.tokens().unwrap();
            let textures = build(&tokens);

            // Canvas & Card corners must have alpha == 0 so zero pixels can bleed past border-radius.
            for img in [&textures.canvas, &textures.surface] {
                if let Some(rgba) = img.to_rgba8() {
                    let w = rgba.width();
                    let h = rgba.height();
                    let s = rgba.as_slice();
                    if w > 4 && h > 4 {
                        for &(cx, cy) in &[(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                            assert_eq!(
                                s[(cy * w + cx) as usize].a,
                                0,
                                "corner ({cx},{cy}) leaked alpha on look {look:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn glossy_and_glass_produce_distinct_visible_textures_even_on_pure_black() {
        let glossy = Colors {
            background: "#000000".into(),
            background_look: ColorLook::Glossy,
            ..Colors::default()
        };
        let glossy_img = build(&glossy.tokens().unwrap()).canvas;

        let glass = Colors {
            background: "#000000".into(),
            background_look: ColorLook::Glass,
            ..Colors::default()
        };
        let glass_img = build(&glass.tokens().unwrap()).canvas;

        let g_buf = glossy_img.to_rgba8().unwrap();
        let f_buf = glass_img.to_rgba8().unwrap();
        assert!(g_buf.as_slice().iter().any(|p| p.a > 12));
        assert!(f_buf.as_slice().iter().any(|p| p.a > 12));
        assert_ne!(g_buf.as_slice(), f_buf.as_slice());
    }
}
