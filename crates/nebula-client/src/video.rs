//! Decoding the video the agent sends.
//!
//! The agent puts parameter sets in front of every keyframe, so this decoder
//! never has to be configured out of band and never has to wait: whatever
//! frame it sees first, if it is a keyframe, is enough to start. That is what
//! makes reconnecting cheap and what makes a client that joins a session
//! already in progress work at all.

use crate::nal;

/// One decoded picture, in planar 4:2:0, owned outright.
///
/// The platform decoders hand back a frame that borrows GPU or decoder-owned
/// memory and cannot cross a thread. Copying the planes out costs a few
/// megabytes per frame, which is real but small next to a decode, and it buys
/// a frame that the renderer can hold on its own thread for as long as it
/// likes. Zero-copy here would mean binding the decoder and the swapchain to
/// one thread; that trade is available later if it ever proves to matter.
#[derive(Debug, Clone, Default)]
pub struct Picture {
    /// Luma width in pixels.
    pub width: u32,
    /// Luma height in pixels.
    pub height: u32,
    /// Luma plane, `width * height` bytes, tightly packed.
    pub y: Vec<u8>,
    /// Blue-difference chroma, quarter resolution, tightly packed.
    pub u: Vec<u8>,
    /// Red-difference chroma, quarter resolution, tightly packed.
    pub v: Vec<u8>,
}

/// Turns an encoded stream into pictures.
pub trait VideoDecoder: Send {
    /// Decode one frame. Returns `None` when the frame produced no picture.
    fn decode(&mut self, frame: &[u8]) -> anyhow::Result<Option<Picture>>;
}

/// Build the decoder for this platform.
pub fn decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::VideoToolbox::default()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!(
            "hardware video decoding is not implemented on this platform yet; \
             NebulaDesk decodes in hardware only, because a software H.264 decoder \
             at 4K60 would cost more CPU than the rest of the client put together"
        )
    }
}

/// Copy a plane out of a decoder's buffer, discarding row padding.
///
/// Decoders align rows to whatever suits their hardware, so a plane's stride
/// is usually wider than the picture. Uploading the padding would tint the
/// right-hand edge of the image with whatever happened to be in memory.
pub(crate) fn pack(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    if stride == width && src.len() >= width * height {
        return src[..width * height].to_vec();
    }
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * stride;
        let end = start + width;
        if end > src.len() {
            // A short plane means the decoder disagrees with us about the
            // frame's size. Pad rather than panic: a slightly wrong picture
            // is recoverable at the next keyframe, a crash is not.
            out.resize(width * height, 0);
            break;
        }
        out.extend_from_slice(&src[start..end]);
    }
    out.resize(width * height, 0);
    out
}

/// De-interleave NV12's combined chroma plane into separate U and V.
pub(crate) fn split_chroma(
    uv: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> (Vec<u8>, Vec<u8>) {
    let mut u = Vec::with_capacity(width * height);
    let mut v = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * stride;
        for column in 0..width {
            let index = start + column * 2;
            u.push(uv.get(index).copied().unwrap_or(128));
            v.push(uv.get(index + 1).copied().unwrap_or(128));
        }
    }
    (u, v)
}

/// The parameter sets a decoder needs before it can be created.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parameters {
    /// Sequence parameter set.
    pub sps: Vec<u8>,
    /// Picture parameter set.
    pub pps: Vec<u8>,
}

impl Parameters {
    /// Whether both sets are present.
    #[must_use]
    pub fn complete(&self) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty()
    }

    /// Take whatever parameter sets appear in this frame.
    ///
    /// Returns whether anything changed, because a change means the stream
    /// has been reconfigured — a new resolution, say — and the decoder built
    /// for the old one will produce nothing but errors.
    pub fn absorb(&mut self, frame: &[u8]) -> bool {
        let mut changed = false;
        for unit in nal::units(frame) {
            match nal::kind(unit) {
                Some(nal::SPS) if self.sps != unit => {
                    self.sps = unit.to_vec();
                    changed = true;
                }
                Some(nal::PPS) if self.pps != unit => {
                    self.pps = unit.to_vec();
                    changed = true;
                }
                _ => {}
            }
        }
        changed
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{pack, split_chroma, Parameters, Picture, VideoDecoder};
    use shiguredo_video_toolbox::{
        DecodedFrame, Decoder, DecoderCodec, DecoderConfig, PixelFormat,
    };

    /// Hardware H.264 decoding through VideoToolbox.
    #[derive(Default)]
    pub struct VideoToolbox {
        decoder: Option<Decoder>,
        parameters: Parameters,
    }

    impl VideoDecoder for VideoToolbox {
        fn decode(&mut self, frame: &[u8]) -> anyhow::Result<Option<Picture>> {
            // Parameter sets ride in front of keyframes, so this is where a
            // decoder first gets built and where a mid-stream resolution
            // change gets noticed.
            if self.parameters.absorb(frame) && self.parameters.complete() {
                self.decoder = Some(
                    Decoder::new(DecoderConfig {
                        codec: DecoderCodec::H264 {
                            sps: &self.parameters.sps,
                            pps: &self.parameters.pps,
                            nalu_len_bytes: 4,
                        },
                        pixel_format: PixelFormat::Nv12,
                    })
                    .map_err(|error| {
                        anyhow::anyhow!("could not create a VideoToolbox decoder: {error:?}")
                    })?,
                );
            }

            // Until a keyframe arrives there is nothing that can be decoded.
            // Dropping quietly is right: this is the normal state for the
            // first few frames after joining.
            let Some(decoder) = self.decoder.as_mut() else {
                return Ok(None);
            };

            let decoded = decoder
                .decode(frame)
                .map_err(|error| anyhow::anyhow!("the decoder rejected a frame: {error:?}"))?;
            Ok(decoded.and_then(convert))
        }
    }

    /// Copy a decoded frame into owned, tightly packed planes.
    fn convert(frame: DecodedFrame) -> Option<Picture> {
        match frame {
            DecodedFrame::I420(f) => {
                let (width, height) = (f.width(), f.height());
                // Empty planes are how this decoder signals a frame it could
                // not actually produce; rendering them would show green.
                if f.y_plane().is_empty() || f.u_plane().is_empty() {
                    return None;
                }
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                Some(Picture {
                    width: width as u32,
                    height: height as u32,
                    y: pack(f.y_plane(), f.y_stride(), width, height),
                    u: pack(f.u_plane(), f.u_stride(), cw, ch),
                    v: pack(f.v_plane(), f.v_stride(), cw, ch),
                })
            }
            DecodedFrame::Nv12(f) => {
                let (width, height) = (f.width(), f.height());
                if f.y_plane().is_empty() || f.uv_plane().is_empty() {
                    return None;
                }
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                let (u, v) = split_chroma(f.uv_plane(), f.uv_stride(), cw, ch);
                Some(Picture {
                    width: width as u32,
                    height: height as u32,
                    y: pack(f.y_plane(), f.y_stride(), width, height),
                    u,
                    v,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an AVCC unit: four-byte big-endian length, then the payload.
    fn unit(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut nal = vec![kind & 0x1f];
        nal.extend_from_slice(body);
        let mut out = (nal.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&nal);
        out
    }

    #[test]
    fn parameter_sets_are_taken_from_the_frame_that_carries_them() {
        let mut parameters = Parameters::default();
        assert!(!parameters.complete());

        let mut frame = unit(nal::SPS, b"sps");
        frame.extend(unit(nal::PPS, b"pps"));
        frame.extend(unit(nal::IDR, b"picture"));

        assert!(parameters.absorb(&frame));
        assert!(parameters.complete());
        assert_eq!(parameters.sps, [nal::SPS, b's', b'p', b's']);
        assert_eq!(parameters.pps, [nal::PPS, b'p', b'p', b's']);
    }

    #[test]
    fn unchanged_parameter_sets_do_not_rebuild_the_decoder() {
        // Every keyframe repeats them, so treating a repeat as a change would
        // throw the decoder away sixty times a second.
        let mut parameters = Parameters::default();
        let mut frame = unit(nal::SPS, b"sps");
        frame.extend(unit(nal::PPS, b"pps"));
        assert!(parameters.absorb(&frame));
        assert!(!parameters.absorb(&frame));

        // A genuine change, such as a new resolution, must be noticed.
        let mut other = unit(nal::SPS, b"different");
        other.extend(unit(nal::PPS, b"pps"));
        assert!(parameters.absorb(&other));
    }

    #[test]
    fn a_frame_with_no_parameter_sets_changes_nothing() {
        let mut parameters = Parameters::default();
        assert!(!parameters.absorb(&unit(nal::NON_IDR, b"picture")));
        assert!(!parameters.complete());
    }

    #[test]
    fn packing_removes_the_padding_decoders_add_to_each_row() {
        // Two rows of three pixels in a buffer whose rows are four wide.
        let padded = [1, 2, 3, 99, 4, 5, 6, 99];
        assert_eq!(pack(&padded, 4, 3, 2), [1, 2, 3, 4, 5, 6]);
        // A plane with no padding is passed through unchanged.
        assert_eq!(pack(&[1, 2, 3, 4], 2, 2, 2), [1, 2, 3, 4]);
    }

    #[test]
    fn a_short_plane_is_padded_rather_than_panicking() {
        // A wrong picture recovers at the next keyframe; a crash does not.
        let packed = pack(&[1, 2], 4, 3, 2);
        assert_eq!(packed.len(), 6);
    }

    #[test]
    fn nv12_chroma_is_split_into_separate_planes() {
        // One row of two chroma samples, in a buffer padded to six bytes.
        let uv = [10, 20, 30, 40, 0, 0];
        let (u, v) = split_chroma(&uv, 6, 2, 1);
        assert_eq!(u, [10, 30]);
        assert_eq!(v, [20, 40]);
    }
}
