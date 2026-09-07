//! Decoding the video the agent sends.
//!
//! The agent puts parameter sets in front of every keyframe, so this decoder
//! never has to be configured out of band and never has to wait: whatever
//! frame it sees first, if it is a keyframe, is enough to start. That is what
//! makes reconnecting cheap and what makes a client that joins a session
//! already in progress work at all.

use crate::nal;

#[cfg(target_os = "linux")]
mod linux;

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
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::VaApi::new()?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// Frames that are ready to decode, plus whether to ask for a keyframe.
#[derive(Debug, Default)]
pub struct Ready {
    /// Payloads in sending order, which is the order they must be decoded in.
    pub frames: Vec<Vec<u8>>,
    /// Whether the reference chain is broken and only a keyframe can fix it.
    pub ask_for_keyframe: bool,
}

/// How many out-of-order frames to hold before giving up on the missing one.
///
/// Reordering observed on a real link runs to about four frames. Eight is
/// room to spare without committing to much latency: at sixty frames a second
/// the whole buffer is worth a little over a tenth of a second, and it only
/// ever fills when a frame is genuinely gone.
const REORDER_WINDOW: usize = 16;

/// How long to wait before asking for a keyframe again.
const ASK_AGAIN: std::time::Duration = std::time::Duration::from_millis(500);

/// How far apart two sequence numbers are, whichever way round they came.
fn distance(a: u32, b: u32) -> u32 {
    a.wrapping_sub(b).min(b.wrapping_sub(a))
}

/// Puts arriving video frames back into the order they were sent in.
///
/// Every frame travels on its own QUIC stream and is read by its own task, so
/// the order they finish in is not the order they were sent in — on a real
/// link they routinely arrive several places out. Frames also go missing,
/// because the agent abandons ones the network cannot carry in time.
///
/// Neither shows up as a decode error. Handing a decoder a P-frame whose
/// reference it has not seen does not fail; it produces a picture built on
/// whatever happened to be in the reference slot, and every frame after it
/// inherits the damage. That is what a torn, ghosted, discolouring stream is,
/// and it never recovers on its own because nothing ever reports a problem.
///
/// So the order is restored here, where the sequence numbers are, and what
/// cannot be restored is turned into a request for a keyframe.
#[derive(Debug, Default)]
pub struct VideoOrder {
    /// The sequence number wanted next, once a keyframe has anchored the
    /// stream. `None` means nothing can be decoded until one arrives.
    next: Option<u32>,
    /// Frames held back because an earlier one has not arrived yet, keyed by
    /// how far ahead of `next` they are so that they sort correctly even when
    /// the sequence counter wraps.
    pending: std::collections::BTreeMap<u64, Vec<u8>>,
    /// How far `next` has advanced in total, which is what `pending` is keyed
    /// against.
    position: u64,
    /// Frames that arrived while nothing was anchored, keyed by raw sequence
    /// number.
    ///
    /// A keyframe is the largest frame there is, so it is routinely overtaken
    /// by the smaller ones sent after it. Throwing those away because the
    /// keyframe had not landed yet leaves a hole the keyframe cannot fill,
    /// and the picture stalls waiting for frames that were already in hand.
    orphans: std::collections::BTreeMap<u32, Vec<u8>>,
    /// When a keyframe was last asked for, if the chain is broken.
    ///
    /// Asking on every frame that arrives during an outage is a burst of
    /// control messages heavy enough, at sixty a second, to take the session
    /// down. Asking exactly once is worse in a different way: the request can
    /// be lost, or arrive while the agent is mid-frame and be answered by a
    /// keyframe that is itself discarded, and then nothing ever asks again
    /// and the session sits in front of a frozen picture. So it repeats, at a
    /// rate that repairs promptly and costs nothing.
    asked: Option<std::time::Instant>,
}

impl VideoOrder {
    /// A buffer that has seen nothing and is waiting for a keyframe.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one arriving frame and return whatever is now ready to decode.
    pub fn accept(&mut self, seq: u32, keyframe: bool, payload: Vec<u8>) -> Ready {
        let Some(next) = self.next else {
            // Nothing here can be decoded yet, but the frames arriving
            // alongside a keyframe that is still in flight are the ones the
            // keyframe is about to make decodable, so they are kept.
            if !keyframe {
                self.orphans
                    .retain(|&held, _| distance(held, seq) <= REORDER_WINDOW as u32);
                self.orphans.insert(seq, payload);
                return self.ask();
            }
            self.anchor(seq);
            self.pending.insert(self.position, payload);
            self.promote(seq);
            return self.drain();
        };

        let ahead = seq.wrapping_sub(next);
        if ahead >= u32::MAX / 2 {
            // It arrived after the picture had already moved past it.
            return Ready::default();
        }

        // A keyframe depends on nothing, so it can be decoded straight away
        // and anything still waiting in front of it is no longer needed.
        if keyframe {
            self.pending.clear();
            self.position += u64::from(ahead);
            self.next = Some(seq);
            self.asked = None;
            self.pending.insert(self.position, payload);
            return self.drain();
        }

        self.pending
            .insert(self.position + u64::from(ahead), payload);
        if self.pending.len() > REORDER_WINDOW {
            // The frame being waited on is not coming. Everything held behind
            // it references it, directly or through its neighbours.
            self.pending.clear();
            self.next = None;
            return self.ask();
        }
        self.drain()
    }

    /// Start decoding from `seq`, discarding anything held before it.
    fn anchor(&mut self, seq: u32) {
        self.pending.clear();
        self.next = Some(seq);
        self.asked = None;
    }

    /// Move the frames held while unanchored into place behind a keyframe.
    ///
    /// Anything at or before the keyframe is redundant, and anything further
    /// ahead than the window is beyond what will ever be waited for.
    fn promote(&mut self, keyframe: u32) {
        for (seq, payload) in std::mem::take(&mut self.orphans) {
            let ahead = seq.wrapping_sub(keyframe);
            if ahead == 0 || ahead > REORDER_WINDOW as u32 {
                continue;
            }
            self.pending
                .insert(self.position + u64::from(ahead), payload);
        }
    }

    /// Release the run of frames starting at the one wanted next.
    fn drain(&mut self) -> Ready {
        let mut ready = Ready::default();
        while let Some(payload) = self.pending.remove(&self.position) {
            ready.frames.push(payload);
            self.position += 1;
            self.next = Some(self.next.unwrap_or_default().wrapping_add(1));
        }
        ready
    }

    /// Ask for a keyframe, unless one was asked for a moment ago.
    fn ask(&mut self) -> Ready {
        let now = std::time::Instant::now();
        if self
            .asked
            .is_some_and(|at| now.duration_since(at) < ASK_AGAIN)
        {
            return Ready::default();
        }
        self.asked = Some(now);
        Ready {
            frames: Vec::new(),
            ask_for_keyframe: true,
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

    /// A frame whose payload is just its sequence number, so that the order
    /// they come back out in is readable.
    fn frame(seq: u32) -> Vec<u8> {
        seq.to_be_bytes().to_vec()
    }

    /// The sequence numbers `accept` released, in order.
    fn released(ready: &Ready) -> Vec<u32> {
        ready
            .frames
            .iter()
            .map(|f| u32::from_be_bytes(f[..4].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn nothing_decodes_until_a_keyframe_arrives() {
        let mut order = VideoOrder::new();
        let ready = order.accept(7, false, frame(7));
        assert!(ready.frames.is_empty());
        assert!(ready.ask_for_keyframe);

        // And only the first of them asks: sixty a second would flood the
        // control channel hard enough to end the session.
        let ready = order.accept(8, false, frame(8));
        assert!(ready.frames.is_empty());
        assert!(!ready.ask_for_keyframe);

        assert_eq!(released(&order.accept(9, true, frame(9))), [9]);
        assert_eq!(released(&order.accept(10, false, frame(10))), [10]);
    }

    #[test]
    fn frames_that_overtook_a_late_keyframe_are_not_thrown_away() {
        // A keyframe is the biggest frame there is, so the small ones sent
        // after it routinely land first. Discarding those leaves a hole the
        // keyframe cannot fill and the picture never starts.
        let mut order = VideoOrder::new();
        for seq in [2, 4, 1, 3] {
            assert!(order.accept(seq, false, frame(seq)).frames.is_empty());
        }
        let ready = order.accept(0, true, frame(0));
        assert_eq!(released(&ready), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn frames_older_than_the_keyframe_are_not_decoded_after_it() {
        let mut order = VideoOrder::new();
        assert!(order.accept(5, false, frame(5)).frames.is_empty());
        assert!(order.accept(7, false, frame(7)).frames.is_empty());
        let ready = order.accept(6, true, frame(6));
        assert_eq!(released(&ready), [6, 7]);
    }

    #[test]
    fn frames_that_overtake_each_other_are_put_back_in_order() {
        // This is what a real link does: the ordering below was recorded from
        // one. Nothing here is lost, so nothing should be dropped and no
        // keyframe should be asked for.
        let mut order = VideoOrder::new();
        assert_eq!(released(&order.accept(0, true, frame(0))), [0]);

        assert!(released(&order.accept(2, false, frame(2))).is_empty());
        assert_eq!(released(&order.accept(1, false, frame(1))), [1, 2]);
        assert_eq!(released(&order.accept(3, false, frame(3))), [3]);
        assert!(released(&order.accept(7, false, frame(7))).is_empty());
        assert!(released(&order.accept(8, false, frame(8))).is_empty());
        assert!(released(&order.accept(6, false, frame(6))).is_empty());
        assert!(released(&order.accept(5, false, frame(5))).is_empty());
        let ready = order.accept(4, false, frame(4));
        assert_eq!(released(&ready), [4, 5, 6, 7, 8]);
        assert!(!ready.ask_for_keyframe);
    }

    #[test]
    fn a_frame_that_never_arrives_costs_a_keyframe_and_not_the_session() {
        let mut order = VideoOrder::new();
        assert_eq!(released(&order.accept(0, true, frame(0))), [0]);

        // Frame 1 was abandoned by the agent. The buffer waits, then gives up.
        let mut asks = 0;
        for seq in 2..=(REORDER_WINDOW as u32 + 2) {
            let ready = order.accept(seq, false, frame(seq));
            assert!(released(&ready).is_empty());
            asks += usize::from(ready.ask_for_keyframe);
        }
        assert_eq!(asks, 1, "giving up should ask exactly once");

        // Everything after that is held until the keyframe lands.
        let ready = order.accept(20, false, frame(20));
        assert!(released(&ready).is_empty());
        assert!(!ready.ask_for_keyframe);

        assert_eq!(released(&order.accept(21, true, frame(21))), [21]);
        assert_eq!(released(&order.accept(22, false, frame(22))), [22]);
    }

    #[test]
    fn a_keyframe_does_not_wait_for_frames_it_has_made_pointless() {
        let mut order = VideoOrder::new();
        assert_eq!(released(&order.accept(0, true, frame(0))), [0]);

        // 1 is missing and 2 is waiting on it, but 3 refreshes the whole
        // picture, so 2 is no longer worth anything.
        assert!(released(&order.accept(2, false, frame(2))).is_empty());
        assert_eq!(released(&order.accept(3, true, frame(3))), [3]);
        assert_eq!(released(&order.accept(4, false, frame(4))), [4]);
    }

    #[test]
    fn a_straggler_is_dropped_rather_than_rewinding_the_picture() {
        let mut order = VideoOrder::new();
        assert_eq!(released(&order.accept(0, true, frame(0))), [0]);
        assert_eq!(released(&order.accept(1, false, frame(1))), [1]);
        assert_eq!(released(&order.accept(2, false, frame(2))), [2]);

        // Frame 1 was retransmitted and its stream finished last.
        let ready = order.accept(1, false, frame(1));
        assert!(released(&ready).is_empty());
        assert!(!ready.ask_for_keyframe);
        assert_eq!(released(&order.accept(3, false, frame(3))), [3]);
    }

    #[test]
    fn the_sequence_counter_may_wrap() {
        let mut order = VideoOrder::new();
        assert_eq!(
            released(&order.accept(u32::MAX - 1, true, frame(u32::MAX - 1))),
            [u32::MAX - 1]
        );
        // The two after the wrap arrive first, and must still sort after it.
        assert!(released(&order.accept(0, false, frame(0))).is_empty());
        assert!(released(&order.accept(1, false, frame(1))).is_empty());
        assert_eq!(
            released(&order.accept(u32::MAX, false, frame(u32::MAX))),
            [u32::MAX, 0, 1]
        );
    }
}
