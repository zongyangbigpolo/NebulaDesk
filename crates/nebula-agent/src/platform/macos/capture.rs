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

use screencapturekit::cg::CGRect;
use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::cv::CVPixelBuffer;
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

fn validate_isolated_window(target: CGRect, content: CGRect) -> anyhow::Result<()> {
    let valid = |r: CGRect| {
        [r.origin.x, r.origin.y, r.size.width, r.size.height]
            .iter()
            .all(|n| n.is_finite())
            && r.size.width > 0.0
            && r.size.height > 0.0
    };
    let same = |a: CGRect, b: CGRect| {
        [
            a.origin.x - b.origin.x,
            a.origin.y - b.origin.y,
            a.size.width - b.size.width,
            a.size.height - b.size.height,
        ]
        .iter()
        .all(|n| n.abs() < 0.5)
    };
    anyhow::ensure!(
        valid(target) && valid(content),
        "invalid isolated capture rectangle"
    );
    anyhow::ensure!(
        same(target, content),
        "native window filter unexpectedly expanded"
    );
    Ok(())
}

fn configure_isolated_window(config: &mut SCStreamConfiguration) {
    // Native sheet shadows can span the containing document. Exclude those
    // shadows and child compositing rather than cropping any parent capture.
    config.set_ignores_shadows_single_window(true);
    config.set_includes_child_windows(false);
}

/// Check the calling process's current grant without showing a consent prompt.
#[must_use]
pub fn screen_capture_allowed() -> bool {
    // SAFETY: this public CoreGraphics query takes no arguments or ownership.
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

/// Captures the main display and encodes it in hardware.
pub struct MacVideo {
    window: Option<(u32, i32)>,
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
            window: None,
            keyframe: Arc::new(AtomicBool::new(false)),
            bitrate: Arc::new(AtomicU32::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// Capture this WindowServer surface only, never its enclosing display.
    pub fn window(window: u32, owner_pid: i32) -> Self {
        let mut source = Self::new();
        source.window = Some((window, owner_pid));
        source
    }
}

impl VideoSource for MacVideo {
    fn preserves_frame_provenance(&self) -> bool {
        true
    }

    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
        // A machine without permission fails both ways depending on macOS
        // version and TCC history: an error, or an empty display list. Both
        // mean the same thing to whoever has to fix it, so both say so.
        let preparing = Instant::now();
        let content = SCShareableContent::get()
            .map_err(|error| anyhow::anyhow!("{}: {error:?}", PERMISSION))?;
        let (filter, source_width, source_height) = if let Some((id, pid)) = self.window {
            let windows = content.windows();
            let window = windows
                .iter()
                .find(|window| {
                    window.window_id() == id
                        && window
                            .owning_application()
                            .is_some_and(|app| app.process_id() == pid)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("the authorized application surface is unavailable")
                })?;
            let bounds = window.frame();
            let filter = SCContentFilter::create().with_window(window).build();
            validate_isolated_window(bounds, filter.content_rect())?;
            tracing::debug!(window = id, target = ?bounds, filter = ?filter.content_rect(),
                "isolated application window capture");
            (
                filter,
                bounds.size.width.round() as u32,
                bounds.size.height.round() as u32,
            )
        } else {
            let displays = content.displays();
            let display = displays
                .first()
                .ok_or_else(|| anyhow::anyhow!(PERMISSION))?;
            (
                SCContentFilter::create().with_display(display).build(),
                display.width(),
                display.height(),
            )
        };
        let (width, height) = if self.window.is_some() {
            anyhow::ensure!(
                config.width >= 2
                    && config.height >= 2
                    && config.width <= 8192
                    && config.height <= 8192
                    && config.width % 2 == 0
                    && config.height % 2 == 0,
                "invalid application capture geometry"
            );
            (config.width, config.height)
        } else {
            fit(source_width, source_height, config.width, config.height)
        };
        let mut stream_config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            // NV12 video range: the one format ScreenCaptureKit produces and
            // VideoToolbox consumes without a conversion pass in between.
            .with_pixel_format(ScPixelFormat::YCbCr_420v)
            .with_queue_depth(QUEUE_DEPTH)
            .with_fps(config.fps)
            // Composite the pointer into the stream. The alternative — drawing
            // it on the client, where it could move with no latency at all —
            // needs the client to know the remote pointer's shape as well as
            // its position, and a client that draws nothing shows a session
            // that appears not to respond to the mouse at all.
            .with_shows_cursor(self.window.is_none());
        if self.window.is_some() {
            configure_isolated_window(&mut stream_config);
        }

        // ScreenCaptureKit delivers on its own dispatch queue, so frames
        // cross into Rust here and are handed to a thread that owns the
        // encoder. The encoder is neither Sync nor safe to touch from an
        // arbitrary queue, and a bounded channel means a slow encoder drops
        // frames rather than growing a backlog of stale ones.
        let (frames, incoming) =
            mpsc::sync_channel::<(CMSampleBuffer, FrameSink)>(QUEUE_DEPTH as usize);
        let producer = sink.clone();
        let mut stream = SCStream::new(&filter, &stream_config);
        tracing::info!(window = ?self.window, elapsed_ms = preparing.elapsed().as_millis(),
            "native capture stream configured");
        stream.add_output_handler(
            move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                if kind == SCStreamOutputType::Screen {
                    // A full channel means the encoder is behind. Dropping the
                    // newest frame is wrong — drop nothing and let the next
                    // one through — but there is no way to replace the queued
                    // one, so the newest is what goes.
                    let _ = frames.try_send((sample, producer.for_frame()));
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
        let window = self.window;
        let worker = std::thread::Builder::new()
            .name("nebula-capture".into())
            .spawn(move || {
                let initializing = Instant::now();
                let encoder = match encoder_for(width, height, config.fps, config.bitrate) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                tracing::info!(
                    ?window,
                    elapsed_ms = initializing.elapsed().as_millis(),
                    "native capture encoder initialized"
                );
                // Only now is capture actually running. Reporting success
                // before this point meant a stream that refused to start
                // produced a session that connected, showed nothing, and
                // reported no error anywhere the operator would look.
                if let Err(error) = stream.start_capture() {
                    let _ = ready.send(Err(anyhow::anyhow!(
                        "{}: ScreenCaptureKit would not start: {error:?}",
                        PERMISSION
                    )));
                    return;
                }
                tracing::info!(?window, "native capture stream started");
                let _ = ready.send(Ok(()));
                pump(
                    Capture {
                        encoder,
                        window,
                        encoded_any: false,
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
                tracing::info!(?window, "stopping native capture stream");
                match stream.stop_capture() {
                    Ok(()) => tracing::info!(?window, "native capture stream stopped"),
                    Err(error) => tracing::warn!(?window, ?error, "native capture stop failed"),
                }
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
            tracing::info!(window = ?self.window, "joining native capture worker");
            // The thread ends when the sample channel closes, which happens
            // when the stream is torn down; joining keeps the display
            // released before the next session tries to claim it.
            let _ = worker.join();
            tracing::info!(window = ?self.window, "native capture worker joined");
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
    window: Option<(u32, i32)>,
    encoded_any: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
}

struct CaptureFrames<T> {
    last: Option<T>,
    force_key: bool,
}

impl<T> CaptureFrames<T> {
    fn new() -> Self {
        Self {
            last: None,
            force_key: true,
        }
    }

    fn select(&mut self, frame: Option<T>, requested: &AtomicBool) -> Option<(&T, bool)> {
        let changed = frame.is_some();
        if let Some(frame) = frame {
            self.last = Some(frame);
        }
        let frame = self.last.as_ref()?;
        if !changed && !self.force_key && !requested.load(Ordering::Relaxed) {
            return None;
        }
        let key = requested.swap(false, Ordering::Relaxed) || self.force_key;
        self.force_key = false;
        Some((frame, key))
    }
}

/// Encode captured frames until capture stops or the client goes away.
fn pump(
    mut capture: Capture,
    incoming: &mpsc::Receiver<(CMSampleBuffer, FrameSink)>,
    sink: &FrameSink,
    keyframe: &AtomicBool,
    bitrate: &AtomicU32,
    running: &AtomicBool,
) {
    let started = Instant::now();
    let mut frames = CaptureFrames::new();
    // VideoToolbox returns frames in submission order (reordering is disabled).
    // Keep the native-image epoch through both the encoder and source queues.
    let mut pending = std::collections::VecDeque::new();

    loop {
        // Waking periodically rather than blocking outright is what makes
        // stopping possible at all. The stream's output handler holds the
        // sending half of this channel, and the stream itself is owned by
        // this thread, so the channel cannot close until this loop has
        // already returned; waiting on it alone deadlocks `stop`.
        let sample = match incoming.recv_timeout(WAKE) {
            Ok(sample) => Some(sample),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
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
                    pending.clear();
                    capture.bitrate = wanted;
                    // A fresh session has no reference frames, so the first
                    // frame out of it must be a keyframe or the client
                    // decodes garbage.
                    frames.force_key = true;
                }
                Err(error) => tracing::warn!(%error, "could not change the encoder's bitrate"),
            }
        }

        // Idle ScreenCaptureKit notifications may have no image buffer. They
        // must neither replace the last usable image nor consume an IDR request.
        // The bridge returns a +1 reference; this owner balances it exactly once.
        let buffer = sample.and_then(|(sample, provenance)| {
            CVPixelBuffer::from_raw(sample.image_buffer_ptr()).map(|buffer| (buffer, provenance))
        });
        if frames.last.is_none() && buffer.is_some() {
            tracing::info!(window = ?capture.window, "native capture received first image buffer");
        }
        if let Some(((buffer, provenance), force_key_frame)) = frames.select(buffer, keyframe) {
            if !provenance.is_current() || pending.len() >= 32 {
                keyframe.store(true, Ordering::Relaxed);
            } else {
                // SAFETY: the cached owner keeps this same-format capture surface
                // alive until VideoToolbox has retained it for asynchronous encoding.
                if let Err(error) = unsafe {
                    capture
                        .encoder
                        .encode_pixel_buffer(buffer.as_ptr(), &EncodeOptions { force_key_frame })
                } {
                    tracing::warn!(?error, "the encoder rejected a frame");
                    keyframe.store(true, Ordering::Relaxed);
                } else {
                    pending.push_back(provenance.clone());
                }
            }
        }
        // Encoding is asynchronous: a cached IDR may complete after submission
        // even when the desktop supplies no further usable capture samples.
        if !drain(&mut capture, &mut pending, keyframe, started) {
            return;
        }
    }
}

/// Forward completed output, including completion during a static desktop.
fn drain(
    capture: &mut Capture,
    pending: &mut std::collections::VecDeque<FrameSink>,
    keyframe: &AtomicBool,
    started: Instant,
) -> bool {
    loop {
        match capture.encoder.next_frame() {
            Ok(Some(frame)) => {
                let Some(sink) = pending.pop_front() else {
                    tracing::error!("native encoder returned a frame without capture provenance");
                    return false;
                };
                let data = avcc(&frame);
                if data.is_empty() {
                    continue;
                }
                if !capture.encoded_any {
                    tracing::info!(window = ?capture.window, keyframe = frame.keyframe,
                        "native capture encoded first frame");
                    capture.encoded_any = true;
                }
                let encoded = EncodedFrame {
                    keyframe: frame.keyframe,
                    timestamp_us: started.elapsed().as_micros() as u64,
                    data,
                };
                if sink.try_send(encoded).is_err() {
                    if sink.is_closed() {
                        return false;
                    }
                    // The queue is short on purpose: a frame that cannot be
                    // sent promptly is better replaced than delivered late.
                    // But every frame dropped here is a gap in the reference
                    // chain, and the client can only discover it a round trip
                    // later and only repair it by asking for a keyframe over
                    // the same congested link. Repairing it locally instead
                    // costs one keyframe and no round trip, and it is decided
                    // where the loss actually happened.
                    keyframe.store(true, Ordering::Relaxed);
                }
            }
            Ok(None) => return true,
            Err(error) => {
                tracing::warn!(?error, "the encoder failed");
                return false;
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

/// Verify VideoToolbox can allocate the encoder used by application captures.
pub(crate) fn application_encoder_available() -> anyhow::Result<()> {
    let mut config = SCStreamConfiguration::new();
    config.set_includes_child_windows(true);
    anyhow::ensure!(
        config.includes_child_windows(),
        "independent application window capture requires macOS 14.2 or newer"
    );
    encoder_for(64, 64, 30, 500_000).map(drop)
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
    fn attached_sheet_requires_its_own_filter_not_a_scaled_parent() {
        let document = CGRect::new(130.0, 93.0, 600.0, 412.0);
        let sheet = CGRect::new(300.0, 230.0, 260.0, 138.0);
        validate_isolated_window(document, document).unwrap();
        validate_isolated_window(sheet, sheet).unwrap();
        assert!(validate_isolated_window(sheet, document).is_err());
    }

    #[test]
    fn window_filter_rejects_expansion_and_invalid_geometry() {
        let document = CGRect::new(130.0, 93.0, 600.0, 412.0);
        let sheet = CGRect::new(300.0, 230.0, 260.0, 138.0);
        assert!(validate_isolated_window(sheet, CGRect::new(0.0, 0.0, 1920.0, 1080.0)).is_err());
        assert!(
            validate_isolated_window(CGRect::new(120.0, 230.0, 260.0, 138.0), document).is_err()
        );
        assert!(validate_isolated_window(CGRect::new(f64::NAN, 0.0, 1.0, 1.0), document).is_err());
    }

    #[test]
    fn isolated_window_configuration_excludes_children_and_group_shadows_without_crop() {
        let mut config = SCStreamConfiguration::new();
        let uncropped = config.source_rect();
        configure_isolated_window(&mut config);
        assert!(!config.includes_child_windows());
        assert!(config.ignores_shadows_single_window());
        assert_eq!(config.source_rect(), uncropped);
    }

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

    #[test]
    fn idle_notifications_keep_the_last_image_and_replay_requested_idr() {
        let request = AtomicBool::new(false);
        let mut frames = CaptureFrames::new();
        assert_eq!(frames.select(Some(7), &request), Some((&7, true)));
        assert_eq!(frames.select(None, &request), None);
        request.store(true, Ordering::Relaxed);
        assert_eq!(frames.select(None, &request), Some((&7, true)));
        assert!(!request.load(Ordering::Relaxed));
        assert_eq!(frames.select(None, &request), None);
        assert_eq!(frames.select(Some(8), &request), Some((&8, false)));
    }

    #[test]
    fn a_request_before_the_first_image_is_not_consumed() {
        let request = AtomicBool::new(true);
        let mut frames = CaptureFrames::new();
        assert_eq!(frames.select(None, &request), None);
        assert!(request.load(Ordering::Relaxed));
        assert_eq!(frames.select(Some(7), &request), Some((&7, true)));
        assert!(!request.load(Ordering::Relaxed));
    }

    #[test]
    fn replacing_or_dropping_the_cache_releases_each_owned_image_once() {
        use std::sync::atomic::AtomicUsize;
        struct Image(Arc<AtomicUsize>);
        impl Drop for Image {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let request = AtomicBool::new(false);
        let mut frames = CaptureFrames::new();
        frames.select(Some(Image(Arc::clone(&dropped))), &request);
        frames.select(None, &request);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        frames.select(Some(Image(Arc::clone(&dropped))), &request);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        drop(frames);
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
    }

    /// Capture must keep going indefinitely, not stop once it has handed out
    /// as many frames as the stream's surface pool holds.
    ///
    /// This is the shape of a real bug: every encoded frame leaked one of the
    /// `QUEUE_DEPTH` surfaces ScreenCaptureKit recycles, so the stream
    /// delivered exactly `QUEUE_DEPTH` frames and then went silent with no
    /// error anywhere. A client saw one still image and nothing after it.
    ///
    /// Needs a real display and Screen Recording permission, so it is not part
    /// of the normal run. On a machine with both:
    ///
    /// ```text
    /// cargo test -p nebula-agent --release -- --ignored capture_outlives
    /// ```
    #[test]
    #[ignore = "needs a display and Screen Recording permission"]
    fn capture_outlives_the_surface_pool() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(QUEUE_DEPTH as usize);
        let mut video = MacVideo::new();
        video
            .start(
                VideoConfig {
                    width: 1280,
                    height: 720,
                    fps: 30,
                    bitrate: 4_000_000,
                },
                tx.into(),
            )
            .expect("capture should start");

        // Comfortably more than the pool, and enough time that a screen which
        // simply is not changing still produces them.
        let wanted = QUEUE_DEPTH as usize * 3;
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let seen = runtime.block_on(async {
            let mut seen = 0;
            while seen < wanted {
                let next =
                    tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await;
                match next {
                    Ok(Some(_)) => seen += 1,
                    _ => break,
                }
            }
            seen
        });
        video.stop();

        assert!(
            seen >= wanted,
            "capture stopped after {seen} frames, wanted at least {wanted}: \
             the stream's surfaces are being leaked"
        );
    }
}
