//! Device evidence for NVIDIA clock/curve writes.
//! Multi-vendor ready: accepts any discrete NVIDIA GPU with a loaded driver and
//! readable VBIOS string; runtime NVML/NVAPI ABI checks, capture, readback and
//! full restore protect every write independently of laptop vendor.
use super::gpu::NvidiaIdentity;
use glph_core::Inventory;

pub(super) fn driver_evidence(identity: &NvidiaIdentity) -> Result<(), String> {
    let name = identity.name.to_ascii_lowercase();
    let is_nvidia = name.contains("nvidia")
        || name.contains("geforce")
        || name.contains("rtx")
        || name.contains("gtx");
    if !is_nvidia || identity.vbios.trim().is_empty() {
        return Err(glph_core::message::msg("support.err.gpu_vbios", &[]));
    }
    if identity.driver.trim().is_empty() {
        return Err(glph_core::message::msg("support.err.driver", &[]));
    }
    Ok(())
}

pub(super) fn model_evidence(
    _inventory: &Inventory,
    identity: &NvidiaIdentity,
) -> Result<(), String> {
    driver_evidence(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(driver: &str) -> NvidiaIdentity {
        NvidiaIdentity {
            uuid: "test-device".into(),
            name: "NVIDIA GeForce RTX 5070 Laptop GPU".into(),
            vbios: "98.06.2a.40.da".into(),
            driver: driver.into(),
        }
    }

    #[test]
    fn multi_vendor_nvidia_gpus_and_loaded_drivers_are_accepted() {
        let hp = Inventory {
            manufacturer: "HP".into(),
            board: "8E35".into(),
            bios: "F.13".into(),
            ..Default::default()
        };
        let asus = Inventory {
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            board: "G614JZ".into(),
            bios: "321".into(),
            ..Default::default()
        };
        for driver in ["617.14", "617.42", "621.40", "625.00"] {
            assert!(model_evidence(&hp, &identity(driver)).is_ok());
            assert!(model_evidence(&asus, &identity(driver)).is_ok());
        }
        for driver in ["", " "] {
            assert!(model_evidence(&hp, &identity(driver)).is_err());
        }
        let mut missing_vbios = identity("621.40");
        missing_vbios.vbios = " ".into();
        assert!(model_evidence(&hp, &missing_vbios).is_err());
        let mut non_nvidia = identity("621.40");
        non_nvidia.name = "AMD Radeon Graphics".into();
        assert!(model_evidence(&hp, &non_nvidia).is_err());
    }
}
