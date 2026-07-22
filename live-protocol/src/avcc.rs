//! Annex B → AVCC conversion and codec descriptor helpers.
//!
//! Used by `live-encoder` to convert encoder output before framing, and by
//! the TS server (via documented byte layout) to build the `/init` response.

// ── Start Code Stripping ────────────────────────────────────────────────────

/// Strip the Annex B start code prefix from a NAL unit.
///
/// Media Foundation produces both 4-byte (`00 00 00 01`) and 3-byte
/// (`00 00 01`) forms.  Returns the raw NAL data after the prefix.
/// If no start code is present, returns the input unchanged.
pub fn strip_start_code(data: &[u8]) -> &[u8] {
    if data.get(..4) == Some(&[0, 0, 0, 1]) { return &data[4..]; }
    if data.get(..3) == Some(&[0, 0, 1])     { return &data[3..]; }
    data
}

/// Serialize NAL units into AVCC format for `EncodedVideoChunk`.
///
/// Each NAL unit is stripped of its Annex B start code and prefixed with a
/// 4-byte big-endian length per ISO 14496-15:
///
/// ```text
/// [u32 BE: nal_length][raw NAL data]
/// [u32 BE: nal_length][raw NAL data]
/// ...
/// ```
pub fn serialize_avcc_payload(nal_units: &[impl AsRef<[u8]>]) -> Vec<u8> {
    let stripped: Vec<&[u8]> = nal_units.iter()
        .map(|nal| strip_start_code(nal.as_ref()))
        .collect();
    let total: usize = stripped.iter().map(|d| 4 + d.len()).sum();

    let mut buf = Vec::with_capacity(total);
    for data in &stripped {
        buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        buf.extend_from_slice(data);
    }
    buf
}

// ── Codec Helpers (used by /init endpoint) ──────────────────────────────────

/// Build the `avc1.PPCCLL` codec string from SPS data.
///
/// SPS layout (after stripping any Annex B start code):
/// `[nal_header, profile_idc, constraint_flags, level_idc, ...]`.
/// The codec string encodes bytes 1-3 as hex.
///
/// Handles SPS both with and without start codes.
///
/// # Panics
///
/// Panics if the stripped SPS is shorter than 4 bytes.
pub fn build_codec_string(sps: &[u8]) -> String {
    let sps = strip_start_code(sps);
    assert!(sps.len() >= 4, "SPS too short to derive codec string ({} bytes)", sps.len());
    format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3])
}

/// Build an AVCDecoderConfigurationRecord (avcC) from SPS and PPS.
///
/// ISO 14496-15 §5.2.4.1 layout (11 + sps.len + pps.len bytes):
/// ```text
/// [u8  : 1]           configurationVersion
/// [u8×3: profile/compat/level from SPS]
/// [u8  : 0xFF]        lengthSizeMinusOne (→ 4-byte NALU lengths)
/// [u8  : 0xE1]        numSPS=1 + reserved bits
/// [u16 BE: sps.len]   sequenceParameterSetLength
/// [sps bytes]
/// [u8  : 1]           numPPS
/// [u16 BE: pps.len]   pictureParameterSetLength
/// [pps bytes]
/// ```
///
/// Handles SPS/PPS both with and without Annex B start codes.
///
/// # Panics
///
/// Panics if the stripped SPS is shorter than 4 bytes.
pub fn build_avcc_descriptor(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let sps = strip_start_code(sps);
    let pps = strip_start_code(pps);
    assert!(sps.len() >= 4, "SPS too short for avcC ({} bytes)", sps.len());

    let total = 11 + sps.len() + pps.len();
    let mut buf = Vec::with_capacity(total);

    buf.push(1);        // configurationVersion
    buf.push(sps[1]);   // AVCProfileIndication
    buf.push(sps[2]);   // profile_compatibility
    buf.push(sps[3]);   // AVCLevelIndication
    buf.push(0xFF);     // lengthSizeMinusOne = 3 (→ 4-byte prefixes)
    buf.push(0xE1);     // numSPS = 1 (+ reserved 111 bits)

    buf.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    buf.extend_from_slice(sps);

    buf.push(1);        // numPPS
    buf.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    buf.extend_from_slice(pps);

    buf
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_start_code ────────────────────────────────────────────────

    #[test]
    fn strip_4byte_start_code() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88];
        assert_eq!(strip_start_code(&data), &[0x65, 0x88]);
    }

    #[test]
    fn strip_3byte_start_code() {
        let data = [0x00, 0x00, 0x01, 0x65, 0x88];
        assert_eq!(strip_start_code(&data), &[0x65, 0x88]);
    }

    #[test]
    fn strip_no_start_code_passthrough() {
        let data = [0x65, 0x88, 0x80];
        assert_eq!(strip_start_code(&data), &[0x65, 0x88, 0x80]);
    }

    #[test]
    fn strip_empty_slice() {
        assert_eq!(strip_start_code(&[]), &[] as &[u8]);
    }

    // ── serialize_avcc_payload ──────────────────────────────────────────

    #[test]
    fn avcc_single_nal_3byte_start_code() {
        let nals: &[&[u8]] = &[&[0x00, 0x00, 0x01, 0x41, 0xAA]];
        let payload = serialize_avcc_payload(nals);
        assert_eq!(payload, vec![0x00, 0x00, 0x00, 0x02, 0x41, 0xAA]);
    }

    #[test]
    fn avcc_multi_nal_mixed_start_codes() {
        let nals: &[&[u8]] = &[
            &[0x00, 0x00, 0x00, 0x01, 0x67, 0x42],  // SPS (4-byte)
            &[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE],  // PPS (4-byte)
            &[0x00, 0x00, 0x01, 0x65, 0x88, 0x80],  // IDR (3-byte)
        ];
        let payload = serialize_avcc_payload(nals);
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x02, 0x67, 0x42,
            0x00, 0x00, 0x00, 0x02, 0x68, 0xCE,
            0x00, 0x00, 0x00, 0x03, 0x65, 0x88, 0x80,
        ];
        assert_eq!(payload, expected);
    }

    // ── build_codec_string ──────────────────────────────────────────────

    #[test]
    fn codec_string_baseline_31_no_start_code() {
        let sps = vec![0x67, 0x42, 0x00, 0x1f, 0xE9];
        assert_eq!(build_codec_string(&sps), "avc1.42001f");
    }

    #[test]
    fn codec_string_with_4byte_start_code() {
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0xE9];
        assert_eq!(build_codec_string(&sps), "avc1.42001f");
    }

    #[test]
    fn codec_string_high_40() {
        let sps = vec![0x67, 0x64, 0x00, 0x28];
        assert_eq!(build_codec_string(&sps), "avc1.640028");
    }

    // ── build_avcc_descriptor ───────────────────────────────────────────

    #[test]
    fn avcc_descriptor_strips_start_codes() {
        let sps = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0xD9];
        let pps = vec![0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x38, 0x80];

        let desc = build_avcc_descriptor(&sps, &pps);
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            1,
            0x42, 0xC0, 0x1E,
            0xFF,
            0xE1,
            0x00, 0x05,
            0x67, 0x42, 0xC0, 0x1E, 0xD9,
            1,
            0x00, 0x04,
            0x68, 0xCE, 0x38, 0x80,
        ];
        assert_eq!(desc, expected);
    }

    #[test]
    fn avcc_descriptor_without_start_codes() {
        let sps = vec![0x67, 0x42, 0xC0, 0x1E, 0xD9];
        let pps = vec![0x68, 0xCE, 0x38, 0x80];

        let desc = build_avcc_descriptor(&sps, &pps);
        assert_eq!(desc.len(), 11 + sps.len() + pps.len());
        assert_eq!(desc[1], 0x42);
        assert_eq!(desc[2], 0xC0);
        assert_eq!(desc[3], 0x1E);
    }
}
