//! Smooth 135° gradient surface textures for `ColorLook::Gradient`.
//!
//! Zero pixelation, zero noise, zero dark edge vignettes. Rounded containers
//! use a crisp 1px anti-aliased SDF rounded-rectangle alpha mask so zero pixels
//! ever bleed past rounded borders in either FemtoVG or Software Renderer.

use glph_core::appearance::{ColorLook, Tokens};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

pub struct MaterialTextures {
    pub nav: Image,
    pub canvas: Image,
    pub surface: Image,
}

#[derive(Clone, Copy)]
enum SurfaceKind {
    Nav,
    Canvas,
    Card,
}

pub fn build(tokens: &Tokens) -> MaterialTextures {
    let nav = render_gradient(
        tokens.nav_background_look,
        tokens.nav,
        tokens.nav_secondary,
        SurfaceKind::Nav,
        960,
        600,
    );
    let canvas = render_gradient(
        tokens.background_look,
        tokens.canvas,
        tokens.canvas_secondary,
        SurfaceKind::Canvas,
        960,
        600,
    );
    let surface = render_gradient(
        tokens.background_look,
        tokens.surface,
        tokens.canvas_secondary,
        SurfaceKind::Card,
        768,
        256,
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

#[inline]
fn container_mask(x: u32, y: u32, w: u32, h: u32, kind: SurfaceKind) -> f32 {
    match kind {
        SurfaceKind::Nav => 1.0,
        SurfaceKind::Canvas => sdf_mask(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, 11.0),
        SurfaceKind::Card => sdf_mask(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, 12.0),
    }
}

#[inline]
fn sdf_mask(px: f32, py: f32, w: f32, h: f32, radius: f32) -> f32 {
    let hx = (w * 0.5).max(1.0);
    let hy = (h * 0.5).max(1.0);
    let r = radius.min(hx).min(hy);
    let qx = (px - hx).abs() - (hx - r);
    let qy = (py - hy).abs() - (hy - r);
    let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    (0.5 - dist).clamp(0.0, 1.0)
}

#[inline]
fn to_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

fn render_gradient(
    look: ColorLook,
    base: [u8; 3],
    secondary: [u8; 3],
    kind: SurfaceKind,
    w: u32,
    h: u32,
) -> Image {
    if look != ColorLook::Gradient {
        return empty_image();
    }

    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    let pixels = buffer.make_mut_slice();
    let inv_w = 1.0 / (w.max(1) as f32);
    let inv_h = 1.0 / (h.max(1) as f32);
    let base_alpha = match kind {
        SurfaceKind::Nav | SurfaceKind::Canvas => 0.96_f32,
        SurfaceKind::Card => 0.28_f32,
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
            let t = smoothstep(0.0, 1.0, u * 0.58 + v * 0.42);
            let r = f32::from(base[0]) * (1.0 - t) + f32::from(secondary[0]) * t;
            let g = f32::from(base[1]) * (1.0 - t) + f32::from(secondary[1]) * t;
            let b = f32::from(base[2]) * (1.0 - t) + f32::from(secondary[2]) * t;
            pixels[row_offset + x as usize] = Rgba8Pixel::new(
                to_u8(r),
                to_u8(g),
                to_u8(b),
                to_u8(base_alpha * mask * 255.0),
            );
        }
    }

    Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glph_core::appearance::{Colors, PalettePreset};

    #[test]
    fn gradient_rounded_textures_keep_all_four_corners_transparent() {
        let colors = Colors {
            background_look: ColorLook::Gradient,
            nav_background_look: ColorLook::Gradient,
            ..PalettePreset::Slate.colors()
        };
        let tokens = colors.tokens().unwrap();
        let textures = build(&tokens);

        for img in [&textures.canvas, &textures.surface] {
            let rgba = img.to_rgba8().unwrap();
            let w = rgba.width();
            let h = rgba.height();
            let s = rgba.as_slice();
            assert!(w > 4 && h > 4);
            for &(cx, cy) in &[(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                assert_eq!(s[(cy * w + cx) as usize].a, 0);
            }
        }
    }
}
