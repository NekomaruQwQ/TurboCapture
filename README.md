# Nekomaru LiveUI

Nekomaru's streaming infrastructure. **Portfolio demonstration only.**

## Platform Baseline

This project assumes the latest stable Windows 11 release, current GPU drivers,
and a modern DirectX runtime and hardware feature level. Compatibility with
older Windows releases, legacy drivers, or legacy GPU feature levels is out of
scope. Some graphics paths deliberately use Direct3D 11 interfaces because they
fit Windows Graphics Capture, shared textures, Media Foundation, and NVENC; that
API choice is not a promise of DirectX 11-era platform compatibility.

## ⚠️ WARNING

**DO NOT build or run this project.**

This software directly interfaces with low-level Windows APIs, DirectX 11 hardware pipelines, and GPU encoder firmware through unsafe native code. It performs raw memory-mapped I/O against your graphics hardware, spawns privileged child processes, and writes directly to system device buffers with no sandboxing.

It is designed for **one specific hardware configuration** and has **no safety checks** for any other environment. Running it on your hardware may cause:

- Unrecoverable GPU driver crashes
- Direct memory corruption via misconfigured DMA transfers
- Firmware-level damage to your video encoder hardware
- Cascading system instability leading to data loss

This is not a general-purpose tool. There are no build instructions and no support. **You have been warned.**

## License

GPLv3 — see [LICENSE](LICENSE).

© Nekomaru
