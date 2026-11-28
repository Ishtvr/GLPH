//! Shared, OS-independent contract for the dedicated `GLPHService`
//! Windows background service (Session 0 / LocalSystem) and its local Named Pipe IPC.
use crate::{
    fan::{self, FanCurve},
    profiles::FanPoint,
    quick_switch::FanMode,
};
use serde::{Deserialize, Serialize};

pub const SERVICE_NAME: &str = "GLPHService";
pub const SERVICE_DISPLAY_NAME: &str = "GLPH Hardware & Cooling Service";
pub const SERVICE_DESCRIPTION: &str = "Direct hardware cooling and zero-stutter sensor background service for GamingLaptopPerformanceHub (GLPH).";
pub const SERVICE_PIPE_NAME: &str = r"\\.\pipe\GLPH.Service.v1";
pub const SERVICE_CONFIG_SCHEMA: u32 = 1;
pub const SERVICE_STATUS_SCHEMA: u32 = 1;
pub const MAX_IPC_FRAME_BYTES: usize = 65_536;

/// Validated machine-wide configuration read by `GLPHService` from
/// `C:\ProgramData\GLPH\service-config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub schema: u32,
    pub fan_mode: FanMode,
    #[serde(default)]
    pub fan_curve: Option<Vec<FanPoint>>,
    #[serde(default)]
    pub chassis_max_temp_c: Option<u8>,
    #[serde(default)]
    pub paused: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            schema: SERVICE_CONFIG_SCHEMA,
            fan_mode: FanMode::Bios,
            fan_curve: None,
            chassis_max_temp_c: None,
            paused: false,
        }
    }
}

impl ServiceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SERVICE_CONFIG_SCHEMA {
            return Err(format!(
                "Unsupported service config schema: {}",
                self.schema
            ));
        }
        if let Some(points) = &self.fan_curve {
            let pairs: Vec<fan::Point> = points
                .iter()
                .map(|p| fan::Point {
                    temperature_c: i32::from(p.temperature_c),
                    percent: p.duty_percent,
                })
                .collect();
            FanCurve::new(pairs)?;
        }
        if let Some(limit) = self.chassis_max_temp_c {
            if !(35..=55).contains(&limit) {
                return Err(format!(
                    "Chassis max temperature {limit} °C is outside the allowed 35–55 °C range"
                ));
            }
        }
        Ok(())
    }

    pub fn effective_ir_limit_c(&self) -> u8 {
        self.chassis_max_temp_c.unwrap_or(fan::IR_EMERGENCY_C)
    }

    pub fn effective_fan_mode(&self) -> FanMode {
        if self.paused {
            FanMode::Bios
        } else {
            self.fan_mode
        }
    }
}

/// Live runtime status reported by `GLPHService` over the Named Pipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub schema: u32,
    pub version: String,
    pub process_id: u32,
    pub cooling_state: String,
    pub detail: String,
    pub cpu_c: Option<f64>,
    pub gpu_c: Option<i32>,
    pub ir_c: Option<u8>,
    pub protection: u8,
    pub cpu_level: u8,
    pub gpu_level: u8,
    pub paused: bool,
    pub suspended: bool,
    pub updated_unix_ms: u64,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self {
            schema: SERVICE_STATUS_SCHEMA,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            process_id: 0,
            cooling_state: "starting".into(),
            detail: String::new(),
            cpu_c: None,
            gpu_c: None,
            ir_c: None,
            protection: 0,
            cpu_level: 0,
            gpu_level: 0,
            paused: false,
            suspended: false,
            updated_unix_ms: 0,
        }
    }
}

/// Bounded local IPC request sent from `glph.exe` to `glph-service.exe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Status {},
    GetConfig {},
    ApplyConfig { config: ServiceConfig },
    SetPaused { paused: bool },
}

/// Bounded local IPC response returned by `glph-service.exe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceResponse {
    Status { status: ServiceStatus },
    Config { config: ServiceConfig },
    Ack { message: String },
    Error { message: String },
}

pub fn encode_ipc<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_IPC_FRAME_BYTES {
        return Err("Service IPC message exceeds frame size limit".into());
    }
    Ok(bytes)
}

pub fn decode_ipc<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    if bytes.is_empty() || bytes.len() > MAX_IPC_FRAME_BYTES {
        return Err("Service IPC frame size is invalid".into());
    }
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_config_validates_curve_and_ir_bounds() {
        let mut cfg = ServiceConfig::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.effective_ir_limit_c(), fan::IR_EMERGENCY_C);

        cfg.chassis_max_temp_c = Some(46);
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.effective_ir_limit_c(), 46);

        cfg.chassis_max_temp_c = Some(30);
        assert!(cfg.validate().is_err());

        cfg.chassis_max_temp_c = Some(45);
        cfg.fan_curve = Some(vec![
            FanPoint {
                temperature_c: 40,
                duty_percent: 30,
            },
            FanPoint {
                temperature_c: 80,
                duty_percent: 85,
            },
        ]);
        assert!(cfg.validate().is_ok());

        // Paused overrides fan_mode to Bios safely.
        cfg.fan_mode = FanMode::Curve;
        assert_eq!(cfg.effective_fan_mode(), FanMode::Curve);
        cfg.paused = true;
        assert_eq!(cfg.effective_fan_mode(), FanMode::Bios);
    }

    #[test]
    fn service_ipc_round_trip_and_unknown_field_rejection() {
        let req = ServiceRequest::ApplyConfig {
            config: ServiceConfig {
                schema: 1,
                fan_mode: FanMode::Curve,
                fan_curve: Some(vec![
                    FanPoint {
                        temperature_c: 45,
                        duty_percent: 35,
                    },
                    FanPoint {
                        temperature_c: 85,
                        duty_percent: 90,
                    },
                ]),
                chassis_max_temp_c: Some(47),
                paused: false,
            },
        };
        let encoded = encode_ipc(&req).unwrap();
        let decoded: ServiceRequest = decode_ipc(&encoded).unwrap();
        assert_eq!(decoded, req);

        // Unknown fields must be rejected.
        let bad = br#"{"op":"status","extra":true}"#;
        assert!(decode_ipc::<ServiceRequest>(bad).is_err());
    }
}
