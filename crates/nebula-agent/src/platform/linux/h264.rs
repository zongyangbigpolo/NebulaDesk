//! The Linux native plugins use Annex B; NDP carries four-byte AVCC lengths.

/// Strictly split a complete AVCC access unit.
pub fn avcc_units(mut bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    anyhow::ensure!(!bytes.is_empty(), "empty H.264 access unit");
    let mut units = Vec::new();
    while !bytes.is_empty() {
        anyhow::ensure!(bytes.len() >= 4, "truncated AVCC length");
        let size = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
        bytes = &bytes[4..];
        anyhow::ensure!(size > 0 && size <= bytes.len(), "invalid AVCC NAL length");
        anyhow::ensure!(bytes[0] & 0x80 == 0, "invalid H.264 NAL header");
        units.push(&bytes[..size]);
        bytes = &bytes[size..];
    }
    Ok(units)
}

/// Convert wire access units without changing NAL order or parameter sets.
pub fn annex_b(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    for unit in avcc_units(bytes)? {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    Ok(out)
}

/// Parameter-set cache ensures every IDR is independently usable by VideoToolbox.
#[derive(Default)]
pub struct AccessUnits {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl AccessUnits {
    /// Convert a parser-aligned Annex B AU into the NDP wire format.
    pub fn convert(&mut self, data: &[u8]) -> anyhow::Result<(bool, Vec<u8>)> {
        let starts: Vec<_> = data
            .windows(3)
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == [0, 0, 1]).then_some(index))
            .collect();
        anyhow::ensure!(
            !starts.is_empty(),
            "native H.264 parser did not produce Annex B"
        );
        anyhow::ensure!(
            data[..starts[0]].iter().all(|byte| *byte == 0),
            "invalid Annex B prefix"
        );
        let mut units = Vec::new();
        let mut idr = false;
        for (index, &start) in starts.iter().enumerate() {
            let mut end = starts.get(index + 1).copied().unwrap_or(data.len());
            while end > start + 3 && data[end - 1] == 0 {
                end -= 1;
            }
            let unit = &data[start + 3..end];
            anyhow::ensure!(
                !unit.is_empty() && unit[0] & 0x80 == 0,
                "invalid Annex B NAL"
            );
            match unit[0] & 0x1f {
                7 => self.sps = unit.to_vec(),
                8 => self.pps = unit.to_vec(),
                5 => {
                    idr = true;
                    units.push(unit);
                }
                _ => units.push(unit),
            }
        }
        let mut out = Vec::with_capacity(data.len() + self.sps.len() + self.pps.len() + 8);
        if idr {
            anyhow::ensure!(
                !self.sps.is_empty() && !self.pps.is_empty(),
                "IDR has no SPS/PPS"
            );
            append(&mut out, &self.sps)?;
            append(&mut out, &self.pps)?;
        }
        for unit in units {
            append(&mut out, unit)?;
        }
        Ok((idr, out))
    }
}

fn append(out: &mut Vec<u8>, unit: &[u8]) -> anyhow::Result<()> {
    out.extend_from_slice(&u32::try_from(unit.len())?.to_be_bytes());
    out.extend_from_slice(unit);
    Ok(())
}

/// Once an encoded frame is lost, suppress every dependent frame until an IDR.
#[derive(Default)]
pub(super) struct Recovery {
    broken: bool,
}

impl Recovery {
    pub fn accept(&self, keyframe: bool) -> bool {
        !self.broken || keyframe
    }
    pub fn delivered(&mut self, keyframe: bool) {
        if keyframe {
            self.broken = false;
        }
    }
    pub fn lost(&mut self) {
        self.broken = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_gets_cached_parameter_sets_and_four_byte_lengths() {
        let mut cache = AccessUnits::default();
        let (key, data) = cache
            .convert(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3])
            .unwrap();
        assert!(key);
        assert_eq!(
            avcc_units(&data).unwrap(),
            vec![&[0x67, 1][..], &[0x68, 2], &[0x65, 3]]
        );
        let (_, repeated) = cache.convert(&[0, 0, 1, 0x65, 4]).unwrap();
        assert_eq!(avcc_units(&repeated).unwrap()[0], [0x67, 1]);
        assert!(annex_b(&data).unwrap().starts_with(&[0, 0, 0, 1, 0x67]));
    }

    #[test]
    fn malformed_bitstreams_fail_instead_of_losing_a_reference_silently() {
        for data in [&[][..], &[0, 0, 0], &[0, 0, 0, 0], &[0, 0, 0, 4, 0x65]] {
            assert!(avcc_units(data).is_err());
        }
        assert!(AccessUnits::default().convert(&[0, 0, 1, 0x65, 1]).is_err());
    }

    #[test]
    fn local_loss_suppresses_deltas_until_a_delivered_idr() {
        let mut recovery = Recovery::default();
        recovery.lost();
        assert!(!recovery.accept(false));
        assert!(recovery.accept(true));
        recovery.lost(); // A full sink can lose the recovery IDR too.
        assert!(!recovery.accept(false));
        recovery.delivered(true);
        assert!(recovery.accept(false));
    }
}
