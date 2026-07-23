# [ARCHIVED] Original Proposal: Windows Media Foundation H.264 Video Streaming

> **📦 ARCHIVED DOCUMENT**
>
> This document contains the **original design proposal** from before implementation began.
> The **actual implementation** differs significantly. For current status, see [`README.md`](./README.md).
>
> **Historical Value**: Useful for understanding alternative architectures we considered (event-driven vs bakery model).

---

## Executive Summary

Implement low-latency (<100ms) video streaming from DirectX11 captured frames to the webview using:
- **Backend**: Windows Media Foundation (WMF) H.264 hardware encoder
- **Transport**: Wry custom protocol handler (`stream://` protocol)
- **Frontend**: WebCodecs API for hardware-accelerated decoding
- **Format**: Raw H.264 NAL units (no container overhead)

## Architecture Overview

```
Capture (ID3D11Texture2D BGRA)
    ↓
Format Converter (GPU: BGRA → NV12)
    ↓
WMF H.264 Encoder (Hardware-accelerated)
    ↓
Stream Manager (Circular buffer, 60 frames)
    ↓
Wry Custom Protocol Handler (stream://)
    ↓
Frontend Decoder (WebCodecs H.264)
    ↓
Canvas Renderer
```

## Key Design Decisions

### 1. Format Conversion: BGRA → NV12
**Why**: Hardware H.264 encoders require NV12 input. Capture produces BGRA.

**Solution**: Use `ID3D11VideoProcessor` for GPU-accelerated conversion (~0.5ms for 1080p).

**Alternative rejected**: CPU conversion (8-15ms) - destroys latency budget.

### 2. Encoding Thread (Separate from UI Thread)
**Why**: Avoid blocking the UI thread during encoding (~5-15ms per frame).

**Architecture**: Use `std::sync::mpsc` channel to send captured textures to encoding thread. Encoding thread processes frames and pushes to stream manager.

**Trade-off**: Adds 1 frame of latency (~16ms at 60fps) but keeps UI responsive. Total latency still well under 100ms target.

**Synchronization**: Copy texture to staging texture on UI thread, send to encoding thread. Encoding thread converts and encodes.

### 3. Low-Latency H.264 Configuration
**Critical settings**:
- **No B-frames**: `CODECAPI_AVEncMPVDefaultBPictureCount = 0` (B-frames add 2+ frame latency)
- **Small GOP**: 2 seconds for fast recovery
- **CBR mode**: Constant bitrate for predictable latency
- **Low latency mode**: `CODECAPI_AVLowLatencyMode = true`
- **Baseline profile**: Best compatibility with WebCodecs

**Error handling**: If encoder configuration fails, log error and continue with default settings. If encoder creation fails entirely, log error and disable streaming (set encoder to None).

### 4. Raw NAL Units (No Container)
**Why**: WebCodecs can decode raw H.264 NAL units directly. MP4/WebM containers add complexity and latency.

**Format**: Annex B format (start codes `00 00 00 01`) from WMF encoder.

### 5. Wry Custom Protocol Instead of WebSocket
**Why**:
- Simpler implementation
- Lower overhead for localhost
- Direct integration with Wry
- No external server needed

**Protocol**: `stream://init` for SPS/PPS, `stream://stream?after=N` for frames (long-polling).

## Implementation Steps

### Step 1: Add Dependencies

**File**: `Cargo.toml`

```toml
[dependencies]
crossbeam = "0.8"  # Lock-free queue for stream manager
```

**Justification**: `crossbeam::queue::ArrayQueue` provides lock-free SPSC queue for the hot path (encoder → protocol handler).

---

### Step 2: Create Format Converter Module

**File**: `src/converter.rs` (NEW)

**Purpose**: Stateless helper for converting BGRA textures to NV12 using GPU Video Processor.

**Key components**:
```rust
/// Stateless format converter helper
pub struct FormatConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    video_processor: ID3D11VideoProcessor,
    video_processor_enumerator: ID3D11VideoProcessorEnumerator,
}

impl FormatConverter {
    pub fn new(device: &ID3D11Device) -> anyhow::Result<Self>

    /// Convert BGRA texture to NV12. Output texture must be pre-allocated by caller.
    pub fn convert(
        &mut self,
        bgra_texture: &ID3D11Texture2D,
        nv12_texture: &ID3D11Texture2D) -> anyhow::Result<()>
}
```

**Implementation details**:
1. Query `ID3D11VideoDevice` from `ID3D11Device` using `api_call!`
2. Create `ID3D11VideoProcessorEnumerator` with input=BGRA, output=NV12
3. Create `ID3D11VideoProcessor` from enumerator
4. In `convert()`: Use `ID3D11VideoContext::VideoProcessorBlt()` to perform GPU conversion into caller-provided NV12 texture

**Error handling**: Use `api_call!` for all Windows API calls. On error, return early with context chain.

**Performance**: ~0.5-1ms for 1080p.

---

### Step 3: Create WMF Encoder Module

**File**: `src/encoder.rs` (NEW)

**Purpose**: Encode NV12 textures to H.264 NAL units using Windows Media Foundation.

**Key structures**:
```rust
pub struct H264Encoder {
    mf_dxgi_manager: IMFDXGIDeviceManager,
    mf_transform: IMFTransform,
    reset_token: u32,
    input_sample: IMFSample,
    width: u32,
    height: u32,
    frame_count: u64,
}

pub struct EncodedNALUnit {
    pub unit_type: NALUnitType,  // SPS=7, PPS=8, IDR=5, NonIDR=1
    pub data: Vec<u8>,
    pub timestamp_us: u64,
}

pub enum NALUnitType {
    SPS = 7,
    PPS = 8,
    IDR = 5,
    NonIDR = 1,
}
```

**Implementation sequence**:

#### 3.1 Initialization (`H264Encoder::new()`)
1. **Initialize MF**: `MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)`
2. **Create DXGI Device Manager**: `MFCreateDXGIDeviceManager(&mut reset_token)`
3. **Register D3D11 Device**: `mf_dxgi_manager.ResetDevice(device, reset_token)`
4. **Find H.264 Encoder**:
   ```rust
   MFTEnumEx(
       MFT_CATEGORY_VIDEO_ENCODER,
       MFT_ENUM_FLAG_HARDWARE,
       &input_type,  // NV12
       &output_type, // H264
       &mut mft_activate)
   ```
   - Prefer hardware encoder (Intel Quick Sync, NVIDIA NVENC, AMD VCE)
   - Fallback to software encoder if hardware unavailable
5. **Configure Input Type**:
   ```rust
   MFCreateMediaType()
   type.SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video)
   type.SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12)
   type.SetUINT64(MF_MT_FRAME_SIZE, pack_u32s(width, height))
   type.SetUINT64(MF_MT_FRAME_RATE, pack_u32s(60, 1))
   mf_transform.SetInputType(0, type, 0)
   ```
6. **Configure Output Type**:
   ```rust
   type.SetGUID(MF_MT_SUBTYPE, MFVideoFormat_H264)
   type.SetUINT32(MF_MT_AVG_BITRATE, 8_000_000)  // 8 Mbps for 1080p60
   type.SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base)
   mf_transform.SetOutputType(0, type, 0)
   ```
7. **Configure Low-Latency Settings** (via `ICodecAPI`):
   ```rust
   let codec_api = mf_transform.cast::<ICodecAPI>()?;
   codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT(0))?;
   codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT(120))?;  // 2 sec at 60fps
   codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT(true))?;
   codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode,
                      &VARIANT(eAVEncCommonRateControlMode_CBR))?;
   ```
8. **Attach DXGI Manager**: `mf_transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mf_dxgi_manager)`
9. **Start Streaming**: `mf_transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)`

#### 3.2 Encoding (`H264Encoder::encode_frame()`)
```rust
pub fn encode_frame(
    &mut self,
    nv12_texture: &ID3D11Texture2D,
    timestamp_us: u64) -> anyhow::Result<Vec<EncodedNALUnit>>
```

**Process**:
1. **Create MF Sample from texture**:
   ```rust
   let mut buffer = None;
   api_call!(unsafe {
       MFCreateDXGISurfaceBuffer(
           &IID_ID3D11Texture2D,
           nv12_texture,
           0,
           false,
           Some(&raw mut buffer))
   }).with_context(|| context!("creating DXGI surface buffer"))?;
   let buffer = buffer.ok_or_else(|| anyhow::anyhow!("buffer is null"))?;

   let mut sample = None;
   api_call!(unsafe { MFCreateSample(Some(&raw mut sample)) })
       .with_context(|| context!("creating MF sample"))?;
   let sample = sample.ok_or_else(|| anyhow::anyhow!("sample is null"))?;

   api_call!(sample.AddBuffer(&buffer))
       .with_context(|| context!("adding buffer to sample"))?;
   api_call!(sample.SetSampleTime((timestamp_us * 10) as i64))
       .with_context(|| context!("setting sample time"))?;
   api_call!(sample.SetSampleDuration(16666 * 10))
       .with_context(|| context!("setting sample duration"))?;
   ```

2. **Feed to encoder**:
   ```rust
   api_call!(self.mf_transform.ProcessInput(0, &sample, 0))
       .with_context(|| context!("feeding frame to encoder"))?;
   ```

3. **Drain output**:
   ```rust
   let mut nal_units = Vec::new();
   loop {
       let mut output_buffer = MFT_OUTPUT_DATA_BUFFER {
           dwStreamID: 0,
           pSample: None,
           dwStatus: 0,
           pEvents: None,
       };
       let mut status = 0;

       let result = api_call!(unsafe {
           self.mf_transform.ProcessOutput(
               0,
               1,
               &raw mut output_buffer,
               &raw mut status)
       });

       match result {
           Ok(_) => {
               if let Some(sample) = output_buffer.pSample {
                   // Convert to contiguous buffer and parse NAL units
                   match api_call!(sample.ConvertToContiguousBuffer())
                       .with_context(|| context!("converting to contiguous buffer"))
                   {
                       Ok(buffer) => {
                           match parse_nal_units_from_buffer(&buffer, timestamp_us) {
                               Ok(units) => nal_units.extend(units),
                               Err(e) => {
                                   log::warn!("Failed to parse NAL units: {:?}", e);
                                   // Continue - skip this frame's NAL units
                               }
                           }
                       }
                       Err(e) => {
                           log::warn!("Failed to convert buffer: {:?}", e);
                           // Continue - skip this frame
                       }
                   }
               }
           }
           Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
               // Normal condition - encoder needs more input
               break;
           }
           Err(e) => {
               log::error!("ProcessOutput failed: {:?}", e);
               break;
           }
       }
   }
   Ok(nal_units)
   ```

**Error handling strategy**:
- Use `api_call!` for all WMF API calls to get proper error context
- Use `.with_context(|| context!(...))` for additional context
- On buffer parsing errors: log warning and skip frame (return empty Vec or continue loop)
- Never panic or unwrap - this must keep streaming even with occasional frame errors

#### 3.3 NAL Unit Parsing
**Input**: Raw buffer from WMF (Annex B format with start codes)

**Algorithm**:
1. Scan for start codes (`00 00 00 01` or `00 00 01`)
2. Parse NAL unit header (first byte after start code)
3. Extract NAL unit type from header: `(header >> 0) & 0x1F`
4. Split into separate `EncodedNALUnit` structs

**Error handling**:
- Use `api_call!` macro for all MF API calls
- Use `.with_context(|| context!("parsing NAL units from buffer"))` for error context
- On parse error, log and return empty Vec (skip frame)
- Never panic or unwrap - this is a livestreaming app

**Safety considerations**:
- Use RAII guard for `IMFMediaBuffer::Lock()/Unlock()` (manual implementation needed)
- Verify buffer size before parsing
- Windows crate handles COM reference counting automatically (no manual Drop needed)

---

### Step 4: Create Stream Manager Module

**File**: `src/stream.rs` (NEW)

**Purpose**: Thread-safe circular buffer for encoded frames, bridging encoder and protocol handler.

**Key structures**:
```rust
pub struct StreamManager {
    frame_queue: Arc<crossbeam::queue::ArrayQueue<StreamFrame>>,
    codec_params: Arc<Mutex<Option<CodecParams>>>,
    sequence_counter: AtomicU64,
}

pub struct StreamFrame {
    pub sequence: u64,
    pub nal_units: Vec<EncodedNALUnit>,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
}

pub struct CodecParams {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
```

**Implementation**:
```rust
impl StreamManager {
    pub fn new(capacity: usize) -> Self {
        Self {
            frame_queue: Arc::new(ArrayQueue::new(capacity)),
            codec_params: Arc::new(Mutex::new(None)),
            sequence_counter: AtomicU64::new(0),
        }
    }

    /// Called by encoder after encoding each frame
    pub fn push_frame(&self, nal_units: Vec<EncodedNALUnit>) -> anyhow::Result<()> {
        // Cache SPS/PPS for new clients
        let has_sps = nal_units.iter().any(|u| u.unit_type == NALUnitType::SPS);
        let has_pps = nal_units.iter().any(|u| u.unit_type == NALUnitType::PPS);
        if has_sps && has_pps {
            let sps = nal_units.iter().find(|u| u.unit_type == NALUnitType::SPS).unwrap();
            let pps = nal_units.iter().find(|u| u.unit_type == NALUnitType::PPS).unwrap();
            *self.codec_params.lock().unwrap() = Some(CodecParams {
                sps: sps.data.clone(),
                pps: pps.data.clone(),
                width: 1920,  // TODO: Get from encoder
                height: 1080,
            });
        }

        let is_keyframe = nal_units.iter().any(|u| u.unit_type == NALUnitType::IDR);
        let sequence = self.sequence_counter.fetch_add(1, Ordering::SeqCst);

        let frame = StreamFrame {
            sequence,
            nal_units,
            timestamp_us: nal_units[0].timestamp_us,
            is_keyframe,
        };

        // If queue is full, drop oldest frame (live streaming behavior)
        if self.frame_queue.is_full() {
            let _ = self.frame_queue.pop();
            log::warn!("Stream queue full, dropping frame");
        }

        self.frame_queue.push(frame).map_err(|_| anyhow::anyhow!("Failed to push frame"))
    }

    /// Called by protocol handler (blocking with timeout)
    pub fn wait_for_frame(&self, after_sequence: u64, timeout: Duration)
        -> Option<StreamFrame> {
        let start = Instant::now();
        loop {
            // Scan queue for frame with sequence > after_sequence
            if let Some(frame) = self.peek_frame_after(after_sequence) {
                return Some(frame);
            }

            if start.elapsed() > timeout {
                return None;
            }

            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn get_codec_params(&self) -> Option<CodecParams> {
        self.codec_params.lock().unwrap().clone()
    }
}
```

**Design rationale**:
- **Capacity = 60 frames**: ~1 second buffer at 60fps (balance memory vs resilience)
- **Lock-free queue**: Hot path (encoder → queue) has no mutex
- **Mutex on codec params only**: Cold path (initialization only)
- **Drop on overflow**: Live streaming behavior (not recording)

---

### Step 5: Create Encoding Thread Architecture

**File**: `src/encoding_thread.rs` (NEW)

**Purpose**: Run encoding pipeline on separate thread to avoid blocking UI thread.

**Architecture**:
```rust
pub struct EncodingThread {
    frame_sender: std::sync::mpsc::Sender<CaptureFrame>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

pub struct CaptureFrame {
    pub texture: ID3D11Texture2D,  // Staging texture (CPU-readable copy)
    pub timestamp_us: u64,
}

impl EncodingThread {
    pub fn new(
        device: ID3D11Device,
        device_context: ID3D11DeviceContext,
        width: u32,
        height: u32,
        stream_manager: Arc<StreamManager>) -> anyhow::Result<Self>
    {
        let (frame_sender, frame_receiver) = std::sync::mpsc::channel::<CaptureFrame>();

        let thread_handle = std::thread::Builder::new()
            .name("EncodingThread".to_string())
            .spawn(move || {
                encoding_thread_main(
                    device,
                    device_context,
                    width,
                    height,
                    stream_manager,
                    frame_receiver);
            })
            .with_context(|| context!("spawning encoding thread"))?;

        Ok(Self {
            frame_sender,
            thread_handle: Some(thread_handle),
        })
    }

    /// Send frame to encoding thread (non-blocking)
    pub fn send_frame(&self, frame: CaptureFrame) {
        // If channel is full or disconnected, log and drop frame
        if let Err(e) = self.frame_sender.send(frame) {
            log::warn!("Failed to send frame to encoding thread: {:?}", e);
        }
    }
}

impl Drop for EncodingThread {
    fn drop(&mut self) {
        // Drop sender to signal thread to exit
        drop(self.frame_sender.clone());

        // Wait for thread to finish
        if let Some(handle) = self.thread_handle.take() {
            if let Err(e) = handle.join() {
                log::error!("Encoding thread panicked: {:?}", e);
            }
        }
    }
}

fn encoding_thread_main(
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    stream_manager: Arc<StreamManager>,
    frame_receiver: std::sync::mpsc::Receiver<CaptureFrame>)
{
    log::info!("Encoding thread started");

    // Initialize encoder and converter
    let mut encoder = match H264Encoder::new(&device, width, height, 60, 8_000_000) {
        Ok(enc) => enc,
        Err(e) => {
            log::error!("Failed to create encoder: {:?}", e);
            return;
        }
    };

    let mut converter = match FormatConverter::new(&device) {
        Ok(conv) => conv,
        Err(e) => {
            log::error!("Failed to create format converter: {:?}", e);
            return;
        }
    };

    // Create NV12 staging texture (reused for all frames)
    let nv12_texture = match create_nv12_texture(&device, width, height) {
        Ok(tex) => tex,
        Err(e) => {
            log::error!("Failed to create NV12 texture: {:?}", e);
            return;
        }
    };

    // Process frames until channel closes
    while let Ok(frame) = frame_receiver.recv() {
        // Convert BGRA -> NV12
        if let Err(e) = converter.convert(&frame.texture, &nv12_texture) {
            log::warn!("Format conversion failed: {:?}", e);
            continue;  // Skip this frame
        }

        // Encode to H.264
        match encoder.encode_frame(&nv12_texture, frame.timestamp_us) {
            Ok(nal_units) if !nal_units.is_empty() => {
                // Push to stream manager
                if let Err(e) = stream_manager.push_frame(nal_units) {
                    log::warn!("Failed to push frame to stream: {:?}", e);
                }
            }
            Ok(_) => {
                // Empty NAL units (encoder buffering) - this is normal
            }
            Err(e) => {
                log::error!("Encoding failed: {:?}", e);
                // Continue - try to recover with next frame
            }
        }
    }

    log::info!("Encoding thread stopped");
}

fn create_nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32) -> anyhow::Result<ID3D11Texture2D>
{
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut texture = None;
    api_call!(unsafe {
        device.CreateTexture2D(
            &raw const desc,
            None,
            Some(&raw mut texture))
    }).with_context(|| context!("creating NV12 staging texture"))?;

    texture.ok_or_else(|| anyhow::anyhow!("texture is null"))
}
```

**Key design points**:
- Use `std::sync::mpsc` channel (simple, built-in, sufficient for this use case)
- Encoding thread owns encoder, converter, and NV12 texture
- UI thread only needs to copy captured texture and send to channel
- On channel errors or encoding failures, log and continue (robustness)
- Thread exits gracefully when channel closes

---

### Step 6: Integrate Custom Protocol Handler

**File**: `src/app.rs` (MODIFY)

#### 6.1 Add modules and imports
```rust
mod converter;
mod encoder;
mod stream;
mod encoding_thread;

use encoding_thread::EncodingThread;
use stream::StreamManager;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
```

#### 6.2 Modify `LiveApp` struct
```rust
struct LiveApp {
    main_capture: Option<CaptureSession>,

    // NEW: Encoding pipeline
    encoding_thread: Option<EncodingThread>,
    stream_manager: Arc<StreamManager>,

    // NEW: Staging texture for copying captured frames
    staging_texture: ID3D11Texture2D,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,

    frontend_window: Window,
    frontend_webview: WebView,
    frontend_capture: CaptureSession,

    control_window: Window,
    output_window: Window,
}
```

#### 6.3 Modify `LiveApp::new()` to add custom protocol and encoding thread
```rust
fn new(event_loop: &ActiveEventLoop) -> anyhow::Result<Self> {
    // ... existing device creation ...

    // NEW: Create stream manager
    let stream_manager = Arc::new(StreamManager::new(60));
    let stream_manager_for_protocol = Arc::clone(&stream_manager);

    // ... existing window creation ...

    // NEW: Add custom protocol handler
    let frontend_webview = WebViewBuilder::new()
        .with_url("http://localhost:9688/")
        .with_custom_protocol("stream", move |request| {
            handle_stream_request(&stream_manager_for_protocol, request)
        })
        .build(&frontend_window)
        .context("failed to create webview for frontend window")?;

    // ... existing capture session creation ...

    // NEW: Create staging texture for copying captured frames
    let (width, height) = frontend_capture.frame_buffer_size();
    let staging_texture = create_staging_texture(&device, width, height)
        .context("failed to create staging texture")?;

    // NEW: Start encoding thread
    let encoding_thread = EncodingThread::new(
        device.clone(),
        device_context.clone(),
        width,
        height,
        Arc::clone(&stream_manager))
        .context("failed to create encoding thread")?;

    Ok(Self {
        main_capture,
        encoding_thread: Some(encoding_thread),
        stream_manager,
        staging_texture,
        device: device.clone(),
        device_context: device_context.clone(),
        frontend_window,
        frontend_webview,
        frontend_capture,
        control_window,
        output_window,
    })
}

fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32) -> anyhow::Result<ID3D11Texture2D>
{
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,  // Match capture format
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut texture = None;
    api_call!(unsafe {
        device.CreateTexture2D(
            &raw const desc,
            None,
            Some(&raw mut texture))
    }).with_context(|| context!("creating staging texture for encoding"))?;

    texture.ok_or_else(|| anyhow::anyhow!("staging texture is null"))
}
```

#### 5.4 Add protocol handler function
```rust
fn handle_stream_request(
    manager: &Arc<StreamManager>,
    request: wry::http::Request<Vec<u8>>)
    -> Result<wry::http::Response<Cow<'static, [u8]>>, Box<dyn std::error::Error>>
{
    use wry::http::Response;

    let path = request.uri().path();

    match path {
        "/init" => {
            // Return SPS/PPS as JSON
            let params = manager.get_codec_params()
                .ok_or("encoder not initialized")?;

            let response = serde_json::json!({
                "sps": base64_encode(&params.sps),
                "pps": base64_encode(&params.pps),
                "width": params.width,
                "height": params.height,
            });

            Response::builder()
                .header("Content-Type", "application/json")
                .header("Access-Control-Allow-Origin", "*")
                .body(Cow::Owned(response.to_string().into_bytes()))
                .map_err(Into::into)
        }

        "/stream" => {
            // Long-polling endpoint for next frame
            let query = request.uri().query().unwrap_or("");
            let after_seq = parse_query_param(query, "after").unwrap_or(0);

            // Wait for next frame (timeout = 100ms)
            let frame = manager.wait_for_frame(after_seq, Duration::from_millis(100))
                .ok_or("timeout waiting for frame")?;

            // Serialize frame as binary
            let mut buffer = Vec::new();
            serialize_stream_frame(&frame, &mut buffer)?;

            Response::builder()
                .header("Content-Type", "application/octet-stream")
                .header("X-Sequence", frame.sequence.to_string())
                .header("X-Timestamp", frame.timestamp_us.to_string())
                .header("X-Keyframe", frame.is_keyframe.to_string())
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Expose-Headers", "X-Sequence,X-Timestamp,X-Keyframe")
                .body(Cow::Owned(buffer))
                .map_err(Into::into)
        }

        _ => {
            Response::builder()
                .status(404)
                .body(Cow::Borrowed(&[]))
                .map_err(Into::into)
        }
    }
}

/// Binary frame format:
/// [u64: timestamp_us]
/// [u32: num_nal_units]
/// For each NAL unit:
///   [u8: nal_unit_type]
///   [u32: data_length]
///   [u8[]: data with start code]
fn serialize_stream_frame(frame: &StreamFrame, buffer: &mut Vec<u8>) -> anyhow::Result<()> {
    buffer.extend_from_slice(&frame.timestamp_us.to_le_bytes());
    buffer.extend_from_slice(&(frame.nal_units.len() as u32).to_le_bytes());

    for unit in &frame.nal_units {
        buffer.push(unit.unit_type as u8);
        buffer.extend_from_slice(&(unit.data.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&unit.data);
    }

    Ok(())
}

fn base64_encode(data: &[u8]) -> String {
    // Simple base64 encoding (can use base64 crate or implement manually)
    use std::io::Write;
    let mut buf = Vec::new();
    let mut encoder = base64::write::EncoderWriter::new(&mut buf, &base64::engine::STANDARD);
    encoder.write_all(data).unwrap();
    drop(encoder);
    String::from_utf8(buf).unwrap()
}

fn parse_query_param(query: &str, key: &str) -> Option<u64> {
    query.split('&')
        .find_map(|pair| {
            let mut parts = pair.split('=');
            if parts.next()? == key {
                parts.next()?.parse().ok()
            } else {
                None
            }
        })
}
```

**Note**: If base64 is not available, add `base64 = "0.22"` to Cargo.toml or implement manual encoding.

#### 6.5 Modify `on_window_event()` to send frames to encoding thread
```rust
fn on_window_event(&mut self, window_id: WindowId, event: WindowEvent) {
    match window_id {
        id if id == self.frontend_window.id() => {
            self.frontend_capture.update();

            // NEW: Copy frame and send to encoding thread
            if let Some(encoding_thread) = &self.encoding_thread {
                let source_texture = self.frontend_capture.frame_buffer();

                // Copy to staging texture (fast GPU operation)
                unsafe {
                    self.device_context.CopyResource(&self.staging_texture, source_texture);
                }

                // Get timestamp
                let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
                    Ok(duration) => duration.as_micros() as u64,
                    Err(e) => {
                        log::warn!("System time error: {:?}", e);
                        0  // Fallback to 0, encoder will still work
                    }
                };

                // Send to encoding thread (non-blocking)
                encoding_thread.send_frame(CaptureFrame {
                    texture: self.staging_texture.clone(),
                    timestamp_us: timestamp,
                });
            }
        }
        _ => {}
    }
}
```

**Note**: The `CopyResource()` operation is very fast (~0.1-0.5ms) and doesn't block significantly. The staging texture is then passed to the encoding thread which does the heavy lifting.

---

### Step 7: Create Frontend Decoder

**File**: `frontend/decoder.ts` (NEW)

```typescript
export interface NALUnitData {
    type: number;
    data: Uint8Array;
}

export interface StreamFrameData {
    timestamp: number;
    nalUnits: NALUnitData[];
    isKeyframe: boolean;
}

export class H264Decoder {
    private decoder: VideoDecoder | null = null;
    private onFrame: (frame: VideoFrame) => void;
    private isConfigured = false;

    constructor(onFrame: (frame: VideoFrame) => void) {
        this.onFrame = onFrame;
    }

    async init() {
        // Fetch SPS/PPS from stream://init
        const response = await fetch('stream://init');
        if (!response.ok) {
            throw new Error(`Failed to fetch codec params: ${response.statusText}`);
        }

        const params = await response.json();
        const sps = base64ToUint8Array(params.sps);
        const pps = base64ToUint8Array(params.pps);
        const width = params.width;
        const height = params.height;

        // Parse profile/level from SPS (bytes 1-3)
        const profile = sps[1];
        const level = sps[3];

        // Build codec string (e.g., "avc1.42001f" for baseline profile level 3.1)
        const codecString = `avc1.${toHex(profile)}00${toHex(level)}`;

        // Build avcC descriptor (ISO 14496-15 format)
        const avcC = buildAvcCDescriptor(sps, pps);

        this.decoder = new VideoDecoder({
            output: (frame) => this.handleFrame(frame),
            error: (e) => console.error('Decoder error:', e),
        });

        const config: VideoDecoderConfig = {
            codec: codecString,
            codedWidth: width,
            codedHeight: height,
            description: avcC,
        };

        this.decoder.configure(config);
        this.isConfigured = true;

        console.log(`Decoder initialized: ${codecString}, ${width}x${height}`);
    }

    async decodeFrame(frameData: StreamFrameData) {
        if (!this.decoder || !this.isConfigured) {
            throw new Error('Decoder not initialized');
        }

        // Concatenate all NAL units for this frame
        const totalSize = frameData.nalUnits.reduce((sum, unit) => sum + unit.data.length, 0);
        const combined = new Uint8Array(totalSize);

        let offset = 0;
        for (const unit of frameData.nalUnits) {
            combined.set(unit.data, offset);
            offset += unit.data.length;
        }

        const chunk = new EncodedVideoChunk({
            type: frameData.isKeyframe ? 'key' : 'delta',
            timestamp: frameData.timestamp,
            data: combined,
        });

        this.decoder.decode(chunk);
    }

    private handleFrame(frame: VideoFrame) {
        this.onFrame(frame);
    }

    close() {
        if (this.decoder) {
            this.decoder.close();
            this.decoder = null;
        }
    }
}

function buildAvcCDescriptor(sps: Uint8Array, pps: Uint8Array): Uint8Array {
    // ISO 14496-15 avcC format
    const spsLength = sps.length;
    const ppsLength = pps.length;

    const avcC = new Uint8Array(
        1 +  // configurationVersion
        3 +  // AVCProfileIndication, profile_compatibility, AVCLevelIndication
        1 +  // lengthSizeMinusOne
        1 +  // numOfSequenceParameterSets
        2 + spsLength +  // SPS length (16-bit) + data
        1 +  // numOfPictureParameterSets
        2 + ppsLength    // PPS length (16-bit) + data
    );

    let offset = 0;

    // configurationVersion = 1
    avcC[offset++] = 1;

    // Copy profile/level from SPS (bytes 1-3)
    avcC[offset++] = sps[1];  // AVCProfileIndication
    avcC[offset++] = sps[2];  // profile_compatibility
    avcC[offset++] = sps[3];  // AVCLevelIndication

    // lengthSizeMinusOne = 0xFF (4 bytes)
    avcC[offset++] = 0xFF;

    // numOfSequenceParameterSets = 1
    avcC[offset++] = 0xE1;

    // SPS length (16-bit big-endian)
    avcC[offset++] = (spsLength >> 8) & 0xFF;
    avcC[offset++] = spsLength & 0xFF;

    // SPS data
    avcC.set(sps, offset);
    offset += spsLength;

    // numOfPictureParameterSets = 1
    avcC[offset++] = 1;

    // PPS length (16-bit big-endian)
    avcC[offset++] = (ppsLength >> 8) & 0xFF;
    avcC[offset++] = ppsLength & 0xFF;

    // PPS data
    avcC.set(pps, offset);

    return avcC;
}

function base64ToUint8Array(base64: string): Uint8Array {
    const binaryString = atob(base64);
    const len = binaryString.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binaryString.charCodeAt(i);
    }
    return bytes;
}

function toHex(value: number): string {
    return value.toString(16).padStart(2, '0');
}

export function parseStreamFrame(buffer: Uint8Array): StreamFrameData {
    const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);

    let offset = 0;

    // Read timestamp (u64 little-endian)
    const timestamp = Number(view.getBigUint64(offset, true));
    offset += 8;

    // Read number of NAL units (u32 little-endian)
    const numNalUnits = view.getUint32(offset, true);
    offset += 4;

    const nalUnits: NALUnitData[] = [];
    let isKeyframe = false;

    for (let i = 0; i < numNalUnits; i++) {
        // Read NAL unit type (u8)
        const type = view.getUint8(offset);
        offset += 1;

        // Read data length (u32 little-endian)
        const dataLength = view.getUint32(offset, true);
        offset += 4;

        // Read data
        const data = buffer.slice(offset, offset + dataLength);
        offset += dataLength;

        nalUnits.push({ type, data });

        // Check if this is an IDR frame (type 5)
        if (type === 5) {
            isKeyframe = true;
        }
    }

    return { timestamp, nalUnits, isKeyframe };
}
```

---

### Step 8: Create Frontend Renderer Component

**File**: `frontend/renderer.tsx` (NEW)

```typescript
import { useRef, useEffect } from 'preact/hooks';
import { css } from '@emotion/css';
import { H264Decoder, parseStreamFrame, type StreamFrameData } from './decoder';

export function VideoRenderer() {
    const canvasRef = useRef<HTMLCanvasElement>(null);
    const decoderRef = useRef<H264Decoder | null>(null);
    const animationFrameRef = useRef<number | null>(null);

    useEffect(() => {
        const canvas = canvasRef.current;
        if (!canvas) return;

        const ctx = canvas.getContext('2d');
        if (!ctx) {
            console.error('Failed to get 2D context');
            return;
        }

        // Create decoder
        const decoder = new H264Decoder((frame: VideoFrame) => {
            renderFrame(canvas, ctx, frame);
        });

        decoderRef.current = decoder;

        // Initialize and start stream loop
        decoder.init()
            .then(() => {
                console.log('Decoder initialized, starting stream');
                startStreamLoop(decoder);
            })
            .catch((e) => {
                console.error('Failed to initialize decoder:', e);
            });

        return () => {
            if (animationFrameRef.current) {
                cancelAnimationFrame(animationFrameRef.current);
            }
            decoder.close();
        };
    }, []);

    return (
        <canvas
            ref={canvasRef}
            className={css({
                width: '100%',
                height: '100%',
                objectFit: 'contain',
                backgroundColor: '#000',
            })}
        />
    );
}

function renderFrame(canvas: HTMLCanvasElement, ctx: CanvasRenderingContext2D, frame: VideoFrame) {
    // Resize canvas if needed
    if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
        console.log(`Canvas resized to ${frame.displayWidth}x${frame.displayHeight}`);
    }

    // Draw frame
    ctx.drawImage(frame, 0, 0);

    // CRITICAL: Close frame to release GPU memory
    frame.close();
}

async function startStreamLoop(decoder: H264Decoder) {
    let lastSequence = 0;
    let consecutiveErrors = 0;
    const MAX_CONSECUTIVE_ERRORS = 10;

    while (true) {
        try {
            const response = await fetch(`stream://stream?after=${lastSequence}`);

            if (!response.ok) {
                console.warn(`Stream request failed: ${response.status}`);
                await sleep(100);
                consecutiveErrors++;
                if (consecutiveErrors >= MAX_CONSECUTIVE_ERRORS) {
                    console.error('Too many consecutive errors, stopping stream');
                    break;
                }
                continue;
            }

            consecutiveErrors = 0;

            // Parse headers
            const sequence = parseInt(response.headers.get('X-Sequence') || '0');
            lastSequence = sequence;

            // Parse binary frame data
            const arrayBuffer = await response.arrayBuffer();
            const frameData = parseStreamFrame(new Uint8Array(arrayBuffer));

            // Decode frame
            await decoder.decodeFrame(frameData);

        } catch (e) {
            console.error('Stream error:', e);
            consecutiveErrors++;
            if (consecutiveErrors >= MAX_CONSECUTIVE_ERRORS) {
                console.error('Too many consecutive errors, stopping stream');
                break;
            }
            await sleep(1000);
        }
    }
}

function sleep(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
}
```

---

### Step 9: Integrate Renderer into Frontend App

**File**: `frontend/app.tsx` (MODIFY)

```typescript
import { createContext } from 'preact';
import { useState } from 'preact/hooks';
import { css } from '@emotion/css';

import {
    FluentProvider,
    Card,
    webDarkTheme,
} from '@fluentui/react-components';

// NEW: Import VideoRenderer
import { VideoRenderer } from './renderer';

export function App() {
    return <FluentProvider theme={webDarkTheme} className={css({
        padding: '8px',
        display: 'flex',
        flexDirection: 'column',
        flex: 1,
        gap: '8px',
    })}>
        <div>header</div>
        <div className={css({
            display: 'flex',
            flexDirection: 'row',
            flex: 1,
            gap: '8px',
        })}>
            {/* MODIFY: Replace empty Card with VideoRenderer */}
            <Card className={css({
                flex: 5,
                borderColor: 'rgba(255, 255, 255, 0.25) !important',
                borderWidth: '1px !important',
                borderStyle: 'solid !important',
                borderRadius: '8px !important',
                backgroundColor: 'black !important',
                padding: '0 !important',  // NEW: Remove padding for video
                overflow: 'hidden',        // NEW: Prevent overflow
            })}>
                <VideoRenderer />
            </Card>
            <div className={css({
                flex: 1,
            })}>
               Hi, I'm Nekomaru OwO
            </div>
        </div>
        <div>footer</div>
    </FluentProvider>
}
```

---

## Expected Latency Budget

| Component | Expected Latency | Notes |
|-----------|------------------|-------|
| Capture | 0-16ms | Windows Graphics Capture API (1 frame) |
| GPU Copy | 0.1-0.5ms | CopyResource to staging texture |
| Channel Send | <0.1ms | mpsc channel (non-blocking) |
| Thread Switch | 0.5-2ms | Context switch to encoding thread |
| BGRA→NV12 | 0.5-1ms | GPU Video Processor |
| H.264 Encode | 5-15ms | Hardware encoder with low-latency settings |
| Protocol Handler | 1-5ms | Lock-free queue, binary serialization |
| Network | <1ms | Loopback (localhost) |
| WebCodecs Decode | 5-10ms | Hardware decoder |
| Canvas Render | 1-5ms | GPU-accelerated |
| **Total** | **14-56ms** | Well under 100ms target |

**Worst case**: Software encoder fallback adds 20-50ms (still <100ms for 1080p).

**Threading benefit**: UI thread is only blocked for ~0.5-1ms (GPU copy), keeping it responsive while encoding happens on separate thread.

---

## Critical Files Summary

### New Files (Create)
1. `src/converter.rs` - Format converter (BGRA→NV12) using Video Processor (stateless helper)
2. `src/encoder.rs` - WMF H.264 encoder with NAL unit extraction
3. `src/stream.rs` - Stream manager with circular buffer (lock-free queue)
4. `src/encoding_thread.rs` - Encoding thread architecture (owns encoder and converter)
5. `frontend/decoder.ts` - WebCodecs H.264 decoder wrapper
6. `frontend/renderer.tsx` - Canvas-based video renderer component

### Modified Files
1. `Cargo.toml` - Add `crossbeam = "0.8"` and `base64 = "0.22"` dependencies
2. `src/app.rs` - Add encoding thread, custom protocol handler, wire frame capture→encoding pipeline
3. `frontend/app.tsx` - Replace empty Card with VideoRenderer

### Additional Files (Declare modules)
- `src/main.rs` - Add `mod converter; mod encoder; mod stream; mod encoding_thread;`

---

## Safety Considerations and Error Handling

### COM Interface Handling
- **Windows crate handles COM automatically:** The `windows` crate manages `AddRef`/`Release` lifecycle automatically. No manual Drop implementations needed for COM interfaces.
- **Must:** Never dereference null COM pointers - always check `Option` from out-parameters using `.ok_or_else()`
- **Must:** Use `api_call!` macro for all Windows API calls to get proper error context

### Buffer Locking
- Use RAII guards for `IMFMediaBuffer::Lock()/Unlock()` (manual Drop needed for this one)
- Never access buffer after unlock
- Verify buffer size before parsing

### Error Handling Rules (Critical for Livestreaming)
- **Never panic or unwrap:** This is a live streaming application that must stay running
- **Use api_call! macro:** For all Windows API calls (provides error context automatically)
- **Use .with_context(|| context!(...)):** For additional error context when api_call! isn't sufficient
- **Log and continue:** On frame-level errors (encoding, parsing), log error with `log::warn!()` and skip frame
- **Log and recover:** On streaming-level errors (encoder init), log error with `log::error!()` and attempt recovery
- **Only fatal on init:** Errors during LiveApp::new() can be fatal (propagate with `?`)

### RAII Pattern for Buffer Locking
```rust
// Use RAII guard for IMFMediaBuffer::Lock/Unlock
struct BufferLock<'a> {
    buffer: &'a IMFMediaBuffer,
    ptr: *mut u8,
    len: usize,
}

impl<'a> BufferLock<'a> {
    fn lock(buffer: &'a IMFMediaBuffer) -> anyhow::Result<Self> {
        let mut ptr = std::ptr::null_mut();
        let mut current_len = 0;
        api_call!(unsafe {
            buffer.Lock(
                &raw mut ptr,
                None,
                Some(&raw mut current_len))
        }).with_context(|| context!("locking IMFMediaBuffer"))?;

        Ok(Self {
            buffer,
            ptr,
            len: current_len as usize
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        unsafe {
            // Ignore error - we're in Drop, can't propagate
            let _ = self.buffer.Unlock();
        }
    }
}
```

---

## Testing Checklist

### Manual Testing
- [ ] Verify stream starts when webview loads
- [ ] Verify smooth 60fps playback
- [ ] Measure end-to-end latency (capture to display)
- [ ] Test window resize behavior
- [ ] Test recovery from GPU device loss (sleep/wake)
- [ ] Test with high motion content
- [ ] Verify no memory leaks (run for 10+ minutes)
- [ ] Test hardware encoder selection (check logs)

### Performance Benchmarks
- [ ] Measure encoder latency per frame
- [ ] Measure format conversion latency
- [ ] Measure protocol handler latency
- [ ] Monitor memory usage over time
- [ ] Monitor CPU usage (should be low with hardware encoding)
- [ ] Monitor GPU usage

---

## Next Steps After Implementation

### Potential Enhancements (Future)
1. **Dynamic resolution**: Support runtime resolution changes
2. **Bitrate adaptation**: Adjust bitrate based on scene complexity
3. **Error concealment**: Add frame interpolation on decode errors
4. **Multiple streams**: Support encoding multiple capture sources
5. **Recording**: Add MP4 muxing for saving to disk
6. **Overlays**: Add text/image overlays before encoding

### Known Limitations
1. **Browser support**: Requires Chrome 94+, Edge 94+, or Safari 16.4+ for WebCodecs
2. **Windows only**: WMF is Windows-specific (consider ffmpeg for cross-platform)
3. **No adaptive streaming**: Fixed bitrate (could add HLS/DASH later)
4. **Local only**: No remote streaming support (could add WebRTC/RTMP later)

---

## Design Philosophy Summary

This implementation prioritizes:

1. **Latency**: Every component optimized for <100ms target
   - Hardware acceleration throughout (GPU converter, WMF encoder, WebCodecs decoder)
   - Synchronous encoding (no queuing latency)
   - Binary protocol (no JSON overhead)
   - Raw NAL units (no container overhead)

2. **Simplicity**: Avoid over-engineering
   - No separate encoding thread
   - No external streaming server
   - No complex container formats
   - Direct canvas rendering

3. **Robustness**: Graceful degradation
   - Frame dropping on overload (live stream behavior)
   - Fallback to software encoder
   - Automatic error recovery
   - RAII for all unsafe resources

4. **Performance**: Efficient resource usage
   - Zero-copy paths where possible
   - Texture and sample reuse
   - Lock-free queue for hot path
   - GPU acceleration for all heavy operations
