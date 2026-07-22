# Audio Streaming Subsystem

**Low-latency raw PCM audio capture from WASAPI → HTTP polling → browser AudioWorklet playback, with delta + gzip compression on the wire.**

**Last Updated**: 2026-03-16

---

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Data Flow](#data-flow)
- [IPC Wire Protocol](#ipc-wire-protocol)
- [HTTP API](#http-api)
- [Frontend Audio Pipeline](#frontend-audio-pipeline)
- [Configuration](#configuration)
- [File Map](#file-map)
- [Bandwidth & Latency Budget](#bandwidth--latency-budget)
- [Testing](#testing)
- [Design Decisions](#design-decisions)

---

## Overview

The audio subsystem captures audio from a named WASAPI input device (e.g. a
virtual loopback microphone) and streams raw PCM s16le over HTTP to the browser.
It runs in parallel with the video pipeline but is architecturally independent —
audio is global (one device, not per-window) while video is per-stream.

The full path from microphone to speaker:

```
WASAPI device → live-audio.exe → stdout IPC → LiveServer → HTTP API → browser AudioWorklet
```

## Architecture

Three layers, mirroring the video pipeline:

```
┌──────────────────────────────────────────────────────────────────────┐
│  Rust: live-audio.exe                                                │
│  WASAPI shared-mode capture → s16le re-chunking → binary IPC stdout  │
└──────────────────────┬───────────────────────────────────────────────┘
                       │ stdout (binary IPC: AudioParams + AudioFrame)
                       │ stderr (env_logger text → server log)
┌──────────────────────▼───────────────────────────────────────────────┐
│  Server: AudioManager (Bun/TypeScript)                               │
│  AudioProtocolParser → AudioBuffer (circular, 100 chunks)            │
│  HTTP API: GET /init, GET /chunks?after=N (delta+gzip compressed)    │
└──────────────────────┬───────────────────────────────────────────────┘
                       │ HTTP polling (~16ms interval)
┌──────────────────────▼───────────────────────────────────────────────┐
│  Frontend: <AudioStream /> (React)                                   │
│  Fetch chunks → delta-decode → AudioWorklet (ring buffer → output)   │
└──────────────────────────────────────────────────────────────────────┘
```

### Key differences from video

| Aspect | Video | Audio |
|--------|-------|-------|
| Scope | Per-window (stream ID) | Global (one device) |
| Encoding | H.264 via NVENC | None (raw PCM s16le, delta+gzip on HTTP) |
| Keyframe gating | Yes (IDR required to start) | No (all chunks independent) |
| Buffer capacity | 60 frames (~1s at 60fps) | 100 chunks (~1s at 10ms/chunk) |
| Generation counter | Yes (stream replacement) | No (single process) |

## Data Flow

### 1. Rust capture (`live-audio.exe`)

1. Opens the named WASAPI capture device in shared mode
2. Queries the device's native mix format via `GetMixFormat()`
3. If the device provides f32le (common for WASAPI shared mode), converts to s16le in-process
4. Writes one `AudioParams` message to stdout (sample rate, channels, bit depth)
5. Registers with MMCSS (`AvSetMmThreadCharacteristicsW("Pro Audio")`) for guaranteed scheduling under heavy CPU load
6. Enters the capture loop:
   - Polls `IAudioCaptureClient::GetBuffer()` every ~5ms
   - Accumulates samples into fixed-size 10ms chunks (480 samples at 48kHz)
   - Writes each chunk as an `AudioFrame` message with a wall-clock timestamp
7. Exits cleanly on stdout broken pipe (server killed the process; reverts MMCSS)

### 2. Server (`AudioManager`)

1. `audioManager.start()` spawns `live-audio.exe --device <name>`
2. stdout is piped through `AudioProtocolParser` (push-based incremental binary parser)
3. Parsed messages are dispatched:
   - `AudioParams` → cached in `AudioBuffer` for the `/init` endpoint
   - `AudioFrame` → pre-serialized and pushed into the circular buffer
4. stderr is piped through the same grouped log renderer as video capture

### 3. Frontend (`<AudioStream />`)

1. Fetches `GET /api/v1/audio/init` (retries on 503 until params arrive)
2. Creates an `AudioContext` at the device's native sample rate
3. Loads the `PcmWorkletProcessor` module via `audioWorklet.addModule()`
4. Polls `GET /api/v1/audio/chunks?after=N` with adaptive timing (16ms normal, 4ms fast retry when empty)
5. Browser auto-decompresses gzip (`Content-Encoding`); parser delta-decodes each chunk's PCM via prefix sum
6. Posts all received chunks to the worklet immediately (no A/V sync gating)
7. The worklet converts s16le → f32, writes to a ring buffer, and outputs via `process()`

## IPC Wire Protocol

Same envelope as the video protocol (`live-encoder`):

```
[u8:  message_type]
[u32 LE: payload_length]
[payload_length bytes: payload]
```

Audio message types use the `0x1x` range to avoid collision with video (`0x01`–`0x02`):

### AudioParams (`0x10`)

Sent once after device initialization.

```
[u32 LE: sample_rate]    e.g. 48000
[u8:     channels]       e.g. 2
[u8:     bits_per_sample] e.g. 16
```

Total payload: 6 bytes.

### AudioFrame (`0x11`)

One chunk of raw PCM audio.

```
[u64 LE: timestamp_us]   wall-clock microseconds since Unix epoch
[N bytes: pcm_data]       interleaved s16le samples
```

At 48kHz stereo with 10ms chunks: N = 480 samples × 2 channels × 2 bytes = 1920 bytes.
Total payload: 8 + 1920 = 1928 bytes per chunk.

### Error (`0xFF`)

Raw UTF-8 error description string. Shared discriminant with the video protocol.

## HTTP API

Mounted at `/api/v1/audio` on the Hono app.

### `GET /init`

Returns audio format parameters as JSON.

**Success (200)**:
```json
{
  "sampleRate": 48000,
  "channels": 2,
  "bitsPerSample": 16
}
```

**Not ready (503)**: The capture process hasn't sent `AudioParams` yet. Frontend retries.

### `GET /chunks?after=N`

Returns audio chunks with sequence number > N as a gzip-compressed binary blob
(`Content-Encoding: gzip` — the browser decompresses transparently).

**Binary layout** (all little-endian, after gzip decompression):
```
[u32: num_chunks]
per chunk:
  [u32: sequence]
  [u32: payload_length]
  [payload_length bytes: payload]
```

Each chunk's payload is `[u64 LE: timestamp_us][delta-encoded s16le PCM bytes]`.

**Delta encoding**: the first sample in each chunk is stored as-is; every
subsequent sample is the difference from the previous sample (`current - prev`).
Resets per chunk — no cross-chunk state.  The frontend reconstructs original
samples via prefix sum (`samples[i] += samples[i-1]`).

Delta encoding clusters sample values near zero, which makes the data highly
compressible with gzip.  Together they reduce audio HTTP bandwidth by ~60–80%
compared to raw PCM.

## Frontend Audio Pipeline

### AudioWorklet (`frontend/src/audio/worklet.ts`)

Runs on the dedicated audio rendering thread (not the main thread).

- **Input**: `MessagePort` receives `{ type: "pcm", samples: Int16Array, channels }` from the main thread
- **Ring buffer**: ~200ms capacity (9600 frames at 48kHz). Stores f32 samples (converted from s16le on write).
- **Pre-buffering**: One-shot gate — outputs silence until the ring accumulates 100ms (4800 frames), then starts playback permanently. Prevents underruns during the startup transient when the ring is near-empty and HTTP polling jitter could drain it to zero.
- **Overflow**: If an incoming chunk won't fit in the ring, the entire chunk is dropped (not truncated mid-sample) to preserve PCM continuity.
- **Output**: `process()` pulls 128 frames per call from the ring buffer into the output channels
- **Underrun**: Outputs silence (zeros) — no glitch artifacts

Why 200ms ring buffer? Large enough to absorb HTTP polling jitter (~25–35ms
effective interval), occasional GC pauses, and bursty chunk delivery — while
keeping latency well under the 250ms streaming target.

### Browser autoplay policy

`AudioContext` starts in `"suspended"` state due to browser autoplay policy.
The component registers `click` and `keydown` listeners that call
`audioContext.resume()` on first user interaction.

## Configuration

All audio configuration lives in `server/common.ts`:

| Constant | Value | Purpose |
|----------|-------|---------|
| `audioExePath` | `../target/debug/live-audio.app.exe` | Path to the Rust binary |
| `audioDeviceName` | `"Loopback L + R (Focusrite USB Audio)"` | WASAPI device friendly name |

The device name is passed to `live-audio.exe` via `--device`. The Rust binary
requires this argument (hard error if missing). Use `--list-devices` to enumerate
available WASAPI capture devices:

```bash
live-audio.exe --list-devices
```

### Tuning constants

| Location | Constant | Default | Purpose |
|----------|----------|---------|---------|
| `main.rs` | `CHUNK_DURATION_MS` | 10 | PCM chunk size (ms) |
| `main.rs` | `POLL_SLEEP_MS` | 5 | WASAPI buffer poll interval (ms) |
| `audio.ts` | `AUDIO_BUFFER_CAPACITY` | 100 | Server circular buffer size (chunks) |
| `audio-api.ts` | — | — | No tuning constants (stateless) |
| `index.tsx` | `POLL_INTERVAL_MS` | 16 | Frontend HTTP poll interval (ms) |
| `index.tsx` | `POLL_FAST_MS` | 4 | Fast retry interval when server had no new chunks (ms) |
| `index.tsx` | `INIT_RETRY_MS` | 500 | Retry delay for `/init` 503s (ms) |
| `worklet.ts` | `RING_CAPACITY_FRAMES` | 9600 | AudioWorklet ring buffer (frames, ~200ms) |
| `worklet.ts` | `PRE_BUFFER_FRAMES` | 4800 | One-shot pre-buffer threshold (frames, ~100ms) |

## File Map

### Rust (`core/live-audio/`)

| File | Purpose |
|------|---------|
| `Cargo.toml` | Crate manifest (deps: windows, clap, env_logger, log, widestring) |
| `src/lib.rs` | IPC wire protocol: message types, serialization, deserialization, tests |
| `src/main.rs` | WASAPI capture loop: device enumeration, format detection, f32→s16 conversion, chunking, MMCSS thread priority |

### Server (`server/`)

| File | Purpose |
|------|---------|
| `audio-protocol.ts` | Push-based incremental binary parser for `0x10`/`0x11` audio IPC messages |
| `audio-buffer.ts` | Circular buffer (100 chunks), pre-serializes on push, no keyframe gating |
| `audio.ts` | Singleton `AudioManager`: spawns process, wires stdout/stderr, lifecycle |
| `audio-api.ts` | Hono sub-router: `GET /init` (JSON) + `GET /chunks?after=N` (delta-encoded, gzip-compressed binary) |
| `common.ts` | `audioExePath` and `audioDeviceName` constants (edited, not new) |
| `index.ts` | Mounts audio API, starts/stops manager in lifecycle hooks (edited, not new) |

### Frontend (`frontend/src/audio/`)

| File | Purpose |
|------|---------|
| `worklet.ts` | AudioWorklet processor: 200ms ring buffer, s16le→f32, silence on underrun |
| `index.tsx` | `<AudioStream />` component: init fetch, adaptive chunk polling, delta decoding, worklet wiring |

### Frontend (edited)

| File | Change |
|------|--------|
| `app.tsx` | Mounts `<AudioStream />` at app root |

## Bandwidth & Latency Budget

### Bandwidth

Raw PCM (before compression):
```
48000 Hz × 2 channels × 16 bits × 10ms chunks
= 48000 × 2 × 2 bytes = 192,000 bytes/sec
= 1.536 Mbit/s
```

With delta encoding + gzip (~60–80% reduction):
```
~0.3–0.6 Mbit/s (content-dependent)
```

For comparison, video is ~8 Mbit/s (CBR H.264). Compressed audio adds ~4–8%
to the total bandwidth.

### Latency breakdown (approximate)

| Stage | Latency |
|-------|---------|
| WASAPI buffer fill | 10–40ms (buffer sized for scheduling headroom) |
| WASAPI poll interval | 0–5ms |
| IPC (stdout pipe) | <1ms |
| Server buffer | 0ms (immediate push) |
| HTTP poll interval | 0–16ms |
| WLAN round trip | ~5ms |
| AudioWorklet ring buffer | 0–200ms (jitter absorption) |
| **Total** | **~30–240ms** |

The bottleneck is the frontend polling interval + worklet ring buffer. The
200ms ring buffer is sized for worst-case jitter absorption; in steady state
the buffer stays near-empty and perceived latency is closer to 30–60ms.

## Testing

### 1. Rust binary standalone

```bash
# List available capture devices
live-audio.exe --list-devices

# Capture to stdout (produces binary output — will look like gibberish)
live-audio.exe --device "Loopback L + R (Focusrite USB Audio)"
# Kill with Ctrl+C after a few seconds
```

### 2. Server API

```bash
# Start the server
cd server && bun run index.ts

# Check audio params (should return JSON after ~1 second)
curl http://localhost:3000/api/v1/audio/init

# Check chunk flow (--compressed to auto-decompress gzip; should return >4 bytes)
curl -s --compressed http://localhost:3000/api/v1/audio/chunks?after=0 | wc -c
```

### 3. Frontend playback (on remote PC)

Open the browser on the remote machine (not localhost — avoids audio feedback loop).
Check browser console for:
- `AudioStream: params 48000Hz 2ch 16-bit`
- Click the page once to satisfy browser autoplay policy
- Audio should play from the captured device

### 4. Resilience

- **Tab close/reopen**: Audio should restart cleanly (new `AudioContext`, new poll loop)
- **Process crash**: Server logs the exit, frontend gets empty chunk responses (no crash)
- **Device unavailable**: `live-audio.exe` exits with error, server logs `process exited with code 1`

## Design Decisions

### Why raw PCM instead of a compressed codec?

At 1.536 Mbit/s, raw PCM is only ~19% of the video bandwidth (8 Mbit/s). On a
local WLAN link this is negligible. Compression (Opus, AAC) would add:
- Encoding latency (~5–20ms depending on frame size)
- Decode complexity in the browser (MediaSource Extensions or WebCodecs)
- Additional failure modes (codec initialization, bitrate negotiation)

Raw PCM keeps the pipeline simple and latency minimal.

### Why a separate binary instead of integrating into `live-encoder.exe`?

Audio capture is global (one device) while video capture is per-window. The
audio device doesn't change when the window selector switches targets. A separate
process keeps the concerns cleanly separated and avoids complicating the
per-window lifecycle in `process.ts`.

### Why HTTP polling instead of WebSocket?

Matches the existing video transport pattern. The video pipeline already achieves
<100ms latency with HTTP polling at ~60fps. Audio polling at ~16ms intervals
(~62 polls/sec) provides comparable responsiveness. WebSocket would save HTTP
overhead per request but adds connection management complexity for minimal gain.

### Why AudioWorklet instead of ScriptProcessorNode?

`ScriptProcessorNode` is deprecated and runs on the main thread — it would
compete with React rendering and HTTP polling for CPU time.
`AudioWorkletProcessor.process()` runs on a dedicated audio rendering thread,
ensuring glitch-free playback independent of main thread load.

### Why 10ms chunks?

- **Smaller (1ms)**: Too many IPC messages, HTTP requests, and buffer management
  overhead for negligible latency improvement.
- **Larger (50–100ms)**: Adds perceptible latency to the audio path, making A/V
  sync harder and the audio feel sluggish.
- **10ms**: Standard for VoIP and real-time audio. At 48kHz stereo s16le, each
  chunk is 1920 bytes — small enough for low overhead, large enough to amortize
  IPC and HTTP costs.

### Why no A/V sync?

Both `live-encoder.exe` and `live-audio.exe` use the same wall clock
(`SystemTime::now()`) on the same machine, so their timestamps are directly
comparable. Audio has no encoding step and arrives ~20ms ahead of video — but
this difference is imperceptible. Explicit sync (holding audio until video
catches up) would add complexity with no audible benefit, so chunks are posted
to the worklet immediately.
