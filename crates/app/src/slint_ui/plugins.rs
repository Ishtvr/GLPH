//! Host-side plugin registry, instant-apply lifecycle, and Slint model sync.
use super::*;
use glph_core::plugins::{
    HP_OMEN_CONTROL_PLUGIN_ID, InstalledPlugin, LocalizedText, NavIcon, PLUGIN_API_VERSION,
    PLUGIN_SCHEMA, PluginCapability, PluginCategory, PluginManifest, PluginRuntime, PluginStatus,
    PluginUiSurface, UiSurfaceKind, VendorFilter,
};
use glph_platform::plugin_store::{InstallOutcome, PluginStore};
use std::{path::PathBuf, time::Duration};

pub(super) struct PluginHostState {
    pub(super) installed: Vec<InstalledPlugin>,
    pub(super) pending_install_path: Option<PathBuf>,
    pub(super) pending_remove_id: Option<String>,
    pub(super) highlighted_id: Option<String>,
    pub(super) highlight_timer: slint::Timer,
    pub(super) rows_model: Rc<VecModel<PluginRowData>>,
    pub(super) nav_model: Rc<VecModel<PluginNavData>>,
}

impl Default for PluginHostState {
    fn default() -> Self {
        Self {
            installed: Vec::new(),
            pending_install_path: None,
            pending_remove_id: None,
            highlighted_id: None,
            highlight_timer: slint::Timer::default(),
            rows_model: Rc::new(VecModel::default()),
            nav_model: Rc::new(VecModel::default()),
        }
    }
}

fn format_capabilities(manifest: Option<&PluginManifest>) -> String {
    let Some(m) = manifest else {
        return String::new();
    };
    m.capabilities
        .iter()
        .map(|cap| t(cap.label_key()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn to_row_data(
    plugin: &InstalledPlugin,
    lang: &str,
    highlighted_id: Option<&str>,
) -> PluginRowData {
    let (status_kind, status_text) = match &plugin.status {
        PluginStatus::Active => (0, t("plugins.status_active")),
        PluginStatus::Inactive => (1, t("plugins.status_inactive")),
        PluginStatus::Incompatible(_) => (2, t("plugins.status_incompatible")),
        PluginStatus::Error(_) => (3, t("plugins.status_error")),
    };
    let category_label = plugin
        .manifest
        .as_ref()
        .map(|m| t(m.category.i18n_key()))
        .unwrap_or_else(|| t("common.unknown"));
    let diagnostic = plugin
        .diagnostic
        .as_deref()
        .map(crate::i18n::message)
        .unwrap_or_default();
    let author = plugin
        .manifest
        .as_ref()
        .map(|m| m.author.clone())
        .unwrap_or_default();

    PluginRowData {
        id: plugin.id.clone().into(),
        folder_name: plugin.folder_name.clone().into(),
        name: plugin.display_name(lang).into(),
        description: plugin.display_description(lang).into(),
        version: plugin.version().into(),
        author: author.into(),
        category_label: category_label.into(),
        status_kind,
        status_text: status_text.into(),
        enabled: plugin.enabled,
        can_toggle: plugin.can_toggle,
        can_remove: plugin.can_remove,
        busy: false,
        highlighted: highlighted_id == Some(plugin.id.as_str()),
        icon_kind: plugin.nav_icon().slint_index(),
        capabilities_text: format_capabilities(plugin.manifest.as_ref()).into(),
        diagnostic: diagnostic.into(),
    }
}

pub(super) fn ui_test_fixture_plugins() -> Vec<InstalledPlugin> {
    let omen_manifest = PluginManifest {
        schema: PLUGIN_SCHEMA,
        id: HP_OMEN_CONTROL_PLUGIN_ID.into(),
        name: LocalizedText {
            de: "HP OMEN Steuerung".into(),
            en: "HP OMEN Control".into(),
        },
        description: LocalizedText {
            de: "4-Zonen-Tastaturbeleuchtung sowie originale HP OMEN Leistungspläne (Quiet, Balanced, Performance & Unleashed)."
                .into(),
            en: "4-zone RGB keyboard lighting and original HP OMEN performance plans (Quiet, Balanced, Performance & Unleashed)."
                .into(),
        },
        version: "2.0.0".into(),
        api_version: PLUGIN_API_VERSION,
        author: "GLPH".into(),
        category: PluginCategory::Lighting,
        runtime: PluginRuntime::HostSurface,
        capabilities: vec![
            PluginCapability::HardwareHpLighting,
            PluginCapability::HardwareHpModes,
            PluginCapability::UiSidebarPage,
        ],
        vendor_filter: Some(VendorFilter {
            manufacturers: vec!["HP".into()],
            require_hp_wmi: true,
        }),
        ui_surface: PluginUiSurface {
            kind: UiSurfaceKind::HpOmenSuite,
            nav_label: LocalizedText {
                de: "HP OMEN".into(),
                en: "HP OMEN".into(),
            },
            nav_icon: NavIcon::Omen,
            nav_order: 10,
        },
        entry: None,
    };
    vec![InstalledPlugin {
        id: HP_OMEN_CONTROL_PLUGIN_ID.into(),
        folder_name: HP_OMEN_CONTROL_PLUGIN_ID.into(),
        manifest: Some(omen_manifest),
        status: PluginStatus::Active,
        enabled: true,
        can_toggle: true,
        can_remove: true,
        diagnostic: None,
        pinned_sha256: None,
    }]
}

impl Host {
    pub(super) fn plugin_store(&self) -> Result<PluginStore, String> {
        PluginStore::user_data()
    }

    pub(super) fn current_inventory(&self) -> glph_core::Inventory {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.inventory.clone())
            .unwrap_or_else(crate::inventory::read)
    }

    pub(super) fn lighting_plugin_active(&self) -> bool {
        self.plugins.borrow().installed.iter().any(|p| {
            p.status.is_active()
                && matches!(
                    p.surface_kind(),
                    Some(UiSurfaceKind::Lighting4Zone | UiSurfaceKind::HpOmenSuite)
                )
        })
    }

    pub(super) fn teardown_lighting_plugin(&self) {
        if self
            .color_picker
            .borrow()
            .as_ref()
            .is_some_and(|p| p.is_lighting_target())
        {
            self.close_color_picker(false);
        }
        *self.lighting.borrow_mut() = None;
        *self.lighting_draft.borrow_mut() = None;
        self.lighting_attempted.set(false);
        if let Some(w) = self.window.upgrade() {
            let d = w.global::<Workbench>();
            d.set_lighting_dirty(false);
            d.set_lighting_error("".into());
            d.set_lighting_ready(false);
            if d.get_page() == 3 && matches!(d.get_active_plugin_surface_kind(), 0 | 3) {
                d.set_page(0);
                d.set_active_plugin_id("".into());
                d.set_active_plugin_title("".into());
                d.set_active_plugin_surface_kind(0);
            }
        }
    }

    pub(super) fn sync_plugin_models(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let d = w.global::<Workbench>();
        let lang = crate::i18n::language_code();
        let state = self.plugins.borrow();
        let highlight = state.highlighted_id.as_deref();
        let new_rows: Vec<PluginRowData> = state
            .installed
            .iter()
            .map(|p| to_row_data(p, lang, highlight))
            .collect();

        // Update rows in-place when length matches so FocusScope stays on the
        // toggled ToggleSwitch instead of resetting keyboard focus.
        if state.rows_model.row_count() == new_rows.len() {
            for (idx, row) in new_rows.into_iter().enumerate() {
                state.rows_model.set_row_data(idx, row);
            }
        } else {
            state.rows_model.set_vec(new_rows);
        }

        let nav_items: Vec<PluginNavData> = state
            .installed
            .iter()
            .filter(|p| p.status.is_active() && p.surface_kind().is_some())
            .map(|p| PluginNavData {
                id: p.id.clone().into(),
                label: p.nav_label(lang).into(),
                icon_kind: p.nav_icon().slint_index(),
                surface_kind: match p.surface_kind() {
                    Some(UiSurfaceKind::Lighting4Zone) => 0,
                    Some(UiSurfaceKind::SensorBoard) => 1,
                    Some(UiSurfaceKind::HpOmenSuite) => 3,
                    _ => 2,
                },
            })
            .collect();
        state.nav_model.set_vec(nav_items);

        let total = state.installed.len();
        let active = state
            .installed
            .iter()
            .filter(|p| p.status.is_active())
            .count();
        d.set_plugins_summary(tf("plugins.summary", &[&total, &active]).into());

        if self.test {
            d.set_plugins_dir(r"%USERPROFILE%\.glph\plugins".into());
        } else if let Ok(store) = self.plugin_store() {
            d.set_plugins_dir(store.plugins_dir().to_string_lossy().as_ref().into());
        }
    }

    pub(super) fn apply_loaded_plugins(
        &self,
        scanned: Vec<InstalledPlugin>,
        highlight_id: Option<String>,
    ) {
        {
            let mut state = self.plugins.borrow_mut();
            state.installed = scanned;
            if let Some(id) = highlight_id {
                state.highlighted_id = Some(id);
            }
        }
        if !self.lighting_plugin_active() {
            self.teardown_lighting_plugin();
        }
        self.sync_plugin_models();
    }

    pub(super) fn load_plugins(&self, highlight_id: Option<String>) {
        if self.test {
            let mut state = self.plugins.borrow_mut();
            if state.installed.is_empty() {
                state.installed = ui_test_fixture_plugins();
            }
            if let Some(id) = highlight_id {
                state.highlighted_id = Some(id);
            }
            drop(state);
            self.sync_plugin_models();
            return;
        }
        let has_cached_inventory = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if !has_cached_inventory {
            let _ = self.aux.send(Aux::LoadPlugins(highlight_id, true));
            return;
        }
        let inv = self.current_inventory();
        let scanned = self
            .plugin_store()
            .map(|store| store.scan(&inv))
            .unwrap_or_default();
        self.apply_loaded_plugins(scanned, highlight_id);
    }

    pub(super) fn arm_plugin_highlight(host: &Rc<Self>, id: String) {
        {
            let mut state = host.plugins.borrow_mut();
            state.highlighted_id = Some(id);
        }
        host.sync_plugin_models();
        let weak = Rc::downgrade(host);
        host.plugins.borrow().highlight_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(1600),
            move || {
                if let Some(h) = weak.upgrade() {
                    h.plugins.borrow_mut().highlighted_id = None;
                    h.sync_plugin_models();
                }
            },
        );
    }

    pub(super) fn toggle_plugin(&self, id: &str, enable: bool) {
        let lang = crate::i18n::language_code();
        if self.test {
            let mut display_name = id.to_owned();
            {
                let mut state = self.plugins.borrow_mut();
                if let Some(p) = state.installed.iter_mut().find(|p| p.id == id) {
                    if !p.can_toggle {
                        return;
                    }
                    display_name = p.display_name(lang);
                    p.enabled = enable;
                    p.status = if enable {
                        PluginStatus::Active
                    } else {
                        PluginStatus::Inactive
                    };
                }
            }
            if !self.lighting_plugin_active() {
                self.teardown_lighting_plugin();
            }
            self.sync_plugin_models();
            let msg = tf(
                if enable {
                    "plugins.activated_notice"
                } else {
                    "plugins.deactivated_notice"
                },
                &[&display_name],
            );
            self.status(&msg, false);
            return;
        }

        let display_name = self
            .plugins
            .borrow()
            .installed
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.display_name(lang))
            .unwrap_or_else(|| id.to_owned());

        let result = self
            .plugin_store()
            .and_then(|store| store.set_enabled(id, enable, None));
        match result {
            Ok(()) => {
                self.load_plugins(None);
                let msg = tf(
                    if enable {
                        "plugins.activated_notice"
                    } else {
                        "plugins.deactivated_notice"
                    },
                    &[&display_name],
                );
                self.status(&msg, false);
            }
            Err(e) => {
                self.load_plugins(None);
                self.status(&crate::i18n::error_message(&e), true);
            }
        }
    }

    pub(super) fn open_plugins_folder(&self) {
        if self.test {
            self.status(t("ui.test_no_write"), false);
            return;
        }
        let res = self
            .plugin_store()
            .and_then(|store| store.ensure_plugins_dir())
            .and_then(|dir| glph_platform::open_plugins_folder(&dir));
        if let Err(e) = res {
            self.status(&crate::i18n::error_message(&e), true);
        }
    }

    pub(super) fn install_plugin_prompt(&self) {
        if self.test {
            self.status(t("ui.test_no_write"), false);
            return;
        }
        let owner = self.hwnd.get() as isize;
        self.send_aux(Aux::PickPluginFile(
            owner,
            t("plugins.picker_title").to_owned(),
            t("plugins.picker_filter").to_owned(),
        ));
    }

    pub(super) fn handle_plugin_install_outcome(
        host: &Rc<Self>,
        path: PathBuf,
        outcome: InstallOutcome,
    ) {
        let lang = crate::i18n::language_code();
        match outcome {
            InstallOutcome::Installed(manifest) => {
                host.plugins.borrow_mut().pending_install_path = None;
                let name = manifest.name.for_lang(lang).to_owned();
                host.load_plugins(Some(manifest.id.clone()));
                Self::arm_plugin_highlight(host, manifest.id);
                let msg = tf("plugins.installed_notice", &[&name]);
                host.status(&msg, false);
                host.completed(&msg);
            }
            InstallOutcome::NeedsReplaceConfirm(manifest) => {
                host.plugins.borrow_mut().pending_install_path = Some(path);
                let name = manifest.name.for_lang(lang).to_owned();
                let text = tf("plugins.confirm_replace", &[&name, &manifest.version]);
                if let Some(w) = host.window.upgrade() {
                    let d = w.global::<Workbench>();
                    d.set_plugin_confirm_text(text.into());
                    d.set_confirm_kind(7);
                }
            }
        }
    }

    pub(super) fn request_remove_plugin(&self, id: &str) {
        let lang = crate::i18n::language_code();
        let name = self
            .plugins
            .borrow()
            .installed
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.display_name(lang))
            .unwrap_or_else(|| id.to_owned());
        self.plugins.borrow_mut().pending_remove_id = Some(id.to_owned());
        if let Some(w) = self.window.upgrade() {
            let d = w.global::<Workbench>();
            d.set_plugin_confirm_text(tf("plugins.confirm_remove", &[&name]).into());
            d.set_confirm_kind(8);
        }
    }

    pub(super) fn confirm_replace_plugin(&self) {
        let path = self.plugins.borrow_mut().pending_install_path.take();
        if let Some(path) = path {
            self.send_aux(Aux::InstallPlugin(path, true));
        }
    }

    pub(super) fn confirm_remove_plugin(&self) {
        let Some(id) = self.plugins.borrow_mut().pending_remove_id.take() else {
            return;
        };
        let lang = crate::i18n::language_code();
        if self.test {
            self.plugins.borrow_mut().installed.retain(|p| p.id != id);
            if !self.lighting_plugin_active() {
                self.teardown_lighting_plugin();
            }
            self.sync_plugin_models();
            return;
        }
        let target = self
            .plugins
            .borrow()
            .installed
            .iter()
            .find(|p| p.id == id)
            .map(|p| {
                (
                    p.folder_name.clone(),
                    p.display_name(lang),
                    p.surface_kind(),
                )
            });
        let Some((folder_name, display_name, surface_kind)) = target else {
            return;
        };
        if matches!(
            surface_kind,
            Some(UiSurfaceKind::Lighting4Zone | UiSurfaceKind::HpOmenSuite)
        ) {
            self.teardown_lighting_plugin();
        }
        match self
            .plugin_store()
            .and_then(|store| store.remove(&folder_name, &id))
        {
            Ok(()) => {
                self.load_plugins(None);
                let msg = tf("plugins.removed_notice", &[&display_name]);
                self.status(&msg, false);
                self.completed(&msg);
            }
            Err(e) => {
                self.load_plugins(None);
                self.status(&crate::i18n::error_message(&e), true);
            }
        }
    }

    pub(super) fn select_plugin_page(&self, id: &str) {
        let lang = crate::i18n::language_code();
        let entry = self
            .plugins
            .borrow()
            .installed
            .iter()
            .find(|p| p.id == id && p.status.is_active())
            .map(|p| (p.nav_label(lang), p.surface_kind()));
        let Some((title, surface)) = entry else {
            return;
        };
        let surface_kind = match surface {
            Some(UiSurfaceKind::Lighting4Zone) => 0,
            Some(UiSurfaceKind::SensorBoard) => 1,
            Some(UiSurfaceKind::HpOmenSuite) => 3,
            _ => 2,
        };
        if let Some(w) = self.window.upgrade() {
            let d = w.global::<Workbench>();
            d.set_active_plugin_id(id.into());
            d.set_active_plugin_title(title.into());
            d.set_active_plugin_surface_kind(surface_kind);
        }
        self.navigate(3);
    }

    pub(super) fn sync_settings_dirty(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let d = w.global::<Workbench>();
        let (general_dirty, appearance_dirty, hardware_dirty) =
            if let Some((base, options, appearance, gpu_tgp_max_w, startup)) =
                self.settings_draft_state()
            {
                (
                    base.options != options || base.run_entry.is_some() != startup,
                    base.appearance != appearance,
                    gpu_tgp_max_w != Ok(base.gpu_tgp_max_w),
                )
            } else {
                let any_err = !d.get_gpu_tgp_max_error().is_empty();
                (false, false, any_err)
            };
        let any_dirty = general_dirty || appearance_dirty || hardware_dirty;
        d.set_settings_dirty(any_dirty);
        d.set_settings_tab_dirty(
            Rc::new(VecModel::from(vec![
                general_dirty,
                false,
                appearance_dirty,
                hardware_dirty,
            ]))
            .into(),
        );
    }

    pub(super) fn discard_settings(&self) {
        let base = self.baseline_settings.borrow().clone();
        if let Some(base) = base {
            self.show_settings(base, false);
        }
    }
}
