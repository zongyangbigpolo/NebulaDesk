//! Media Foundation uses Annex B; NDP uses four-byte AVCC lengths.

use anyhow::{bail, ensure};

pub fn annex_b(avcc: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut rest = avcc;
    let mut out = Vec::with_capacity(avcc.len());
    while !rest.is_empty() {
        ensure!(rest.len() >= 4, "truncated AVCC NAL length");
        let length = u32::from_be_bytes(rest[..4].try_into()?) as usize;
        rest = &rest[4..];
        ensure!(
            length > 0 && length <= rest.len(),
            "invalid AVCC NAL length"
        );
        ensure!(rest[0] & 0x80 == 0, "invalid H.264 forbidden bit");
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rest[..length]);
        rest = &rest[length..];
    }
    ensure!(!out.is_empty(), "empty H.264 frame");
    Ok(out)
}

fn units(bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push((i, i + 4));
            i += 4;
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let Some(&(first, _)) = starts.first() else {
        bail!("Media Foundation returned H.264 without Annex B start codes");
    };
    ensure!(
        bytes[..first].iter().all(|&b| b == 0),
        "invalid Annex B prefix"
    );
    let mut out = Vec::new();
    for (index, &(_, start)) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(bytes.len(), |&(at, _)| at);
        while end > start && bytes[end - 1] == 0 {
            end -= 1;
        }
        ensure!(end > start, "empty Annex B NAL");
        out.push(&bytes[start..end]);
    }
    Ok(out)
}

#[derive(Default)]
pub struct ParameterSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl ParameterSets {
    pub fn absorb(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        for unit in units(bytes)? {
            match unit[0] & 31 {
                7 => self.sps = unit.to_vec(),
                8 => self.pps = unit.to_vec(),
                _ => {}
            }
        }
        Ok(())
    }

    /// Keyframe status comes from an IDR NAL, not a driver's clean-point hint.
    pub fn frame(&mut self, bytes: &[u8]) -> anyhow::Result<(bool, Vec<u8>)> {
        self.absorb(bytes)?;
        let nals = units(bytes)?;
        let idr = nals.iter().any(|unit| unit[0] & 31 == 5);
        let mut out = Vec::with_capacity(bytes.len() + self.sps.len() + self.pps.len() + 8);
        if idr {
            ensure!(
                !self.sps.is_empty() && !self.pps.is_empty(),
                "hardware encoder emitted an IDR without SPS/PPS"
            );
            append(&mut out, &self.sps)?;
            append(&mut out, &self.pps)?;
        }
        for unit in nals {
            if matches!(unit[0] & 31, 1 | 5) {
                let rbsp = unescape(&unit[1..unit.len().min(64)]);
                let mut bits = Bits {
                    data: &rbsp,
                    offset: 0,
                };
                bits.ue()?;
                let slice = bits.ue()?;
                ensure!(
                    slice <= 9 && slice % 5 != 1,
                    "hardware encoder produced a B slice despite zero-B-frame configuration"
                );
            }
            if !matches!(unit[0] & 31, 7 | 8) {
                append(&mut out, unit)?;
            }
        }
        Ok((idr, out))
    }
}

fn append(out: &mut Vec<u8>, unit: &[u8]) -> anyhow::Result<()> {
    out.extend_from_slice(&u32::try_from(unit.len())?.to_be_bytes());
    out.extend_from_slice(unit);
    Ok(())
}

/// Read just the progressive, 8-bit 4:2:0 geometry needed to configure MF.
pub fn dimensions(sps: &[u8]) -> anyhow::Result<(u32, u32)> {
    ensure!(
        sps.first().is_some_and(|b| b & 31 == 7),
        "missing H.264 SPS"
    );
    let rbsp = unescape(&sps[1..]);
    let mut bits = Bits {
        data: &rbsp,
        offset: 0,
    };
    let profile = bits.read(8)?;
    bits.read(16)?;
    bits.ue()?;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        ensure!(bits.ue()? == 1, "Windows decoder requires 4:2:0 H.264");
        ensure!(
            bits.ue()? == 0 && bits.ue()? == 0,
            "Windows decoder requires 8-bit H.264"
        );
        bits.read(1)?;
        if bits.read(1)? != 0 {
            for index in 0..8 {
                if bits.read(1)? != 0 {
                    let mut last = 8;
                    let mut next = 8;
                    for _ in 0..if index < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + bits.se()? + 256).rem_euclid(256);
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    bits.ue()?;
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.read(1)?;
            bits.se()?;
            bits.se()?;
            let count = bits.ue()?;
            ensure!(count <= 255, "invalid SPS picture-order cycle");
            for _ in 0..count {
                bits.se()?;
            }
        }
        2 => {}
        _ => bail!("invalid SPS picture order"),
    }
    bits.ue()?;
    bits.read(1)?;
    let macro_width = bits.ue()?;
    let macro_height = bits.ue()?;
    ensure!(
        macro_width < 512 && macro_height < 512,
        "H.264 dimensions exceed 8192 pixels"
    );
    ensure!(bits.read(1)? == 1, "interlaced H.264 is not supported");
    bits.read(1)?;
    let (mut width, mut height) = ((macro_width + 1) * 16, (macro_height + 1) * 16);
    if bits.read(1)? != 0 {
        let left = bits.ue()?;
        let right = bits.ue()?;
        let top = bits.ue()?;
        let bottom = bits.ue()?;
        ensure!(
            left == 0 && top == 0,
            "nonzero H.264 crop origin is unsupported"
        );
        width = width
            .checked_sub(
                right
                    .checked_mul(2)
                    .ok_or_else(|| anyhow::anyhow!("SPS crop overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("invalid horizontal SPS crop"))?;
        height = height
            .checked_sub(
                bottom
                    .checked_mul(2)
                    .ok_or_else(|| anyhow::anyhow!("SPS crop overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("invalid vertical SPS crop"))?;
    }
    ensure!(width >= 2 && height >= 2, "empty H.264 picture");
    Ok((width, height))
}

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    rbsp
}

struct Bits<'a> {
    data: &'a [u8],
    offset: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> anyhow::Result<u32> {
        ensure!(
            self.offset + count <= self.data.len() * 8,
            "truncated H.264 SPS"
        );
        let mut value = 0;
        for _ in 0..count {
            value =
                (value << 1) | u32::from((self.data[self.offset / 8] >> (7 - self.offset % 8)) & 1);
            self.offset += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut leading = 0;
        while self.read(1)? == 0 {
            leading += 1;
            ensure!(leading < 31, "oversized H.264 Exp-Golomb value");
        }
        Ok((1 << leading) - 1 + self.read(leading)?)
    }
    fn se(&mut self) -> anyhow::Result<i32> {
        let value = self.ue()?;
        Ok(if value & 1 != 0 {
            value.div_ceil(2) as i32
        } else {
            -(value as i32 / 2)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_start_codes_and_cached_parameter_sets() {
        let mut sets = ParameterSets::default();
        sets.absorb(&[0, 0, 0, 1, 0x67, 42, 0, 0, 1, 0x68, 23])
            .unwrap();
        let (key, data) = sets.frame(&[0, 0, 1, 0x65, 0xb8]).unwrap();
        assert!(key);
        assert_eq!(
            annex_b(&data).unwrap(),
            [0, 0, 0, 1, 0x67, 42, 0, 0, 0, 1, 0x68, 23, 0, 0, 0, 1, 0x65, 0xb8]
        );
        assert!(!sets.frame(&[0, 0, 1, 0x41, 0xe0]).unwrap().0);
        assert!(sets.frame(&[0, 0, 1, 0x41, 0xa8]).is_err());
    }

    #[test]
    fn rejects_truncation_and_unconfigured_idr() {
        for data in [&[][..], &[0, 0, 0], &[0, 0, 0, 0], &[0, 0, 0, 2, 0x65]] {
            assert!(annex_b(data).is_err());
        }
        assert!(ParameterSets::default().frame(&[0, 0, 1, 0x65]).is_err());
    }

    fn sps(width_mbs: u32, height_mbs: u32, crop_bottom: u32) -> Vec<u8> {
        fn ue(bits: &mut String, value: u32) {
            let value = format!("{:b}", value + 1);
            bits.push_str(&"0".repeat(value.len() - 1));
            bits.push_str(&value);
        }
        let mut bits = String::new();
        for value in [0, 0, 0, 0, 1] {
            ue(&mut bits, value);
        }
        bits.push('0');
        ue(&mut bits, width_mbs - 1);
        ue(&mut bits, height_mbs - 1);
        bits.push_str("111");
        for value in [0, 0, 0, crop_bottom] {
            ue(&mut bits, value);
        }
        bits.push('1');
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        let mut data = vec![0x67, 66, 0, 31];
        for byte in bits.as_bytes().chunks_exact(8) {
            data.push(
                byte.iter()
                    .fold(0, |value, bit| (value << 1) | (bit - b'0')),
            );
        }
        data
    }

    #[test]
    fn progressive_sps_geometry_and_crop_are_bounded() {
        assert_eq!(dimensions(&sps(120, 68, 4)).unwrap(), (1920, 1080));
        assert_eq!(dimensions(&sps(80, 45, 0)).unwrap(), (1280, 720));
        assert!(dimensions(&sps(80, 45, 400)).is_err());
        assert!(dimensions(&sps(513, 1, 0)).is_err());
        assert!(dimensions(&[0x67, 66]).is_err());
    }
}
