//! Embeds Windows version metadata into `glph-service.exe`.
#[allow(dead_code)]
#[path = "../app/build_support/win_resource.rs"]
mod win_resource;

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../app/build_support/win_resource.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = [0u16; 4];
    for (slot, part) in parts.iter_mut().zip(version.split(['.', '-', '+'])) {
        *slot = part.parse().unwrap_or(0);
    }
    let resources = vec![win_resource::version_resource(&win_resource::VersionInfo {
        version: parts,
        prerelease: !env!("CARGO_PKG_VERSION_PRE").is_empty(),
        strings: &[
            ("FileDescription", "GLPH Hardware & Cooling Service"),
            ("FileVersion", version),
            ("InternalName", "glph-service"),
            (
                "LegalCopyright",
                "GLPH (GamingLaptopPerformanceHub) contributors; third-party notices included",
            ),
            ("OriginalFilename", "glph-service.exe"),
            ("ProductName", "GLPH"),
            ("ProductVersion", version),
        ],
    })];
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("glph-service.res");
    fs::write(&output, win_resource::res_file(&resources)).expect("Write service resource file");
    println!("cargo:rustc-link-arg-bins={}", output.display());
}
