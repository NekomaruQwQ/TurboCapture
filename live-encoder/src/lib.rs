//! `live-encoder` library — fixed shared-texture H.264 pipeline.
//!
//! Provides the GPU conversion, Media Foundation encoder, and H.264 NAL types
//! used by the managed worker and encoder callback.

pub mod converter;
pub mod d3d11;
pub mod encoder;
pub mod pipeline;

// ── H.264 NAL Unit Types ────────────────────────────────────────────────────

/// NAL unit types relevant to our H.264 baseline profile stream.
///
/// The 5-bit `nal_unit_type` field is defined in ITU-T H.264 Table 7-1.
/// We only handle the four types that appear in a baseline-profile stream
/// with no B-frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NALUnitType {
    /// Non-IDR slice (inter-predicted P-frame).
    NonIDR = 1,
    /// Instantaneous Decoder Refresh (keyframe).
    IDR = 5,
    /// Sequence Parameter Set — codec configuration.
    SPS = 7,
    /// Picture Parameter Set — picture-level parameters.
    PPS = 8,
}

impl NALUnitType {
    /// Parse NAL unit type from the first byte after a start code.
    ///
    /// The type is encoded in the lower 5 bits of the NAL header byte
    /// (H.264 spec section 7.3.1).  Returns `None` for types we don't handle.
    pub const fn from_header(header: u8) -> Option<Self> {
        match header & 0x1F {
            1 => Some(Self::NonIDR),
            5 => Some(Self::IDR),
            7 => Some(Self::SPS),
            8 => Some(Self::PPS),
            _ => None,
        }
    }
}

/// A single encoded H.264 NAL unit.
#[derive(Debug, Clone)]
pub struct NALUnit {
    /// Type of this NAL unit.
    pub unit_type: NALUnitType,
    /// Raw NAL unit data including the Annex B start code (`00 00 00 01` or `00 00 01`).
    pub data: Vec<u8>,
}
