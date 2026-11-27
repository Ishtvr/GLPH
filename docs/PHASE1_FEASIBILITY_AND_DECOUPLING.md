# PerformanceHub 2.0 — Phase 1 Hardware Feasibility & Decoupled Architecture

Measured live on reference hardware (**HP OMEN 16 `8E35 / BIOS F.13`**, AMD Ryzen 9 8940HX `SMU 0x546F00`, PM-Table `0x540208`, NVIDIA GeForce RTX 5070 Laptop `621.40`, Secure Boot **ON**, Windows Defender **100% enabled**, zero BIOS flashes).

---

## 1. Phase 1 Empirical Hardware Results (27 Nov 2026)

| Control Path | Tested Operation | Empirical Result | Decision in PerformanceHub 2.0 |
|---|---|---|---|
| **Direct GPU Power Limit (TGP)** via `0000\vbios` flat `Policy[2]` | Patch `limitRated = limitMax = tgpRated = inflection[0] = target_w` (without HP Unleashed `0x1A` or Dynamic Boost) | **115 W fixed:** NVML `115/5/115 W`, live burn `114.4–115.4 W` (`2145–2340 MHz`, `61.3 °C`).<br>**135 W fixed:** NVML `135/5/135 W`, live burn `134.5–138.4 W` (`2242–2482 MHz`, `68.1 °C`). | **Shipped as primary GPU power path (`50–135 W`, ceiling up to `175 W` in Settings).** Zero HP WMI (`0x1A`/`0x22`/`0x29`) required! |
| **NVPCF Interference** (`nvpcf.sys`, `ACPI\NVDA0820\NPCF`) | Tested both `0x0503` (bits 2 & 9 cleared) and `0x0507` (bit 9 cleared only) under live combined GPU+CPU load | Clearing bit 2 (`0x0503`) triggers a `-15 W` `nvlddmkm` fallback (`135 -> 120 W`). Clearing **only bit 9 (`0x0507`)** preserves full **`135.00 W`** GPU TGP while completely stopping CPU TDP (`80 W`) overwrites! | **Shipped in `nvpcf_ppt.rs` (`CpuOnly = 0x0507`)**, auto-restored on Restore. |
| **CPU Fast PPT** (`RSMU 0x56`) | Write `110 W` (`110_000 mW`) | PM cell 2 & cell 0 (`STAPM`) immediately read **`110.0 W`** — completely independent of GPU settings. | **Shipped (`15–160 W` slider)** in CPU section. |
| **CPU Slow PPT > 80 W** (`RSMU 0xCB`, `MP1 0x5F/0x60/0x4F`, HP `0x29`) | Write `85 W` and `90 W` | **Hard SMU firmware clamp:** every SMU mailbox & HP WMI path returned `status=1` while PM cell 4 stayed pinned at **`80.001 W`**. | **Honest `15–80 W` slider** in CPU section (no fake `> 80 W` range). |
| **CPU Slow PPT < 45 W** (`RSMU 0xCB`, `MP1 0x5F`) | Write `35 W` while Fast PPT = `110 W` | PM cell 4 immediately read **`35.0 W`** while Fast PPT remained **`110.0 W`**. | **Fully supported (`15–80 W`)** with zero HP `45 W` floor. |
| **CPU TDC > 108 A & EDC > 160 A** (`RSMU 0x57/0x58`, `MP1 0x3C/0x3D`) | Write `+5 A` (`113 A TDC`, `165 A EDC`) and downward (`100 A TDC`, `150 A EDC`) | Downward works immediately (`100.0 A`, `150.0 A`). Upward is **hard-clamped in signed SMU `0x546F00`** at **`108.001 A TDC`** and **`160.001 A EDC`**. | **Honest `30–108 A TDC` & `30–160 A EDC`** with transparent SMU cap info text. |
| **CPU Max Boost Clock (`Fmax`)** (`RSMU 0x70` / `0x6E`) | Write & readback `4350..=5550 MHz` (stock `5350 MHz`, `+200 MHz` max) | Live verified (`5550 MHz` request reads back `5549 MHz`, `±2 MHz` tolerance). | **Shipped (`Feature::CpuBoostLimitMhz`)** in CPU Boost & PBO group. |

---

## 2. Decoupled Multi-Vendor Architecture

1. **Zero GPU ↔ CPU Coupling:**
   - The Overview page is cleanly divided into **4 independent top-level sections**: **GPU**, **CPU**, **Thermals / Temperature**, and **Windows**.
   - Direct GPU Power Limit (`50–135 W+`) no longer depends on HP Unleashed (`0x1A`), `cTGP`, `Dynamic Boost (PPAB)`, or `Shared Budget (TPP)`.
   - On HP systems using Direct Fixed GPU TGP without legacy Dynamic Boost, PerformanceHub sets `PPAB = 0` (preserving `cTGP` and the live GPU thermal target) so the GPU receives its full requested wattage even under heavy CPU load.
   - Applying a clean PerformanceHub preset on non-HP hardware dispatches **zero HP WMI commands**.
2. **Follow-Up Live Proofs (27 Nov 2026 — 140 W GPU + 45 W CPU & Thermal Controls):**
   - **140 W Direct Fixed GPU + 45 W CPU Combined Burn (`AC=True`):**
     - With `PPAB = 1` (Dynamic Boost on): under simultaneous GPU + 6-thread CPU load (`45 W → 39.2 W`), GPU enforced climbed **`125.00 → 130.13 W`**, real draw hit **`129.43 W` at `2640–2647 MHz` (`67.4 °C` GPU, `36 °C` IR chassis)** with **`0 mW` battery drain**.
     - With `PPAB = 0` (`HP 0x22 = [1, 0, 1, 87]`): GPU enforced jumped immediately to a **flat `140.00 W`** even while CPU simultaneously drew **`64.1 W`** (`0 mW` battery drain).
   - **GPU Max-Temperature (`GPS`, `Feature::GpuTemperatureLimitC`):** Writing `HP 0x22` byte 3 (`87 → 83 → 87 °C`) verified live on **both** HP WMI `0x21` byte 3 (`83 °C`) and NVIDIA `NVML GPS_CURR(7)` (`83 °C`). Shipped as **`65–87 °C`** slider in **Thermals / Temperature**.
   - **Chassis Max-Temperature (`IR 0x23`, `Feature::CpuSkinTemperatureLimitC`):** Live IR surface sensor (`0x23`) read `36 °C` steady under full 130 W GPU + 45 W CPU burn. Shipped as **`35–55 °C`** slider in **Thermals / Temperature**, persisted in `chassis-max-temp.json` and enforced live by `fan_service.rs`.
   - **External-Monitor-Safe GPU Restart (`vbios_tgp.rs`):** Raised `DISABLE_SETTLE` to `2000 ms` and `POST_ENABLE_SETTLE` to `1000 ms` to avoid `CM_PROB_FAILED_POST_START` (Code 43) on external displays during VBIOS power-limit switches.
2. **Cross-Vendor Readiness (ASUS, MSI, Lenovo, HP, ...):**
   - **GPU (`vbios_tgp.rs`, `gpu_support.rs`, `nvpcf_ppt.rs`):** Dynamically locates any active discrete NVIDIA PCI adapter (`VEN_10DE`), parses its own Power Policy Table v4.0 structurally, validates monotonic invariants, and verifies every write via NVML readback.
   - **CPU (`smu.rs`, `cpu_tuning.rs`):** Qualifies on AMD Ryzen + signed PawnIO driver + signed `RyzenSMU.bin` hash + Raphael/Dragon Range SMU family (`0x54xxxx`) + PM-table `0x540208` contract, without locking to HP board/BIOS strings.
   - **Vendor Isolation:** HP-specific controls (4-zone RGB, HP Max-Fan/Fan Curve, HP BIOS MUX, optional legacy HP mode prelude, `HPOmenCap`) remain cleanly gated to HP hardware so non-HP laptops run GPU, CPU, and Windows tuning cleanly.
   - **Isolated Storage:** Uses `%USERPROFILE%\.performance-hub` so journals, presets, and VBIOS backups never collide with an existing `OmenHub++` installation.
