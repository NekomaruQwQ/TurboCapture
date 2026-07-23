# Nekomaru LiveUI — M4 Architecture Design

**Date**: 2026-03-18 (designed), 2026-03-23 (updated)
**Authors**: Nekomaru + Claude
**Status**: Foundation document — captures the design discussion that will guide M4 development. All open questions resolved.

---

## Table of Contents

- [Architectural Evolution](#architectural-evolution)
- [The Problem That Started It All](#the-problem-that-started-it-all)
- [Design Journey](#design-journey)
- [M4 Final Architecture](#m4-final-architecture)
- [Component Design](#component-design)
- [IPC Protocols](#ipc-protocols)
- [Server Design](#server-design)
- [Distributed Deployment](#distributed-deployment)
- [Building Blocks from M3](#building-blocks-from-m3)
- [Design Principles](#design-principles)
- [Resolved Design Decisions](#resolved-design-decisions)

---

## Architectural Evolution

Each milestone unlocked capabilities that the previous one couldn't support.

### M0: Prototype

Auto-selector only — the first proof of concept. Captured the foreground window and streamed it. No multi-stream, no frontend UI beyond a basic viewer.

### M1: Monolith (single Rust/wry process)

Everything in one process: window capture, GPU encoding, HTTP protocol, webview rendering.

**Limitations:**
- Can't view in a normal browser (wry custom protocol only)
- Can't run multiple captures (single encoding thread)
- Can't iterate on the frontend without recompiling Rust
- Can't develop frontend without the full Rust app running

### M2: Client-Server with TS Server

Split into frontend (React/Vite) + server (TypeScript, Hono/Bun) + Rust child processes (live-video, live-audio) + Rust webview host (live-app). The longest-lived architecture.

**What it unlocked:**
- Browser-based viewer — the frontend can run anywhere
- live-app (wry webview) on Machine A, everything else on Machine B
- Frontend HMR via Vite — fast iteration without Rust recompilation
- Face capture (via OBS) on the streaming machine, freeing the working machine from that CPU load

**Limitations:**
- TS server had hand-written binary parsers (`protocol.ts`, `audio-protocol.ts`, `kpm-protocol.ts`) mirroring Rust lib crate types — protocol duplication
- Window enumeration required shelling out to `live-video.exe --enumerate-windows`
- KPM was a child process (`live-kpm.exe`) with 12-byte binary IPC

### M3: Full RIIR (current)

Server rewritten in Rust (Axum). All protocol duplication eliminated — server calls `live_video::read_message()` directly. KPM merged in-process (`WH_KEYBOARD_LL` hook on a dedicated message pump thread). Window enumeration via direct library call. Single Rust workspace.

**What it unlocked:**
- Zero protocol parsing code to maintain
- In-process KPM with `AtomicU32` — no IPC, no serialization
- `enumerate_windows()` as a library call — no child process spawn
- `tokio::sync::RwLock` for read-heavy frame polling (N clients, 1 writer at 60fps)

**Remaining limitations:**
- Every window switch kills and respawns `live-video.exe` — NVENC pipeline teardown (~100ms gap + frontend decoder reinit)
- Server contains unsafe Win32 code (KPM keyboard hook, window enumeration)
- All capture processes must be co-located with the server (stdout pipes)
- live-audio (WASAPI → network → browser) is fragile — choppy audio is a disaster for livestreaming

### M4: Microservices (proposed)

Independent workers communicating via HTTP/WS. Each component is a standalone executable with no assumptions about who spawned it or where the other components run.

**What it unlocks:**
- Seamless window switches — encoder persists, only the capture session is swapped
- Distributed deployment — capture on one machine, server + viewer + OBS on another
- YouTube Music can run on the streaming machine — OBS captures audio directly, eliminating live-audio entirely
- Multiple capture workers on different machines, all feeding the same server
- Server returns to TypeScript — the RIIR rationale no longer applies
- Each component independently testable and deployable

---

## The Problem That Started It All

When the auto-selector switches the captured window, the server kills the old `live-video.exe` and spawns a new one. This tears down the entire pipeline:

1. NVENC session teardown + reinit (~50-100ms)
2. New SPS/PPS → generation bump → frontend decoder reinit
3. Visible freeze during transition

The original question: **can we keep one encoder process per stream ID and just swap what it captures?**

---

## Design Journey

This section traces how we arrived at the final architecture, preserving the reasoning behind each decision.

### Step 1: Separate Capture from Encoding

**Idea**: Split `live-video.exe` into capture (GPU) and encoding (NVENC). The capture process passes frames to the encoder via a DX11 shared texture. On window switch, only the capture session is replaced — the encoder keeps running.

**Key insight**: The server already specifies a fixed output resolution per stream. The resampler always outputs to the same dimensions regardless of source window. So the encoder's input format never changes across switches — no MFT reconfiguration needed.

**Shared texture sync**: DX11 `IDXGIKeyedMutex` — alternating keys (producer acquires 0, writes, releases 1; consumer acquires 1, reads, releases 0).

### Step 2: Introduce live-core

**Concern**: Moving capture into the server would put unsafe Win32/GPU code in the server process. A crash in WinRT Capture or D3D11 would take down the entire HTTP server.

**Solution**: A new `live-core.exe` process takes ALL Win32/GPU responsibility: D3D11 device, window enumeration, capture, resampling/cropping, foreground polling, selector logic, YTM manager, KPM keyboard hook, DPI awareness.

**Benefit**: The server becomes pure safe Rust — zero unsafe code, maximum stability.

### Step 3: Server Spawns Everything (P2P Pipes)

**Question**: Should live-core spawn live-video, or should the server spawn both?

**Decision**: Server spawns all processes with point-to-point pipes. The server is the only component that understands the full topology. live-core doesn't need to know live-video exists. Each process has its own pipe pair to the server.

### Step 4: Core Becomes Autonomous

**Refinement**: live-core doesn't need many commands from the server. It should auto-start the selector, YTM manager, and KPM on boot. The server only needs to send config changes.

**Further refinement**: The server doesn't even need to send config changes — core can **poll** the server for config on the same 2s interval it already polls the foreground window.

**Result**: Server → core commands reduced to just `set_selector_config` and `set_selector_preset`. Then reduced to zero — core polls `GET /api/v1/streams/auto/config` from the server.

### Step 5: Fixed Textures, No Stdin for Video

**Simplification**: Each stream ID has a fixed resolution hardcoded in live-core. Shared textures are created once on startup and never changed. This eliminates texture swapping, `set_texture` commands, and dynamic handle passing.

**Consequence**: live-video needs no stdin at all — everything is CLI args (`--shared-handle`, `--width`, `--height`). If device lost, kill and respawn everything.

**Principle established**: No internal start/stop state. If the process is running, it's active. Kill to stop.

### Step 6: Core as HTTP Client

**Question**: What IPC between core and server?

**Options considered**:
1. ~~Shared stdin/stdout bus~~ — pipe reads are destructive (no broadcast), large writes interleave
2. ~~Multiplexed stdout~~ — core envelopes all child output (adds copy + latency)
3. ~~P2P pipes~~ — works but couples core to being a child process
4. **HTTP** — core is an HTTP client, POSTs events, polls config

**Decision**: HTTP. The server is already an HTTP server. Core's traffic is low-frequency (events every 2s, KPM on change). This makes the server a pure HTTP surface — one interface for everything.

**Bonus**: Core can run independently on any machine. Just point it at the server URL.

### Step 7: WebSocket for Video/Audio

**Question**: If core uses HTTP, should video/audio also move from pipes to WebSocket?

**Analysis**: Pipes require video to be a child process of the server (co-located). WebSocket enables video to push frames from any machine.

**The decisive insight**: In the current setup, live-app already runs on Machine A while everything else runs on Machine B. If video uses WebSocket, we can move the server to Machine A too — leaving only the capture + encoder on Machine B. This means the streaming machine (with OBS) runs the server, and the working machine only runs what needs direct GPU/window access.

**Decision**: WebSocket for video/audio. The server becomes a thin WS relay:
- Receive binary frame from encoder WS
- Cache codec params + last keyframe (for late-joining clients)
- Fan-out to all frontend WS clients
- No circular buffer, no sequence numbers, no cursor logic

### Step 8: Multiple Instances

**Realization**: If capture workers are just HTTP/WS clients, nothing prevents multiple instances on different machines, all talking to the same server.

**Deployment unlocked**:
- Machine A (streaming): server + `live-capture --mode crop` for YouTube Music (local) + OBS
- Machine B (working): `live-capture --mode auto` for main stream (remote WS to Machine A)

**YouTube Music audio**: If YouTube Music runs on Machine A (the OBS machine), OBS captures system audio directly. No live-audio needed. No network audio transfer. No chopping.

### Step 9: Merge Encoder Back into Capture

**Realization**: With WebSocket output, the original reason to split capture from encoding across processes (stdout pipes required co-location with server) no longer applies. Capture and encoding can be in the same process — the "bakery model" stays in-process exactly as M3 works today.

**Window switch is still seamless**: hot-swap the CaptureSession, encoder never restarts. Same staging texture, same encoding thread.

**Result**: No shared textures needed. No keyed mutex. No cross-process GPU coordination. Much simpler.

### Step 10: Unified Binary with Capture Modes

Instead of one parameterized `live-core` or separate binaries per behavior, a single `live-capture` binary with `--mode` selects the behavior:

- **`--mode base`** (default, implied when `--mode` is omitted): Basic capture unit. Takes HWND, resolution, outputs H.264 via WS. The foundation that other modes build on.
- **`--mode auto`**: `base` + auto-selector support. Polls foreground window, matches patterns from server config, hot-swaps capture session. For the "main" stream.
- **`--mode crop`**: `base` + crop region extraction. Takes absolute bounding box coordinates instead of resample dimensions. For YouTube Music and similar fixed-region captures.
- **`live-kpm`**: Remains a separate binary — different domain (keyboard hook, no GPU), different communication pattern (HTTP POST, not WS).

Each mode is a self-contained worker. No inter-process GPU sharing.

### Step 11: Shared Protocol Crate (live-protocol)

All components need a common binary framing protocol. Rather than each crate defining its own wire format (as in M3, where `live-video/src/lib.rs` defined the stdout protocol), a shared `live-protocol` lib crate provides a single 8-byte aligned frame header and message type enum used by all producers and consumers.

The header is 4-byte aligned (unlike M3's 5-byte header) for clean DataView access:

```
Offset  Field            Size    Notes
0       message_type     u8      0x01=CodecParams, 0x02=Frame, 0x10=KpmUpdate, ...
1       flags            u8      bit 0: IS_KEYFRAME (video), bits 1-7: reserved
2       reserved         u16     zero for now
4       payload_length   u32 LE
```

Metadata like `is_keyframe` moves from the payload into the header `flags` field, so routing decisions (in `live-ws` and the server) only need to read bytes 0-1 — no payload inspection.

### Step 12: Stdout-First Producers + live-ws Relay

**Insight**: Capture and KPM processes don't need to know about WebSocket. They write `live-protocol` framed messages to stdout. A separate `live-ws` binary reads stdin and relays each message as a WS binary message to the server.

```
live-capture ... | live-ws --mode video --server ws://machineA:3000/.../input
live-kpm        | live-ws --server ws://machineA:3000/.../kpm/input
```

**Benefits**:
- Producers have one code path (stdout). No WS client, no reconnect logic, no networking dependencies.
- `live-ws` is reusable for any framed stream. One reconnect/backoff implementation.
- Testing: `live-capture > dump.bin` IS the production code path.
- Clean separation of concerns: capture = GPU/encoding, relay = networking.

**`live-ws --mode video`** additionally caches the last `CodecParams` message and last keyframe (identified via header `message_type` and `flags.IS_KEYFRAME`). On WS reconnect, it replays cached messages before resuming the live stream, so the server immediately has valid codec state.

### Step 13: Remove live-audio

live-audio (WASAPI capture → network → browser playback) has been the most fragile part of the project. Choppy audio is a disaster for livestreaming.

With YouTube Music on the streaming machine, OBS captures audio directly from the system. No network transfer, no audio worklet, no ring buffer underruns.

**live-audio is eliminated entirely.**

### Step 14: TypeScript Server

The M3 RIIR (Rewrite It In Rust) rationale for the server was:
1. Eliminate protocol duplication (TS had hand-written binary parsers mirroring Rust types)
2. Direct library calls (`live_video::read_message()`, `enumerate_windows()`)
3. Single process (server + children share Rust types)

In M4, **all three reasons are gone**:
1. Server doesn't parse binary protocols — it relays opaque bytes
2. No library calls — all communication is HTTP/WS
3. Microservice architecture — each service is its own process

A TypeScript server (Bun/Hono) would offer:
- Faster iteration on the HTTP/WS relay logic
- Native Vite/frontend integration
- Portfolio demonstration (full-stack range)
- The video relay is essentially `ws.on("message", frame => broadcast(frame))`

---

## M4 Final Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Machine A (Streaming)                                            │
│                                                                  │
│   live-capture --mode crop | live-ws ──WS──→ ┌──────────┐      │
│   (youtube-music, local)                      │          │      │
│                                               │  Server  │      │
│                              WS (from LAN) ─→ │  (TS)    │ ──WS─→ Frontend
│   live-capture --mode auto | live-ws ─────┘  │          │       │  / OBS
│   (main, Machine B)                           │          │      │
│                              WS (from LAN) ─→ │          │      │
│   live-kpm | live-ws ────────────────────┘    └──────────┘      │
│   (Machine B)                                      ↑             │
│                                         HTTP (config poll)       │
│   YouTube Music ← audio captured by OBS            │             │
│   OBS ← captures everything                       │             │
│   live-app (wry webview)                           │             │
└────────────────────────────────────────────────────┼─────────────┘
                                                     │
┌────────────────────────────────────────────────────┼─────────────┐
│ Machine B (Working)                                │             │
│                                                    │             │
│   live-capture --mode auto ─→ stdout ─→ live-ws ──WS──→ machineA│
│   (foreground polling, hot-swap encoder)                         │
│                                    HTTP (config poll) ──→ machineA│
│                                    HTTP (streamInfo)  ──→ machineA│
│                                                                  │
│   live-kpm ─→ stdout ─→ live-ws ──WS──→ machineA                │
│                                                                  │
│   ┌──────────┐                                                   │
│   │ Coding   │                                                   │
│   │ Sessions │                                                   │
│   └──────────┘                                                   │
└──────────────────────────────────────────────────────────────────┘
```

### Component Summary

| Component | Language | Role | Input | Output |
|---|---|---|---|---|
| **live-protocol** | Rust (lib) | Shared binary frame header + message types | — | Used by all Rust crates |
| **live-capture** | Rust | Capture + encode (base/auto/crop modes) | CLI args (mode, HWND, resolution) | stdout (live-protocol framed) + HTTP POSTs (auto mode metadata) |
| **live-kpm** | Rust | Keystroke counter | — | stdout (live-protocol framed) |
| **live-ws** | Rust | stdin → WS relay | stdin (live-protocol framed) | WS binary messages to server |
| **Server** | TypeScript (Bun/Hono) | HTTP/WS hub, relay, string store, config | WS (video, kpm), HTTP (worker events) | WS (frontend), HTTP API |
| **live-app** | Rust (wry) | Webview host | CLI args (URL) | — |
| **Frontend** | React + Vite | Viewer UI | WS (video, strings, kpm) | — |

---

## Component Design

### live-protocol (Rust, lib crate)

Shared binary framing protocol used by all Rust components. Defines the 8-byte frame header, message types, flags, and read/write helpers.

**Types**:
- `MessageType` enum (`#[repr(u8)]`): `CodecParams = 0x01`, `Frame = 0x02`, `KpmUpdate = 0x10`, `Error = 0xFF`
- `Flags` constants: bit 0 = `IS_KEYFRAME`
- `FrameHeader`: `message_type: u8`, `flags: u8`, `reserved: u16`, `payload_length: u32`
- Read/write: `write_message()`, `read_message()`
- AVCC helpers: `strip_start_code()`, `serialize_avcc_payload()`, `build_codec_string()`, `build_avcc_descriptor()` — moved from `live-server/src/video/buffer.rs`

**Video payload layouts** (documented for TS server interop):
- `CodecParams (0x01)`: `[u16 LE: width][u16 LE: height][u16 LE: sps_len][sps][u16 LE: pps_len][pps]`
- `Frame (0x02)`: `[u64 LE: timestamp_us][avcc bytes]` (is_keyframe is in the header flags, not in the payload)

### live-capture (Rust, library + binary)

The foundational capture unit. Compiles to both a library and a binary. **Always outputs to stdout** via `live-protocol` framing — no networking code.

**Library** (`live-capture/src/lib.rs`): Reusable capture + encode pipeline.
- WinRT `CaptureSession` wrapper
- GPU resampler (fullscreen quad shader) and cropper (`CopySubresourceRegion`)
- BGRA→NV12 converter (`ID3D11VideoProcessor`)
- NVENC H.264 encoder (async MFT)
- Annex B → AVCC conversion (via `live-protocol` helpers)
- "Bakery model": capture thread writes to staging texture, encoding thread reads at fixed FPS
- D3D11 device and texture helpers

**Binary** (`live-capture/src/main.rs`): Three capture modes selected by `--mode`. All modes write `live-protocol` framed messages to stdout. Pipe through `live-ws` for network delivery.

```bash
# Base mode (default when --mode is omitted) — capture a specific window, resample to target resolution
live-capture.exe --hwnd 0x1A2B --width 1920 --height 1200 \
  | live-ws --mode video --server ws://machineA:3000/api/v1/ws/video/my-stream/input

# Auto mode — auto-selector polls foreground, hot-swaps capture, resampling implied
live-capture.exe --mode auto --width 1920 --height 1200 \
  --config-url http://machineA:3000/api/v1/streams/auto/config \
  --info-url http://machineA:3000/api/core/streamInfo/main \
  | live-ws --mode video --server ws://machineA:3000/api/v1/ws/video/main/input

# Crop mode — extract an absolute subrect at native resolution
live-capture.exe --mode crop --hwnd 0x1A2B \
  --crop-min-x 0 --crop-min-y 600 --crop-max-x 1920 --crop-max-y 700 \
  | live-ws --mode video --server ws://machineA:3000/api/v1/ws/video/youtube-music/input

# Dump to file for testing (production code path, no special test mode)
live-capture.exe --hwnd 0x1A2B --width 1920 --height 1200 > dump.bin
```

**Hot-swap support** (auto mode): The library exposes a method to replace the `CaptureSession` targeting a new HWND while keeping the encoder running. The staging texture dimensions don't change (fixed per stream).

**Auto mode responsibilities**:
- Polls foreground window every 2s (`enumerate_windows::get_foreground_window()`)
- Matches against selector config (polled from `--config-url`)
- Hot-swaps capture session on match (via library)
- POSTs stream info to server on each switch (`POST /api/core/streamInfo/:streamId`) — the only HTTP live-capture does
- Writes encoded frames to stdout (live-protocol framing)

**Thread model** (all modes):
- Encoding thread: reads staging texture, converts, encodes, writes to stdout
- Capture callback: WinRT thread pool, writes staging texture
- HTTP client thread (auto mode only): selector timer + config polling + event POSTs

### live-ws (Rust, binary)

Stdin-to-WebSocket relay. Reads `live-protocol` framed messages from stdin and forwards each as a WS binary message to the server.

```bash
# Default mode — dumb framed forwarding with auto-reconnect
live-kpm | live-ws --server ws://machineA:3000/api/v1/ws/kpm/input

# Video mode — additionally caches CodecParams + last keyframe for reconnect replay
live-capture ... | live-ws --mode video --server ws://machineA:3000/api/v1/ws/video/main/input
```

**Architecture**:
- Stdin reader: blocking thread reads `live_protocol::read_message()` in a loop, sends to a channel
- WS writer: tokio task consumes channel, sends to WS. On disconnect: drain/discard channel, reconnect with exponential backoff, replay cache if `--mode video`, resume.

**`--mode video`**: peeks at `message_type` and `flags.IS_KEYFRAME` in the header to cache:
- Last `CodecParams` message (for codec state on reconnect)
- Last keyframe (for a clean entry point on reconnect)

On reconnect, replays cached messages before resuming the live stream. This means the encoder doesn't need to know about WS state — it just writes to stdout continuously.

### live-kpm (Rust, binary)

Standalone keystroke counter. Same privacy-by-design: never reads key identity. **Outputs to stdout** via `live-protocol` framing. Pipe through `live-ws` for network delivery.

```bash
live-kpm | live-ws --server ws://machineA:3000/api/v1/ws/kpm/input
# Or dump to file for testing:
live-kpm > kpm.bin
```

**Architecture**:
- Message pump thread: `WH_KEYBOARD_LL` hook, atomic counter
- Timer thread: 50ms batch polling, sliding window calculator, writes `MessageType::KpmUpdate` messages to stdout on value change

### Server (TypeScript, Bun/Hono)

Thin HTTP/WS hub. No Win32, no GPU, no binary protocol parsing.

**Video relay**:
- Encoder-facing: `WS /api/v1/ws/video/:id/input` — receives binary frames from capture workers
- Frontend-facing: `WS /api/v1/ws/video/:id` — pushes frames to viewers
- Caches: codec params (for `/init`) + last keyframe (for late-joining clients)
- Fan-out: broadcast to all connected frontend clients
- Stream presence: a stream exists when an encoder WS is connected for that stream ID

**Other endpoints** (carried from M3):
- `GET/PUT/DELETE /api/v1/strings/:key` — string store
- `WS /api/v1/ws/strings` — string store push
- `WS /api/v1/ws/kpm` — KPM push (fed by `WS /api/v1/ws/kpm/input` from live-kpm via live-ws)
- `GET/PUT /api/v1/streams/auto/config` — selector config (auto mode polls this)
- `GET /api/v1/streams/:id/init` — codec string + avcC descriptor (built from cached params)
- `GET /api/v1/streams` — list active streams (derived from connected encoder WS sockets)

**Internal WS endpoints** (for live-ws relay):
- `WS /api/v1/ws/video/:id/input` — binary frames from live-capture via live-ws
- `WS /api/v1/ws/kpm/input` — KPM updates from live-kpm via live-ws

**Internal HTTP endpoints** (for live-capture auto mode):
- `POST /api/core/streamInfo/:streamId` — capture switch metadata

---

## IPC Protocols

### live-protocol Frame Header (shared by all components)

All binary IPC uses the `live-protocol` 8-byte aligned frame header:

```
Offset  Field            Size    Notes
0       message_type     u8      0x01=CodecParams, 0x02=Frame, 0x10=KpmUpdate, 0xFF=Error
1       flags            u8      bit 0: IS_KEYFRAME (video), bits 1-7: reserved
2       reserved         u16     zero for now
4       payload_length   u32 LE
[payload_length bytes follow]
```

This header is used on stdout (live-capture → live-ws), on WebSocket (live-ws → server → frontend), and in dump files. Every consumer reads the same format.

The server peeks at bytes 0-1 via `DataView` to identify message type and keyframe flag. No payload inspection needed for routing decisions.

### Producer → stdout (live-protocol framed binary)

**live-capture** writes `CodecParams` (0x01) and `Frame` (0x02) messages. Frame payloads contain AVCC data (Annex B → AVCC conversion happens in the encoding thread).

**live-kpm** writes `KpmUpdate` (0x10) messages. Payload is `[i64 LE: kpm_value]`.

### live-ws → Server (WebSocket, binary)

`live-ws` forwards each stdin message as one WS binary message (header included, no stripping). The server receives the same 8-byte header + payload.

### live-capture → Server (HTTP, JSON — auto mode only)

**`POST /api/core/streamInfo/:streamId`**
```json
{
  "hwnd": "0x1A2B",
  "title": "Visual Studio Code",
  "file_description": "Visual Studio Code",
  "mode": "code"
}
```

### Server → Frontend (WebSocket, binary)

The server relays `live-protocol` framed messages directly to frontend WS clients. The frontend reads the same 8-byte header format. This differs from M3 (which used a server-specific `[generation][num_frames][...]` envelope) — the M4 frontend parses `live-protocol` messages directly.

For late-joining clients, the server sends cached CodecParams + last keyframe immediately on WS connect.

---

## Server Design

### Why TypeScript Again

The M3 RIIR (Rewrite It In Rust) was justified by three things — all of which disappear in M4:

| RIIR Rationale | M3 Status | M4 Status |
|---|---|---|
| Eliminate protocol duplication (TS had hand-written binary parsers) | Solved — server calls `live_video::read_message()` directly | **Gone** — server relays opaque bytes, never parses them |
| Direct library calls (no child process for window enum) | Solved — `enumerate_windows()` called as Rust library | **Gone** — window enum is in the capture worker |
| Single process (server + children share types) | Solved — one Rust workspace | **Gone** — microservice architecture, each process independent |

A TypeScript relay server (Bun/Hono) offers:
- Faster iteration on HTTP/WS logic
- Native integration with Vite (no reverse proxy)
- Portfolio demonstration of full-stack capability
- The core relay logic is trivially simple

### Relay Architecture

The server does NOT buffer frames. It relays them.

```
live-ws ──WS──→ [DataView: read header bytes 0-1] ──→ broadcast to frontend WS clients
                    │
                    ├─ CodecParams (0x01)? → cache for /init endpoint
                    └─ Frame (0x02) + IS_KEYFRAME? → cache for late-joining clients
```

No circular buffer. No sequence numbers. No `after=N` cursor. No generation tracking. The complexity that existed in M3's `StreamBuffer` and `StreamRegistry` is eliminated because the encoder is persistent and frames flow through immediately.

---

## Distributed Deployment

### Current M3 Deployment

```
Machine A (streaming):  live-app (wry webview) ← connects to Machine B
Machine B (working):    live-server + live-video + live-audio + live-kpm + YouTube Music
```

- YouTube Music audio must traverse the network → live-audio → server → WS → frontend → AudioWorklet
- Choppy audio is a recurring problem
- Face capture competes with rustc for CPU on Machine B

### M4 Deployment

```
Machine A (streaming):  server + live-capture --mode crop (youtube-music) + live-kpm (optional) + YouTube Music + OBS + live-app
Machine B (working):    live-capture --mode auto (main)
```

- YouTube Music audio: OBS captures system audio directly on Machine A. Zero network. Zero latency.
- Only the main video stream crosses the LAN (~1.8 MB/s at 60fps, trivial on gigabit)
- Machine B runs only what needs direct window/GPU access
- Face capture (OBS camera) stays on Machine A — no CPU competition with rustc

### Why This Works

Each producer is a stdout-first executable piped through `live-ws` for network delivery:
- `live-capture --mode auto ... | live-ws --mode video --server ws://machineA:3000/.../main/input` (Machine B)
- `live-capture --mode crop ... | live-ws --mode video --server ws://machineA:3000/.../youtube-music/input` (Machine A)
- `live-kpm | live-ws --server ws://machineA:3000/.../kpm/input` (either machine)

No process assumes it was spawned by another. Producers write to stdout. `live-ws` handles all networking. Just pipes and WebSocket.

---

## Building Blocks from M3

These M3 components are battle-tested and will be refactored in-place for M4:

### Carry Over (Rust, into live-capture library)

| M3 File | Purpose | Changes for M4 |
|---|---|---|
| `live-video/src/encoder.rs` + `encoder/` | NVENC H.264 async MFT | None — unchanged |
| `live-video/src/converter.rs` | BGRA→NV12 via `ID3D11VideoProcessor` | None — unchanged |
| `live-video/src/capture.rs` | WinRT `CaptureSession` + `CropBox` | Add hot-swap method for session replacement |
| `live-video/src/resample.rs` + `.hlsl` | GPU fullscreen quad resampler | None — unchanged |
| `live-video/src/d3d11.rs` | D3D11 device + texture helpers | Extract to shared helper crate |
| `live-video/src/lib.rs` | Binary frame protocol types | Move payload types to live-protocol; frame header replaced by new 8-byte aligned header |
| `live-server/src/video/buffer.rs` | Annex B → AVCC conversion logic | Move AVCC helpers to live-protocol (shared by capture worker and TS server) |
| `live-server/src/selector/config.rs` | Pattern parsing, `PresetConfig`, `should_capture()` | Move to live-capture (used by `--mode auto`) |
| `live-server/src/kpm/calculator.rs` | Sliding window KPM calculator | Move to live-kpm |
| `live-server/src/kpm/hook.rs` | `WH_KEYBOARD_LL` hook + message pump | Move to live-kpm |
| `live-server/src/message_pump.rs` | Reusable Win32 message pump | Move to live-kpm |
| `crates/enumerate-windows/` | Window enumeration library | Used by live-capture `--mode auto` |
| `crates/set-dpi-awareness/` | Per-monitor DPI v2 | Used by all Rust binaries |

### Carry Over (Frontend, largely unchanged)

| M3 File | Purpose | Changes for M4 |
|---|---|---|
| `frontend/src/video/` | `StreamRenderer`, H264Decoder, WebGL color-key | Minimal — generation logic may simplify |
| `frontend/src/kpm.tsx` | KPM meter + VU bar | None — unchanged |
| `frontend/src/strings.ts` | `useStrings()` hook (WS push) | None — unchanged |
| `frontend/src/ws.ts` | WS helpers (connectWs, auto-reconnect) | None — unchanged |
| `frontend/src/app.tsx` | Main viewer shell | Minimal layout changes |
| `frontend/src/widgets/` | Clock, Mode, Capture, About widgets | None — unchanged |

### Discard (replaced by M4 architecture)

| M3 Component | Why Discarded |
|---|---|
| `live-server/src/video/process.rs` (StreamRegistry, spawn_and_wire) | No child process management — workers connect via WS |
| `live-server/src/video/buffer.rs` (circular buffer) | Server is a relay, not a buffer. AVCC conversion moves to capture worker |
| `live-server/src/selector/manager.rs` (foreground polling) | Polling moves to `live-capture --mode auto` |
| `live-server/src/youtube_music/manager.rs` (YTM window detection) | YouTube Music capture is a separate `live-capture --mode crop` instance |
| `live-server/src/kpm/hook.rs` (in-process keyboard hook) | KPM moves to standalone live-kpm |
| `live-server/src/audio/` (audio buffer, process, routes, WS) | live-audio eliminated entirely |
| `live-audio/` (WASAPI capture) | OBS captures audio directly |
| `live-server/src/vite_proxy.rs` (reverse proxy to Vite) | TS server with native Vite integration |
| `live-server/src/state.rs` (AppState with 6 RwLock subsystems) | Server state is much simpler (relay caches + string store) |

---

## Design Principles

Principles that emerged from the design discussion and should guide M4 implementation.

### 1. No Internal Start/Stop State

If a process is running, it's active. Kill it to stop it. No state machines, no `Starting` → `Running` → `Stopped` transitions. This eliminates an entire class of bugs around state synchronization.

### 2. Stateless Executables

Each component gets all its configuration from CLI args and HTTP. No stdin commands, no dynamic reconfiguration messages. If the configuration needs to change, kill and restart with new args.

Exception: `live-capture --mode auto` polls config from the server, but this is read-only polling of an HTTP endpoint, not a command channel.

### 3. Stdout-First Producers

Producers (`live-capture`, `live-kpm`) always write to stdout via `live-protocol` framing. They have zero networking dependencies. This means:
- `live-capture --hwnd 0x... --width 1920 --height 1200 > dump.bin` — pipe to file for testing
- `live-capture ... | live-ws --mode video --server ws://...` — network delivery via relay
- `live-kpm > kpm.bin` — dump for testing
- `live-kpm | live-ws --server ws://...` — network delivery via relay

The production code path and the test code path are the same — only the downstream consumer differs.

### 4. Independently Runnable

Every component can run standalone for testing and debugging. No component assumes it was spawned by another. Server runs with or without any workers connected.

### 5. Pipes + WS Everywhere

Producers communicate via stdout pipes to `live-ws`, which relays to the server via WebSocket. `live-capture --mode auto` additionally uses HTTP for low-frequency metadata (config polling, capture switch events). This enables distributed deployment naturally — just point `live-ws` at a remote server URL.

### 6. Server is a Relay, Not a Manager

The server doesn't spawn processes, doesn't manage lifecycles, doesn't know which machines workers run on. It receives connections and relays data. Workers are responsible for their own lifecycle and reconnection.

In production, the server MAY spawn local workers for convenience — but this is an operational choice, not an architectural requirement.

### 7. Errors Go to stderr

No error protocol between components. Each process logs to stderr via the standard logging crate. When components are co-located, stderr is inherited and logs interleave naturally. When distributed, each process logs locally.

### 8. Fixed Resolutions

Each stream has a fixed output resolution determined at capture time. The resampler always outputs to the same dimensions regardless of source window. This means the encoder never needs reconfiguration on window switch — the staging texture is the same size, the NV12 converter is the same size, the MFT media types don't change.

---

## Resolved Design Decisions

These questions were identified during the initial design session (2026-03-18) and resolved on 2026-03-23.

### 1. Server Framework → Hono

**Decision**: Bun + Hono. Familiar from the M2 TS server, lightweight, well-suited for the relay pattern.

### 2. Binary Frame Relay → DataView on live-protocol Header

**Decision**: `DataView` on `ArrayBuffer` is sufficient. The 8-byte `live-protocol` header puts `message_type` at byte 0 and `flags` (including `IS_KEYFRAME`) at byte 1 — two byte reads per message. No payload inspection, no native addon needed.

### 3. AVCC Conversion → Capture Worker

**Decision**: Annex B → AVCC conversion moves to the capture worker (Rust). The server relays truly opaque bytes and never needs to understand H.264 framing. The `/init` endpoint serves cached CodecParams that the capture worker already sends in AVCC-ready format.

### 4. YTM Management → Separate Crop Instance

**Decision**: YouTube Music is a separate `live-capture --mode crop` instance, not part of auto mode. Single binary, different `--mode` flag. This keeps auto mode focused on foreground polling and crop mode focused on fixed-region extraction. YTM instance is managed externally (launched manually or by a process manager).

### 5. Frontend Stream Status → WS Connection Presence

**Decision**: The server tracks active streams by connected encoder WS connections. A stream exists when an encoder WS is connected for that stream ID. `GET /api/v1/streams` derives its response from this — same endpoint, different source of truth compared to M3's process-based tracking.

### 6. Reconnection → live-ws Handles It, Encoder Doesn't Know

**Decision**: `live-ws` handles auto-reconnect with exponential backoff. The encoder writes to stdout continuously — it doesn't know about WS state. During disconnect, `live-ws` discards incoming messages. In `--mode video`, `live-ws` caches the last CodecParams and last keyframe; on reconnect, it replays cached messages before resuming the live stream. This gives the server a clean codec state and a valid keyframe without any back-channel to the encoder.

### 7. Shared Protocol → live-protocol Crate

**Decision**: A shared `live-protocol` lib crate defines the 8-byte aligned frame header used by all Rust components. The TS server hand-writes matching constants (a few byte offsets and enum values — simple enough that code generation is unnecessary).

### 8. Stdout-First Producers → live-ws Relay

**Decision**: Producers (`live-capture`, `live-kpm`) always write `live-protocol` framed messages to stdout. A separate `live-ws` binary reads stdin and relays to the server via WebSocket. This gives producers one code path (stdout), makes testing trivial (`> dump.bin`), and centralizes all networking/reconnection logic in one reusable binary.

---

*This document captures the design discussion of 2026-03-18, with open questions resolved 2026-03-23 and the live-protocol/live-ws additions from 2026-03-23. It should be treated as a foundation — not a specification. Implementation decisions will evolve as M4 development progresses.*
