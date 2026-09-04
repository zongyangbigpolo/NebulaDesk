//! Walking AVCC bitstreams.
//!
//! AVCC frames every NAL unit with a four-byte big-endian length, which is
//! what both VideoToolbox and Media Foundation want and what the agent
//! produces. Nothing here interprets a NAL's contents; the point is only to
//! find the parameter sets so a decoder can be built from the stream itself.

/// Sequence parameter set.
pub const SPS: u8 = 7;
/// Picture parameter set.
pub const PPS: u8 = 8;
/// Coded slice of an IDR picture — a keyframe.
pub const IDR: u8 = 5;
/// Coded slice of a non-IDR picture.
pub const NON_IDR: u8 = 1;

/// How many bytes prefix each unit.
const LENGTH_PREFIX: usize = 4;

/// The NAL unit type of a unit body, if it has one.
#[must_use]
pub fn kind(unit: &[u8]) -> Option<u8> {
    unit.first().map(|byte| byte & 0x1f)
}

/// Iterate the NAL units in an AVCC frame.
///
/// A malformed or truncated frame ends the iteration rather than producing
/// garbage: this runs on data from the network, and the only safe reading of
/// a length that overruns the buffer is that there is nothing more to read.
pub fn units(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut offset = 0usize;
    std::iter::from_fn(move || {
        let header = frame.get(offset..offset + LENGTH_PREFIX)?;
        let length = u32::from_be_bytes(header.try_into().ok()?) as usize;
        let start = offset + LENGTH_PREFIX;
        let unit = frame.get(start..start.checked_add(length)?)?;
        offset = start + length;
        (!unit.is_empty()).then_some(unit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            out.extend_from_slice(unit);
        }
        out
    }

    #[test]
    fn units_are_read_back_in_order() {
        let frame = avcc(&[&[7, 1, 2], &[8, 3], &[5, 4, 5, 6]]);
        let read: Vec<_> = units(&frame).collect();
        assert_eq!(read, vec![&[7u8, 1, 2][..], &[8, 3][..], &[5, 4, 5, 6][..]]);
        assert_eq!(kind(read[0]), Some(SPS));
        assert_eq!(kind(read[1]), Some(PPS));
        assert_eq!(kind(read[2]), Some(IDR));
    }

    #[test]
    fn the_type_ignores_the_bits_above_it() {
        // The low five bits are the type; ref_idc and the forbidden bit sit
        // above and must not leak into the answer.
        assert_eq!(kind(&[0x67]), Some(SPS));
        assert_eq!(kind(&[0x65]), Some(IDR));
        assert_eq!(kind(&[]), None);
    }

    #[test]
    fn a_truncated_frame_stops_rather_than_reading_past_the_end() {
        // A length that claims more than is present: nothing is yielded.
        let mut frame = 100u32.to_be_bytes().to_vec();
        frame.extend_from_slice(&[7, 1]);
        assert_eq!(units(&frame).count(), 0);

        // A frame that ends mid-header stops after the units it did contain.
        let mut frame = avcc(&[&[7, 1, 2]]);
        frame.extend_from_slice(&[0, 0]);
        assert_eq!(units(&frame).count(), 1);
    }

    #[test]
    fn empty_and_zero_length_units_yield_nothing() {
        assert_eq!(units(&[]).count(), 0);
        assert_eq!(units(&0u32.to_be_bytes()).count(), 0);
    }
}
