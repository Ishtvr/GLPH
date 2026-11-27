//! Hardware-independent plugin manifest, compatibility, state sidecar, and
//! forward declarative UI contracts (including HWiNFO-style Sensor Board).
use crate::Inventory;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PLUGIN_SCHEMA: u32 = 1;
pub const PLUGIN_API_VERSION: u32 = 1;
pub const PLUGINS_STATE_SCHEMA: u32 = 1;

/// Maximum files allowed inside a single unpacked plugin bundle.
pub const MAX_PLUGIN_FILES: usize = 64;
/// Maximum total unpacked size of a plugin bundle (32 MiB).
pub const MAX_PLUGIN_UNPACKED_BYTES: u64 = 32 * 1024 * 1024;
/// Bounded declarative sensor board frame limits (forward Tier-2 contract).
pub const MAX_SENSOR_GROUPS: usize = 32;
pub const MAX_SENSOR_ROWS: usize = 256;

/// Official ID of the installable HP OMEN 4-Zone Keyboard Lighting plugin.
pub const HP_LIGHTING_PLUGIN_ID: &str = "hp-omen-4zone-lighting";

const RESERVED_IDS: &[&str] = &[
    "overview", "cooling", "settings", "system", "core", "plugins",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalizedText {
    pub de: String,
    pub en: String,
}

impl LocalizedText {
    pub fn for_lang(&self, lang_code: &str) -> &str {
        if lang_code.starts_with("de") {
            &self.de
        } else {
            &self.en
        }
    }

    fn validate(&self, field: &str, max_chars: usize, allow_empty: bool) -> Result<(), String> {
        for (lang, text) in [("de", &self.de), ("en", &self.en)] {
            let trimmed = text.trim();
            if !allow_empty && trimmed.is_empty() {
                return Err(format!("plugin manifest {field}.{lang} must not be empty"));
            }
            if trimmed.chars().count() > max_chars {
                return Err(format!(
                    "plugin manifest {field}.{lang} exceeds {max_chars} characters"
                ));
            }
            if trimmed.chars().any(|c| c.is_control()) {
                return Err(format!(
                    "plugin manifest {field}.{lang} contains control characters"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCategory {
    Lighting,
    Sensors,
    Telemetry,
    Utility,
}

impl PluginCategory {
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Lighting => "plugins.category.lighting",
            Self::Sensors => "plugins.category.sensors",
            Self::Telemetry => "plugins.category.telemetry",
            Self::Utility => "plugins.category.utility",
        }
    }
}

/// Execution tier of an installed plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntime {
    /// Installable manifest bundle backed by an audited built-in host adapter
    /// and compiled Slint surface (zero external binary / Defender friction).
    HostSurface,
    /// Out-of-process JSON-RPC plugin spawned de-elevated at Medium Integrity
    /// over an SDDL-locked named pipe (used by future Sensor & external plugins).
    Process,
}

/// Closed set of AOT-compiled Slint surfaces a plugin may mount in the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiSurfaceKind {
    #[serde(rename = "lighting-4zone")]
    Lighting4Zone,
    SensorBoard,
    Panel,
}

/// Closed set of compiled sidebar SVG icons (`chrome.slint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavIcon {
    Keyboard,
    Sensors,
    Gauge,
    Chip,
    Plug,
}

impl NavIcon {
    pub fn slint_index(self) -> i32 {
        match self {
            Self::Keyboard => 2,
            Self::Sensors => 4,
            Self::Gauge => 5,
            Self::Chip => 6,
            Self::Plug => 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginCapability {
    #[serde(rename = "hardware.hp_lighting")]
    HardwareHpLighting,
    #[serde(rename = "sensors.read")]
    SensorsRead,
    #[serde(rename = "host.telemetry_subscribe")]
    HostTelemetrySubscribe,
    #[serde(rename = "ui.sidebar_page")]
    UiSidebarPage,
}

impl PluginCapability {
    pub fn label_key(self) -> &'static str {
        match self {
            Self::HardwareHpLighting => "plugins.cap.hp_lighting",
            Self::SensorsRead => "plugins.cap.sensors_read",
            Self::HostTelemetrySubscribe => "plugins.cap.host_telemetry",
            Self::UiSidebarPage => "plugins.cap.sidebar_page",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorFilter {
    #[serde(default)]
    pub manufacturers: Vec<String>,
    #[serde(default)]
    pub require_hp_wmi: bool,
}

impl VendorFilter {
    /// Returns `Ok(())` when the current hardware satisfies this filter,
    /// otherwise returns a localized message marker explaining why it does not.
    pub fn check(&self, inventory: &Inventory) -> Result<(), String> {
        if self.require_hp_wmi && !inventory.hp_bios_interface_present {
            return Err(crate::message::msg("plugins.err.requires_hp_wmi", &[]));
        }
        if !self.manufacturers.is_empty() {
            let current = inventory.manufacturer.trim();
            let matched = self
                .manufacturers
                .iter()
                .any(|m| m.trim().eq_ignore_ascii_case(current));
            if !matched {
                return Err(crate::message::msg(
                    "plugins.err.vendor_mismatch",
                    &[&self.manufacturers.join(", ")],
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiSurface {
    pub kind: UiSurfaceKind,
    pub nav_label: LocalizedText,
    pub nav_icon: NavIcon,
    #[serde(default = "default_nav_order")]
    pub nav_order: u16,
}

fn default_nav_order() -> u16 {
    100
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEntry {
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema: u32,
    pub id: String,
    pub name: LocalizedText,
    pub description: LocalizedText,
    pub version: String,
    pub api_version: u32,
    #[serde(default)]
    pub author: String,
    pub category: PluginCategory,
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub vendor_filter: Option<VendorFilter>,
    pub ui_surface: PluginUiSurface,
    #[serde(default)]
    pub entry: Option<PluginEntry>,
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != PLUGIN_SCHEMA {
            return Err(format!(
                "unsupported plugin manifest schema {}",
                self.schema
            ));
        }
        if self.api_version != PLUGIN_API_VERSION {
            return Err(format!(
                "unsupported plugin api_version {}",
                self.api_version
            ));
        }
        validate_plugin_id(&self.id)?;
        self.name.validate("name", 48, false)?;
        self.description.validate("description", 160, true)?;
        self.ui_surface.nav_label.validate("nav_label", 20, false)?;
        validate_semver(&self.version)?;
        if self.author.chars().count() > 64 || self.author.chars().any(|c| c.is_control()) {
            return Err("invalid plugin author string".into());
        }
        if self.capabilities.len() > 16 {
            return Err("too many plugin capabilities".into());
        }
        match self.runtime {
            PluginRuntime::HostSurface => {
                if self.entry.is_some() {
                    return Err("host_surface plugin must not specify an entry executable".into());
                }
                if self.ui_surface.kind == UiSurfaceKind::Lighting4Zone
                    && !self
                        .capabilities
                        .contains(&PluginCapability::HardwareHpLighting)
                {
                    return Err(
                        "lighting-4zone plugin must declare hardware.hp_lighting capability".into(),
                    );
                }
            }
            PluginRuntime::Process => {
                if self
                    .capabilities
                    .contains(&PluginCapability::HardwareHpLighting)
                {
                    return Err("external process plugins cannot claim hardware.hp_lighting".into());
                }
                let entry = self
                    .entry
                    .as_ref()
                    .ok_or_else(|| "process plugin requires an entry executable".to_owned())?;
                validate_relative_path(&entry.executable)?;
                if entry.args.len() > 8 {
                    return Err("process plugin entry supports at most 8 arguments".into());
                }
                if entry
                    .args
                    .iter()
                    .any(|a| a.chars().count() > 128 || a.chars().any(|c| c.is_control()))
                {
                    return Err("process plugin entry contains invalid arguments".into());
                }
                validate_sha256_hex(&entry.sha256)?;
            }
        }
        Ok(())
    }
}

pub fn validate_plugin_id(id: &str) -> Result<(), String> {
    let bytes = id.as_bytes();
    if !(2..=63).contains(&bytes.len()) {
        return Err("plugin id must be 2..=63 characters".into());
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err("plugin id must start with a lowercase letter or digit".into());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("plugin id may only contain lowercase letters, digits, and '-'".into());
    }
    if RESERVED_IDS.contains(&id) {
        return Err(format!("plugin id '{id}' is reserved"));
    }
    Ok(())
}

pub fn validate_semver(version: &str) -> Result<(), String> {
    let core = version.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!(
            "invalid plugin version '{version}' (expected X.Y.Z)"
        ));
    }
    if version.len() > 32 || version.chars().any(|c| c.is_control()) {
        return Err("plugin version is too long or contains control characters".into());
    }
    Ok(())
}

pub fn validate_sha256_hex(hex: &str) -> Result<(), String> {
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("plugin entry sha256 must be 64 lowercase hex characters".into());
    }
    Ok(())
}

/// Validates a relative path inside a plugin bundle or archive against
/// traversal (`..`), absolute paths, drive prefixes, ADS (`:`), and control chars.
pub fn validate_relative_path(raw: &str) -> Result<(), String> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(':')
        || normalized.chars().any(|c| c.is_control())
    {
        return Err(format!("unsafe plugin path '{raw}'"));
    }
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("unsafe path segment in '{raw}'"));
        }
    }
    Ok(())
}

/// Sidecar state persisted in `%USERPROFILE%\.performance-hub\plugins.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsStateFile {
    pub schema: u32,
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginStateEntry>,
}

impl Default for PluginsStateFile {
    fn default() -> Self {
        Self {
            schema: PLUGINS_STATE_SCHEMA,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStateEntry {
    pub enabled: bool,
    #[serde(default)]
    pub pinned_sha256: Option<String>,
    #[serde(default)]
    pub installed_at_unix_s: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum PluginStatus {
    Active,
    Inactive,
    Incompatible(String),
    Error(String),
}

impl PluginStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub id: String,
    pub folder_name: String,
    pub manifest: Option<PluginManifest>,
    pub status: PluginStatus,
    pub enabled: bool,
    pub can_toggle: bool,
    pub can_remove: bool,
    pub diagnostic: Option<String>,
    pub pinned_sha256: Option<String>,
}

impl InstalledPlugin {
    pub fn display_name(&self, lang_code: &str) -> String {
        self.manifest
            .as_ref()
            .map(|m| m.name.for_lang(lang_code).to_owned())
            .unwrap_or_else(|| self.folder_name.clone())
    }

    pub fn display_description(&self, lang_code: &str) -> String {
        self.manifest
            .as_ref()
            .map(|m| m.description.for_lang(lang_code).to_owned())
            .unwrap_or_else(|| self.diagnostic.clone().unwrap_or_default())
    }

    pub fn nav_label(&self, lang_code: &str) -> String {
        self.manifest
            .as_ref()
            .map(|m| m.ui_surface.nav_label.for_lang(lang_code).to_owned())
            .unwrap_or_else(|| self.folder_name.clone())
    }

    pub fn version(&self) -> &str {
        self.manifest
            .as_ref()
            .map(|m| m.version.as_str())
            .unwrap_or("—")
    }

    pub fn nav_icon(&self) -> NavIcon {
        self.manifest
            .as_ref()
            .map(|m| m.ui_surface.nav_icon)
            .unwrap_or(NavIcon::Plug)
    }

    pub fn nav_order(&self) -> u16 {
        self.manifest
            .as_ref()
            .map(|m| m.ui_surface.nav_order)
            .unwrap_or(999)
    }

    pub fn surface_kind(&self) -> Option<UiSurfaceKind> {
        self.manifest.as_ref().map(|m| m.ui_surface.kind)
    }
}

// ---------------------------------------------------------------------------
// Forward Declarative Sensor Board Contract (Plugin #2: HWiNFO-style Sensors)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorCategory {
    Cpu,
    Gpu,
    System,
    Cooling,
    Power,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorSeverity {
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorRow {
    pub id: String,
    pub label: String,
    pub current: Option<f64>,
    pub min: Option<f64>,
    pub avg: Option<f64>,
    pub max: Option<f64>,
    pub unit: String,
    pub ratio: Option<f32>,
    pub severity: SensorSeverity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorGroup {
    pub id: String,
    pub title: String,
    pub category: SensorCategory,
    pub summary: String,
    pub rows: Vec<SensorRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorBoardSnapshot {
    pub schema: u32,
    pub groups: Vec<SensorGroup>,
}

impl SensorBoardSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != PLUGIN_SCHEMA {
            return Err(format!("unsupported sensor board schema {}", self.schema));
        }
        if self.groups.len() > MAX_SENSOR_GROUPS {
            return Err("sensor board exceeds maximum group count".into());
        }
        let total_rows: usize = self.groups.iter().map(|g| g.rows.len()).sum();
        if total_rows > MAX_SENSOR_ROWS {
            return Err("sensor board exceeds maximum row count".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lighting_manifest() -> PluginManifest {
        PluginManifest {
            schema: PLUGIN_SCHEMA,
            id: HP_LIGHTING_PLUGIN_ID.into(),
            name: LocalizedText {
                de: "HP OMEN Tastaturbeleuchtung".into(),
                en: "HP OMEN Keyboard Lighting".into(),
            },
            description: LocalizedText {
                de: "4-Zonen-RGB-Steuerung für HP OMEN Notebooks.".into(),
                en: "4-zone RGB lighting for HP OMEN laptops.".into(),
            },
            version: "2.0.0".into(),
            api_version: PLUGIN_API_VERSION,
            author: "PerformanceHub".into(),
            category: PluginCategory::Lighting,
            runtime: PluginRuntime::HostSurface,
            capabilities: vec![
                PluginCapability::HardwareHpLighting,
                PluginCapability::UiSidebarPage,
            ],
            vendor_filter: Some(VendorFilter {
                manufacturers: vec!["HP".into()],
                require_hp_wmi: true,
            }),
            ui_surface: PluginUiSurface {
                kind: UiSurfaceKind::Lighting4Zone,
                nav_label: LocalizedText {
                    de: "Beleuchtung".into(),
                    en: "Lighting".into(),
                },
                nav_icon: NavIcon::Keyboard,
                nav_order: 10,
            },
            entry: None,
        }
    }

    #[test]
    fn valid_hp_lighting_manifest_passes_and_checks_vendor_filter() {
        let m = lighting_manifest();
        m.validate().unwrap();
        let hp = Inventory {
            manufacturer: "HP".into(),
            hp_bios_interface_present: true,
            ..Default::default()
        };
        assert!(m.vendor_filter.as_ref().unwrap().check(&hp).is_ok());

        let asus = Inventory {
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            hp_bios_interface_present: false,
            ..Default::default()
        };
        assert!(m.vendor_filter.as_ref().unwrap().check(&asus).is_err());
    }

    #[test]
    fn external_process_cannot_claim_hp_lighting_or_bad_paths() {
        let mut m = lighting_manifest();
        m.id = "custom-sensors".into();
        m.runtime = PluginRuntime::Process;
        m.ui_surface.kind = UiSurfaceKind::SensorBoard;
        m.entry = Some(PluginEntry {
            executable: "bin/sensor-agent.exe".into(),
            args: vec![],
            sha256: "a".repeat(64),
        });
        assert!(m.validate().is_err());

        m.capabilities = vec![
            PluginCapability::SensorsRead,
            PluginCapability::HostTelemetrySubscribe,
            PluginCapability::UiSidebarPage,
        ];
        assert!(m.validate().is_ok());

        m.entry.as_mut().unwrap().executable = "../escape.exe".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn reserved_ids_and_zip_slip_paths_are_rejected() {
        assert!(validate_plugin_id("settings").is_err());
        assert!(validate_plugin_id("Overview").is_err());
        assert!(validate_plugin_id("hp-omen-4zone-lighting").is_ok());
        assert!(validate_relative_path("plugin.json").is_ok());
        assert!(validate_relative_path("../evil.exe").is_err());
        assert!(validate_relative_path("C:\\Windows\\cmd.exe").is_err());
        assert!(validate_relative_path("foo.txt:ads").is_err());
    }

    #[test]
    fn plugins_state_sidecar_roundtrips_with_deny_unknown_fields() {
        let mut state = PluginsStateFile::default();
        state.plugins.insert(
            HP_LIGHTING_PLUGIN_ID.into(),
            PluginStateEntry {
                enabled: true,
                pinned_sha256: None,
                installed_at_unix_s: Some(1_795_750_000),
            },
        );
        let raw = serde_json::to_string_pretty(&state).unwrap();
        let parsed: PluginsStateFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, state);
        assert!(
            serde_json::from_str::<PluginsStateFile>(r#"{"schema":1,"plugins":{},"extra":true}"#)
                .is_err()
        );
    }
}
