//! Global application preferences. These never authorize a hardware profile.
use serde::{Deserialize, Serialize};

/// Default live telemetry polling interval in milliseconds.
pub const TELEMETRY_POLL_DEFAULT_MS: u32 = 1000;
/// Allowed discrete telemetry polling intervals in milliseconds.
pub const TELEMETRY_POLL_OPTIONS_MS: [u32; 5] = [500, 1000, 2000, 3000, 5000];

pub fn is_valid_telemetry_poll_ms(ms: u32) -> bool {
    TELEMETRY_POLL_OPTIONS_MS.contains(&ms)
}

pub fn telemetry_poll_index(ms: u32) -> usize {
    TELEMETRY_POLL_OPTIONS_MS
        .iter()
        .position(|&opt| opt == ms)
        .unwrap_or_else(|| {
            TELEMETRY_POLL_OPTIONS_MS
                .iter()
                .position(|&opt| opt == TELEMETRY_POLL_DEFAULT_MS)
                .unwrap_or(1)
        })
}

pub fn telemetry_poll_from_index(index: usize) -> u32 {
    TELEMETRY_POLL_OPTIONS_MS
        .get(index)
        .copied()
        .unwrap_or(TELEMETRY_POLL_DEFAULT_MS)
}

pub fn telemetry_poll_label(ms: u32) -> &'static str {
    match ms {
        500 => "500 ms",
        1000 => "1 s",
        2000 => "2 s",
        3000 => "3 s",
        5000 => "5 s",
        _ => "1 s",
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguagePreference {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "de")]
    German,
    #[serde(rename = "en")]
    English,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppSettings {
    pub schema: u32,
    pub language: LanguagePreference,
    pub start_in_tray: bool,
    pub close_to_tray: bool,
    /// Mode or preset applied automatically without the charger, as a
    /// [`crate::battery::Selection`] key; `None` turns this off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_preset: Option<String>,
    /// Rewrite the applied preset's CPU limits and GPU flags when firmware
    /// changes them (see [`crate::hold`]). Off until the user turns it on.
    pub hold_preset: bool,
    /// Live telemetry polling interval in milliseconds. Must be one of
    /// [`TELEMETRY_POLL_OPTIONS_MS`].
    pub telemetry_poll_ms: u32,
}
impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema: 1,
            language: LanguagePreference::System,
            start_in_tray: false,
            close_to_tray: true,
            battery_preset: None,
            hold_preset: false,
            telemetry_poll_ms: TELEMETRY_POLL_DEFAULT_MS,
        }
    }
}
impl AppSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err(crate::message::msg("settings.invalid_schema", &[]));
        }
        if self
            .battery_preset
            .as_deref()
            .is_some_and(|key| crate::battery::Selection::parse(key).is_none())
        {
            return Err(crate::message::msg("settings.invalid_battery", &[]));
        }
        if !is_valid_telemetry_poll_ms(self.telemetry_poll_ms) {
            return Err(crate::message::msg(
                "settings.invalid_telemetry_poll",
                &[&self.telemetry_poll_ms],
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_close_preference_keeps_its_value_without_enabling_any_startup() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"schema":1,"close_to_tray":false}"#).unwrap();
        assert!(!settings.close_to_tray);
        assert!(!settings.start_in_tray);
        assert_eq!(settings.language, LanguagePreference::System);
        assert_eq!(settings.battery_preset, None);
        assert!(!settings.hold_preset);
        assert_eq!(settings.telemetry_poll_ms, TELEMETRY_POLL_DEFAULT_MS);
        assert!(settings.validate().is_ok());
    }
    #[test]
    fn battery_preset_is_a_valid_selection_key() {
        let mut settings = AppSettings {
            battery_preset: Some("mode:quiet".into()),
            ..Default::default()
        };
        assert!(settings.validate().is_ok());
        settings.battery_preset = Some("mode:turbo".into());
        assert!(settings.validate().is_err());
        // Without a battery preset the file keeps its previous shape.
        assert!(
            !serde_json::to_string(&AppSettings::default())
                .unwrap()
                .contains("battery")
        );
    }
    #[test]
    fn telemetry_poll_rate_accepts_discrete_options_and_rejects_arbitrary_values() {
        for (idx, &ms) in TELEMETRY_POLL_OPTIONS_MS.iter().enumerate() {
            let settings = AppSettings {
                telemetry_poll_ms: ms,
                ..Default::default()
            };
            assert!(is_valid_telemetry_poll_ms(ms));
            assert_eq!(telemetry_poll_index(ms), idx);
            assert_eq!(telemetry_poll_from_index(idx), ms);
            assert!(!telemetry_poll_label(ms).is_empty());
            assert!(settings.validate().is_ok());
            let json = serde_json::to_string(&settings).unwrap();
            let round_trip: AppSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(round_trip, settings);
        }
        for bad in [0, 100, 250, 750, 1500, 4000, 10_000, u32::MAX] {
            let settings = AppSettings {
                telemetry_poll_ms: bad,
                ..Default::default()
            };
            assert!(!is_valid_telemetry_poll_ms(bad));
            let err = settings.validate().unwrap_err();
            assert!(err.contains("settings.invalid_telemetry_poll"));
        }
        assert_eq!(telemetry_poll_from_index(99), TELEMETRY_POLL_DEFAULT_MS);
        assert_eq!(telemetry_poll_index(9999), 1);
    }
    #[test]
    fn unknown_preferences_and_future_schemas_cannot_be_overwritten_as_defaults() {
        assert!(serde_json::from_str::<AppSettings>(r#"{"schema":1,"language":"fr"}"#).is_err());
        assert!(serde_json::from_str::<AppSettings>(r#"{"schema":1,"future":true}"#).is_err());
        let settings: AppSettings = serde_json::from_str(r#"{"schema":2}"#).unwrap();
        assert!(settings.validate().is_err());
    }
}
