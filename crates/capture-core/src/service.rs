//! Platform-neutral Axum service and bounded native-host channel boundary.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, rejection::JsonRejection},
    extract::ws::{Message, WebSocket},
    http::{Method, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::Engine as _;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot, watch};
use tower_http::cors::{Any, CorsLayer};

use crate::{
    config::{
        ConfigError, ConfigSnapshot, InstanceConfig, RenderConfig,
        ValidatedInstanceConfig, ValidationIssue,
    },
    status::{CaptureState, MediaStatus, TargetSummary},
    video::{CodecConfiguration, VideoEvent, encode_event},
};

/// Maximum time one viewer may hold the handler on a socket write.
const VIEWER_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Capacities for every queued communication boundary owned by `capture-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCapacities {
    /// Rare commands from async viewer handling to the media owner.
    pub media_commands: usize,
    /// Encoded events applying backpressure between media and async domains.
    pub encoded_output: usize,
    /// Per-viewer broadcast history before a lagging viewer is disconnected.
    pub video_fanout: usize,
}

impl Default for ChannelCapacities {
    fn default() -> Self {
        Self { media_commands: 8, encoded_output: 8, video_fanout: 32 }
    }
}

/// Rare request delivered from the service domain to the media owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    /// Force the next independently decodable H.264 IDR access unit.
    RequestKeyframe,
}

/// Terminal result sent once when the native media owner exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaCompletion {
    /// The media owner completed without a fatal failure.
    Clean,
    /// A foundational media invariant failed and the process should exit.
    Fatal {
        /// Complete operator-facing failure diagnostic.
        message: String,
    },
}

/// Channel endpoints owned exclusively by the platform host.
pub struct HostChannels {
    /// Latest accepted complete configuration; superseded values do not queue.
    pub configurations: watch::Receiver<ConfigSnapshot>,
    /// Rare bounded commands such as fresh-keyframe requests.
    pub commands: mpsc::Receiver<MediaCommand>,
    /// Latest media status publisher.
    pub status: watch::Sender<MediaStatus>,
    /// Bounded encoded output publisher; `blocking_send` provides thread backpressure.
    pub video: mpsc::Sender<VideoEvent>,
    /// One-shot completion sender consumed when the media owner terminates.
    pub completion: oneshot::Sender<MediaCompletion>,
}

/// One configured instance service before or after its router is started.
pub struct InstanceService {
    state: Arc<ServiceState>,
    video_input: Option<mpsc::Receiver<VideoEvent>>,
    media_completion: Option<oneshot::Receiver<MediaCompletion>>,
}

impl InstanceService {
    /// Constructs the platform-neutral state and both sides of every channel.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::ZeroChannelCapacity`] when any bounded channel
    /// would otherwise be unusable.
    pub fn new(
        initial_config: ValidatedInstanceConfig,
        capacities: ChannelCapacities) -> Result<(Self, HostChannels), ServiceError> {
        for (name, capacity) in [
            ("media_commands", capacities.media_commands),
            ("encoded_output", capacities.encoded_output),
            ("video_fanout", capacities.video_fanout),
        ] {
            if capacity == 0 {
                return Err(ServiceError::ZeroChannelCapacity { name });
            }
        }

        let snapshot = ConfigSnapshot::initial(initial_config);
        let (config_tx, config_rx) = watch::channel(snapshot.clone());
        let (command_tx, command_rx) = mpsc::channel(capacities.media_commands);
        let (status_tx, status_rx) = watch::channel(MediaStatus::default());
        let (video_input_tx, video_input_rx) = mpsc::channel(capacities.encoded_output);
        let (video_tx, _) = broadcast::channel(capacities.video_fanout);
        let (completion_tx, completion_rx) = oneshot::channel();
        let state = Arc::new(ServiceState {
            config: Mutex::new(snapshot),
            config_tx,
            command_tx,
            status_rx,
            video_tx,
            codec: RwLock::new(None),
            viewer_count: AtomicUsize::new(0),
        });

        Ok((
            Self {
                state,
                video_input: Some(video_input_rx),
                media_completion: Some(completion_rx),
            },
            HostChannels {
                configurations: config_rx,
                commands: command_rx,
                status: status_tx,
                video: video_input_tx,
                completion: completion_tx,
            }))
    }

    /// Starts the encoded-output dispatcher and returns the complete Axum router.
    ///
    /// The router may be cloned after construction. Calling this method twice is
    /// rejected because one dispatcher must own the encoded-output receiver.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when no Tokio runtime is active or the router
    /// has already been started.
    pub fn router(&mut self) -> Result<Router, ServiceError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_runtime_error| ServiceError::MissingTokioRuntime)?;
        let mut video_input = self.video_input.take()
            .ok_or(ServiceError::RouterAlreadyStarted)?;
        let dispatcher_state = Arc::clone(&self.state);
        runtime.spawn(async move {
            while let Some(event) = video_input.recv().await {
                if let &VideoEvent::CodecConfiguration(ref config) = &event {
                    *dispatcher_state.codec.write().await = Some(config.clone());
                }
                // No receivers is ordinary while the capture instance has no viewers.
                let _ = dispatcher_state.video_tx.send(event);
            }
        });

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::PUT])
            .allow_headers([CONTENT_TYPE]);
        Ok(Router::new()
            .route("/api/status", get(get_status))
            .route("/api/config", get(get_config).put(put_config))
            .route("/api/initialization", get(get_initialization))
            .route("/api/video", get(video_upgrade))
            .layer(cors)
            .with_state(Arc::clone(&self.state)))
    }

    /// Takes the one-shot native media completion receiver.
    ///
    /// A process entry point takes this exactly once and selects it against the
    /// network server result. A second call returns `None`.
    pub const fn take_media_completion(
        &mut self) -> Option<oneshot::Receiver<MediaCompletion>> {
        self.media_completion.take()
    }
}

/// Failures constructing or starting the platform-neutral instance service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// A bounded channel was configured with no usable slots.
    #[error("channel {name} must have non-zero capacity")]
    ZeroChannelCapacity {
        /// Invalid capacity field name.
        name: &'static str,
    },
    /// Router startup requires an active Tokio runtime for the dispatcher.
    #[error("capture-core router requires an active Tokio runtime")]
    MissingTokioRuntime,
    /// The encoded-output receiver was already transferred to a dispatcher.
    #[error("capture-core router has already been started")]
    RouterAlreadyStarted,
}

/// State shared by handlers and the single encoded-output dispatcher.
struct ServiceState {
    config: Mutex<ConfigSnapshot>,
    config_tx: watch::Sender<ConfigSnapshot>,
    command_tx: mpsc::Sender<MediaCommand>,
    status_rx: watch::Receiver<MediaStatus>,
    video_tx: broadcast::Sender<VideoEvent>,
    codec: RwLock<Option<CodecConfiguration>>,
    viewer_count: AtomicUsize,
}

/// Public status assembled from media, configuration, and viewer snapshots.
#[derive(Serialize)]
struct StatusResponse {
    configuration_generation: u64,
    state: CaptureState,
    target: Option<TargetSummary>,
    output_width: u32,
    output_height: u32,
    frame_rate: u32,
    captured_frames: u64,
    encoded_frames: u64,
    capture_rate: f64,
    encode_rate: f64,
    viewer_count: usize,
}

/// `GET /api/status` returns only values useful for diagnosing one live instance.
async fn get_status(State(state): State<Arc<ServiceState>>) -> Json<StatusResponse> {
    let config = state.config.lock().await.clone();
    let status = state.status_rx.borrow().clone();
    let video = &config.config.config().video;
    Json(StatusResponse {
        configuration_generation: config.generation,
        state: status.state,
        target: status.target,
        output_width: video.width,
        output_height: video.height,
        frame_rate: video.frame_rate,
        captured_frames: status.captured_frames,
        encoded_frames: status.encoded_frames,
        capture_rate: status.capture_rate,
        encode_rate: status.encode_rate,
        viewer_count: state.viewer_count.load(Ordering::Relaxed),
    })
}

/// Complete configuration response shared by `GET` and successful `PUT`.
#[derive(Serialize)]
struct ConfigResponse {
    generation: u64,
    config: InstanceConfig,
}

/// `GET /api/config` returns the active canonical full configuration.
async fn get_config(State(state): State<Arc<ServiceState>>) -> Json<ConfigResponse> {
    let snapshot = state.config.lock().await;
    Json(ConfigResponse {
        generation: snapshot.generation,
        config: snapshot.config.config().clone(),
    })
}

/// `PUT /api/config` validates and atomically replaces one complete candidate.
async fn put_config(
    State(state): State<Arc<ServiceState>>,
    payload: Result<Json<InstanceConfig>, JsonRejection>) -> Result<Json<ConfigResponse>, ApiError> {
    let Json(candidate) = payload
        .map_err(|rejection| ApiError::malformed_json(&rejection))?;
    let mut active = state.config.lock().await;
    let candidate = active
        .config
        .validate_replacement(candidate)
        .map_err(ApiError::from_config)?;
    let next = active.replaced(candidate).map_err(ApiError::from_config)?;
    *active = next.clone();
    state.config_tx.send_replace(next.clone());
    drop(active);

    Ok(Json(ConfigResponse {
        generation: next.generation,
        config: next.config.config().clone(),
    }))
}

/// Viewer initialization response available before opening the WebSocket.
#[derive(Serialize)]
struct InitializationResponse {
    configuration_generation: u64,
    render: RenderConfig,
    decoder: Option<DecoderInitialization>,
}

/// JSON representation of the current WebCodecs decoder configuration.
#[derive(Serialize)]
struct DecoderInitialization {
    generation: u64,
    codec: String,
    width: u16,
    height: u16,
    description: String,
}

impl From<&CodecConfiguration> for DecoderInitialization {
    fn from(config: &CodecConfiguration) -> Self {
        Self {
            generation: config.generation(),
            codec: config.codec_string(),
            width: config.width(),
            height: config.height(),
            description: base64::engine::general_purpose::STANDARD
                .encode(config.avcc_description()),
        }
    }
}

/// `GET /api/initialization` returns current render and optional decoder data.
async fn get_initialization(
    State(state): State<Arc<ServiceState>>) -> Json<InitializationResponse> {
    let config = state.config.lock().await.clone();
    let status = state.status_rx.borrow().clone();
    let profile = status.target.as_ref().map(|target| target.profile.as_str());
    let render = config.config.render_for_profile(profile).clone();
    let decoder = state
        .codec
        .read()
        .await
        .as_ref()
        .map(DecoderInitialization::from);
    Json(InitializationResponse {
        configuration_generation: config.generation,
        render,
        decoder,
    })
}

/// Upgrade a viewer after all ordinary HTTP extraction has succeeded.
async fn video_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServiceState>>) -> Response {
    ws.on_upgrade(move |socket| handle_viewer(socket, state))
}

/// Per-viewer text message carrying independently mutable render parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ViewerControlMessage {
    /// Complete render parameters selected from configuration and media status.
    RenderConfiguration {
        configuration_generation: u64,
        profile: Option<String>,
        render: RenderConfig,
    },
    /// The media command boundary was unavailable during viewer startup.
    Error {
        code: &'static str,
        message: &'static str,
    },
}

/// Serve one viewer while gating decoder generations on fresh keyframes.
async fn handle_viewer(mut socket: WebSocket, state: Arc<ServiceState>) {
    let _viewer = ViewerCountGuard::new(&state.viewer_count);
    let mut video_rx = state.video_tx.subscribe();
    let mut config_rx = state.config_tx.subscribe();
    let mut status_rx = state.status_rx.clone();
    let mut last_render = None;

    if send_current_render(
        &mut socket,
        &config_rx,
        &status_rx,
        &mut last_render).await.is_err()
    {
        return;
    }
    let cached_codec = state.codec.read().await.clone();
    let mut codec_generation = cached_codec.as_ref().map(CodecConfiguration::generation);
    if let Some(config) = cached_codec
        && send_video_event(&mut socket, &VideoEvent::CodecConfiguration(config)).await.is_err()
    {
        return;
    }

    // Subscription happens first, so any accepted IDR is newer than the
    // viewer's observation boundary even if it races the explicit command.
    if request_keyframe(&mut socket, &state).await.is_err() {
        return;
    }

    let mut waiting_for_keyframe = true;
    loop {
        tokio::select! {
            message = socket.recv() => match message {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                _ => {}
            },
            changed = config_rx.changed() => {
                if changed.is_err()
                    || send_current_render(
                        &mut socket,
                        &config_rx,
                        &status_rx,
                        &mut last_render).await.is_err()
                {
                    break;
                }
            }
            changed = status_rx.changed() => {
                if changed.is_err()
                    || send_current_render(
                        &mut socket,
                        &config_rx,
                        &status_rx,
                        &mut last_render).await.is_err()
                {
                    break;
                }
            }
            event = video_rx.recv() => match event {
                Ok(event) => if forward_video_event(
                    &mut socket,
                    &state,
                    event,
                    &mut codec_generation,
                    &mut waiting_for_keyframe).await.is_err()
                {
                    break;
                },
                Err(broadcast::error::RecvError::Lagged(_)
                    | broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

/// Apply decoder-generation and fresh-keyframe gates to one broadcast event.
async fn forward_video_event(
    socket: &mut WebSocket,
    state: &ServiceState,
    event: VideoEvent,
    codec_generation: &mut Option<u64>,
    waiting_for_keyframe: &mut bool) -> Result<(), ()> {
    match event {
        VideoEvent::CodecConfiguration(config) => {
            let decoder_discontinuity = codec_generation
                .is_some_and(|generation| generation != config.generation());
            *codec_generation = Some(config.generation());
            *waiting_for_keyframe = true;
            send_video_event(socket, &VideoEvent::CodecConfiguration(config)).await?;
            if decoder_discontinuity {
                request_keyframe(socket, state).await?;
            }
        }
        VideoEvent::AccessUnit(unit) => {
            if *codec_generation != Some(unit.codec_generation())
                || *waiting_for_keyframe && !unit.is_keyframe()
            {
                return Ok(());
            }
            *waiting_for_keyframe = false;
            send_video_event(socket, &VideoEvent::AccessUnit(unit)).await?;
        }
    }
    Ok(())
}

/// Send the render configuration matching the latest config and target profile.
async fn send_current_render(
    socket: &mut WebSocket,
    config_rx: &watch::Receiver<ConfigSnapshot>,
    status_rx: &watch::Receiver<MediaStatus>,
    last_render: &mut Option<ViewerControlMessage>) -> Result<(), ()> {
    let config = config_rx.borrow().clone();
    let profile = status_rx
        .borrow()
        .target
        .as_ref()
        .map(|target| target.profile.clone());
    let render = config.config.render_for_profile(profile.as_deref()).clone();
    let message = ViewerControlMessage::RenderConfiguration {
        configuration_generation: config.generation,
        profile,
        render,
    };
    if last_render.as_ref() == Some(&message) {
        return Ok(());
    }
    send_control(socket, &message).await?;
    *last_render = Some(message);
    Ok(())
}

/// Queue one fresh-IDR request or explain why this viewer cannot initialize.
async fn request_keyframe(
    socket: &mut WebSocket,
    state: &ServiceState) -> Result<(), ()> {
    if state.command_tx.try_send(MediaCommand::RequestKeyframe).is_ok() {
        return Ok(());
    }
    let _send_result = send_control(
        socket,
        &ViewerControlMessage::Error {
            code: "media_command_unavailable",
            message: "media owner cannot accept a fresh-keyframe request",
        }).await;
    Err(())
}

/// Serialize and send one text control message without panicking on JSON errors.
async fn send_control(
    socket: &mut WebSocket,
    message: &ViewerControlMessage) -> Result<(), ()> {
    let serialized = serde_json::to_string(message).map_err(|_serialize_error| ())?;
    complete_viewer_send(
        socket.send(Message::Text(serialized.into())),
        VIEWER_SEND_TIMEOUT).await
}

/// Serialize and send one checked binary video event.
async fn send_video_event(socket: &mut WebSocket, event: &VideoEvent) -> Result<(), ()> {
    let encoded = encode_event(event).map_err(|_protocol_error| ())?;
    complete_viewer_send(
        socket.send(Message::Binary(encoded.into())),
        VIEWER_SEND_TIMEOUT).await
}

/// Bound one socket write so a stalled viewer cannot retain its handler forever.
async fn complete_viewer_send<F, E>(send: F, deadline: Duration) -> Result<(), ()>
where
    F: Future<Output = Result<(), E>>,
{
    tokio::time::timeout(deadline, send)
        .await
        .map_err(|_elapsed| ())?
        .map_err(|_send_error| ())
}

/// RAII viewer counter that cannot leak on an early handler return.
struct ViewerCountGuard<'a> {
    count: &'a AtomicUsize,
}

impl<'a> ViewerCountGuard<'a> {
    /// Increment the shared count for the lifetime of this guard.
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self { count }
    }
}

impl Drop for ViewerCountGuard<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Structured API error envelope returned for every expected client failure.
#[derive(Serialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

/// Machine-readable error detail with optional validation context.
#[derive(Serialize)]
struct ApiErrorDetail {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    issues: Vec<ValidationIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restart_fields: Vec<String>,
}

/// Expected API failure paired with its ordinary HTTP classification.
struct ApiError {
    status: StatusCode,
    detail: ApiErrorDetail,
}

impl ApiError {
    /// Translate Axum's JSON rejection into the service's stable error shape.
    fn malformed_json(rejection: &JsonRejection) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            detail: ApiErrorDetail {
                code: "malformed_json",
                message: rejection.body_text(),
                issues: Vec::new(),
                restart_fields: Vec::new(),
            },
        }
    }

    /// Translate typed configuration failures without losing validation detail.
    fn from_config(error: ConfigError) -> Self {
        match error {
            ConfigError::Invalid { issues } => Self {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                detail: ApiErrorDetail {
                    code: "invalid_configuration",
                    message: "configuration failed semantic validation".to_owned(),
                    issues,
                    restart_fields: Vec::new(),
                },
            },
            ConfigError::RestartRequired { fields } => Self {
                status: StatusCode::CONFLICT,
                detail: ApiErrorDetail {
                    code: "restart_required",
                    message: "configuration changes startup-only media settings".to_owned(),
                    issues: Vec::new(),
                    restart_fields: fields,
                },
            },
            error => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                detail: ApiErrorDetail {
                    code: "configuration_state_error",
                    message: error.to_string(),
                    issues: Vec::new(),
                    restart_fields: Vec::new(),
                },
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ApiErrorBody { error: self.detail })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

    use axum::{
        body::{Body, to_bytes},
        http::{
            Method, Request,
            header::{
                ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_HEADERS,
                ACCESS_CONTROL_REQUEST_METHOD, CONTENT_TYPE, ORIGIN,
            },
        },
    };
    use futures_util::StreamExt as _;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::{connect_async, tungstenite};
    use tower::ServiceExt as _;

    use crate::{
        config::{
            RenderProfiles, SelectionConfig, SelectionProfileConfig, SourceConfig,
            VideoConfig,
        },
        video::{AccessUnit, CodecConfiguration, VideoMessage, decode_message},
    };

    use super::*;

    /// Complete valid test configuration with one render-aware profile.
    fn test_config() -> InstanceConfig {
        InstanceConfig {
            selection: SelectionConfig {
                prefer_foreground: true,
                enabled: vec!["code".to_owned()],
                profiles: BTreeMap::from([(
                    "code".to_owned(),
                    SelectionProfileConfig {
                        include: vec!["Code.exe".to_owned()],
                        exclude: vec![],
                    })]),
            },
            source: SourceConfig::default(),
            video: VideoConfig {
                width: 1920,
                height: 1200,
                frame_rate: 60,
                bit_rate: 8_000_000,
            },
            render: RenderProfiles::default(),
        }
    }

    /// Build one valid decoder configuration with generation-specific bytes.
    fn test_codec(generation: u64, suffix: u8) -> CodecConfiguration {
        CodecConfiguration::new(
            generation,
            1920,
            1200,
            vec![0x67, 0x42, 0xC0, 0x1E, suffix],
            vec![0x68, 0xCE, 0x38, suffix])
            .unwrap()
    }

    /// Build one valid single-NAL access unit for viewer-gating tests.
    fn test_access_unit(
        generation: u64,
        timestamp_us: u64,
        keyframe: bool) -> AccessUnit {
        AccessUnit::new(
            generation,
            timestamp_us,
            keyframe,
            vec![
                0,
                0,
                0,
                2,
                if keyframe { 0x65 } else { 0x41 },
                timestamp_us as u8])
            .unwrap()
    }

    /// Emulate the native owner across initial and replacement codecs.
    async fn run_fake_media(mut host: HostChannels) -> HostChannels {
        assert_eq!(host.commands.recv().await, Some(MediaCommand::RequestKeyframe));
        host.video
            .send(VideoEvent::AccessUnit(test_access_unit(1, 1, false)))
            .await
            .unwrap();
        host.video
            .send(VideoEvent::AccessUnit(test_access_unit(1, 2, true)))
            .await
            .unwrap();
        host.video
            .send(VideoEvent::CodecConfiguration(test_codec(2, 0xDA)))
            .await
            .unwrap();
        assert_eq!(host.commands.recv().await, Some(MediaCommand::RequestKeyframe));
        host.video
            .send(VideoEvent::AccessUnit(test_access_unit(2, 3, false)))
            .await
            .unwrap();
        host.video
            .send(VideoEvent::AccessUnit(test_access_unit(2, 4, true)))
            .await
            .unwrap();
        host
    }

    /// Construct a started router and its fake native-host endpoints.
    fn test_service() -> (Router, HostChannels) {
        let (mut service, host) = InstanceService::new(
            test_config().validate().unwrap(),
            ChannelCapacities::default())
            .unwrap();
        let router = service.router().unwrap();
        (router, host)
    }

    /// Bind one router to an ephemeral loopback port for transport-level tests.
    async fn spawn_test_server(router: Router) -> (SocketAddr, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, server)
    }

    /// Wait until the dispatcher has published an initial decoder configuration.
    async fn wait_for_decoder_cache(router: &Router) {
        for _ in 0..10 {
            let response = router
                .clone()
                .oneshot(Request::get("/api/initialization").body(Body::empty()).unwrap())
                .await
                .unwrap();
            if !response_json(response).await["decoder"].is_null() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("decoder configuration did not reach the service cache");
    }

    /// Wait for the status snapshot to report an expected active viewer count.
    async fn wait_for_viewer_count(router: &Router, expected: usize) {
        for _ in 0..20 {
            let response = router
                .clone()
                .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
                .await
                .unwrap();
            if response_json(response).await["viewer_count"] == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("viewer count did not reach {expected}");
    }

    /// Read until one access unit arrives, ignoring render and decoder setup.
    async fn receive_access_unit(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) -> AccessUnit {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let tungstenite::Message::Binary(data) = message
                && let VideoMessage::AccessUnit(unit) = decode_message(&data).unwrap()
            {
                return unit;
            }
        }
    }

    /// Request and publish a distinct fresh keyframe for two viewer lifetimes.
    async fn run_reconnecting_media(mut host: HostChannels) -> HostChannels {
        for timestamp_us in [10, 20] {
            assert_eq!(host.commands.recv().await, Some(MediaCommand::RequestKeyframe));
            host.video
                .send(VideoEvent::AccessUnit(test_access_unit(1, timestamp_us - 1, false)))
                .await
                .unwrap();
            host.video
                .send(VideoEvent::AccessUnit(test_access_unit(1, timestamp_us, true)))
                .await
                .unwrap();
        }
        host
    }

    /// Fetch one bound instance's configuration response through real HTTP.
    async fn fetch_bound_config(address: SocketAddr) -> serde_json::Value {
        let body = reqwest::get(format!("http://{address}/api/config"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        serde_json::from_str(&body).unwrap()
    }

    /// Decode one response body as JSON for stable shape assertions.
    async fn response_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn status_should_combine_media_configuration_and_viewer_snapshots() {
        let (router, host) = test_service();
        host.status.send_replace(MediaStatus {
            state: CaptureState::Capturing,
            target: Some(TargetSummary {
                profile: "code".to_owned(),
                executable_name: "Code.exe".to_owned(),
                title: "capture-core".to_owned(),
            }),
            captured_frames: 10,
            encoded_frames: 9,
            capture_rate: 60.0,
            encode_rate: 59.5,
        });

        let response = router
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = response_json(response).await;

        assert_eq!(body["configuration_generation"], 1);
        assert_eq!(body["state"], "capturing");
        assert_eq!(body["target"]["profile"], "code");
    }

    #[tokio::test]
    async fn status_should_not_include_transient_media_diagnostics() {
        let (router, _host) = test_service();
        let response = router
            .oneshot(Request::get("/api/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = response_json(response).await;

        assert!(body.get("diagnostic").is_none());
    }

    #[tokio::test]
    async fn invalid_put_should_retain_the_last_valid_generation() {
        let (router, _host) = test_service();
        let mut candidate = test_config();
        candidate.video.width = 1;
        let payload = serde_json::to_vec(&candidate).unwrap();

        let rejected = router
            .clone()
            .oneshot(Request::put("/api/config")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap())
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let active = router
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = response_json(active).await;
        assert_eq!(body["generation"], 1);
        assert_eq!(body["config"]["video"]["width"], 1920);
    }

    #[tokio::test]
    async fn malformed_json_should_use_the_structured_error_envelope() {
        let (router, _host) = test_service();
        let response = router
            .oneshot(Request::put("/api/config")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{not-json"))
                .unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "malformed_json");
    }

    #[tokio::test]
    async fn trusted_cross_origin_preflight_should_allow_configuration_updates() {
        let (router, _host) = test_service();
        let response = router
            .oneshot(Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/config")
                .header(ORIGIN, "http://viewer.test")
                .header(ACCESS_CONTROL_REQUEST_METHOD, "PUT")
                .header(ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                .body(Body::empty())
                .unwrap())
            .await
            .unwrap();

        assert_eq!(response.headers()[ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }

    #[tokio::test]
    async fn viewer_send_should_time_out_when_transport_stalls() {
        let stalled = std::future::pending::<Result<(), ()>>();
        let result = complete_viewer_send(stalled, Duration::from_millis(1)).await;

        assert_eq!(result, Err(()));
    }

    #[tokio::test]
    async fn restart_required_put_should_report_fields_and_retain_generation() {
        let (router, _host) = test_service();
        let mut candidate = test_config();
        candidate.video.frame_rate = 30;
        let payload = serde_json::to_vec(&candidate).unwrap();

        let response = router
            .clone()
            .oneshot(Request::put("/api/config")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response_json(response).await;
        assert_eq!(body["error"]["restart_fields"], serde_json::json!(["video"]));

        let active = router
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response_json(active).await["generation"], 1);
    }

    #[tokio::test]
    async fn valid_put_should_publish_one_complete_next_generation() {
        let (router, mut host) = test_service();
        let mut candidate = test_config();
        candidate.selection.prefer_foreground = false;
        let payload = serde_json::to_vec(&candidate).unwrap();

        let accepted = router
            .oneshot(Request::put("/api/config")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(payload))
                .unwrap())
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        host.configurations.changed().await.unwrap();
        assert_eq!(host.configurations.borrow().generation, 2);
        assert!(!host
            .configurations
            .borrow()
            .config
            .config()
            .selection
            .prefer_foreground);
    }

    #[tokio::test]
    async fn viewer_count_should_follow_connection_lifetime() {
        let (router, _host) = test_service();
        let status_router = router.clone();
        let (address, server) = spawn_test_server(router).await;
        let (mut socket, _) = connect_async(format!("ws://{address}/api/video"))
            .await
            .unwrap();

        wait_for_viewer_count(&status_router, 1).await;
        socket.close(None).await.unwrap();
        wait_for_viewer_count(&status_router, 0).await;

        server.abort();
    }

    #[tokio::test]
    async fn reconnecting_viewer_should_require_a_distinct_fresh_keyframe() {
        let (router, host) = test_service();
        host.video
            .send(VideoEvent::CodecConfiguration(test_codec(1, 0xD9)))
            .await
            .unwrap();
        wait_for_decoder_cache(&router).await;
        let (address, server) = spawn_test_server(router).await;
        let fake_media = tokio::spawn(run_reconnecting_media(host));

        let (mut first_socket, _) = connect_async(format!("ws://{address}/api/video"))
            .await
            .unwrap();
        let first = receive_access_unit(&mut first_socket).await;
        first_socket.close(None).await.unwrap();

        let (mut second_socket, _) = connect_async(format!("ws://{address}/api/video"))
            .await
            .unwrap();
        let second = receive_access_unit(&mut second_socket).await;
        second_socket.close(None).await.unwrap();

        assert_eq!(
            [(first.timestamp_us(), first.is_keyframe()),
                (second.timestamp_us(), second.is_keyframe())],
            [(10, true), (20, true)]);
        let _host = fake_media.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn bound_instances_should_serve_distinct_configuration() {
        let (router_a, _host_a) = test_service();
        let mut config_b = test_config();
        config_b.selection.enabled[0] = "terminal".to_owned();
        let mut terminal = config_b.selection.profiles.remove("code").unwrap();
        terminal.include = vec!["WindowsTerminal.exe".to_owned()];
        config_b.selection.profiles.insert("terminal".to_owned(), terminal);
        let (mut service_b, _host_b) = InstanceService::new(
            config_b.validate().unwrap(),
            ChannelCapacities::default())
            .unwrap();
        let router_b = service_b.router().unwrap();
        let (address_a, server_a) = spawn_test_server(router_a).await;
        let (address_b, server_b) = spawn_test_server(router_b).await;

        let config_a = fetch_bound_config(address_a).await;
        let config_b = fetch_bound_config(address_b).await;

        assert_eq!(
            (&config_a["config"]["selection"]["enabled"][0],
                &config_b["config"]["selection"]["enabled"][0]),
            (&serde_json::json!("code"), &serde_json::json!("terminal")));
        server_a.abort();
        server_b.abort();
    }

    #[tokio::test]
    async fn stopping_one_bound_instance_should_leave_the_other_available() {
        let (router_a, _host_a) = test_service();
        let (router_b, _host_b) = test_service();
        let (_address_a, server_a) = spawn_test_server(router_a).await;
        let (address_b, server_b) = spawn_test_server(router_b).await;

        server_a.abort();
        let config_b = fetch_bound_config(address_b).await;

        assert_eq!(config_b["generation"], 1);
        server_b.abort();
    }

    #[tokio::test]
    async fn fake_host_should_gate_late_viewer_and_codec_changes_on_fresh_idrs() {
        let (router, host) = test_service();
        host.video
            .send(VideoEvent::CodecConfiguration(test_codec(1, 0xD9)))
            .await
            .unwrap();

        wait_for_decoder_cache(&router).await;
        let (address, server) = spawn_test_server(router).await;

        let fake_media = tokio::spawn(run_fake_media(host));

        let (mut socket, _) = connect_async(format!("ws://{address}/api/video"))
            .await
            .unwrap();
        let control = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let tungstenite::Message::Text(control) = control else {
            panic!("viewer should receive render configuration before video");
        };
        let control: serde_json::Value = serde_json::from_str(control.as_ref()).unwrap();
        assert_eq!(control["type"], "render_configuration");

        let mut codec_generation = None;
        let mut keyframe_timestamps = Vec::new();
        for _ in 0..8 {
            let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let tungstenite::Message::Binary(data) = message {
                match decode_message(&data).unwrap() {
                    VideoMessage::CodecConfiguration(config) => {
                        codec_generation = Some(config.generation());
                    }
                    VideoMessage::AccessUnit(unit) => {
                        assert!(unit.is_keyframe());
                        assert_eq!(codec_generation, Some(unit.codec_generation()));
                        keyframe_timestamps.push(unit.timestamp_us());
                        if keyframe_timestamps.len() == 2 {
                            break;
                        }
                    }
                }
            }
        }
        assert_eq!(keyframe_timestamps, [2, 4]);

        socket.close(None).await.unwrap();
        let _host = fake_media.await.unwrap();
        server.abort();
    }
}
