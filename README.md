# PerformanceHub

A lightweight, native Windows hardware performance & tuning tool for gaming laptops (starting on HP OMEN and expanding to vendor-agnostic AMD/NVIDIA/Windows control across ASUS, MSI, and more), written in **Rust** and **Slint**.

**In development · `2.0.0-alpha.1` · Windows x64**

## Goals & Architecture

- **Decoupled Power Control:** Independent **GPU**, **CPU**, and **Windows** sections on the Overview page — no cross-domain locks or hidden coupling between GPU power limits and CPU PPT/Slow/TDC/EDC/Curve Optimizer settings.
- **Direct Hardware Paths:** Prioritizes vendor-agnostic interfaces (**PawnIO / AMD SMU**, **NVIDIA NVML / `nvlddmkm`**, and **Windows Power/Display APIs**) over proprietary OEM WMI modes wherever verified safe.
- **Optional Vendor Modules:** OEM-specific features (such as HP fan curves, Max-Fan, and 4-zone RGB) remain cleanly isolated so PerformanceHub can run on other laptop vendors without blocking core GPU/CPU/Windows controls.
- **Safety & Transparency:** Zero telemetry, Defender-safe, Secure-Boot-compatible, explicit readback and rollback journals.

## Build

Install Rust for Windows x64 with the MSVC build tools and Windows SDK, then run:

```powershell
cargo build --release
.\target\release\performance-hub.exe
```

## License & Disclaimer

[MIT](LICENSE) **Use at your own risk.**
