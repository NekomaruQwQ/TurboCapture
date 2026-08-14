//! Checked private video framing and WebCodecs initialization helpers.

use std::sync::Arc;

/// Size of the aligned private video message header.
pub const HEADER_SIZE: usize = 8;

/// Keyframe bit carried by access-unit headers.
const FLAG_KEYFRAME: u8 = 1;

/// Maximum complete payload accepted by the private parser.
const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Fixed generation and timestamp prefix in every access-unit payload.
const ACCESS_UNIT_PREFIX_SIZE: usize = 16;

/// Largest AVCC body whose typed access unit is always serializable.
const MAX_AVCC_SIZE: usize = MAX_PAYLOAD_SIZE - ACCESS_UNIT_PREFIX_SIZE;

/// Decoder media type and H.264 parameter sets for one codec generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfiguration {
    generation: u64,
    width: u16,
    height: u16,
    profile: u8,
    compatibility: u8,
    level: u8,
    sps: Arc<[u8]>,
    pps: Arc<[u8]>,
}

impl CodecConfiguration {
    /// Constructs one checked decoder configuration.
    ///
    /// # Errors
    ///
    /// Returns [`VideoProtocolError`] when a dimension is zero, an SPS cannot
    /// describe a WebCodecs codec string, or a parameter set exceeds `u16`.
    pub fn new(
        generation: u64,
        width: u32,
        height: u32,
        sps: impl Into<Arc<[u8]>>,
        pps: impl Into<Arc<[u8]>>) -> Result<Self, VideoProtocolError> {
        let width = u16::try_from(width)
            .ok()
            .filter(|width| *width != 0)
            .ok_or(VideoProtocolError::InvalidDimension { name: "width", value: width })?;
        let height = u16::try_from(height)
            .ok()
            .filter(|height| *height != 0)
            .ok_or(VideoProtocolError::InvalidDimension { name: "height", value: height })?;
        let sps = sps.into();
        let pps = pps.into();
        let sequence_bytes = strip_start_code(&sps);
        let picture_bytes = strip_start_code(&pps);
        let &[_, profile, compatibility, level, ..] = sequence_bytes else {
            return Err(VideoProtocolError::SpsTooShort { length: sequence_bytes.len() });
        };
        validate_parameter_set("sps", sequence_bytes)?;
        validate_parameter_set("pps", picture_bytes)?;

        Ok(Self {
            generation,
            width,
            height,
            profile,
            compatibility,
            level,
            sps: Arc::from(sequence_bytes),
            pps: Arc::from(picture_bytes),
        })
    }

    /// Returns the codec generation carried by matching access units.
    #[inline]
    pub const fn generation(&self) -> u64 { self.generation }

    /// Returns the encoded width in pixels.
    #[inline]
    pub const fn width(&self) -> u16 { self.width }

    /// Returns the encoded height in pixels.
    #[inline]
    pub const fn height(&self) -> u16 { self.height }

    /// Returns the raw SPS without an Annex B start code.
    #[inline]
    pub fn sps(&self) -> &[u8] { &self.sps }

    /// Returns the raw PPS without an Annex B start code.
    #[inline]
    pub fn pps(&self) -> &[u8] { &self.pps }

    /// Builds the `avc1.PPCCLL` identifier consumed by WebCodecs.
    pub fn codec_string(&self) -> String {
        format!(
            "avc1.{:02x}{:02x}{:02x}",
            self.profile,
            self.compatibility,
            self.level)
    }

    /// Builds an ISO 14496-15 AVCDecoderConfigurationRecord.
    pub fn avcc_description(&self) -> Vec<u8> {
        let mut description = Vec::with_capacity(11 + self.sps.len() + self.pps.len());
        description.extend_from_slice(&[
            1,
            self.profile,
            self.compatibility,
            self.level,
            0xFF,
            0xE1]);
        description.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        description.extend_from_slice(&self.sps);
        description.push(1);
        description.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        description.extend_from_slice(&self.pps);
        description
    }
}

/// One AVCC-formatted H.264 access unit owned by the async service boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    codec_generation: u64,
    timestamp_us: u64,
    keyframe: bool,
    avcc: Arc<[u8]>,
}

impl AccessUnit {
    /// Constructs one access unit after validating all AVCC length prefixes.
    ///
    /// # Errors
    ///
    /// Returns [`VideoProtocolError`] for an empty, zero-length, truncated, or
    /// oversized AVCC payload.
    pub fn new(
        codec_generation: u64,
        timestamp_us: u64,
        keyframe: bool,
        avcc: impl Into<Arc<[u8]>>) -> Result<Self, VideoProtocolError> {
        let avcc = avcc.into();
        validate_avcc(&avcc)?;
        Ok(Self { codec_generation, timestamp_us, keyframe, avcc })
    }

    /// Returns the codec generation required to decode this access unit.
    #[inline]
    pub const fn codec_generation(&self) -> u64 { self.codec_generation }

    /// Returns the microsecond presentation timestamp.
    #[inline]
    pub const fn timestamp_us(&self) -> u64 { self.timestamp_us }

    /// Returns whether this access unit is an independently decodable IDR.
    #[inline]
    pub const fn is_keyframe(&self) -> bool { self.keyframe }

    /// Returns the directly WebCodecs-compatible AVCC bytes.
    #[inline]
    pub fn avcc(&self) -> &[u8] { &self.avcc }
}

/// Typed media event sent by the native host through the bounded output channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoEvent {
    /// Decoder configuration that begins or replaces a codec generation.
    CodecConfiguration(CodecConfiguration),
    /// Encoded picture associated with one codec generation.
    AccessUnit(AccessUnit),
}

/// Complete private video message decoded from one WebSocket binary message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoMessage {
    /// Decoder configuration that begins or replaces a codec generation.
    CodecConfiguration(CodecConfiguration),
    /// Encoded picture associated with one codec generation.
    AccessUnit(AccessUnit),
}

impl From<VideoEvent> for VideoMessage {
    fn from(event: VideoEvent) -> Self {
        match event {
            VideoEvent::CodecConfiguration(config) => Self::CodecConfiguration(config),
            VideoEvent::AccessUnit(unit) => Self::AccessUnit(unit),
        }
    }
}

/// Serialize one typed event into a complete private WebSocket message.
///
/// # Errors
///
/// Returns [`VideoProtocolError`] if the complete payload cannot be represented
/// by the four-byte header length.
pub fn encode_event(event: &VideoEvent) -> Result<Vec<u8>, VideoProtocolError> {
    match event {
        &VideoEvent::CodecConfiguration(ref config) => encode_codec_configuration(config),
        &VideoEvent::AccessUnit(ref unit) => encode_access_unit(unit),
    }
}

/// Parse one complete private WebSocket video message.
///
/// # Errors
///
/// Returns [`VideoProtocolError`] for malformed headers, unknown types or
/// flags, length mismatches, malformed payloads, and invalid AVCC data.
pub fn decode_message(data: &[u8]) -> Result<VideoMessage, VideoProtocolError> {
    if data.len() < HEADER_SIZE {
        return Err(VideoProtocolError::TruncatedHeader { length: data.len() });
    }
    let Some(header) = data
        .get(..HEADER_SIZE)
        .and_then(|header| <&[u8; HEADER_SIZE]>::try_from(header).ok())
    else {
        return Err(VideoProtocolError::TruncatedHeader { length: data.len() });
    };
    let [message_type, flags, reserved_low, reserved_high, l0, l1, l2, l3] = *header;
    if reserved_low != 0 || reserved_high != 0 {
        return Err(VideoProtocolError::ReservedHeaderBits);
    }
    let payload_length = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
    if payload_length > MAX_PAYLOAD_SIZE {
        return Err(VideoProtocolError::PayloadTooLarge {
            length: payload_length,
            maximum: MAX_PAYLOAD_SIZE,
        });
    }
    let actual = data.len() - HEADER_SIZE;
    if payload_length != actual {
        return Err(VideoProtocolError::PayloadLengthMismatch {
            declared: payload_length,
            actual,
        });
    }
    let payload = &data[HEADER_SIZE..];
    match message_type {
        1 => {
            if flags != 0 {
                return Err(VideoProtocolError::UnknownFlags { message_type: 1, flags });
            }
            decode_codec_configuration(payload).map(VideoMessage::CodecConfiguration)
        }
        2 => {
            if flags & !FLAG_KEYFRAME != 0 {
                return Err(VideoProtocolError::UnknownFlags { message_type: 2, flags });
            }
            decode_access_unit(payload, flags & FLAG_KEYFRAME != 0)
                .map(VideoMessage::AccessUnit)
        }
        message_type => Err(VideoProtocolError::UnknownMessageType { message_type }),
    }
}

/// Convert Annex B NAL units into one checked AVCC access-unit payload.
///
/// # Errors
///
/// Returns [`VideoProtocolError`] if no NAL units are supplied, a stripped NAL
/// is empty, a length exceeds `u32`, or total-size arithmetic overflows.
pub fn serialize_avcc(
    nal_units: &[impl AsRef<[u8]>]) -> Result<Vec<u8>, VideoProtocolError> {
    if nal_units.is_empty() {
        return Err(VideoProtocolError::EmptyAvcc);
    }
    let mut total = 0usize;
    let mut stripped = Vec::with_capacity(nal_units.len());
    for nal in nal_units {
        let nal = strip_start_code(nal.as_ref());
        if nal.is_empty() {
            return Err(VideoProtocolError::EmptyNalUnit);
        }
        u32::try_from(nal.len())
            .map_err(|_length_error| VideoProtocolError::NalUnitTooLarge {
                length: nal.len(),
            })?;
        total = total
            .checked_add(4)
            .and_then(|total| total.checked_add(nal.len()))
            .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
        stripped.push(nal);
    }
    if total > MAX_AVCC_SIZE {
        return Err(VideoProtocolError::PayloadTooLarge {
            length: total,
            maximum: MAX_AVCC_SIZE,
        });
    }

    let mut avcc = Vec::with_capacity(total);
    for nal in stripped {
        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        avcc.extend_from_slice(nal);
    }
    Ok(avcc)
}

/// Typed failures produced by the private video contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VideoProtocolError {
    /// A coded dimension is zero or cannot be represented by the protocol.
    #[error("invalid codec {name} {value}; expected 1..={}", u16::MAX)]
    InvalidDimension {
        /// Dimension name.
        name: &'static str,
        /// Rejected caller value.
        value: u32,
    },
    /// The SPS cannot supply profile, compatibility, and level bytes.
    #[error("SPS is too short: {length} bytes")]
    SpsTooShort {
        /// Stripped SPS byte count.
        length: usize,
    },
    /// An SPS or PPS exceeds its two-byte private-protocol length.
    #[error("{kind} parameter set is too large: {length} bytes")]
    ParameterSetTooLarge {
        /// Parameter-set kind.
        kind: &'static str,
        /// Rejected byte count.
        length: usize,
    },
    /// No NAL units were supplied for an access unit.
    #[error("AVCC access unit is empty")]
    EmptyAvcc,
    /// A supplied NAL becomes empty after stripping its Annex B start code.
    #[error("AVCC contains an empty NAL unit")]
    EmptyNalUnit,
    /// A NAL unit exceeds the four-byte AVCC length field.
    #[error("NAL unit is too large: {length} bytes")]
    NalUnitTooLarge {
        /// Rejected NAL byte count.
        length: usize,
    },
    /// Checked payload-size arithmetic overflowed `usize`.
    #[error("video payload size overflowed")]
    PayloadSizeOverflow,
    /// A payload exceeds the bounded private-message limit.
    #[error("video payload is {length} bytes; maximum is {maximum}")]
    PayloadTooLarge {
        /// Rejected payload size.
        length: usize,
        /// Maximum accepted payload size.
        maximum: usize,
    },
    /// Fewer than eight header bytes were supplied.
    #[error("video header is truncated: {length} bytes")]
    TruncatedHeader {
        /// Available byte count.
        length: usize,
    },
    /// Reserved header bytes were non-zero.
    #[error("reserved video header bits are non-zero")]
    ReservedHeaderBits,
    /// The header payload length does not match the WebSocket message.
    #[error("declared payload length {declared} does not match actual length {actual}")]
    PayloadLengthMismatch {
        /// Header-declared byte count.
        declared: usize,
        /// Actual byte count after the header.
        actual: usize,
    },
    /// The message type is not part of the M0 private video contract.
    #[error("unknown video message type {message_type:#04x}")]
    UnknownMessageType {
        /// Rejected type byte.
        message_type: u8,
    },
    /// Flags outside the message type's allowed mask were set.
    #[error("unknown flags {flags:#04x} for video message type {message_type:#04x}")]
    UnknownFlags {
        /// Message type whose flag byte was rejected.
        message_type: u8,
        /// Rejected flag byte.
        flags: u8,
    },
    /// A typed payload ended before all fixed or variable fields were present.
    #[error("{message} payload is truncated")]
    TruncatedPayload {
        /// Payload kind used in diagnostics.
        message: &'static str,
    },
    /// Bytes remained after parsing the exact codec payload.
    #[error("codec configuration contains {trailing} trailing bytes")]
    TrailingCodecBytes {
        /// Unexpected byte count.
        trailing: usize,
    },
    /// An AVCC length prefix extends beyond the supplied access-unit payload.
    #[error("AVCC NAL declares {declared} bytes with only {remaining} remaining")]
    TruncatedNalUnit {
        /// Length prefix value.
        declared: usize,
        /// Available bytes after the prefix.
        remaining: usize,
    },
}

/// Remove either standard Annex B start-code form without copying.
fn strip_start_code(data: &[u8]) -> &[u8] {
    if data.get(..4) == Some(&[0, 0, 0, 1]) {
        &data[4..]
    } else if data.get(..3) == Some(&[0, 0, 1]) {
        &data[3..]
    } else {
        data
    }
}

/// Ensure a parameter set can be represented by the private codec payload.
fn validate_parameter_set(
    kind: &'static str,
    parameter_set: &[u8]) -> Result<(), VideoProtocolError> {
    u16::try_from(parameter_set.len())
        .map(|_| ())
        .map_err(|_length_error| VideoProtocolError::ParameterSetTooLarge {
            kind,
            length: parameter_set.len(),
        })
}

/// Serialize one decoder configuration payload and aligned header.
fn encode_codec_configuration(
    config: &CodecConfiguration) -> Result<Vec<u8>, VideoProtocolError> {
    let payload_length = 8usize
        .checked_add(2 + 2 + 2 + config.sps.len() + 2 + config.pps.len())
        .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
    let mut message = message_with_header(1, 0, payload_length)?;
    message.extend_from_slice(&config.generation.to_le_bytes());
    message.extend_from_slice(&config.width.to_le_bytes());
    message.extend_from_slice(&config.height.to_le_bytes());
    message.extend_from_slice(&(config.sps.len() as u16).to_le_bytes());
    message.extend_from_slice(&config.sps);
    message.extend_from_slice(&(config.pps.len() as u16).to_le_bytes());
    message.extend_from_slice(&config.pps);
    Ok(message)
}

/// Serialize one encoded access unit payload and aligned header.
fn encode_access_unit(unit: &AccessUnit) -> Result<Vec<u8>, VideoProtocolError> {
    let payload_length = ACCESS_UNIT_PREFIX_SIZE
        .checked_add(unit.avcc.len())
        .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
    let flags = if unit.keyframe { FLAG_KEYFRAME } else { 0 };
    let mut message = message_with_header(2, flags, payload_length)?;
    message.extend_from_slice(&unit.codec_generation.to_le_bytes());
    message.extend_from_slice(&unit.timestamp_us.to_le_bytes());
    message.extend_from_slice(&unit.avcc);
    Ok(message)
}

/// Allocate one message and append its checked fixed header.
fn message_with_header(
    message_type: u8,
    flags: u8,
    payload_length: usize) -> Result<Vec<u8>, VideoProtocolError> {
    if payload_length > MAX_PAYLOAD_SIZE {
        return Err(VideoProtocolError::PayloadTooLarge {
            length: payload_length,
            maximum: MAX_PAYLOAD_SIZE,
        });
    }
    let payload_length_u32 = u32::try_from(payload_length)
        .map_err(|_length_error| VideoProtocolError::PayloadTooLarge {
            length: payload_length,
            maximum: u32::MAX as usize,
        })?;
    let capacity = HEADER_SIZE
        .checked_add(payload_length)
        .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
    let mut message = Vec::with_capacity(capacity);
    message.extend_from_slice(&[message_type, flags, 0, 0]);
    message.extend_from_slice(&payload_length_u32.to_le_bytes());
    Ok(message)
}

/// Decode the exact codec-configuration payload layout.
fn decode_codec_configuration(
    payload: &[u8]) -> Result<CodecConfiguration, VideoProtocolError> {
    let mut reader = PayloadReader::new(payload, "codec configuration");
    let generation = reader.read_u64()?;
    let width = reader.read_u16()?;
    let height = reader.read_u16()?;
    let sps_length = reader.read_u16()? as usize;
    let sps = reader.read_bytes(sps_length)?;
    let pps_length = reader.read_u16()? as usize;
    let pps = reader.read_bytes(pps_length)?;
    if reader.remaining() != 0 {
        return Err(VideoProtocolError::TrailingCodecBytes {
            trailing: reader.remaining(),
        });
    }
    CodecConfiguration::new(generation, width.into(), height.into(), sps, pps)
}

/// Decode the exact access-unit payload layout and validate AVCC framing.
fn decode_access_unit(
    payload: &[u8],
    keyframe: bool) -> Result<AccessUnit, VideoProtocolError> {
    let mut reader = PayloadReader::new(payload, "access unit");
    let generation = reader.read_u64()?;
    let timestamp_us = reader.read_u64()?;
    let avcc = reader.read_bytes(reader.remaining())?;
    AccessUnit::new(generation, timestamp_us, keyframe, avcc)
}

/// Validate that an AVCC payload is an exact non-empty sequence of NAL units.
fn validate_avcc(avcc: &[u8]) -> Result<(), VideoProtocolError> {
    if avcc.is_empty() {
        return Err(VideoProtocolError::EmptyAvcc);
    }
    if avcc.len() > MAX_AVCC_SIZE {
        return Err(VideoProtocolError::PayloadTooLarge {
            length: avcc.len(),
            maximum: MAX_AVCC_SIZE,
        });
    }
    let mut position = 0usize;
    while position < avcc.len() {
        let prefix_end = position
            .checked_add(4)
            .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
        if prefix_end > avcc.len() {
            return Err(VideoProtocolError::TruncatedNalUnit {
                declared: 4,
                remaining: avcc.len() - position,
            });
        }
        let length = u32::from_be_bytes(
            avcc[position..prefix_end]
                .try_into()
                .map_err(|_length_error| VideoProtocolError::TruncatedNalUnit {
                    declared: 4,
                    remaining: avcc.len() - position,
                })?) as usize;
        if length == 0 {
            return Err(VideoProtocolError::EmptyNalUnit);
        }
        position = prefix_end;
        let nal_end = position
            .checked_add(length)
            .ok_or(VideoProtocolError::PayloadSizeOverflow)?;
        if nal_end > avcc.len() {
            return Err(VideoProtocolError::TruncatedNalUnit {
                declared: length,
                remaining: avcc.len() - position,
            });
        }
        position = nal_end;
    }
    Ok(())
}

/// Bounds-checked little-endian reader for one already-bounded payload.
struct PayloadReader<'a> {
    data: &'a [u8],
    position: usize,
    message: &'static str,
}

impl<'a> PayloadReader<'a> {
    /// Construct a reader whose failures name the payload kind.
    const fn new(data: &'a [u8], message: &'static str) -> Self {
        Self { data, position: 0, message }
    }

    /// Return unread byte count without advancing.
    const fn remaining(&self) -> usize { self.data.len() - self.position }

    /// Read one little-endian `u16`.
    fn read_u16(&mut self) -> Result<u16, VideoProtocolError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes(
            bytes.try_into().map_err(|_length_error| self.truncated())?))
    }

    /// Read one little-endian `u64`.
    fn read_u64(&mut self) -> Result<u64, VideoProtocolError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_length_error| self.truncated())?))
    }

    /// Borrow an exact number of bytes and advance the cursor.
    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], VideoProtocolError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| self.truncated())?;
        if end > self.data.len() {
            return Err(self.truncated());
        }
        let bytes = &self.data[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    /// Build a payload-specific truncation error.
    const fn truncated(&self) -> VideoProtocolError {
        VideoProtocolError::TruncatedPayload { message: self.message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Representative baseline-profile decoder configuration.
    fn codec() -> CodecConfiguration {
        CodecConfiguration::new(
            7,
            1920,
            1200,
            [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0xD9],
            [0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80])
            .unwrap()
    }

    /// Representative one-NAL AVCC access unit.
    fn access_unit(keyframe: bool) -> AccessUnit {
        AccessUnit::new(
            7,
            16_667,
            keyframe,
            [0, 0, 0, 3, if keyframe { 0x65 } else { 0x41 }, 0x88, 0x80])
            .unwrap()
    }

    #[test]
    fn codec_configuration_should_round_trip_without_start_codes() {
        let encoded = encode_event(&VideoEvent::CodecConfiguration(codec())).unwrap();
        let VideoMessage::CodecConfiguration(decoded) = decode_message(&encoded).unwrap() else {
            panic!("expected codec configuration");
        };

        assert_eq!(decoded, codec());
    }

    #[test]
    fn access_unit_should_round_trip_keyframe_flag_and_timestamp() {
        let encoded = encode_event(&VideoEvent::AccessUnit(access_unit(true))).unwrap();
        let VideoMessage::AccessUnit(decoded) = decode_message(&encoded).unwrap() else {
            panic!("expected access unit");
        };

        assert_eq!(decoded, access_unit(true));
    }

    #[test]
    fn serialize_avcc_should_strip_mixed_annex_b_prefixes() {
        let nal_units: &[&[u8]] = &[
            &[0, 0, 0, 1, 0x67, 0x42],
            &[0, 0, 1, 0x68, 0xCE],
        ];
        let avcc = serialize_avcc(nal_units).unwrap();

        assert_eq!(
            avcc,
            [0, 0, 0, 2, 0x67, 0x42, 0, 0, 0, 2, 0x68, 0xCE]);
    }

    #[test]
    fn parser_should_reject_every_header_length_mismatch() {
        let mut encoded = encode_event(&VideoEvent::AccessUnit(access_unit(false))).unwrap();
        encoded[4..8].copy_from_slice(&1u32.to_le_bytes());
        let actual = encoded.len() - HEADER_SIZE;

        assert_eq!(
            decode_message(&encoded).unwrap_err(),
            VideoProtocolError::PayloadLengthMismatch { declared: 1, actual });
    }

    #[test]
    fn parser_should_reject_truncated_avcc_nal_units() {
        let error = AccessUnit::new(1, 0, false, [0, 0, 0, 5, 0x41]).unwrap_err();
        assert_eq!(
            error,
            VideoProtocolError::TruncatedNalUnit { declared: 5, remaining: 1 });
    }

    #[test]
    fn codec_helpers_should_build_webcodecs_values_without_panicking() {
        let codec = codec();

        assert_eq!(codec.codec_string(), "avc1.42c01e");
        assert_eq!(codec.avcc_description()[..6], [1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1]);
    }
}
