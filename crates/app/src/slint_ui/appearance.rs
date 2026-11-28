//! Appearance is a local draft. Invalid intermediate hex never repaints or saves.
use super::*;
use glph_core::appearance::{Appearance, ColorLook, Colors, PalettePreset, Tokens};

pub(crate) fn paint(theme: Theme<'_>, tokens: Tokens) {
    let color = |c: [u8; 3]| slint::Color::from_rgb_u8(c[0], c[1], c[2]);
    theme.on_tr(|key| t(key.as_str()).into());
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

    theme.set_canvas_hi(color(tokens.canvas_hi));
    theme.set_canvas_mid(color(tokens.canvas_mid));
    theme.set_canvas_lo(color(tokens.canvas_lo));
    theme.set_surface_hi(color(tokens.surface_hi));
    theme.set_surface_mid(color(tokens.surface_mid));
    theme.set_surface_lo(color(tokens.surface_lo));
    theme.set_control_hi(color(tokens.control_hi));
    theme.set_control_mid(color(tokens.control_mid));
    theme.set_control_lo(color(tokens.control_lo));
    theme.set_nav_hi(color(tokens.nav_hi));
    theme.set_nav_mid(color(tokens.nav_mid));
    theme.set_nav_lo(color(tokens.nav_lo));
    theme.set_accent_hi(color(tokens.accent_hi));
    theme.set_accent_mid(color(tokens.accent_mid));
    theme.set_accent_lo(color(tokens.accent_lo));
    theme.set_rim_hi(color(tokens.rim_hi));
    theme.set_rim_lo(color(tokens.rim_lo));
    theme.set_nav_rim_hi(color(tokens.nav_rim_hi));
    theme.set_nav_rim_lo(color(tokens.nav_rim_lo));

    theme.set_chart_cpu(color(tokens.chart_cpu));
    theme.set_chart_gpu(color(tokens.chart_gpu));
    theme.set_chart_grid(color(tokens.chart_grid));

    let textures = crate::material_textures::build(&tokens);
    theme.set_nav_texture(textures.nav);
    theme.set_canvas_texture(textures.canvas);
    theme.set_surface_texture(textures.surface);
}
impl Host {
    pub(super) fn show_appearance(&self, value: Appearance) {
        if let Some(w) = self.window.upgrade() {
            let d = w.global::<Workbench>();
            let colors = value.colors();
            d.set_color_look_choices(strings(ColorLook::ALL.map(|l| t(l.i18n_key()).to_owned())));
            d.set_palette_index(
                PalettePreset::ALL
                    .iter()
                    .position(|p| *p == value.palette)
                    .unwrap_or(0) as i32,
            );
            d.set_color_background(colors.background.into());
            d.set_color_nav_background(colors.nav_background.into());
            d.set_color_foreground(colors.foreground.into());
            d.set_color_accent(colors.accent.into());
            d.set_background_look_index(colors.background_look.index());
            d.set_nav_background_look_index(colors.nav_background_look.index());
            d.set_foreground_look_index(colors.foreground_look.index());
            d.set_accent_look_index(colors.accent_look.index());
            d.set_color_background_secondary(colors.background_secondary.into());
            d.set_color_nav_background_secondary(colors.nav_background_secondary.into());
            d.set_color_foreground_secondary(colors.foreground_secondary.into());
            d.set_color_accent_secondary(colors.accent_secondary.into());
        }
        *self.appearance_draft.borrow_mut() = value;
        self.preview_appearance();
        self.sync_color_fields();
    }
    pub(super) fn preview_appearance(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let d = w.global::<Workbench>();
        match self
            .appearance_draft
            .borrow()
            .validate()
            .and_then(|_| self.appearance_draft.borrow().colors().tokens())
        {
            Ok(tokens) => {
                self.appearance_tokens.set(tokens);
                paint(w.global::<Theme>(), tokens);
                self.paint_about();
                w.window().with_winit_window(|native| {
                    native.set_theme(Some(if tokens.dark {
                        winit::window::Theme::Dark
                    } else {
                        winit::window::Theme::Light
                    }))
                });
                d.set_appearance_error("".into());
            }
            Err(e) => d.set_appearance_error(crate::i18n::error_message(&e).into()),
        }
        self.sync_settings_dirty();
    }
    pub(super) fn paint_about(&self) {
        if let Some(window) = self.about.borrow().as_ref() {
            paint(window.global::<Theme>(), self.appearance_tokens.get());
            window.window().with_winit_window(|native| {
                native.set_theme(Some(if self.appearance_tokens.get().dark {
                    winit::window::Theme::Dark
                } else {
                    winit::window::Theme::Light
                }))
            });
        }
    }
    pub(super) fn select_palette(&self, index: i32) {
        if self.blocked() {
            return;
        }
        if let Some(palette) = usize::try_from(index)
            .ok()
            .and_then(|i| PalettePreset::ALL.get(i))
        {
            let mut appearance = self.appearance_draft.borrow().clone();
            appearance.palette = *palette;
            if *palette != PalettePreset::Custom && appearance.custom.tokens().is_err() {
                appearance.custom = self
                    .baseline_settings
                    .borrow()
                    .as_ref()
                    .map_or_else(Colors::default, |s| s.appearance.custom.clone());
            }
            self.show_appearance(appearance);
        }
    }
    pub(super) fn select_look(&self, channel: i32, look_index: i32) {
        if self.blocked() {
            return;
        }
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let d = w.global::<Workbench>();
        let clamped = ColorLook::from_index(look_index).index();
        match channel {
            0 => d.set_background_look_index(clamped),
            1 => d.set_foreground_look_index(clamped),
            2 => d.set_accent_look_index(clamped),
            3 => d.set_nav_background_look_index(clamped),
            _ => return,
        }
        self.edit_colors();
    }
    pub(super) fn edit_colors(&self) {
        if self.blocked() {
            return;
        }
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let d = w.global::<Workbench>();
        d.set_palette_index(4);
        *self.appearance_draft.borrow_mut() = Appearance {
            palette: PalettePreset::Custom,
            custom: Colors {
                background: d.get_color_background().to_string(),
                nav_background: d.get_color_nav_background().to_string(),
                foreground: d.get_color_foreground().to_string(),
                accent: d.get_color_accent().to_string(),
                background_look: ColorLook::from_index(d.get_background_look_index()),
                nav_background_look: ColorLook::from_index(d.get_nav_background_look_index()),
                foreground_look: ColorLook::from_index(d.get_foreground_look_index()),
                accent_look: ColorLook::from_index(d.get_accent_look_index()),
                background_secondary: d.get_color_background_secondary().to_string(),
                nav_background_secondary: d.get_color_nav_background_secondary().to_string(),
                foreground_secondary: d.get_color_foreground_secondary().to_string(),
                accent_secondary: d.get_color_accent_secondary().to_string(),
            },
        };
        self.preview_appearance();
        self.sync_color_fields();
    }
}
