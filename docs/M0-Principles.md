# TurboCapture M0 Principles

**Status:** Authoritative for TurboCapture M0  
**Companion plan:** [`M0-Migration-Plan.md`](M0-Migration-Plan.md)  
**Milestone numbering:** TurboCapture begins at M0; LiveUI milestone numbers do not continue here.

## Purpose

TurboCapture is Nekomaru's personal livestream capture utility. Its two defining capabilities are:

1. Selecting a window through deterministic, policy-based rules.
2. Capturing native Windows video and streaming opaque encoded frames to a browser canvas.

The program exists to support Nekomaru's own livestreaming workflow. Public use is intentionally discouraged because the behavior and visual result are part of the stream's identity. M0 is therefore allowed to be narrow, machine-specific, and opinionated.

These principles are design constraints, not temporary shortcomings to compensate for. A proposed feature that conflicts with them should be rejected or deferred unless this document is deliberately revised.

## 1. Exact Environment, Not a Platform Matrix

TurboCapture targets exactly the current livestreaming machine:

- The installed Windows version and current system configuration.
- The installed GPU, display adapters, drivers, encoders, and their known capabilities.
- The actual display topology, hardware names, audio-independent capture workflow, and browser environment used for livestreaming.
- The current Rust, Bun, browser, and graphics toolchains used from this repository.

Other operating systems, older Windows versions, fallback GPUs, unknown adapters, software encoders, and degraded feature sets are unsupported. A mismatch in a required hardware or platform invariant is a fatal startup error with a useful diagnostic. M0 does not add compatibility layers, capability negotiation, or fallback implementations for environments that will not be used.

Hardware identifiers and capabilities may be explicit configuration or constants when that makes the intended environment clearer. They should still be validated at startup so a machine change fails immediately rather than producing subtly incorrect capture.

## 2. Source-Run, Single-Machine Deployment

TurboCapture is always built and run from source. M0 does not require:

- Installers, release archives, automatic updates, or distribution packaging.
- A stable installation layout or globally discoverable executable.
- Cross-compilation or binaries for other machines.
- A production static-file server for the viewer.

All native components run on the livestreaming machine. A browser viewer may run on the same machine or another device on the trusted LAN. Runtime configuration may therefore use explicit executable paths, addresses, and ports that are meaningful only in this environment.

## 3. Trusted Network

The local machine and LAN are trusted. M0 has no authentication, authorization, TLS termination, user accounts, tenant isolation, secret management, or hostile-client hardening.

HTTP and WebSocket services should expose only the small surface needed by the utility, but they do not need security infrastructure designed for an untrusted network. Cross-origin access required by the independently hosted viewer should be allowed explicitly. Binding beyond the trusted LAN is an operator error, not a supported deployment mode.

## 4. Clarity Before Generality

The implementation prioritizes, in order:

1. Correct behavior in the actual livestreaming workflow.
2. Clear ownership and understandable control flow.
3. Maintainability and ease of diagnosis.
4. Sufficient latency and throughput on the target hardware.

Generic frameworks, plugin systems, abstract backend hierarchies, dynamic discovery, distributed orchestration, and speculative optimizations are not goals. Add abstraction only after a second real use demonstrates it. Measure hot paths on the target machine before optimizing them.

The same rule applies to concurrency: use the fewest ownership domains and bounded communication paths that satisfy Windows API constraints and keep the async network runtime responsive.

## 5. No Migration Compatibility Burden

A stable pre-split LiveUI branch remains available for livestreaming while TurboCapture is built. The migration does not need to preserve compatibility with the combined repository or keep old and new pipelines operational together.

Consequently:

- No compatibility adapters, dual protocols, feature flags, or temporary bridge processes are required.
- Intermediate commits should keep the code being developed understandable and testable, but old LiveUI binaries need not remain runnable.
- Old components may be removed as soon as their useful behavior has been migrated and verified.
- Historical designs remain available through version control; they do not need active archive copies in the final source tree.

## 6. Process-per-Stream Ownership

Each `capture-windows` process owns exactly one logical video stream and at most one active Windows capture session at a time. The process owns the entire native media path for that stream:

```text
window observation and selection
  -> WGC session
  -> D3D11 crop / resample / format conversion
  -> Media Foundation H.264 encoder
  -> in-process HTTP and WebSocket service
```

The process lifecycle is the stream lifecycle:

- Start a process to create a stream.
- Kill or exit the process to stop it.
- Start multiple processes for multiple independent streams.
- Restart one failed stream without disturbing the others.

There is no internal stopped mode and no multi-stream native manager inside `capture-windows`. Target loss is different from stream shutdown: a running process may wait for policy to select another eligible window while continuing to expose status.

`capture-control` is a separate controlling surface. It starts and kills `capture-windows` processes and communicates with running instances only through their REST APIs. Detailed control-surface behavior is intentionally outside the M0 migration plan.

## 7. Three Deliberate Crate Boundaries

### `capture-core` library

`capture-core` owns platform-independent concepts shared by a capture instance:

- Configuration, validation, and shared data structures.
- Pure auto-selector policy over already-observed window facts.
- Private HTTP, WebSocket, status, and video message types.
- The platform-independent Axum router and handlers.
- The Clap argument definitions used by `capture-windows`.

It must not enumerate windows, call Win32, own D3D or Media Foundation objects, or spawn capture processes. Its policy and API behavior must be testable without a desktop or GPU.

### `capture-windows` binary

`capture-windows` owns everything tied to the Windows graphics and media device:

- Runtime window observation and conversion to `capture-core` facts.
- COM initialization, WGC sessions, and target switching.
- D3D11 device selection and GPU resources.
- Crop, resample, color-format conversion, and opaque H.264 encoding.
- Hosting a `capture-core` Axum service for its one stream.

It uses the CLI definition from `capture-core`; it does not maintain a second argument model.

### `capture-control` binary

`capture-control` owns orchestration and human-facing control. It treats each `capture-windows` instance as an opaque child service. Except for knowing how to locate the `capture-windows` executable, it must not depend on Windows capture or graphics behavior.

Its UI, persistence model, and process supervision policy will be planned separately and are not blockers for the M0 capture pipeline.

## 8. Opaque Native Video, Browser-Owned Transparency

TurboCapture transports only opaque video. The native pipeline may crop, resample, and convert formats required for capture and encoding, but it does not encode an alpha channel or invent a transparency transport.

Transparency-producing presentation work remains in the browser canvas, as in the existing viewer:

- Color keying and alpha generation.
- Matte shaping and edge treatment.
- Unspill or other operations whose result requires transparency.
- Composition into the final transparent canvas consumed by LiveUI or OBS.

The capture instance sends the viewer the render configuration needed for those operations. This configuration is private to TurboCapture and may evolve together with the viewer.

This boundary deliberately removes side-by-side color/alpha packing, dual-stream alpha synchronization, unusual coded dimensions, and native alpha reconstruction from M0.

## 9. Private, Direct Communication

Each `capture-windows` instance serves its own REST status/configuration surface and video WebSocket. There is no relay, aggregation server, stdout media protocol, shared cross-process texture, or service-discovery layer in the media path.

The control surface uses REST only. The viewer connects directly to the selected capture instance for video and render configuration. A configured address and port identify an instance; M0 does not require globally stable stream IDs or dynamic discovery.

The protocol is private and versioned only as needed to keep the repository's native and browser code coherent. Backward compatibility across revisions is not a requirement.

## 10. Configuration and Failure Semantics

Configuration updates are complete replacements, not partially applied patches. `capture-core` validates a candidate before making it visible to the media owner. An invalid update returns a useful error and leaves the last valid configuration active.

Settings fall into two categories:

- Startup settings, such as listen address, port, and hardware adapter identity, take effect by restarting the process.
- Live settings, such as selection rules, crop, output geometry where supported, and browser render parameters, may be replaced while the process runs.

M0 distinguishes expected runtime conditions from broken invariants:

- No eligible target, target closure, and viewer disconnect are recoverable conditions.
- Invalid live configuration is rejected without disturbing the active configuration.
- Unsupported hardware, required-device mismatch, server bind failure, unrecoverable D3D failure, and unrecoverable encoder failure terminate the instance with a non-zero exit.

Automatic process restart is not required. Manual restart is sufficient until the separate `capture-control` design chooses otherwise.

## 11. Explicit Non-Goals for M0

M0 does not attempt to provide:

- Public distribution or a supported third-party user experience.
- Authentication, encryption, or untrusted-network exposure.
- Platform, GPU, encoder, or browser portability.
- Audio capture, keystroke metrics, widgets, tokens, marquees, or other LiveUI features.
- A native preview or embedded webview.
- A generalized multi-stream process.
- Transparent video encoding.
- Compatibility with the pre-split LiveUI runtime or wire protocols.
- Automatic restart, high availability, telemetry infrastructure, or production operations tooling.
- A stable external API or SDK.

These omissions are what keep TurboCapture small enough to remain a dependable personal utility.
