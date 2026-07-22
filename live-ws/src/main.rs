//! `live-ws` — stdin-to-WebSocket relay for Nekomaru LiveUI.
//!
//! Reads `live-protocol` framed messages from stdin and forwards each as a
//! WS binary message to the server.  Auto-reconnects with exponential backoff.
//!
//! ## Modes
//!
//! - **(default)**: framed forwarding.  Discards messages during disconnect.
//! - **`--mode video`**: additionally caches the last `CodecParams` and last
//!   keyframe.  On reconnect, replays cached messages before resuming.
//! - **`--mode audio`**: caches the last `AudioConfig` message.  On reconnect,
//!   replays it before resuming (no keyframe concept for raw PCM).
//!
//! ## Usage
//!
//! ```text
//! live-encoder ... | live-ws --mode video --server ws://machineA:3000/internal/streams/main
//! live-kpm        | live-ws --server ws://machineA:3000/internal/kpm
//! live-audio ...  | live-ws --mode audio --server ws://machineA:3000/internal/audio
//! ```

use live_protocol::{HEADER_SIZE, MessageType, flags};

use clap::Parser;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite;

use std::time::Duration;

// ── CLI ─────────────────────────────────────────────────────────────────────

/// Stdin-to-WebSocket relay.
#[derive(Parser)]
#[command(name = "live-ws")]
struct CliArgs {
    /// WebSocket server URL to connect to.
    #[arg(long)]
    server: String,

    /// Relay mode.  `video` enables codec params + keyframe caching;
    /// `audio` enables audio config caching for reconnect replay.
    #[arg(long, value_parser = parse_mode, default_value = "default")]
    mode: RelayMode,

    /// Stream ID tag for log output.
    #[arg(long)]
    stream_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    /// Dumb framed forwarding with auto-reconnect.
    Default,
    /// Video mode: caches last CodecParams + last keyframe for reconnect replay.
    Video,
    /// Audio mode: caches last AudioConfig for reconnect replay.
    Audio,
}

fn parse_mode(s: &str) -> Result<RelayMode, String> {
    match s {
        "default" => Ok(RelayMode::Default),
        "video" => Ok(RelayMode::Video),
        "audio" => Ok(RelayMode::Audio),
        other => Err(format!("unknown mode '{other}' (expected 'default', 'video', or 'audio')")),
    }
}

// ── Constants ───────────────────────────────────────────────────────────────

const INITIAL_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 4000;

/// Channel capacity — large enough to absorb burst frames while the WS
/// writer is reconnecting.  At 60fps each frame is ~10-50KB; 120 frames
/// is ~2-6MB of buffered data.
const CHANNEL_CAPACITY: usize = 120;

// ── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::formatted_builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let args = CliArgs::parse();

    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    // Stdin reader runs on a blocking thread (stdin is synchronous).
    let stdin_handle = tokio::task::spawn_blocking(move || stdin_reader(&tx));

    // WS writer runs on the async runtime.
    let ws_handle = tokio::spawn(ws_writer(rx, args.server, args.mode));

    // Wait for either task to finish.  Typically stdin_reader ends first
    // (when the producer process exits / pipe closes).
    tokio::select! {
        result = stdin_handle => {
            if let Err(e) = result {
                log::error!("stdin reader panicked: {e}");
            }
        }
        result = ws_handle => {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("ws writer error: {e}"),
                Err(e) => log::error!("ws writer panicked: {e}"),
            }
        }
    }

    Ok(())
}

// ── Stdin Reader ────────────────────────────────────────────────────────────

/// Blocking stdin reader.  Reads complete `live-protocol` framed messages
/// and sends them to the channel.  Exits on EOF or broken channel.
fn stdin_reader(tx: &mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin().lock();
    // Consecutive messages dropped because the channel was full.
    // Logs once on the first drop, then reports the total on recovery.
    let mut dropped: usize = 0;

    loop {
        match live_protocol::read_message_raw(&mut stdin) {
            Ok(Some(raw)) => {
                // Non-blocking send: if the channel is full (WS writer can't
                // keep up), we drop the message.  This is intentional — the
                // alternative is backpressuring the producer, which would stall
                // the encoder.
                if tx.try_send(raw).is_err() {
                    // Channel full or closed.  If closed, we'll exit on the
                    // next iteration when try_send fails again.
                    dropped += 1;
                    if dropped == 1 {
                        log::warn!("channel full or closed, dropping messages");
                    }
                } else if dropped > 0 {
                    log::warn!("dropped {dropped} messages while channel was full");
                    dropped = 0;
                } else {
                    // Successfully sent message.
                }
            }
            Ok(None) => {
                // Clean EOF — producer exited.
                log::info!("stdin EOF, shutting down");
                break;
            }
            Err(e) => {
                log::error!("stdin read error: {e}");
                break;
            }
        }
    }
}

// ── WS Writer ───────────────────────────────────────────────────────────────

/// Replay cached CodecParams and keyframe on reconnect (video mode only).
/// Returns `Err` if a send fails (caller should reconnect).
async fn replay_cached<S>(
    ws_write: &mut futures_util::stream::SplitSink<S, tungstenite::Message>,
    cached_codec_params: Option<&Vec<u8>>,
    cached_keyframe: Option<&Vec<u8>>,
) -> Result<(), ()>
where
    S: futures_util::Sink<tungstenite::Message> + Unpin,
{
    if let Some(params) = cached_codec_params {
        log::info!("replaying cached CodecParams ({}B)", params.len());
        send_binary(ws_write, params.clone()).await?;
    }
    if let Some(keyframe) = cached_keyframe {
        log::info!("replaying cached keyframe ({}B)", keyframe.len());
        send_binary(ws_write, keyframe.clone()).await?;
    }
    Ok(())
}

/// Update the video-mode caches from an incoming raw message.
fn update_video_cache(
    raw: &[u8],
    cached_codec_params: &mut Option<Vec<u8>>,
    cached_keyframe: &mut Option<Vec<u8>>,
) {
    let Some(&[msg_type, msg_flags, ..]) = raw.first_chunk::<HEADER_SIZE>() else { return; };

    if msg_type == MessageType::CodecParams as u8 {
        *cached_codec_params = Some(raw.to_vec());
    } else if msg_type == MessageType::Frame as u8
        && (msg_flags & flags::IS_KEYFRAME) != 0 {
        *cached_keyframe = Some(raw.to_vec());
    } else {
        // Not a cacheable message type.
    }
}

/// Update the audio-mode cache from an incoming raw message.
fn update_audio_cache(
    raw: &[u8],
    cached_audio_config: &mut Option<Vec<u8>>,
) {
    let Some(&[msg_type, ..]) = raw.first_chunk::<HEADER_SIZE>() else { return };

    if msg_type == MessageType::AudioConfig as u8 {
        *cached_audio_config = Some(raw.to_vec());
    }
}

/// Async WS writer.  Connects to the server, consumes messages from the
/// channel, and sends them as WS binary messages.  Reconnects on failure.
///
/// In video mode, caches the last `CodecParams` and last keyframe for
/// replay on reconnect.  In audio mode, caches the last `AudioConfig`.
async fn ws_writer(
    mut rx: mpsc::Receiver<Vec<u8>>,
    server_url: String,
    mode: RelayMode,
) -> anyhow::Result<()> {
    let mut backoff = INITIAL_BACKOFF_MS;

    // Mode-specific caches for reconnect replay.
    let mut cached_codec_params: Option<Vec<u8>> = None;
    let mut cached_keyframe: Option<Vec<u8>> = None;
    let mut cached_audio_config: Option<Vec<u8>> = None;

    loop {
        // ── Connect ─────────────────────────────────────────────────────
        log::info!("connecting to {server_url}");
        let ws_stream = match tokio_tungstenite::connect_async(&server_url).await {
            Ok((stream, _response)) => {
                log::info!("connected to {server_url}");
                backoff = INITIAL_BACKOFF_MS;
                stream
            }
            Err(e) => {
                log::warn!("connection failed: {e}, retrying in {backoff}ms");
                tokio::time::sleep(Duration::from_millis(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF_MS);
                continue;
            }
        };

        let (mut ws_write, _ws_read) = futures_stream_split(ws_stream);

        // ── Replay cached messages on reconnect ─────────────────────
        let replay_ok = match mode {
            RelayMode::Video => replay_cached(
                &mut ws_write,
                cached_codec_params.as_ref(),
                cached_keyframe.as_ref()).await.is_ok(),
            RelayMode::Audio => replay_cached(
                &mut ws_write,
                cached_audio_config.as_ref(),
                None).await.is_ok(),
            RelayMode::Default => true,
        };
        if !replay_ok { continue; } // reconnect

        // ── Forward loop ────────────────────────────────────────────
        loop {
            let Some(raw) = rx.recv().await else {
                // Channel closed — producer exited.
                log::info!("channel closed, shutting down ws writer");
                return Ok(());
            };

            match mode {
                RelayMode::Video => update_video_cache(
                    &raw, &mut cached_codec_params, &mut cached_keyframe),
                RelayMode::Audio => update_audio_cache(
                    &raw, &mut cached_audio_config),
                RelayMode::Default => {}
            }

            if send_binary(&mut ws_write, raw).await.is_err() {
                log::warn!("ws send failed, reconnecting");
                break; // break to outer reconnect loop
            }
        }

        // Drain channel of stale messages before reconnecting.
        let mut drained = 0usize;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            log::info!("drained {drained} stale messages");
        }

        log::info!("reconnecting in {backoff}ms");
        tokio::time::sleep(Duration::from_millis(backoff)).await;
        backoff = (backoff * 2).min(MAX_BACKOFF_MS);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a WS stream into write/read halves.
///
/// `tokio-tungstenite` returns a combined stream+sink.  We only need the
/// write half for sending; the read half is dropped (the server doesn't
/// send anything to the encoder input WS).
fn futures_stream_split<S>(stream: S) -> (
    futures_util::stream::SplitSink<S, tungstenite::Message>,
    futures_util::stream::SplitStream<S>,
) where
    S: futures_util::Stream + futures_util::Sink<tungstenite::Message>,
{
    use futures_util::StreamExt as _;
    stream.split()
}

/// Send a binary message, returning `Err` on failure (triggering reconnect).
async fn send_binary<S>(
    sink: &mut futures_util::stream::SplitSink<S, tungstenite::Message>,
    data: Vec<u8>,
) -> Result<(), ()>
where
    S: futures_util::Sink<tungstenite::Message> + Unpin,
{
    use futures_util::SinkExt as _;
    sink.send(tungstenite::Message::Binary(data.into()))
        .await
        .map_err(|_err| ())
}
