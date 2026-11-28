//! Public NVIDIA telemetry only. No undocumented power-policy setters.
use glph_core::GpuSample;
use libloading::Library;
use std::{
    ffi::{c_char, c_void},
    mem::size_of,
    ptr,
};

type Handle = *mut c_void;
type StatusFn = unsafe extern "C" fn() -> i32;
type Query = unsafe extern "C" fn(u32) -> *mut c_void;
type EnumGpus = unsafe extern "C" fn(*mut Handle, *mut u32) -> i32;
type NameFn = unsafe extern "C" fn(Handle, *mut c_char) -> i32;
type ThermalFn = unsafe extern "C" fn(Handle, u32, *mut Thermal) -> i32;
type ClockFn = unsafe extern "C" fn(Handle, *mut Clocks) -> i32;
type NvmlDevice = unsafe extern "C" fn(u32, *mut Handle) -> i32;
type NvmlName = unsafe extern "C" fn(Handle, *mut c_char, u32) -> i32;
type NvmlValue = unsafe extern "C" fn(Handle, *mut u32) -> i32;
type NvmlLimits = unsafe extern "C" fn(Handle, *mut u32, *mut u32) -> i32;
type NvmlUtilizationFn = unsafe extern "C" fn(Handle, *mut NvmlUtilization) -> i32;
type NvmlIndexedValueFn = unsafe extern "C" fn(Handle, i32, *mut u32) -> i32;

/// Public NVML `nvmlUtilization_t` C layout (8 bytes, align 4).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Sensor {
    controller: i32,
    minimum: i32,
    maximum: i32,
    current: i32,
    target: i32,
}
#[repr(C)]
#[derive(Default)]
struct Thermal {
    version: u32,
    count: u32,
    sensors: [Sensor; 3],
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Domain {
    present: u32,
    frequency: u32,
}
#[repr(C)]
#[derive(Default)]
struct Clocks {
    version: u32,
    clock_type: u32,
    domains: [Domain; 32],
}

pub(super) fn system_library(name: &str) -> Result<Library, String> {
    // SAFETY: Only fixed driver DLL names are used. Restrict search to System32,
    // preventing DLL loading from the working directory or PATH.
    unsafe { libloading::os::windows::Library::load_with_flags(name, 0x00000800) }
        .map(Into::into)
        .map_err(|e| format!("{name}: {e}"))
}
pub(super) fn text_buffer(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn same_gpu_name(first: &str, second: &str) -> bool {
    first.trim_start_matches("NVIDIA ") == second.trim_start_matches("NVIDIA ")
}

fn nvapi_session_lost(status: i32) -> bool {
    matches!(status, -4 | -6 | -8 | -10)
}
fn nvml_session_lost(status: i32) -> bool {
    // 1 = UNINITIALIZED, 11 = DRIVER_NOT_LOADED, 15 = GPU_IS_LOST,
    // 18 = RESET_REQUIRED, 999 = UNKNOWN (e.g. after dGPU PnP restart).
    matches!(status, 1 | 11 | 15 | 18 | 999)
}

struct Nvapi {
    _library: Library,
    unload: StatusFn,
    gpu: Handle,
    name: String,
    thermal: ThermalFn,
    clocks: ClockFn,
}
impl Nvapi {
    fn new() -> Result<Self, String> {
        let library = system_library("nvapi64.dll")?;
        // SAFETY: Function IDs and C ABIs match NVIDIA's public nvapi headers.
        // Typed pointers never outlive the library retained in Self.
        unsafe {
            let query: Query = *library
                .get(b"nvapi_QueryInterface\0")
                .map_err(|e| e.to_string())?;
            let get = |id| {
                let p = query(id);
                if p.is_null() {
                    Err(format!("Missing NVAPI function {id:#x}"))
                } else {
                    Ok(p)
                }
            };
            let init: StatusFn = std::mem::transmute(get(0x0150e828)?);
            let unload: StatusFn = std::mem::transmute(get(0xd22bdd7e)?);
            let enumerate: EnumGpus = std::mem::transmute(get(0xe5ac921f)?);
            let name_fn: NameFn = std::mem::transmute(get(0xceee8e9f)?);
            let thermal: ThermalFn = std::mem::transmute(get(0xe3640a56)?);
            let clocks: ClockFn = std::mem::transmute(get(0xdcb616c3)?);
            let status = init();
            if status != 0 {
                return Err(format!("NVAPI initialization: {status}"));
            }
            let mut gpus = [ptr::null_mut(); 64];
            let mut count = 0;
            let status = enumerate(gpus.as_mut_ptr(), &mut count);
            if status != 0 || count != 1 || gpus[0].is_null() {
                unload();
                return Err(format!("NVAPI GPU enumeration: {status}, count {count}"));
            }
            let mut name = [0; 64];
            if name_fn(gpus[0], name.as_mut_ptr()) != 0 {
                unload();
                return Err("NVAPI GPU identity unavailable".into());
            }
            Ok(Self {
                _library: library,
                unload,
                gpu: gpus[0],
                name: text_buffer(&name),
                thermal,
                clocks,
            })
        }
    }
    fn sample(&self) -> (GpuSample, bool) {
        let mut sample = GpuSample {
            name: self.name.clone(),
            ..Default::default()
        };
        let mut thermal = Thermal {
            version: size_of::<Thermal>() as u32 | (2 << 16),
            ..Default::default()
        };
        let mut clocks = Clocks {
            version: size_of::<Clocks>() as u32 | (3 << 16),
            ..Default::default()
        };
        // SAFETY: Driver handle and function pointers are owned by this live instance.
        // Structures are repr(C), versioned and initialized, including reserved fields.
        unsafe {
            let status = (self.thermal)(self.gpu, 0, &mut thermal);
            if status == 0 {
                sample.temperature_c = thermal
                    .sensors
                    .iter()
                    .take((thermal.count as usize).min(3))
                    .find(|s| s.target == 1)
                    .map(|s| s.current);
            } else {
                sample
                    .warnings
                    .push(format!("NVAPI temperature unavailable: {status}"));
                if nvapi_session_lost(status) {
                    return (sample, true);
                }
            }
            let status = (self.clocks)(self.gpu, &mut clocks);
            if status == 0 && clocks.domains[0].present & 1 != 0 {
                sample.clock_mhz = Some(clocks.domains[0].frequency / 1000);
            } else {
                sample
                    .warnings
                    .push(format!("NVAPI clock unavailable: {status}"));
            }
            if nvapi_session_lost(status) {
                return (sample, true);
            }
        }
        (sample, false)
    }
}
impl Drop for Nvapi {
    fn drop(&mut self) {
        unsafe {
            (self.unload)();
        }
    }
}

struct Nvml {
    _library: Library,
    shutdown: StatusFn,
    gpu: Handle,
    name: String,
    power: NvmlValue,
    utilization: Option<NvmlUtilizationFn>,
    temperature: Option<NvmlIndexedValueFn>,
    clock_info: Option<NvmlIndexedValueFn>,
    enforced_limit: Option<NvmlValue>,
    default_limit: NvmlValue,
    constraints: NvmlLimits,
    /// Static VBIOS limits cached once per live NVML session to avoid two
    /// redundant driver calls on every live telemetry tick.
    cached_default_limit_w: Option<f64>,
    cached_maximum_limit_w: Option<f64>,
}
impl Nvml {
    fn new(expected_name: Option<&str>) -> Result<Self, String> {
        let library = system_library("nvml.dll")?;
        // SAFETY: These are the documented NVML C signatures; DLL lifetime is retained.
        unsafe {
            let init: StatusFn = *library.get(b"nvmlInit_v2\0").map_err(|e| e.to_string())?;
            let shutdown: StatusFn = *library.get(b"nvmlShutdown\0").map_err(|e| e.to_string())?;
            let device: NvmlDevice = *library
                .get(b"nvmlDeviceGetHandleByIndex_v2\0")
                .map_err(|e| e.to_string())?;
            let name_fn: NvmlName = *library
                .get(b"nvmlDeviceGetName\0")
                .map_err(|e| e.to_string())?;
            let power = *library
                .get(b"nvmlDeviceGetPowerUsage\0")
                .map_err(|e| e.to_string())?;
            let utilization = library
                .get::<NvmlUtilizationFn>(b"nvmlDeviceGetUtilizationRates\0")
                .ok()
                .map(|f| *f);
            let temperature = library
                .get::<NvmlIndexedValueFn>(b"nvmlDeviceGetTemperature\0")
                .ok()
                .map(|f| *f);
            let clock_info = library
                .get::<NvmlIndexedValueFn>(b"nvmlDeviceGetClockInfo\0")
                .ok()
                .map(|f| *f);
            let enforced_limit = library
                .get::<NvmlValue>(b"nvmlDeviceGetEnforcedPowerLimit\0")
                .ok()
                .map(|f| *f);
            let default_limit: NvmlValue = *library
                .get(b"nvmlDeviceGetPowerManagementDefaultLimit\0")
                .map_err(|e| e.to_string())?;
            let constraints: NvmlLimits = *library
                .get(b"nvmlDeviceGetPowerManagementLimitConstraints\0")
                .map_err(|e| e.to_string())?;
            let status = init();
            if status != 0 {
                return Err(format!("NVML initialization: {status}"));
            }
            let mut gpu = ptr::null_mut();
            let status = device(0, &mut gpu);
            if status != 0 || gpu.is_null() {
                shutdown();
                return Err(format!("NVML GPU enumeration: {status}"));
            }
            let mut name = [0; 96];
            let status = name_fn(gpu, name.as_mut_ptr(), name.len() as u32);
            if status != 0 {
                shutdown();
                return Err(format!("NVML identity: {status}"));
            }
            let name = text_buffer(&name);
            if expected_name.is_some_and(|expected| !same_gpu_name(expected, &name)) {
                shutdown();
                return Err("NVML/NVAPI GPU names differ; power readings were not combined".into());
            }
            let mut default_mw = 0u32;
            let cached_default_limit_w =
                (default_limit(gpu, &mut default_mw) == 0).then(|| f64::from(default_mw) / 1000.0);
            let (mut min_mw, mut max_mw) = (0u32, 0u32);
            let cached_maximum_limit_w = (constraints(gpu, &mut min_mw, &mut max_mw) == 0)
                .then(|| f64::from(max_mw) / 1000.0);
            Ok(Self {
                _library: library,
                shutdown,
                gpu,
                name,
                power,
                utilization,
                temperature,
                clock_info,
                enforced_limit,
                default_limit,
                constraints,
                cached_default_limit_w,
                cached_maximum_limit_w,
            })
        }
    }
    fn sample(&self, sample: &mut GpuSample) -> bool {
        if sample.name.is_empty() {
            sample.name = self.name.clone();
        }
        sample.default_limit_w = self.cached_default_limit_w;
        sample.maximum_limit_w = self.cached_maximum_limit_w;
        // SAFETY: All out parameters have the exact documented C layouts.
        unsafe {
            let mut power_mw = 0;
            let status = (self.power)(self.gpu, &mut power_mw);
            if status == 0 {
                sample.power_w = Some(f64::from(power_mw) / 1000.0);
            } else {
                sample
                    .warnings
                    .push(format!("NVML power unavailable: {status}"));
                if nvml_session_lost(status) {
                    return true;
                }
            }
            if let Some(getter) = self.utilization {
                let mut util = NvmlUtilization::default();
                let status = getter(self.gpu, &mut util);
                if status == 0 && util.gpu <= 100 {
                    sample.utilization_percent = Some(util.gpu as u8);
                } else if nvml_session_lost(status) {
                    return true;
                }
            }
            // Fallback temperature if NVAPI was unavailable or failed this tick.
            if sample.temperature_c.is_none()
                && let Some(getter) = self.temperature
            {
                let mut temp = 0u32;
                let status = getter(self.gpu, 0 /* NVML_TEMPERATURE_GPU */, &mut temp);
                if status == 0 && (1..=150).contains(&temp) {
                    sample.temperature_c = Some(temp as i32);
                } else if nvml_session_lost(status) {
                    return true;
                }
            }
            // Fallback graphics clock if NVAPI was unavailable or failed this tick.
            if sample.clock_mhz.is_none()
                && let Some(getter) = self.clock_info
            {
                let mut clock = 0u32;
                let status = getter(self.gpu, 0 /* NVML_CLOCK_GRAPHICS */, &mut clock);
                if status == 0 && clock <= 6000 {
                    sample.clock_mhz = Some(clock);
                } else if nvml_session_lost(status) {
                    return true;
                }
            }
            if let Some(getter) = self.enforced_limit {
                let mut value = 0;
                let status = getter(self.gpu, &mut value);
                if status == 0 && value > 0 {
                    sample.enforced_limit_w = Some(f64::from(value) / 1000.0);
                } else if nvml_session_lost(status) {
                    return true;
                }
            }
        }
        false
    }
}
impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
        }
    }
}

/// The fan controller needs only one temperature. Keeping the full NVAPI and
/// NVML dashboard sessions resident added about 27 MB on the reference system.
/// This session uses only the public NVAPI thermal ABI, with its DLL and shutdown
/// lifetime retained on the same thread and an unambiguous single-GPU target.
pub(super) struct GpuTemperature {
    nvapi: Nvapi,
}
impl GpuTemperature {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            nvapi: Nvapi::new()?,
        })
    }
    pub fn sample(&self) -> Result<i32, String> {
        let mut thermal = Thermal {
            version: size_of::<Thermal>() as u32 | (2 << 16),
            ..Default::default()
        };
        // SAFETY: Live single-GPU session, public versioned repr(C) structure.
        let status = unsafe { (self.nvapi.thermal)(self.nvapi.gpu, 0, &mut thermal) };
        if status != 0 || !(1..=3).contains(&thermal.count) {
            return Err(format!(
                "NVAPI fan temperature unavailable: status {status}, count {}",
                thermal.count
            ));
        }
        thermal
            .sensors
            .iter()
            .take(thermal.count as usize)
            .find(|sensor| sensor.target == 1 && (1..=125).contains(&sensor.current))
            .map(|sensor| sensor.current)
            .ok_or("NVAPI GPU temperature missing or invalid".into())
    }
}

/// Enforced power limit and draw only, for "Keep preset". Open it for one
/// read in a disposable process, never retained in the dashboard: a retained
/// session crashed the dashboard inside `nvml.dll` (access violation while
/// reading device state; the trigger is not identified).
pub struct GpuLimitProbe {
    _library: Library,
    shutdown: StatusFn,
    gpu: Handle,
    enforced: NvmlValue,
    power: NvmlValue,
}
impl GpuLimitProbe {
    pub fn new() -> Result<Self, String> {
        let library = system_library("nvml.dll")?;
        // SAFETY: Documented NVML C signatures; the DLL lifetime is retained.
        unsafe {
            let init: StatusFn = *library.get(b"nvmlInit_v2\0").map_err(|e| e.to_string())?;
            let shutdown: StatusFn = *library.get(b"nvmlShutdown\0").map_err(|e| e.to_string())?;
            let device: NvmlDevice = *library
                .get(b"nvmlDeviceGetHandleByIndex_v2\0")
                .map_err(|e| e.to_string())?;
            let enforced: NvmlValue = *library
                .get(b"nvmlDeviceGetEnforcedPowerLimit\0")
                .map_err(|e| e.to_string())?;
            let power: NvmlValue = *library
                .get(b"nvmlDeviceGetPowerUsage\0")
                .map_err(|e| e.to_string())?;
            let status = init();
            if status != 0 {
                return Err(format!("NVML initialization: {status}"));
            }
            let mut gpu = ptr::null_mut();
            let status = device(0, &mut gpu);
            if status != 0 || gpu.is_null() {
                shutdown();
                return Err(format!("NVML GPU enumeration: {status}"));
            }
            Ok(Self {
                _library: library,
                shutdown,
                gpu,
                enforced,
                power,
            })
        }
    }
    /// (enforced limit, draw) in milliwatts.
    pub fn read(&self) -> Result<(u32, u32), String> {
        let (mut limit, mut draw) = (0, 0);
        // SAFETY: Live handle and exact documented u32 out parameters.
        let status = unsafe { (self.enforced)(self.gpu, &mut limit) };
        if status != 0 {
            return Err(format!("NVML enforced power limit unavailable: {status}"));
        }
        // SAFETY: As above.
        let status = unsafe { (self.power)(self.gpu, &mut draw) };
        if status != 0 {
            return Err(format!("NVML power unavailable: {status}"));
        }
        Ok((limit, draw))
    }
}
impl Drop for GpuLimitProbe {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
        }
    }
}

/// Board-agnostic one-shot NVML query of `(default_limit_mw, min_limit_mw, max_limit_mw)`
/// for verifying flashless GPU Dynamic Boost TGP ceiling changes after a
/// dGPU PnP restart on any discrete NVIDIA GPU.
pub fn read_nvml_power_limits() -> Result<(u32, u32, u32), String> {
    read_nvml_power_limits_for(None)
}

/// Board-agnostic one-shot NVML query of `(default_limit_mw, min_limit_mw, max_limit_mw)`
/// used by device-defaults discovery on any NVIDIA laptop.
pub(super) fn read_any_nvml_power_limits() -> Result<(u32, u32, u32), String> {
    read_nvml_power_limits_for(None)
}

fn read_nvml_power_limits_for(expected_name: Option<&str>) -> Result<(u32, u32, u32), String> {
    let nvml = Nvml::new(expected_name)?;
    let (mut default_mw, mut min_mw, mut max_mw) = (0u32, 0u32, 0u32);
    // SAFETY: Live NVML device handle and exact documented u32 out parameters.
    unsafe {
        let status = (nvml.default_limit)(nvml.gpu, &mut default_mw);
        if status != 0 {
            return Err(format!("NVML default power limit unavailable: {status}"));
        }
        let status = (nvml.constraints)(nvml.gpu, &mut min_mw, &mut max_mw);
        if status != 0 {
            return Err(format!("NVML power constraints unavailable: {status}"));
        }
    }
    Ok((default_mw, min_mw, max_mw))
}

/// Construct and use on a single worker thread. No unsafe Send/Sync implementation.
pub struct Nvidia {
    nvapi: Option<Nvapi>,
    nvml: Option<Nvml>,
    warnings: Vec<String>,
    session_lost: bool,
}
impl Default for Nvidia {
    fn default() -> Self {
        Self::new()
    }
}
impl Nvidia {
    pub fn new() -> Self {
        let mut warnings = vec![];
        let nvapi = match Nvapi::new() {
            Ok(api) => Some(api),
            Err(e) => {
                warnings.push(e);
                None
            }
        };
        let nvml = match Nvml::new(nvapi.as_ref().map(|api| api.name.as_str())) {
            Ok(api) => Some(api),
            Err(e) => {
                warnings.push(e);
                None
            }
        };
        Self {
            nvapi,
            nvml,
            warnings,
            session_lost: false,
        }
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn needs_reconnect(&self) -> bool {
        self.session_lost
    }
    pub fn sample(&mut self) -> Option<GpuSample> {
        if self.nvapi.is_none() && self.nvml.is_none() {
            self.session_lost = true;
            return None;
        }
        let (mut sample, mut lost) = self
            .nvapi
            .as_ref()
            .map(|api| api.sample())
            .unwrap_or_default();
        if let Some(nvml) = self.nvml.as_ref().filter(|_| !lost) {
            lost |= nvml.sample(&mut sample);
        }
        self.session_lost = lost;
        if lost {
            self.warnings.extend(sample.warnings.iter().cloned());
        }
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnects_on_lost_session_but_not_unsupported_sensors() {
        assert!(nvapi_session_lost(-10));
        assert!(nvapi_session_lost(-8));
        assert!(!nvapi_session_lost(-104));
        assert!(nvml_session_lost(15));
        assert!(nvml_session_lost(11));
        assert!(nvml_session_lost(18));
        assert!(nvml_session_lost(999));
        assert!(!nvml_session_lost(3));
    }
    #[test]
    fn public_nvapi_structures_have_exact_abi_sizes() {
        assert_eq!(size_of::<Thermal>(), 68);
        assert_eq!(size_of::<Clocks>(), 264);
        assert_eq!(size_of::<NvmlUtilization>(), 8);
        assert_eq!(std::mem::align_of::<NvmlUtilization>(), 4);
        assert_eq!(std::mem::offset_of!(NvmlUtilization, gpu), 0);
        assert_eq!(std::mem::offset_of!(NvmlUtilization, memory), 4);
    }
    #[test]
    fn refuses_to_combine_different_gpu_models() {
        assert!(same_gpu_name(
            "GeForce RTX 5070 Laptop GPU",
            "NVIDIA GeForce RTX 5070 Laptop GPU"
        ));
        assert!(same_gpu_name(
            "NVIDIA GeForce RTX 5070 Laptop GPU",
            "GeForce RTX 5070 Laptop GPU"
        ));
        assert!(!same_gpu_name(
            "GeForce RTX 5070 Laptop GPU",
            "NVIDIA GeForce RTX 5070 Ti Laptop GPU"
        ));
    }
}
