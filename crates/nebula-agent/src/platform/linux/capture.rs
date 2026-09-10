use std::os::fd::AsRawFd;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::Context;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use gstreamer_video::UpstreamForceKeyUnitEvent;

use super::{
    h264::{AccessUnits, Recovery},
    pipeline::Pipeline,
    portal::Portal,
};
use crate::media::{EncodedFrame, FrameSink, VideoConfig, VideoSource};

pub(super) struct LinuxVideo {
    allow_input: bool,
    window_only: bool,
    portal: Arc<Mutex<Option<Portal>>>,
    running: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    bitrate: Arc<AtomicU32>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LinuxVideo {
    pub fn new(allow_input: bool, portal: Arc<Mutex<Option<Portal>>>) -> Self {
        Self {
            allow_input,
            window_only: false,
            portal,
            running: Arc::new(AtomicBool::new(false)),
            keyframe: Arc::new(AtomicBool::new(false)),
            bitrate: Arc::new(AtomicU32::new(0)),
            worker: None,
        }
    }

    /// Consent-only window capture. It is not an authenticated published APP
    /// target and therefore must not be returned by `Platform::application`.
    #[allow(dead_code)]
    pub fn consented_window() -> Self {
        let mut source = Self::new(false, Arc::new(Mutex::new(None)));
        source.window_only = true;
        source
    }
}

impl VideoSource for LinuxVideo {
    fn start(&mut self, config: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
        anyhow::ensure!(self.worker.is_none(), "Linux capture is already started");
        anyhow::ensure!(
            (2..=8192).contains(&config.width)
                && (2..=8192).contains(&config.height)
                && (1..=120).contains(&config.fps)
                && (100_000..=100_000_000).contains(&config.bitrate),
            "invalid Linux capture dimensions, framerate or bitrate"
        );
        let mut state = self
            .portal
            .lock()
            .map_err(|_| anyhow::anyhow!("portal state poisoned"))?;
        anyhow::ensure!(
            state.is_none(),
            "this scoped Linux platform already has a video source"
        );
        // Instantiate hardware elements before asking the user to share a screen.
        gst::init()?;
        for plugin in ["pipewiresrc", "vapostproc", "vah264enc", "h264parse"] {
            anyhow::ensure!(gst::ElementFactory::find(plugin).is_some(),
                "missing native Linux plugin {plugin}; install VA-API drivers and GStreamer packages; \
                 no software video fallback is permitted");
        }
        let (portal, stream) = if self.window_only {
            anyhow::ensure!(
                !self.allow_input,
                "consented window input must remain disabled"
            );
            Portal::open_window_consent()?
        } else {
            Portal::open(self.allow_input)?
        };
        let active = portal.active.clone();
        let (width, height) = fit(stream.width, stream.height, config.width, config.height);
        let pipeline = Pipeline::new(&format!(
            "pipewiresrc name=capture do-timestamp=true keepalive-time=100 \
             ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videorate drop-only=true max-rate={} \
             ! vapostproc \
             ! video/x-raw(memory:VAMemory),format=NV12,width={width},height={height} \
             ! vah264enc name=encoder rate-control=cbr b-frames=0 ref-frames=1 key-int-max={} bitrate={} \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=frames max-buffers=2 drop=false sync=false wait-on-eos=false enable-last-sample=false",
            config.fps, config.fps * 2, config.bitrate.div_ceil(1000),
        ))?;
        let source = pipeline.element("capture")?;
        source.set_property("fd", stream.fd.as_raw_fd());
        // Portal node IDs are object IDs, not target-object's object.serial.
        source.set_property("path", stream.node.to_string());
        let output = pipeline
            .element("frames")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("capture appsink has the wrong type"))?;
        let encoder = pipeline.element("encoder")?;
        pipeline.play()?;
        *state = Some(portal);
        drop(state);
        self.running.store(true, Ordering::Release);
        self.bitrate.store(config.bitrate, Ordering::Relaxed);
        let running = self.running.clone();
        let keyframe = self.keyframe.clone();
        let bitrate = self.bitrate.clone();
        let portal_state = self.portal.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("nebula-linux-video".into())
            .spawn(move || {
                let result = pump(
                    &pipeline,
                    &output,
                    &encoder,
                    &sink,
                    &ready,
                    &running,
                    &active,
                    &keyframe,
                    &bitrate,
                    config.bitrate,
                );
                if let Err(error) = result {
                    tracing::error!(%error, "Linux screen capture stopped");
                    let _ = ready.try_send(Err(error));
                }
                running.store(false, Ordering::Release);
                // PipeWire owns a duplicate of fd; keep the original alive until NULL.
                drop(pipeline);
                drop(stream);
                match portal_state.lock() {
                    Ok(mut state) => {
                        state.take();
                    }
                    Err(_) => tracing::error!("portal state poisoned during capture shutdown"),
                }
            });
        match worker {
            Ok(worker) => self.worker = Some(worker),
            Err(error) => {
                self.stop();
                return Err(error.into());
            }
        }
        let result = started
            .recv_timeout(Duration::from_secs(12))
            .context("no hardware-encoded screen frame arrived within 12 seconds")
            .and_then(|result| result);
        if result.is_err() {
            self.stop();
        }
        result
    }

    fn request_keyframe(&mut self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }
    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate.store(
            bits_per_second.clamp(100_000, 100_000_000),
            Ordering::Relaxed,
        );
    }
    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Linux capture worker panicked");
            }
        }
        match self.portal.lock() {
            Ok(mut state) => {
                state.take();
            }
            Err(_) => tracing::error!("portal state poisoned during shutdown"),
        }
    }
}

impl Drop for LinuxVideo {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn pump(
    pipeline: &Pipeline,
    output: &AppSink,
    encoder: &gst::Element,
    sink: &FrameSink,
    ready: &mpsc::SyncSender<anyhow::Result<()>>,
    running: &AtomicBool,
    active: &AtomicBool,
    keyframe: &AtomicBool,
    bitrate: &AtomicU32,
    mut current_bitrate: u32,
) -> anyhow::Result<()> {
    let mut access_units = AccessUnits::default();
    let mut recovery = Recovery::default();
    recovery.lost();
    let mut first = true;
    let mut last_timestamp = None;
    let started = Instant::now();
    while running.load(Ordering::Acquire) && !sink.is_closed() {
        anyhow::ensure!(
            active.load(Ordering::Acquire),
            "desktop portal permission ended"
        );
        pipeline.check()?;
        let target = bitrate.load(Ordering::Relaxed);
        if target != current_bitrate {
            encoder.set_property("bitrate", target.div_ceil(1000));
            current_bitrate = target;
        }
        if keyframe.swap(false, Ordering::Relaxed) {
            anyhow::ensure!(
                output.send_event(
                    UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build()
                ),
                "VA-API encoder refused an IDR request"
            );
        }
        let Some(sample) = output.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            anyhow::ensure!(!output.is_eos(), "PipeWire capture ended");
            anyhow::ensure!(
                !first || started.elapsed() < Duration::from_secs(10),
                "PipeWire/VA-API produced no frame; verify portal selection and GPU support"
            );
            continue;
        };
        let buffer = sample
            .buffer()
            .context("capture sample contains no H.264 buffer")?;
        let map = buffer.map_readable()?;
        let (idr, data) = access_units.convert(map.as_slice())?;
        if data.is_empty() {
            continue;
        } // Parameter-set-only buffers are cached.
        if !recovery.accept(idr) {
            continue;
        }
        let timestamp_us = buffer
            .pts()
            .context("captured frame has no timestamp")?
            .useconds();
        anyhow::ensure!(
            last_timestamp.is_none_or(|last| timestamp_us >= last),
            "VA-API capture timestamps moved backwards"
        );
        last_timestamp = Some(timestamp_us);
        let frame = EncodedFrame {
            keyframe: idr,
            timestamp_us,
            data,
        };
        match sink.try_send(frame) {
            Ok(()) => {
                recovery.delivered(idr);
                if first {
                    let _ = ready.send(Ok(()));
                    first = false;
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                recovery.lost();
                keyframe.store(true, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    (
        ((f64::from(width) * scale) as u32 & !1).max(2),
        ((f64::from(height) * scale) as u32 & !1).max(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_keeps_aspect_ratio_and_even_dimensions() {
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(1920, 1200, 1920, 1080), (1728, 1080));
        assert_eq!(fit(1365, 767, 1920, 1080), (1364, 766));
    }
}
