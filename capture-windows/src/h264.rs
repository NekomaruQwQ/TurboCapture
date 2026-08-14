//! Borrowed Annex B packetization into `capture-core` video events.

use anyhow::{Context as _, ensure};
use capture_core::{
    AccessUnit, CodecConfiguration, VideoEvent, serialize_avcc,
};

/// Stateful parameter-set cache and codec-generation owner.
pub struct H264Packetizer {
    width: u32,
    height: u32,
    generation: u64,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl H264Packetizer {
    /// Create an uninitialized packetizer for fixed output dimensions.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height, generation: 0, sps: None, pps: None }
    }

    /// Convert one complete Annex B encoder output into ordered core events.
    ///
    /// A changed SPS/PPS pair advances the decoder generation and is emitted
    /// immediately before any access unit referring to that generation.
    pub fn packetize(
        &mut self,
        annex_b: &[u8],
        timestamp_us: u64) -> anyhow::Result<Vec<VideoEvent>> {
        let nal_units = split_annex_b(annex_b)?;
        let mut observed_sequence = None;
        let mut observed_picture = None;
        let mut keyframe = false;
        for nal in &nal_units {
            match nal.kind {
                5 => keyframe = true,
                7 => observed_sequence = Some(nal.payload),
                8 => observed_picture = Some(nal.payload),
                _ => {}
            }
        }

        let sequence_changed = observed_sequence
            .is_some_and(|sequence| self.sps.as_deref() != Some(sequence));
        let picture_changed = observed_picture
            .is_some_and(|picture| self.pps.as_deref() != Some(picture));
        if sequence_changed {
            self.sps = observed_sequence.map(<[u8]>::to_vec);
        }
        if picture_changed {
            self.pps = observed_picture.map(<[u8]>::to_vec);
        }
        let changed = sequence_changed || picture_changed;
        let sps = self.sps.as_deref().context("encoder output has no SPS for its active codec")?;
        let pps = self.pps.as_deref().context("encoder output has no PPS for its active codec")?;
        let mut events = Vec::with_capacity(usize::from(changed) + 1);
        if changed {
            self.generation = self.generation.checked_add(1)
                .context("H.264 codec generation exhausted")?;
            let codec = CodecConfiguration::new(
                self.generation,
                self.width,
                self.height,
                sps.to_vec(),
                pps.to_vec())
                .context("encoder produced invalid H.264 parameter sets")?;
            events.push(VideoEvent::CodecConfiguration(codec));
        }
        ensure!(self.generation > 0, "encoder emitted an access unit before codec configuration");

        let avcc = serialize_avcc(&nal_units)
            .context("encoder produced invalid Annex B output")?;
        let access_unit = AccessUnit::new(self.generation, timestamp_us, keyframe, avcc)
            .context("encoder produced an invalid access unit")?;
        events.push(VideoEvent::AccessUnit(access_unit));
        Ok(events)
    }
}

/// One borrowed NAL including its prefix and excluding it for header inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NalUnit<'a> {
    full: &'a [u8],
    payload: &'a [u8],
    kind: u8,
}

impl AsRef<[u8]> for NalUnit<'_> {
    /// Expose the full prefixed NAL to the core's borrowed AVCC serializer.
    #[inline]
    fn as_ref(&self) -> &[u8] { self.full }
}

/// Split both standard Annex B prefix forms while retaining every NAL type.
fn split_annex_b(data: &[u8]) -> anyhow::Result<Vec<NalUnit<'_>>> {
    let mut starts = Vec::new();
    let mut position = 0usize;
    while position + 3 <= data.len() {
        let prefix_length = if data.get(position..position + 4) == Some(&[0, 0, 0, 1]) {
            4
        } else if data.get(position..position + 3) == Some(&[0, 0, 1]) {
            3
        } else {
            position += 1;
            continue;
        };
        starts.push((position, prefix_length));
        position += prefix_length;
    }
    ensure!(!starts.is_empty(), "encoder output contains no Annex B NAL unit");
    ensure!(
        data[..starts[0].0].iter().all(|byte| *byte == 0),
        "encoder output contains bytes before its first Annex B start code");

    let mut nal_units = Vec::with_capacity(starts.len());
    for (index, &(start, prefix_length)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(data.len(), |next| next.0);
        let payload_start = start + prefix_length;
        ensure!(payload_start < end, "encoder output contains an empty Annex B NAL unit");
        let payload = &data[payload_start..end];
        nal_units.push(NalUnit {
            full: &data[start..end],
            payload,
            kind: payload[0] & 0x1F,
        });
    }
    Ok(nal_units)
}

#[cfg(test)]
mod tests {
    use capture_core::{VideoEvent, VideoProtocolError};

    use super::*;

    /// Baseline parameter sets plus one IDR with mixed prefix lengths.
    const FIRST_ACCESS_UNIT: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E,
        0, 0, 1, 0x68, 0xCE, 0x38,
        0, 0, 0, 1, 0x06, 0x05,
        0, 0, 1, 0x65, 0x88,
    ];

    #[test]
    fn splitter_should_preserve_unrecognized_nal_types() {
        let units = split_annex_b(FIRST_ACCESS_UNIT).unwrap();
        assert_eq!(units.iter().map(|unit| unit.kind).collect::<Vec<_>>(), [7, 8, 6, 5]);
    }

    #[test]
    fn packetizer_should_publish_codec_before_first_keyframe() {
        let events = H264Packetizer::new(1920, 1200)
            .packetize(FIRST_ACCESS_UNIT, 123)
            .unwrap();

        assert!(matches!(events[0], VideoEvent::CodecConfiguration(_)));
        let &VideoEvent::AccessUnit(ref access_unit) = &events[1] else {
            panic!("expected access unit after codec configuration");
        };
        assert!(access_unit.is_keyframe());
        assert_eq!(access_unit.timestamp_us(), 123);
    }

    #[test]
    fn packetizer_should_advance_generation_only_when_parameter_sets_change() {
        let mut packetizer = H264Packetizer::new(1280, 720);
        let initial = packetizer.packetize(FIRST_ACCESS_UNIT, 0).unwrap();
        let same = packetizer.packetize(&[0, 0, 1, 0x41, 0x80], 1).unwrap();
        let changed = packetizer.packetize(&[
            0, 0, 1, 0x67, 0x42, 0xC0, 0x20,
            0, 0, 1, 0x68, 0xCE, 0x38,
            0, 0, 1, 0x65, 0x80,
        ], 2).unwrap();

        assert_eq!(initial.len(), 2);
        assert_eq!(same.len(), 1);
        let &VideoEvent::CodecConfiguration(ref codec) = &changed[0] else {
            panic!("changed SPS should publish codec configuration");
        };
        assert_eq!(codec.generation(), 2);
    }

    #[test]
    fn packetizer_should_reject_picture_data_before_parameter_sets() {
        let error = H264Packetizer::new(1280, 720)
            .packetize(&[0, 0, 1, 0x41, 0x80], 0)
            .unwrap_err();
        assert!(error.to_string().contains("no SPS"));
    }

    #[test]
    fn split_should_reject_trailing_empty_nal() {
        let error = split_annex_b(&[0, 0, 1]).unwrap_err();
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn core_protocol_error_remains_a_source() {
        let error: anyhow::Error = VideoProtocolError::EmptyAvcc.into();
        assert!(error.downcast_ref::<VideoProtocolError>().is_some());
    }
}
