//! Screen capture and hardware encoding on macOS.
//!
//! ScreenCaptureKit hands out `CVPixelBuffer`s that already live in GPU
//! memory, and VideoToolbox will encode one without ever copying it back to
//! the CPU. That pairing is the whole reason for the choice: at 4K60 a single
//! round trip through main memory per frame is more work than everything else
//! this program does put together.
//!
//! # What goes on the wire
//!
//! AVCC — each NAL unit prefixed with a four-byte big-endian length. Every
//! keyframe carries its own parameter sets, so a client that joins late, or
//! that lost the stream and came back, can decode from the first frame it
//! sees without asking for anything.
//!
//! # Permission
//!
//! Screen Recording is granted per signed binary, and without it
//! ScreenCaptureKit returns an empty display list rather than an error. That
//! is reported here as a refusal to start, because a session that connects
//! and then shows nothing is far harder to diagnose than one that says why.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::configuration::pixel_format::PixelFormat as ScPixelFormat;
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;
use screencapturekit::stream::output_type::SCStreamOutputType;
use screencapturekit::stream::SCStream;
use shiguredo_video_toolbox::{
    CodecConfig, EncodeOptions, Encoder, EncoderConfig, H264EncoderConfig, H264EntropyMode,
    H264Profile, PixelFormat,
};

use crate::media::{EncodedFrame, FrameSink, VideoConfig, VideoSource};

/// How many frames ScreenCaptureKit may hold before it starts dropping.
///
/// Small on purpose. A deep queue does not make a slow encoder faster; it
/// just means the frames that eventually arrive describe a screen that has
/// already moved on.
const QUEUE_DEPTH: u32 = 3;

/// How often the encode loop looks up to see whether it should still be
/// running. Short enough that stopping feels immediate, long enough that an
/// idle screen costs nothing.
const WAKE: std::time::Duration = std::time::Duration::from_millis(100);

/// What to tell an operator when the screen cannot be captured.
///
/// Screen Recording is granted to a *binary*, and rebuilding the agent
/// invalidates the grant, so this is the failure people will hit most often
/// and by some margin. It is worth spelling out.
const PERMISSION: &str = "cannot capture this display. Grant Screen Recording to this binary \
     in System Settings > Privacy & Security > Screen & System Audio Recording, then run the \
     agent again. If it is already listed, remove it and re-add it: the permission is tied to \
     the exact binary and a rebuild invalidates it";

/// Captures the main display and encodes it in hardware.
pub struct MacVideo {
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Default for MacVideo {
    fn default() -> Self {
        Self::new()
    }
}

impl MacVideo {
    /// Create a source that has not started capturing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keyframe: Arc::new(AtomicBool::new(false)),
            bitrate: Arc::new(AtomicU32::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }
}

impl VideoSource for MacVideo {
    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
        // A machine without permission fails both ways depending on macOS
        // version and TCC history: an error, or an empty display list. Both
        // mean the same thing to whoever has to fix it, so both say so.
        let content = SCShareableContent::get()
            .map_err(|error| anyhow::anyhow!("{}: {error:?}", PERMISSION))?;
        let displays = content.displays();
        let display = displays
            .first()
            .ok_or_else(|| anyhow::anyhow!(PERMISSION))?;

        // Capture at the size the client asked for and let ScreenCaptureKit
        // do the scaling: it is already compositing, so scaling there is free
        // and scaling in the encoder is not.
        let (width, height) = fit(
            display.width(),
            display.height(),
            config.width,
            config.height,
        );

        let filter = SCContentFilter::create().with_display(display).build();
        let stream_config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            // NV12 video range: the one format ScreenCaptureKit produces and
            // VideoToolbox consumes without a conversion pass in between.
            .with_pixel_format(ScPixelFormat::YCbCr_420v)
            .with_queue_depth(QUEUE_DEPTH)
            .with_fps(config.fps)
            // The remote pointer is drawn by the client, which knows where it
            // put it; compositing the local one too would show two.
            .with_shows_cursor(false);

        // ScreenCaptureKit delivers on its own dispatch queue, so frames
        // cross into Rust here and are handed to a thread that owns the
        // encoder. The encoder is neither Sync nor safe to touch from an
        // arbitrary queue, and a bounded channel means a slow encoder drops
        // frames rather than growing a backlog of stale ones.
        let (frames, incoming) = mpsc::sync_channel::<CMSampleBuffer>(QUEUE_DEPTH as usize);
        let mut stream = SCStream::new(&filter, &stream_config);
        stream.add_output_handler(
            move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                if kind == SCStreamOutputType::Screen {
                    // A full channel means the encoder is behind. Dropping the
                    // newest frame is wrong — drop nothing and let the next
                    // one through — but there is no way to replace the queued
                    // one, so the newest is what goes.
                    let _ = frames.try_send(sample);
                }
            },
            SCStreamOutputType::Screen,
        );

        let keyframe = Arc::clone(&self.keyframe);
        let bitrate = Arc::clone(&self.bitrate);
        let running = Arc::clone(&self.running);
        self.bitrate.store(config.bitrate, Ordering::Relaxed);
        running.store(true, Ordering::Relaxed);

        let (ready, started) = mpsc::channel::<anyhow::Result<()>>();
        let worker = std::thread::Builder::new()
            .name("nebula-capture".into())
            .spawn(move || {
                let encoder = match encoder_for(width, height, config.fps, config.bitrate) {
                    Ok(encoder) => {
                        let _ = ready.send(Ok(()));
                        encoder
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if let Err(error) = stream.start_capture() {
                    tracing::error!(?error, "ScreenCaptureKit would not start");
                    return;
                }
                pump(
                    Capture {
                        encoder,
                        width,
                        height,
                        fps: config.fps,
                        bitrate: config.bitrate,
                    },
                    &incoming,
                    &sink,
                    &keyframe,
                    &bitrate,
                    &running,
                );
                let _ = stream.stop_capture();
                running.store(false, Ordering::Relaxed);
            })?;

        started
            .recv()
            .map_err(|_| anyhow::anyhow!("the capture thread died before it started"))??;
        self.worker = Some(worker);
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate.store(bits_per_second, Ordering::Relaxed);
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // The thread ends when the sample channel closes, which happens
            // when the stream is torn down; joining keeps the display
            // released before the next session tries to claim it.
            let _ = worker.join();
        }
    }
}

impl Drop for MacVideo {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An encoder together with the settings it was built for.
///
/// The two travel together because changing any of them means building a new
/// encoder, and an encoder that disagrees with the frames being fed to it
/// fails in ways that are hard to attribute.
struct Capture {
    encoder: Encoder,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

/// Encode captured frames until capture stops or the client goes away.
fn pump(
    mut capture: Capture,
    incoming: &mpsc::Receiver<CMSampleBuffer>,
    sink: &FrameSink,
    keyframe: &AtomicBool,
    bitrate: &AtomicU32,
    running: &AtomicBool,
) {
    let started = Instant::now();
    // The first frame a client sees must be decodable on its own.
    let mut force_key = true;

    loop {
        // Waking periodically rather than blocking outright is what makes
        // stopping possible at all. The stream's output handler holds the
        // sending half of this channel, and the stream itself is owned by
        // this thread, so the channel cannot close until this loop has
        // already returned; waiting on it alone deadlocks `stop`.
        let sample = match incoming.recv_timeout(WAKE) {
            Ok(sample) => sample,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A screen that is not changing produces no frames, which is
                // the desired behaviour and not a fault: the client already
                // shows the last one.
                if running.load(Ordering::Relaxed) && !sink.is_closed() {
                    continue;
                }
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if sink.is_closed() || !running.load(Ordering::Relaxed) {
            return;
        }

        // Rebuilding the session costs a keyframe, so only do it when the
        // client's estimate has actually moved.
        let wanted = bitrate.load(Ordering::Relaxed);
        if wanted > 0 && differs_materially(capture.bitrate, wanted) {
            match encoder_for(capture.width, capture.height, capture.fps, wanted) {
                Ok(replacement) => {
                    capture.encoder = replacement;
                    capture.bitrate = wanted;
                    // A fresh session has no reference frames, so the first
                    // frame out of it must be a keyframe or the client
                    // decodes garbage.
                    force_key = true;
                }
                Err(error) => tracing::warn!(%error, "could not change the encoder's bitrate"),
            }
        }

        let options = EncodeOptions {
            force_key_frame: force_key || keyframe.swap(false, Ordering::Relaxed),
        };
        force_key = false;

        let buffer = sample.image_buffer_ptr();
        if buffer.is_null() {
            continue;
        }
        // SAFETY: the pointer comes straight from a live CMSampleBuffer that
        // outlives this call, and the stream was configured with the same
        // dimensions and pixel format the encoder was built for.
        if let Err(error) = unsafe { capture.encoder.encode_pixel_buffer(buffer, &options) } {
            tracing::warn!(?error, "the encoder rejected a frame");
            continue;
        }
        drop(sample);

        loop {
            match capture.encoder.next_frame() {
                Ok(Some(frame)) => {
                    let data = avcc(&frame);
                    if data.is_empty() {
                        continue;
                    }
                    let encoded = EncodedFrame {
                        keyframe: frame.keyframe,
                        timestamp_us: started.elapsed().as_micros() as u64,
                        data,
                    };
                    if sink.try_send(encoded).is_err() && sink.is_closed() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(?error, "the encoder failed");
                    return;
                }
            }
        }
    }
}

/// Build a VideoToolbox encoder for this session.
fn encoder_for(width: u32, height: u32, fps: u32, bitrate: u32) -> anyhow::Result<Encoder> {
    Encoder::new(EncoderConfig {
        width,
        height,
        codec: CodecConfig::H264(H264EncoderConfig {
            // High profile, because the bandwidth saved on a mostly-static
            // desktop is large and every machine this targets decodes it in
            // hardware.
            profile: H264Profile::High,
            // CABAC: slower to encode, meaningfully smaller, and the encoder
            // is not the bottleneck on any machine with a media engine.
            entropy_mode: H264EntropyMode::Cabac,
        }),
        pixel_format: PixelFormat::Nv12,
        average_bitrate: Some(u64::from(bitrate)),
        fps_numerator: fps.max(1),
        fps_denominator: 1,
        // An interactive session is judged on latency, not on how it looks
        // paused, and B-frames would trade the first for the second. With no
        // reordering the encoder may also emit each frame as soon as it is
        // done, which is what `max_frame_delay_count: None` then means.
        allow_frame_reordering: false,
        allow_temporal_compression: true,
        real_time: true,
        max_frame_delay_count: None,
        // No fixed GOP count: keyframes are expensive and the client asks
        // for one when it needs one. The ten-second ceiling is a backstop so
        // a stream that lost a frame can always recover on its own.
        max_key_frame_interval: None,
        max_key_frame_interval_duration: Some(std::time::Duration::from_secs(10)),
        prioritize_encoding_speed_over_quality: false,
        maximize_power_efficiency: false,
    })
    .map_err(|error| anyhow::anyhow!("could not create a VideoToolbox encoder: {error:?}"))
}

/// Serialise a frame as AVCC, with parameter sets ahead of every keyframe.
///
/// The encoder reports parameter sets out of band. Putting them in front of
/// each keyframe costs a few dozen bytes and removes an entire class of
/// failure: a client that joins late, or that reconnects after a drop, never
/// has to be told them separately or wait for a stream that describes itself.
fn avcc(frame: &shiguredo_video_toolbox::EncodedFrame) -> Vec<u8> {
    if !frame.keyframe {
        return frame.data.clone();
    }

    let sets = frame
        .vps_list
        .iter()
        .chain(frame.sps_list.iter())
        .chain(frame.pps_list.iter());

    let mut out = Vec::with_capacity(frame.data.len() + 256);
    for set in sets {
        out.extend_from_slice(&(set.len() as u32).to_be_bytes());
        out.extend_from_slice(set);
    }
    out.extend_from_slice(&frame.data);
    out
}

/// Whether a new bitrate is worth rebuilding the encoder for.
fn differs_materially(current: u32, wanted: u32) -> bool {
    let low = current.saturating_sub(current / 5);
    let high = current.saturating_add(current / 5);
    wanted < low || wanted > high
}

/// Fit the requested size inside the display without distorting it.
///
/// A client asking for 1920x1080 from a 16:10 display must not be given a
/// stretched picture; it gets the largest 16:10 image that fits in the box it
/// asked for. Dimensions are rounded to even numbers because 4:2:0 chroma is
/// subsampled by two and odd sizes make encoders behave strangely.
fn fit(display_w: u32, display_h: u32, want_w: u32, want_h: u32) -> (u32, u32) {
    if display_w == 0 || display_h == 0 {
        return (even(want_w), even(want_h));
    }
    let scale = f64::from(want_w) / f64::from(display_w);
    let scale = scale.min(f64::from(want_h) / f64::from(display_h)).min(1.0);
    (
        even((f64::from(display_w) * scale).round() as u32),
        even((f64::from(display_h) * scale).round() as u32),
    )
}

fn even(value: u32) -> u32 {
    value.max(2) & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_preserves_the_display_shape() {
        // A 16:10 display into a 16:9 request: the height is the constraint.
        let (w, h) = fit(2560, 1600, 1920, 1080);
        assert_eq!((w, h), (1728, 1080));
        assert!((w as f64 / h as f64 - 2560.0 / 1600.0).abs() < 0.01);

        // Same shape both ways: an exact fit.
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));

        // A display smaller than the request is never upscaled: sending more
        // pixels than exist wastes bandwidth to no visible effect.
        assert_eq!(fit(1280, 800, 1920, 1080), (1280, 800));
    }

    #[test]
    fn dimensions_are_always_even() {
        let (w, h) = fit(1365, 767, 1365, 767);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert_eq!(fit(0, 0, 1921, 1081), (1920, 1080));
    }

    #[test]
    fn small_bitrate_changes_do_not_rebuild_the_encoder() {
        // Rebuilding costs a keyframe, so the client's noisy estimate must
        // not be able to make the stream stutter.
        assert!(!differs_materially(10_000_000, 10_500_000));
        assert!(!differs_materially(10_000_000, 9_500_000));
        assert!(differs_materially(10_000_000, 5_000_000));
        assert!(differs_materially(10_000_000, 20_000_000));
    }
}
