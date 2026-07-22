//! Shared binary framing protocol for Nekomaru LiveUI.
//!
//! All M4 components use this crate for IPC: producers (`live-encoder`,
//! `live-kpm`) write framed messages to stdout, and `live-ws` reads them
//! from stdin for WebSocket relay.  The TS server and frontend consume the
//! same wire format.
//!
//! ## Frame Header (8 bytes, 4-byte aligned)
//!
//! ```text
//! Offset  Field            Size
//! 0       message_type     u8
//! 1       flags            u8
//! 2       reserved         u16 (zero)
//! 4       payload_length   u32 LE
//! ```

pub mod audio;
pub mod avcc;
pub mod video;

use std::io;
use std::io::{Read, Write};

// ── Frame Header ────────────────────────────────────────────────────────────

/// Size of the binary frame header in bytes.
pub const HEADER_SIZE: usize = 8;

/// Message type discriminant (byte 0 of every frame header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    /// Video codec initialization parameters (SPS, PPS, resolution).
    CodecParams = 0x01,
    /// One encoded video frame (AVCC payload).
    Frame = 0x02,
    /// KPM (keystrokes per minute) update.
    KpmUpdate = 0x10,
    /// Audio format parameters (sample rate, channels, bit depth).
    /// Sent once at capture start.
    AudioConfig = 0x11,
    /// One chunk of raw PCM audio data with a wall-clock timestamp.
    AudioChunk = 0x12,
    /// Non-fatal error description (UTF-8).
    Error = 0xFF,
}

impl MessageType {
    /// Parse from a raw byte.  Returns `None` for unknown types.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::CodecParams),
            0x02 => Some(Self::Frame),
            0x10 => Some(Self::KpmUpdate),
            0x11 => Some(Self::AudioConfig),
            0x12 => Some(Self::AudioChunk),
            0xFF => Some(Self::Error),
            _ => None,
        }
    }
}

/// Header flag bits (byte 1 of every frame header).
pub mod flags {
    /// Set on video frames that contain an IDR keyframe.
    pub const IS_KEYFRAME: u8 = 1 << 0;
}

/// Parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub message_type: u8,
    pub flags: u8,
    pub payload_length: u32,
}

// ── Write ───────────────────────────────────────────────────────────────────

/// Write a complete framed message (header + payload) to a writer.
pub fn write_message(
    w: &mut impl Write,
    message_type: MessageType,
    flags: u8,
    payload: &[u8],
) -> io::Result<()> {
    // Header: [u8 type][u8 flags][u16 reserved=0][u32 LE payload_length]
    let mut header = [0u8; HEADER_SIZE];
    header[0] = message_type as u8;
    header[1] = flags;
    // header[2..4] = reserved (already zero)
    header[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());

    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

// ── Read ────────────────────────────────────────────────────────────────────

/// Read the next frame header from a byte stream.
///
/// Returns `Ok(None)` on clean EOF (stream ended between messages).
/// Returns `Err` on unexpected EOF mid-header.
pub fn read_header(r: &mut impl Read) -> io::Result<Option<FrameHeader>> {
    let mut buf = [0u8; HEADER_SIZE];
    match r.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    Ok(Some(FrameHeader {
        message_type: buf[0],
        flags: buf[1],
        payload_length: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
    }))
}

/// Read a complete framed message (header + payload).
///
/// Returns `Ok(None)` on clean EOF between messages.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<(FrameHeader, Vec<u8>)>> {
    let Some(header) = read_header(r)? else { return Ok(None) };

    let mut payload = vec![0u8; header.payload_length as usize];
    r.read_exact(&mut payload)?;

    Ok(Some((header, payload)))
}

/// Read a complete framed message and return the raw bytes (header + payload)
/// as a single contiguous buffer.  Used by `live-ws` which forwards the
/// entire message without modification.
///
/// Returns `Ok(None)` on clean EOF between messages.
pub fn read_message_raw(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header_buf = [0u8; HEADER_SIZE];
    match r.read_exact(&mut header_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }

    let payload_len = u32::from_le_bytes(
        [header_buf[4], header_buf[5], header_buf[6], header_buf[7]]) as usize;

    let mut buf = Vec::with_capacity(HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header_buf);
    buf.resize(HEADER_SIZE + payload_len, 0);
    r.read_exact(&mut buf[HEADER_SIZE..])?;

    Ok(Some(buf))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_roundtrip() {
        let mut buf = Vec::new();
        write_message(
            &mut buf, MessageType::Frame, flags::IS_KEYFRAME, &[0xAA, 0xBB])
            .unwrap();

        assert_eq!(buf.len(), HEADER_SIZE + 2);

        let mut cursor = Cursor::new(&buf);
        let header = read_header(&mut cursor).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::Frame as u8);
        assert_eq!(header.flags, flags::IS_KEYFRAME);
        assert_eq!(header.payload_length, 2);
    }

    #[test]
    fn message_roundtrip() {
        let payload = vec![1, 2, 3, 4, 5];
        let mut buf = Vec::new();
        write_message(&mut buf, MessageType::KpmUpdate, 0, &payload).unwrap();

        let mut cursor = Cursor::new(&buf);
        let (header, data) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::KpmUpdate as u8);
        assert_eq!(header.flags, 0);
        assert_eq!(data, payload);
    }

    #[test]
    fn raw_message_roundtrip() {
        let payload = vec![0xDE, 0xAD];
        let mut buf = Vec::new();
        write_message(&mut buf, MessageType::CodecParams, 0, &payload).unwrap();

        let mut cursor = Cursor::new(&buf);
        let raw = read_message_raw(&mut cursor).unwrap().unwrap();
        assert_eq!(raw.len(), HEADER_SIZE + 2);
        assert_eq!(raw[0], MessageType::CodecParams as u8);
        assert_eq!(&raw[HEADER_SIZE..], &[0xDE, 0xAD]);
    }

    #[test]
    fn eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_header(&mut cursor).unwrap().is_none());
        assert!(read_message(&mut Cursor::new(Vec::<u8>::new())).unwrap().is_none());
        assert!(read_message_raw(&mut Cursor::new(Vec::<u8>::new())).unwrap().is_none());
    }

    #[test]
    fn sequential_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, MessageType::CodecParams, 0, &[1]).unwrap();
        write_message(&mut buf, MessageType::Frame, flags::IS_KEYFRAME, &[2, 3]).unwrap();
        write_message(&mut buf, MessageType::Error, 0, b"oops").unwrap();

        let mut cursor = Cursor::new(&buf);

        let (h, d) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(h.message_type, MessageType::CodecParams as u8);
        assert_eq!(d, [1]);

        let (h, d) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(h.message_type, MessageType::Frame as u8);
        assert_ne!(h.flags & flags::IS_KEYFRAME, 0);
        assert_eq!(d, [2, 3]);

        let (h, d) = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(h.message_type, MessageType::Error as u8);
        assert_eq!(d, b"oops");

        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn message_type_from_byte() {
        assert_eq!(MessageType::from_byte(0x01), Some(MessageType::CodecParams));
        assert_eq!(MessageType::from_byte(0x02), Some(MessageType::Frame));
        assert_eq!(MessageType::from_byte(0x10), Some(MessageType::KpmUpdate));
        assert_eq!(MessageType::from_byte(0x11), Some(MessageType::AudioConfig));
        assert_eq!(MessageType::from_byte(0x12), Some(MessageType::AudioChunk));
        assert_eq!(MessageType::from_byte(0xFF), Some(MessageType::Error));
        assert_eq!(MessageType::from_byte(0x99), None);
    }

    #[test]
    fn header_is_4byte_aligned() {
        assert_eq!(HEADER_SIZE % 4, 0);
    }
}
